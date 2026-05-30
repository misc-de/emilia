use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use adw::prelude::*;
use relm4::factory::FactoryVecDeque;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::core::category::{self, Category};
use crate::core::db::Library;
use crate::core::player::Player;
use crate::core::{cover, online, scanner};
use crate::model::{AlbumMeta, ArtistMeta, Track};
use crate::ui::album_row::{AlbumCard, AlbumOutput};
use crate::ui::artist_row::{ArtistCard, ArtistOutput};
use crate::ui::fs_row::{FsEntry, FsInput, FsOutput, FsRow, RowOpts};

/// Ziel der Detailansicht (langes Drücken): eine Datei/ein Ordner im
/// Dateibrowser, ein Interpret, ein Album oder ein Konzert (= Pfad → `Fs`).
#[derive(Clone)]
enum CtxTarget {
    Fs(FsEntry),
    Artist(ArtistMeta),
    Album(AlbumMeta),
}

impl CtxTarget {
    /// Überschrift des Detaildialogs.
    fn heading(&self) -> String {
        match self {
            CtxTarget::Fs(e) => e.heading(),
            CtxTarget::Artist(m) => m.name.clone(),
            CtxTarget::Album(m) => {
                if m.artist.is_empty() {
                    m.album.clone()
                } else {
                    format!("{} - {}", m.artist, m.album)
                }
            }
        }
    }
}

/// Musikalische Bedeutung eines Dateisystem-Ordners (für Wiedergabe & EQ).
enum FsKind {
    /// Ordner eines Interpreten (Name = bekannter Interpret).
    Artist(String),
    /// Ordner genau eines Albums.
    Album { artist: String, album: String },
}

/// Navigationsbereiche: (Stack-Name, Tooltip, Icon). Reihenfolge = Anzeige.
const SECTIONS: [(&str, &str, &str); 5] = [
    ("files", "Dateisystem", "folder-symbolic"),
    ("artists", "Interpreten", "avatar-default-symbolic"),
    ("albums", "Alben", "media-optical-symbolic"),
    ("concerts", "Konzerte", "emilia-concert-symbolic"),
    ("playlists", "Playlisten", "view-list-symbolic"),
];

/// Ab dieser Spieldauer gilt ein Titel als „Lang-Inhalt" und bekommt
/// automatisch eine Resume-Position (15 Minuten).
const RESUME_MIN_DURATION_MS: i64 = 15 * 60 * 1000;
/// Vor dieser Position wird kein Resume gemerkt (zu nah am Anfang).
const RESUME_MIN_POS_MS: i64 = 5_000;
/// So nah vor dem Ende gilt der Titel als fertig → Resume auf 0 zurücksetzen.
const RESUME_END_GUARD_MS: i64 = 10_000;

pub struct App {
    library: Library,
    player: Player,
    entries: FactoryVecDeque<FsRow>,
    albums: FactoryVecDeque<AlbumCard>,
    album_count: usize,
    artists: FactoryVecDeque<ArtistCard>,
    artist_count: usize,
    enriching: bool,
    enrich_status: String,
    /// Cover & Metadaten beim Start automatisch online nachladen (nur bei
    /// nicht-getakteter Verbindung; in den Einstellungen abschaltbar).
    auto_enrich: bool,
    /// Fortschritts-Leiste vom Nutzer ausgeblendet? (Abruf läuft im Hintergrund weiter.)
    enrich_banner_hidden: bool,
    /// Abbruch-Flag für den Anreicherungs-Worker.
    enrich_cancel: Arc<AtomicBool>,
    acoustid_key: Option<String>,
    fanart_key: Option<String>,
    /// Aktuell aktiver Audio-Ausgang (PipeWire-Sink), für die EQ-Auflösung.
    active_output: String,
    music_dir: Option<String>,
    root_dir: Option<PathBuf>,
    browse_dir: Option<PathBuf>,
    /// Aktuell im Dateibrowser angezeigter Ordner (für das Merken der Scrollposition).
    shown_dir: Option<PathBuf>,
    /// Gemerkte Scrollpositionen je Ordner im Dateibrowser, damit man beim
    /// Zurücknavigieren wieder auf gleicher Höhe landet.
    fs_scroll: std::rc::Rc<std::cell::RefCell<std::collections::HashMap<PathBuf, f64>>>,
    loading: bool,
    queue: Vec<PathBuf>,
    queue_pos: usize,
    /// Pfad des aktuell in den Player geladenen Titels (für das Sichern der
    /// Resume-Position beim Wechsel auf einen anderen Titel).
    playing_path: Option<PathBuf>,
    now_playing: Option<String>,
    playing: bool,
    shuffle: bool,
    context_target: Option<CtxTarget>,
    toast_overlay: adw::ToastOverlay,
    // Konzerte
    concert_items: Vec<(String, String, bool)>,
    concerts_list: gtk::ListBox,
    concert_hint_dismissed: bool,
    concerts_hidden: bool,
    concert_nav_buttons: Vec<gtk::ToggleButton>,
    view_stack: adw::ViewStack,
    /// Navigations-Container für die Unterseiten (Interpret → Alben → Album).
    nav_view: adw::NavigationView,
    /// Gemerkte Scrollposition der zuletzt verlassenen Übersichtsseite
    /// (Scroller + Wert), um sie beim Zurücknavigieren wiederherzustellen.
    overview_scroll: std::rc::Rc<std::cell::RefCell<Option<(gtk::ScrolledWindow, f64)>>>,
}

#[derive(Debug)]
pub enum Msg {
    Activate(usize),
    ToggleQueue(usize),
    ShowContextMenu(usize),
    ShowArtistDetail(usize),
    ShowAlbumDetail(usize),
    /// Detailseite eines Albums über (Interpret, Album) öffnen (aus Unterseiten).
    ShowAlbumDetailFor { artist: String, album: String },
    /// Detailseite eines einzelnen Liedes über seinen Pfad öffnen.
    ShowTrackDetail(String),
    /// Lieder-Unterseite eines Albums aus der Album-Übersicht öffnen (kurzes Tippen).
    ShowAlbumTracks(usize),
    ShowConcertDetail(usize),
    /// Kurzes Tippen auf einen Interpreten: dessen Alben & Lieder auflisten.
    OpenArtistTracks(usize),
    /// Tippen auf ein Album in der Interpreten-Unterseite: dessen Titel als
    /// weitere Unterseite auflisten.
    OpenAlbumTracks { artist: String, album: String },
    /// Einen Titel aus der Interpreten-Übersicht abspielen (Queue = alle Titel
    /// des Interpreten, Start beim getippten).
    PlayArtistTrack { name: String, path: String },
    /// Einen Titel aus der Album-Unterseite abspielen (Queue = ganzes Album in
    /// Track-Reihenfolge, Start beim getippten).
    PlayAlbumTrack { artist: String, album: String, path: String },
    /// Ganzes Album in Track-Reihenfolge abspielen (Play-Button der Album-Zeile).
    PlayAlbum { artist: String, album: String },
    CtxPlay,
    /// Album in Track-Reihenfolge abspielen (Shuffle aus, am Ende Stopp).
    CtxPlayAlbum,
    /// Alle Titel des Interpreten abspielen: Alben nach Jahr (neueste bzw.
    /// älteste zuerst), je Album von Track 1 top-down (Shuffle aus).
    CtxPlayArtist {
        newest_first: bool,
    },
    CtxAddQueue,
    CtxAddPlaylist,
    CtxEqualizer,
    CtxShare,
    ShareHost,
    ShareScan,
    TrackFinished,
    /// Periodischer Tick: Resume-Position des laufenden Titels sichern.
    PersistResume,
    Next,
    Prev,
    ToggleShuffle,
    NavUp,
    FilesGoStart,
    Refresh,
    /// Header-Knopf: Abruf starten – oder, falls schon läuft, abbrechen.
    ToggleEnrich,
    /// Fortschritts-Leiste ausblenden (der Abruf läuft im Hintergrund weiter).
    HideEnrichBanner,
    TogglePlay,
    /// Detailansicht des gerade laufenden Titels öffnen (Klick auf die Leiste).
    OpenNowPlaying,
    OpenSettings,
    OpenGlobalEq,
    SetMusicDir(PathBuf),
    SetAcoustidKey(String),
    /// Primäres Cover eines Albums festlegen (zuletzt im Galerie-Karussell gezeigt).
    SetAlbumCover { artist: String, album: String, path: String },
    /// Primäres Foto eines Interpreten festlegen (zuletzt im Galerie-Karussell gezeigt).
    SetArtistImage { name: String, path: String },
    SetFanartKey(String),
    /// Automatischen Online-Abruf an-/ausschalten.
    SetAutoEnrich(bool),
    /// Merkmal einer Ebene setzen (oder bei `None` auf „erben" zurücksetzen).
    SetCategory {
        scope: &'static str,
        key: String,
        value: Option<&'static str>,
    },
    /// Equalizer-Bänder eines Ausgangs + einer Ebene speichern und anwenden.
    SetEq {
        output: String,
        scope: &'static str,
        key: String,
        bands: [f64; 10],
    },
    /// Equalizer eines Ausgangs + einer Ebene zurücksetzen (erbt wieder).
    ClearEq {
        output: String,
        scope: &'static str,
        key: String,
    },
    // Konzerte
    ConcertImport,
    ConcertDismissHint,
    ConcertHideSection,
    ConcertAdd(Vec<(String, String, bool)>),
    PlayConcert(usize),
    ConcertRemove(usize),
    SetConcertsVisible(bool),
}

/// Ergebnisse der Hintergrund-Worker (Ordner lesen bzw. Online-Anreicherung).
#[derive(Debug)]
pub enum Cmd {
    Entries(Vec<FsEntry>),
    /// Fortschritt einer Anreicherungsphase (`phase` = Anzeigetext).
    EnrichProgress { phase: String, done: usize, total: usize },
    /// Abschluss: Anzahl zugeordneter Alben, Interpreten und Titel.
    EnrichDone { albums: usize, artists: usize, tracks: usize },
    /// Zwischenstand: Alben-/Interpreten-Ansicht neu laden (z. B. nach einer Phase).
    ReloadViews,
    /// Lokaler Bibliotheks-Scan fertig; `then_enrich` = danach ggf. online nachladen.
    ScanDone { then_enrich: bool },
    /// Gefundene Konzert-Kandidaten (für den Import-Dialog).
    Candidates(Vec<crate::core::concert::Candidate>),
}

#[relm4::component(pub)]
impl Component for App {
    type Init = ();
    type Input = Msg;
    type Output = ();
    /// Ergebnis der Hintergrund-Worker (Ordner lesen / Online-Anreicherung).
    type CommandOutput = Cmd;

