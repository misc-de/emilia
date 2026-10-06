//! Station half of [`StreamPage`]'s inherent impl: the header sort control,
//! the station list and logo gallery, the station detail / rename / logo
//! dialogs, and the "+" flow (directory search or stream address). The
//! struct, its messages and the `Component` impl stay in
//! [`crate::ui::stream_page`].

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use std::cell::RefCell;
use std::rc::Rc;

use crate::core::db::Library;
use crate::i18n::{gettext, gettext_f};
use crate::ui::app::{SortCrit, StreamView};
use crate::ui::app_gallery::{gallery_cell, spawn_gallery_decode};
use crate::ui::app_helpers::{cover_widget, on_secondary_click};
use crate::ui::app_sort::{SortToggle, sort_popover};
use crate::ui::entry_row::EntryRow;
use crate::ui::stream_page::{STREAM_ICON, StreamCmd, StreamInput, StreamOutput, StreamPage};
use crate::ui::stream_page_logic::{
    search_result_subtitle, sort_stations, station_headers, stream_subtitle,
};
use crate::ui::widgets::{action_row, detail_box, present_detail_refreshable};

/// Fetches the station logos not yet in the cache (worker thread — network).
/// Returns whether any came in, i.e. whether a redraw would show something new.
pub(super) fn cache_missing_station_logos() -> bool {
    let Ok(lib) = Library::open() else {
        return false;
    };
    lib.streams()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|st| st.favicon)
        .filter(|url| crate::core::online::station_image_path(url).is_none())
        .filter(|url| crate::core::online::cache_station_image(url).is_some())
        .count()
        > 0
}

/// Safety prompt before a destructive page action; sends `then` to ourselves on
/// confirm (the actual deletion is still deferred via an undo toast afterwards).
pub(super) fn confirm_delete(
    root: &adw::ApplicationWindow,
    sender: &ComponentSender<StreamPage>,
    heading: &str,
    label: &str,
    then: StreamInput,
) {
    let confirm = adw::AlertDialog::new(Some(heading), None);
    confirm.add_response("cancel", &gettext("Cancel"));
    confirm.add_response("ok", label);
    confirm.set_response_appearance("ok", adw::ResponseAppearance::Destructive);
    confirm.set_default_response(Some("cancel"));
    confirm.set_close_response("cancel");
    let sender = sender.clone();
    let then = std::cell::RefCell::new(Some(then));
    confirm.connect_response(None, move |_, resp| {
        if resp == "ok"
            && let Some(t) = then.borrow_mut().take()
        {
            sender.input(t);
        }
    });
    confirm.present(Some(root));
}

impl StreamPage {
    /// Show detail dialogs as bottom sheets on the phone.
    pub(super) fn adapt_detail_dialog(&self, dialog: &adw::Dialog) {
        crate::ui::widgets::adapt_dialog(dialog, self.mobile);
    }

    /// (Re)builds the header sort button (direction icon + criteria popover) for
    /// the currently visible sub-view. Stations sort by name; recordings by name /
    /// recording date / length.
    pub(super) fn rebuild_sort(&self, sender: &ComponentSender<Self>) {
        let (state, crits, no_group) = match self.stream_view {
            StreamView::Channels => (
                self.stations_sort,
                vec![(SortCrit::Name, gettext("Name"))],
                self.stations_no_group,
            ),
            StreamView::Recordings => (
                self.recordings_sort,
                vec![
                    (SortCrit::Name, gettext("Name")),
                    (SortCrit::Release, gettext("Date")),
                    (SortCrit::Length, gettext("Length")),
                ],
                self.recordings_no_group,
            ),
            StreamView::Heard => (
                self.heard_sort,
                vec![
                    (SortCrit::Name, gettext("Name")),
                    (SortCrit::Release, gettext("Date")),
                ],
                self.heard_no_group,
            ),
        };
        let (crit, desc) = state;
        let input = sender.input_sender().clone();
        let group_input = input.clone();
        let mut toggles = vec![SortToggle {
            label: gettext("Without grouping"),
            active: no_group,
            on_toggle: Box::new(move |off| {
                let _ = group_input.send(StreamInput::SetNoGroup(off));
            }),
            sub: false,
        }];
        // The stations sub-view additionally offers a logo gallery (recordings
        // carry no covers, so they group but never gallery).
        if matches!(self.stream_view, StreamView::Channels) {
            let gallery_input = input.clone();
            toggles.push(SortToggle {
                label: gettext("Gallery view"),
                active: self.stations_gallery,
                on_toggle: Box::new(move |on| {
                    let _ = gallery_input.send(StreamInput::SetGallery(on));
                }),
                sub: false,
            });
            let desc_input = input.clone();
            toggles.push(SortToggle {
                label: gettext("Show description"),
                active: self.stations_gallery_desc,
                on_toggle: Box::new(move |on| {
                    let _ = desc_input.send(StreamInput::SetGalleryDesc(on));
                }),
                sub: true,
            });
        }
        let popover = sort_popover(
            &crits,
            crit,
            desc,
            move |crit, desc| {
                let _ = input.send(StreamInput::SetSort(crit, desc));
            },
            toggles,
        );
        // Both sub-views sort (stations by name; recordings by name/date/length);
        // show the button only when the visible sub-view has entries.
        let visible = match self.stream_view {
            StreamView::Channels => !self.stream_items.is_empty(),
            StreamView::Recordings => !self.recording_items.is_empty(),
            StreamView::Heard => !self.heard_items.is_empty(),
        };
        *self.sort_slot.borrow_mut() = visible.then_some((popover, desc));
        let _ = sender.output(StreamOutput::SortChanged);
    }

