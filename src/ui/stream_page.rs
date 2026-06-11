//! Internet radio as a standalone relm4 component: the station list, the
//! add/search dialog, the station & recording detail dialogs, and the saved-
//! recordings list (including the live "currently recording" entry). Extracted
//! from the `App` god-object, mirroring [`crate::ui::podcasts_page`] and
//! [`crate::ui::yt_page`].
//!
//! **Boundary:** the *page* lives here; the **timeshift recorder** and all
//! playback stay on `App` (see `app_streaming.rs`) — playing/recording a station
//! mutates the single player/mini/mpris and a background ring-buffer worker, and
//! the replay/waveform subpages read that recorder. The page reaches the
//! transport via [`StreamOutput`] (`ToggleStream`/`PlayRecording`/`OpenReplay`/
//! `EditRecording`) and is told the playback + live-recording state back via
//! [`StreamInput::PlaybackStateChanged`]/[`StreamInput::SetLiveRecording`].

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use std::cell::RefCell;
use std::rc::Rc;

use crate::core::db::Library;
use crate::core::streaming::StationResult;
use crate::i18n::{gettext, gettext_f};
use crate::model::{RecordingItem, StreamItem};
use crate::ui::app::StreamView;
use crate::ui::app_helpers::{cover_widget, on_secondary_click};

/// Placeholder icon when a station has no logo.
const STREAM_ICON: &str = "audio-x-generic-symbolic";

/// Formats Unix seconds as "DD.MM.YYYY HH:MM" in local time.
fn format_datetime(secs: i64) -> String {
    gtk::glib::DateTime::from_unix_local(secs)
        .and_then(|d| d.format("%d.%m.%Y %H:%M"))
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// Content box for the dialogs (uniform margins).
fn detail_box() -> gtk::Box {
    gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(6)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build()
}

/// Activatable action row with an icon prefix (for the detail dialogs).
fn action_row(title: &str, icon: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(title)
        .activatable(true)
        .build();
    row.add_prefix(&gtk::Image::from_icon_name(icon));
    row
}

/// Embeds the content scrollably in a dialog with a header bar and shows it.
fn present_dialog(dialog: &adw::Dialog, content: &gtk::Box, root: &adw::ApplicationWindow) {
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .vexpand(true)
        .child(content)
        .build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&scroller));
    dialog.set_child(Some(&toolbar));
    dialog.set_content_width(600);
    dialog.present(Some(root));
}

/// Subtitle of a station: genre/country, as far as available.
fn stream_subtitle(st: &StreamItem) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(t) = st.tags.as_deref().filter(|s| !s.trim().is_empty()) {
        let tags: Vec<&str> = t
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .take(3)
            .collect();
        if !tags.is_empty() {
            parts.push(tags.join(" · "));
        }
    }
    if let Some(c) = st.country.as_deref().filter(|s| !s.trim().is_empty()) {
        parts.push(c.to_string());
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" — "))
    }
}

/// The internet-radio page component.
pub(crate) struct StreamPage {
    library: Library,
    window: Option<adw::ApplicationWindow>,
    mobile: bool,
    /// Mirror of the transport's `playing_stream` (for the station row icons).
    playing_stream: Option<i64>,
    /// Mirror of the transport's current local-file path (for recording rows).
    playing_path: Option<String>,
    /// Mirror of the transport play/pause state.
    playing: bool,
    /// Mirror of the running recording (`stream_id`, current ICY title) for the
    /// live entry at the top of the recordings list. `None` when not recording.
    live_recording: Option<(i64, Option<String>)>,
    /// Mirror of the timeshift buffer size (for the "Replay (buffer)" action).
    buffer_minutes: u32,
    stream_view: StreamView,
    stream_items: Vec<StreamItem>,
    streams_list: gtk::ListBox,
    stream_search_results: Vec<StationResult>,
    stream_search_failed: bool,
    stream_search: Rc<RefCell<Option<(adw::Dialog, gtk::ListBox)>>>,
    recording_items: Vec<RecordingItem>,
    recordings_list: gtk::ListBox,
    stream_play_buttons: Rc<RefCell<Vec<(i64, gtk::Button)>>>,
    rec_play_buttons: Rc<RefCell<Vec<(String, gtk::Button)>>>,
}