    view! {
        adw::ApplicationWindow {
            set_title: Some("Emilia"),
            set_default_width: 800,
            set_default_height: 600,

            #[local_ref]
            toast_overlay -> adw::ToastOverlay {
                #[wrap(Some)]
                #[name = "nav_view"]
                set_child = &adw::NavigationView {
                    // Wurzelseite: die eigentliche App (Navigation, Inhalt, Mini-Player).
                    // Interpreten-/Album-Unterseiten werden darauf geschoben.
                    adw::NavigationPage {
                        set_title: "Emilia",
                        set_tag: Some("main"),
                        #[wrap(Some)]
                        #[name = "split"]
                        set_child = &adw::OverlaySplitView {
                set_collapsed: false,
                set_enable_show_gesture: false,
                set_enable_hide_gesture: false,
                set_min_sidebar_width: 64.0,
                set_max_sidebar_width: 84.0,

                // Seitenleiste (Desktop): icon-only Navigation links
                #[wrap(Some)]
                set_sidebar = &adw::ToolbarView {
                    add_top_bar = &adw::HeaderBar {
                        #[wrap(Some)]
                        set_title_widget = &adw::WindowTitle::new("", ""),
                    },
                    #[wrap(Some)]
                    #[name = "sidebar_nav"]
                    set_content = &gtk::Box {
                        set_orientation: gtk::Orientation::Vertical,
                        set_spacing: 4,
                        set_margin_top: 8,
                        set_margin_start: 6,
                        set_margin_end: 6,
                        set_halign: gtk::Align::Center,
                        set_valign: gtk::Align::Start,
                    },
                },

                #[wrap(Some)]
                set_content = &adw::ToolbarView {
                    add_top_bar = &adw::HeaderBar {
                        #[wrap(Some)]
                        set_title_widget = &adw::WindowTitle::new("Emilia", ""),
                        pack_start = &gtk::Button {
                            set_icon_name: "emblem-system-symbolic",
                            set_tooltip_text: Some("Einstellungen"),
                            connect_clicked => Msg::OpenSettings,
                        },
                        pack_start = &gtk::Button {
                            set_icon_name: "view-refresh-symbolic",
                            set_tooltip_text: Some("Ordner neu einlesen"),
                            connect_clicked => Msg::Refresh,
                        },
                        pack_start = &gtk::Button {
                            // Während eines laufenden Abrufs zum Abbrechen-Knopf.
                            #[watch]
                            set_icon_name: if model.enriching {
                                "process-stop-symbolic"
                            } else {
                                "folder-download-symbolic"
                            },
                            #[watch]
                            set_tooltip_text: Some(if model.enriching {
                                "Abruf abbrechen"
                            } else {
                                "Cover & Metadaten online abrufen"
                            }),
                            connect_clicked => Msg::ToggleEnrich,
                        },
                    },

                    // Top-Navigation (icon-only) – nur im schmalen (mobilen) Modus
                    #[name = "top_nav"]
                    add_top_bar = &gtk::Box {
                        set_halign: gtk::Align::Center,
                        set_spacing: 6,
                        set_visible: false,
                        set_margin_top: 4,
                        set_margin_bottom: 6,
                    },

                    // Globaler Fortschritt der Online-Anreicherung – die graue Leiste
                    // auf allen Seiten. Der Nutzer kann sie ausblenden (der Abruf
                    // läuft im Hintergrund weiter; Abbrechen über den Header-Knopf).
                    add_top_bar = &adw::Banner {
                        #[watch]
                        set_revealed: model.enriching && !model.enrich_banner_hidden,
                        #[watch]
                        set_title: &model.enrich_status,
                        set_button_label: Some("Ausblenden"),
                        connect_button_clicked => Msg::HideEnrichBanner,
                    },

                    // Inhalt mit Lade-Overlay
                    #[wrap(Some)]
                    set_content = &gtk::Overlay {
                        #[wrap(Some)]
                        #[name = "view_stack"]
                        set_child = &adw::ViewStack {
                            add_titled_with_icon[Some("files"), "Dateisystem", "folder-symbolic"] =
                                &gtk::Box {
                                    set_orientation: gtk::Orientation::Vertical,

                                    #[name = "files_page"]
                                    gtk::Box {
                                        set_orientation: gtk::Orientation::Vertical,
                                        set_vexpand: true,

                                        // Pfad-/Zurück-Leiste – nur in Unterordnern
                                        gtk::Box {
                                            set_spacing: 6,
                                            set_margin_all: 6,
                                            #[watch]
                                            set_visible: model.can_go_up(),
                                            gtk::Button {
                                                set_icon_name: "go-previous-symbolic",
                                                set_tooltip_text: Some("Zurück"),
                                                add_css_class: "flat",
                                                #[watch]
                                                set_sensitive: model.can_go_up(),
                                                connect_clicked => Msg::NavUp,
                                            },
                                            gtk::Label {
                                                set_hexpand: true,
                                                set_xalign: 0.0,
                                                set_ellipsize: gtk::pango::EllipsizeMode::Start,
                                                add_css_class: "heading",
                                                #[watch]
                                                set_label: &model.folder_label(),
                                            },
                                        },

                                        gtk::ScrolledWindow {
                                            set_vexpand: true,
                                            #[local_ref]
                                            entries_box -> gtk::ListBox {
                                                set_valign: gtk::Align::Start,
                                                set_margin_top: 0,
                                                set_margin_bottom: 0,
                                                set_margin_start: 12,
                                                set_margin_end: 12,
                                                set_css_classes: &["boxed-list"],
                                            },
                                        },
                                    },
                                },
                            add_titled_with_icon[Some("artists"), "Interpreten", "avatar-default-symbolic"] =
                                &gtk::Box {
                                    set_orientation: gtk::Orientation::Vertical,

                                    adw::Banner {
                                        set_visible: false,
                                        #[watch]
                                        set_title: &model.enrich_status,
                                    },

                                    adw::StatusPage {
                                        set_icon_name: Some("avatar-default-symbolic"),
                                        set_title: "Keine Interpreten",
                                        set_description: Some(
                                            "Musikordner einlesen und „Online-Metadaten“ abrufen",
                                        ),
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.artist_count == 0,
                                    },

                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.artist_count > 0,
                                        #[local_ref]
                                        artists_box -> gtk::ListBox {
                                            set_valign: gtk::Align::Start,
                                            set_margin_top: 0,
                                            set_margin_bottom: 12,
                                            set_margin_start: 12,
                                            set_margin_end: 12,
                                            set_css_classes: &["boxed-list"],
                                        },
                                    },
                                },
                            add_titled_with_icon[Some("albums"), "Alben", "media-optical-symbolic"] =
                                &gtk::Box {
                                    set_orientation: gtk::Orientation::Vertical,

                                    // Fortschritt der Online-Anreicherung
                                    adw::Banner {
                                        set_visible: false,
                                        #[watch]
                                        set_title: &model.enrich_status,
                                    },

                                    // Leerzustand, solange keine Alben bekannt sind
                                    adw::StatusPage {
                                        set_icon_name: Some("media-optical-symbolic"),
                                        set_title: "Keine Alben",
                                        set_description: Some(
                                            "Musikordner einlesen und „Online-Metadaten“ abrufen",
                                        ),
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.album_count == 0,
                                    },

                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.album_count > 0,
                                        #[local_ref]
                                        albums_box -> gtk::ListBox {
                                            set_valign: gtk::Align::Start,
                                            set_margin_top: 0,
                                            set_margin_bottom: 12,
                                            set_margin_start: 12,
                                            set_margin_end: 12,
                                            set_css_classes: &["boxed-list"],
                                        },
                                    },
                                },
                            add_titled_with_icon[Some("concerts"), "Konzerte", "emilia-concert-symbolic"] =
                                &gtk::Box {
                                    set_orientation: gtk::Orientation::Vertical,

                                    // Liste der markierten Konzerte
                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: !model.concert_items.is_empty(),
                                        #[local_ref]
                                        concerts_list -> gtk::ListBox {
                                            set_valign: gtk::Align::Start,
                                            set_margin_top: 0,
                                            set_margin_bottom: 12,
                                            set_margin_start: 12,
                                            set_margin_end: 12,
                                            set_css_classes: &["boxed-list"],
                                        },
                                    },

                                    // Hinweis + Aktionen (leer & Hinweis aktiv)
                                    adw::StatusPage {
                                        set_icon_name: Some("emilia-concert-symbolic"),
                                        set_title: "Konzerte",
                                        set_description: Some("Hier kannst du deine gesammelten Konzerte auflisten. Über Konzerte importieren bekommst du eine Übersicht vermuteter Konzerte: Alben mit live, unplugged oder concert im Namen sowie Einzeldateien ab 30 Minuten. Markiere sie als Konzert, dann erscheinen sie hier. Du kannst Konzerte auch jederzeit später über die Optionen hinzufügen."),
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.concert_items.is_empty() && !model.concert_hint_dismissed,
                                        #[wrap(Some)]
                                        set_child = &gtk::Box {
                                            set_orientation: gtk::Orientation::Vertical,
                                            set_spacing: 8,
                                            set_halign: gtk::Align::Center,
                                            gtk::Button {
                                                set_label: "Konzerte importieren",
                                                set_css_classes: &["suggested-action", "pill"],
                                                connect_clicked => Msg::ConcertImport,
                                            },
                                            gtk::Button {
                                                set_label: "Das mache ich selber",
                                                add_css_class: "pill",
                                                connect_clicked => Msg::ConcertDismissHint,
                                            },
                                            gtk::Button {
                                                set_label: "Menüpunkt ausblenden",
                                                add_css_class: "pill",
                                                connect_clicked => Msg::ConcertHideSection,
                                            },
                                        },
                                    },

                                    // Leerzustand (leer & Hinweis ausgeblendet):
                                    // Nutzer hat „Das mache ich selber" gewählt – daher
                                    // bewusst KEIN Import-Button mehr.
                                    adw::StatusPage {
                                        set_icon_name: Some("emilia-concert-symbolic"),
                                        set_title: "Keine Konzerte",
                                        set_description: Some("Markiere ein Album oder einen Titel über die Optionen als Konzert."),
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.concert_items.is_empty() && model.concert_hint_dismissed,
                                    },
                                },
                            add_titled_with_icon[Some("playlists"), "Playlisten", "view-list-symbolic"] =
                                &adw::StatusPage {
                                    set_icon_name: Some("view-list-symbolic"),
                                    set_title: "Playlisten",
                                    set_description: Some("Kommt bald"),
                                },
                        },

                        // Zentrierter Spinner während des Einlesens
                        add_overlay = &gtk::Box {
                            set_orientation: gtk::Orientation::Vertical,
                            set_halign: gtk::Align::Center,
                            set_valign: gtk::Align::Center,
                            set_spacing: 12,
                            set_can_target: false,
                            #[watch]
                            set_visible: model.loading,

                            gtk::Spinner {
                                set_spinning: true,
                                set_size_request: (48, 48),
                            },
                            gtk::Label {
                                set_label: "Musikdaten werden eingelesen",
                                add_css_class: "dim-label",
                            },
                        },
                    },

                    // Mini-Player unten mit Transport-Steuerung
                    add_bottom_bar = &gtk::Box {
                        set_orientation: gtk::Orientation::Vertical,
                        set_spacing: 2,
                        set_margin_top: 4,
                        set_margin_bottom: 6,
                        set_margin_start: 10,
                        set_margin_end: 10,

                        gtk::Button {
                            add_css_class: "flat",
                            set_tooltip_text: Some("Details zum laufenden Titel"),
                            // Nur anklickbar, wenn etwas läuft.
                            #[watch]
                            set_sensitive: model.now_playing.is_some(),
                            connect_clicked => Msg::OpenNowPlaying,
                            #[wrap(Some)]
                            set_child = &gtk::Label {
                                set_xalign: 0.5,
                                set_ellipsize: gtk::pango::EllipsizeMode::End,
                                add_css_class: "caption",
                                #[watch]
                                set_label: model.now_playing.as_deref()
                                    .unwrap_or("Nichts ausgewählt"),
                            },
                        },

                        gtk::CenterBox {
                            // Zufall linksbündig, Transport-Tasten mittig
                            #[wrap(Some)]
                            set_start_widget = &gtk::ToggleButton {
                                set_icon_name: "media-playlist-shuffle-symbolic",
                                set_tooltip_text: Some("Zufall"),
                                set_valign: gtk::Align::Center,
                                add_css_class: "flat",
                                #[watch]
                                set_active: model.shuffle,
                                connect_clicked => Msg::ToggleShuffle,
                            },
                            #[wrap(Some)]
                            set_center_widget = &gtk::Box {
                                set_spacing: 6,
                                gtk::Button {
                                    set_icon_name: "media-skip-backward-symbolic",
                                    set_tooltip_text: Some("Zurück"),
                                    add_css_class: "flat",
                                    connect_clicked => Msg::Prev,
                                },
                                gtk::Button {
                                    #[watch]
                                    set_icon_name: if model.playing {
                                        "media-playback-pause-symbolic"
                                    } else {
                                        "media-playback-start-symbolic"
                                    },
                                    set_tooltip_text: Some("Wiedergabe/Pause"),
                                    add_css_class: "circular",
                                    connect_clicked => Msg::TogglePlay,
                                },
                                gtk::Button {
                                    set_icon_name: "media-skip-forward-symbolic",
                                    set_tooltip_text: Some("Vorwärts"),
                                    add_css_class: "flat",
                                    connect_clicked => Msg::Next,
                                },
                            },
                        },
                    },
                },
                }
                    }
                }
            }
        }
    }

    fn init(
        _init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        // Eigene App-Icons (z. B. das Konzert-Mikro) auffindbar machen.
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::IconTheme::for_display(&display)
                .add_search_path(concat!(env!("CARGO_MANIFEST_DIR"), "/data/icons"));

            // Cover/Fotos in Alben-/Interpreten-Liste ganz links (kein Einzug).
            let css = gtk::CssProvider::new();
            css.load_from_string(
                "row.emilia-flush > box.header { padding-left: 0px; margin-left: 0px; }\
                 row.emilia-flush > box.header > box.prefixes { margin-left: 0px; margin-right: 8px; }",
            );
            gtk::style_context_add_provider_for_display(
                &display,
                &css,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }

        let library = Library::open().expect("Konnte Bibliotheks-DB nicht öffnen");
        let player = Player::new().expect("Konnte GStreamer nicht initialisieren");
        let music_dir = library.get_setting("music_dir").ok().flatten();
        let root_dir = music_dir.as_ref().map(PathBuf::from);
        // Zuletzt offenen Ordner wiederherstellen – nur wenn er noch existiert
        // und unter dem Startordner liegt; sonst den Startordner selbst.
        let browse_dir = library
            .get_setting("browse_dir")
            .ok()
            .flatten()
            .map(PathBuf::from)
            .filter(|p| root_dir.as_ref().is_some_and(|r| p.starts_with(r)) && p.is_dir())
            .or_else(|| root_dir.clone());

        // Zuletzt gespeicherte Fenstergröße / Maximierung.
        let saved_w = library
            .get_setting("win_width")
            .ok()
            .flatten()
            .and_then(|s| s.parse::<i32>().ok());
        let saved_h = library
            .get_setting("win_height")
            .ok()
            .flatten()
            .and_then(|s| s.parse::<i32>().ok());
        let saved_max = matches!(
            library.get_setting("win_maximized").ok().flatten().as_deref(),
            Some("1")
        );
        // Konzerte-Optionen.
        let concert_hint_dismissed = matches!(
            library
                .get_setting("concert_hint_dismissed")
                .ok()
                .flatten()
                .as_deref(),
            Some("1")
        );
        let concerts_hidden = matches!(
            library.get_setting("concerts_hidden").ok().flatten().as_deref(),
            Some("1")
        );
        // Automatischer Online-Abruf (Standard: an; nur „0" schaltet ihn aus).
        let auto_enrich = !matches!(
            library.get_setting("auto_enrich").ok().flatten().as_deref(),
            Some("0")
        );
        // Zuletzt offener Navigationspunkt (nur gültige Sektionsnamen zulassen).
        let saved_section = library
            .get_setting("active_section")
            .ok()
            .flatten()
            .filter(|s| SECTIONS.iter().any(|(name, _, _)| name == s));

        let entries = FactoryVecDeque::builder()
            .launch(gtk::ListBox::default())
            .forward(sender.input_sender(), |out| match out {
                FsOutput::Activated(index) => Msg::Activate(index.current_index()),
                FsOutput::LongPress(index) => Msg::ShowContextMenu(index.current_index()),
                FsOutput::DoubleClick(index) => Msg::ToggleQueue(index.current_index()),
            });

        let albums = FactoryVecDeque::builder()
            .launch(gtk::ListBox::default())
            .forward(sender.input_sender(), |out| match out {
                AlbumOutput::Activated(index) => Msg::ShowAlbumTracks(index.current_index()),
                AlbumOutput::LongPress(index) => Msg::ShowAlbumDetail(index.current_index()),
            });

        let artists = FactoryVecDeque::builder()
            .launch(gtk::ListBox::default())
            .forward(sender.input_sender(), |out| match out {
                ArtistOutput::Activated(index) => Msg::OpenArtistTracks(index.current_index()),
                ArtistOutput::LongPress(index) => Msg::ShowArtistDetail(index.current_index()),
            });

        let acoustid_key = library.get_setting("acoustid_key").ok().flatten();
        let fanart_key = library.get_setting("fanart_key").ok().flatten();
        let active_output = crate::core::output::default_output().unwrap_or_default();

        // Beim Titelende automatisch den nächsten Eintrag der Warteschlange spielen.
        {
            let sender = sender.clone();
            player.connect_eos(move || sender.input(Msg::TrackFinished));
        }

        // Während der Wiedergabe regelmäßig die Resume-Position sichern, damit
        // ein Hörspiel auch nach einem Absturz/Schließen dort weiterläuft.
        {
            let sender = sender.clone();
            gtk::glib::timeout_add_seconds_local(5, move || {
                sender.input(Msg::PersistResume);
                gtk::glib::ControlFlow::Continue
            });
        }

        let toast_overlay = adw::ToastOverlay::new();
        let concerts_list = gtk::ListBox::new();

        let mut model = App {
            library,
            player,
            entries,
            albums,
            album_count: 0,
            artists,
            artist_count: 0,
            enriching: false,
            enrich_status: String::new(),
            auto_enrich,
            enrich_banner_hidden: false,
            enrich_cancel: Arc::new(AtomicBool::new(false)),
            acoustid_key,
            fanart_key,
            active_output,
            music_dir,
            root_dir,
            browse_dir,
            shown_dir: None,
            fs_scroll: std::rc::Rc::new(std::cell::RefCell::new(std::collections::HashMap::new())),
            loading: false,
            queue: Vec::new(),
            queue_pos: 0,
            playing_path: None,
            now_playing: None,
            playing: false,
            shuffle: false,
            context_target: None,
            toast_overlay: toast_overlay.clone(),
            concert_items: Vec::new(),
            concerts_list: concerts_list.clone(),
            concert_hint_dismissed,
            concerts_hidden,
            concert_nav_buttons: Vec::new(),
            view_stack: adw::ViewStack::new(),
            nav_view: adw::NavigationView::new(),
            overview_scroll: std::rc::Rc::new(std::cell::RefCell::new(None)),
        };

        model.load_dir(&sender);
        model.reload_albums();
        model.reload_artists();
        model.load_concerts(&sender);
        // Bibliothek beim Start automatisch einlesen und – bei WLAN/LAN und
        // aktiviertem Schalter – fehlende Cover/Metadaten im Hintergrund nachladen.
        model.start_scan(&sender, true);

        let entries_box = model.entries.widget();
        let albums_box = model.albums.widget();
        let artists_box = model.artists.widget();
        let widgets = view_output!();
        model.view_stack = widgets.view_stack.clone();
        model.nav_view = widgets.nav_view.clone();

        // Scrollposition der Übersicht über Navigation hinweg erhalten:
        // `adw::NavigationView` setzt die Position beim Wieder-Einblenden auf 0
        // zurück. Daher beim Zurückkehren zur Wurzelseite den gemerkten Wert
        // wiederherstellen (kurz verzögert, nach dem Neu-Layout).
        {
            let saved = model.overview_scroll.clone();
            widgets.nav_view.connect_popped(move |nav, _page| {
                // Nur wenn wir zur Wurzel-Übersicht zurückkehren.
                let is_root = nav
                    .visible_page()
                    .and_then(|p| p.tag())
                    .is_some_and(|t| t == "main");
                if !is_root {
                    return;
                }
                if let Some((sc, value)) = saved.borrow().clone() {
                    // Kurz verzögert wiederherstellen (erst nach dem Neu-Layout, das
                    // den Scroller sonst auf 0 zurücksetzt); zweiter Versuch als
                    // Absicherung gegen Timing-Schwankungen.
                    for delay in [50u64, 250] {
                        let sc = sc.clone();
                        gtk::glib::timeout_add_local_once(
                            std::time::Duration::from_millis(delay),
                            move || sc.vadjustment().set_value(value),
                        );
                    }
                }
            });
        }

        // Adaptiv: nur bei mobiler (schmaler) Breite Seitenleiste einklappen und
        // Top-Navi zeigen. Auf dem Desktop bleibt initial die linke Seitenleiste.
        let breakpoint = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
            adw::BreakpointConditionLengthType::MaxWidth,
            550.0,
            adw::LengthUnit::Sp,
        ));
        let yes = true.to_value();
        breakpoint.add_setter(&widgets.split, "collapsed", Some(&yes));
        breakpoint.add_setter(&widgets.top_nav, "visible", Some(&yes));
        root.add_breakpoint(breakpoint);

        // Icon-only Navigation (Seitenleiste + oben) erzeugen und an den Stack koppeln.
        let mut nav_buttons: Vec<(&'static str, gtk::ToggleButton)> = Vec::new();
        let mut concert_btns: Vec<gtk::ToggleButton> = Vec::new();
        for container in [widgets.sidebar_nav.clone(), widgets.top_nav.clone()] {
            let mut group_leader: Option<gtk::ToggleButton> = None;
            for (name, label, icon) in SECTIONS {
                // „Konzerte"-Menüpunkt ggf. komplett auslassen.
                if name == "concerts" && concerts_hidden {
                    continue;
                }
                let btn = gtk::ToggleButton::builder()
                    .icon_name(icon)
                    .tooltip_text(label)
                    .build();
                btn.add_css_class("flat");
                match &group_leader {
                    Some(leader) => btn.set_group(Some(leader)),
                    None => group_leader = Some(btn.clone()),
                }
                {
                    let stack = widgets.view_stack.clone();
                    let sender = sender.clone();
                    btn.connect_clicked(move |b| {
                        if b.is_active() {
                            stack.set_visible_child_name(name);
                            // Klick auf den Menüpunkt = zum Anfang des Bereichs.
                            if name == "files" {
                                sender.input(Msg::FilesGoStart);
                            }
                        }
                    });
                }
                container.append(&btn);
                if name == "concerts" {
                    concert_btns.push(btn.clone());
                }
                nav_buttons.push((name, btn));
            }
        }
        model.concert_nav_buttons = concert_btns;
        // Aktiven Button passend zur sichtbaren Stack-Seite setzen.
        let sync_active = move |stack: &adw::ViewStack, buttons: &[(&'static str, gtk::ToggleButton)]| {
            let cur = stack.visible_child_name();
            let cur = cur.as_deref().unwrap_or("files");
            for (name, btn) in buttons {
                btn.set_active(*name == cur);
            }
        };
        // Zuletzt offenen Navigationspunkt wiederherstellen.
        if let Some(section) = saved_section.as_deref() {
            widgets.view_stack.set_visible_child_name(section);
        }
        sync_active(&widgets.view_stack, &nav_buttons);
        widgets
            .view_stack
            .connect_visible_child_notify(move |stack| sync_active(stack, &nav_buttons));

        // Wisch-Geste auf der ganzen Dateisystem-Seite: nach rechts = zurück.
        let swipe = gtk::GestureSwipe::new();
        swipe.set_touch_only(false);
        {
            let sender = sender.clone();
            swipe.connect_swipe(move |_, vx, vy| {
                if vx > 300.0 && vx.abs() > vy.abs() * 1.5 {
                    sender.input(Msg::NavUp);
                }
            });
        }
        widgets.files_page.add_controller(swipe);

        // Fenstergröße wiederherstellen und beim Schließen speichern.
        if let (Some(w), Some(h)) = (saved_w, saved_h) {
            root.set_default_size(w, h);
        }
        if saved_max {
            root.maximize();
        }
        let stack_for_close = widgets.view_stack.clone();
        root.connect_close_request(move |win| {
            let section = stack_for_close.visible_child_name();
            save_window_state(
                win.default_width(),
                win.default_height(),
                win.is_maximized(),
                section.as_deref(),
            );
            gtk::glib::Propagation::Proceed
        });

        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, sender: ComponentSender<Self>, root: &Self::Root) {
        match msg {
            Msg::Activate(index) => {
                let entry = self.entries.guard().get(index).map(|r| r.entry.clone());
                if let Some(entry) = entry {
                    if entry.is_dir() {
                        self.browse_dir = Some(entry.path().clone());
                        self.load_dir(&sender);
                    } else {
                        let path = entry.path().clone();
                        // Aktives Lied erneut antippen → Wiedergabe umschalten
                        // (Pause/Weiter), statt neu zu starten.
                        let is_active = self.now_playing.is_some()
                            && self.queue.get(self.queue_pos) == Some(&path);
                        if is_active {
                            if self.playing {
                                self.save_resume();
                                self.player.pause();
                            } else {
                                self.player.resume();
                            }
                            self.playing = !self.playing;
                            self.refresh_queue_icons();
                        } else {
                            // Einzeltitel: Warteschlange = nur dieser Titel.
                            self.queue = vec![path];
                            self.queue_pos = 0;
                            self.play_current();
                            self.refresh_queue_icons();
                        }
                    }
                }
            }
            Msg::ToggleQueue(index) => {
                let path = self
                    .entries
                    .guard()
                    .get(index)
                    .filter(|r| !r.entry.is_dir())
                    .map(|r| r.entry.path().clone());
                if let Some(path) = path {
                    if let Some(pos) = self.queue.iter().position(|p| *p == path) {
                        self.queue.remove(pos);
                        if self.queue_pos > pos {
                            self.queue_pos -= 1;
                        }
                        self.toast("Aus der Warteschlange entfernt");
                    } else {
                        self.queue.push(path);
                        self.toast("Zum nächsten Abspielen vorgemerkt");
                    }
                    self.refresh_queue_icons();
                }
            }
            Msg::ShowContextMenu(index) => {
                let entry = self
                    .entries
                    .guard()
                    .get(index)
                    .map(|r| CtxTarget::Fs(r.entry.clone()));
                if entry.is_some() {
                    self.context_target = entry;
                    self.open_context_menu(root, &sender);
                }
            }
            Msg::ShowArtistDetail(index) => {
                let meta = self.artists.guard().get(index).map(|c| c.meta.clone());
                if let Some(meta) = meta {
                    self.context_target = Some(CtxTarget::Artist(meta));
                    self.open_context_menu(root, &sender);
                }
            }
            Msg::ShowAlbumDetail(index) => {
                let meta = self.albums.guard().get(index).map(|c| c.meta.clone());
                if let Some(meta) = meta {
                    self.context_target = Some(CtxTarget::Album(meta));
                    self.open_context_menu(root, &sender);
                }
            }
            Msg::ShowAlbumDetailFor { artist, album } => {
                // Album-Metadaten laden (für Cover/Jahr), sonst leeren Eintrag.
                let meta = self
                    .library
                    .get_album_meta(&artist, &album)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| crate::model::AlbumMeta::pending(artist, album));
                self.context_target = Some(CtxTarget::Album(meta));
                self.open_context_menu(root, &sender);
            }
            Msg::ShowTrackDetail(path) => {
                self.context_target =
                    Some(CtxTarget::Fs(FsEntry::file(PathBuf::from(path))));
                self.open_context_menu(root, &sender);
            }
            Msg::ShowAlbumTracks(index) => {
                let meta = self.albums.guard().get(index).map(|c| c.meta.clone());
                if let Some(meta) = meta {
                    self.open_album_tracks(&sender, &meta.artist, &meta.album);
                }
            }
            Msg::ShowConcertDetail(index) => {
                // Ein Konzert ist ein Pfad – als Datei/Ordner-Eintrag in denselben
                // Dialog wie im Dateibrowser geben.
                if let Some((path, _, is_dir)) = self.concert_items.get(index).cloned() {
                    let path = PathBuf::from(path);
                    let entry = if is_dir {
                        FsEntry::dir(path)
                    } else {
                        FsEntry::file(path)
                    };
                    self.context_target = Some(CtxTarget::Fs(entry));
                    self.open_context_menu(root, &sender);
                }
            }
            Msg::OpenArtistTracks(index) => {
                let meta = self.artists.guard().get(index).map(|c| c.meta.clone());
                if let Some(meta) = meta {
                    self.open_artist_tracks(&sender, &meta);
                }
            }
            Msg::OpenAlbumTracks { artist, album } => {
                self.open_album_tracks(&sender, &artist, &album);
            }
            Msg::PlayArtistTrack { name, path } => {
                // Queue = alle Titel des Interpreten (album-übergreifend),
                // Start beim getippten Titel.
                let files: Vec<PathBuf> = self
                    .artist_albums(&name)
                    .into_iter()
                    .flat_map(|(_, tracks)| tracks)
                    .map(|t| PathBuf::from(t.path))
                    .collect();
                let target = PathBuf::from(&path);
                if let Some(pos) = files.iter().position(|p| *p == target) {
                    self.queue = files;
                    self.queue_pos = pos;
                    self.play_current();
                    self.refresh_queue_icons();
                    // Zur Hauptseite zurück, damit der Mini-Player sichtbar ist.
                    self.nav_view.pop_to_tag("main");
                }
            }
            Msg::PlayAlbumTrack { artist, album, path } => {
                // Queue = ganzes Album in Track-Reihenfolge, Start beim getippten.
                // `artist` ist hier der (Seiten-)Interpret – dieselbe Titelmenge
                // wie auf der Album-Unterseite.
                let files: Vec<PathBuf> = self
                    .album_tracks_for_artist(&artist, &album)
                    .into_iter()
                    .map(|t| PathBuf::from(t.path))
                    .collect();
                let target = PathBuf::from(&path);
                if let Some(pos) = files.iter().position(|p| *p == target) {
                    self.queue = files;
                    self.queue_pos = pos;
                    self.play_current();
                    self.refresh_queue_icons();
                    self.nav_view.pop_to_tag("main");
                }
            }
            Msg::PlayAlbum { artist, album } => {
                // Ganzes Album ab Titel 1 in Track-Reihenfolge (Shuffle aus).
                let files: Vec<PathBuf> = self
                    .album_tracks_for_artist(&artist, &album)
                    .into_iter()
                    .map(|t| PathBuf::from(t.path))
                    .collect();
                if !files.is_empty() {
                    self.shuffle = false;
                    self.queue = files;
                    self.queue_pos = 0;
                    self.play_current();
                    self.refresh_queue_icons();
                    self.nav_view.pop_to_tag("main");
                }
            }
            Msg::CtxPlay => {
                if let Some(entry) = self.context_target.clone() {
                    let files = self.ctx_files(&entry);
                    if !files.is_empty() {
                        self.queue = files;
                        self.queue_pos = 0;
                        self.play_current();
                        self.refresh_queue_icons();
                    }
                }
            }
            Msg::CtxPlayAlbum => {
                // Album immer in Track-Reihenfolge ab Lied 1, ohne Zufall; am Ende
                // der Queue stoppt `play_next` von selbst (kein weiteres Lied).
                if let Some((artist, album)) = self.ctx_album() {
                    let files = self.album_files(&artist, &album);
                    if !files.is_empty() {
                        self.shuffle = false;
                        self.queue = files;
                        self.queue_pos = 0;
                        self.play_current();
                        self.refresh_queue_icons();
                    }
                }
            }
            Msg::CtxPlayArtist { newest_first } => {
                // Alben nach Jahr (älteste/neueste zuerst), je Album top-down,
                // ohne Zufall.
                if let Some(name) = self.ctx_artist() {
                    let files = self.artist_files_ordered(&name, newest_first);
                    if !files.is_empty() {
                        self.shuffle = false;
                        self.queue = files;
                        self.queue_pos = 0;
                        self.play_current();
                        self.refresh_queue_icons();
                    }
                }
            }
            Msg::CtxAddQueue => {
                if let Some(entry) = self.context_target.clone() {
                    let mut files = self.ctx_files(&entry);
                    let n = files.len();
                    let was_empty = self.queue.is_empty();
                    self.queue.append(&mut files);
                    if was_empty && !self.queue.is_empty() {
                        self.queue_pos = 0;
                        self.play_current();
                    }
                    self.refresh_queue_icons();
                    self.toast(&format!("{n} Titel zur Queue hinzugefügt"));
                }
            }
            Msg::CtxAddPlaylist => self.toast("Playlists kommen bald"),
            Msg::CtxEqualizer => self.open_eq_dialog(root, &sender),
            Msg::CtxShare => self.open_share_dialog(root, &sender),
            Msg::ShareHost => {
                self.toast("Verbindungsdienst – kommt bald");
            }
            Msg::ShareScan => {
                self.toast("QR-Code einlesen – kommt bald");
            }
            Msg::TrackFinished => {
                // Titel zu Ende gehört → Resume vergessen, nächstes Mal von vorn.
                // `take()` verhindert, dass play_current die (End-)Position erneut
                // als Resume-Punkt speichert.
                if let Some(path) = self.playing_path.take() {
                    let _ = self.library.set_resume_path(&path.to_string_lossy(), 0);
                }
                self.play_next();
            }
            Msg::PersistResume => {
                if self.playing {
                    self.save_resume();
                }
            }
            Msg::Next => self.play_next(),
            Msg::Prev => self.play_prev(),
            Msg::ToggleShuffle => self.shuffle = !self.shuffle,
            Msg::NavUp => {
                if self.can_go_up() {
                    if let Some(parent) = self.browse_dir.as_ref().and_then(|d| d.parent()) {
                        self.browse_dir = Some(parent.to_path_buf());
                        self.load_dir(&sender);
                    }
                }
            }
            Msg::FilesGoStart => {
                if let Some(root) = self.root_dir.clone() {
                    if self.browse_dir.as_ref() != Some(&root) {
                        self.browse_dir = Some(root);
                        self.load_dir(&sender);
                    }
                }
            }
            Msg::Refresh => {
                self.load_dir(&sender);
                // „Neu einlesen" aktualisiert auch die Bibliothek (Interpreten/Alben).
                self.start_scan(&sender, false);
            }
            Msg::ToggleEnrich => {
                if self.enriching {
                    self.enrich_cancel.store(true, Ordering::Relaxed);
                    self.enrich_status = "Wird abgebrochen …".to_string();
                } else {
                    self.run_enrich(&sender, true);
                }
            }
            Msg::HideEnrichBanner => self.enrich_banner_hidden = true,
            Msg::OpenSettings => self.open_settings(root, &sender),
            Msg::OpenGlobalEq => self.open_global_eq(root, &sender),
            Msg::SetMusicDir(path) => {
                let dir = path.to_string_lossy().into_owned();
                if let Err(e) = self.library.set_setting("music_dir", &dir) {
                    tracing::error!("Konnte Musikordner nicht speichern: {e}");
                }
                self.music_dir = Some(dir);
                self.root_dir = Some(path.clone());
                self.browse_dir = Some(path);
                self.load_dir(&sender);
                // Neuen Ordner einlesen und (WLAN + Schalter) automatisch nachladen.
                self.start_scan(&sender, true);
            }
            Msg::SetAcoustidKey(key) => {
                let key = key.trim().to_string();
                let _ = self.library.set_setting("acoustid_key", &key);
                self.acoustid_key = if key.is_empty() { None } else { Some(key) };
            }
            Msg::SetAlbumCover { artist, album, path } => {
                let mut meta = self
                    .library
                    .get_album_meta(&artist, &album)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| crate::model::AlbumMeta::pending(&artist, &album));
                // Nur bei tatsächlicher Änderung speichern + Ansichten auffrischen.
                if meta.cover_path.as_deref() != Some(path.as_str()) {
                    meta.cover_path = Some(path);
                    let _ = self.library.upsert_album_meta(&meta);
                    self.reload_albums();
                }
            }
            Msg::SetArtistImage { name, path } => {
                let mut meta = self
                    .library
                    .get_artist_meta(&name)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| crate::model::ArtistMeta::pending(&name));
                if meta.image_path.as_deref() != Some(path.as_str()) {
                    meta.image_path = Some(path);
                    let _ = self.library.upsert_artist_meta(&meta);
                    self.reload_artists();
                }
            }
            Msg::SetFanartKey(key) => {
                let key = key.trim().to_string();
                let _ = self.library.set_setting("fanart_key", &key);
                self.fanart_key = if key.is_empty() { None } else { Some(key) };
            }
            Msg::SetAutoEnrich(on) => {
                self.auto_enrich = on;
                let _ = self
                    .library
                    .set_setting("auto_enrich", if on { "1" } else { "0" });
            }
            Msg::SetCategory { scope, key, value } => {
                if let Err(e) = self.library.set_category(scope, &key, value) {
                    tracing::error!("Merkmal konnte nicht gespeichert werden: {e}");
                }
            }
            Msg::SetEq {
                output,
                scope,
                key,
                bands,
            } => {
                let _ = self.library.set_eq(&output, scope, &key, &bands);
                // Effektiven EQ des aktiven Ausgangs neu auflösen und anwenden
                // (hörbar, sofern die bearbeitete Ebene aktuell greift).
                self.apply_current_eq();
            }
            Msg::ClearEq { output, scope, key } => {
                let _ = self.library.clear_eq(&output, scope, &key);
                self.apply_current_eq();
            }
            Msg::ConcertImport => {
                let Some(root) = self.root_dir.clone() else {
                    self.toast("Kein Musikordner festgelegt");
                    return;
                };
                let existing = self.library.concert_paths().unwrap_or_default();
                self.toast("Suche nach Konzerten läuft …");
                sender.spawn_oneshot_command(move || {
                    Cmd::Candidates(crate::core::concert::scan_candidates(&root, &existing))
                });
            }
            Msg::ConcertDismissHint => {
                self.concert_hint_dismissed = true;
                let _ = self.library.set_setting("concert_hint_dismissed", "1");
            }
            Msg::ConcertHideSection => {
                self.concerts_hidden = true;
                let _ = self.library.set_setting("concerts_hidden", "1");
                for btn in &self.concert_nav_buttons {
                    btn.set_visible(false);
                }
                // Auf den vorherigen Menüpunkt wechseln (Konzerte ist nun weg).
                let prev = SECTIONS
                    .iter()
                    .position(|(n, _, _)| *n == "concerts")
                    .and_then(|i| i.checked_sub(1))
                    .map(|i| SECTIONS[i].0)
                    .unwrap_or("files");
                self.view_stack.set_visible_child_name(prev);
                self.toast("Menüpunkt Konzerte ausgeblendet");
            }
            Msg::ConcertAdd(items) => {
                let n = items.len();
                for (path, title, is_dir) in &items {
                    let _ = self.library.add_concert(path, title, *is_dir);
                }
                self.load_concerts(&sender);
                self.toast(&format!("{n} Konzert(e) hinzugefügt"));
            }
            Msg::PlayConcert(index) => {
                if let Some((path, _, is_dir)) = self.concert_items.get(index).cloned() {
                    self.play_path(&path, is_dir);
                }
            }
            Msg::ConcertRemove(index) => {
                if let Some((path, _, _)) = self.concert_items.get(index).cloned() {
                    let _ = self.library.remove_concert(&path);
                    self.load_concerts(&sender);
                    self.toast("Konzert entfernt");
                }
            }
            Msg::SetConcertsVisible(visible) => {
                self.concerts_hidden = !visible;
                let _ = self
                    .library
                    .set_setting("concerts_hidden", if visible { "0" } else { "1" });
                for btn in &self.concert_nav_buttons {
                    btn.set_visible(visible);
                }
            }
            Msg::TogglePlay => {
                if self.now_playing.is_none() {
                    return;
                }
                if self.playing {
                    self.save_resume();
                    self.player.pause();
                } else {
                    self.player.resume();
                }
                self.playing = !self.playing;
                // Play-/Pause-Icon des aktiven Titels in der Liste anpassen.
                self.refresh_queue_icons();
            }
            Msg::OpenNowPlaying => {
                // Detailansicht des laufenden Titels (als Datei-Eintrag).
                if let Some(path) = self.queue.get(self.queue_pos).cloned() {
                    self.context_target = Some(CtxTarget::Fs(FsEntry::file(path)));
                    self.open_context_menu(root, &sender);
                }
            }
        }
    }

    /// Ergebnisse der Hintergrund-Worker verarbeiten.
    fn update_cmd(
        &mut self,
        msg: Self::CommandOutput,
        sender: ComponentSender<Self>,
        root: &Self::Root,
    ) {
        match msg {
            Cmd::Entries(entries) => {
                // „Mixed Album": mehr als ein unterschiedlicher Interpret im Ordner.
                let distinct: std::collections::HashSet<String> =
                    entries.iter().filter_map(|e| e.effective_artist()).collect();
                let opts = RowOpts {
                    show_artist: distinct.len() > 1,
                };
                let queue = self.queue.clone();
                let mut guard = self.entries.guard();
                guard.clear();
                for e in entries {
                    let queued = !e.is_dir() && queue.iter().any(|p| p == e.path());
                    guard.push_back((e, opts, queued));
                }
                drop(guard);
                self.loading = false;

                // Dieser Ordner wird jetzt angezeigt; gemerkte Scrollposition (vom
                // letzten Besuch) nach dem Layout wiederherstellen.
                self.shown_dir = self.browse_dir.clone();
                if let (Some(dir), Some(sc)) = (self.browse_dir.clone(), self.fs_scroller()) {
                    if let Some(&value) = self.fs_scroll.borrow().get(&dir) {
                        for delay in [50u64, 250] {
                            let sc = sc.clone();
                            gtk::glib::timeout_add_local_once(
                                std::time::Duration::from_millis(delay),
                                move || sc.vadjustment().set_value(value),
                            );
                        }
                    }
                }
            }
            Cmd::EnrichProgress { phase, done, total } => {
                self.enrich_status = format!("{phase}: {done}/{total}");
            }
            Cmd::EnrichDone {
                albums,
                artists,
                tracks,
            } => {
                self.enriching = false;
                self.reload_albums();
                self.reload_artists();
                self.toast(&format!(
                    "{albums} Cover, {artists} Interpreten-Fotos, {tracks} Titel zugeordnet"
                ));
            }
            Cmd::ReloadViews => {
                self.reload_albums();
                self.reload_artists();
            }
            Cmd::ScanDone { then_enrich } => {
                // Bibliothek ist eingelesen → Ansichten aktualisieren.
                self.reload_albums();
                self.reload_artists();
                // Danach automatisch online nachladen – aber nur, wenn gewünscht,
                // nicht schon ein Abruf läuft und die Verbindung nicht getaktet ist
                // (also WLAN/LAN, keine mobilen Daten). Der lokale Scan lief schon,
                // daher hier ohne erneutes Einlesen.
                if then_enrich
                    && self.auto_enrich
                    && !self.enriching
                    && self.root_dir.is_some()
                    && online_unmetered()
                {
                    self.run_enrich(&sender, false);
                }
            }
            Cmd::Candidates(candidates) => {
                if candidates.is_empty() {
                    self.toast("Keine neuen Konzert-Kandidaten gefunden");
                } else {
                    self.open_concert_import_dialog(root, &sender, candidates);
                }
            }
        }
    }
}