    /// Gallery variant of the stations: a grid of station logos. Tap opens the
    /// station's detail/replay; long press the detail dialog — same as the rows.
    pub(super) fn fill_streams_gallery(&self, sender: &ComponentSender<Self>) {
        let fb = &self.streams_gallery;
        crate::ui::widgets::reset_gallery_grid(fb, self.gallery_columns);
        let mut to_decode: Vec<(String, gtk::Picture)> = Vec::new();
        for st in self.stream_items.clone() {
            let logo = st
                .favicon
                .as_deref()
                .and_then(crate::core::online::station_image_path);
            let (cell, pic) = gallery_cell(
                logo.as_deref(),
                STREAM_ICON,
                &st.name,
                self.stations_gallery_desc,
            );
            if let (Some(path), Some(pic)) = (logo.as_deref(), pic)
                && crate::ui::widgets::cached_thumb(path).is_none()
            {
                to_decode.push((path.to_string(), pic));
            }
            let id = st.id;
            let click = gtk::GestureClick::new();
            {
                let sender = sender.clone();
                click.connect_released(move |g, n, _, _| {
                    if n == 1 {
                        g.set_state(gtk::EventSequenceState::Claimed);
                        sender.input(StreamInput::OpenStream(id));
                    }
                });
            }
            cell.add_controller(click);
            on_secondary_click(&cell, {
                let sender = sender.clone();
                move || sender.input(StreamInput::OpenStream(id))
            });
            let long_press = gtk::GestureLongPress::new();
            {
                let sender = sender.clone();
                long_press.connect_pressed(move |g, _, _| {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    sender.input(StreamInput::OpenStream(id));
                });
            }
            cell.add_controller(long_press);
            fb.append(&cell);
        }
        spawn_gallery_decode(to_decode);
    }

    pub(super) fn reload_streams(&mut self, sender: &ComponentSender<Self>) {
        self.stream_items = self.library.streams().unwrap_or_default();
        sort_stations(&mut self.stream_items, self.stations_sort.1);
        // Refresh the title-bar sort control (visibility depends on emptiness);
        // done before the gallery early-return below so both paths cover it.
        self.rebuild_sort(sender);
        // Alphabetical headings (by name) for the list; none in gallery mode.
        *self.station_headers.borrow_mut() =
            station_headers(&self.stream_items, self.stations_no_group);
        if self.stations_gallery {
            self.fill_streams_gallery(sender);
            return;
        }
        self.stream_marks.clear();
        while let Some(child) = self.streams_list.first_child() {
            self.streams_list.remove(&child);
        }
        for st in self.stream_items.clone() {
            // Not activatable: like a library track, the station plays via its
            // play button; long press / right click opens the detail view.
            let id = st.id;
            let logo = st
                .favicon
                .as_deref()
                .and_then(crate::core::online::station_image_path);
            let row = EntryRow::new(&st.name)
                .subtitle(&stream_subtitle(&st).unwrap_or_default())
                .cover(logo.as_deref(), STREAM_ICON)
                .play_button(
                    &gettext("Play/Pause"),
                    self.playing_stream == Some(id),
                    self.playing,
                    {
                        let sender = sender.clone();
                        move || {
                            let _ = sender.output(StreamOutput::ToggleStream(id));
                        }
                    },
                )
                .marked_in(&self.stream_marks, id.to_string())
                .on_detail({
                    let sender = sender.clone();
                    move || sender.input(StreamInput::OpenStream(id))
                })
                .build();
            self.streams_list.append(&row);
        }
        self.streams_list.invalidate_headers();
        self.refresh_stream_icons();
    }