#[derive(Debug)]
pub(crate) enum StreamInput {
    // --- driven by the parent ---
    Reload,
    ReloadRecordings,
    PlaybackStateChanged {
        playing_stream: Option<i64>,
        playing_path: Option<String>,
        playing: bool,
    },
    SetLiveRecording(Option<(i64, Option<String>)>),
    SetBufferMinutes(u32),
    SetMobile(bool),
    SetWindow(adw::ApplicationWindow),
    // --- view-internal ---
    SetView(StreamView),
    Add,
    Search(String),
    AddResult(usize),
    AddUrl(String),
    OpenStream(i64),
    RenameDialog(i64),
    Rename {
        id: i64,
        name: String,
    },
    Delete(i64),
    OpenRecording(i64),
    RecordingDelete(i64),
    RecordingDeleteConfirmed(i64),
    AddRecordingToLibrary(i64),
}

#[derive(Debug)]
pub(crate) enum StreamOutput {
    /// Transport: play/pause a station (start it if not running).
    ToggleStream(i64),
    /// Transport: play a saved recording file.
    PlayRecording(String),
    /// Transport: open the timeshift replay subpage of a station (reads the recorder).
    OpenReplay(i64),
    /// Open the waveform editor (a parent subpage) for a recording.
    EditRecording(i64),
    /// Show the "station removed" undo toast; the deferred deletion runs in the
    /// parent transport (it must stop the player/recorder if it is running).
    StreamDeleteUndo(i64),
    /// Show the "recording deleted" undo toast; deferred deletion comes back as
    /// `RecordingDeleteConfirmed`.
    RecordingDeleteUndo(i64),
    /// A recording was copied into the music library → reload artist/album views.
    LibraryChanged,
    /// Share a selection (a station) over device sync.
    Share(crate::core::sync::share::Selection),
    /// Informational toast.
    Toast(String),
}

#[derive(Debug)]
pub(crate) enum StreamCmd {
    SearchResults(Vec<StationResult>),
    SearchFailed,
    SearchCoversReady,
    /// Station logos finished caching → redraw the station list.
    ReloadStreams,
}

#[relm4::component(pub(crate))]
impl Component for StreamPage {
    type Init = ();
    type Input = StreamInput;
    type Output = StreamOutput;
    type CommandOutput = StreamCmd;

    view! {
        #[root]
        gtk::Box {
            set_orientation: gtk::Orientation::Vertical,

            // Tab switcher: stations / recordings + "+" for a new station.
            gtk::Box {
                set_spacing: 6,
                set_margin_top: 2,
                set_margin_bottom: 4,
                set_margin_start: 12,
                set_margin_end: 12,
                add_css_class: "linked",
                gtk::ToggleButton {
                    set_label: &gettext("Stations"),
                    set_hexpand: true,
                    #[watch]
                    set_active: model.stream_view == StreamView::Channels,
                    connect_clicked => StreamInput::SetView(StreamView::Channels),
                },
                gtk::ToggleButton {
                    set_label: &gettext("Recordings"),
                    set_hexpand: true,
                    #[watch]
                    set_active: model.stream_view == StreamView::Recordings,
                    connect_clicked => StreamInput::SetView(StreamView::Recordings),
                },
                gtk::Button {
                    set_icon_name: "list-add-symbolic",
                    set_tooltip_text: Some(&gettext("Add station")),
                    add_css_class: "flat",
                    connect_clicked => StreamInput::Add,
                },
            },

            // Stations.
            gtk::ScrolledWindow {
                set_vexpand: true,
                #[watch]
                set_visible: model.stream_view == StreamView::Channels && !model.stream_items.is_empty(),
                #[local_ref]
                streams_list -> gtk::ListBox {
                    set_valign: gtk::Align::Start,
                    set_margin_top: 10,
                    set_margin_bottom: 12,
                    set_margin_start: 12,
                    set_margin_end: 12,
                    set_css_classes: &["boxed-list"],
                },
            },
            adw::StatusPage {
                set_icon_name: Some("internet-radio-symbolic"),
                set_title: &gettext("No stations"),
                set_description: Some(&gettext("Add a stream address or search for a station worldwide.")),
                set_vexpand: true,
                #[watch]
                set_visible: model.stream_view == StreamView::Channels && model.stream_items.is_empty(),
            },

            // Recordings.
            gtk::ScrolledWindow {
                set_vexpand: true,
                #[watch]
                set_visible: model.stream_view == StreamView::Recordings && (!model.recording_items.is_empty() || model.live_recording.is_some()),
                #[local_ref]
                recordings_list -> gtk::ListBox {
                    set_valign: gtk::Align::Start,
                    set_margin_top: 10,
                    set_margin_bottom: 12,
                    set_margin_start: 12,
                    set_margin_end: 12,
                    set_css_classes: &["boxed-list"],
                },
            },
            adw::StatusPage {
                set_icon_name: Some("media-record-symbolic"),
                set_title: &gettext("No recordings"),
                set_description: Some(&gettext("Record the current song while a station plays.")),
                set_vexpand: true,
                #[watch]
                set_visible: model.stream_view == StreamView::Recordings && model.recording_items.is_empty() && model.live_recording.is_none(),
            },
        }
    }

