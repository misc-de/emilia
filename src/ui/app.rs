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
use crate::core::{cover, scanner};
use crate::model::{AlbumMeta, ArtistMeta, Track};
use crate::ui::album_row::{AlbumCard, AlbumOutput};
use crate::ui::artist_row::{ArtistCard, ArtistOutput};
use crate::ui::enrich::enrich_worker;
use crate::ui::fs_row::{FsEntry, FsOutput, FsRow, RowOpts};

/// Ziel der Detailansicht (langes Drücken): eine Datei/ein Ordner im
/// Dateibrowser, ein Interpret, ein Album oder ein Konzert (= Pfad → `Fs`).
#[derive(Clone)]
pub(crate) enum CtxTarget {
    Fs(FsEntry),
    Artist(ArtistMeta),
    Album(AlbumMeta),
}

impl CtxTarget {
    /// Überschrift des Detaildialogs.
    pub(crate) fn heading(&self) -> String {
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
pub(crate) enum FsKind {
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
pub(crate) const RESUME_MIN_DURATION_MS: i64 = 15 * 60 * 1000;
/// Vor dieser Position wird kein Resume gemerkt (zu nah am Anfang).
const RESUME_MIN_POS_MS: i64 = 5_000;
/// So nah vor dem Ende gilt der Titel als fertig → Resume auf 0 zurücksetzen.
const RESUME_END_GUARD_MS: i64 = 10_000;

/// Resume-Position mit Wächtern: nahe Anfang oder Ende wird auf 0 gesetzt,
/// damit ein quasi fertiger Titel beim nächsten Mal von vorn beginnt.
pub(crate) fn guarded_resume(pos_ms: i64, dur_ms: i64) -> i64 {
    if pos_ms < RESUME_MIN_POS_MS {
        0
    } else if dur_ms > 0 && pos_ms > dur_ms - RESUME_END_GUARD_MS {
        0
    } else {
        pos_ms
    }
}

pub struct App {
    pub(crate) library: Library,
    pub(crate) player: Player,
    /// Sperrbildschirm-/Medientasten-Steuerung (MPRIS, optional).
    pub(crate) mpris: crate::core::mpris::Mpris,
    pub(crate) entries: FactoryVecDeque<FsRow>,
    pub(crate) albums: FactoryVecDeque<AlbumCard>,
    pub(crate) album_count: usize,
    pub(crate) artists: FactoryVecDeque<ArtistCard>,
    pub(crate) artist_count: usize,
    pub(crate) enriching: bool,
    pub(crate) enrich_status: String,
    /// Cover & Metadaten beim Start automatisch online nachladen (nur bei
    /// nicht-getakteter Verbindung; in den Einstellungen abschaltbar).
    pub(crate) auto_enrich: bool,
    /// Fortschritts-Leiste vom Nutzer ausgeblendet? (Abruf läuft im Hintergrund weiter.)
    pub(crate) enrich_banner_hidden: bool,
    /// Abbruch-Flag für den Anreicherungs-Worker.
    pub(crate) enrich_cancel: Arc<AtomicBool>,
    pub(crate) acoustid_key: Option<String>,
    pub(crate) fanart_key: Option<String>,
    /// Aktuell aktiver Audio-Ausgang (PipeWire-Sink), für die EQ-Auflösung.
    pub(crate) active_output: String,
    pub(crate) music_dir: Option<String>,
    pub(crate) root_dir: Option<PathBuf>,
    pub(crate) browse_dir: Option<PathBuf>,
    /// Aktuell im Dateibrowser angezeigter Ordner (für das Merken der Scrollposition).
    pub(crate) shown_dir: Option<PathBuf>,
    /// Gemerkte Scrollpositionen je Ordner im Dateibrowser, damit man beim
    /// Zurücknavigieren wieder auf gleicher Höhe landet.
    pub(crate) fs_scroll: std::rc::Rc<std::cell::RefCell<std::collections::HashMap<PathBuf, f64>>>,
    pub(crate) loading: bool,
    pub(crate) queue: Vec<PathBuf>,
    pub(crate) queue_pos: usize,
    /// Pfad des aktuell in den Player geladenen Titels (für das Sichern der
    /// Resume-Position beim Wechsel auf einen anderen Titel).
    pub(crate) playing_path: Option<PathBuf>,
    /// Schnappschuss (Pfad, Position, Dauer) des laufenden Resume-Titels, vom
    /// 1-s-Tick aktualisiert. Wird beim Schließen einmalig in die DB geschrieben,
    /// damit beim harten Beenden höchstens ~1 s Hörposition verloren geht.
    pub(crate) close_resume: std::rc::Rc<std::cell::RefCell<Option<(String, i64, i64)>>>,
    pub(crate) now_playing: Option<String>,
    pub(crate) playing: bool,
    /// Aktuelle Position und Gesamtdauer des laufenden Titels (ms) – für die
    /// Seekleiste im Mini-Player.
    pub(crate) position_ms: i64,
    pub(crate) track_duration_ms: i64,
    pub(crate) shuffle: bool,
    pub(crate) context_target: Option<CtxTarget>,
    pub(crate) toast_overlay: adw::ToastOverlay,
    // Konzerte
    pub(crate) concert_items: Vec<(String, String, bool)>,
    pub(crate) concerts_list: gtk::ListBox,
    pub(crate) concert_hint_dismissed: bool,
    pub(crate) concerts_hidden: bool,
    pub(crate) concert_nav_buttons: Vec<gtk::ToggleButton>,
    pub(crate) view_stack: adw::ViewStack,
    /// Navigations-Container für die Unterseiten (Interpret → Alben → Album).
    pub(crate) nav_view: adw::NavigationView,
    /// Gemerkte Scrollposition der zuletzt verlassenen Übersichtsseite
    /// (Scroller + Wert), um sie beim Zurücknavigieren wiederherzustellen.
    pub(crate) overview_scroll: std::rc::Rc<std::cell::RefCell<Option<(gtk::ScrolledWindow, f64)>>>,
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
    /// Befehl vom Sperrbildschirm / von Medientasten (MPRIS).
    Mpris(crate::core::mpris::MprisCommand),
    /// 1-s-Tick: Position/Dauer der Seekleiste aktualisieren.
    Tick,
    /// Sprung an eine Position (ms) durch Ziehen/Klicken der Seekleiste.
    Seek(i64),
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

                        // Seekleiste: Position / Regler / Gesamtdauer.
                        gtk::Box {
                            set_spacing: 6,
                            set_margin_start: 4,
                            set_margin_end: 4,
                            #[watch]
                            set_visible: model.now_playing.is_some(),

                            gtk::Label {
                                add_css_class: "caption",
                                add_css_class: "numeric",
                                #[watch]
                                set_label: &fmt_duration(model.position_ms),
                            },
                            #[name = "seek_scale"]
                            gtk::Scale {
                                set_orientation: gtk::Orientation::Horizontal,
                                set_hexpand: true,
                                set_draw_value: false,
                                set_valign: gtk::Align::Center,
                                #[watch]
                                set_range: (0.0, model.track_duration_ms.max(1000) as f64),
                                #[watch]
                                set_value: model.position_ms as f64,
                            },
                            gtk::Label {
                                add_css_class: "caption",
                                add_css_class: "numeric",
                                #[watch]
                                set_label: &fmt_duration(model.track_duration_ms),
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

        // Sekündlicher Tick für die Seekleiste (Position/Dauer aktualisieren).
        {
            let sender = sender.clone();
            gtk::glib::timeout_add_seconds_local(1, move || {
                sender.input(Msg::Tick);
                gtk::glib::ControlFlow::Continue
            });
        }

        // MPRIS-Dienst starten: Befehle vom Sperrbildschirm/von Medientasten
        // werden als Msg::Mpris in die Komponente eingespeist.
        let mpris = crate::core::mpris::Mpris::start({
            let sender = sender.clone();
            move |cmd| sender.input(Msg::Mpris(cmd))
        });

        let toast_overlay = adw::ToastOverlay::new();
        let concerts_list = gtk::ListBox::new();

        let mut model = App {
            library,
            player,
            mpris,
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
            close_resume: std::rc::Rc::new(std::cell::RefCell::new(None)),
            now_playing: None,
            playing: false,
            position_ms: 0,
            track_duration_ms: 0,
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

        // Seekleiste: Ziehen/Klicken springt im laufenden Titel an die Position.
        // `change-value` feuert nur bei Nutzer-Interaktion (nicht beim
        // programmatischen `set_value` des Ticks), darum gibt es kein Zerren.
        {
            let sender = sender.clone();
            widgets.seek_scale.connect_change_value(move |_, _, value| {
                sender.input(Msg::Seek(value as i64));
                gtk::glib::Propagation::Proceed
            });
        }

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
        let close_resume = model.close_resume.clone();
        root.connect_close_request(move |win| {
            // Letzte Hörposition sichern (deckt den Spalt zum 5-s-Speichern).
            if let Some((path, pos, dur)) = close_resume.borrow().clone() {
                if let Ok(lib) = Library::open() {
                    let _ = lib.set_resume_path(&path, guarded_resume(pos, dur));
                }
            }
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
                            self.mpris.set_playing(self.playing);
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
                *self.close_resume.borrow_mut() = None;
                self.play_next();
            }
            Msg::PersistResume => {
                if self.playing {
                    self.save_resume();
                    if let Some(pos) = self.player.position_ms() {
                        self.mpris.set_position(pos);
                    }
                }
            }
            Msg::Tick => {
                if self.playing {
                    if let Some(pos) = self.player.position_ms() {
                        self.position_ms = pos;
                    }
                    if let Some(dur) = self.player.duration_ms() {
                        self.track_duration_ms = dur;
                    }
                    // Close-Schnappschuss mitführen.
                    if let Some(entry) = self.close_resume.borrow_mut().as_mut() {
                        entry.1 = self.position_ms;
                        entry.2 = self.track_duration_ms;
                    }
                }
            }
            Msg::Seek(ms) => {
                let ms = ms.max(0);
                self.position_ms = ms;
                if self.player.seek_ms(ms).is_ok() {
                    self.mpris.seeked(ms);
                }
            }
            Msg::Mpris(cmd) => {
                use crate::core::mpris::MprisCommand as M;
                match cmd {
                    M::PlayPause => {
                        if self.now_playing.is_some() {
                            if self.playing {
                                self.save_resume();
                                self.player.pause();
                            } else {
                                self.player.resume();
                            }
                            self.playing = !self.playing;
                            self.mpris.set_playing(self.playing);
                            self.refresh_queue_icons();
                        }
                    }
                    M::Play => {
                        if self.now_playing.is_some() && !self.playing {
                            self.player.resume();
                            self.playing = true;
                            self.mpris.set_playing(true);
                            self.refresh_queue_icons();
                        }
                    }
                    M::Pause => {
                        if self.now_playing.is_some() && self.playing {
                            self.save_resume();
                            self.player.pause();
                            self.playing = false;
                            self.mpris.set_playing(false);
                            self.refresh_queue_icons();
                        }
                    }
                    M::Next => self.play_next(),
                    M::Prev => self.play_prev(),
                    M::Stop => {
                        self.save_resume();
                        self.player.stop();
                        self.playing = false;
                        self.playing_path = None;
                        self.position_ms = 0;
                        self.track_duration_ms = 0;
                        *self.close_resume.borrow_mut() = None;
                        self.mpris.set_stopped();
                        self.refresh_queue_icons();
                    }
                    M::Raise => root.present(),
                    M::SeekBy(offset_us) => {
                        let cur = self.player.position_ms().unwrap_or(0);
                        let target = (cur + offset_us / 1000).max(0);
                        if self.player.seek_ms(target).is_ok() {
                            self.mpris.seeked(target);
                        }
                    }
                    M::SetPosition(pos_us) => {
                        let target = (pos_us / 1000).max(0);
                        if self.player.seek_ms(target).is_ok() {
                            self.mpris.seeked(target);
                        }
                    }
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
                self.mpris.set_playing(self.playing);
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

/// Formatiert Millisekunden als `m:ss` bzw. `h:mm:ss` (Negatives → 0).
pub(crate) fn fmt_duration(ms: i64) -> String {
    let secs = ms.max(0) / 1000;
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

impl App {
    /// Scroller der Dateiliste (Vorfahre der Einträge-`ListBox`).
    pub(crate) fn fs_scroller(&self) -> Option<gtk::ScrolledWindow> {
        self.entries
            .widget()
            .ancestor(gtk::ScrolledWindow::static_type())
            .and_downcast::<gtk::ScrolledWindow>()
    }

    /// Startet das Einlesen des aktuellen Ordners im Hintergrund (mit Spinner).
    pub(crate) fn load_dir(&mut self, sender: &ComponentSender<Self>) {
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
    pub(crate) fn reload_albums(&mut self) {
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
    pub(crate) fn start_scan(&self, sender: &ComponentSender<Self>, then_enrich: bool) {
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
    pub(crate) fn run_enrich(&mut self, sender: &ComponentSender<Self>, scan_first: bool) {
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
    pub(crate) fn reload_artists(&mut self) {
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
    pub(crate) fn entry_files(&self, entry: &FsEntry) -> Vec<PathBuf> {
        if entry.is_dir() {
            scanner::collect_audio_files(entry.path())
        } else {
            vec![entry.path().clone()]
        }
    }

    /// Alle Dateien eines Interpreten (aus der Bibliothek), in Abspielreihenfolge.
    pub(crate) fn artist_files(&self, name: &str) -> Vec<PathBuf> {
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
    pub(crate) fn album_files(&self, artist: &str, album: &str) -> Vec<PathBuf> {
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
    pub(crate) fn artist_albums(&self, name: &str) -> Vec<(String, Vec<Track>)> {
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
    pub(crate) fn artist_sections(&self, name: &str) -> (Vec<(String, String, Vec<Track>)>, Vec<Track>) {
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
    pub(crate) fn album_tracks_for_artist(&self, name: &str, album: &str) -> Vec<Track> {
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
    pub(crate) fn push_subpage(&self, title: &str, content: &gtk::Box) {
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
    pub(crate) fn open_artist_tracks(&self, sender: &ComponentSender<Self>, meta: &ArtistMeta) {
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
    pub(crate) fn open_album_tracks(&self, sender: &ComponentSender<Self>, name: &str, album: &str) {
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

        // Vorhandene Discs ermitteln (None gilt als CD 1). Mehr als eine → die
        // Titel werden nach „CD 1" / „CD 2" … getrennt dargestellt.
        let mut discs: Vec<u32> = tracks.iter().map(|t| t.disc_no.unwrap_or(1)).collect();
        discs.sort_unstable();
        discs.dedup();
        let multi_disc = discs.len() > 1;

        // Baut eine Titelzeile (Cover, Tracknummer, Dauer, Play + Gesten).
        let make_row = |t: &Track| -> adw::ActionRow {
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
            row
        };

        if multi_disc {
            for (i, disc) in discs.iter().enumerate() {
                let group = adw::PreferencesGroup::builder()
                    .title(format!("CD {disc}"))
                    .build();
                // Album-Jahr/Titelzahl als Untertitel der ersten Disc-Gruppe.
                if i == 0 {
                    group.set_description(Some(
                        album_subtitle(year, tracks.len()).as_str(),
                    ));
                }
                for t in tracks.iter().filter(|t| t.disc_no.unwrap_or(1) == *disc) {
                    group.add(&make_row(t));
                }
                content.append(&group);
            }
        } else {
            let group = adw::PreferencesGroup::builder()
                .title(gtk::glib::markup_escape_text(album))
                .description(album_subtitle(year, tracks.len()))
                .build();
            for t in &tracks {
                group.add(&make_row(t));
            }
            content.append(&group);
        }

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
    pub(crate) fn ctx_files(&self, target: &CtxTarget) -> Vec<PathBuf> {
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
    pub(crate) fn fs_music_kind(&self, entry: &FsEntry) -> Option<FsKind> {
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
    pub(crate) fn fs_eq_level(
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
    pub(crate) fn ctx_album(&self) -> Option<(String, String)> {
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
    pub(crate) fn ctx_artist(&self) -> Option<String> {
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
    pub(crate) fn artist_albums_dated(&self, name: &str) -> Vec<(Option<i32>, String, Vec<Track>)> {
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
    pub(crate) fn artist_files_ordered(&self, name: &str, newest_first: bool) -> Vec<PathBuf> {
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
    pub(crate) fn artist_year_range(&self, name: &str) -> Option<(&'static str, String)> {
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

    pub(crate) fn ctx_cover(&self, target: &CtxTarget) -> (Option<gtk::gdk::Texture>, &'static str) {
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
    pub(crate) fn append_cover_or_gallery(
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
    pub(crate) fn ctx_gallery_paths(&self, entry: &CtxTarget) -> Vec<String> {
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
    pub(crate) fn ctx_info_lines(&self, target: &CtxTarget) -> Vec<(&'static str, String)> {
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
    pub(crate) fn ctx_merkmale(
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
    pub(crate) fn artist_merkmale(
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
    pub(crate) fn album_merkmale(
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
    pub(crate) fn files_summary(files: &[PathBuf], with_year: bool) -> String {
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

    pub(crate) fn toast(&self, msg: &str) {
        self.toast_overlay.add_toast(adw::Toast::new(msg));
    }

    /// Beschafft ein Cover als Textur. Für einen **Ordner** das Ordner-Cover
    /// (= Albumbild); für eine **Einzeldatei** bewusst **kein** Ordnerbild, damit
    /// ein Titel kein fremdes Cover aus einem geteilten Ordner erbt – stattdessen
    /// das eingebettete Bild der Datei bzw. das online zugeordnete Album-Cover.
    /// `None`, wenn nichts Passendes gefunden wird.
    pub(crate) fn cover_texture(&self, entry: &FsEntry) -> Option<gtk::gdk::Texture> {
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

    /// Detailzeilen für die "Mehr Infos"-Aufklappung.
    pub(crate) fn info_lines(&self, entry: &FsEntry) -> Vec<(&'static str, String)> {
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

    /// Nur nach oben, solange wir innerhalb des Startordners bleiben.
    pub(crate) fn can_go_up(&self) -> bool {
        match (&self.browse_dir, &self.root_dir) {
            (Some(cur), Some(root)) => cur != root && cur.starts_with(root),
            _ => false,
        }
    }

    /// Beschriftung der Pfadleiste (aktueller Ordnername bzw. Hinweis).
    pub(crate) fn folder_label(&self) -> String {
        match &self.browse_dir {
            Some(dir) => dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("/")
                .to_string(),
            None => "Kein Musikordner – bitte in den Einstellungen festlegen".to_string(),
        }
    }

}

#[cfg(test)]
mod tests {
    use super::{fmt_duration, guarded_resume};

    #[test]
    fn guarded_resume_clamps_start_and_end() {
        let dur = 3_600_000; // 1 h
        // Mittendrin → unverändert.
        assert_eq!(guarded_resume(1_000_000, dur), 1_000_000);
        // Nahe Anfang (< 5 s) → 0.
        assert_eq!(guarded_resume(3_000, dur), 0);
        // Nahe Ende (< 10 s Rest) → 0 (nächstes Mal von vorn).
        assert_eq!(guarded_resume(dur - 5_000, dur), 0);
        // Unbekannte Dauer (0) → keine Ende-Prüfung, Position bleibt.
        assert_eq!(guarded_resume(1_000_000, 0), 1_000_000);
    }

    #[test]
    fn fmt_duration_formats_minutes_and_hours() {
        assert_eq!(fmt_duration(0), "0:00");
        assert_eq!(fmt_duration(5_000), "0:05");
        assert_eq!(fmt_duration(65_000), "1:05");
        assert_eq!(fmt_duration(600_000), "10:00");
        // Hörspiel-Längen mit Stunden.
        assert_eq!(fmt_duration(3_661_000), "1:01:01");
        // Negativwerte werden auf 0 geklemmt.
        assert_eq!(fmt_duration(-1), "0:00");
    }
}
