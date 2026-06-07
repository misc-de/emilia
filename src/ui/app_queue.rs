//! Queue dialog: shows the explicit **user queue** ("Add to queue") – the
//! tracks that play next, ahead of the rest of the currently playing album.
//! Consecutive tracks of the same album collapse into one album row. Every row
//! (single track *and* album) can be reordered via its drag handle and carries
//! its runtime plus a play button (start here now). The whole queue is cleared
//! via the header button (playback keeps running).

use std::path::PathBuf;

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::i18n::{gettext, ngettext_n};
use crate::ui::app::{App, Msg};

impl App {
    /// Opens the queue dialog.
    pub(crate) fn open_queue_dialog(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        // The list is a model widget (rebuilt on changes); detach it from any
        // possibly old dialog before re-attaching.
        if self.transport.queue_list.parent().is_some() {
            self.transport.queue_list.unparent();
        }
        self.reload_queue_list();

        self.transport.queue_list.set_css_classes(&["boxed-list"]);
        self.transport.queue_list.set_valign(gtk::Align::Start);
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        content.append(&self.transport.queue_list);

        let scroller = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .child(&content)
            .build();
        let toolbar = adw::ToolbarView::new();
        // Full height: the dialog always uses the available window height (Adwaita
        // clamps the oversized `content_height` to the window), so the queue list
        // fills the screen and scrolls instead of hugging its content.
        let dialog = adw::Dialog::builder()
            .title(gettext("Queue"))
            .content_width(400)
            .content_height(100000)
            .build();

        // Header bar with a trash button at the top left for clearing (with
        // confirmation). After clearing, the dialog closes automatically.
        let header = adw::HeaderBar::new();
        let clear = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text(gettext("Clear queue"))
            .css_classes(["flat"])
            .build();
        {
            let sender = sender.clone();
            let root = root.clone();
            let dialog = dialog.clone();
            clear.connect_clicked(move |_| {
                let confirm = adw::AlertDialog::new(
                    Some(&gettext("Clear queue?")),
                    Some(&gettext("All tracks will be removed from the queue.")),
                );
                confirm.add_response("cancel", &gettext("Cancel"));
                confirm.add_response("clear", &gettext("Clear"));
                confirm.set_response_appearance("clear", adw::ResponseAppearance::Destructive);
                confirm.set_default_response(Some("cancel"));
                confirm.set_close_response("cancel");
                let sender = sender.clone();
                let dialog = dialog.clone();
                confirm.connect_response(None, move |_, resp| {
                    if resp == "clear" {
                        sender.input(Msg::QueueClear);
                        dialog.close();
                    }
                });
                confirm.present(Some(&root));
            });
        }
        header.pack_start(&clear);
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&scroller));
        dialog.set_child(Some(&toolbar));
        dialog.present(Some(root));
    }

    /// Rebuilds the queue list from the explicit **user queue**. Consecutive
    /// tracks of the same album collapse into a single album row (total
    /// runtime); lone tracks stay individual rows. Every row (single *and*
    /// album) carries a drag handle for reordering – album rows move as one
    /// block – its runtime and a play button (start here now).
    pub(crate) fn reload_queue_list(&self) {
        while let Some(child) = self.transport.queue_list.first_child() {
            self.transport.queue_list.remove(&child);
        }
        if self.transport.user_queue.is_empty() {
            self.transport.queue_list.append(
                &adw::ActionRow::builder()
                    .title(gettext("The queue is empty"))
                    .build(),
            );
            return;
        }

        // Fetch the metadata of the whole queue in one batch query instead of
        // one `track_by_path` per entry (queues can be hundreds long).
        let paths: Vec<String> = self
            .transport
            .user_queue
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        let by_path = self.library.tracks_by_paths(&paths).unwrap_or_default();
        let items: Vec<(usize, Option<String>, Option<String>, i64)> = self
            .transport
            .user_queue
            .iter()
            .enumerate()
            .map(|(idx, path)| {
                let ps = path.to_string_lossy();
                let t = by_path.get(ps.as_ref());
                let album = t
                    .and_then(|t| t.album.clone())
                    .filter(|a| !a.trim().is_empty());
                let artist = t
                    .and_then(|t| t.artist.clone())
                    .filter(|a| !a.trim().is_empty());
                let dur = t
                    .and_then(|t| t.duration_ms)
                    .or_else(|| {
                        // YouTube tracks aren't in `track`; use the cached
                        // duration (stored in seconds) for the runtime display.
                        crate::core::youtube::parse_yt_path(&ps)
                            .and_then(|vid| self.library.yt_duration(&vid).ok().flatten())
                            .map(|secs| secs * 1000)
                    })
                    .unwrap_or(0)
                    .max(0);
                (idx, album, artist, dur)
            })
            .collect();

        // Trailing widgets: runtime + "play from here" button. `start`/`len`
        // identify the queue entry (album rows span `len` tracks).
        let add_tail = |row: &adw::ActionRow, start: usize, len: usize, total_ms: i64| {
            let dur = if total_ms > 0 {
                crate::ui::app::fmt_duration(total_ms)
            } else {
                Default::default()
            };
            row.add_suffix(
                &gtk::Label::builder()
                    .label(&dur)
                    .valign(gtk::Align::Center)
                    .css_classes(["dim-label", "numeric"])
                    .build(),
            );
            let play = gtk::Button::builder()
                .icon_name("media-playback-start-symbolic")
                .tooltip_text(gettext("Play"))
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .build();
            let input = self.input.clone();
            play.connect_clicked(move |_| {
                let _ = input.send(Msg::PlayQueueAt { start, len });
            });
            row.add_suffix(&play);
        };

        // Drag handle (left) + drag source/drop target for reordering. Album
        // rows carry the whole block (`len` tracks); single rows carry one entry.
        // The drag payload is `"start:len"`.
        let add_dnd = |row: &adw::ActionRow, start: usize, len: usize| {
            let handle = gtk::Image::from_icon_name("list-drag-handle-symbolic");
            handle.set_tooltip_text(Some(&gettext("Drag to reorder")));
            row.add_prefix(&handle);

            let payload = format!("{start}:{len}");
            let drag = gtk::DragSource::new();
            drag.set_actions(gtk::gdk::DragAction::MOVE);
            drag.connect_prepare(move |_, _, _| {
                Some(gtk::gdk::ContentProvider::for_value(&payload.to_value()))
            });
            row.add_controller(drag);

            let to = start;
            let input = self.input.clone();
            let drop = gtk::DropTarget::new(String::static_type(), gtk::gdk::DragAction::MOVE);
            drop.connect_drop(move |_, value, _, _| match value.get::<String>() {
                Ok(s) => match s
                    .split_once(':')
                    .and_then(|(a, b)| Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?)))
                {
                    Some((from, len)) => {
                        let _ = input.send(Msg::QueueMoveRange { from, len, to });
                        true
                    }
                    None => false,
                },
                Err(_) => false,
            });
            row.add_controller(drop);
        };

        // Render: consecutive tracks of the same album collapse into one album
        // row (total runtime); lone tracks stay individual rows.
        let mut gi = 0;
        while gi < items.len() {
            let album = items[gi].1.clone();
            let mut end = gi + 1;
            if album.is_some() {
                while end < items.len() && items[end].1 == album {
                    end += 1;
                }
            }
            let group = &items[gi..end];
            let start_idx = group[0].0;
            let len = group.len();

            if len >= 2 {
                // --- Album row (moves as one block of `len` tracks). ---
                let total: i64 = group.iter().map(|g| g.3).sum();
                let count = ngettext_n("{n} track", "{n} tracks", len as u32);
                let artist0 = group[0].2.clone();
                let group_artist = if group.iter().all(|g| g.2 == artist0) {
                    artist0
                } else {
                    Some(gettext("Various artists"))
                };
                let subtitle = match group_artist {
                    Some(a) => format!("{a} · {count}"),
                    None => count,
                };
                let row = adw::ActionRow::builder()
                    .title(gtk::glib::markup_escape_text(&album.unwrap_or_default()))
                    .build();
                row.set_subtitle(&gtk::glib::markup_escape_text(&subtitle));
                let cover = self.entry_cover(
                    "track",
                    &self.transport.user_queue[start_idx].to_string_lossy(),
                    false,
                );
                row.add_prefix(&crate::ui::app::cover_widget(
                    cover.as_deref(),
                    "media-optical-symbolic",
                ));
                add_dnd(&row, start_idx, len);
                add_tail(&row, start_idx, len, total);
                self.transport.queue_list.append(&row);
            } else {
                // --- Single track row. ---
                let path = &self.transport.user_queue[start_idx];
                let row = adw::ActionRow::builder()
                    .title(gtk::glib::markup_escape_text(&self.display_name(path)))
                    .build();
                let cover = self.entry_cover("track", &path.to_string_lossy(), false);
                row.add_prefix(&crate::ui::app::cover_widget(
                    cover.as_deref(),
                    "audio-x-generic-symbolic",
                ));
                add_dnd(&row, start_idx, 1);
                add_tail(&row, start_idx, 1, group[0].3);
                self.transport.queue_list.append(&row);
            }
            gi = end;
        }
    }

    /// Clear the explicit user queue (the playing context keeps running).
    pub(crate) fn on_queue_clear(&mut self) {
        // Clear only the explicit user queue; the currently playing
        // album/track (the context) keeps running untouched.
        self.transport.user_queue.clear();
        self.reload_queue_list();
        self.refresh_queue_icons();
        self.save_queue();
        self.toast(&gettext("Queue cleared"));
    }

    /// Reorder the user queue: move the `len`-track block at `from` to `to`
    /// (album rows move as one block).
    pub(crate) fn on_queue_move_range(&mut self, from: usize, len: usize, to: usize) {
        let n = self.transport.user_queue.len();
        // Dropping a block onto itself is a no-op.
        if from < n && len >= 1 && !(to >= from && to < from + len) {
            let len = len.min(n - from);
            let block: Vec<PathBuf> = self.transport.user_queue.drain(from..from + len).collect();
            // After removal everything past the block shifts left by `len`.
            let insert_at =
                if to > from { to - len } else { to }.min(self.transport.user_queue.len());
            for (i, p) in block.into_iter().enumerate() {
                self.transport.user_queue.insert(insert_at + i, p);
            }
            self.reload_queue_list();
            self.refresh_queue_icons();
            self.save_queue();
        }
    }
}