    fn init(
        _init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let library = Library::open_or_memory();
        let streams_list = gtk::ListBox::new();
        let recordings_list = gtk::ListBox::new();
        let model = StreamPage {
            library,
            window: None,
            mobile: false,
            playing_stream: None,
            playing_path: None,
            playing: false,
            live_recording: None,
            buffer_minutes: 0,
            stream_view: StreamView::Channels,
            stream_items: Vec::new(),
            streams_list: streams_list.clone(),
            stream_search_results: Vec::new(),
            stream_search_failed: false,
            stream_search: Rc::new(RefCell::new(None)),
            recording_items: Vec::new(),
            recordings_list: recordings_list.clone(),
            stream_play_buttons: Rc::new(RefCell::new(Vec::new())),
            rec_play_buttons: Rc::new(RefCell::new(Vec::new())),
        };
        // Cache the station logos once in the background, then redraw.
        sender.spawn_oneshot_command(|| {
            if let Ok(lib) = Library::open() {
                for st in lib.streams().unwrap_or_default() {
                    if let Some(url) = st.favicon {
                        crate::core::online::cache_station_image(&url);
                    }
                }
            }
            StreamCmd::ReloadStreams
        });
        let widgets = view_output!();
        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: StreamInput, sender: ComponentSender<Self>, _root: &Self::Root) {
        match msg {
            StreamInput::Reload => self.reload_streams(&sender),
            StreamInput::ReloadRecordings => self.reload_recordings(&sender),
            StreamInput::PlaybackStateChanged {
                playing_stream,
                playing_path,
                playing,
            } => {
                self.playing_stream = playing_stream;
                self.playing_path = playing_path;
                self.playing = playing;
                self.refresh_stream_icons();
                self.refresh_recording_icons();
            }
            StreamInput::SetLiveRecording(state) => {
                self.live_recording = state;
                self.reload_recordings(&sender);
            }
            StreamInput::SetBufferMinutes(n) => self.buffer_minutes = n,
            StreamInput::SetMobile(b) => self.mobile = b,
            StreamInput::SetWindow(w) => self.window = Some(w),
            StreamInput::SetView(v) => self.stream_view = v,
            StreamInput::Add => self.open_add_stream_dialog(&sender),
            StreamInput::Search(term) => {
                let term = term.trim().to_string();
                if !term.is_empty() {
                    let _ = sender.output(StreamOutput::Toast(gettext("Searching …")));
                    sender.spawn_command(move |out| {
                        let results = match crate::core::streaming::search_stations(&term) {
                            Ok(r) => r,
                            Err(_) => {
                                let _ = out.send(StreamCmd::SearchFailed);
                                return;
                            }
                        };
                        let _ = out.send(StreamCmd::SearchResults(results.clone()));
                        for r in &results {
                            if let Some(img) = r.favicon.as_deref() {
                                crate::core::online::cache_station_image(img);
                            }
                        }
                        let _ = out.send(StreamCmd::SearchCoversReady);
                    });
                }
            }
            StreamInput::AddResult(index) => self.add_stream_result(&sender, index),
            StreamInput::AddUrl(url) => self.stream_add_url(&sender, url),
            StreamInput::OpenStream(id) => self.open_stream(&sender, id),
            StreamInput::RenameDialog(id) => self.open_rename_stream_dialog(&sender, id),
            StreamInput::Rename { id, name } => {
                let name = name.trim();
                if !name.is_empty() {
                    let _ = self.library.rename_stream(id, name);
                    self.reload_streams(&sender);
                }
            }
            StreamInput::Delete(id) => {
                let _ = sender.output(StreamOutput::StreamDeleteUndo(id));
            }
            StreamInput::OpenRecording(id) => self.open_recording(&sender, id),
            StreamInput::RecordingDelete(id) => {
                let _ = sender.output(StreamOutput::RecordingDeleteUndo(id));
            }
            StreamInput::RecordingDeleteConfirmed(id) => {
                if let Ok(Some(path)) = self.library.delete_recording(id) {
                    let _ = std::fs::remove_file(&path);
                }
                self.reload_recordings(&sender);
            }
            StreamInput::AddRecordingToLibrary(id) => self.add_recording_to_library(&sender, id),
        }
    }