    /// Refreshes the Play/Pause icons of the station rows.
    pub(super) fn refresh_stream_icons(&self) {
        let cur = self.playing_stream.map(|id| id.to_string());
        self.stream_marks
            .apply_all(self.playing, |key| cur.as_deref() == Some(key));
    }

    /// Keeps the play/pause icon of each recording row in sync.
    pub(super) fn refresh_recording_icons(&self) {
        let cur = self.playing_path.clone();
        self.rec_marks
            .apply_all(self.playing, |key| cur.as_deref() == Some(key));
    }

    /// Station detail dialog: replay (buffer), rename, remove.
    pub(super) fn open_stream(&self, sender: &ComponentSender<Self>, id: i64) {
        let Some(root) = self.window.clone() else {
            return;
        };
        let Some(st) = self.stream_items.iter().find(|s| s.id == id).cloned() else {
            return;
        };
        let dialog = adw::Dialog::builder().title(&st.name).build();
        self.adapt_detail_dialog(&dialog);
        let content = detail_box();

        let info = adw::PreferencesGroup::new();
        let head = adw::ActionRow::builder()
            .title(gtk::glib::markup_escape_text(&st.name))
            .build();
        if let Some(sub) = stream_subtitle(&st) {
            head.set_subtitle(&gtk::glib::markup_escape_text(&sub));
        }
        let logo = st
            .favicon
            .as_deref()
            .and_then(crate::core::online::station_image_path);
        content.append(&crate::ui::widgets::detail_cover(
            logo.as_deref(),
            STREAM_ICON,
        ));
        info.add(&head);
        content.append(&info);

        let actions = adw::PreferencesGroup::new();
        if self.buffer_minutes > 5 {
            let replay = action_row(&gettext("Replay (buffer)"), "media-seek-backward-symbolic");
            {
                let (sender, dialog) = (sender.clone(), dialog.clone());
                replay.connect_activated(move |_| {
                    let _ = sender.output(StreamOutput::OpenReplay(id));
                    dialog.close();
                });
            }
            actions.add(&replay);
        }
        let rename = action_row(&gettext("Rename station"), "document-edit-symbolic");
        {
            let (sender, dialog) = (sender.clone(), dialog.clone());
            rename.connect_activated(move |_| {
                sender.input(StreamInput::RenameDialog(id));
                dialog.close();
            });
        }
        actions.add(&rename);
        let logo_row = action_row(&gettext("Change logo"), "image-x-generic-symbolic");
        {
            let (sender, dialog) = (sender.clone(), dialog.clone());
            logo_row.connect_activated(move |_| {
                sender.input(StreamInput::LogoDialog(id));
                dialog.close();
            });
        }
        actions.add(&logo_row);
        let eq = action_row(
            &gettext("Equalizer settings"),
            "multimedia-equalizer-symbolic",
        );
        {
            let (sender, dialog) = (sender.clone(), dialog.clone());
            eq.connect_activated(move |_| {
                let _ = sender.output(StreamOutput::OpenEqualizer(id));
                dialog.close();
            });
        }
        actions.add(&eq);
        let share = action_row(&gettext("Share"), "emilia-share-symbolic");
        {
            let (sender, dialog) = (sender.clone(), dialog.clone());
            share.connect_activated(move |_| {
                let _ = sender.output(StreamOutput::Share(Box::new(
                    crate::core::sync::share::Selection {
                        stations: vec![id],
                        ..Default::default()
                    },
                )));
                dialog.close();
            });
        }
        actions.add(&share);
        let remove = action_row(&gettext("Remove station"), "user-trash-symbolic");
        {
            let (sender, dialog, root) = (sender.clone(), dialog.clone(), root.clone());
            remove.connect_activated(move |_| {
                dialog.close();
                confirm_delete(
                    &root,
                    &sender,
                    &gettext("Remove this station?"),
                    &gettext("Remove"),
                    StreamInput::Delete(id),
                );
            });
        }
        actions.add(&remove);
        content.append(&actions);

        {
            let sender = sender.clone();
            present_detail_refreshable(&dialog, &content, &root, move || {
                sender.input(StreamInput::RefreshStream(id));
            });
        }
    }

