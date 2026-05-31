use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use adw::prelude::*;
use relm4::factory::FactoryVecDeque;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::core::db::Library;
use crate::core::player::Player;
use crate::core::scanner;
use crate::model::{AlbumMeta, ArtistMeta, Track};
use crate::ui::album_row::{AlbumCard, AlbumOutput};
use crate::ui::artist_row::{ArtistCard, ArtistOutput};
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
    // Playlisten
    pub(crate) playlist_items: Vec<(i64, String, i64)>,
    pub(crate) playlists_list: gtk::ListBox,
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
    // Playlisten
    /// „Neue Playlist"-Dialog öffnen.
    PlaylistNew,
    /// Playlist mit diesem Namen anlegen.
    PlaylistCreate(String),
    /// Playlist anlegen und die aktuellen Kontext-Dateien hinzufügen.
    PlaylistCreateAddTo(String),
    /// Titel-Unterseite einer Playlist öffnen.
    OpenPlaylist(i64),
    /// Ganze Playlist abspielen.
    PlayPlaylist(i64),
    /// Playlist löschen.
    PlaylistDelete(i64),
    /// Aktuelle Kontext-Dateien zu dieser Playlist hinzufügen.
    PlaylistAddTo(i64),
    /// Einen Titel aus einer Playlist abspielen (Queue = ganze Playlist).
    PlaylistTrack { id: i64, path: String },
    /// Einen Titel aus einer Playlist entfernen.
    PlaylistRemoveTrack { id: i64, path: String },
    /// Umbenennen-Dialog einer Playlist öffnen.
    PlaylistRenameDialog(i64),
    /// Playlist umbenennen.
    PlaylistRename { id: i64, name: String },
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
                                &gtk::Box {
                                    set_orientation: gtk::Orientation::Vertical,

                                    gtk::Box {
                                        set_halign: gtk::Align::Center,
                                        set_margin_top: 12,
                                        set_margin_bottom: 6,
                                        gtk::Button {
                                            set_label: "Neue Playlist",
                                            set_css_classes: &["suggested-action", "pill"],
                                            connect_clicked => Msg::PlaylistNew,
                                        },
                                    },

                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: !model.playlist_items.is_empty(),
                                        #[local_ref]
                                        playlists_list -> gtk::ListBox {
                                            set_valign: gtk::Align::Start,
                                            set_margin_top: 0,
                                            set_margin_bottom: 12,
                                            set_margin_start: 12,
                                            set_margin_end: 12,
                                            set_css_classes: &["boxed-list"],
                                        },
                                    },

                                    adw::StatusPage {
                                        set_icon_name: Some("view-list-symbolic"),
                                        set_title: "Keine Playlisten",
                                        set_description: Some("Erstelle eine Playlist oder füge Titel über die Optionen hinzu."),
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.playlist_items.is_empty(),
                                    },
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
        let playlists_list = gtk::ListBox::new();

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
            playlist_items: Vec::new(),
            playlists_list: playlists_list.clone(),
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
        model.reload_playlists(&sender);
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
            Msg::CtxAddPlaylist => self.open_add_to_playlist_dialog(root, &sender),
            Msg::PlaylistNew => self.open_new_playlist_dialog(root, &sender),
            Msg::PlaylistCreate(name) => {
                let name = name.trim();
                if !name.is_empty() {
                    let _ = self.library.create_playlist(name);
                    self.reload_playlists(&sender);
                    self.toast("Playlist erstellt");
                }
            }
            Msg::PlaylistCreateAddTo(name) => {
                let name = name.trim();
                if !name.is_empty() {
                    if let Ok(id) = self.library.create_playlist(name) {
                        self.add_context_to_playlist(id, &sender);
                    }
                }
            }
            Msg::PlaylistAddTo(id) => self.add_context_to_playlist(id, &sender),
            Msg::OpenPlaylist(id) => {
                if let Some((_, name, _)) =
                    self.playlist_items.iter().find(|(pid, _, _)| *pid == id).cloned()
                {
                    self.open_playlist(&sender, id, &name);
                }
            }
            Msg::PlayPlaylist(id) => {
                let paths = self.library.playlist_paths(id).unwrap_or_default();
                if !paths.is_empty() {
                    self.queue = paths.into_iter().map(PathBuf::from).collect();
                    self.queue_pos = 0;
                    self.play_current();
                    self.refresh_queue_icons();
                }
            }
            Msg::PlaylistTrack { id, path } => {
                let queue: Vec<PathBuf> = self
                    .library
                    .playlist_paths(id)
                    .unwrap_or_default()
                    .into_iter()
                    .map(PathBuf::from)
                    .collect();
                if let Some(pos) = queue.iter().position(|p| p.to_string_lossy() == path) {
                    self.queue = queue;
                    self.queue_pos = pos;
                    self.play_current();
                    self.refresh_queue_icons();
                }
            }
            Msg::PlaylistDelete(id) => {
                let _ = self.library.delete_playlist(id);
                self.reload_playlists(&sender);
                self.toast("Playlist gelöscht");
            }
            Msg::PlaylistRemoveTrack { id, path } => {
                let _ = self.library.remove_from_playlist(id, &path);
                self.reload_playlists(&sender);
                // Unterseite neu aufbauen (alte ersetzen).
                self.nav_view.pop();
                if let Some((_, name, _)) =
                    self.playlist_items.iter().find(|(pid, _, _)| *pid == id).cloned()
                {
                    self.open_playlist(&sender, id, &name);
                }
            }
            Msg::PlaylistRenameDialog(id) => self.open_rename_playlist_dialog(root, &sender, id),
            Msg::PlaylistRename { id, name } => {
                let name = name.trim();
                if !name.is_empty() {
                    let _ = self.library.rename_playlist(id, name);
                    self.reload_playlists(&sender);
                }
            }
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
pub(crate) fn most_common_artist(tracks: &[Track]) -> String {
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
pub(crate) fn album_subtitle(year: Option<i32>, track_count: usize) -> String {
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
pub(crate) fn duration_label(ms: i64) -> gtk::Label {
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
pub(crate) fn find_scroller(widget: &gtk::Widget) -> Option<gtk::ScrolledWindow> {
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

pub(crate) fn cover_widget(path: Option<&str>, placeholder: &str) -> gtk::Widget {
    let texture = path.and_then(crate::ui::widgets::thumb_cached);
    crate::ui::widgets::rounded_image(texture.as_ref(), placeholder, 48)
}

/// Liest Unterordner und Audiodateien eines Ordners (Ordner zuerst, sortiert).
/// Läuft im Hintergrund-Thread – darf daher blockieren.
pub(crate) fn read_entries(dir: PathBuf) -> Vec<FsEntry> {
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
