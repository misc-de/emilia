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
//!
//! The inherent impl is split by topic: the station list, its dialogs and the
//! add/search flow live in [`crate::ui::stream_page_stations`], the
//! recordings and "Recently heard" lists with their dialogs in
//! [`crate::ui::stream_page_recordings`], and the GTK-free display/sort logic
//! in [`crate::ui::stream_page_logic`].

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use std::cell::RefCell;
use std::rc::Rc;

use crate::core::db::Library;
use crate::core::streaming::StationResult;
use crate::i18n::gettext;
use crate::model::{HeardItem, RecordingItem, StreamItem};
use crate::ui::app::{SortCrit, StreamView};
use crate::ui::app_sort::read_sort;
use crate::ui::stream_page_logic::{
    first_nonblank, normalize_logo_url, parse_gallery_columns, view_setting_key,
};
use crate::ui::stream_page_recordings::lookup_song;
use crate::ui::stream_page_stations::cache_missing_station_logos;

/// Placeholder icon when a station has no logo.
pub(super) const STREAM_ICON: &str = "audio-x-generic-symbolic";

/// The internet-radio page component.
pub(crate) struct StreamPage {
    pub(super) library: Library,
    pub(super) window: Option<adw::ApplicationWindow>,
    pub(super) mobile: bool,
    /// Mirror of the transport's `playing_stream` (for the station row icons).
    pub(super) playing_stream: Option<i64>,
    /// Mirror of the transport's current local-file path (for recording rows).
    pub(super) playing_path: Option<String>,
    /// Mirror of the transport play/pause state.
    pub(super) playing: bool,
    /// Mirror of the running recording (`stream_id`, current ICY title) for the
    /// live entry at the top of the recordings list. `None` when not recording.
    pub(super) live_recording: Option<(i64, Option<String>)>,
    /// Mirror of the timeshift buffer size (for the "Replay (buffer)" action).
    pub(super) buffer_minutes: u32,
    pub(super) stream_view: StreamView,
    pub(super) stream_items: Vec<StreamItem>,
    pub(super) streams_list: gtk::ListBox,
    pub(super) stream_search_results: Vec<StationResult>,
    pub(super) stream_search_failed: bool,
    pub(super) stream_search: Rc<RefCell<Option<(adw::Dialog, gtk::ListBox)>>>,
    pub(super) recording_items: Vec<RecordingItem>,
    pub(super) recordings_list: gtk::ListBox,
    /// "Recently heard": songs recognized from a station's ICY title while
    /// streaming (no audio captured — pure history).
    pub(super) heard_items: Vec<HeardItem>,
    pub(super) heard_list: gtk::ListBox,
    /// Play/pause controls of the station rows, keyed by station id.
    pub(super) stream_marks: crate::ui::play_mark::Marks,
    /// Play/pause controls of the recording rows, keyed by file path.
    pub(super) rec_marks: crate::ui::play_mark::Marks,
    /// Per-sub-view sort (criterion + descending): stations by name; recordings by
    /// name / recording date / length. Persisted as "sort_stations[_desc]" /
    /// "sort_recordings[_desc]".
    pub(super) stations_sort: (SortCrit, bool),
    pub(super) recordings_sort: (SortCrit, bool),
    pub(super) heard_sort: (SortCrit, bool),
    /// "Without grouping" per sub-view (no alphabetical headings). Persisted as
    /// "nogroup_stations" / "nogroup_recordings" / "nogroup_heard".
    pub(super) stations_no_group: bool,
    pub(super) recordings_no_group: bool,
    pub(super) heard_no_group: bool,
    /// Stations gallery on/off (cover grid of station logos). Persisted as
    /// "gallery_stations". Recordings carry no covers, so they have no gallery.
    pub(super) stations_gallery: bool,
    /// "Show description" (sort popover): station gallery tiles framed with
    /// their name instead of the bare logo. Persisted as
    /// "gallery_desc_stations".
    pub(super) stations_gallery_desc: bool,
    /// Tiles per row in the stations gallery (mirrors the global setting).
    pub(super) gallery_columns: u32,
    /// Per-row alphabetical headings of the stations / recordings / heard lists.
    pub(super) station_headers: Rc<RefCell<Option<Vec<String>>>>,
    pub(super) recording_headers: Rc<RefCell<Option<Vec<String>>>>,
    pub(super) heard_headers: Rc<RefCell<Option<Vec<String>>>>,
    /// Gallery variant of the stations (logo grid). Its container box lives only
    /// in the view tree (a `#[local_ref]`); the flow box is filled imperatively.
    pub(super) streams_gallery: gtk::FlowBox,
    /// Hand-off for the shared title-bar sort button: [`Self::rebuild_sort`]
    /// writes the popover + direction here (or `None` to hide it) for the active
    /// sub-view, then signals the parent via [`StreamOutput::SortChanged`].
    pub(super) sort_slot: crate::ui::app_sort::SortSlot,
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
    /// Change the current sub-view's sort (criterion + descending), from the header.
    SetSort(SortCrit, bool),
    /// Toggle alphabetical grouping of the current sub-view's list (`true` = off).
    SetNoGroup(bool),
    /// Toggle the stations gallery (Channels sub-view only).
    SetGallery(bool),
    /// Toggle the stations gallery tiles' name ("Show description").
    SetGalleryDesc(bool),
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
    /// Open the "Change logo" dialog of a station.
    LogoDialog(i64),
    /// Set a station's logo to an image URL (`None`/empty = remove the logo).
    SetLogoUrl(i64, Option<String>),
    /// Use a local image file as a station's logo.
    SetLogoFile(i64, std::path::PathBuf),
    /// Search the web for a station's logo (`true` = report the outcome).
    FindLogo(i64, bool),
    /// Detail view's refresh: fetch the station's directory details and search
    /// its logo again, then reopen the detail view.
    RefreshStream(i64),
    /// Detail view's refresh: look up artist, album and cover of a recording
    /// again (written into the file), then reopen the detail view.
    RefreshRecording(i64),
    /// Detail view's refresh: look up artist and cover of a recognized song
    /// again, then reopen the detail view.
    RefreshHeard(i64),
    OpenRecording(i64),
    RecordingDelete(i64),
    RecordingDeleteConfirmed(i64),
    AddRecordingToLibrary(i64),
    /// Rebuild the "Recently heard" list (a new song was recognized or a cover
    /// landed).
    ReloadHeard,
    /// Open the detail dialog of a recognized song.
    OpenHeard(i64),
    /// Remove one entry from the "Recently heard" history.
    HeardDelete(i64),
}

