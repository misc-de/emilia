//! Lyrics & karaoke: load lyrics for the running track (embedded tags → DB
//! cache → LRCLIB online) and drive the live karaoke dialog (highlight + scroll
//! the active line). The static lyrics pulldown on the file-info pages lives in
//! [`crate::ui::app_dialogs`]; this module supplies its async online fill-in.
//!
//! Like the rest of the online metadata, fetched lyrics are only ever cached in
//! the database, never written back into the audio file's tags.

use std::path::PathBuf;
use std::time::Duration;

use adw::prelude::*;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::tag::Accessor;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::core::db::Library;
use crate::core::lyrics::Lyrics;
use crate::i18n::gettext;
use crate::ui::app::{App, Cmd, LyricsView, Msg};
use crate::ui::app_helpers::online_available;

/// Best available `(artist, title, album, duration_secs)` for an online lyrics
/// lookup: from the library row when usable, otherwise straight from the tags.
fn lookup_info(lib: &Library, path: &str) -> Option<(String, String, Option<String>, Option<u64>)> {
    if let Some(t) = lib.track_by_path(path).ok().flatten() {
        let artist = t.artist.clone().unwrap_or_default();
        if !artist.trim().is_empty() && !t.title.trim().is_empty() {
            let dur = t.duration_ms.map(|m| (m.max(0) / 1000) as u64).filter(|d| *d > 0);
            return Some((artist, t.title, t.album, dur));
        }
    }
    // Fallback: read the tags directly (e.g. a not-yet-scanned file).
    let tagged = lofty::read_from_path(path).ok()?;
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag())?;
    let artist = tag
        .artist()
        .map(|c| c.trim().to_string())
        .filter(|s| !s.is_empty())?;
    let title = tag
        .title()
        .map(|c| c.trim().to_string())
        .filter(|s| !s.is_empty())?;
    let album = tag
        .album()
        .map(|c| c.trim().to_string())
        .filter(|s| !s.is_empty());
    let dur = tagged.properties().duration().as_secs();
    Some((artist, title, album, (dur > 0).then_some(dur)))
}

impl App {
    /// Loads lyrics for the just-started track: embedded tags + DB cache
    /// (instant, offline), then an LRCLIB lookup in the background when no
    /// synchronized lyrics are available yet. Called from `play_current`.
    pub(crate) fn load_lyrics(&mut self, sender: &ComponentSender<Self>, path: PathBuf) {
        let path_str = path.to_string_lossy().to_string();
        // New track → drop the previous track's lyrics and close an open view.
        self.close_lyrics_view();
        self.lyrics.current = None;
        self.lyrics.for_path = Some(path_str.clone());

        // 1) Embedded (unsynchronized) tags – instant, offline.
        let embedded = crate::core::scanner::read_lyrics(&path);
        // 2) DB cache – may already hold synchronized lyrics from earlier.
        let cached = self.library.get_cached_lyrics(&path_str);
        self.lyrics.current = match (cached, embedded) {
            // Prefer cached synced lyrics over embedded plain text.
            (Some(c), _) if c.has_synced() => Some(c),
            (_, Some(text)) => Some(Lyrics::from_parts(Some(text), None)),
            (Some(c), None) => Some(c),
            (None, None) => None,
        };

        // Already have karaoke lyrics, offline, or recently confirmed missing →
        // no online lookup.
        let have_synced = self.lyrics.current.as_ref().is_some_and(|l| l.has_synced());
        if have_synced
            || !online_available()
            || self.library.lyrics_recently_missing(&path_str)
        {
            return;
        }
        let Some((artist, title, album, dur)) = lookup_info(&self.library, &path_str) else {
            return;
        };
        sender.spawn_command(move |out| {
            let client = crate::core::online::OnlineClient::new();
            let lyrics = client
                .fetch_lyrics(&artist, &title, album.as_deref(), dur)
                .ok()
                .flatten();
            let _ = out.send(Cmd::LyricsLoaded {
                path: path_str,
                lyrics,
            });
        });
    }

    /// Background LRCLIB lookup for the running track finished. Caches the
    /// result (positive or negative) and applies it if the track is unchanged.
    pub(crate) fn on_lyrics_loaded(&mut self, path: String, lyrics: Option<Lyrics>) {
        match &lyrics {
            Some(l) => {
                self.library
                    .store_lyrics(&path, l.plain.as_deref(), l.synced_raw.as_deref(), "lrclib")
            }
            // Remember the miss so we don't refetch on every play for a while.
            None => self.library.store_lyrics(&path, None, None, "none"),
        }
        // Stale result for a track we no longer show → keep it cached, ignore.
        if self.lyrics.for_path.as_deref() != Some(path.as_str()) {
            return;
        }
        if let Some(l) = lyrics {
            // Don't downgrade existing synced lyrics with a plain-only result.
            let keep = self
                .lyrics
                .current
                .as_ref()
                .is_some_and(|c| c.has_synced() && !l.has_synced());
            if !keep {
                self.lyrics.current = Some(l);
            }
        }
    }

