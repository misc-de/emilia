//! Queue dialog: shows the explicit **user queue** ("Add to queue") – the
//! tracks that play next, ahead of the rest of the currently playing album.
//! Consecutive tracks of the same album collapse into one album row. Every row
//! (single track *and* album) can be reordered via its drag handle and carries
//! its runtime plus a play button (start here now). The whole queue is cleared
//! via the header button (playback keeps running).

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::i18n::{gettext, ngettext_n};
use crate::ui::app::{App, Msg};
use crate::ui::app_favorites::{entry_is_active, mark_key};
use crate::ui::app_playback::TransportMsg;

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
                        sender.input(Msg::Transport(TransportMsg::QueueClear));
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
        crate::ui::app_helpers::close_on_click_outside(&dialog);
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
        // The controls of the old rows are gone with them.
        self.transport.queue_marks.clear();
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
        // What is running right now, asked once for the whole rebuild: the rows
        // mark it the same way every other list does (see
        // [`crate::ui::app_favorites::entry_is_active`]).
        let playing = self.mini.playing;
        let cur_path = self.transport.playing_path.clone();
        let cur_album = self.playing_album();
        let add_tail =
            |row: &adw::ActionRow, start: usize, len: usize, total_ms: i64, key: String| {
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
                // A queue entry that is the one playing shows a pause icon, like the
                // same track does in every other list; pressing it then toggles
                // pause/resume instead of re-queuing the block.
                let active = match key.split_once('\u{1}') {
                    Some((scope, k)) => {
                        entry_is_active(cur_path.as_deref(), cur_album.as_deref(), scope, k)
                    }
                    None => false,
                };
                let play = crate::ui::play_mark::button(&gettext("Play"), active, playing);
                let input = self.input.clone();
                play.connect_clicked(move |_| {
                    let _ = input.send(Msg::Transport(TransportMsg::PlayQueueAt { start, len }));
                });
                self.transport.queue_marks.add(key, &play);
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
                        let _ = input.send(Msg::Transport(TransportMsg::QueueMoveRange {
                            from,
                            len,
                            to,
                        }));
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
                let album_name = album.unwrap_or_default();
                let row = adw::ActionRow::builder()
                    .title(gtk::glib::markup_escape_text(&album_name))
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
                add_tail(&row, start_idx, len, total, mark_key("album", &album_name));
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
                add_tail(
                    &row,
                    start_idx,
                    1,
                    group[0].3,
                    mark_key("track", &path.to_string_lossy()),
                );
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
    /// (album rows move as one block). See [`move_range`] for the index rules.
    pub(crate) fn on_queue_move_range(&mut self, from: usize, len: usize, to: usize) {
        if move_range(&mut self.transport.user_queue, from, len, to) {
            self.reload_queue_list();
            self.refresh_queue_icons();
            self.save_queue();
        }
    }
}

/// Moves the `len`-item block starting at `from` so that it lands in front of
/// the item that sat at index `to` *before* the move (`to == items.len()`
/// appends). `len` is clamped to the end of the list. Returns whether the block
/// was re-inserted; nothing happens when `from` is out of range, `len` is zero
/// or `to` points into the block itself (dropping a block onto itself).
fn move_range<T>(items: &mut Vec<T>, from: usize, len: usize, to: usize) -> bool {
    let n = items.len();
    // Dropping a block onto itself is a no-op.
    if from >= n || len == 0 || (to >= from && to < from + len) {
        return false;
    }
    let len = len.min(n - from);
    let block: Vec<T> = items.drain(from..from + len).collect();
    // After removal everything past the block shifts left by `len`.
    let insert_at = if to > from { to - len } else { to }.min(items.len());
    for (i, p) in block.into_iter().enumerate() {
        items.insert(insert_at + i, p);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::move_range;

    fn items() -> Vec<char> {
        vec!['a', 'b', 'c', 'd', 'e']
    }

    #[test]
    fn moves_a_single_item_forward() {
        let mut v = items();
        assert!(move_range(&mut v, 0, 1, 2));
        assert_eq!(v, ['b', 'a', 'c', 'd', 'e']);
        let mut v = items();
        assert!(move_range(&mut v, 1, 1, 5));
        assert_eq!(v, ['a', 'c', 'd', 'e', 'b']);
    }

    #[test]
    fn moves_a_single_item_backward() {
        let mut v = items();
        assert!(move_range(&mut v, 3, 1, 1));
        assert_eq!(v, ['a', 'd', 'b', 'c', 'e']);
        let mut v = items();
        assert!(move_range(&mut v, 4, 1, 0));
        assert_eq!(v, ['e', 'a', 'b', 'c', 'd']);
    }

    #[test]
    fn moves_a_block_as_one_unit() {
        let mut v = items();
        assert!(move_range(&mut v, 1, 2, 4));
        assert_eq!(v, ['a', 'd', 'b', 'c', 'e']);
        let mut v = items();
        assert!(move_range(&mut v, 1, 2, 5));
        assert_eq!(v, ['a', 'd', 'e', 'b', 'c']);
        let mut v = items();
        assert!(move_range(&mut v, 3, 2, 0));
        assert_eq!(v, ['d', 'e', 'a', 'b', 'c']);
    }

    #[test]
    fn dropping_a_block_onto_itself_is_a_no_op() {
        for to in 1..=2 {
            let mut v = items();
            assert!(!move_range(&mut v, 1, 2, to), "to={to}");
            assert_eq!(v, items());
        }
        // Dropping right behind the block re-inserts it where it was.
        let mut v = items();
        assert!(move_range(&mut v, 1, 2, 3));
        assert_eq!(v, items());
    }

    #[test]
    fn out_of_range_arguments_leave_the_list_untouched() {
        let mut v = items();
        assert!(!move_range(&mut v, 5, 1, 0));
        assert!(!move_range(&mut v, 0, 0, 3));
        assert_eq!(v, items());
        let mut empty: Vec<char> = Vec::new();
        assert!(!move_range(&mut empty, 0, 1, 0));
        assert!(empty.is_empty());
    }

    #[test]
    fn overlong_lengths_and_targets_are_clamped() {
        // The block runs to the end of the list.
        let mut v = items();
        assert!(move_range(&mut v, 3, 10, 0));
        assert_eq!(v, ['d', 'e', 'a', 'b', 'c']);
        // A target past the end appends.
        let mut v = items();
        assert!(move_range(&mut v, 0, 1, 42));
        assert_eq!(v, ['b', 'c', 'd', 'e', 'a']);
    }
}