/// Speichert Fenstergröße/Maximierung und den zuletzt offenen Navigationspunkt
/// (eigene kurzlebige DB-Verbindung, da im Close-Handler aufgerufen).
fn save_window_state(width: i32, height: i32, maximized: bool, section: Option<&str>) {
    if let Ok(lib) = Library::open() {
        let _ = lib.set_setting("win_width", &width.to_string());
        let _ = lib.set_setting("win_height", &height.to_string());
        let _ = lib.set_setting("win_maximized", if maximized { "1" } else { "0" });
        if let Some(sec) = section {
            let _ = lib.set_setting("active_section", sec);
        }
    }
}

/// Formatiert Millisekunden als `m:ss` bzw. `h:mm:ss`.
pub(crate) fn fmt_duration(ms: i64) -> String {
    let secs = ms / 1000;
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Ob ein automatischer Online-Abruf erlaubt ist: Verbindung vorhanden **und**
/// nicht getaktet (also WLAN/LAN, keine mobilen Daten). Grundlage über
/// `gio::NetworkMonitor` (nutzt NetworkManager) – ohne zusätzliche Abhängigkeit.
fn online_unmetered() -> bool {
    use gtk::gio::prelude::NetworkMonitorExt;
    let nm = gtk::gio::NetworkMonitor::default();
    nm.is_network_available() && !nm.is_network_metered()
}

/// Häufigste Interpreten-Angabe (rohe Tag-Zeichenkette) einer Titelmenge – dient
/// als Anzeige-/Schlüsselinterpret eines Albums (für Cover & Album-Metadaten).
fn most_common_artist(tracks: &[Track]) -> String {
    let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for t in tracks {
        if let Some(a) = t.artist.as_deref() {
            *counts.entry(a).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .max_by_key(|(_, n)| *n)
        .map(|(a, _)| a.to_string())
        .unwrap_or_default()
}

/// Untertitel einer Album-Zeile: „Jahr · N Lieder" (Jahr nur, wenn bekannt).
fn album_subtitle(year: Option<i32>, track_count: usize) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(y) = year {
        parts.push(y.to_string());
    }
    parts.push(format!(
        "{track_count} {}",
        if track_count == 1 { "Lied" } else { "Lieder" }
    ));
    parts.join(" · ")
}

/// Rechtsbündige, dezente Dauer-Beschriftung für eine Titel-Zeile.
fn duration_label(ms: i64) -> gtk::Label {
    gtk::Label::builder()
        .label(fmt_duration(ms))
        .css_classes(["dim-label", "numeric"])
        .build()
}

/// Quadratische 48-px-Cover-Vorschau aus einem Dateipfad – **synchron** dekodiert
/// und gecacht; fehlt das Bild, zeigt der Rahmen das Platzhalter-Icon. Gedacht für
/// die bedarfsweise geöffneten, kurzen Unterseiten-Listen.
/// Erster `ScrolledWindow` im Widget-Teilbaum (Tiefensuche), z. B. um die
/// Scrollposition der gerade sichtbaren Übersichts-Sektion zu finden.
fn find_scroller(widget: &gtk::Widget) -> Option<gtk::ScrolledWindow> {
    // Unsichtbare Teilbäume überspringen – sonst greift man z. B. den internen,
    // versteckten Scroller einer leeren `adw::StatusPage` statt der echten Liste.
    if !widget.is_visible() {
        return None;
    }
    if let Some(sc) = widget.downcast_ref::<gtk::ScrolledWindow>() {
        return Some(sc.clone());
    }
    let mut child = widget.first_child();
    while let Some(c) = child {
        if let Some(sc) = find_scroller(&c) {
            return Some(sc);
        }
        child = c.next_sibling();
    }
    None
}

fn cover_widget(path: Option<&str>, placeholder: &str) -> gtk::Widget {
    let texture = path.and_then(crate::ui::widgets::thumb_cached);
    crate::ui::widgets::rounded_image(texture.as_ref(), placeholder, 48)
}

/// Liest Unterordner und Audiodateien eines Ordners (Ordner zuerst, sortiert).
/// Läuft im Hintergrund-Thread – darf daher blockieren.
fn read_entries(dir: PathBuf) -> Vec<FsEntry> {
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else if scanner::is_audio(&path) {
                files.push(path);
            }
        }
    }
    dirs.sort();
    files.sort();

    let mut out = Vec::with_capacity(dirs.len() + files.len());
    out.extend(dirs.into_iter().map(FsEntry::dir));
    out.extend(files.into_iter().map(FsEntry::file));
    out
}

