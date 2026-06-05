//! Queue dialog: the currently playing track is at the top, the following ones
//! can be reordered via a drag handle. Each row shows its runtime and a play
//! button on the right (the current one toggles play/pause, others jump to that
//! entry). The whole queue is cleared via the header button.

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
        self.reload_queue_list(sender);

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
            .propagate_natural_height(true)
            .vexpand(true)
            .child(&content)
            .build();
        let toolbar = adw::ToolbarView::new();
        // Height follows the content (natural height of the scroller); only the
        // width is fixed. Long queues grow to the window height, then scroll.
        let dialog = adw::Dialog::builder()
            .title(gettext("Queue"))
            .content_width(400)
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

    /// Rebuilds the queue list: starting from the currently playing track (top).
    /// Consecutive tracks of the same album collapse into a single album row
    /// (showing the total runtime); lone tracks stay individual rows with a drag
    /// handle. Each row carries its runtime and a play button on the right.
    pub(crate) fn reload_queue_list(&self, sender: &ComponentSender<Self>) {
        while let Some(child) = self.transport.queue_list.first_child() {
            self.transport.queue_list.remove(&child);
        }
        if self.transport.queue.is_empty() {
            self.transport.queue_list.append(
                &adw::ActionRow::builder()
                    .title(gettext("The queue is empty"))
                    .build(),
            );
            return;
        }

        let pos = self.transport.queue_pos;
        // Metadata for the visible slice (current track onward): album/artist/runtime.
        let items: Vec<(usize, Option<String>, Option<String>, i64)> = self
            .transport
            .queue
            .iter()
            .enumerate()
            .skip(pos)
            .map(|(idx, path)| {
                let ps = path.to_string_lossy();
                let t = self.library.track_by_path(&ps).ok().flatten();
                let album = t
                    .as_ref()
                    .and_then(|t| t.album.clone())
                    .filter(|a| !a.trim().is_empty());
                let artist = t
                    .as_ref()
                    .and_then(|t| t.artist.clone())
                    .filter(|a| !a.trim().is_empty());
                let dur = t
                    .as_ref()
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

        // Shared trailing widgets: runtime + play/pause button (far right, like
        // single-song rows). `idx` is the queue index playback should start at.
        let add_tail = |row: &adw::ActionRow, idx: usize, total_ms: i64, is_current: bool| {
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
                .icon_name(if is_current && self.mini.playing {
                    "media-playback-pause-symbolic"
                } else {
                    "media-playback-start-symbolic"
                })
                .tooltip_text(gettext("Play/Pause"))
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .build();
            let sender = sender.clone();
            play.connect_clicked(move |_| {
                sender.input(if is_current {
                    Msg::TogglePlay
                } else {
                    Msg::PlayQueueAt(idx)
                });
            });
            row.add_suffix(&play);
        };

        // Render: consecutive tracks of the same album collapse into one album
        // row (total runtime); lone tracks stay individual rows. The first row is
        // always the current track/album ("Now playing").
        let mut gi = 0;
        let mut first = true;
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
            let is_current = first;
            first = false;

            if group.len() >= 2 {
                // --- Album row: a single entry with the total runtime. ---
                let total: i64 = group.iter().map(|g| g.3).sum();
                let count = ngettext_n("{n} track", "{n} tracks", group.len() as u32);
                let artist0 = group[0].2.clone();
                let group_artist = if group.iter().all(|g| g.2 == artist0) {
                    artist0
                } else {
                    Some(gettext("Various artists"))
                };
                let subtitle = if is_current {
                    format!("{} · {count}", gettext("Now playing"))
                } else {
                    match group_artist {
                        Some(a) => format!("{a} · {count}"),
                        None => count,
                    }
                };
                let row = adw::ActionRow::builder()
                    .title(gtk::glib::markup_escape_text(&album.unwrap_or_default()))
                    .build();
                row.set_subtitle(&gtk::glib::markup_escape_text(&subtitle));
                let cover = self.entry_cover(
                    "track",
                    &self.transport.queue[start_idx].to_string_lossy(),
                    false,
                );
                row.add_prefix(&crate::ui::app::cover_widget(
                    cover.as_deref(),
                    "media-optical-symbolic",
                ));
                add_tail(&row, start_idx, total, is_current);
                self.transport.queue_list.append(&row);
            } else {
                // --- Single track row. ---
                let path = &self.transport.queue[start_idx];
                let row = adw::ActionRow::builder()
                    .title(gtk::glib::markup_escape_text(&self.display_name(path)))
                    .build();
                let cover = self.entry_cover("track", &path.to_string_lossy(), false);
                row.add_prefix(&crate::ui::app::cover_widget(
                    cover.as_deref(),
                    "audio-x-generic-symbolic",
                ));
                if is_current {
                    row.set_subtitle(&gettext("Now playing"));
                }
                // Drag handle (left) for reordering individual tracks. The
                // now-playing track is reorderable too: `QueueMove` keeps
                // `queue_pos` pointing at it, so playback is unaffected.
                let handle = gtk::Image::from_icon_name("list-drag-handle-symbolic");
                handle.set_tooltip_text(Some(&gettext("Drag to reorder")));
                row.add_prefix(&handle);

                let qidx = start_idx;
                let drag = gtk::DragSource::new();
                drag.set_actions(gtk::gdk::DragAction::MOVE);
                drag.connect_prepare(move |_, _, _| {
                    Some(gtk::gdk::ContentProvider::for_value(
                        &(qidx as i32).to_value(),
                    ))
                });
                row.add_controller(drag);

                let drop = gtk::DropTarget::new(i32::static_type(), gtk::gdk::DragAction::MOVE);
                {
                    let sender = sender.clone();
                    drop.connect_drop(move |_, value, _, _| match value.get::<i32>() {
                        Ok(from) => {
                            sender.input(Msg::QueueMove {
                                from: from as usize,
                                to: qidx,
                            });
                            true
                        }
                        Err(_) => false,
                    });
                }
                row.add_controller(drop);
                add_tail(&row, start_idx, group[0].3, is_current);
                self.transport.queue_list.append(&row);
            }
            gi = end;
        }
    }
}