    fn update_cmd(&mut self, cmd: StreamCmd, sender: ComponentSender<Self>, _root: &Self::Root) {
        match cmd {
            StreamCmd::SearchResults(results) => {
                self.stream_search_failed = false;
                self.stream_search_results = results;
                self.rebuild_stream_search_results(&sender);
            }
            StreamCmd::SearchFailed => {
                self.stream_search_failed = true;
                self.stream_search_results.clear();
                self.rebuild_stream_search_results(&sender);
            }
            StreamCmd::SearchCoversReady => self.rebuild_stream_search_results(&sender),
            StreamCmd::ReloadStreams => self.reload_streams(&sender),
        }
    }
}

/// Safety prompt before a destructive page action; sends `then` to ourselves on
/// confirm (the actual deletion is still deferred via an undo toast afterwards).
fn confirm_delete(
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
        if resp == "ok" {
            if let Some(t) = then.borrow_mut().take() {
                sender.input(t);
            }
        }
    });
    confirm.present(Some(root));
}

impl StreamPage {
    /// Show detail dialogs as bottom sheets on the phone.
    fn adapt_detail_dialog(&self, dialog: &adw::Dialog) {
        if self.mobile {
            dialog.set_presentation_mode(adw::DialogPresentationMode::BottomSheet);
        }
    }

    /// Rebuilds the station list.
    fn reload_streams(&mut self, sender: &ComponentSender<Self>) {
        self.stream_items = self.library.streams().unwrap_or_default();
        self.stream_play_buttons.borrow_mut().clear();
        while let Some(child) = self.streams_list.first_child() {
            self.streams_list.remove(&child);
        }
        for st in self.stream_items.clone() {
            // Not activatable: like a library track, the station plays via its
            // play button; long press / right click opens the detail view.
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&st.name))
                .build();
            row.add_css_class("emilia-flush");
            if let Some(sub) = stream_subtitle(&st) {
                row.set_subtitle(&gtk::glib::markup_escape_text(&sub));
            }
            let logo = st
                .favicon
                .as_deref()
                .and_then(crate::core::online::station_image_path);
            row.add_prefix(&cover_widget(logo.as_deref(), STREAM_ICON));
            let id = st.id;

            let pp = gtk::Button::builder()
                .icon_name("media-playback-start-symbolic")
                .valign(gtk::Align::Center)
                .tooltip_text(gettext("Play/Pause"))
                .build();
            pp.add_css_class("flat");
            {
                let sender = sender.clone();
                pp.connect_clicked(move |_| {
                    let _ = sender.output(StreamOutput::ToggleStream(id));
                });
            }
            self.stream_play_buttons.borrow_mut().push((id, pp.clone()));
            row.add_suffix(&pp);