#[derive(Debug)]
pub(crate) enum StreamOutput {
    /// Transport: play/pause a station (start it if not running).
    ToggleStream(i64),
    /// Transport: play a saved recording file.
    PlayRecording(String),
    /// Transport: open the timeshift replay subpage of a station (reads the recorder).
    OpenReplay(i64),
    /// Open the equalizer editor (a parent dialog) for a station (per-station EQ).
    OpenEqualizer(i64),
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
    /// Play a recognized song: the transport prefers a saved recording, then a
    /// library track, otherwise streams it via YouTube.
    PlayHeard {
        artist: Option<String>,
        title: String,
    },
    /// Download a recognized song via YouTube into the music library.
    DownloadHeard {
        artist: Option<String>,
        title: String,
    },
    /// Share a selection (a station) over device sync. Boxed: `Selection` is far
    /// larger than the other variants (`clippy::large_enum_variant`).
    Share(Box<crate::core::sync::share::Selection>),
    /// Informational toast.
    Toast(String),
    /// The sort slot was rebuilt → the parent refreshes the shared title-bar
    /// sort button (if the Streaming section is showing).
    SortChanged,
}

#[derive(Debug)]
pub(crate) enum StreamCmd {
    SearchResults(Vec<StationResult>),
    SearchFailed,
    SearchCoversReady,
    /// Station logos finished caching → redraw the station list.
    ReloadStreams,
    /// Startup logo cache finished; `true` if it brought in a logo that was
    /// missing → redraw the stations (they were built from the cache).
    LogosCached(bool),
    /// Background logo search for a station finished (`favicon` = found image
    /// URL, already cached); `report` = tell the user the outcome.
    LogoFound {
        id: i64,
        favicon: Option<String>,
        report: bool,
    },
    /// A station refresh finished: its directory entry (if listed) and the
    /// logo found (if any, already cached).
    StreamRefreshed {
        id: i64,
        meta: Option<StationResult>,
        favicon: Option<String>,
    },
    /// A recording refresh finished (`found` = new metadata came in).
    RecordingRefreshed {
        id: i64,
        found: bool,
    },
    /// A recognized-song refresh finished; `artist` = artist found online.
    HeardRefreshed {
        id: i64,
        artist: Option<String>,
        found: bool,
    },
}

