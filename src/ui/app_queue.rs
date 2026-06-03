//! Queue dialog: the currently playing track is at the top, the following ones
//! can be reordered via a drag handle and removed via a trash button.

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::i18n::gettext;
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
            .vexpand(true)
            .child(&content)
            .build();
        let toolbar = adw::ToolbarView::new();
        let dialog = adw::Dialog::builder()
            .title(&gettext("Queue"))
            .content_width(400)
            .content_height(620)
            .build();

        // Header bar with a trash button at the top left for clearing (with
        // confirmation). After clearing, the dialog closes automatically.
        let header = adw::HeaderBar::new();
        let clear = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text(&gettext("Clear queue"))
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

    /// Rebuilds the queue list: starting from the currently playing track (top),
    /// the following ones with drag handle + trash button.
    pub(crate) fn reload_queue_list(&self, sender: &ComponentSender<Self>) {
        while let Some(child) = self.transport.queue_list.first_child() {
            self.transport.queue_list.remove(&child);
        }
        if self.transport.queue.is_empty() {
            self.transport.queue_list
                .append(&adw::ActionRow::builder().title(&gettext("The queue is empty")).build());
            return;
        }

        let pos = self.transport.queue_pos;
        for (offset, path) in self.transport.queue.iter().skip(pos).enumerate() {
            let qidx = pos + offset;
            let is_current = offset == 0;
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&self.display_name(path)))
                .build();

            if is_current {
                row.set_subtitle(&gettext("Now playing"));
                row.add_prefix(&gtk::Image::from_icon_name("media-playback-start-symbolic"));
            } else {
                // Drag handle (left) for reordering.
                let handle = gtk::Image::from_icon_name("list-drag-handle-symbolic");
                handle.set_tooltip_text(Some(&gettext("Drag to reorder")));
                row.add_prefix(&handle);

                let drag = gtk::DragSource::new();
                drag.set_actions(gtk::gdk::DragAction::MOVE);
                drag.connect_prepare(move |_, _, _| {
                    Some(gtk::gdk::ContentProvider::for_value(&(qidx as i32).to_value()))
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
            }

            // Trash button (right) for removing.
            let trash = gtk::Button::builder()
                .icon_name("user-trash-symbolic")
                .tooltip_text(&gettext("Remove from queue"))
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .build();
            {
                let sender = sender.clone();
                trash.connect_clicked(move |_| sender.input(Msg::QueueRemove(qidx)));
            }
            row.add_suffix(&trash);

            self.transport.queue_list.append(&row);
        }
    }
}