            on_secondary_click(&row, {
                let sender = sender.clone();
                move || sender.input(StreamInput::OpenStream(id))
            });
            let lp = gtk::GestureLongPress::new();
            {
                let sender = sender.clone();
                lp.connect_pressed(move |g, _, _| {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    sender.input(StreamInput::OpenStream(id));
                });
            }
            row.add_controller(lp);
            self.streams_list.append(&row);
        }
        self.refresh_stream_icons();
    }

    /// Refreshes the Play/Pause icons of the station rows.
    fn refresh_stream_icons(&self) {
        let playing = self.playing;
        let cur = self.playing_stream;
        let mut btns = self.stream_play_buttons.borrow_mut();
        btns.retain(|(_, b)| b.root().is_some());
        for (id, btn) in btns.iter() {
            let active = cur == Some(*id) && playing;
            btn.set_icon_name(if active {
                "media-playback-pause-symbolic"
            } else {
                "media-playback-start-symbolic"
            });
        }
    }

    /// Keeps the play/pause icon of each recording row in sync.
    fn refresh_recording_icons(&self) {
        let playing = self.playing;
        let cur = self.playing_path.as_deref();
        let mut btns = self.rec_play_buttons.borrow_mut();
        btns.retain(|(_, b)| b.root().is_some());
        for (path, btn) in btns.iter() {
            let active = cur == Some(path.as_str()) && playing;
            btn.set_icon_name(if active {
                "media-playback-pause-symbolic"
            } else {
                "media-playback-start-symbolic"
            });
        }
    }

    /// Station detail dialog: replay (buffer), rename, remove.
    fn open_stream(&self, sender: &ComponentSender<Self>, id: i64) {
        let Some(root) = self.window.clone() else {
            return;
        };
        let Some(st) = self.stream_items.iter().find(|s| s.id == id).cloned() else {
            return;
        };
        let dialog = adw::Dialog::builder()
            .title(gtk::glib::markup_escape_text(&st.name))
            .build();
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
        head.add_prefix(&cover_widget(logo.as_deref(), STREAM_ICON));
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
        let share = action_row(&gettext("Share"), "emilia-share-symbolic");
        {
            let (sender, dialog) = (sender.clone(), dialog.clone());
            share.connect_activated(move |_| {
                let _ = sender.output(StreamOutput::Share(crate::core::sync::share::Selection {
                    stations: vec![id],
                    ..Default::default()
                }));
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

        present_dialog(&dialog, &content, &root);
    }

    /// Dialog: rename a station (name prefilled).
    fn open_rename_stream_dialog(&self, sender: &ComponentSender<Self>, id: i64) {
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

    /// Dialog for adding a station (worldwide search + manual URL).
    fn open_add_stream_dialog(&self, sender: &ComponentSender<Self>) {
        let Some(root) = self.window.clone() else {
            return;
        };
        let dialog = adw::Dialog::builder().title(gettext("Add station")).build();
        self.adapt_detail_dialog(&dialog);
        let content = detail_box();

        let search_group = adw::PreferencesGroup::builder()
            .title(gettext("Search"))
            .description(gettext("Find a station worldwide by name"))
            .build();
        let search_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .build();
        let search_entry = gtk::SearchEntry::builder()
            .placeholder_text(gettext("Station name …"))
            .hexpand(true)
            .build();
        crate::ui::widgets::no_autofocus(&search_entry);
        let search_btn = gtk::Button::builder().label(gettext("Search")).build();
        search_btn.add_css_class("suggested-action");
        search_row.append(&search_entry);
        search_row.append(&search_btn);
        search_group.add(&search_row);
        content.append(&search_group);

        {
            let (sender, entry) = (sender.clone(), search_entry.clone());
            search_entry.connect_activate(move |_| {
                let term = entry.text().to_string();
                if !term.trim().is_empty() {
                    sender.input(StreamInput::Search(term));
                }
            });
        }
        {
            let (sender, entry) = (sender.clone(), search_entry.clone());
            search_btn.connect_clicked(move |_| {
                let term = entry.text().to_string();
                if !term.trim().is_empty() {
                    sender.input(StreamInput::Search(term));
                }
            });
        }

        let results = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .build();
        results.add_css_class("boxed-list");
        results.set_visible(false);
        content.append(&results);

        let url_group = adw::PreferencesGroup::builder()
            .title(gettext("Or enter a stream address"))
            .build();
        let url_entry = adw::EntryRow::builder()
            .title(gettext("Stream address (URL)"))
            .show_apply_button(true)
            .build();
        crate::ui::widgets::no_autofocus(&url_entry);
        {
            let (sender, dialog) = (sender.clone(), dialog.clone());
            url_entry.connect_apply(move |e| {
                let url = e.text().to_string();
                if !url.trim().is_empty() {
                    sender.input(StreamInput::AddUrl(url));
                    dialog.close();
                }
            });
        }
        url_group.add(&url_entry);
        content.append(&url_group);

        *self.stream_search.borrow_mut() = Some((dialog.clone(), results.clone()));
        {
            let slot = self.stream_search.clone();
            dialog.connect_closed(move |_| {
                *slot.borrow_mut() = None;
            });
        }
        present_dialog(&dialog, &content, &root);
    }

    /// Redraws the results list in the open add dialog.
    fn rebuild_stream_search_results(&self, sender: &ComponentSender<Self>) {
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
            dialog.set_content_height(300);
            return;
        }

        let rows = self.stream_search_results.len() as i32;
        dialog.set_content_height((320 + rows * 66).min(760));

        for (i, r) in self.stream_search_results.iter().enumerate() {
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&r.name))
                .activatable(true)
                .build();
            let mut sub: Vec<String> = Vec::new();
            if let Some(c) = r.country.as_deref().filter(|s| !s.trim().is_empty()) {
                sub.push(c.to_string());
            }
            if let Some(t) = r.tags.as_deref().filter(|s| !s.trim().is_empty()) {
                let tags: Vec<&str> = t
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .take(2)
                    .collect();
                if !tags.is_empty() {
                    sub.push(tags.join(" · "));
                }
            }
            if !sub.is_empty() {
                row.set_subtitle(&gtk::glib::markup_escape_text(&sub.join(" — ")));
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
    fn add_stream_result(&mut self, sender: &ComponentSender<Self>, index: usize) {
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
            Ok(_) => {
                self.reload_streams(sender);
                let _ = sender.output(StreamOutput::Toast(gettext_f(
                    "Added: {n}",
                    &[("n", &r.name)],
                )));
                if let Some(fav) = r.favicon.clone() {
                    sender.spawn_command(move |out| {
                        crate::core::online::cache_station_image(&fav);
                        let _ = out.send(StreamCmd::ReloadStreams);
                    });
                }
            }
            Err(_) => {
                let _ = sender.output(StreamOutput::Toast(gettext("Could not add station")));
            }
        }
    }

    /// Add a station directly from a URL.
    fn stream_add_url(&mut self, sender: &ComponentSender<Self>, url: String) {
        let url = url.trim().to_string();
        if !url.is_empty() {
            let name = crate::core::streaming::name_from_url(&url);
            match self
                .library
                .add_stream(&name, &url, None, None, None, None, None)
            {
                Ok(_) => {
                    self.reload_streams(sender);
                    let _ = sender.output(StreamOutput::Toast(gettext("Station added")));
                }
                Err(_) => {
                    let _ = sender.output(StreamOutput::Toast(gettext("Could not add station")));
                }
            }
        }
    }

    /// Rebuilds the "Recordings" list (live entry + saved recordings).
    fn reload_recordings(&mut self, sender: &ComponentSender<Self>) {
        self.recording_items = self.library.recordings().unwrap_or_default();
        for rec in &mut self.recording_items {
            if rec.duration_ms <= 0 {
                let ms = crate::core::scanner::duration_secs(std::path::Path::new(&rec.path))
                    as i64
                    * 1000;
                if ms > 0 {
                    let _ = self.library.set_recording_duration(rec.id, ms);
                    rec.duration_ms = ms;
                }
            }
        }
        self.rec_play_buttons.borrow_mut().clear();
        while let Some(child) = self.recordings_list.first_child() {
            self.recordings_list.remove(&child);
        }

        // Live entry for the song currently being recorded.
        if let Some((stream_id, current_title)) = self.live_recording.clone() {
            let station = self
                .stream_items
                .iter()
                .find(|s| s.id == stream_id)
                .map(|s| s.name.clone());
            let (artist, title) = match current_title.as_deref() {
                Some(t) => crate::core::online::recording_query_candidates(t, station.as_deref())
                    .into_iter()
                    .next()
                    .unwrap_or((None, t.trim().to_string())),
                None => (None, gettext("Current recording")),
            };
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&title))
                .build();
            row.add_css_class("emilia-flush");
            let mut sub: Vec<String> = Vec::new();
            if let Some(a) = artist.as_deref().filter(|s| !s.trim().is_empty()) {
                sub.push(a.to_string());
            }
            if let Some(s) = station.as_deref().filter(|s| !s.trim().is_empty()) {
                sub.push(s.to_string());
            }
            sub.push(gettext("Recording …"));
            row.set_subtitle(&gtk::glib::markup_escape_text(&sub.join(" · ")));
            let cover =
                crate::core::online::recording_cover_path(artist.as_deref().unwrap_or(""), &title);
            row.add_prefix(&cover_widget(cover.as_deref(), "media-record-symbolic"));
            let dot = gtk::Image::from_icon_name("media-record-symbolic");
            dot.set_valign(gtk::Align::Center);
            dot.set_css_classes(&["emilia-record-dot", "emilia-recording"]);
            row.add_suffix(&dot);
            self.recordings_list.append(&row);
        }

        for rec in self.recording_items.clone() {
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&rec.title))
                .activatable(true)
                .build();
            row.add_css_class("emilia-flush");
            let mut sub: Vec<String> = Vec::new();
            if let Some(a) = rec.artist.as_deref().filter(|s| !s.trim().is_empty()) {
                sub.push(a.to_string());
            }
            if let Some(s) = rec.station.as_deref().filter(|s| !s.trim().is_empty()) {
                sub.push(s.to_string());
            }
            sub.push(format_datetime(rec.recorded_at));
            if !sub.is_empty() {
                row.set_subtitle(&gtk::glib::markup_escape_text(&sub.join(" · ")));
            }
            let placeholder = if rec.incomplete {
                "media-playlist-consecutive-symbolic"
            } else {
                "audio-x-generic-symbolic"
            };
            let cover = crate::core::online::recording_cover_path(
                rec.artist.as_deref().unwrap_or(""),
                &rec.title,
            );
            row.add_prefix(&cover_widget(cover.as_deref(), placeholder));
            if rec.incomplete {
                row.set_tooltip_text(Some(&gettext("Incomplete (beginning was missing)")));
            }
            if rec.duration_ms > 0 {
                let dur = gtk::Label::new(Some(&crate::ui::app::fmt_duration(rec.duration_ms)));
                dur.set_valign(gtk::Align::Center);
                dur.set_css_classes(&["dim-label", "numeric"]);
                row.add_suffix(&dur);
            }
            let is_active = self.playing_path.as_deref() == Some(rec.path.as_str());
            let play_btn = gtk::Button::from_icon_name(if is_active && self.playing {
                "media-playback-pause-symbolic"
            } else {
                "media-playback-start-symbolic"
            });
            play_btn.set_valign(gtk::Align::Center);
            play_btn.set_tooltip_text(Some(&gettext("Play")));
            play_btn.add_css_class("flat");
            {
                let sender = sender.clone();
                let path = rec.path.clone();
                play_btn.connect_clicked(move |_| {
                    let _ = sender.output(StreamOutput::PlayRecording(path.clone()));
                });
            }
            row.add_suffix(&play_btn);
            self.rec_play_buttons
                .borrow_mut()
                .push((rec.path.clone(), play_btn));
            {
                let sender = sender.clone();
                let path = rec.path.clone();
                row.connect_activated(move |_| {
                    let _ = sender.output(StreamOutput::PlayRecording(path.clone()));
                });
            }
            on_secondary_click(&row, {
                let sender = sender.clone();
                let id = rec.id;
                move || sender.input(StreamInput::OpenRecording(id))
            });
            let lp = gtk::GestureLongPress::new();
            {
                let sender = sender.clone();
                let id = rec.id;
                lp.connect_pressed(move |g, _, _| {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    sender.input(StreamInput::OpenRecording(id));
                });
            }
            row.add_controller(lp);
            self.recordings_list.append(&row);
        }
    }

    /// Detail dialog of a saved recording.
    fn open_recording(&self, sender: &ComponentSender<Self>, id: i64) {
        let Some(root) = self.window.clone() else {
            return;
        };
        let Some(rec) = self.recording_items.iter().find(|r| r.id == id).cloned() else {
            return;
        };
        let tag = crate::core::scanner::read_track(std::path::Path::new(&rec.path)).ok();
        let album = tag
            .as_ref()
            .and_then(|t| t.album.clone())
            .filter(|a| !a.trim().is_empty());
        let artist = rec
            .artist
            .clone()
            .filter(|a| !a.trim().is_empty())
            .or_else(|| tag.as_ref().and_then(|t| t.artist.clone()))
            .filter(|a| !a.trim().is_empty());

        let dialog = adw::Dialog::builder()
            .title(gtk::glib::markup_escape_text(&rec.title))
            .build();
        self.adapt_detail_dialog(&dialog);
        let content = detail_box();

        let info = adw::PreferencesGroup::new();
        let head = adw::ActionRow::builder()
            .title(gtk::glib::markup_escape_text(&rec.title))
            .build();
        if let Some(a) = artist.as_deref() {
            head.set_subtitle(&gtk::glib::markup_escape_text(a));
        }
        let cover =
            crate::core::online::recording_cover_path(artist.as_deref().unwrap_or(""), &rec.title);
        head.add_prefix(&cover_widget(cover.as_deref(), "audio-x-generic-symbolic"));
        info.add(&head);
        content.append(&info);

        let details = adw::PreferencesGroup::new();
        let info_row = |label: &str, value: &str| {
            let r = adw::ActionRow::builder().title(label).build();
            r.set_subtitle(&gtk::glib::markup_escape_text(value));
            r.add_css_class("property");
            r
        };
        if let Some(ar) = artist.as_deref() {
            details.add(&info_row(&gettext("Artist"), ar));
        }
        if let Some(al) = album.as_deref() {
            details.add(&info_row(&gettext("Album"), al));
        }
        if let Some(st) = rec.station.as_deref().filter(|s| !s.trim().is_empty()) {
            details.add(&info_row(&gettext("Station"), st));
        }
        details.add(&info_row(
            &gettext("Recorded"),
            &format_datetime(rec.recorded_at),
        ));
        if rec.incomplete {
            details.add(&info_row(
                &gettext("Note"),
                &gettext("Incomplete (beginning was missing)"),
            ));
        }
        content.append(&details);

        let actions = adw::PreferencesGroup::new();
        let play = action_row(&gettext("Play"), "media-playback-start-symbolic");
        {
            let (sender, dialog, path) = (sender.clone(), dialog.clone(), rec.path.clone());
            play.connect_activated(move |_| {
                let _ = sender.output(StreamOutput::PlayRecording(path.clone()));
                dialog.close();
            });
        }
        actions.add(&play);
        let add_lib = action_row(&gettext("Add to library"), "list-add-symbolic");
        {
            let (sender, dialog) = (sender.clone(), dialog.clone());
            add_lib.connect_activated(move |_| {
                sender.input(StreamInput::AddRecordingToLibrary(id));
                dialog.close();
            });
        }
        actions.add(&add_lib);
        let edit = action_row(&gettext("Edit"), "document-edit-symbolic");
        {
            let (sender, dialog) = (sender.clone(), dialog.clone());
            edit.connect_activated(move |_| {
                let _ = sender.output(StreamOutput::EditRecording(id));
                dialog.close();
            });
        }
        actions.add(&edit);
        let share = action_row(&gettext("Share"), "emilia-share-symbolic");
        {
            let (sender, dialog) = (sender.clone(), dialog.clone());
            share.connect_activated(move |_| {
                let _ = sender.output(StreamOutput::Share(crate::core::sync::share::Selection {
                    recordings: vec![id],
                    ..Default::default()
                }));
                dialog.close();
            });
        }
        actions.add(&share);
        let remove = action_row(&gettext("Delete recording"), "user-trash-symbolic");
        {
            let (sender, dialog, root) = (sender.clone(), dialog.clone(), root.clone());
            remove.connect_activated(move |_| {
                dialog.close();
                confirm_delete(
                    &root,
                    &sender,
                    &gettext("Delete this recording?"),
                    &gettext("Delete"),
                    StreamInput::RecordingDelete(id),
                );
            });
        }
        actions.add(&remove);
        content.append(&actions);

        present_dialog(&dialog, &content, &root);
    }

    /// Copies a recording into the primary music library, then registers it.
    fn add_recording_to_library(&mut self, sender: &ComponentSender<Self>, id: i64) {
        let Some(rec) = self.recording_items.iter().find(|r| r.id == id).cloned() else {
            return;
        };
        let Some(music_dir) = self
            .library
            .get_setting("music_dir")
            .ok()
            .flatten()
            .filter(|s| !s.trim().is_empty())
        else {
            let _ = sender.output(StreamOutput::Toast(gettext("Set a music folder first")));
            return;
        };
        let src = std::path::PathBuf::from(&rec.path);
        if !src.exists() {
            let _ = sender.output(StreamOutput::Toast(gettext("File not found")));
            return;
        }

        let mut track = crate::core::scanner::read_track(&src).unwrap_or(crate::model::Track {
            id: 0,
            path: rec.path.clone(),
            title: rec.title.clone(),
            artist: rec.artist.clone(),
            album: None,
            genre: None,
            track_no: None,
            disc_no: None,
            duration_ms: None,
            resume_ms: 0,
            year: None,
        });
        let artist = track
            .artist
            .clone()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| rec.artist.clone())
            .filter(|s| !s.trim().is_empty());
        let title = if track.title.trim().is_empty() {
            rec.title.clone()
        } else {
            track.title.clone()
        };

        use crate::core::youtube::sanitize_filename;
        let ext = src.extension().and_then(|e| e.to_str()).unwrap_or("mp3");
        let mut dest = std::path::PathBuf::from(&music_dir);
        match artist.as_deref().filter(|s| !s.trim().is_empty()) {
            Some(a) => dest.push(sanitize_filename(a)),
            None => dest.push("Recordings"),
        }
        if let Some(al) = track.album.as_deref().filter(|s| !s.trim().is_empty()) {
            dest.push(sanitize_filename(al));
        }
        dest.push(format!("{}.{ext}", sanitize_filename(&title)));

        if dest.exists() {
            let _ = sender.output(StreamOutput::Toast(gettext("Already in the library")));
            return;
        }
        if dest
            .parent()
            .is_some_and(|p| std::fs::create_dir_all(p).is_err())
            || std::fs::copy(&src, &dest).is_err()
        {
            let _ = sender.output(StreamOutput::Toast(gettext("Could not add to the library")));
            return;
        }

        let dest_str = dest.to_string_lossy().into_owned();
        if let Some(cover) =
            crate::core::online::recording_cover_path(artist.as_deref().unwrap_or(""), &title)
        {
            if let Ok(bytes) = std::fs::read(&cover) {
                crate::core::online::store_track_cover_bytes(&dest_str, &bytes);
            }
        }

        track.id = 0;
        track.path = dest_str;
        track.title = title;
        track.artist = artist;
        track.resume_ms = 0;
        if self.library.upsert_track(&track).is_ok() {
            let _ = sender.output(StreamOutput::LibraryChanged);
            let _ = sender.output(StreamOutput::Toast(gettext("Added to the library")));
        } else {
            let _ = std::fs::remove_file(&dest);
            let _ = sender.output(StreamOutput::Toast(gettext("Could not add to the library")));
        }
    }
}