    /// Dialog: rename a station (name prefilled).
    pub(super) fn open_rename_stream_dialog(&self, sender: &ComponentSender<Self>, id: i64) {
        let Some(root) = self.window.clone() else {
            return;
        };
        let current = self
            .stream_items
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.name.clone())
            .unwrap_or_default();
        let dialog = adw::AlertDialog::new(Some(&gettext("Rename station")), None);
        let entry = gtk::Entry::builder()
            .text(&current)
            .activates_default(true)
            .build();
        crate::ui::widgets::no_autofocus(&entry);
        dialog.set_extra_child(Some(&entry));
        dialog.add_responses(&[
            ("cancel", &gettext("Cancel")),
            ("rename", &gettext("Rename")),
        ]);
        dialog.set_response_appearance("rename", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("rename"));
        {
            let sender = sender.clone();
            dialog.connect_response(None, move |_, resp| {
                if resp == "rename" {
                    sender.input(StreamInput::Rename {
                        id,
                        name: entry.text().to_string(),
                    });
                }
            });
        }
        dialog.present(Some(&root));
    }

    /// Dialog: change a station's logo — search it online, pick an image file,
    /// or enter an image URL (an empty URL removes the logo).
    pub(super) fn open_logo_dialog(&self, sender: &ComponentSender<Self>, id: i64) {
        let Some(root) = self.window.clone() else {
            return;
        };
        let current = self
            .stream_items
            .iter()
            .find(|s| s.id == id)
            .and_then(|s| s.favicon.clone())
            .filter(|f| !crate::core::online::is_local_station_logo(f))
            .unwrap_or_default();
        let dialog = adw::AlertDialog::new(
            Some(&gettext("Change logo")),
            Some(&gettext(
                "Search the station's website for its logo, choose an image file, \
                 or enter the address of an image.",
            )),
        );
        let entry = gtk::Entry::builder()
            .text(&current)
            .placeholder_text(gettext("Image URL (https://…)"))
            .input_purpose(gtk::InputPurpose::Url)
            .activates_default(true)
            .build();
        crate::ui::widgets::no_autofocus(&entry);
        dialog.set_extra_child(Some(&entry));
        dialog.add_responses(&[
            ("cancel", &gettext("Cancel")),
            ("search", &gettext("Search online")),
            ("file", &gettext("Choose image…")),
            ("apply", &gettext("Use URL")),
        ]);
        dialog.set_response_appearance("apply", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("apply"));
        {
            let (sender, root) = (sender.clone(), root.clone());
            dialog.connect_response(None, move |_, resp| match resp {
                "search" => sender.input(StreamInput::FindLogo(id, true)),
                "apply" => sender.input(StreamInput::SetLogoUrl(id, Some(entry.text().into()))),
                "file" => {
                    let filter = gtk::FileFilter::new();
                    filter.add_pixbuf_formats();
                    filter.set_name(Some(&gettext("Images")));
                    let filters = gtk::gio::ListStore::new::<gtk::FileFilter>();
                    filters.append(&filter);
                    let chooser = gtk::FileDialog::builder()
                        .title(gettext("Choose logo"))
                        .filters(&filters)
                        .build();
                    let sender = sender.clone();
                    chooser.open(Some(&root), gtk::gio::Cancellable::NONE, move |res| {
                        if let Some(path) = res.ok().and_then(|f| f.path()) {
                            sender.input(StreamInput::SetLogoFile(id, path));
                        }
                    });
                }
                _ => {}
            });
        }
        dialog.present(Some(&root));
    }

    /// The "+": a centered choice modal like the Files "+" — search the
    /// worldwide station directory, or enter a stream address by hand.
    pub(super) fn open_add_stream_dialog(&self, sender: &ComponentSender<Self>) {
        let Some(root) = self.window.clone() else {
            return;
        };
        let slot = self.stream_search.clone();
        let (sender, win) = (sender.clone(), root.clone());
        let dialog = crate::ui::widgets::choice_modal(
            &gettext("Add station"),
            &[
                ("search", gettext("Search stations")),
                ("url", gettext("Enter stream address")),
            ],
            "search",
            move |resp| match resp {
                "search" => open_stream_search_modal(&sender, &slot, &win),
                "url" => {
                    let sender = sender.clone();
                    let (dialog, _) = crate::ui::widgets::entry_modal(
                        &gettext("Enter stream address"),
                        &gettext("Stream address (URL)"),
                        &gettext("Add"),
                        move |url| sender.input(StreamInput::AddUrl(url)),
                    );
                    dialog.present(Some(&win));
                }
                _ => {}
            },
        );
        dialog.present(Some(&root));
    }

    /// Redraws the results list in the open add dialog.
    pub(super) fn rebuild_stream_search_results(&self, sender: &ComponentSender<Self>) {
        let guard = self.stream_search.borrow();
        let Some((dialog, list)) = guard.as_ref() else {
            return;
        };
        while let Some(child) = list.first_child() {
            list.remove(&child);
        }
        list.set_visible(true);

        if self.stream_search_results.is_empty() {
            let row = if self.stream_search_failed {
                let r = adw::ActionRow::builder()
                    .title(gettext("Station service unreachable"))
                    .subtitle(gettext("Check your connection and try again"))
                    .build();
                r.set_subtitle_lines(2);
                r
            } else {
                adw::ActionRow::builder()
                    .title(gettext("No stations found"))
                    .build()
            };
            row.set_sensitive(false);
            list.append(&row);
            return;
        }

        for (i, r) in self.stream_search_results.iter().enumerate() {
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&r.name))
                .activatable(true)
                .build();
            if let Some(sub) = search_result_subtitle(r) {
                row.set_subtitle(&gtk::glib::markup_escape_text(&sub));
            }
            let logo = r
                .favicon
                .as_deref()
                .and_then(crate::core::online::station_image_path);
            row.add_prefix(&cover_widget(logo.as_deref(), STREAM_ICON));
            row.add_suffix(&gtk::Image::from_icon_name("list-add-symbolic"));
            {
                let (sender, dialog) = (sender.clone(), dialog.clone());
                row.connect_activated(move |_| {
                    sender.input(StreamInput::AddResult(i));
                    dialog.close();
                });
            }
            list.append(&row);
        }
    }

    /// Adds a search result as a station and loads its logo in the background.
    pub(super) fn add_stream_result(&mut self, sender: &ComponentSender<Self>, index: usize) {
        let Some(r) = self.stream_search_results.get(index).cloned() else {
            return;
        };
        match self.library.add_stream(
            &r.name,
            &r.url,
            r.favicon.as_deref(),
            r.tags.as_deref(),
            r.country.as_deref(),
            r.codec.as_deref(),
            r.bitrate,
        ) {
            Ok(id) => {
                self.reload_streams(sender);
                let _ = sender.output(StreamOutput::Toast(gettext_f(
                    "Added: {n}",
                    &[("n", &r.name)],
                )));
                // Radio-Browser's favicon is often missing or dead (404) —
                // then search the station's website for a logo instead.
                let fav = r.favicon.clone();
                let url = r.url.clone();
                sender.spawn_command(move |out| {
                    let cached = fav
                        .as_deref()
                        .and_then(crate::core::online::cache_station_image)
                        .is_some();
                    let _ = out.send(if cached {
                        StreamCmd::ReloadStreams
                    } else {
                        StreamCmd::LogoFound {
                            id,
                            favicon: crate::core::station_logo::find_logo(&url),
                            report: false,
                        }
                    });
                });
            }
            Err(_) => {
                let _ = sender.output(StreamOutput::Toast(gettext("Could not add station")));
            }
        }
    }

    /// Add a station directly from a URL.
    pub(super) fn stream_add_url(&mut self, sender: &ComponentSender<Self>, url: String) {
        let url = url.trim().to_string();
        if !url.is_empty() {
            let name = crate::core::streaming::name_from_url(&url);
            match self
                .library
                .add_stream(&name, &url, None, None, None, None, None)
            {
                Ok(id) => {
                    self.reload_streams(sender);
                    let _ = sender.output(StreamOutput::Toast(gettext("Station added")));
                    // A station added by URL brings no logo — look one up.
                    sender.input(StreamInput::FindLogo(id, false));
                }
                Err(_) => {
                    let _ = sender.output(StreamOutput::Toast(gettext("Could not add station")));
                }
            }
        }
    }
}

/// Second step of the stations "+": the directory search. Registers the dialog
/// in `slot` so the worker's results land in its list.
fn open_stream_search_modal(
    sender: &ComponentSender<StreamPage>,
    slot: &Rc<RefCell<Option<(adw::Dialog, gtk::ListBox)>>>,
    root: &impl IsA<gtk::Widget>,
) {
    let sender = sender.clone();
    let (dialog, entry, results) = crate::ui::widgets::search_modal(
        &gettext("Search stations"),
        &gettext("Station name …"),
        move |term| sender.input(StreamInput::Search(term)),
    );
    let dialog: adw::Dialog = dialog.upcast();
    *slot.borrow_mut() = Some((dialog.clone(), results));
    {
        let slot = slot.clone();
        dialog.connect_closed(move |this| {
            // A dialog reopened during this one's close animation already owns
            // the slot; leave it alone.
            let is_current = slot.borrow().as_ref().is_some_and(|(d, _)| d == this);
            if is_current {
                *slot.borrow_mut() = None;
            }
        });
    }
    dialog.present(Some(root));
    entry.grab_focus();
}
