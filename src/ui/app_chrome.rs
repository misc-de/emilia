//! Small state queries and chrome helpers of the root component: the tick
//! gate, the loading overlay (visibility/text), list rebuilds, the mobile
//! breakpoint and the file browser's path bar. Split out of
//! [`crate::ui::app`] – pure code movement, still inherent `impl App` methods.

use relm4::adw;
use relm4::gtk::prelude::*;
use relm4::prelude::*;

use crate::i18n::gettext;
use crate::ui::app::{ActiveSource, App, Msg};

impl App {
    /// Keep `tick_active` current: the per-second tick is only needed while
    /// playing or while a timeshift recording runs. Called after every message
    /// so the timer stops delivering ticks the moment the app goes idle.
    pub(crate) fn sync_tick_active(&self) {
        self.tick_active
            .set(self.mini.playing || self.streaming.record_state.is_some());
    }

    /// One background worker of a manual refresh reported back → decrement the
    /// pending counter (saturating, so a stray completion can never wrap it).
    /// When it hits zero the loading overlay hides itself again (see the view).
    pub(crate) fn refresh_done(&mut self) {
        self.refresh_pending = self.refresh_pending.saturating_sub(1);
        if self.refresh_pending == 0 {
            self.refresh_progress = None;
        }
    }

    /// A refresh or library scan is still working in the background.
    pub(crate) fn background_busy(&self) -> bool {
        self.refresh_pending > 0 || self.scanning
    }

    /// Whether the loading overlay should be shown: either a folder/list load is
    /// in progress or a manual refresh still has background workers running —
    /// unless the user tapped the latter away. A tapped-away run also skips its
    /// closing summary, so the overlay never pops back up on its own.
    pub(crate) fn overlay_visible(&self) -> bool {
        self.libview.loading
            || (!self.overlay_dismissed
                && (self.background_busy() || self.refresh_summary.is_some()))
    }

    /// Called after every message: a run starting from idle forgets an earlier
    /// tap-away (so its overlay shows again), and the flag the click-outside
    /// gesture reads is kept current. The tap-away is deliberately *not* reset
    /// when a run ends: its summary arrives just after, and must stay hidden.
    pub(crate) fn sync_overlay_dismissable(&mut self) {
        let busy = self.background_busy();
        if busy && !self.overlay_was_busy {
            self.overlay_dismissed = false;
        }
        self.overlay_was_busy = busy;
        self.overlay_dismissable.set(
            !self.overlay_dismissed && (self.background_busy() || self.refresh_summary.is_some()),
        );
    }

    /// Tapping/clicking next to the loading overlay while a refresh/scan runs
    /// hides it like a modal; the work continues and the refresh button shows
    /// it (green, spinning) until it is done. The tap itself is swallowed so it
    /// doesn't also open whatever lies beneath. A tap on the overlay itself
    /// (e.g. "Cancel") is left alone.
    pub(crate) fn connect_overlay_click_away(
        content: &gtk::Overlay,
        loading_box: &gtk::Box,
        model: &App,
        sender: &ComponentSender<Self>,
    ) {
        let click = gtk::GestureClick::new();
        click.set_propagation_phase(gtk::PropagationPhase::Capture);
        let dismissable = model.overlay_dismissable.clone();
        let loading_box = loading_box.downgrade();
        let content_weak = content.downgrade();
        let input = sender.input_sender().clone();
        click.connect_pressed(move |gesture, _n, x, y| {
            if !dismissable.get() {
                return;
            }
            let (Some(b), Some(content)) = (loading_box.upgrade(), content_weak.upgrade()) else {
                return;
            };
            let inside = content
                .compute_point(&b, &gtk::graphene::Point::new(x as f32, y as f32))
                .is_some_and(|p| b.contains(p.x() as f64, p.y() as f64));
            if inside {
                return;
            }
            gesture.set_state(gtk::EventSequenceState::Claimed);
            dismissable.set(false);
            let _ = input.send(Msg::DismissOverlay);
        });
        content.add_controller(click);
    }

    /// Text beneath the overlay spinner. A specific load label (e.g. a YouTube
    /// playlist) wins; otherwise a manual refresh shows "Updating …", and
    /// finally the default "reading data" of a plain folder/list load.
    pub(crate) fn overlay_text(&self) -> String {
        // Tapped away: only a plain folder/list load can still show the overlay.
        if self.overlay_dismissed {
            return match &self.libview.loading_label {
                Some(label) => label.clone(),
                None => self.libview.loading_text(),
            };
        }
        if let Some(summary) = &self.refresh_summary {
            summary.clone()
        } else if let Some(label) = &self.libview.loading_label {
            label.clone()
        } else if self.scanning {
            gettext("Reading in your music collection — this may take a moment the first time")
        } else if self.refresh_pending > 0 {
            gettext("Updating …")
        } else {
            self.libview.loading_text()
        }
    }

    /// Rebuilds **all** lists (after switching gallery/list or the
    /// column count). Each reload function fills – depending on `gallery_view` – the
    /// list or the gallery variant.
    pub(crate) fn rebuild_all_lists(&mut self, sender: &ComponentSender<Self>) {
        self.reload_library_overviews();
        self.load_dir(sender);
        self.load_favorites(sender);
        self.load_audiobooks(sender);
        self.load_concerts(sender);
        // Podcasts rebuild themselves in their component (told via
        // `PodcastsInput::SetGalleryView` from the gallery toggle).
    }

    /// Narrow (mobile) mode? Driven purely by the width breakpoint – not by the
    /// split's `collapsed`, which is also forced when the navigation is hidden
    /// (single visible menu item) and would otherwise misreport desktop as
    /// mobile.
    pub(crate) fn is_mobile(&self) -> bool {
        self.nav.narrow.get()
    }

    /// Show detail dialogs on the phone over the **full width**
    /// (bottom sheet); on the desktop floating as before (auto).
    pub(crate) fn adapt_detail_dialog(&self, dialog: &adw::Dialog) {
        crate::ui::widgets::adapt_dialog(dialog, self.is_mobile());
    }

    /// Only upwards, as long as we stay within the start folder.
    pub(crate) fn can_go_up(&self) -> bool {
        // Remote source: going back possible as long as not at the music root.
        if let Some(rel) = &self.files.remote_browse {
            return !rel.is_empty();
        }
        match (&self.files.browse_dir, &self.files.root_dir) {
            (Some(cur), Some(root)) => cur != root && cur.starts_with(root),
            _ => false,
        }
    }

    /// Display name of the active source (for the path bar at the root).
    pub(crate) fn active_source_name(&self) -> String {
        match &self.files.active_source {
            ActiveSource::Primary => gettext("Music"),
            ActiveSource::Source(id) => self
                .files
                .sources
                .iter()
                .find(|s| s.id == *id)
                .map(|s| s.name.clone())
                .unwrap_or_default(),
        }
    }

    /// Label of the path bar (current folder name or hint).
    pub(crate) fn folder_label(&self) -> String {
        // Remote source: last path segment or source name at the root.
        if let Some(rel) = &self.files.remote_browse {
            if rel.is_empty() {
                return self.active_source_name();
            }
            return rel.rsplit('/').next().unwrap_or(rel).to_string();
        }
        match &self.files.browse_dir {
            Some(dir) => dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("/")
                .to_string(),
            None => gettext("No music folder – please set one in settings"),
        }
    }
}