    /// Opens the karaoke dialog for the running track and starts the
    /// fine-grained highlight timer. Synced lyrics scroll/highlight live; an
    /// unsynced result is shown as plain centered text.
    pub(crate) fn show_lyrics(&mut self) {
        // Already open (same track – a track change drops the view) → raise it.
        if let Some(view) = self.lyrics.view.as_ref() {
            view.dialog.present(Some(&self.toast_overlay));
            return;
        }
        let Some(lyr) = self.lyrics.current.clone() else {
            return;
        };
        if !lyr.has_any() {
            return;
        }

        let title = self
            .mini
            .now_playing
            .clone()
            .unwrap_or_else(|| gettext("Lyrics"));
        let dialog = adw::Dialog::builder().title(&title).build();
        dialog.set_content_width(520);
        dialog.set_content_height(640);
        self.adapt_detail_dialog(&dialog);

        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();
        let container = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .margin_top(28)
            .margin_bottom(28)
            .margin_start(16)
            .margin_end(16)
            .build();

        let mut lines = Vec::new();
        if lyr.has_synced() {
            for (_, text) in &lyr.synced {
                // Blank lines (intros/instrumental breaks) get a music note.
                let display = if text.trim().is_empty() { "♪" } else { text };
                let label = gtk::Label::builder()
                    .label(display)
                    .wrap(true)
                    .justify(gtk::Justification::Center)
                    .xalign(0.5)
                    .build();
                label.add_css_class("emilia-lyric-line");
                label.add_css_class("dim-label");
                container.append(&label);
                lines.push(label);
            }
        } else if let Some(text) = lyr.display_text() {
            let label = gtk::Label::builder()
                .label(&text)
                .wrap(true)
                .selectable(true)
                .justify(gtk::Justification::Center)
                .xalign(0.5)
                .build();
            container.append(&label);
        }
        scroller.set_child(Some(&container));
        dialog.set_child(Some(&scroller));

        // Closing stops the timer and drops the view.
        let input = self.input.clone();
        dialog.connect_closed(move |_| {
            let _ = input.send(Msg::LyricsClosed);
        });

        // Fine-grained karaoke timer – only runs while the dialog is open.
        let timer = if lyr.has_synced() {
            let input = self.input.clone();
            Some(gtk::glib::timeout_add_local(
                Duration::from_millis(200),
                move || {
                    let _ = input.send(Msg::LyricsTick);
                    gtk::glib::ControlFlow::Continue
                },
            ))
        } else {
            None
        };

        self.lyrics.view = Some(LyricsView {
            lines,
            scroller,
            container,
            active: None,
            timer,
            dialog: dialog.clone(),
        });
        dialog.present(Some(&self.toast_overlay));
        // Highlight the current line straight away.
        self.update_lyrics_highlight();
    }

    /// Karaoke tick: move the highlight (and auto-scroll) to the line active at
    /// the current playback position. No-op when the dialog is closed.
    pub(crate) fn update_lyrics_highlight(&mut self) {
        let pos = self.player.position_ms().unwrap_or(self.mini.position_ms);
        let active = match self.lyrics.current.as_ref() {
            Some(l) => l.active_line(pos),
            None => return,
        };
        let Some(view) = self.lyrics.view.as_mut() else {
            return;
        };
        if active == view.active {
            return;
        }
        if let Some(old) = view.active.and_then(|i| view.lines.get(i)) {
            old.remove_css_class("emilia-lyric-active");
            old.add_css_class("dim-label");
        }
        if let Some(new) = active.and_then(|i| view.lines.get(i)) {
            new.remove_css_class("dim-label");
            new.add_css_class("emilia-lyric-active");
            // Scroll the active line into the vertical center.
            if let Some(b) = new.compute_bounds(&view.container) {
                let va = view.scroller.vadjustment();
                let target = b.y() as f64 + b.height() as f64 / 2.0 - va.page_size() / 2.0;
                let max = (va.upper() - va.page_size()).max(va.lower());
                va.set_value(target.clamp(va.lower(), max));
            }
        }
        view.active = active;
    }

    /// Stops the karaoke timer, closes the dialog and drops the view.
    pub(crate) fn close_lyrics_view(&mut self) {
        if let Some(mut view) = self.lyrics.view.take() {
            if let Some(id) = view.timer.take() {
                id.remove();
            }
            view.dialog.close();
        }
    }

    /// Kicks off an online lyrics lookup for an open file-info dialog whose
    /// `label`/`group` are kept in `lyrics.file_pending`. Result arrives as
    /// [`Msg::FileLyricsFetched`].
    pub(crate) fn fetch_file_lyrics(&self, path: &str) {
        if !online_available() {
            return;
        }
        let Some((artist, title, album, dur)) = lookup_info(&self.library, path) else {
            return;
        };
        let input = self.input.clone();
        let path = path.to_string();
        std::thread::spawn(move || {
            let client = crate::core::online::OnlineClient::new();
            let lyrics = client
                .fetch_lyrics(&artist, &title, album.as_deref(), dur)
                .ok()
                .flatten();
            let _ = input.send(Msg::FileLyricsFetched { path, lyrics });
        });
    }

    /// Online lyrics for the open file-info dialog returned: cache them and, if
    /// the dialog still shows the same file, reveal the pulldown with the text.
    pub(crate) fn on_file_lyrics_fetched(&mut self, path: String, lyrics: Option<Lyrics>) {
        match &lyrics {
            Some(l) => {
                self.library
                    .store_lyrics(&path, l.plain.as_deref(), l.synced_raw.as_deref(), "lrclib")
            }
            None => self.library.store_lyrics(&path, None, None, "none"),
        }
        // Mirror into the running track's state too, if it's the same file.
        if self.lyrics.for_path.as_deref() == Some(path.as_str()) {
            if let Some(l) = &lyrics {
                let keep = self
                    .lyrics
                    .current
                    .as_ref()
                    .is_some_and(|c| c.has_synced() && !l.has_synced());
                if !keep {
                    self.lyrics.current = Some(l.clone());
                }
            }
        }
        // Fill the pending file-info pulldown (if still showing this file).
        let pending = self.lyrics.file_pending.borrow();
        let Some((pending_path, label, group)) = pending.as_ref() else {
            return;
        };
        if *pending_path != path {
            // A different file-info dialog was opened meanwhile.
            return;
        }
        if let Some(text) = lyrics.and_then(|l| l.display_text()) {
            label.set_label(&text);
            group.set_visible(true);
        }
    }
}