#[relm4::component(pub(crate))]
impl Component for StreamPage {
    type Init = crate::ui::app_sort::SortSlot;
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
                add_css_class: "emilia-tabbar",
                gtk::ToggleButton {
                    set_label: &gettext("Stations"),
                    set_hexpand: true,
                    #[watch]
                    set_active: model.stream_view == StreamView::Channels,
                    connect_clicked => StreamInput::SetView(StreamView::Channels),
                },
                gtk::ToggleButton {
                    set_label: &gettext("Recently"),
                    set_hexpand: true,
                    #[watch]
                    set_active: model.stream_view == StreamView::Heard,
                    connect_clicked => StreamInput::SetView(StreamView::Heard),
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

            // Stations (list).
            gtk::ScrolledWindow {
                set_vexpand: true,
                #[watch]
                set_visible: model.stream_view == StreamView::Channels && !model.stream_items.is_empty() && !model.stations_gallery,
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
            // Stations (logo gallery).
            gtk::ScrolledWindow {
                set_vexpand: true,
                #[watch]
                set_visible: model.stream_view == StreamView::Channels && !model.stream_items.is_empty() && model.stations_gallery,
                #[local_ref]
                streams_gallery_box -> gtk::Box {
                    set_orientation: gtk::Orientation::Vertical,
                    set_spacing: 6,
                    set_valign: gtk::Align::Start,
                    set_margin_top: 10,
                    set_margin_bottom: 12,
                    set_margin_start: 12,
                    set_margin_end: 12,
                    append: &model.streams_gallery,
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

            // Recently heard (recognized songs).
            gtk::ScrolledWindow {
                set_vexpand: true,
                #[watch]
                set_visible: model.stream_view == StreamView::Heard && !model.heard_items.is_empty(),
                #[local_ref]
                heard_list -> gtk::ListBox {
                    set_valign: gtk::Align::Start,
                    set_margin_top: 10,
                    set_margin_bottom: 12,
                    set_margin_start: 12,
                    set_margin_end: 12,
                    set_css_classes: &["boxed-list"],
                },
            },
            adw::StatusPage {
                set_icon_name: Some("audio-x-generic-symbolic"),
                set_title: &gettext("Nothing heard yet"),
                set_description: Some(&gettext("Songs are recognized from the title a station broadcasts while it plays, and collected here — so you can play them back later (from your library, otherwise via YouTube). This only works for stations that transmit the current title.")),
                set_vexpand: true,
                #[watch]
                set_visible: model.stream_view == StreamView::Heard && model.heard_items.is_empty(),
            },
        }
    }

    fn init(
        sort_slot: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let library = Library::open_or_memory();
        let streams_list = gtk::ListBox::new();
        let recordings_list = gtk::ListBox::new();
        let heard_list = gtk::ListBox::new();
        let streams_gallery = gtk::FlowBox::new();
        let streams_gallery_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
        // Restore the per-sub-view sorts. Stations default to name-ascending;
        // recordings to newest-first (recording date, descending).
        let stations_sort = read_sort(&library, "stations", SortCrit::Name, false);
        let recordings_sort = read_sort(&library, "recordings", SortCrit::Release, true);
        // The "Recently" list defaults to newest-first (last heard, descending).
        let heard_sort = read_sort(&library, "heard", SortCrit::Release, true);
        let stations_no_group = matches!(
            library
                .get_setting("nogroup_stations")
                .ok()
                .flatten()
                .as_deref(),
            Some("1")
        );
        let recordings_no_group = matches!(
            library
                .get_setting("nogroup_recordings")
                .ok()
                .flatten()
                .as_deref(),
            Some("1")
        );
        let heard_no_group = matches!(
            library
                .get_setting("nogroup_heard")
                .ok()
                .flatten()
                .as_deref(),
            Some("1")
        );
        let stations_gallery_on = matches!(
            library
                .get_setting("gallery_stations")
                .ok()
                .flatten()
                .as_deref(),
            Some("1")
        );
        let stations_gallery_desc = library
            .get_setting("gallery_desc_stations")
            .ok()
            .flatten()
            .as_deref()
            != Some("0");
        let gallery_columns = parse_gallery_columns(
            library
                .get_setting("gallery_columns")
                .ok()
                .flatten()
                .as_deref(),
        );
        let station_headers = Rc::new(RefCell::new(None));
        let recording_headers = Rc::new(RefCell::new(None));
        let heard_headers = Rc::new(RefCell::new(None));
        streams_list.set_header_func(crate::ui::app_gallery::list_section_header_func(
            station_headers.clone(),
        ));
        recordings_list.set_header_func(crate::ui::app_gallery::list_section_header_func(
            recording_headers.clone(),
        ));
        heard_list.set_header_func(crate::ui::app_gallery::list_section_header_func(
            heard_headers.clone(),
        ));
        let mut model = StreamPage {
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
            heard_items: Vec::new(),
            heard_list: heard_list.clone(),
            stream_marks: Default::default(),
            rec_marks: Default::default(),
            stations_sort,
            recordings_sort,
            heard_sort,
            stations_no_group,
            recordings_no_group,
            heard_no_group,
            stations_gallery: stations_gallery_on,
            stations_gallery_desc,
            gallery_columns,
            station_headers,
            recording_headers,
            heard_headers,
            streams_gallery: streams_gallery.clone(),
            sort_slot,
        };
        // Fetch the station logos still missing from the cache in the
        // background; the stations are rebuilt only if one came in.
        sender.spawn_oneshot_command(|| StreamCmd::LogosCached(cache_missing_station_logos()));
        let widgets = view_output!();
        // Show the stations right away from the disk-cached logos (which also
        // builds the header sort popover for the restored sort + sub-view).
        // Waiting for the fetch above instead left the page empty for as long
        // as a dead logo host took to time out.
        model.reload_streams(&sender);
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
            StreamInput::SetView(v) => {
                self.stream_view = v;
                // The criteria differ per sub-view → rebuild the popover.
                self.rebuild_sort(&sender);
            }
            StreamInput::SetSort(crit, desc) => {
                // Apply to the sort of the currently visible sub-view.
                let key = view_setting_key(self.stream_view);
                let slot = match self.stream_view {
                    StreamView::Channels => &mut self.stations_sort,
                    StreamView::Recordings => &mut self.recordings_sort,
                    StreamView::Heard => &mut self.heard_sort,
                };
                if *slot != (crit, desc) {
                    *slot = (crit, desc);
                    let _ = self
                        .library
                        .set_setting(&format!("sort_{key}"), crit.as_key());
                    let _ = self
                        .library
                        .set_setting(&format!("sort_{key}_desc"), if desc { "1" } else { "0" });
                    match self.stream_view {
                        StreamView::Channels => self.reload_streams(&sender),
                        StreamView::Recordings => self.reload_recordings(&sender),
                        StreamView::Heard => self.reload_heard(&sender),
                    }
                }
            }
            StreamInput::SetNoGroup(off) => {
                let key = view_setting_key(self.stream_view);
                let slot = match self.stream_view {
                    StreamView::Channels => &mut self.stations_no_group,
                    StreamView::Recordings => &mut self.recordings_no_group,
                    StreamView::Heard => &mut self.heard_no_group,
                };
                if *slot != off {
                    *slot = off;
                    let _ = self
                        .library
                        .set_setting(&format!("nogroup_{key}"), if off { "1" } else { "0" });
                    match self.stream_view {
                        StreamView::Channels => self.reload_streams(&sender),
                        StreamView::Recordings => self.reload_recordings(&sender),
                        StreamView::Heard => self.reload_heard(&sender),
                    }
                }
            }
            StreamInput::SetGallery(on) => {
                if self.stations_gallery != on {
                    self.stations_gallery = on;
                    let _ = self
                        .library
                        .set_setting("gallery_stations", if on { "1" } else { "0" });
                    self.reload_streams(&sender);
                }
            }
            StreamInput::SetGalleryDesc(on) => {
                if self.stations_gallery_desc != on {
                    self.stations_gallery_desc = on;
                    let _ = self
                        .library
                        .set_setting("gallery_desc_stations", if on { "1" } else { "0" });
                    self.reload_streams(&sender);
                }
            }
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
            StreamInput::LogoDialog(id) => self.open_logo_dialog(&sender, id),
            StreamInput::SetLogoUrl(id, url) => {
                let url = normalize_logo_url(url);
                let _ = self.library.set_stream_favicon(id, url.as_deref());
                self.reload_streams(&sender);
                if let Some(url) = url {
                    sender.spawn_command(move |out| {
                        let ok = crate::core::online::cache_station_image(&url).is_some();
                        let _ = out.send(if ok {
                            StreamCmd::ReloadStreams
                        } else {
                            StreamCmd::LogoFound {
                                id,
                                favicon: None,
                                report: true,
                            }
                        });
                    });
                }
            }
            StreamInput::SetLogoFile(id, path) => {
                match crate::core::online::import_station_logo(&path) {
                    Some(fav) => {
                        let _ = self.library.set_stream_favicon(id, Some(&fav));
                        self.reload_streams(&sender);
                    }
                    None => {
                        let _ =
                            sender.output(StreamOutput::Toast(gettext("Could not load the image")));
                    }
                }
            }
            StreamInput::FindLogo(id, report) => {
                let Some(url) = self
                    .stream_items
                    .iter()
                    .find(|s| s.id == id)
                    .map(|s| s.url.clone())
                else {
                    return;
                };
                if report {
                    let _ = sender.output(StreamOutput::Toast(gettext("Searching for a logo…")));
                }
                sender.spawn_command(move |out| {
                    let favicon = crate::core::station_logo::find_logo(&url);
                    let _ = out.send(StreamCmd::LogoFound {
                        id,
                        favicon,
                        report,
                    });
                });
            }
            StreamInput::RefreshStream(id) => {
                let Some(st) = self.stream_items.iter().find(|s| s.id == id).cloned() else {
                    return;
                };
                let _ = sender.output(StreamOutput::Toast(gettext("Refreshing …")));
                sender.spawn_command(move |out| {
                    let meta = crate::core::streaming::station_by_url(&st.url)
                        .ok()
                        .flatten();
                    // A logo the user picked from a file stays; otherwise search
                    // again (the directory's favicon first, then the homepage).
                    let own = st
                        .favicon
                        .as_deref()
                        .is_some_and(crate::core::online::is_local_station_logo);
                    let favicon = (!own)
                        .then(|| crate::core::station_logo::find_logo(&st.url))
                        .flatten();
                    let _ = out.send(StreamCmd::StreamRefreshed { id, meta, favicon });
                });
            }
            StreamInput::RefreshRecording(id) => {
                let Some(rec) = self.recording_items.iter().find(|r| r.id == id).cloned() else {
                    return;
                };
                let _ = sender.output(StreamOutput::Toast(gettext("Refreshing …")));
                sender.spawn_command(move |out| {
                    let path = std::path::PathBuf::from(&rec.path);
                    let tag = crate::core::scanner::read_track(&path).ok();
                    let artist = first_nonblank(rec.artist.clone(), tag.and_then(|t| t.artist));
                    let hit = lookup_song(artist.as_deref(), &rec.title, rec.station.as_deref());
                    let found = hit.is_some();
                    if let Some((found_artist, album, cover)) = hit {
                        let artist = artist.or(found_artist);
                        crate::core::recorder::embed_cover(
                            &path,
                            artist.as_deref(),
                            &rec.title,
                            album.as_deref(),
                            &cover,
                        );
                        let _ = crate::core::online::store_recording_cover(
                            artist.as_deref().unwrap_or(""),
                            &rec.title,
                            &cover,
                        );
                    }
                    let _ = out.send(StreamCmd::RecordingRefreshed { id, found });
                });
            }
            StreamInput::RefreshHeard(id) => {
                let Some(h) = self.heard_items.iter().find(|x| x.id == id).cloned() else {
                    return;
                };
                let _ = sender.output(StreamOutput::Toast(gettext("Refreshing …")));
                sender.spawn_command(move |out| {
                    let artist = h.artist.clone().filter(|a| !a.trim().is_empty());
                    let hit = lookup_song(artist.as_deref(), &h.title, h.station.as_deref());
                    let found = hit.is_some();
                    let mut new_artist = None;
                    if let Some((found_artist, _, cover)) = hit {
                        // Stored under the artist the list shows, so it finds it.
                        let shown = artist.clone().or_else(|| found_artist.clone());
                        let _ = crate::core::online::store_recording_cover(
                            shown.as_deref().unwrap_or(""),
                            &h.title,
                            &cover,
                        );
                        if artist.is_none() {
                            new_artist = found_artist;
                        }
                    }
                    let _ = out.send(StreamCmd::HeardRefreshed {
                        id,
                        artist: new_artist,
                        found,
                    });
                });
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
            StreamInput::ReloadHeard => self.reload_heard(&sender),
            StreamInput::OpenHeard(id) => self.open_heard(&sender, id),
            StreamInput::HeardDelete(id) => {
                let _ = self.library.delete_heard(id);
                self.reload_heard(&sender);
                let _ = sender.output(StreamOutput::Toast(gettext("Removed from the list")));
            }
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
            StreamCmd::LogosCached(fetched) => {
                if fetched {
                    self.reload_streams(&sender);
                }
            }
            StreamCmd::LogoFound {
                id,
                favicon,
                report,
            } => match favicon {
                Some(fav) => {
                    let _ = self.library.set_stream_favicon(id, Some(&fav));
                    self.reload_streams(&sender);
                    if report {
                        let _ = sender.output(StreamOutput::Toast(gettext("Logo updated")));
                    }
                }
                None if report => {
                    let _ = sender.output(StreamOutput::Toast(gettext("No logo found")));
                }
                None => {}
            },
            StreamCmd::StreamRefreshed { id, meta, favicon } => {
                if let Some(m) = meta.as_ref() {
                    let _ = self.library.set_stream_meta(
                        id,
                        m.tags.as_deref(),
                        m.country.as_deref(),
                        m.codec.as_deref(),
                        m.bitrate,
                    );
                }
                if let Some(fav) = favicon.as_deref() {
                    let _ = self.library.set_stream_favicon(id, Some(fav));
                    crate::ui::widgets::forget_thumb(
                        crate::core::online::station_image_path(fav).as_deref(),
                    );
                }
                if meta.is_none() && favicon.is_none() {
                    let _ = sender.output(StreamOutput::Toast(gettext("Nothing found")));
                }
                self.reload_streams(&sender);
                self.open_stream(&sender, id);
            }
            StreamCmd::RecordingRefreshed { id, found } => {
                if let Some(r) = self.recording_items.iter().find(|r| r.id == id) {
                    crate::ui::widgets::forget_thumb(
                        crate::core::online::recording_cover_path(
                            r.artist.as_deref().unwrap_or(""),
                            &r.title,
                        )
                        .as_deref(),
                    );
                }
                if !found {
                    let _ = sender.output(StreamOutput::Toast(gettext("Nothing found")));
                }
                self.reload_recordings(&sender);
                self.open_recording(&sender, id);
            }
            StreamCmd::HeardRefreshed { id, artist, found } => {
                if let Some(h) = self.heard_items.iter().find(|h| h.id == id) {
                    let shown = h.artist.as_deref().or(artist.as_deref()).unwrap_or("");
                    crate::ui::widgets::forget_thumb(
                        crate::core::online::recording_cover_path(shown, &h.title).as_deref(),
                    );
                }
                if let Some(a) = artist.as_deref() {
                    let _ = self.library.set_heard_artist(id, a);
                }
                if !found {
                    let _ = sender.output(StreamOutput::Toast(gettext("Nothing found")));
                }
                self.reload_heard(&sender);
                self.open_heard(&sender, id);
            }
        }
    }
}