/// Hintergrund-Worker für die Online-Anreicherung. Öffnet eine eigene
/// DB-Verbindung, liest die Tags unter `root` ein (read-only auf den Dateien!)
/// und reichert in drei Phasen an:
///   1. Alben  → MusicBrainz + Cover Art Archive (Cover)
///   2. Interpreten → Deezer (Fotos)
///   3. Titel  → Chromaprint/AcoustID (nur Dateien mit lückenhaften Tags;
///      benötigt einen AcoustID-Key, sonst wird die Phase übersprungen)
fn enrich_worker(
    root: PathBuf,
    acoustid_key: Option<String>,
    fanart_key: Option<String>,
    cancel: Arc<AtomicBool>,
    scan_first: bool,
    out: &relm4::Sender<Cmd>,
) {
    let lib = match Library::open() {
        Ok(lib) => lib,
        Err(e) => {
            tracing::error!("DB für Online-Abruf nicht erreichbar: {e}");
            let _ = out.send(Cmd::EnrichDone {
                albums: 0,
                artists: 0,
                tracks: 0,
            });
            return;
        }
    };

    // Tags in die Bibliothek einlesen (verändert die Dateien nicht). Beim
    // automatischen Lauf entfällt das – der lokale Scan lief bereits.
    if scan_first {
        if let Err(e) = scanner::scan_into(&lib, &root) {
            tracing::warn!("Scan vor Online-Abruf fehlgeschlagen: {e}");
        }
    }

    let client = online::OnlineClient::new();
    let mut covers = 0usize;
    let mut artists_matched = 0usize;
    let mut tracks_matched = 0usize;
    let stopped = || cancel.load(Ordering::Relaxed);

    // Gesamtsummen für die Fortschrittsanzeige (gegen die ganze Bibliothek,
    // nicht nur gegen die Restmenge der jeweiligen Phase).
    let total_albums = lib.album_count().unwrap_or(0).max(0) as usize;

    'work: {
        // Phase 1: Cover aus den Dateien (eingebettet/Ordnerbild) – schnell, offline.
        let missing = lib.albums_missing_cover().unwrap_or_default();
        // Bereits mit Cover versehene Alben gelten als „erledigt" → der Zähler
        // startet dort und läuft bis zur Gesamtzahl (z. B. 4717/4726 … 4726/4726).
        let base = total_albums.saturating_sub(missing.len());
        for (i, (artist, album, path)) in missing.iter().enumerate() {
            if stopped() {
                break 'work;
            }
            if let Some(cover_path) = online::local_album_cover(artist, album, path) {
                let mut m = crate::model::AlbumMeta::pending(artist, album);
                m.cover_path = Some(cover_path);
                m.status = "local".to_string();
                let _ = lib.upsert_album_meta(&m);
                covers += 1;
            }
            let _ = out.send(Cmd::EnrichProgress {
                phase: "Cover".to_string(),
                done: base + i + 1,
                total: total_albums,
            });
        }
        let _ = out.send(Cmd::ReloadViews);

        // Phase 2: Interpreten-Fotos (Deezer) – parallel, kleine Bilder --------
        // Bereits zugeordnete überspringen, nur den Rest parallel laden.
        let all_artists = lib.distinct_artists().unwrap_or_default();
        let total_artists = all_artists.len();
        let mut to_fetch = Vec::new();
        for name in all_artists {
            match lib.get_artist_meta(&name).ok().flatten() {
                Some(m) if m.status == "matched" => artists_matched += 1,
                _ => to_fetch.push(name),
            }
        }
        if stopped() {
            break 'work;
        }
        // Bereits zugeordnete Interpreten zählen als „erledigt" (Fortschritts-Basis).
        let artists_base = artists_matched;
        artists_matched +=
            fetch_artists_parallel(&client, to_fetch, &cancel, &lib, artists_base, total_artists, out);
        let _ = out.send(Cmd::ReloadViews);
        if stopped() {
            break 'work;
        }

        // Phase 3: Online-Cover nur noch für Alben ganz ohne Bild (Lückenfüller).
        let still_missing = lib.albums_missing_cover().unwrap_or_default();
        let base = total_albums.saturating_sub(still_missing.len());
        for (i, (artist, album, _)) in still_missing.iter().enumerate() {
            if stopped() {
                break 'work;
            }
            if !artist.is_empty()
                && online::enrich_album(&client, &lib, artist, album).status == "matched"
            {
                covers += 1;
            }
            std::thread::sleep(online::RATE_LIMIT);
            let _ = out.send(Cmd::EnrichProgress {
                phase: "Cover".to_string(),
                done: base + i + 1,
                total: total_albums,
            });
        }
        let _ = out.send(Cmd::ReloadViews);

        // Phase 4: Titel-Erkennung per Fingerprint -----------------------------
        if let Some(key) = acoustid_key.filter(|k| !k.is_empty()) {
            if online::fingerprint_available() {
                let candidates = lib.tracks_needing_id().unwrap_or_default();
                // Gesamtsumme = alle Titel; bereits vollständige zählen als erledigt.
                let total_tracks = lib.track_count().unwrap_or(0).max(0) as usize;
                let base = total_tracks.saturating_sub(candidates.len());
                for (i, track) in candidates.iter().enumerate() {
                    if stopped() {
                        break 'work;
                    }
                    let path = PathBuf::from(&track.path);
                    let already = lib.get_track_meta(&track.path).ok().flatten();
                    if already.as_ref().map(|m| m.status.as_str()) == Some("matched") {
                        tracks_matched += 1;
                    } else {
                        if online::enrich_track_fingerprint(&client, &lib, &key, &path).status
                            == "matched"
                        {
                            tracks_matched += 1;
                        }
                        std::thread::sleep(online::ACOUSTID_DELAY);
                    }
                    let _ = out.send(Cmd::EnrichProgress {
                        phase: "Titel".to_string(),
                        done: base + i + 1,
                        total: total_tracks,
                    });
                }
            } else {
                tracing::info!("Fingerprint-Phase übersprungen (fpcalc fehlt)");
            }
        }

        // Phase 5: Album-Galerien (mehrere Cover je Album, Cover Art Archive) –
        // parallel; der reine Bildabruf unterliegt keinem MusicBrainz-1/s-Limit.
        let gallery_albums: Vec<(String, String, String)> = lib
            .albums_overview()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|m| m.mbid.map(|id| (m.artist, m.album, id)))
            .collect();
        if stopped() {
            break 'work;
        }
        fetch_album_galleries_parallel(&client, gallery_albums, &cancel, &lib, out);
        let _ = out.send(Cmd::ReloadViews);

        // Phase 6: Interpreten-Galerien (mehrere Fotos via fanart.tv – nur mit Key).
        if let Some(fkey) = fanart_key.filter(|k| !k.is_empty()) {
            let names = lib.distinct_artists().unwrap_or_default();
            let fa_total = names.len();
            for (i, name) in names.iter().enumerate() {
                if stopped() {
                    break 'work;
                }
                online::enrich_artist_gallery(&client, &lib, name, &fkey);
                std::thread::sleep(online::RATE_LIMIT);
                let _ = out.send(Cmd::EnrichProgress {
                    phase: "Interpreten-Bilder".to_string(),
                    done: i + 1,
                    total: fa_total,
                });
            }
            let _ = out.send(Cmd::ReloadViews);
        }
    }

    let _ = out.send(Cmd::EnrichDone {
        albums: covers,
        artists: artists_matched,
        tracks: tracks_matched,
    });
}

/// Lädt Künstlerfotos **parallel** (mehrere Netz-Threads), schreibt die
/// Ergebnisse aber serialisiert über die eine DB-Verbindung des Koordinators.
/// Gibt die Anzahl neu zugeordneter Interpreten zurück.
fn fetch_artists_parallel(
    client: &online::OnlineClient,
    names: Vec<String>,
    cancel: &Arc<AtomicBool>,
    lib: &Library,
    // Fortschritt gegen die Gesamtzahl: `done_base` = schon erledigte Interpreten,
    // `grand_total` = alle Interpreten der Bibliothek.
    done_base: usize,
    grand_total: usize,
    out: &relm4::Sender<Cmd>,
) -> usize {
    use std::collections::VecDeque;
    use std::sync::mpsc;
    use std::sync::Mutex;

    let total = names.len();
    if total == 0 {
        return 0;
    }

    let jobs = Arc::new(Mutex::new(VecDeque::from(names)));
    let (tx, rx) = mpsc::channel::<(String, Option<Vec<u8>>, bool)>();
    let n_threads = total.min(online::ARTIST_FETCH_THREADS);

    let mut handles = Vec::with_capacity(n_threads);
    for _ in 0..n_threads {
        let client = client.clone();
        let jobs = jobs.clone();
        let cancel = cancel.clone();
        let tx = tx.clone();
        handles.push(std::thread::spawn(move || loop {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let Some(name) = jobs.lock().unwrap().pop_front() else {
                break;
            };
            let (image, errored) = match client.fetch_artist_image(&name) {
                Ok(img) => (img, false),
                Err(_) => (None, true),
            };
            if tx.send((name, image, errored)).is_err() {
                break;
            }
        }));
    }
    drop(tx); // nur die Thread-Klone halten den Sender → rx endet, wenn alle fertig

    // Koordinator: Ergebnisse serialisiert in die DB schreiben + Fortschritt.
    let mut matched = 0usize;
    let mut done = 0usize;
    while let Ok((name, image, errored)) = rx.recv() {
        let meta = online::store_artist_image(&name, image, errored);
        if meta.status == "matched" {
            matched += 1;
        }
        let _ = lib.upsert_artist_meta(&meta);
        done += 1;
        let _ = out.send(Cmd::EnrichProgress {
            phase: "Interpreten".to_string(),
            done: done_base + done,
            total: grand_total,
        });
        if done % 16 == 0 {
            let _ = out.send(Cmd::ReloadViews);
        }
    }

    for h in handles {
        let _ = h.join();
    }
    matched
}

/// Lädt **mehrere** Album-Galerien parallel aus dem Cover Art Archive. Anders als
/// die MusicBrainz-Suche unterliegt der reine Bildabruf (die MBID ist bekannt)
/// keinem 1/s-Limit; nur die DB-Schreibzugriffe werden serialisiert (Koordinator).
fn fetch_album_galleries_parallel(
    client: &online::OnlineClient,
    albums: Vec<(String, String, String)>,
    cancel: &Arc<AtomicBool>,
    lib: &Library,
    out: &relm4::Sender<Cmd>,
) {
    use std::collections::VecDeque;
    use std::sync::mpsc;
    use std::sync::Mutex;

    let total = albums.len();
    if total == 0 {
        return;
    }

    let jobs = Arc::new(Mutex::new(VecDeque::from(albums)));
    let (tx, rx) = mpsc::channel::<(String, String, Vec<(Vec<u8>, String)>)>();
    let n_threads = total.min(online::GALLERY_FETCH_THREADS);

    let mut handles = Vec::with_capacity(n_threads);
    for _ in 0..n_threads {
        let client = client.clone();
        let jobs = jobs.clone();
        let cancel = cancel.clone();
        let tx = tx.clone();
        handles.push(std::thread::spawn(move || loop {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let Some((artist, album, mbid)) = jobs.lock().unwrap().pop_front() else {
                break;
            };
            let imgs = client.fetch_album_gallery(&mbid).unwrap_or_default();
            if tx.send((artist, album, imgs)).is_err() {
                break;
            }
        }));
    }
    drop(tx); // nur die Thread-Klone halten den Sender → rx endet, wenn alle fertig

    // Koordinator: Ergebnisse serialisiert in den Cache + die DB schreiben.
    let mut done = 0usize;
    while let Ok((artist, album, imgs)) = rx.recv() {
        online::store_album_gallery(lib, &artist, &album, &imgs);
        done += 1;
        let _ = out.send(Cmd::EnrichProgress {
            phase: "Album-Bilder".to_string(),
            done,
            total,
        });
        if done % 16 == 0 {
            let _ = out.send(Cmd::ReloadViews);
        }
    }

    for h in handles {
        let _ = h.join();
    }
}

impl App {
    /// Scroller der Dateiliste (Vorfahre der Einträge-`ListBox`).
    fn fs_scroller(&self) -> Option<gtk::ScrolledWindow> {
        self.entries
            .widget()
            .ancestor(gtk::ScrolledWindow::static_type())
            .and_downcast::<gtk::ScrolledWindow>()
    }

    /// Startet das Einlesen des aktuellen Ordners im Hintergrund (mit Spinner).
    fn load_dir(&mut self, sender: &ComponentSender<Self>) {
        // Scrollposition des gerade gezeigten Ordners merken, bevor er ersetzt wird.
        if let (Some(dir), Some(sc)) = (self.shown_dir.clone(), self.fs_scroller()) {
            self.fs_scroll
                .borrow_mut()
                .insert(dir, sc.vadjustment().value());
        }
        match self.browse_dir.clone() {
            Some(dir) => {
                // Aktuellen Ordner merken (für „weitermachen, wo man war").
                let _ = self.library.set_setting("browse_dir", &dir.to_string_lossy());
                self.loading = true;
                sender.spawn_oneshot_command(move || Cmd::Entries(read_entries(dir)));
            }
            None => {
                self.entries.guard().clear();
                self.loading = false;
            }
        }
    }

    /// Lädt die Album-Übersicht aus der DB in die Factory (inkl. Online-Cover).
    fn reload_albums(&mut self) {
        let albums = self.library.albums_overview().unwrap_or_default();
        self.album_count = albums.len();
        let mut guard = self.albums.guard();
        guard.clear();
        for a in albums {
            guard.push_back(a);
        }
    }

    /// Liest die Bibliothek (Tags → DB) **im Hintergrund** ein – rein lokal, ohne
    /// Netz. `then_enrich`: danach ggf. automatisch online nachladen (entscheidet
    /// der `ScanDone`-Handler anhand Schalter + Verbindung).
    fn start_scan(&self, sender: &ComponentSender<Self>, then_enrich: bool) {
        let Some(root) = self.root_dir.clone() else {
            return;
        };
        sender.spawn_oneshot_command(move || {
            match Library::open() {
                Ok(lib) => {
                    if let Err(e) = scanner::scan_into(&lib, &root) {
                        tracing::warn!("Bibliotheks-Scan fehlgeschlagen: {e}");
                    }
                }
                Err(e) => tracing::error!("DB für Scan nicht erreichbar: {e}"),
            }
            Cmd::ScanDone { then_enrich }
        });
    }

    /// Startet die Online-Anreicherung im Hintergrund. `scan_first`: zuvor noch die
    /// Tags einlesen (beim manuellen Abruf) – beim automatischen Lauf entfällt das,
    /// weil der lokale Scan bereits durchlief. Die Audiodateien werden dabei nur
    /// gelesen, niemals verändert.
    fn run_enrich(&mut self, sender: &ComponentSender<Self>, scan_first: bool) {
        let Some(root) = self.root_dir.clone() else {
            self.toast("Kein Musikordner festgelegt – bitte in den Einstellungen wählen");
            return;
        };
        if self.enriching {
            return;
        }
        // Fehlender AcoustID-Key/fpcalc: Titel-Erkennung wird still übersprungen.
        let key = self.acoustid_key.clone();
        let fkey = self.fanart_key.clone();
        self.enrich_cancel.store(false, Ordering::Relaxed);
        let cancel = self.enrich_cancel.clone();
        self.enriching = true;
        // Neuer Lauf → Fortschritts-Leiste wieder einblenden.
        self.enrich_banner_hidden = false;
        self.enrich_status = if scan_first {
            "Bibliothek wird eingelesen …".to_string()
        } else {
            "Cover & Metadaten werden gesucht …".to_string()
        };
        sender.spawn_command(move |out| enrich_worker(root, key, fkey, cancel, scan_first, &out));
    }

    /// Lädt die Interpreten-Übersicht aus der DB in die Factory (inkl. Foto).
    fn reload_artists(&mut self) {
        let artists = self.library.artists_overview().unwrap_or_default();
        self.artist_count = artists.len();
        let mut guard = self.artists.guard();
        guard.clear();
        for a in artists {
            guard.push_back(a);
        }
    }

    /// Liefert die abspielbaren Dateien eines Eintrags: bei Ordnern rekursiv,
    /// bei Dateien nur die eine.
    fn entry_files(&self, entry: &FsEntry) -> Vec<PathBuf> {
        if entry.is_dir() {
            scanner::collect_audio_files(entry.path())
        } else {
            vec![entry.path().clone()]
        }
    }

    /// Alle Dateien eines Interpreten (aus der Bibliothek), in Abspielreihenfolge.
    fn artist_files(&self, name: &str) -> Vec<PathBuf> {
        // Wie die Interpreten-Liste (artist_sections/artist_albums): ein Titel
        // zählt zum Interpreten, wenn dessen Name in der – ggf. aus „feat."
        // zerlegten – Interpreten-Angabe vorkommt (case-insensitiv). Sonst zählte
        // die Detailseite Gast-/zusammengesetzte Titel nicht mit und zeigte „0
        // Lieder", obwohl die Liederliste sie führt.
        let target = name.to_lowercase();
        self.library
            .all_tracks()
            .unwrap_or_default()
            .into_iter()
            .filter(|t| {
                t.artist.as_deref().is_some_and(|a| {
                    crate::core::artist::split_artists(a)
                        .iter()
                        .any(|s| s.to_lowercase() == target)
                })
            })
            .map(|t| PathBuf::from(t.path))
            .collect()
    }

    /// Alle Dateien eines Albums (Interpret + Album), in Abspielreihenfolge.
    fn album_files(&self, artist: &str, album: &str) -> Vec<PathBuf> {
        self.library
            .all_tracks()
            .unwrap_or_default()
            .into_iter()
            .filter(|t| {
                t.album.as_deref() == Some(album) && t.artist.as_deref() == Some(artist)
            })
            .map(|t| PathBuf::from(t.path))
            .collect()
    }

    /// Alle Titel eines (ggf. aus „feat." zerlegten) Interpreten, nach Album
    /// gruppiert. Ein Titel zählt zum Interpreten, wenn dessen Name in der
    /// zerlegten Interpreten-Angabe des Titels vorkommt (case-insensitiv) –
    /// passend zur Interpreten-Liste, die ebenfalls „feat."-Angaben aufteilt.
    /// Alben in der Reihenfolge aus `all_tracks` (alphabetisch), Titel je Album
    /// nach Tracknummer.
    fn artist_albums(&self, name: &str) -> Vec<(String, Vec<Track>)> {
        let target = name.to_lowercase();
        let mut order: Vec<String> = Vec::new();
        let mut groups: std::collections::HashMap<String, Vec<Track>> =
            std::collections::HashMap::new();
        for t in self.library.all_tracks().unwrap_or_default() {
            let belongs = t.artist.as_deref().is_some_and(|a| {
                crate::core::artist::split_artists(a)
                    .iter()
                    .any(|s| s.to_lowercase() == target)
            });
            if !belongs {
                continue;
            }
            let album = t.album.clone().unwrap_or_default();
            if !groups.contains_key(&album) {
                order.push(album.clone());
            }
            groups.entry(album).or_default().push(t);
        }
        order
            .into_iter()
            .map(|album| {
                let tracks = groups.remove(&album).unwrap_or_default();
                (album, tracks)
            })
            .collect()
    }

    /// Teilt die Titel eines Interpreten in **eigene Alben** und **Einzellieder**:
    ///
    /// * Gehören dem Interpreten **alle** Titel eines Albums (laut Bibliothek), ist
    ///   es sein Album → eigener Album-Eintrag `(Album, Anzeige-Interpret, Titel)`.
    /// * Ist er nur auf **einem Teil** des Albums vertreten (z. B. als Gast auf
    ///   2–3 Stücken), zählen diese Titel zu den Einzelliedern.
    /// * Titel ganz ohne Album sind ebenfalls Einzellieder.
    ///
    /// Alben in der Reihenfolge aus `all_tracks`; Titel je Album nach Tracknummer.
    fn artist_sections(&self, name: &str) -> (Vec<(String, String, Vec<Track>)>, Vec<Track>) {
        let target = name.to_lowercase();
        let all = self.library.all_tracks().unwrap_or_default();

        // Titel des Interpreten nach Albumname gruppieren (Reihenfolge bewahren).
        let mut order: Vec<String> = Vec::new();
        let mut groups: std::collections::HashMap<String, Vec<Track>> =
            std::collections::HashMap::new();
        for t in all {
            let belongs = t.artist.as_deref().is_some_and(|a| {
                crate::core::artist::split_artists(a)
                    .iter()
                    .any(|s| s.to_lowercase() == target)
            });
            if !belongs {
                continue;
            }
            let album = t.album.clone().unwrap_or_default();
            if !groups.contains_key(&album) {
                order.push(album.clone());
            }
            groups.entry(album).or_default().push(t);
        }

        let mut albums: Vec<(String, String, Vec<Track>)> = Vec::new();
        let mut singles: Vec<Track> = Vec::new();
        for album in order {
            let mine = groups.remove(&album).unwrap_or_default();
            if album.is_empty() {
                singles.extend(mine);
                continue;
            }
            // „Eigenes Album": der Interpret ist auf der Mehrheit der ihm
            // zugeordneten Titel der erstgenannte (Haupt-)Interpret. Ist er
            // überall nur Gast (… feat. <name>), zählen die Titel als Einzellieder.
            let own = mine
                .iter()
                .filter(|t| {
                    t.artist.as_deref().is_some_and(|a| {
                        crate::core::artist::split_artists(a)
                            .first()
                            .is_some_and(|p| p.to_lowercase() == target)
                    })
                })
                .count();
            // Nur als Album zeigen, wenn mindestens zwei Titel vorhanden sind –
            // ein einzelnes Lied (z. B. nur ein Stück eines Albums in der
            // Bibliothek) zählt als Einzellied, nicht als Album.
            if mine.len() >= 2 && own > 0 && own * 2 >= mine.len() {
                let display_artist = most_common_artist(&mine);
                albums.push((album, display_artist, mine));
            } else {
                singles.extend(mine);
            }
        }
        (albums, singles)
    }

    /// Titel, die zu „diesem Album dieses Interpreten" gehören: alle Bibliotheks-
    /// titel mit dem Albumnamen, in deren (zerlegter) Interpreten-Angabe `name`
    /// vorkommt. Bereits nach Tracknummer sortiert (Reihenfolge aus `all_tracks`).
    fn album_tracks_for_artist(&self, name: &str, album: &str) -> Vec<Track> {
        let target = name.to_lowercase();
        self.library
            .all_tracks()
            .unwrap_or_default()
            .into_iter()
            .filter(|t| {
                t.album.as_deref() == Some(album)
                    && t.artist.as_deref().is_some_and(|a| {
                        crate::core::artist::split_artists(a)
                            .iter()
                            .any(|s| s.to_lowercase() == target)
                    })
            })
            .collect()
    }

    /// Hüllt einen Inhalt in eine scrollbare Unterseite (mit Kopfleiste +
    /// Zurück-Pfeil) und schiebt sie auf den Navigations-Stapel.
    fn push_subpage(&self, title: &str, content: &gtk::Box) {
        // Verlassen wir die Wurzel-Übersicht, die aktuelle Scrollposition der
        // sichtbaren Sektion merken (wird beim Zurückkehren wiederhergestellt).
        let leaving_root = self
            .nav_view
            .visible_page()
            .and_then(|p| p.tag())
            .is_some_and(|t| t == "main");
        if leaving_root {
            if let Some(sc) = self
                .view_stack
                .visible_child()
                .and_then(|c| find_scroller(&c))
            {
                let value = sc.vadjustment().value();
                *self.overview_scroll.borrow_mut() = Some((sc, value));
            }
        }

        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .vexpand(true)
            .child(content)
            .build();
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&scroller));
        let page = adw::NavigationPage::builder()
            .title(title)
            .child(&toolbar)
            .build();
        self.nav_view.push(&page);
    }

    /// Kurzes Tippen auf einen Interpreten: öffnet eine Unterseite, die zuerst
    /// dessen **Alben** (mit Cover) und danach die **Einzellieder** (Titel ohne
    /// Album, mit Cover) auflistet. Tippen auf ein Album öffnet dessen Titel als
    /// weitere Unterseite; Tippen auf ein Einzellied spielt es ab.
    fn open_artist_tracks(&self, sender: &ComponentSender<Self>, meta: &ArtistMeta) {
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();

        // Eigene Alben vom Rest (Gast-Titel + Titel ohne Album) trennen.
        let (album_groups, singles) = self.artist_sections(&meta.name);

        if album_groups.is_empty() && singles.is_empty() {
            content.append(
                &adw::StatusPage::builder()
                    .icon_name("avatar-default-symbolic")
                    .title("Keine Titel")
                    .description("Für diesen Interpreten sind keine Lieder in der Bibliothek.")
                    .build(),
            );
        }

        // --- Alben zuerst ---
        if !album_groups.is_empty() {
            let n = album_groups.len();
            let group = adw::PreferencesGroup::builder()
                .title("Alben")
                .description(format!("{n} {}", if n == 1 { "Album" } else { "Alben" }))
                .build();
            for (album, display_artist, tracks) in &album_groups {
                let album_meta = self
                    .library
                    .get_album_meta(display_artist, album)
                    .ok()
                    .flatten();
                let year = album_meta.as_ref().and_then(|m| m.year);
                let cover_path = album_meta.as_ref().and_then(|m| m.cover_path.clone());

                let row = adw::ActionRow::builder()
                    .title(gtk::glib::markup_escape_text(album))
                    .subtitle(album_subtitle(year, tracks.len()))
                    .activatable(true)
                    .build();
                row.add_css_class("emilia-flush");
                row.add_prefix(&cover_widget(cover_path.as_deref(), "media-optical-symbolic"));

                // Gesamtlaufzeit aller Albumtitel + Play-Button (Layout wie bei
                // den Einzelliedern). Der Button spielt das ganze Album ab; ein
                // Tippen auf die Zeile öffnet weiterhin die Album-Unterseite.
                let total_ms: i64 = tracks.iter().filter_map(|t| t.duration_ms).sum();
                if total_ms > 0 {
                    row.add_suffix(&duration_label(total_ms));
                }
                let play = gtk::Button::from_icon_name("media-playback-start-symbolic");
                play.add_css_class("flat");
                play.set_valign(gtk::Align::Center);
                play.set_tooltip_text(Some("Album abspielen"));
                {
                    let sender = sender.clone();
                    let name = meta.name.clone();
                    let album = album.clone();
                    play.connect_clicked(move |_| {
                        sender.input(Msg::PlayAlbum {
                            artist: name.clone(),
                            album: album.clone(),
                        });
                    });
                }
                row.add_suffix(&play);

                let album = album.clone();
                let display_artist = display_artist.clone();
                // Kurzes Tippen: Album-Unterseite (Lieder des Albums).
                {
                    let sender = sender.clone();
                    let name = meta.name.clone();
                    let album = album.clone();
                    row.connect_activated(move |_| {
                        sender.input(Msg::OpenAlbumTracks {
                            artist: name.clone(),
                            album: album.clone(),
                        });
                    });
                }
                // Langes Drücken: Album-Detailansicht.
                {
                    let sender = sender.clone();
                    let gesture = gtk::GestureLongPress::new();
                    gesture.connect_pressed(move |g, _, _| {
                        g.set_state(gtk::EventSequenceState::Claimed);
                        sender.input(Msg::ShowAlbumDetailFor {
                            artist: display_artist.clone(),
                            album: album.clone(),
                        });
                    });
                    row.add_controller(gesture);
                }
                group.add(&row);
            }
            content.append(&group);
        }

        // --- danach die Einzellieder (Gast-Titel + Titel ohne Album) ---
        if !singles.is_empty() {
            let n = singles.len();
            let group = adw::PreferencesGroup::builder()
                .title("Einzellieder")
                .description(format!("{n} {}", if n == 1 { "Lied" } else { "Lieder" }))
                .build();
            for t in &singles {
                // Cover-Reihenfolge (nie ein fremdes Ordnerbild):
                // 1) eingebettetes Bild des Titels selbst,
                // 2) Cover des tatsächlichen Albums (auch bei Gast-Titeln),
                // 3) Foto des Haupt-Interpreten.
                let cover_path = crate::core::online::local_track_cover(&t.path)
                    .or_else(|| {
                        let album = t.album.as_deref().filter(|a| !a.trim().is_empty())?;
                        let artist = t.artist.as_deref().unwrap_or("");
                        // Erst exakt (Interpret, Album), sonst irgendein Cover des Albums.
                        self.library
                            .get_album_meta(artist, album)
                            .ok()
                            .flatten()
                            .and_then(|m| m.cover_path)
                            .or_else(|| self.library.album_cover(album).ok().flatten())
                    })
                    .or_else(|| {
                        let artist = t.artist.as_deref().filter(|a| !a.trim().is_empty())?;
                        let primary =
                            crate::core::artist::split_artists(artist).into_iter().next()?;
                        self.library
                            .get_artist_meta(&primary)
                            .ok()
                            .flatten()
                            .and_then(|m| m.image_path)
                    });
                let row = adw::ActionRow::builder()
                    .title(gtk::glib::markup_escape_text(&t.title))
                    .activatable(true)
                    .build();
                // Album als Sekundär-Info unter dem Liednamen (falls vorhanden).
                if let Some(al) = t.album.as_deref().filter(|a| !a.trim().is_empty()) {
                    row.set_subtitle(&gtk::glib::markup_escape_text(al));
                }
                row.add_css_class("emilia-flush");
                row.add_prefix(&cover_widget(cover_path.as_deref(), "audio-x-generic-symbolic"));
                if let Some(ms) = t.duration_ms {
                    if ms > 0 {
                        row.add_suffix(&duration_label(ms));
                    }
                }
                row.add_suffix(&gtk::Image::from_icon_name("media-playback-start-symbolic"));

                let path = t.path.clone();
                // Kurzes Tippen: Titel abspielen.
                {
                    let sender = sender.clone();
                    let name = meta.name.clone();
                    let path = path.clone();
                    row.connect_activated(move |_| {
                        sender.input(Msg::PlayArtistTrack {
                            name: name.clone(),
                            path: path.clone(),
                        });
                    });
                }
                // Langes Drücken: Detailansicht des Liedes.
                {
                    let sender = sender.clone();
                    let gesture = gtk::GestureLongPress::new();
                    gesture.connect_pressed(move |g, _, _| {
                        g.set_state(gtk::EventSequenceState::Claimed);
                        sender.input(Msg::ShowTrackDetail(path.clone()));
                    });
                    row.add_controller(gesture);
                }
                group.add(&row);
            }
            content.append(&group);
        }

        self.push_subpage(&meta.name, &content);
    }

    /// Tippen auf ein Album in der Interpreten-Unterseite: listet dessen Titel
    /// (mit Album-Cover) als weitere Unterseite auf. Tippen auf einen Titel
    /// spielt das gesamte Album ab diesem Titel ab.
    fn open_album_tracks(&self, sender: &ComponentSender<Self>, name: &str, album: &str) {
        // Titel des Albums – `all_tracks` liefert bereits nach Tracknummer sortiert.
        let tracks = self.album_tracks_for_artist(name, album);

        // Cover/Jahr liegen unter dem (häufigsten) rohen Interpreten-Credit.
        let display_artist = most_common_artist(&tracks);
        let album_meta = self
            .library
            .get_album_meta(&display_artist, album)
            .ok()
            .flatten();
        let year = album_meta.as_ref().and_then(|m| m.year);
        let cover_path = album_meta.as_ref().and_then(|m| m.cover_path.clone());
        // Album-Cover einmal dekodieren und in allen Titelzeilen wiederverwenden.
        let cover = cover_path
            .as_deref()
            .and_then(crate::ui::widgets::thumb_cached);

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();

        let group = adw::PreferencesGroup::builder()
            .title(gtk::glib::markup_escape_text(album))
            .description(album_subtitle(year, tracks.len()))
            .build();
        for t in &tracks {
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&t.title))
                .activatable(true)
                .build();
            row.add_css_class("emilia-flush");
            row.add_prefix(&crate::ui::widgets::rounded_image(
                cover.as_ref(),
                "media-optical-symbolic",
                48,
            ));
            if let Some(no) = t.track_no {
                row.add_prefix(
                    &gtk::Label::builder()
                        .label(no.to_string())
                        .width_chars(2)
                        .xalign(1.0)
                        .css_classes(["dim-label", "numeric"])
                        .build(),
                );
            }
            if let Some(ms) = t.duration_ms {
                if ms > 0 {
                    row.add_suffix(&duration_label(ms));
                }
            }
            row.add_suffix(&gtk::Image::from_icon_name("media-playback-start-symbolic"));

            let path = t.path.clone();
            // Kurzes Tippen: Titel abspielen (ganzes Album ab hier).
            {
                let sender = sender.clone();
                let name = name.to_string();
                let album = album.to_string();
                let path = path.clone();
                row.connect_activated(move |_| {
                    sender.input(Msg::PlayAlbumTrack {
                        artist: name.clone(),
                        album: album.clone(),
                        path: path.clone(),
                    });
                });
            }
            // Langes Drücken: Detailansicht des Liedes.
            {
                let sender = sender.clone();
                let gesture = gtk::GestureLongPress::new();
                gesture.connect_pressed(move |g, _, _| {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    sender.input(Msg::ShowTrackDetail(path.clone()));
                });
                row.add_controller(gesture);
            }
            group.add(&row);
        }
        content.append(&group);

        // Kopfzeile: bevorzugt der Album-Interpret, sonst der Seiten-Interpret.
        let header_artist = if display_artist.is_empty() {
            name
        } else {
            display_artist.as_str()
        };
        let title = if header_artist.is_empty() {
            album.to_string()
        } else {
            format!("{header_artist} – {album}")
        };
        self.push_subpage(&title, &content);
    }

    // ---- Ziel-abhängige Helfer für die Detailansicht (Datei/Ordner, Interpret, Album) ----

    /// Abspielbare Dateien des Detailziels.
    fn ctx_files(&self, target: &CtxTarget) -> Vec<PathBuf> {
        match target {
            CtxTarget::Fs(e) => self.entry_files(e),
            CtxTarget::Artist(m) => self.artist_files(&m.name),
            CtxTarget::Album(m) => self.album_files(&m.artist, &m.album),
        }
    }

    /// Cover-/Foto-Textur plus passendes Platzhalter-Icon.
    /// Erkennt, ob ein Dateisystem-Ordner einem Interpreten oder einem Album
    /// entspricht, und liefert die passende EQ-Ebene als
    /// `(Überschrift, Hinweis, scope, key)` – passend zu [`Self::open_eq_editor`].
    /// So lässt sich der Equalizer direkt aus der Dateiansicht auf Interpret- bzw.
    /// Album-Ebene einstellen, mit denselben Schlüsseln wie in der Interpreten-/
    /// Album-Übersicht (damit sich die Einstellungen nicht doppeln).
    /// Erkennt, ob ein Dateisystem-Ordner einem Interpreten oder einem Album
    /// entspricht. Grundlage für Wiedergabe („Album/Interpreten abspielen") und
    /// die EQ-Ebene aus der Dateiansicht.
    fn fs_music_kind(&self, entry: &FsEntry) -> Option<FsKind> {
        if !entry.is_dir() {
            return None;
        }
        // Ordnername = bekannter Interpret? → Interpret (gleicher Schlüssel wie
        // in der Interpreten-Übersicht).
        if let Ok(Some(meta)) = self.library.get_artist_meta(entry.name()) {
            return Some(FsKind::Artist(meta.name));
        }
        // Sonst: enthält der Ordner Titel genau eines Albums? → Album.
        let dir = entry.path();
        let tracks: Vec<Track> = self
            .library
            .all_tracks()
            .unwrap_or_default()
            .into_iter()
            .filter(|t| std::path::Path::new(&t.path).starts_with(dir))
            .collect();
        let albums: std::collections::HashSet<&str> = tracks
            .iter()
            .filter_map(|t| t.album.as_deref())
            .filter(|a| !a.is_empty())
            .collect();
        if albums.len() == 1 {
            let album = albums.into_iter().next().unwrap().to_string();
            let artist = tracks
                .iter()
                .find_map(|t| t.artist.clone())
                .unwrap_or_default();
            return Some(FsKind::Album { artist, album });
        }
        None
    }

    /// EQ-Ebene `(Überschrift, Hinweis, scope, key)` eines Dateisystem-Ordners,
    /// passend zu [`Self::open_eq_editor`] – leitet sich aus [`Self::fs_music_kind`] ab.
    fn fs_eq_level(
        &self,
        entry: &FsEntry,
    ) -> Option<(&'static str, String, Option<&'static str>, &'static str, String)> {
        match self.fs_music_kind(entry)? {
            FsKind::Artist(name) => Some((
                "den Interpreten",
                name.clone(),
                Some("Gilt auch für die Alben und Lieder dieses Interpreten."),
                "artist",
                name,
            )),
            FsKind::Album { artist, album } => {
                let key = category::album_key(&artist, &album);
                Some((
                    "das Album",
                    album,
                    Some("Gilt auch für die Lieder dieses Albums."),
                    "album",
                    key,
                ))
            }
        }
    }

    /// Album-Identität (Interpret, Album) des aktuellen Kontextziels, falls es ein
    /// Album ist (Album-Karte oder als Album erkannter Ordner).
    fn ctx_album(&self) -> Option<(String, String)> {
        match self.context_target.as_ref()? {
            CtxTarget::Album(m) => Some((m.artist.clone(), m.album.clone())),
            CtxTarget::Fs(e) => match self.fs_music_kind(e)? {
                FsKind::Album { artist, album } => Some((artist, album)),
                FsKind::Artist(_) => None,
            },
            CtxTarget::Artist(_) => None,
        }
    }

    /// Interpretenname des aktuellen Kontextziels, falls es ein Interpret ist
    /// (Interpreten-Karte oder als Interpret erkannter Ordner).
    fn ctx_artist(&self) -> Option<String> {
        match self.context_target.as_ref()? {
            CtxTarget::Artist(m) => Some(m.name.clone()),
            CtxTarget::Fs(e) => match self.fs_music_kind(e)? {
                FsKind::Artist(name) => Some(name),
                FsKind::Album { .. } => None,
            },
            CtxTarget::Album(_) => None,
        }
    }

    /// Alben eines Interpreten mit (sofern bekannt) Erscheinungsjahr aus den
    /// Album-Metadaten. Titel je Album bereits nach Tracknummer (siehe
    /// [`Self::artist_albums`]).
    fn artist_albums_dated(&self, name: &str) -> Vec<(Option<i32>, String, Vec<Track>)> {
        self.artist_albums(name)
            .into_iter()
            .map(|(album, tracks)| {
                let artist = tracks
                    .first()
                    .and_then(|t| t.artist.clone())
                    .unwrap_or_default();
                let year = self
                    .library
                    .get_album_meta(&artist, &album)
                    .ok()
                    .flatten()
                    .and_then(|m| m.year);
                (year, album, tracks)
            })
            .collect()
    }

    /// Alle Titel eines Interpreten in Abspielreihenfolge: Alben nach Jahr
    /// (älteste bzw. neueste zuerst, unbekannte Jahre ans Ende), je Album von
    /// Track 1 top-down.
    fn artist_files_ordered(&self, name: &str, newest_first: bool) -> Vec<PathBuf> {
        let mut albums = self.artist_albums_dated(name);
        albums.sort_by(|a, b| {
            use std::cmp::Ordering;
            let by_year = match (a.0, b.0) {
                (Some(x), Some(y)) => {
                    if newest_first {
                        y.cmp(&x)
                    } else {
                        x.cmp(&y)
                    }
                }
                // Bekanntes Jahr vor unbekanntem (in beiden Richtungen).
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            };
            by_year.then_with(|| a.1.cmp(&b.1))
        });
        albums
            .into_iter()
            .flat_map(|(_, _, tracks)| tracks.into_iter().map(|t| PathBuf::from(t.path)))
            .collect()
    }

    /// Jahres-Info der Alben eines Interpreten als `(Label, Wert)`: bei mindestens
    /// zwei **unterschiedlichen** Jahren „Jahre" + „von – bis", bei genau einem
    /// bekannten Jahr „Jahr" + Einzeljahr. `None`, wenn kein Jahr bekannt ist.
    fn artist_year_range(&self, name: &str) -> Option<(&'static str, String)> {
        let mut years: Vec<i32> = self
            .artist_albums_dated(name)
            .into_iter()
            .filter_map(|(year, _, _)| year)
            .collect();
        years.sort_unstable();
        years.dedup();
        match years.as_slice() {
            [] => None,
            [y] => Some(("Jahr", y.to_string())),
            _ => Some(("Jahre", format!("{} – {}", years[0], years[years.len() - 1]))),
        }
    }

    fn ctx_cover(&self, target: &CtxTarget) -> (Option<gtk::gdk::Texture>, &'static str) {
        match target {
            CtxTarget::Fs(e) => {
                // Zuerst ein (Album-)Cover: Cover-Datei, eingebettet, oder online
                // via Tags. Trifft Album-Ordner und Einzeltitel.
                if let Some(tex) = self.cover_texture(e) {
                    (Some(tex), "media-optical-symbolic")
                } else {
                    // Kein Cover gefunden: nächstbestes – das Interpreten-Foto.
                    // Ordner → Ordnername, Datei → Interpret aus den Tags.
                    let artist = if e.is_dir() {
                        Some(e.name().to_string())
                    } else {
                        scanner::read_track(e.path()).ok().and_then(|t| t.artist)
                    };
                    let photo = artist
                        .filter(|a| !a.trim().is_empty())
                        .and_then(|a| self.library.get_artist_meta(&a).ok().flatten())
                        .and_then(|m| m.image_path)
                        .and_then(|p| gtk::gdk::Texture::from_filename(&p).ok());
                    match photo {
                        Some(tex) => (Some(tex), "avatar-default-symbolic"),
                        None => (None, "media-optical-symbolic"),
                    }
                }
            }
            CtxTarget::Artist(m) => {
                let tex = m
                    .image_path
                    .as_deref()
                    .and_then(|p| gtk::gdk::Texture::from_filename(p).ok());
                (tex, "avatar-default-symbolic")
            }
            CtxTarget::Album(m) => {
                let tex = m
                    .cover_path
                    .as_deref()
                    .and_then(|p| gtk::gdk::Texture::from_filename(p).ok());
                (tex, "media-optical-symbolic")
            }
        }
    }


    /// Hängt das Cover/Foto an: bei mehreren Bildern ein Karussell mit Punkten,
    /// sonst das einzelne (primäre) Bild wie bisher.
    fn append_cover_or_gallery(
        &self,
        content: &gtk::Box,
        entry: &CtxTarget,
        sender: &ComponentSender<Self>,
        dialog: &adw::Dialog,
    ) {
        let (texture, placeholder) = self.ctx_cover(entry);
        let mut paths = self.ctx_gallery_paths(entry);

        // Aktuelles Primärbild nach vorn, damit das Karussell darauf startet
        // (so wird beim Schließen ohne Blättern nichts ungewollt geändert).
        let primary = match entry {
            CtxTarget::Album(m) => m.cover_path.clone(),
            CtxTarget::Artist(m) => m.image_path.clone(),
            CtxTarget::Fs(_) => None,
        };
        if let Some(pos) = primary.and_then(|p| paths.iter().position(|x| *x == p)) {
            let p = paths.remove(pos);
            paths.insert(0, p);
        }

        if paths.len() > 1 {
            let carousel = adw::Carousel::new();
            carousel.set_halign(gtk::Align::Center);
            for path in &paths {
                let tex = gtk::gdk::Texture::from_filename(path).ok();
                let img = crate::ui::widgets::rounded_image(tex.as_ref(), placeholder, 180);
                carousel.append(&img);
            }
            let dots = adw::CarouselIndicatorDots::new();
            dots.set_carousel(Some(&carousel));

            let gallery = gtk::Box::new(gtk::Orientation::Vertical, 6);
            gallery.set_halign(gtk::Align::Center);
            gallery.append(&carousel);
            gallery.append(&dots);
            content.append(&gallery);

            // Beim Schließen der Detailansicht das zuletzt im Karussell gezeigte
            // Bild sofort als primäres Cover/Foto übernehmen (gilt dann überall).
            let album_id = match entry {
                CtxTarget::Album(m) => Some((m.artist.clone(), m.album.clone())),
                _ => None,
            };
            let artist_id = match entry {
                CtxTarget::Artist(m) => Some(m.name.clone()),
                _ => None,
            };
            let sender = sender.clone();
            dialog.connect_closed(move |_| {
                let idx = carousel.position().round().max(0.0) as usize;
                let Some(path) = paths.get(idx).cloned() else {
                    return;
                };
                if let Some((artist, album)) = &album_id {
                    sender.input(Msg::SetAlbumCover {
                        artist: artist.clone(),
                        album: album.clone(),
                        path,
                    });
                } else if let Some(name) = &artist_id {
                    sender.input(Msg::SetArtistImage {
                        name: name.clone(),
                        path,
                    });
                }
            });
        } else {
            let cover = crate::ui::widgets::rounded_image(texture.as_ref(), placeholder, 180);
            let cover_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            cover_box.set_halign(gtk::Align::Center);
            cover_box.set_hexpand(false);
            cover_box.append(&cover);
            content.append(&cover_box);
        }
    }

    /// Gespeicherte Galerie-Bildpfade eines Ziels (nur existierende Dateien).
    fn ctx_gallery_paths(&self, entry: &CtxTarget) -> Vec<String> {
        let stored = match entry {
            CtxTarget::Artist(m) => self.library.artist_images(&m.name).unwrap_or_default(),
            CtxTarget::Album(m) => {
                self.library.album_images(&m.artist, &m.album).unwrap_or_default()
            }
            CtxTarget::Fs(_) => Vec::new(),
        };
        stored
            .into_iter()
            .filter(|p| std::path::Path::new(p).exists())
            .collect()
    }

    /// Detailzeilen für die "Mehr Infos"-Aufklappung.
    fn ctx_info_lines(&self, target: &CtxTarget) -> Vec<(&'static str, String)> {
        match target {
            CtxTarget::Fs(e) => self.info_lines(e),
            CtxTarget::Artist(m) => {
                let files = self.artist_files(&m.name);
                let mut lines = vec![("Interpret", m.name.clone())];
                // Jahr/Jahre der Alben, je nach Album-Metadaten.
                let year = self.artist_year_range(&m.name);
                let year_shown = year.is_some();
                if let Some((label, value)) = year {
                    lines.push((label, value));
                }
                lines.push(("Kurzübersicht", Self::files_summary(&files, !year_shown)));
                lines
            }
            CtxTarget::Album(m) => {
                let mut lines = Vec::new();
                if !m.artist.is_empty() {
                    lines.push(("Interpret", m.artist.clone()));
                }
                lines.push(("Album", m.album.clone()));
                if let Some(y) = m.year {
                    lines.push(("Jahr", y.to_string()));
                }
                let files = self.album_files(&m.artist, &m.album);
                lines.push(("Kurzübersicht", Self::files_summary(&files, m.year.is_none())));
                lines
            }
        }
    }

    /// "Merkmale"-Gruppe des Detailziels (Datei: alle Ebenen; Interpret/Album: passend).
    fn ctx_merkmale(
        &self,
        target: &CtxTarget,
        sender: &ComponentSender<Self>,
    ) -> Option<adw::PreferencesGroup> {
        match target {
            CtxTarget::Fs(e) => self.build_merkmale(e, sender),
            CtxTarget::Artist(m) => Some(self.artist_merkmale(&m.name, sender)),
            CtxTarget::Album(m) => Some(self.album_merkmale(&m.artist, &m.album, sender)),
        }
    }

    /// "Merkmale"-Gruppe für einen Interpreten: eine Auswahl auf Interpret-Ebene.
    fn artist_merkmale(
        &self,
        name: &str,
        sender: &ComponentSender<Self>,
    ) -> adw::PreferencesGroup {
        let group = adw::PreferencesGroup::builder().title("Merkmale").build();
        let expander = adw::ExpanderRow::builder().title("Merkmal").build();

        let (eff, src) = self.library.resolve_category(Some(name), None, "");
        let eff_label = Category::from_str(&eff).unwrap_or(Category::DEFAULT).label();
        let src_label = if src == "artist" { "Interpret" } else { "Standard" };
        expander.set_subtitle(&format!("{eff_label} (von: {src_label})"));

        let cur = self.library.get_category("artist", name).ok().flatten();
        self.add_category_row(
            &expander,
            sender,
            &format!("Interpret: {name}"),
            "artist",
            name.to_string(),
            cur,
        );

        group.add(&expander);
        group
    }

    /// "Merkmale"-Gruppe für ein Album: Album-Ebene plus geerbte Interpret-Ebene.
    fn album_merkmale(
        &self,
        artist: &str,
        album: &str,
        sender: &ComponentSender<Self>,
    ) -> adw::PreferencesGroup {
        let group = adw::PreferencesGroup::builder().title("Merkmale").build();
        let expander = adw::ExpanderRow::builder().title("Merkmal").build();

        let (eff, src) = self.library.resolve_category(Some(artist), Some(album), "");
        let eff_label = Category::from_str(&eff).unwrap_or(Category::DEFAULT).label();
        let src_label = match src {
            "album" => "Album",
            "artist" => "Interpret",
            _ => "Standard",
        };
        expander.set_subtitle(&format!("{eff_label} (von: {src_label})"));

        // Album-Ebene
        let key = category::album_key(artist, album);
        let cur = self.library.get_category("album", &key).ok().flatten();
        self.add_category_row(
            &expander,
            sender,
            &format!("Album: {album}"),
            "album",
            key,
            cur,
        );
        // Interpret-Ebene (geerbt)
        if !artist.is_empty() {
            let cur = self.library.get_category("artist", artist).ok().flatten();
            self.add_category_row(
                &expander,
                sender,
                &format!("Interpret: {artist}"),
                "artist",
                artist.to_string(),
                cur,
            );
        }

        group.add(&expander);
        group
    }

    /// Kurzübersicht über eine Dateimenge: „N Alben - M Lieder - 2001–2010".
    /// Kurzübersicht „N Alben - M Lieder[ - Jahr/Bereich]". Das Jahr wird nur
    /// angehängt, wenn `with_year` gesetzt ist – sobald eine eigene „Jahr"/„Jahre"-
    /// Zeile angezeigt wird, entfällt es hier (Dopplung vermeiden).
    fn files_summary(files: &[PathBuf], with_year: bool) -> String {
        let songs = files.len();
        let mut albums = std::collections::HashSet::new();
        let mut min_year: Option<u32> = None;
        let mut max_year: Option<u32> = None;
        for f in files {
            let (album, year) = scanner::read_album_year(f);
            if let Some(a) = album {
                albums.insert(a);
            }
            if let Some(y) = year {
                min_year = Some(min_year.map_or(y, |m| m.min(y)));
                max_year = Some(max_year.map_or(y, |m| m.max(y)));
            }
        }

        let mut value = String::new();
        let n = albums.len();
        if n > 0 {
            value.push_str(&format!("{n} {} - ", if n == 1 { "Album" } else { "Alben" }));
        }
        value.push_str(&format!("{songs} {}", if songs == 1 { "Lied" } else { "Lieder" }));
        if with_year {
            if let (Some(a), Some(b)) = (min_year, max_year) {
                let span = if a == b {
                    a.to_string()
                } else {
                    format!("{a}\u{2013}{b}")
                };
                value.push_str(&format!(" - {span}"));
            }
        }
        value
    }

    fn toast(&self, msg: &str) {
        self.toast_overlay.add_toast(adw::Toast::new(msg));
    }

    /// Aktionsmenü beim langen Drücken (Ordner oder Titel).
    fn open_context_menu(&self, root: &adw::ApplicationWindow, sender: &ComponentSender<Self>) {
        let Some(entry) = self.context_target.as_ref() else {
            return;
        };

        let dialog = adw::Dialog::builder().title(entry.heading()).build();

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(6)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();

        // Cover/Foto bzw. – bei mehreren Bildern – ein Karussell mit Punkten.
        self.append_cover_or_gallery(&content, entry, sender, &dialog);

        // "Mehr Infos" – aufklappbar mit Detailzeilen
        let info_group = adw::PreferencesGroup::new();
        let expander = adw::ExpanderRow::builder().title("Mehr Infos").build();
        for (label, value) in self.ctx_info_lines(entry) {
            let row = adw::ActionRow::builder()
                .title(label)
                .subtitle(gtk::glib::markup_escape_text(&value))
                .build();
            row.set_subtitle_lines(2);
            expander.add_row(&row);
        }
        info_group.add(&expander);
        content.append(&info_group);

        // "Merkmale" – Kategorie je Ebene (Titel/Album/Interpret), vererbt.
        if let Some(merkmale) = self.ctx_merkmale(entry, sender) {
            content.append(&merkmale);
        }

        // Aktionen
        let action_group = adw::PreferencesGroup::new();
        // Wiedergabe-Art des Ziels bestimmen (Label + Reihenfolge der Play-Aktion).
        #[derive(Clone, Copy)]
        enum PlayKind {
            Album,
            Artist,
            Other,
        }
        let play_kind = match entry {
            CtxTarget::Album(_) => PlayKind::Album,
            CtxTarget::Artist(_) => PlayKind::Artist,
            CtxTarget::Fs(e) if e.is_dir() => match self.fs_music_kind(e) {
                Some(FsKind::Album { .. }) => PlayKind::Album,
                Some(FsKind::Artist(_)) => PlayKind::Artist,
                None => PlayKind::Other,
            },
            CtxTarget::Fs(_) => PlayKind::Other,
        };
        // Equalizer dort anbieten, wo es eine eindeutige Ebene gibt: bei Titeln
        // und Karten sowie bei Ordnern, die als Interpret oder Album erkannt werden.
        let show_eq = !matches!(
            (entry, play_kind),
            (CtxTarget::Fs(e), PlayKind::Other) if e.is_dir()
        );

        // Play-Aktion: bei Album/Interpret eigener Text und eigene Reihenfolge.
        let play_row = adw::ActionRow::builder()
            .title(match play_kind {
                PlayKind::Album => "Album abspielen",
                PlayKind::Artist => "Interpreten abspielen",
                PlayKind::Other => "Abspielen",
            })
            .activatable(true)
            .build();
        play_row.add_prefix(&gtk::Image::from_icon_name("media-playback-start-symbolic"));
        match play_kind {
            PlayKind::Artist => {
                // Album-Reihenfolge wählbar, auf gleicher Höhe wie die Aktion.
                let order = gtk::DropDown::from_strings(&["Älteste zuerst", "Neueste zuerst"]);
                order.set_valign(gtk::Align::Center);
                order.set_tooltip_text(Some("Reihenfolge der Alben"));
                play_row.add_suffix(&order);
                let sender = sender.clone();
                let dialog = dialog.clone();
                play_row.connect_activated(move |_| {
                    sender.input(Msg::CtxPlayArtist {
                        newest_first: order.selected() == 1,
                    });
                    dialog.close();
                });
            }
            PlayKind::Album => {
                let sender = sender.clone();
                let dialog = dialog.clone();
                play_row.connect_activated(move |_| {
                    sender.input(Msg::CtxPlayAlbum);
                    dialog.close();
                });
            }
            PlayKind::Other => {
                let sender = sender.clone();
                let dialog = dialog.clone();
                play_row.connect_activated(move |_| {
                    sender.input(Msg::CtxPlay);
                    dialog.close();
                });
            }
        }
        action_group.add(&play_row);

        // Übrige Aktionen.
        let mut actions: Vec<(&str, &str, fn() -> Msg)> = vec![
            ("Zur Queue hinzufügen", "list-add-symbolic", || Msg::CtxAddQueue),
            ("Zur Playlist hinzufügen", "view-list-symbolic", || {
                Msg::CtxAddPlaylist
            }),
        ];
        if show_eq {
            actions.push(("Equalizer-Einstellungen", "preferences-other-symbolic", || {
                Msg::CtxEqualizer
            }));
        }
        actions.push(("Teilen", "emblem-shared-symbolic", || Msg::CtxShare));
        for (label, icon, make_msg) in actions {
            let row = adw::ActionRow::builder()
                .title(label)
                .activatable(true)
                .build();
            row.add_prefix(&gtk::Image::from_icon_name(icon));
            let sender = sender.clone();
            let dialog = dialog.clone();
            row.connect_activated(move |_| {
                sender.input(make_msg());
                dialog.close();
            });
            action_group.add(&row);
        }
        content.append(&action_group);

        // Bei zu großem Inhalt (z. B. auf dem Phone) vertikal scrollen, sonst
        // den Dialog auf die natürliche Inhaltshöhe wachsen lassen. `Automatic`
        // blendet bei Überlauf einen Scrollbalken ein – mit `External` wurden die
        // unteren Aktionen (Equalizer, Teilen) auf schmalen Fenstern unerreichbar.
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .propagate_natural_height(true)
            .propagate_natural_width(true)
            .vexpand(true)
            .child(&content)
            .build();
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&scroller));
        dialog.set_child(Some(&toolbar));
        dialog.present(Some(root));
    }

    /// „Teilen"-Dialog: Verbindung anbieten (Dienst starten) oder QR-Code einlesen.
    /// Die eigentliche Geräte-Sync-Logik folgt später.
    fn open_share_dialog(&self, root: &adw::ApplicationWindow, sender: &ComponentSender<Self>) {
        let dialog = adw::Dialog::builder().title("Teilen").build();

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(6)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();

        let group = adw::PreferencesGroup::builder()
            .description("Mit einem anderen Gerät verbinden, um Inhalte zu synchronisieren.")
            .build();

        let actions: [(&str, &str, &str, fn() -> Msg); 2] = [
            (
                "Verbindung anbieten",
                "Dienst starten und auf ein anderes Gerät warten",
                "network-wireless-symbolic",
                || Msg::ShareHost,
            ),
            (
                "QR-Code einlesen",
                "Den Code eines anderen Geräts scannen",
                "camera-photo-symbolic",
                || Msg::ShareScan,
            ),
        ];

        for (title, subtitle, icon, make_msg) in actions {
            let row = adw::ActionRow::builder()
                .title(title)
                .subtitle(subtitle)
                .activatable(true)
                .build();
            row.add_prefix(&gtk::Image::from_icon_name(icon));
            let sender = sender.clone();
            let dialog = dialog.clone();
            row.connect_activated(move |_| {
                sender.input(make_msg());
                dialog.close();
            });
            group.add(&row);
        }

        content.append(&group);

        // Bei zu großem Inhalt (z. B. auf dem Phone) vertikal scrollen, sonst
        // den Dialog auf die natürliche Inhaltshöhe wachsen lassen. `Automatic`
        // blendet bei Überlauf einen Scrollbalken ein.
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .propagate_natural_height(true)
            .propagate_natural_width(true)
            .vexpand(true)
            .child(&content)
            .build();
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&scroller));
        dialog.set_child(Some(&toolbar));
        dialog.present(Some(root));
    }

    /// Beschafft ein Cover als Textur. Für einen **Ordner** das Ordner-Cover
    /// (= Albumbild); für eine **Einzeldatei** bewusst **kein** Ordnerbild, damit
    /// ein Titel kein fremdes Cover aus einem geteilten Ordner erbt – stattdessen
    /// das eingebettete Bild der Datei bzw. das online zugeordnete Album-Cover.
    /// `None`, wenn nichts Passendes gefunden wird.
    fn cover_texture(&self, entry: &FsEntry) -> Option<gtk::gdk::Texture> {
        if entry.is_dir() {
            if let Some(path) = cover::find_cover_file(entry.path()) {
                if let Ok(texture) = gtk::gdk::Texture::from_filename(&path) {
                    return Some(texture);
                }
            }
        }

        let audio = if entry.is_dir() {
            std::fs::read_dir(entry.path())
                .ok()
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| scanner::is_audio(p))
                .min()
        } else {
            Some(entry.path().clone())
        };

        if let Some(audio) = &audio {
            if let Some(bytes) = cover::embedded_cover(audio) {
                if let Ok(tex) =
                    gtk::gdk::Texture::from_bytes(&gtk::glib::Bytes::from(bytes.as_slice()))
                {
                    return Some(tex);
                }
            }
        }

        // Zuletzt: online geladenes Cover aus dem Cache (über die Tags zugeordnet).
        let track = scanner::read_track(audio.as_ref()?).ok()?;
        let (artist, album) = (track.artist?, track.album?);
        let meta = self.library.get_album_meta(&artist, &album).ok().flatten()?;
        let path = meta.cover_path?;
        gtk::gdk::Texture::from_filename(&path).ok()
    }

    /// Baut die „Merkmale"-Gruppe für einen Titel: je eine Auswahl für die
    /// Titel-, Album- und Interpret-Ebene. Höhere Ebenen werden vererbt, jede
    /// kann individuell übersteuert werden. Für Ordner: `None`.
    fn build_merkmale(
        &self,
        entry: &FsEntry,
        sender: &ComponentSender<Self>,
    ) -> Option<adw::PreferencesGroup> {
        if entry.is_dir() {
            return None;
        }
        let track = scanner::read_track(entry.path()).ok()?;
        let path = entry.path().to_string_lossy().into_owned();
        let artist = track.artist.filter(|s| !s.is_empty());
        let album = track.album.filter(|s| !s.is_empty());

        let group = adw::PreferencesGroup::builder().title("Merkmale").build();
        let expander = adw::ExpanderRow::builder().title("Merkmal").build();

        // Effektives Merkmal (geerbt/aufgelöst) als Untertitel.
        let (eff, src) =
            self.library
                .resolve_category(artist.as_deref(), album.as_deref(), &path);
        let eff_label = Category::from_str(&eff).unwrap_or(Category::DEFAULT).label();
        let src_label = match src {
            "track" => "Titel",
            "album" => "Album",
            "artist" => "Interpret",
            _ => "Standard",
        };
        expander.set_subtitle(&format!("{eff_label} (von: {src_label})"));

        // Titel-Ebene
        let cur = self.library.get_category("track", &path).ok().flatten();
        self.add_category_row(&expander, sender, "Dieser Titel", "track", path.clone(), cur);
        // Album-Ebene
        if let Some(al) = &album {
            let key = category::album_key(artist.as_deref().unwrap_or(""), al);
            let cur = self.library.get_category("album", &key).ok().flatten();
            self.add_category_row(&expander, sender, &format!("Album: {al}"), "album", key, cur);
        }
        // Interpret-Ebene
        if let Some(a) = &artist {
            let cur = self.library.get_category("artist", a).ok().flatten();
            self.add_category_row(
                &expander,
                sender,
                &format!("Interpret: {a}"),
                "artist",
                a.clone(),
                cur,
            );
        }

        group.add(&expander);
        Some(group)
    }

    /// Eine Auswahl-Zeile („Erben" + die vier Merkmale) für eine Ebene.
    fn add_category_row(
        &self,
        expander: &adw::ExpanderRow,
        sender: &ComponentSender<Self>,
        title: &str,
        scope: &'static str,
        key: String,
        current: Option<String>,
    ) {
        let list = gtk::StringList::new(&["Erben", "Musik", "Konzert", "Podcast", "Hörbuch"]);
        let row = adw::ComboRow::builder().title(title).model(&list).build();

        // Aktuelle Festlegung vorauswählen (0 = erben).
        let selected = current
            .as_deref()
            .and_then(Category::from_str)
            .and_then(|c| Category::ALL.iter().position(|x| *x == c))
            .map(|i| i as u32 + 1)
            .unwrap_or(0);
        row.set_selected(selected);

        let sender = sender.clone();
        row.connect_selected_notify(move |r| {
            let idx = r.selected();
            let value = if idx == 0 {
                None
            } else {
                Some(Category::ALL[(idx - 1) as usize].as_str())
            };
            sender.input(Msg::SetCategory {
                scope,
                key: key.clone(),
                value,
            });
        });
        expander.add_row(&row);
    }

    /// Equalizer-Dialog: oben **Ausgang** (Gerät/Bluetooth) und **Ebene**
    /// (Global/Interpret/Album/Titel) wählen, darunter zehn Frequenzregler.
    /// Änderungen wirken sofort und werden je Ausgang+Ebene gespeichert; beim
    /// Abspielen greift die Vererbung (Titel→Album→Interpret→Global, dann der
    /// Standard-Ausgang als Basis).
    fn open_eq_dialog(&self, root: &adw::ApplicationWindow, sender: &ComponentSender<Self>) {
        let Some(entry) = self.context_target.as_ref() else {
            return;
        };

        // Genau eine Ebene je Ziel; die Vererbung nach unten (Interpret→Album→Titel)
        // übernimmt beim Abspielen `resolve_eq`. „Global" liegt in den Einstellungen.
        let (subject, name, note, scope, key): (
            &'static str,
            String,
            Option<&str>,
            &'static str,
            String,
        ) = match entry {
            CtxTarget::Artist(m) => (
                "den Interpreten",
                m.name.clone(),
                Some("Gilt auch für die Alben und Lieder dieses Interpreten."),
                "artist",
                m.name.clone(),
            ),
            CtxTarget::Album(m) => (
                "das Album",
                m.album.clone(),
                Some("Gilt auch für die Lieder dieses Albums."),
                "album",
                category::album_key(&m.artist, &m.album),
            ),
            CtxTarget::Fs(e) if !e.is_dir() => (
                "den Titel",
                e.display_title(),
                None,
                "track",
                e.path().to_string_lossy().into_owned(),
            ),
            // Ordner: als Interpret oder Album erkennen; sonst kein EQ.
            CtxTarget::Fs(e) => match self.fs_eq_level(e) {
                Some(level) => level,
                None => {
                    self.toast("Equalizer ist hier nicht verfügbar");
                    return;
                }
            },
        };

        self.open_eq_editor(root, sender, subject, &name, note, scope, key);
    }

    /// Globaler Equalizer (aus den Einstellungen): Basis für alles ohne eigene
    /// Festlegung auf Interpret-, Album- oder Titel-Ebene.
    fn open_global_eq(&self, root: &adw::ApplicationWindow, sender: &ComponentSender<Self>) {
        self.open_eq_editor(
            root,
            sender,
            "den globalen Equalizer",
            "",
            Some("Gilt für alles ohne eigene Einstellung für Interpret, Album oder Titel."),
            "global",
            String::new(),
        );
    }

    /// Equalizer-Editor für genau eine Ebene (scope/key) mit Ausgang-Auswahl.
    /// Genutzt vom Detail-EQ (Interpret/Album/Titel) und vom globalen EQ.
    fn open_eq_editor(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
        subject: &str,
        name: &str,
        note: Option<&str>,
        scope: &'static str,
        key: String,
    ) {
        use std::cell::{Cell, RefCell};
        use std::rc::Rc;

        // Ausgänge: „Standard (alle)" als Basis + automatisch erkannte Geräte.
        let mut outputs: Vec<(String, String)> =
            vec![("Standard (alle Ausgänge)".to_string(), String::new())];
        for o in crate::core::output::list_outputs() {
            outputs.push((o.name, o.id));
        }
        let out_default = outputs
            .iter()
            .position(|(_, id)| !id.is_empty() && *id == self.active_output)
            .unwrap_or(0);

        // Bänder je Ausgang vorladen (kein DB-Zugriff in den Closures).
        let preloaded: Vec<[f64; 10]> = outputs
            .iter()
            .map(|(_, oid)| self.library.get_eq(oid, scope, &key).ok().flatten().unwrap_or([0.0; 10]))
            .collect();

        let outputs = Rc::new(outputs);
        let bands = Rc::new(RefCell::new(preloaded));
        let cur_out = Rc::new(Cell::new(out_default));
        let key = Rc::new(key);
        let loading = Rc::new(Cell::new(false));

        let dialog = adw::Dialog::builder()
            .title("Equalizer")
            .content_width(440)
            .content_height(620)
            .build();
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(6)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();

        // Kopf: „Einstellungen für …" dezent, der Name darunter zentriert und
        // hervorgehoben. Beim globalen EQ (ohne Namen) trägt der Präfix selbst die
        // Überschrift.
        let has_name = !name.is_empty();
        let header = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .build();
        let prefix_css: Vec<&str> = if has_name {
            vec!["dim-label", "caption"]
        } else {
            vec!["title-2"]
        };
        let prefix = gtk::Label::builder()
            .label(format!("Einstellungen für {subject}"))
            .halign(gtk::Align::Center)
            .justify(gtk::Justification::Center)
            .wrap(true)
            .css_classes(prefix_css)
            .build();
        header.append(&prefix);
        if has_name {
            let name_label = gtk::Label::builder()
                .label(name)
                .halign(gtk::Align::Center)
                .justify(gtk::Justification::Center)
                .wrap(true)
                .css_classes(["title-2"])
                .build();
            header.append(&name_label);
        }
        if let Some(n) = note {
            let note_label = gtk::Label::builder()
                .label(n)
                .halign(gtk::Align::Center)
                .justify(gtk::Justification::Center)
                .wrap(true)
                .css_classes(["dim-label", "caption"])
                .build();
            header.append(&note_label);
        }
        content.append(&header);

        // Ausgang-Auswahl (eigene Gruppe ohne Titel – Kopf steht darüber).
        let sel_group = adw::PreferencesGroup::new();

        let out_labels: Vec<&str> = outputs.iter().map(|(l, _)| l.as_str()).collect();
        let out_combo = adw::ComboRow::builder()
            .title("Ausgang")
            .subtitle("Gerät / Bluetooth")
            .model(&gtk::StringList::new(&out_labels))
            .build();
        out_combo.set_selected(out_default as u32);
        sel_group.add(&out_combo);
        content.append(&sel_group);

        // Zehn Frequenzregler.
        let freqs = [
            "29 Hz", "59 Hz", "119 Hz", "237 Hz", "474 Hz", "947 Hz", "1.9 kHz", "3.8 kHz",
            "7.5 kHz", "15 kHz",
        ];
        let bands_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .build();
        let mut scales = Vec::with_capacity(10);
        let start = bands.borrow()[out_default];
        for (i, freq) in freqs.iter().enumerate() {
            let row = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(8)
                .build();
            let label = gtk::Label::builder()
                .label(*freq)
                .width_chars(7)
                .xalign(0.0)
                .css_classes(["caption", "numeric"])
                .build();
            let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, -24.0, 12.0, 1.0);
            scale.set_hexpand(true);
            scale.set_draw_value(true);
            scale.set_value_pos(gtk::PositionType::Right);
            scale.add_mark(0.0, gtk::PositionType::Top, None);
            scale.set_value(start[i]);
            row.append(&label);
            row.append(&scale);
            bands_box.append(&row);
            scales.push(scale);
        }
        let scales = Rc::new(scales);
        content.append(&bands_box);

        // Reglerbewegung → Wert merken + speichern (+ live anwenden via Msg).
        for (i, scale) in scales.iter().enumerate() {
            let bands = bands.clone();
            let cur_out = cur_out.clone();
            let loading = loading.clone();
            let outputs = outputs.clone();
            let key = key.clone();
            let sender = sender.clone();
            scale.connect_value_changed(move |s| {
                if loading.get() {
                    return;
                }
                let o = cur_out.get();
                bands.borrow_mut()[o][i] = s.value();
                let arr = bands.borrow()[o];
                let (_, oid) = &outputs[o];
                sender.input(Msg::SetEq {
                    output: oid.clone(),
                    scope,
                    key: (*key).clone(),
                    bands: arr,
                });
            });
        }

        // Ausgang wechseln → Regler aus den Vorlade-Werten neu laden.
        {
            let bands = bands.clone();
            let cur_out = cur_out.clone();
            let loading = loading.clone();
            let scales = scales.clone();
            out_combo.connect_selected_notify(move |c| {
                cur_out.set(c.selected() as usize);
                loading.set(true);
                let arr = bands.borrow()[cur_out.get()];
                for (i, sc) in scales.iter().enumerate() {
                    sc.set_value(arr[i]);
                }
                loading.set(false);
            });
        }

        // Aktuelle Auswahl neutralstellen und auf „erben" zurücksetzen.
        let reset = gtk::Button::builder()
            .label("Zurücksetzen")
            .css_classes(["pill"])
            .halign(gtk::Align::Center)
            .build();
        {
            let bands = bands.clone();
            let cur_out = cur_out.clone();
            let loading = loading.clone();
            let scales = scales.clone();
            let outputs = outputs.clone();
            let key = key.clone();
            let sender = sender.clone();
            reset.connect_clicked(move |_| {
                let o = cur_out.get();
                bands.borrow_mut()[o] = [0.0; 10];
                loading.set(true);
                for sc in scales.iter() {
                    sc.set_value(0.0);
                }
                loading.set(false);
                let (_, oid) = &outputs[o];
                sender.input(Msg::ClearEq {
                    output: oid.clone(),
                    scope,
                    key: (*key).clone(),
                });
            });
        }
        content.append(&reset);

        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .vexpand(true)
            .child(&content)
            .build();
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&scroller));
        dialog.set_child(Some(&toolbar));
        dialog.present(Some(root));
    }

    /// Detailzeilen für die "Mehr Infos"-Aufklappung.
    fn info_lines(&self, entry: &FsEntry) -> Vec<(&'static str, String)> {
        let mut lines = Vec::new();
        if entry.is_dir() {
            // Als Album/Interpret erkannte Ordner zeigen passende Infos inkl. Jahr.
            let mut year_shown = false;
            match self.fs_music_kind(entry) {
                Some(FsKind::Album { artist, album }) => {
                    if !artist.is_empty() {
                        lines.push(("Interpret", artist.clone()));
                    }
                    lines.push(("Album", album.clone()));
                    if let Some(y) = self
                        .library
                        .get_album_meta(&artist, &album)
                        .ok()
                        .flatten()
                        .and_then(|m| m.year)
                    {
                        lines.push(("Jahr", y.to_string()));
                        year_shown = true;
                    }
                }
                Some(FsKind::Artist(name)) => {
                    lines.push(("Interpret", name.clone()));
                    if let Some((label, value)) = self.artist_year_range(&name) {
                        lines.push((label, value));
                        year_shown = true;
                    }
                }
                None => {}
            }
            let files = self.entry_files(entry);
            lines.push(("Kurzübersicht", Self::files_summary(&files, !year_shown)));
        } else {
            match scanner::read_track(entry.path()) {
                Ok(t) => {
                    lines.push(("Titel", t.title));
                    // Interpret/Album für die Jahres-Auflösung merken (werden
                    // beim Anzeigen verbraucht).
                    let (artist, album) = (t.artist.clone(), t.album.clone());
                    if let Some(a) = t.artist {
                        lines.push(("Interpret", a));
                    }
                    if let Some(al) = t.album {
                        lines.push(("Album", al));
                    }
                    if let Some(d) = t.duration_ms {
                        lines.push(("Dauer", fmt_duration(d)));
                    }
                    // Jahr (aus den Album-Metadaten) direkt unter der Dauer.
                    if let (Some(artist), Some(album)) = (artist, album) {
                        if let Some(y) = self
                            .library
                            .get_album_meta(&artist, &album)
                            .ok()
                            .flatten()
                            .and_then(|m| m.year)
                        {
                            lines.push(("Jahr", y.to_string()));
                        }
                    }
                }
                Err(_) => {}
            }

            // Per Fingerprint (AcoustID) erkannte Vorschläge – nur Anzeige,
            // wird nicht in die Datei geschrieben.
            if let Ok(Some(m)) = self
                .library
                .get_track_meta(&entry.path().to_string_lossy())
            {
                if m.status == "matched" {
                    if let Some(t) = m.title {
                        lines.push(("Erkannt (Titel)", t));
                    }
                    if let Some(a) = m.artist {
                        lines.push(("Erkannt (Interpret)", a));
                    }
                    if let Some(al) = m.album {
                        lines.push(("Erkannt (Album)", al));
                    }
                }
            }
        }
        lines
    }

    /// Aktualisiert die Queue-Markierung aller sichtbaren Dateizeilen.
    fn refresh_queue_icons(&mut self) {
        let queue = self.queue.clone();
        // Aktuell laufender Titel (für die Play-Markierung).
        let active_path = self.queue.get(self.queue_pos).cloned();
        let states: Vec<(usize, bool, bool)> = {
            let guard = self.entries.guard();
            (0..guard.len())
                .filter_map(|i| {
                    guard.get(i).map(|r| {
                        let is_file = !r.entry.is_dir();
                        let q = is_file && queue.iter().any(|p| p == r.entry.path());
                        let a = is_file
                            && active_path.as_deref() == Some(r.entry.path().as_path());
                        (i, q, a)
                    })
                })
                .collect()
        };
        let playing = self.playing;
        for (i, q, a) in states {
            self.entries.send(i, FsInput::SetQueued(q));
            self.entries.send(i, FsInput::SetActive { active: a, playing });
        }
    }

    /// Nächster Titel: bei Zufall ein zufälliger, sonst der folgende.
    /// Am sequentiellen Ende wird gestoppt.
    fn play_next(&mut self) {
        if self.queue.is_empty() {
            return;
        }
        let len = self.queue.len();
        let next = if self.shuffle {
            gtk::glib::random_int_range(0, len as i32) as usize
        } else if self.queue_pos + 1 < len {
            self.queue_pos + 1
        } else {
            self.save_resume();
            self.player.stop();
            self.playing = false;
            self.playing_path = None;
            self.refresh_queue_icons();
            return;
        };
        self.queue_pos = next;
        self.play_current();
    }

    /// Vorheriger Titel (sequentiell).
    fn play_prev(&mut self) {
        if !self.queue.is_empty() && self.queue_pos > 0 {
            self.queue_pos -= 1;
            self.play_current();
        }
    }

    /// Spielt den aktuellen Eintrag der Warteschlange ab.
    /// Anzeigename eines Titels für die Leiste: „Interpret - Titel" aus den Tags,
    /// notfalls der Dateiname.
    fn track_display_name(path: &std::path::Path) -> String {
        let stem = || {
            path.file_stem()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string()
        };
        match scanner::read_track(path) {
            Ok(t) => {
                let title = if t.title.trim().is_empty() {
                    stem()
                } else {
                    t.title
                };
                match t.artist {
                    Some(a) if !a.trim().is_empty() => format!("{a} - {title}"),
                    _ => title,
                }
            }
            Err(_) => stem(),
        }
    }

    /// Ob für diesen Titel eine Resume-Position geführt werden soll: bei langen
    /// Titeln (Hörspiele) immer, sonst nur, wenn er als Hörbuch oder Podcast
    /// eingestuft ist. Normale (kurze) Musiktitel starten stets von vorn.
    fn should_resume(&self, t: &Track) -> bool {
        if t.duration_ms.unwrap_or(0) >= RESUME_MIN_DURATION_MS {
            return true;
        }
        let (cat, _) =
            self.library
                .resolve_category(t.artist.as_deref(), t.album.as_deref(), &t.path);
        matches!(cat.as_str(), "audiobook" | "podcast")
    }

    /// Sichert die aktuelle Wiedergabeposition des geladenen Titels als
    /// Resume-Punkt. Nahe Anfang oder Ende wird auf 0 zurückgesetzt, damit ein
    /// quasi fertiger Titel beim nächsten Mal von vorn beginnt.
    fn save_resume(&self) {
        let Some(path) = self.playing_path.clone() else {
            return;
        };
        let path_str = path.to_string_lossy();
        let Some(track) = self.library.track_by_path(&path_str).ok().flatten() else {
            return;
        };
        if !self.should_resume(&track) {
            return;
        }
        let Some(pos) = self.player.position_ms() else {
            return;
        };
        let dur = self.player.duration_ms().or(track.duration_ms).unwrap_or(0);
        let resume = if pos < RESUME_MIN_POS_MS {
            0
        } else if dur > 0 && pos > dur - RESUME_END_GUARD_MS {
            0
        } else {
            pos
        };
        let _ = self.library.set_resume_path(&path_str, resume);
    }

    fn play_current(&mut self) {
        // Position des bisher laufenden Titels sichern, bevor ein neuer geladen wird.
        self.save_resume();
        let Some(path) = self.queue.get(self.queue_pos).cloned() else {
            return;
        };
        let path_str = path.to_string_lossy().to_string();
        // Gespeicherte Resume-Position – nur für Lang-Inhalte (s. should_resume).
        let track = self.library.track_by_path(&path_str).ok().flatten();
        let resume_ms = match &track {
            Some(t) if self.should_resume(t) => t.resume_ms,
            _ => 0,
        };
        match self.player.play_file(&path_str, resume_ms) {
            Ok(()) => {
                self.playing_path = Some(path.clone());
                self.now_playing = Some(Self::track_display_name(&path));
                self.playing = true;
                // Aktiven Ausgang (kann sich geändert haben) auffrischen.
                self.active_output = crate::core::output::default_output().unwrap_or_default();
                self.apply_current_eq();
                // Play-/Queue-Markierungen in der Liste an den neuen Titel anpassen.
                self.refresh_queue_icons();
            }
            Err(e) => tracing::error!("Wiedergabe fehlgeschlagen: {e}"),
        }
    }

    /// Löst den Equalizer für den laufenden Titel + aktiven Ausgang auf
    /// (Titel→Album→Interpret→Global, dann Standard-Ausgang) und setzt ihn live.
    /// Ohne Festlegung: neutral (alle Bänder 0).
    fn apply_current_eq(&self) {
        let Some(path) = self.queue.get(self.queue_pos) else {
            return;
        };
        let (artist, album) = match scanner::read_track(path) {
            Ok(t) => (t.artist, t.album),
            Err(_) => (None, None),
        };
        let bands = self
            .library
            .resolve_eq(
                &self.active_output,
                artist.as_deref(),
                album.as_deref(),
                &path.to_string_lossy(),
            )
            .unwrap_or([0.0; 10]);
        self.player.set_eq_bands(&bands);
    }

    /// Spielt einen Pfad ab (Ordner rekursiv bzw. Einzeldatei) als neue Queue.
    fn play_path(&mut self, path: &str, is_dir: bool) {
        let p = PathBuf::from(path);
        let files = if is_dir {
            scanner::collect_audio_files(&p)
        } else {
            vec![p]
        };
        if !files.is_empty() {
            self.queue = files;
            self.queue_pos = 0;
            self.play_current();
            self.refresh_queue_icons();
        }
    }

    /// Lädt die markierten Konzerte aus der DB und baut die Liste neu auf.
    fn load_concerts(&mut self, sender: &ComponentSender<Self>) {
        self.concert_items = self.library.concerts().unwrap_or_default();

        while let Some(child) = self.concerts_list.first_child() {
            self.concerts_list.remove(&child);
        }
        for (i, (_, title, is_dir)) in self.concert_items.iter().enumerate() {
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(title))
                .subtitle(if *is_dir { "Album" } else { "Datei" })
                .activatable(true)
                .build();
            let icon = if *is_dir {
                "folder-symbolic"
            } else {
                "audio-x-generic-symbolic"
            };
            row.add_prefix(&gtk::Image::from_icon_name(icon));

            // Entfernen-Knopf (Markierung löschen, Dateien bleiben unberührt).
            let remove = gtk::Button::builder()
                .icon_name("user-trash-symbolic")
                .tooltip_text("Konzert entfernen")
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .build();
            {
                let sender = sender.clone();
                remove.connect_clicked(move |_| sender.input(Msg::ConcertRemove(i)));
            }
            row.add_suffix(&remove);
            row.add_suffix(&gtk::Image::from_icon_name("media-playback-start-symbolic"));

            {
                let sender = sender.clone();
                row.connect_activated(move |_| sender.input(Msg::PlayConcert(i)));
            }

            // Langes Drücken: Detailansicht – wie unter „Dateisystem".
            let long_press = gtk::GestureLongPress::new();
            {
                let sender = sender.clone();
                long_press.connect_pressed(move |gesture, _, _| {
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                    sender.input(Msg::ShowConcertDetail(i));
                });
            }
            row.add_controller(long_press);

            self.concerts_list.append(&row);
        }
    }

    /// Import-Dialog: Liste der Kandidaten zum Markieren + „Hinzufügen".
    fn open_concert_import_dialog(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
        candidates: Vec<crate::core::concert::Candidate>,
    ) {
        use std::rc::Rc;

        let dialog = adw::Dialog::builder()
            .title("Konzerte importieren")
            .content_width(440)
            .content_height(560)
            .build();

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(6)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();

        // Alle-auswählen-Schalter
        let all_group = adw::PreferencesGroup::new();
        let all = adw::SwitchRow::builder()
            .title("Alle auswählen")
            .active(true)
            .build();
        all_group.add(&all);
        content.append(&all_group);

        // Kandidaten
        let group = adw::PreferencesGroup::builder()
            .title(format!("{} Kandidaten", candidates.len()))
            .build();
        let mut rows = Vec::with_capacity(candidates.len());
        for c in candidates {
            let row = adw::SwitchRow::builder()
                .title(gtk::glib::markup_escape_text(&c.title))
                .subtitle(gtk::glib::markup_escape_text(&c.subtitle))
                .active(true)
                .build();
            group.add(&row);
            rows.push((c, row));
        }
        content.append(&group);
        let rows = Rc::new(rows);

        {
            let rows = rows.clone();
            all.connect_active_notify(move |s| {
                let on = s.is_active();
                for (_, r) in rows.iter() {
                    r.set_active(on);
                }
            });
        }

        let add = gtk::Button::builder()
            .label("Hinzufügen")
            .css_classes(["suggested-action", "pill"])
            .hexpand(true)
            .build();
        {
            let rows = rows.clone();
            let sender = sender.clone();
            let dialog = dialog.clone();
            add.connect_clicked(move |_| {
                let selected: Vec<(String, String, bool)> = rows
                    .iter()
                    .filter(|(_, r)| r.is_active())
                    .map(|(c, _)| (c.path.clone(), c.title.clone(), c.is_dir))
                    .collect();
                sender.input(Msg::ConcertAdd(selected));
                dialog.close();
            });
        }

        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&content)
            .build();
        let bottom = gtk::Box::builder()
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(12)
            .margin_end(12)
            .build();
        bottom.append(&add);

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&scroller));
        toolbar.add_bottom_bar(&bottom);
        dialog.set_child(Some(&toolbar));
        dialog.present(Some(root));
    }

    /// Nur nach oben, solange wir innerhalb des Startordners bleiben.
    fn can_go_up(&self) -> bool {
        match (&self.browse_dir, &self.root_dir) {
            (Some(cur), Some(root)) => cur != root && cur.starts_with(root),
            _ => false,
        }
    }

    /// Beschriftung der Pfadleiste (aktueller Ordnername bzw. Hinweis).
    fn folder_label(&self) -> String {
        match &self.browse_dir {
            Some(dir) => dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("/")
                .to_string(),
            None => "Kein Musikordner – bitte in den Einstellungen festlegen".to_string(),
        }
    }

    /// Öffnet den Einstellungsdialog (u. a. Musikordner festlegen).
    fn open_settings(&self, root: &adw::ApplicationWindow, sender: &ComponentSender<Self>) {
        let dialog = adw::PreferencesDialog::new();
        let page = adw::PreferencesPage::builder()
            .title("Einstellungen")
            .icon_name("emblem-system-symbolic")
            .build();
        let group = adw::PreferencesGroup::builder()
            .title("Bibliothek")
            .description("Startordner für die Dateisystem-Ansicht")
            .build();

        let current = self.music_dir.as_deref().unwrap_or("Nicht festgelegt");
        let row = adw::ActionRow::builder()
            .title("Musikordner")
            .subtitle(gtk::glib::markup_escape_text(current))
            .subtitle_lines(2)
            .build();

        let button = gtk::Button::builder()
            .icon_name("folder-open-symbolic")
            .tooltip_text("Ordner wählen")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();

        {
            let sender = sender.clone();
            let win = root.clone();
            let row = row.clone();
            button.connect_clicked(move |_| {
                let chooser = gtk::FileDialog::builder()
                    .title("Musikordner wählen")
                    .build();
                let sender = sender.clone();
                let row = row.clone();
                chooser.select_folder(Some(&win), gtk::gio::Cancellable::NONE, move |res| {
                    if let Ok(folder) = res {
                        if let Some(path) = folder.path() {
                            row.set_subtitle(&gtk::glib::markup_escape_text(
                                &path.to_string_lossy(),
                            ));
                            sender.input(Msg::SetMusicDir(path));
                        }
                    }
                });
            });
        }

        row.add_suffix(&button);
        row.set_activatable_widget(Some(&button));
        group.add(&row);
        page.add(&group);

        // Globaler Equalizer (Basis für alles ohne eigene Interpret-/Album-/Titel-EQ).
        let eq_group = adw::PreferencesGroup::builder()
            .title("Equalizer")
            .description(
                "Globale Klangregelung. Sie gilt überall, sofern nicht für einen \
                 Interpreten, ein Album oder einen Titel eine eigene Einstellung gesetzt ist.",
            )
            .build();
        let eq_row = adw::ActionRow::builder()
            .title("Globaler Equalizer")
            .subtitle("Zehn Bänder, je Ausgang")
            .activatable(true)
            .build();
        eq_row.add_prefix(&gtk::Image::from_icon_name("preferences-other-symbolic"));
        eq_row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        {
            let sender = sender.clone();
            eq_row.connect_activated(move |_| sender.input(Msg::OpenGlobalEq));
        }
        eq_group.add(&eq_row);
        page.add(&eq_group);

        // Online-Erkennung: AcoustID-Key für die Titel-Erkennung per Fingerprint.
        let online_group = adw::PreferencesGroup::builder()
            .title("Online-Erkennung")
            .description(
                "Optionaler AcoustID-Key für die Titel-Erkennung per Fingerprint \
                 (kostenlos unter acoustid.org/new-application). Cover & Künstlerfotos \
                 funktionieren ohne Key.",
            )
            .build();
        let key_row = adw::EntryRow::builder().title("AcoustID API-Key").build();
        key_row.set_text(self.acoustid_key.as_deref().unwrap_or(""));
        key_row.set_show_apply_button(true);
        {
            let sender = sender.clone();
            key_row.connect_apply(move |r| {
                sender.input(Msg::SetAcoustidKey(r.text().to_string()));
            });
        }
        online_group.add(&key_row);

        let fanart_row = adw::EntryRow::builder()
            .title("fanart.tv API-Key (optional, für mehrere Interpreten-Fotos)")
            .build();
        fanart_row.set_text(self.fanart_key.as_deref().unwrap_or(""));
        fanart_row.set_show_apply_button(true);
        {
            let sender = sender.clone();
            fanart_row.connect_apply(move |r| {
                sender.input(Msg::SetFanartKey(r.text().to_string()));
            });
        }
        online_group.add(&fanart_row);

        let auto_row = adw::SwitchRow::builder()
            .title("Automatisch abrufen (nur WLAN)")
            .subtitle(
                "Beim Start fehlende Cover, Fotos & Titel im Hintergrund laden – \
                 nur bei nicht-getakteter Verbindung",
            )
            .active(self.auto_enrich)
            .build();
        {
            let sender = sender.clone();
            auto_row.connect_active_notify(move |r| {
                sender.input(Msg::SetAutoEnrich(r.is_active()));
            });
        }
        online_group.add(&auto_row);
        page.add(&online_group);

        // Bereiche: ausgeblendete Navigationspunkte wieder einblenden.
        let sections_group = adw::PreferencesGroup::builder().title("Bereiche").build();
        let concerts_row = adw::SwitchRow::builder()
            .title("Konzerte anzeigen")
            .subtitle("Menüpunkt „Konzerte“ in der Navigation")
            .active(!self.concerts_hidden)
            .build();
        {
            let sender = sender.clone();
            concerts_row.connect_active_notify(move |r| {
                sender.input(Msg::SetConcertsVisible(r.is_active()));
            });
        }
        sections_group.add(&concerts_row);
        page.add(&sections_group);

        dialog.add(&page);
        dialog.present(Some(root));
    }
}
