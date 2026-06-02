use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use adw::prelude::*;
use relm4::factory::FactoryVecDeque;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::core::db::Library;
use crate::core::player::Player;
use crate::core::scanner;
use crate::i18n::{gettext, gettext_f, ngettext_n};
use crate::model::{AlbumMeta, ArtistMeta, Track};
use crate::ui::album_row::{AlbumCard, AlbumOutput};
use crate::ui::app_podcast::fetch_and_store_podcast;
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

/// Navigationsbereiche: (Stack-Name, Tooltip, Icon). Die **Standard**-Reihenfolge;
/// die tatsächliche Anzeige-/Menüreihenfolge ist in `section_order` gespeichert
/// und vom Nutzer verschiebbar.
// Die Labels sind englische gettext-`msgid`; am Anzeigeort mit `gettext()`
// übersetzen (siehe Nutzung in `build_nav` / `win_title`).
pub(crate) const SECTIONS: [(&str, &str, &str); 9] = [
    ("favorites", "Favorites", "emilia-favorite-symbolic"),
    ("files", "Files", "folder-symbolic"),
    ("artists", "Artists", "avatar-default-symbolic"),
    ("albums", "Albums", "media-optical-symbolic"),
    ("concerts", "Concerts", "emilia-concert-symbolic"),
    ("podcasts", "Podcasts", "microphone-symbolic"),
    ("audiobooks", "Audiobooks", "emilia-audiobook-symbolic"),
    ("playlists", "Playlists", "view-list-symbolic"),
    ("stats", "Statistics", "emilia-stats-symbolic"),
];

/// Liefert (Tooltip/Label als msgid, Icon) eines Bereichs anhand seines
/// Stack-Namens. Das Label am Anzeigeort mit `gettext()` übersetzen.
pub(crate) fn section_meta(name: &str) -> Option<(&'static str, &'static str)> {
    SECTIONS
        .iter()
        .find(|(n, _, _)| *n == name)
        .map(|(_, label, icon)| (*label, *icon))
}

/// Sicherheitsabfrage vor destruktiven Aktionen (Löschen/Entfernen). Zeigt einen
/// Bestätigungsdialog relativ zu `parent` (irgendein Widget im Fenster) und
/// sendet `msg` erst nach Bestätigung. `confirm_label` beschriftet den
/// (destruktiven) Bestätigungsknopf, z. B. `gettext("Delete")` / `gettext("Remove")`.
pub(crate) fn confirm_destructive(
    parent: &impl IsA<gtk::Widget>,
    heading: &str,
    confirm_label: &str,
    sender: ComponentSender<App>,
    msg: Msg,
) {
    let confirm = adw::AlertDialog::new(Some(heading), None);
    confirm.add_response("cancel", &gettext("Cancel"));
    confirm.add_response("ok", confirm_label);
    confirm.set_response_appearance("ok", adw::ResponseAppearance::Destructive);
    confirm.set_default_response(Some("cancel"));
    confirm.set_close_response("cancel");
    // `connect_response` ist `Fn`; die Nachricht daher nur einmalig entnehmen.
    let msg = std::cell::RefCell::new(Some(msg));
    confirm.connect_response(None, move |_, resp| {
        if resp == "ok" {
            if let Some(m) = msg.borrow_mut().take() {
                sender.input(m);
            }
        }
    });
    confirm.present(Some(parent));
}

/// Vor dieser Position wird kein Resume gemerkt (zu nah am Anfang).
const RESUME_MIN_POS_MS: i64 = 5_000;
/// So nah vor dem Ende gilt der Titel als fertig → Resume auf 0 zurücksetzen.
const RESUME_END_GUARD_MS: i64 = 10_000;

/// Resume-Position mit Wächtern: nahe Anfang oder Ende wird auf 0 gesetzt,
/// damit ein quasi fertiger Titel beim nächsten Mal von vorn beginnt.
/// Aktuelle Zeit in Unix-Sekunden (für die Hörstatistik-Zeitstempel).
pub(crate) fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Wendet das Farbschema („system"/„dark"/„light") über den globalen
/// libadwaita-StyleManager an. „system" folgt der Desktop-Einstellung.
pub(crate) fn apply_color_scheme(code: &str) {
    let scheme = match code {
        "dark" => adw::ColorScheme::ForceDark,
        "light" => adw::ColorScheme::ForceLight,
        _ => adw::ColorScheme::Default,
    };
    adw::StyleManager::default().set_color_scheme(scheme);
}

pub(crate) fn guarded_resume(pos_ms: i64, dur_ms: i64) -> i64 {
    if pos_ms < RESUME_MIN_POS_MS {
        0
    } else if dur_ms > 0 && pos_ms > dur_ms - RESUME_END_GUARD_MS {
        0
    } else {
        pos_ms
    }
}

/// Welche Ansicht die Podcast-Seite zeigt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PodcastView {
    /// Neueste Episoden (Beiträge) über alle Abos hinweg.
    Newest,
    /// Übersicht der abonnierten Podcasts.
    Overview,
}

/// Zeitraum der Hörstatistik. Bewusst gleitende Fenster (statt Kalenderjahr) –
/// kalenderfrei und ohne zusätzliche Datums-Abhängigkeit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StatsPeriod {
    /// Letzte 4 Wochen.
    Weeks4,
    /// Letzte 12 Monate.
    Year,
    /// Seit Beginn.
    All,
}

/// Laufende Hör-Sitzung eines Titels. Beim Wechsel/Ende wird sie als **ein**
/// `play_event` in die Statistik geschrieben (siehe `finalize_play_session`).
/// Rein lokal – verlässt das Gerät nie.
pub(crate) struct PlaySession {
    pub(crate) path: PathBuf,
    /// Startzeitpunkt (Unix-Sekunden).
    pub(crate) started_at: i64,
    /// Tatsächlich gehörte Zeit (vom 1-s-Tick, nur während „Playing" gezählt).
    pub(crate) played_ms: i64,
    /// Schnappschuss der Titellänge (0 = noch unbekannt → bei Tick nachgezogen).
    pub(crate) duration_ms: i64,
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
    /// Anzeigesprache: "system" (System-Locale), "de" oder "en". In den
    /// Einstellungen umschaltbar; greift nach einem Neustart der App.
    pub(crate) ui_language: String,
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
    /// Zufalls-Reihenfolge der Queue-Indizes (Fisher-Yates), wird bei aktivem
    /// Zufall durchlaufen, damit jeder Titel **genau einmal** an die Reihe kommt.
    pub(crate) shuffle_order: Vec<usize>,
    /// Position innerhalb von `shuffle_order`.
    pub(crate) shuffle_idx: usize,
    /// Zuletzt gespielte Titel (für „vorheriges Lied" per Doppelklick auf Zurück).
    pub(crate) play_history: Vec<PathBuf>,
    /// Beim Zurückspringen aus der History nicht erneut in die History schreiben.
    pub(crate) skip_history_push: bool,
    /// Zeitpunkt des letzten Zurück-Klicks (Doppelklick-Erkennung, < 1 s).
    pub(crate) last_prev: Option<std::time::Instant>,
    /// Pausierte Warteschlange, während ein einzelnes Lied dazwischengespielt
    /// wird (Liste + Position). Nach dem Einzellied wird sie fortgesetzt.
    pub(crate) interrupted_queue: Option<(Vec<PathBuf>, usize)>,
    /// Zurück-Stapel verdrängter Wiedergabe-Kontexte (Queue + Position). Wird
    /// gefüllt, wenn eine neue Auswahl die laufende Warteschlange ersetzt, und
    /// erlaubt „voriges Lied **inkl. Playlist** weiterhören" (Zurück-Taste).
    pub(crate) nav_stack: Vec<(Vec<PathBuf>, usize)>,
    /// Zuletzt von `play_current` gespielter Kontext (zur Erkennung, ob die
    /// Warteschlange durch eine neue Auswahl ersetzt wurde).
    pub(crate) prev_ctx: Option<(Vec<PathBuf>, usize)>,
    /// Pfad des aktuell in den Player geladenen Titels (für das Sichern der
    /// Resume-Position beim Wechsel auf einen anderen Titel).
    pub(crate) playing_path: Option<PathBuf>,
    /// Schnappschuss (Pfad, Position, Dauer) des laufenden Resume-Titels, vom
    /// 1-s-Tick aktualisiert. Wird beim Schließen einmalig in die DB geschrieben,
    /// damit beim harten Beenden höchstens ~1 s Hörposition verloren geht.
    pub(crate) close_resume: std::rc::Rc<std::cell::RefCell<Option<(String, i64, i64)>>>,
    /// Laufende Hör-Sitzung für die Statistik (siehe [`PlaySession`]).
    pub(crate) play_session: Option<PlaySession>,
    /// Schnappschuss der Sitzung fürs Schließen (Pfad, Start, gehört, Dauer) –
    /// analog `close_resume`, damit beim harten Beenden das letzte Ereignis
    /// nicht verloren geht.
    pub(crate) close_session: std::rc::Rc<std::cell::RefCell<Option<(String, i64, i64, i64)>>>,
    pub(crate) now_playing: Option<String>,
    pub(crate) playing: bool,
    /// Aktuelle Position und Gesamtdauer des laufenden Titels (ms) – für die
    /// Seekleiste im Mini-Player.
    pub(crate) position_ms: i64,
    pub(crate) track_duration_ms: i64,
    pub(crate) shuffle: bool,
    /// Wiederholen: am Ende der Warteschlange bzw. des Einzeltitels von vorn.
    pub(crate) repeat: bool,
    pub(crate) context_target: Option<CtxTarget>,
    /// Play-Zeile des offenen Detail-Dialogs samt zugehörigem Titel-Pfad. Wird
    /// ausgeblendet, solange genau dieser Titel läuft, und wieder eingeblendet,
    /// sobald er endet (siehe `refresh_ctx_play`).
    pub(crate) ctx_play: std::rc::Rc<std::cell::RefCell<Option<(adw::ActionRow, PathBuf)>>>,
    pub(crate) toast_overlay: adw::ToastOverlay,
    // Konzerte
    // Konzerte/Hörbücher: (scope, key, Titel, is_dir) – wie Favoriten.
    pub(crate) concert_items: Vec<(String, String, String, bool)>,
    pub(crate) concerts_list: gtk::ListBox,
    pub(crate) concert_hint_dismissed: bool,
    /// Ausgeblendete Navigations-Menüpunkte (Stack-Namen). Betrifft sowohl die
    /// Navigation als auch die Auswahl in den Eigenschaften.
    pub(crate) hidden_sections: std::collections::HashSet<String>,
    /// Anzeige-Reihenfolge der Menüpunkte (Stack-Namen). Vom Nutzer verschiebbar.
    pub(crate) section_order: Vec<&'static str>,
    /// Alle Navigations-Schaltflächen je Menüpunkt mit Container-Kennung
    /// (`true` = Seitenleiste, `false` = obere Leiste) – zum Ein-/Ausblenden und
    /// Umsortieren zur Laufzeit.
    pub(crate) nav_buttons: Vec<(&'static str, bool, gtk::ToggleButton)>,
    /// Navigations-Container (Seitenleiste, obere Leiste) zum Umsortieren.
    pub(crate) sidebar_nav: gtk::Box,
    pub(crate) top_nav: gtk::Box,
    /// Haupt-Splitview – eingeklappt (`is_collapsed`) bedeutet schmaler/mobiler
    /// Modus; danach richten sich z. B. die Detail-Dialoge (volle Breite).
    pub(crate) split: adw::OverlaySplitView,
    // Favoriten: (scope, key, Titel, is_dir)
    pub(crate) favorite_items: Vec<(String, String, String, bool)>,
    pub(crate) favorites_list: gtk::ListBox,
    // Hörbücher: (Pfad, Titel, is_dir)
    pub(crate) audiobook_items: Vec<(String, String, String, bool)>,
    pub(crate) audiobooks_list: gtk::ListBox,
    // Playlisten
    pub(crate) playlist_items: Vec<(i64, String, i64)>,
    pub(crate) playlists_list: gtk::ListBox,
    // Podcasts: (id, Titel, Bild-URL, Episodenzahl)
    pub(crate) podcast_items: Vec<(i64, String, Option<String>, i64)>,
    pub(crate) podcasts_list: gtk::ListBox,
    /// Welche Podcast-Ansicht sichtbar ist: neueste Episoden oder Abo-Übersicht.
    pub(crate) podcast_view: PodcastView,
    /// Neueste Episoden über alle Abos (für die „Neuste"-Ansicht).
    pub(crate) newest_items: Vec<crate::model::EpisodeRef>,
    /// Container der „Neuste"-Liste: je Zeitabschnitt (Heute/Gestern/…) eine
    /// eigene Gruppe, imperativ in `reload_newest` befüllt.
    pub(crate) newest_list: gtk::Box,
    /// Treffer der letzten Podcast-Suche (iTunes), für den Abo-Dialog.
    pub(crate) podcast_search_results: Vec<crate::core::podcast::PodcastSearchResult>,
    /// Solange der Abo-Such-Dialog offen ist: (Dialog, Trefferliste). Damit lassen
    /// sich asynchron eintreffende Treffer in die bereits gezeigte Liste einfügen.
    pub(crate) podcast_search:
        std::rc::Rc<std::cell::RefCell<Option<(adw::Dialog, gtk::ListBox)>>>,
    /// URL der aktuell geladenen Podcast-Episode (für die Play/Pause-Markierung
    /// der Beitragszeilen); `None`, wenn gerade Musik bzw. keine Episode läuft.
    pub(crate) playing_episode_url: Option<String>,
    /// Play/Pause-Knöpfe der sichtbaren Beitragszeilen (Audio-URL → Knopf), um ihr
    /// Icon beim Wechsel des Wiedergabestands aufzufrischen. Abgehängte (tote)
    /// Einträge werden beim Auffrischen verworfen.
    pub(crate) episode_play_buttons:
        std::rc::Rc<std::cell::RefCell<Vec<(String, gtk::Button)>>>,
    /// „Abspielen"-Zeile eines offenen Beitrag-Detaildialogs (Zeile, Audio-URL) –
    /// wird ausgeblendet, solange genau diese Episode läuft.
    pub(crate) ctx_episode_play:
        std::rc::Rc<std::cell::RefCell<Option<(adw::ActionRow, String)>>>,
    /// Liste im Warteschlangen-Dialog (wird bei Änderungen neu aufgebaut).
    pub(crate) queue_list: gtk::ListBox,
    /// Inhalt der Statistik-Seite (imperativ befüllt, wie die Listen oben).
    pub(crate) stats_box: gtk::Box,
    /// Aktuell gewählter Zeitraum der Hörstatistik.
    pub(crate) stats_period: StatsPeriod,
    pub(crate) view_stack: adw::ViewStack,
    /// Seekleiste des Mini-Players (für Kapitelmarken via `add_mark`).
    pub(crate) seek_scale: gtk::Scale,
    /// Label unter dem Titel, das beim Überfahren der Seekleiste die Bezeichnung
    /// des Kapitels an der Mausposition zeigt (imperativ gesteuert).
    pub(crate) chapter_label: gtk::Label,
    /// Kapitel (Zeit + Bezeichnung) der laufenden Episode – mit dem Hover-
    /// Controller der Seekleiste geteilt.
    pub(crate) chapters: std::rc::Rc<std::cell::RefCell<Vec<(i64, String)>>>,
    /// Wird gerade über die Seekleiste gefahren? Dann zeigt das Label temporär
    /// das überfahrene Kapitel; sonst läuft es mit der Wiedergabeposition mit.
    pub(crate) hovering_seek: std::rc::Rc<std::cell::Cell<bool>>,
    /// Navigations-Container für die Unterseiten (Interpret → Alben → Album).
    pub(crate) nav_view: adw::NavigationView,
    /// Gemerkte Scrollposition der zuletzt verlassenen Übersichtsseite
    /// (Scroller + Wert), um sie beim Zurücknavigieren wiederherzustellen.
    pub(crate) overview_scroll: std::rc::Rc<std::cell::RefCell<Option<(gtk::ScrolledWindow, f64)>>>,
    /// Zustand der Geräte-Synchronisierung (Server/Client + Dialog-Widgets).
    pub(crate) sync: crate::ui::app_sync::SyncState,
    /// Ob aktuell ein Gerät gekoppelt ist – steuert das grüne Sync-Icon oben.
    pub(crate) sync_connected: bool,
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
    /// Wie `PlayAlbumTrack`, aber interpretenübergreifend (Alben-Übersicht):
    /// Queue = alle Titel des Albumnamens.
    PlayAlbumByNameTrack { album: String, path: String },
    /// Tippen auf einen Album-/Ordner-Eintrag in Konzerten/Hörbüchern: dessen
    /// Titel als Unterseite auflisten (statt direkt abzuspielen).
    OpenEntryTracks { scope: String, key: String },
    /// Einen Titel eines Ordner-Hörbuchs/-Konzerts abspielen (Queue = Ordner in
    /// Reihenfolge, Start beim getippten).
    PlayFolderTrack { folder: String, path: String },
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
    // --- Geräte-Synchronisierung (erreichbar über „Teilen") ---
    /// Server-Modus starten (QR-Code anzeigen, auf Kopplung warten).
    SyncStartServer,
    /// Client-Modus starten (Webcam-Scan).
    SyncStartScan,
    /// Ein QR-Code wurde dekodiert (URL als Text).
    SyncQrDecoded(String),
    /// Der Sync-Dialog wurde geschlossen – Server/Kamera aufräumen.
    SyncDialogClosed,
    TrackFinished,
    /// Zeitraum der Hörstatistik umschalten.
    SetStatsPeriod(StatsPeriod),
    /// Statistik-Seite neu aufbauen (z. B. beim Öffnen des Bereichs).
    RefreshStats,
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
    ToggleRepeat,
    NavUp,
    FilesGoStart,
    Refresh,
    /// Fortschritts-Leiste ausblenden (der Abruf läuft im Hintergrund weiter).
    HideEnrichBanner,
    TogglePlay,
    /// Detailansicht des gerade laufenden Titels öffnen (Klick auf die Leiste).
    OpenNowPlaying,
    OpenSettings,
    /// Auf eine neuere Flatpak-Version prüfen (nur als Flatpak; im Hintergrund).
    CheckForUpdates,
    /// Gefundene Aktualisierung über das Flatpak-Portal einspielen.
    InstallFlatpakUpdate,
    /// Ergebnis der Flatpak-Aktualisierung (`Ok` = fertig, Neustart nötig).
    FlatpakUpdateFinished(Result<(), String>),
    OpenGlobalEq,
    /// Equalizer für den gerade laufenden Titel öffnen.
    OpenCurrentEq,
    /// Warteschlangen-Dialog öffnen.
    ShowQueue,
    /// Einen Eintrag aus der Warteschlange entfernen (Queue-Index).
    QueueRemove(usize),
    /// Die gesamte Warteschlange leeren (nach Rückfrage) und Wiedergabe stoppen.
    QueueClear,
    /// Einen Warteschlangen-Eintrag verschieben (Queue-Indizes).
    QueueMove { from: usize, to: usize },
    SetMusicDir(PathBuf),
    SetAcoustidKey(String),
    /// Primäres Cover eines Albums festlegen (zuletzt im Galerie-Karussell gezeigt).
    SetAlbumCover { artist: String, album: String, path: String },
    /// Primäres Foto eines Interpreten festlegen (zuletzt im Galerie-Karussell gezeigt).
    SetArtistImage { name: String, path: String },
    /// Eigenes Cover/Foto für das aktuelle Detailziel hochladen (Dateidialog).
    UploadCover,
    SetFanartKey(String),
    /// Automatischen Online-Abruf an-/ausschalten.
    SetAutoEnrich(bool),
    /// Anzeigesprache umstellen ("system"/"de"/"en"); startet die App neu.
    SetLanguage(String),
    /// Farbschema umstellen ("system"/"dark"/"light"); greift sofort.
    SetColorScheme(String),
    /// Merkmal einer Ebene setzen (oder bei `None` auf „erben" zurücksetzen).
    /// Bereiche (Eigenschaften) einer Ebene setzen; leerer Wert = ausgeblendet.
    SetAreas {
        scope: &'static str,
        key: String,
        value: String,
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
    /// Einen Navigations-Menüpunkt ein-/ausblenden (Stack-Name).
    SetSectionVisible {
        section: &'static str,
        visible: bool,
    },
    /// Menüpunkt in der Reihenfolge verschieben (Indizes in `section_order`).
    MoveSection {
        from: usize,
        to: usize,
    },
    /// Einen ausgeblendeten Inhalt wieder einblenden (Festlegung zurücksetzen).
    UnhideEntry {
        scope: String,
        key: String,
    },
    // Favoriten
    /// Aktuelles Detailziel als Favorit setzen/entfernen.
    ToggleFavorite,
    /// Favorit (Index in `favorite_items`) abspielen.
    PlayFavorite(usize),
    /// Detailansicht eines Favoriten öffnen.
    ShowFavoriteDetail(usize),
    /// Favoriten umsortieren (Indizes in `favorite_items`).
    MoveFavorite { from: usize, to: usize },
    // Hörbücher
    /// Hörbuch (Index in `audiobook_items`) abspielen.
    PlayAudiobook(usize),
    /// Detailansicht eines Hörbuchs öffnen.
    ShowAudiobookDetail(usize),
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
    // Podcasts
    /// Abo-Dialog (Suche + Feed-Adresse) öffnen.
    PodcastSubscribe,
    /// Podcasts zu diesem Suchbegriff suchen (iTunes-Verzeichnis, im Hintergrund).
    PodcastSearch(String),
    /// Feed unter dieser Adresse abonnieren (im Hintergrund holen).
    PodcastSubscribeUrl(String),
    /// Episoden-Unterseite eines Podcasts öffnen.
    OpenPodcast(i64),
    /// Podcast entfernen.
    PodcastDelete(i64),
    /// Feed eines Podcasts neu laden.
    PodcastRefresh(i64),
    /// Beitrag (Episode) umschalten: starten bzw. – wenn schon die laufende –
    /// pausieren/fortsetzen. Vom Antippen der Zeile und vom Play/Pause-Knopf.
    ToggleEpisode { url: String, title: String },
    /// Podcast-Ansicht umschalten (Neuste / Übersicht).
    SetPodcastView(PodcastView),
    /// Detailansicht eines Beitrags (Episode) aus der „Neuste"-Liste (Index).
    ShowEpisodeDetail(usize),
    /// Detailansicht einer Episode aus der Episodenliste eines Podcasts.
    ShowPodcastEpisodeDetail { podcast_id: i64, index: usize },
    /// Klick auf eine Zeitsprungmarke in den Shownotes: an die Stelle springen
    /// (Episode bei Bedarf dort starten).
    EpisodeSeekTo { url: String, title: String, ms: i64 },
    /// Detailansicht/Verwaltung eines Abos (Podcast-Id) – Aktualisieren/Entfernen.
    ShowPodcastDetail(i64),
}

/// Ergebnisse der Hintergrund-Worker (Ordner lesen bzw. Online-Anreicherung).
#[derive(Debug)]
pub enum Cmd {
    Entries(Vec<FsEntry>),
    /// Fortschritt einer Anreicherungsphase (`phase` = Anzeigetext).
    EnrichProgress { phase: String, done: usize, total: usize },
    /// Online-Anreicherung abgeschlossen.
    EnrichDone,
    /// Zwischenstand: Alben-/Interpreten-Ansicht neu laden (z. B. nach einer Phase).
    ReloadViews,
    /// Lokaler Bibliotheks-Scan fertig; `then_enrich` = danach ggf. online nachladen.
    ScanDone { then_enrich: bool },
    /// Gefundene Konzert-Kandidaten (für den Import-Dialog).
    Candidates(Vec<crate::core::concert::Candidate>),
    /// Podcast-Feed geholt: `Some(Titel)` bei Erfolg, sonst `None`.
    PodcastFetched(Option<String>),
    /// Treffer der Podcast-Suche (für den offenen Abo-Dialog).
    PodcastSearchResults(Vec<crate::core::podcast::PodcastSearchResult>),
    /// Cover-Thumbnails der Suchtreffer sind gecacht → Trefferliste neu zeichnen.
    PodcastSearchCoversReady,
    /// Podcast-Liste neu aufbauen (z. B. nachdem Feed-Bilder gecacht wurden).
    ReloadPodcasts,
    /// Ereignis aus dem Sync-Server-Thread bzw. Client-Worker.
    Sync(crate::core::sync::SyncEvent),
    /// Ergebnis der Update-Prüfung (im Hintergrund ermittelt).
    UpdateChecked(crate::core::update::CheckResult),
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
                set_min_sidebar_width: 180.0,
                set_max_sidebar_width: 240.0,

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
                        set_margin_bottom: 8,
                        set_margin_start: 6,
                        set_margin_end: 6,
                        set_halign: gtk::Align::Fill,
                        // Volle Höhe, damit „Einstellungen" ganz unten sitzt.
                        set_valign: gtk::Align::Fill,
                        set_vexpand: true,
                    },
                },

                #[wrap(Some)]
                #[name = "content_view"]
                set_content = &adw::ToolbarView {
                    // Desktop: etwas Luft **oben** über dem Inhalt (nicht links –
                    // der Inhalt schließt bündig an die Seitenleiste an). Im
                    // schmalen (mobilen) Modus per Breakpoint wieder auf 0 (siehe `init`).
                    set_margin_top: 20,
                    add_top_bar = &adw::HeaderBar {
                        #[wrap(Some)]
                        #[name = "win_title"]
                        set_title_widget = &adw::WindowTitle::new("Emilia", ""),
                        // Einstellungen oben nur im schmalen (mobilen) Modus – im
                        // Desktop-Modus sitzt der Punkt unten in der Seitenleiste.
                        #[name = "settings_top_btn"]
                        pack_start = &gtk::Button {
                            set_icon_name: "emblem-system-symbolic",
                            set_tooltip_text: Some(&gettext("Settings")),
                            set_visible: false,
                            connect_clicked => Msg::OpenSettings,
                        },
                        pack_start = &gtk::Button {
                            set_icon_name: "view-refresh-symbolic",
                            set_tooltip_text: Some(&gettext("Rescan folder")),
                            connect_clicked => Msg::Refresh,
                        },
                        // Geräte-Synchronisierung: öffnet denselben „Teilen"-Dialog
                        // wie die Aktion im Detailmenü (kein eigenes Popover). Bei
                        // bestehender Kopplung wird das Icon grün dargestellt
                        // (CSS-Klasse, siehe unten).
                        #[name = "sync_btn"]
                        pack_start = &gtk::Button {
                            set_icon_name: "emilia-share-symbolic",
                            set_tooltip_text: Some(&gettext("Share")),
                            connect_clicked => Msg::CtxShare,
                            #[watch]
                            set_css_classes: if model.sync_connected {
                                &["sync-connected"]
                            } else {
                                &[]
                            },
                        },
                    },

                    // Top-Navigation (icon-only) – nur im schmalen (mobilen) Modus
                    #[name = "top_nav"]
                    add_top_bar = &gtk::Box {
                        set_halign: gtk::Align::Center,
                        set_spacing: 6,
                        set_visible: false,
                        set_margin_top: 2,
                        set_margin_bottom: 2,
                    },

                    // Globaler Fortschritt der Online-Anreicherung – die graue Leiste
                    // auf allen Seiten. Der Nutzer kann sie ausblenden (der Abruf
                    // läuft im Hintergrund weiter; Abbrechen über den Header-Knopf).
                    add_top_bar = &adw::Banner {
                        #[watch]
                        set_revealed: model.enriching && !model.enrich_banner_hidden,
                        #[watch]
                        set_title: &model.enrich_status,
                        set_button_label: Some(&gettext("Hide")),
                        connect_button_clicked => Msg::HideEnrichBanner,
                    },

                    // Inhalt mit Lade-Overlay
                    #[wrap(Some)]
                    set_content = &gtk::Overlay {
                        #[wrap(Some)]
                        #[name = "view_stack"]
                        set_child = &adw::ViewStack {
                            add_titled_with_icon[Some("files"), &gettext("Files"), "folder-symbolic"] =
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
                                                set_tooltip_text: Some(&gettext("Back")),
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
                            add_titled_with_icon[Some("artists"), &gettext("Artists"), "avatar-default-symbolic"] =
                                &gtk::Box {
                                    set_orientation: gtk::Orientation::Vertical,

                                    adw::Banner {
                                        set_visible: false,
                                        #[watch]
                                        set_title: &model.enrich_status,
                                    },

                                    adw::StatusPage {
                                        set_icon_name: Some("avatar-default-symbolic"),
                                        set_title: &gettext("No artists"),
                                        set_description: Some(
                                            &gettext("Scan a music folder and fetch online metadata"),
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
                            add_titled_with_icon[Some("albums"), &gettext("Albums"), "media-optical-symbolic"] =
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
                                        set_title: &gettext("No albums"),
                                        set_description: Some(
                                            &gettext("Scan a music folder and fetch online metadata"),
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
                            add_titled_with_icon[Some("concerts"), &gettext("Concerts"), "emilia-concert-symbolic"] =
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
                                        set_title: &gettext("Concerts"),
                                        set_description: Some(&gettext("Here you can list your collected concerts. Via Import concerts you get an overview of likely concerts: albums with live, unplugged or concert in the name, plus single files of 30 minutes or more. Mark them as a concert and they'll appear here. You can also add concerts later at any time via the options.")),
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.concert_items.is_empty() && !model.concert_hint_dismissed,
                                        #[wrap(Some)]
                                        set_child = &gtk::Box {
                                            set_orientation: gtk::Orientation::Vertical,
                                            set_spacing: 8,
                                            set_halign: gtk::Align::Center,
                                            gtk::Button {
                                                set_label: &gettext("Import concerts"),
                                                set_css_classes: &["suggested-action", "pill"],
                                                connect_clicked => Msg::ConcertImport,
                                            },
                                            gtk::Button {
                                                set_label: &gettext("I'll do it myself"),
                                                add_css_class: "pill",
                                                connect_clicked => Msg::ConcertDismissHint,
                                            },
                                            gtk::Button {
                                                set_label: &gettext("Hide menu item"),
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
                                        set_title: &gettext("No concerts"),
                                        set_description: Some(&gettext("Mark an album or a track as a concert via the options.")),
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.concert_items.is_empty() && model.concert_hint_dismissed,
                                    },
                                },
                            add_titled_with_icon[Some("playlists"), &gettext("Playlists"), "view-list-symbolic"] =
                                &gtk::Box {
                                    set_orientation: gtk::Orientation::Vertical,

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
                                        set_title: &gettext("No playlists"),
                                        set_description: Some(&gettext("Create a playlist or add tracks via the options.")),
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.playlist_items.is_empty(),
                                    },

                                    // Aktion ganz unten.
                                    gtk::Box {
                                        set_halign: gtk::Align::Center,
                                        set_margin_top: 6,
                                        set_margin_bottom: 10,
                                        gtk::Button {
                                            set_label: &gettext("New playlist"),
                                            set_css_classes: &["suggested-action", "pill"],
                                            connect_clicked => Msg::PlaylistNew,
                                        },
                                    },
                                },
                            add_titled_with_icon[Some("podcasts"), &gettext("Podcasts"), "microphone-symbolic"] =
                                &gtk::Box {
                                    set_orientation: gtk::Orientation::Vertical,

                                    // Kopf: Umschalter „Neuste" / „Übersicht" und „+" zum Abonnieren.
                                    gtk::Box {
                                        set_orientation: gtk::Orientation::Horizontal,
                                        set_spacing: 6,
                                        set_margin_top: 2,
                                        // Etwas (knappe) Luft unter den Schaltern; die erste
                                        // Abschnitts-Überschrift sitzt so ~10px höher.
                                        set_margin_bottom: 4,
                                        set_margin_start: 12,
                                        set_margin_end: 12,

                                        gtk::ToggleButton {
                                            set_label: &gettext("Newest"),
                                            set_hexpand: true,
                                            #[watch]
                                            set_active: model.podcast_view == PodcastView::Newest,
                                            connect_clicked => Msg::SetPodcastView(PodcastView::Newest),
                                        },
                                        gtk::ToggleButton {
                                            set_label: &gettext("Overview"),
                                            set_hexpand: true,
                                            #[watch]
                                            set_active: model.podcast_view == PodcastView::Overview,
                                            connect_clicked => Msg::SetPodcastView(PodcastView::Overview),
                                        },
                                        gtk::Button {
                                            set_icon_name: "list-add-symbolic",
                                            set_tooltip_text: Some(&gettext("Subscribe to podcast")),
                                            add_css_class: "flat",
                                            connect_clicked => Msg::PodcastSubscribe,
                                        },
                                    },

                                    // „Neuste": neueste Episoden über alle Abos.
                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.podcast_view == PodcastView::Newest && !model.newest_items.is_empty(),
                                        #[local_ref]
                                        newest_list -> gtk::Box {
                                            set_orientation: gtk::Orientation::Vertical,
                                            set_spacing: 6,
                                            set_valign: gtk::Align::Start,
                                            // Erste Überschrift dichter an die Umschalter (≈10px höher).
                                            set_margin_top: 0,
                                            set_margin_bottom: 12,
                                            set_margin_start: 12,
                                            set_margin_end: 12,
                                        },
                                    },
                                    adw::StatusPage {
                                        set_icon_name: Some("microphone-symbolic"),
                                        set_title: &gettext("No episodes"),
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.podcast_view == PodcastView::Newest && model.newest_items.is_empty(),
                                    },

                                    // „Übersicht": abonnierte Podcasts.
                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.podcast_view == PodcastView::Overview && !model.podcast_items.is_empty(),
                                        #[local_ref]
                                        podcasts_list -> gtk::ListBox {
                                            set_valign: gtk::Align::Start,
                                            // 10px Abstand nach unten zum Content (nicht am Umschalter kleben).
                                            set_margin_top: 10,
                                            set_margin_bottom: 12,
                                            set_margin_start: 12,
                                            set_margin_end: 12,
                                            set_css_classes: &["boxed-list"],
                                        },
                                    },
                                    adw::StatusPage {
                                        set_icon_name: Some("microphone-symbolic"),
                                        set_title: &gettext("No podcasts"),
                                        set_description: Some(&gettext("Subscribe to a podcast via its feed address (RSS).")),
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.podcast_view == PodcastView::Overview && model.podcast_items.is_empty(),
                                    },
                                },
                            add_titled_with_icon[Some("favorites"), &gettext("Favorites"), "emilia-favorite-symbolic"] =
                                &gtk::Box {
                                    set_orientation: gtk::Orientation::Vertical,

                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: !model.favorite_items.is_empty(),
                                        #[local_ref]
                                        favorites_list -> gtk::ListBox {
                                            set_valign: gtk::Align::Start,
                                            set_margin_top: 0,
                                            set_margin_bottom: 12,
                                            set_margin_start: 12,
                                            set_margin_end: 12,
                                            set_css_classes: &["boxed-list"],
                                        },
                                    },

                                    adw::StatusPage {
                                        set_icon_name: Some("emilia-favorite-symbolic"),
                                        set_title: &gettext("No favorites"),
                                        set_description: Some(&gettext("Mark tracks, albums or artists with the star under \"More info\".")),
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.favorite_items.is_empty(),
                                    },
                                },
                            add_titled_with_icon[Some("audiobooks"), &gettext("Audiobooks"), "emilia-audiobook-symbolic"] =
                                &gtk::Box {
                                    set_orientation: gtk::Orientation::Vertical,

                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: !model.audiobook_items.is_empty(),
                                        #[local_ref]
                                        audiobooks_list -> gtk::ListBox {
                                            set_valign: gtk::Align::Start,
                                            set_margin_top: 0,
                                            set_margin_bottom: 12,
                                            set_margin_start: 12,
                                            set_margin_end: 12,
                                            set_css_classes: &["boxed-list"],
                                        },
                                    },

                                    adw::StatusPage {
                                        set_icon_name: Some("emilia-audiobook-symbolic"),
                                        set_title: &gettext("No audiobooks"),
                                        set_description: Some(&gettext("Mark albums, folders or tracks as audiobooks via the properties.")),
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.audiobook_items.is_empty(),
                                    },
                                },
                            add_titled_with_icon[Some("stats"), &gettext("Statistics"), "emilia-stats-symbolic"] =
                                &gtk::Box {
                                    set_orientation: gtk::Orientation::Vertical,
                                    // Inhalt wird imperativ in `refresh_stats` befüllt.
                                    #[local_ref]
                                    stats_box -> gtk::Box {
                                        set_orientation: gtk::Orientation::Vertical,
                                        set_vexpand: true,
                                    },
                                },
                        },

                        // Zentrierter Spinner während des Einlesens – auf einer
                        // halbtransparenten Fläche, damit die Schrift über dem
                        // Inhalt lesbar bleibt (CSS-Klasse, siehe `init`).
                        add_overlay = &gtk::Box {
                            set_orientation: gtk::Orientation::Vertical,
                            set_halign: gtk::Align::Center,
                            set_valign: gtk::Align::Center,
                            set_spacing: 12,
                            set_can_target: false,
                            add_css_class: "emilia-loading",
                            #[watch]
                            set_visible: model.loading,

                            gtk::Spinner {
                                set_spinning: true,
                                set_size_request: (48, 48),
                            },
                            gtk::Label {
                                set_label: &gettext("Reading music data"),
                                add_css_class: "dim-label",
                            },
                        },
                    },

                    // Mini-Player unten mit Transport-Steuerung. Die Leiste bleibt
                    // immer sichtbar; ohne ausgewählten Titel werden nur die
                    // Songzeile (Titel + Seekleiste) ausgeblendet und die
                    // Transport-Tasten ausgegraut.
                    add_bottom_bar = &gtk::Box {
                        set_orientation: gtk::Orientation::Vertical,
                        set_spacing: 2,
                        set_margin_top: 4,
                        set_margin_bottom: 6,
                        set_margin_start: 10,
                        set_margin_end: 10,

                        gtk::Button {
                            add_css_class: "flat",
                            set_tooltip_text: Some(&gettext("Details of the current track")),
                            // Ohne ausgewählten Titel ganz ausblenden (gibt Platz frei).
                            #[watch]
                            set_visible: model.now_playing.is_some(),
                            connect_clicked => Msg::OpenNowPlaying,
                            #[wrap(Some)]
                            set_child = &gtk::Label {
                                set_xalign: 0.5,
                                set_justify: gtk::Justification::Center,
                                // Lange Titel auf bis zu 2 Zeilen umbrechen statt die
                                // Leiste zu sprengen; danach mit … kürzen. Die
                                // Breitenbegrenzung verhindert, dass ein langer Titel
                                // die Mindestbreite des Fensters aufbläht.
                                set_wrap: true,
                                set_wrap_mode: gtk::pango::WrapMode::WordChar,
                                set_lines: 2,
                                set_ellipsize: gtk::pango::EllipsizeMode::End,
                                set_max_width_chars: 28,
                                add_css_class: "caption",
                                // Nichts ausgewählt → kein Text (Leiste wirkt inaktiv).
                                #[watch]
                                set_label: model.now_playing.as_deref().unwrap_or(""),
                            },
                        },

                        // Kapitelbezeichnung beim Überfahren der Seekleiste
                        // (imperativ über den Hover-Controller gesteuert).
                        #[name = "chapter_label"]
                        gtk::Label {
                            set_xalign: 0.5,
                            set_ellipsize: gtk::pango::EllipsizeMode::End,
                            set_max_width_chars: 36,
                            set_visible: false,
                            add_css_class: "caption",
                            add_css_class: "dim-label",
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
                            // Links EQ + Zufall, mittig die Transport-Tasten. Die
                            // mittige Gruppe ist symmetrisch (Zurück | Play | Vor),
                            // damit Play/Zurück/Vor unabhängig von EQ/Zufall/
                            // Warteschlange in der **absoluten Mitte** bleiben.
                            #[wrap(Some)]
                            set_start_widget = &gtk::Box {
                                set_spacing: 6,
                                set_valign: gtk::Align::Center,
                                gtk::Button {
                                    set_label: "EQ",
                                    set_tooltip_text: Some(&gettext("Equalizer for this track")),
                                    set_valign: gtk::Align::Center,
                                    add_css_class: "flat",
                                    #[watch]
                                    set_sensitive: model.now_playing.is_some(),
                                    connect_clicked => Msg::OpenCurrentEq,
                                },
                                // Zufall (nur ab 2 Titeln); links bei EQ, damit die
                                // Transport-Mitte nicht verschoben wird.
                                gtk::ToggleButton {
                                    set_icon_name: "media-playlist-shuffle-symbolic",
                                    set_tooltip_text: Some(&gettext("Shuffle")),
                                    set_valign: gtk::Align::Center,
                                    add_css_class: "flat",
                                    #[watch]
                                    set_visible: model.queue.len() >= 2,
                                    #[watch]
                                    set_sensitive: model.now_playing.is_some(),
                                    #[watch]
                                    set_active: model.shuffle,
                                    #[watch]
                                    set_opacity: if model.shuffle { 1.0 } else { 0.4 },
                                    connect_clicked => Msg::ToggleShuffle,
                                },
                            },
                            #[wrap(Some)]
                            set_center_widget = &gtk::Box {
                                set_spacing: 6,
                                gtk::Button {
                                    set_icon_name: "media-skip-backward-symbolic",
                                    set_tooltip_text: Some(&gettext("Back")),
                                    add_css_class: "flat",
                                    // Nichts ausgewählt → ausgegraut.
                                    #[watch]
                                    set_sensitive: model.now_playing.is_some(),
                                    connect_clicked => Msg::Prev,
                                },
                                gtk::Button {
                                    #[watch]
                                    set_icon_name: if model.playing {
                                        "media-playback-pause-symbolic"
                                    } else {
                                        "media-playback-start-symbolic"
                                    },
                                    set_tooltip_text: Some(&gettext("Play/Pause")),
                                    add_css_class: "circular",
                                    // Größer als die übrigen Transport-Tasten
                                    // (Größe via CSS-Klasse, siehe `init`).
                                    add_css_class: "emilia-bigplay",
                                    set_valign: gtk::Align::Center,
                                    #[watch]
                                    set_sensitive: model.now_playing.is_some(),
                                    connect_clicked => Msg::TogglePlay,
                                },
                                gtk::Button {
                                    set_icon_name: "media-skip-forward-symbolic",
                                    set_tooltip_text: Some(&gettext("Forward")),
                                    add_css_class: "flat",
                                    #[watch]
                                    set_sensitive: model.now_playing.is_some(),
                                    connect_clicked => Msg::Next,
                                },
                                // Wiederholen (Loop): am Ende der Warteschlange bzw.
                                // des Einzeltitels von vorn. Aktiv = weiß, aus =
                                // ausgegraut.
                                gtk::ToggleButton {
                                    set_icon_name: "media-playlist-repeat-symbolic",
                                    set_tooltip_text: Some(&gettext("Repeat")),
                                    set_valign: gtk::Align::Center,
                                    add_css_class: "flat",
                                    #[watch]
                                    set_sensitive: model.now_playing.is_some(),
                                    #[watch]
                                    set_active: model.repeat,
                                    #[watch]
                                    set_opacity: if model.repeat { 1.0 } else { 0.4 },
                                    connect_clicked => Msg::ToggleRepeat,
                                },
                            },
                            // Warteschlange (rechts unten).
                            #[wrap(Some)]
                            set_end_widget = &gtk::Button {
                                set_icon_name: "list-high-priority-symbolic",
                                set_tooltip_text: Some(&gettext("Queue")),
                                set_valign: gtk::Align::Center,
                                add_css_class: "flat",
                                #[watch]
                                set_sensitive: model.now_playing.is_some(),
                                connect_clicked => Msg::ShowQueue,
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
            // App-Icon (logo.png unter dem App-Id-Namen) für Fenster/Taskleiste –
            // greift auch ohne installierte .desktop-Datei (z. B. `cargo run`).
            gtk::Window::set_default_icon_name("de.cais.Emilia");

            // Cover/Fotos in Alben-/Interpreten-Liste ganz links (kein Einzug).
            let css = gtk::CssProvider::new();
            css.load_from_string(
                "row.emilia-flush > box.header { padding-left: 0px; margin-left: 0px; }\
                 row.emilia-flush > box.header > box.prefixes { margin-left: 0px; margin-right: 8px; }\
                 button.sync-connected { color: @success_color; }\
                 button.emilia-bigplay { min-width: 46px; min-height: 46px; padding: 0px; }\
                 button.emilia-bigplay image { -gtk-icon-size: 34px; }\
                 box.emilia-loading { background-color: alpha(@window_bg_color, 0.85); border-radius: 18px; padding: 22px 30px; }\
                 progressbar.emilia-hourbar, progressbar.emilia-hourbar > trough, progressbar.emilia-hourbar > trough > progress { min-width: 0px; }",
            );
            gtk::style_context_add_provider_for_display(
                &display,
                &css,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }

        let library = Library::open().expect("Failed to open the library database");
        let player = Player::new().expect("Failed to initialize GStreamer");
        // Farbschema (Standard: System) sofort anwenden.
        apply_color_scheme(
            library
                .get_setting("color_scheme")
                .ok()
                .flatten()
                .as_deref()
                .unwrap_or("system"),
        );
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
        // Ausgeblendete Menüpunkte (kommasepariert). Alter Schlüssel
        // „concerts_hidden=1" wird weiterhin berücksichtigt.
        let mut hidden_sections: std::collections::HashSet<String> = library
            .get_setting("hidden_sections")
            .ok()
            .flatten()
            .map(|s| {
                s.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if matches!(
            library.get_setting("concerts_hidden").ok().flatten().as_deref(),
            Some("1")
        ) {
            hidden_sections.insert("concerts".to_string());
        }
        // Menü-Reihenfolge (kommaseparierte Stack-Namen). Unbekannte Namen werden
        // verworfen, neue Bereiche in Standardreihenfolge hinten ergänzt – so
        // tauchen künftige Menüpunkte automatisch auf.
        let mut section_order: Vec<&'static str> = library
            .get_setting("section_order")
            .ok()
            .flatten()
            .map(|s| {
                s.split(',')
                    .filter_map(|name| {
                        SECTIONS.iter().find(|(n, _, _)| *n == name.trim()).map(|(n, _, _)| *n)
                    })
                    .collect()
            })
            .unwrap_or_default();
        for (name, _, _) in SECTIONS {
            if !section_order.contains(&name) {
                section_order.push(name);
            }
        }
        // Automatischer Online-Abruf (Standard: an; nur „0" schaltet ihn aus).
        let auto_enrich = !matches!(
            library.get_setting("auto_enrich").ok().flatten().as_deref(),
            Some("0")
        );
        // Wiederholen-Zustand (Standard: aus).
        let repeat_on = matches!(
            library.get_setting("repeat").ok().flatten().as_deref(),
            Some("1")
        );
        // Anzeigesprache (Standard: System-Locale). Wirksam wurde sie bereits
        // beim Start in `main` über `i18n::init`; hier nur für die Anzeige im
        // Einstellungs-Umschalter.
        let ui_language = library
            .get_setting("ui_language")
            .ok()
            .flatten()
            .unwrap_or_else(|| "system".to_string());
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
        let podcasts_list = gtk::ListBox::new();
        let newest_list = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let favorites_list = gtk::ListBox::new();
        let audiobooks_list = gtk::ListBox::new();
        let queue_list = gtk::ListBox::new();
        let stats_box = gtk::Box::new(gtk::Orientation::Vertical, 0);

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
            ui_language,
            active_output,
            music_dir,
            root_dir,
            browse_dir,
            shown_dir: None,
            fs_scroll: std::rc::Rc::new(std::cell::RefCell::new(std::collections::HashMap::new())),
            loading: false,
            queue: Vec::new(),
            queue_pos: 0,
            shuffle_order: Vec::new(),
            shuffle_idx: 0,
            play_history: Vec::new(),
            skip_history_push: false,
            last_prev: None,
            interrupted_queue: None,
            nav_stack: Vec::new(),
            prev_ctx: None,
            playing_path: None,
            close_resume: std::rc::Rc::new(std::cell::RefCell::new(None)),
            play_session: None,
            close_session: std::rc::Rc::new(std::cell::RefCell::new(None)),
            now_playing: None,
            playing: false,
            position_ms: 0,
            track_duration_ms: 0,
            shuffle: false,
            repeat: repeat_on,
            context_target: None,
            ctx_play: std::rc::Rc::new(std::cell::RefCell::new(None)),
            toast_overlay: toast_overlay.clone(),
            concert_items: Vec::new(),
            concerts_list: concerts_list.clone(),
            favorite_items: Vec::new(),
            favorites_list: favorites_list.clone(),
            audiobook_items: Vec::new(),
            audiobooks_list: audiobooks_list.clone(),
            playlist_items: Vec::new(),
            playlists_list: playlists_list.clone(),
            podcast_items: Vec::new(),
            podcasts_list: podcasts_list.clone(),
            podcast_view: PodcastView::Newest,
            newest_items: Vec::new(),
            newest_list: newest_list.clone(),
            podcast_search_results: Vec::new(),
            podcast_search: std::rc::Rc::new(std::cell::RefCell::new(None)),
            playing_episode_url: None,
            episode_play_buttons: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
            ctx_episode_play: std::rc::Rc::new(std::cell::RefCell::new(None)),
            queue_list: queue_list.clone(),
            stats_box: stats_box.clone(),
            stats_period: StatsPeriod::All,
            concert_hint_dismissed,
            hidden_sections,
            section_order,
            nav_buttons: Vec::new(),
            sidebar_nav: gtk::Box::new(gtk::Orientation::Vertical, 0),
            top_nav: gtk::Box::new(gtk::Orientation::Horizontal, 0),
            split: adw::OverlaySplitView::new(),
            view_stack: adw::ViewStack::new(),
            seek_scale: gtk::Scale::default(),
            chapter_label: gtk::Label::default(),
            chapters: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
            hovering_seek: std::rc::Rc::new(std::cell::Cell::new(false)),
            nav_view: adw::NavigationView::new(),
            overview_scroll: std::rc::Rc::new(std::cell::RefCell::new(None)),
            sync: crate::ui::app_sync::SyncState::default(),
            sync_connected: false,
        };

        // Warteschlange vom letzten Mal wiederherstellen (nur noch vorhandene
        // Dateien). Es wird **nicht** automatisch abgespielt – der Titel steht
        // bereit im Mini-Player und startet beim Druck auf „Play".
        let saved_pos: usize = model
            .library
            .get_setting("queue_pos")
            .ok()
            .flatten()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let raw_queue: Vec<PathBuf> = model
            .library
            .get_setting("queue_paths")
            .ok()
            .flatten()
            .map(|s| {
                s.split('\n')
                    .filter(|l| !l.is_empty())
                    .map(PathBuf::from)
                    .collect()
            })
            .unwrap_or_default();
        let mut q = Vec::new();
        let mut q_pos = 0usize;
        for (i, p) in raw_queue.iter().enumerate() {
            if p.exists() {
                if i <= saved_pos {
                    q_pos = q.len();
                }
                q.push(p.clone());
            }
        }
        if !q.is_empty() {
            q_pos = q_pos.min(q.len() - 1);
            model.now_playing = Some(Self::track_display_name(&q[q_pos]));
            model.queue = q;
            model.queue_pos = q_pos;
        }

        model.load_dir(&sender);
        model.reload_albums();
        model.reload_artists();
        model.load_concerts(&sender);
        model.load_favorites(&sender);
        model.load_audiobooks(&sender);
        model.reload_playlists(&sender);
        model.reload_podcasts(&sender);
        model.refresh_stats(&sender);
        // Podcast-Feed-Bilder einmalig im Hintergrund cachen, dann die Liste neu
        // aufbauen, damit die Cover erscheinen (kein UI-Block beim Start).
        sender.spawn_oneshot_command(|| {
            if let Ok(lib) = Library::open() {
                for (_, _, image, _) in lib.podcasts().unwrap_or_default() {
                    if let Some(url) = image {
                        crate::core::online::cache_podcast_image(&url);
                    }
                }
            }
            Cmd::ReloadPodcasts
        });
        // Bibliothek beim Start automatisch einlesen und – bei WLAN/LAN und
        // aktiviertem Schalter – fehlende Cover/Metadaten im Hintergrund nachladen.
        model.start_scan(&sender, true);

        let entries_box = model.entries.widget();
        let albums_box = model.albums.widget();
        let artists_box = model.artists.widget();
        let widgets = view_output!();
        model.view_stack = widgets.view_stack.clone();
        model.nav_view = widgets.nav_view.clone();
        model.split = widgets.split.clone();
        model.seek_scale = widgets.seek_scale.clone();
        model.chapter_label = widgets.chapter_label.clone();

        // Hover über die Seekleiste → temporär das überfahrene Kapitel unter dem
        // Titel anzeigen; beim Verlassen zurück auf das aktuelle Kapitel (an der
        // Wiedergabeposition). Aktualisiert nur das Label (kein View-Neuaufbau).
        // Eine kleine Helferfunktion setzt das Label aus einem Zeitwert.
        fn show_chapter_at(
            label: &gtk::Label,
            chapters: &std::cell::RefCell<Vec<(i64, String)>>,
            val_ms: i64,
        ) {
            let chaps = chapters.borrow();
            let name = chaps
                .iter()
                .rev()
                .find(|(ms, _)| *ms <= val_ms)
                .map(|(_, n)| n.clone())
                .filter(|n| !n.is_empty());
            match name {
                Some(n) => {
                    label.set_text(&n);
                    label.set_visible(true);
                }
                None => label.set_visible(false),
            }
        }
        {
            let chapters = model.chapters.clone();
            let hovering = model.hovering_seek.clone();
            let scale = widgets.seek_scale.clone();
            let label = widgets.chapter_label.clone();
            let motion = gtk::EventControllerMotion::new();
            {
                let (chapters, scale, label, hovering) =
                    (chapters.clone(), scale.clone(), label.clone(), hovering.clone());
                motion.connect_motion(move |_, x, _| {
                    if chapters.borrow().is_empty() {
                        return;
                    }
                    let adj = scale.adjustment();
                    let w = scale.width() as f64;
                    let span = adj.upper() - adj.lower();
                    if w <= 0.0 || span <= 0.0 {
                        return;
                    }
                    hovering.set(true);
                    let val = adj.lower() + (x / w).clamp(0.0, 1.0) * span;
                    show_chapter_at(&label, &chapters, val as i64);
                });
            }
            motion.connect_leave(move |_| {
                hovering.set(false);
                // Zurück auf das Kapitel an der aktuellen Wiedergabeposition.
                let pos = scale.adjustment().value() as i64;
                show_chapter_at(&label, &chapters, pos);
            });
            widgets.seek_scale.add_controller(motion);
        }

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
        // Einstellungen oben nur im schmalen Modus zeigen (Desktop: Seitenleiste).
        breakpoint.add_setter(&widgets.settings_top_btn, "visible", Some(&yes));
        // Der Desktop-Abstand über dem Inhalt entfällt im schmalen Modus.
        breakpoint.add_setter(&widgets.content_view, "margin-top", Some(&0i32.to_value()));
        root.add_breakpoint(breakpoint);

        // Icon-only Navigation (Seitenleiste + oben) in der **gespeicherten
        // Reihenfolge** erzeugen und an den Stack koppeln. Alle Schaltflächen
        // werden erzeugt; ausgeblendete Menüpunkte sind nur unsichtbar.
        model.sidebar_nav = widgets.sidebar_nav.clone();
        model.top_nav = widgets.top_nav.clone();
        let mut nav_buttons: Vec<(&'static str, bool, gtk::ToggleButton)> = Vec::new();
        for (is_sidebar, container) in [
            (true, widgets.sidebar_nav.clone()),
            (false, widgets.top_nav.clone()),
        ] {
            let mut group_leader: Option<gtk::ToggleButton> = None;
            for &name in &model.section_order {
                let Some((label, icon)) = section_meta(name) else {
                    continue;
                };
                let btn = gtk::ToggleButton::builder().build();
                btn.set_visible(!model.hidden_sections.contains(name));
                btn.add_css_class("flat");
                if is_sidebar {
                    // Desktop-Seitenleiste: Icon **mit Beschriftung**. Etwas
                    // größeres Icon (gut sichtbar, nie kleiner als der Standard).
                    let inner = gtk::Box::new(gtk::Orientation::Horizontal, 10);
                    let img = gtk::Image::from_icon_name(icon);
                    img.set_pixel_size(22);
                    inner.append(&img);
                    inner.append(&gtk::Label::new(Some(&gettext(label))));
                    btn.set_child(Some(&inner));
                    btn.set_hexpand(true);
                } else {
                    // Mobile Top-Leiste: nur Icon, deutlich größer (≈1,6×) als die
                    // Standardgröße – nie kleiner als jetzt.
                    let img = gtk::Image::from_icon_name(icon);
                    img.set_pixel_size(26);
                    btn.set_child(Some(&img));
                    btn.set_tooltip_text(Some(&gettext(label)));
                }
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
                nav_buttons.push((name, is_sidebar, btn));
            }
        }
        model.nav_buttons = nav_buttons.clone();

        // Desktop-Seitenleiste: „Einstellungen" ganz unten – Aufbau/Design wie
        // die Menüpunkte darüber (Icon + Beschriftung). Ein dehnbarer Zwischen-
        // raum schiebt den Knopf ans untere Ende.
        let spacer = gtk::Box::new(gtk::Orientation::Vertical, 0);
        spacer.set_vexpand(true);
        widgets.sidebar_nav.append(&spacer);
        let settings_btn = gtk::Button::builder().build();
        settings_btn.add_css_class("flat");
        settings_btn.set_hexpand(true);
        let settings_inner = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        settings_inner.append(&gtk::Image::from_icon_name("emblem-system-symbolic"));
        settings_inner.append(&gtk::Label::new(Some(&gettext("Settings"))));
        settings_btn.set_child(Some(&settings_inner));
        {
            let sender = sender.clone();
            settings_btn.connect_clicked(move |_| sender.input(Msg::OpenSettings));
        }
        widgets.sidebar_nav.append(&settings_btn);

        // Aktiven Button passend zur sichtbaren Stack-Seite setzen und den Namen
        // des Menüpunkts dezent als Untertitel der Kopfzeile anzeigen.
        let win_title = widgets.win_title.clone();
        let sync_active =
            move |stack: &adw::ViewStack, buttons: &[(&'static str, bool, gtk::ToggleButton)]| {
                let cur = stack.visible_child_name();
                let cur = cur.as_deref().unwrap_or("files");
                for (name, _is_sidebar, btn) in buttons {
                    btn.set_active(*name == cur);
                }
                win_title.set_subtitle(
                    &section_meta(cur)
                        .map(|(l, _)| gettext(l))
                        .unwrap_or_default(),
                );
            };
        // Zuletzt offenen Navigationspunkt wiederherstellen – aber keinen
        // ausgeblendeten. Notfalls auf den ersten sichtbaren Menüpunkt (in der
        // gewählten Reihenfolge) fallen.
        let restore = saved_section
            .as_deref()
            .filter(|s| !model.hidden_sections.contains(*s))
            .or_else(|| {
                model
                    .section_order
                    .iter()
                    .copied()
                    .find(|n| !model.hidden_sections.contains(*n))
            });
        if let Some(section) = restore {
            widgets.view_stack.set_visible_child_name(section);
        }
        sync_active(&widgets.view_stack, &nav_buttons);
        {
            let sender = sender.clone();
            widgets
                .view_stack
                .connect_visible_child_notify(move |stack| {
                    sync_active(stack, &nav_buttons);
                    // Statistik beim Öffnen des Bereichs frisch berechnen.
                    if stack.visible_child_name().as_deref() == Some("stats") {
                        sender.input(Msg::RefreshStats);
                    }
                });
        }

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
        let close_session = model.close_session.clone();
        root.connect_close_request(move |win| {
            // Letzte Hörposition sichern (deckt den Spalt zum 5-s-Speichern).
            if let Some((path, pos, dur)) = close_resume.borrow().clone() {
                if let Ok(lib) = Library::open() {
                    let _ = lib.set_resume_path(&path, guarded_resume(pos, dur));
                }
            }
            // Laufende Hör-Sitzung als letztes Ereignis sichern (sonst ginge der
            // gerade laufende Titel bei hartem Beenden verloren).
            if let Some((path, started_at, played_ms, dur)) = close_session.borrow().clone() {
                if played_ms > 0 {
                    if let Ok(lib) = Library::open() {
                        let _ = lib.log_play(&path, started_at, played_ms, dur, false, None);
                    }
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
                            // Läuft gerade eine echte Warteschlange? Dann das
                            // Einzellied dazwischenschieben und die Warteschlange
                            // danach an ihrer Stelle fortsetzen (sie bleibt erhalten).
                            if self.playing
                                && self.queue.len() > 1
                                && self.interrupted_queue.is_none()
                            {
                                self.interrupted_queue =
                                    Some((self.queue.clone(), self.queue_pos));
                            }
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
                        self.toast(&gettext("Removed from queue"));
                    } else {
                        self.queue.push(path);
                        self.toast(&gettext("Will play next"));
                    }
                    self.refresh_queue_icons();
                    self.save_queue();
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
                    // Foto des geöffneten Interpreten vorrangig nachladen.
                    self.fetch_focus_artist(&sender, &meta.name);
                    self.context_target = Some(CtxTarget::Artist(meta));
                    self.open_context_menu(root, &sender);
                }
            }
            Msg::ShowAlbumDetail(index) => {
                let meta = self.albums.guard().get(index).map(|c| c.meta.clone());
                if let Some(meta) = meta {
                    // Cover des geöffneten Albums vorrangig nachladen.
                    self.fetch_focus_album(&sender, &meta.artist, &meta.album);
                    self.context_target = Some(CtxTarget::Album(meta));
                    self.open_context_menu(root, &sender);
                }
            }
            Msg::ShowAlbumDetailFor { artist, album } => {
                self.fetch_focus_album(&sender, &artist, &album);
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
                // Alben-Übersicht: nach Albumname öffnen (Interpret egal).
                let album = self.albums.guard().get(index).map(|c| c.meta.album.clone());
                if let Some(album) = album {
                    self.open_album_by_name(&sender, &album);
                }
            }
            Msg::ShowConcertDetail(index) => {
                if let Some((scope, key, _, is_dir)) = self.concert_items.get(index).cloned() {
                    self.context_target = Some(self.entry_target(&scope, &key, is_dir));
                    self.open_context_menu(root, &sender);
                }
            }
            Msg::OpenArtistTracks(index) => {
                let meta = self.artists.guard().get(index).map(|c| c.meta.clone());
                if let Some(meta) = meta {
                    // Foto des geöffneten Interpreten vorrangig nachladen.
                    self.fetch_focus_artist(&sender, &meta.name);
                    self.open_artist_tracks(&sender, &meta);
                }
            }
            Msg::OpenAlbumTracks { artist, album } => {
                self.fetch_focus_album(&sender, &artist, &album);
                self.open_album_tracks(&sender, &artist, &album);
            }
            Msg::OpenEntryTracks { scope, key } => match scope.as_str() {
                "album" => {
                    // key = „Interpret\u{1}Album"
                    let mut parts = key.splitn(2, '\u{1}');
                    let artist = parts.next().unwrap_or("").to_string();
                    let album = parts.next().unwrap_or("").to_string();
                    self.open_album_tracks(&sender, &artist, &album);
                }
                "folder" => self.open_folder_tracks(&sender, &key),
                _ => {}
            },
            Msg::PlayFolderTrack { folder, path } => {
                let files: Vec<PathBuf> = self
                    .folder_tracks_ordered(&folder)
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
            Msg::PlayAlbumByNameTrack { album, path } => {
                // Queue = alle Titel des Albumnamens (interpretenübergreifend),
                // Start beim getippten – passend zur Alben-Übersicht.
                let files: Vec<PathBuf> = self
                    .album_tracks_by_name(&album)
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
                    self.toast(&gettext_f(
                        "Added {n} tracks to the queue",
                        &[("n", &n.to_string())],
                    ));
                }
            }
            Msg::CtxAddPlaylist => self.open_add_to_playlist_dialog(root, &sender),
            Msg::PlaylistNew => self.open_new_playlist_dialog(root, &sender),
            Msg::PlaylistCreate(name) => {
                let name = name.trim();
                if !name.is_empty() {
                    let _ = self.library.create_playlist(name);
                    self.reload_playlists(&sender);
                    self.toast(&gettext("Playlist created"));
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
                self.toast(&gettext("Playlist deleted"));
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
            Msg::PodcastSubscribe => self.open_subscribe_podcast_dialog(root, &sender),
            Msg::PodcastSearch(term) => {
                let term = term.trim().to_string();
                if !term.is_empty() {
                    self.toast(&gettext("Searching …"));
                    sender.spawn_command(move |out| {
                        let results =
                            crate::core::podcast::search_podcasts(&term).unwrap_or_default();
                        // Treffer sofort zeigen (noch ohne Cover) …
                        let _ = out.send(Cmd::PodcastSearchResults(results.clone()));
                        // … und die Cover-Thumbnails danach im Hintergrund nachladen.
                        for r in &results {
                            if let Some(img) = r.image_url.as_deref() {
                                crate::core::online::cache_podcast_image(img);
                            }
                        }
                        let _ = out.send(Cmd::PodcastSearchCoversReady);
                    });
                }
            }
            Msg::PodcastSubscribeUrl(url) => {
                let url = url.trim().to_string();
                if !url.is_empty() {
                    self.toast(&gettext("Loading feed …"));
                    sender.spawn_command(move |out| {
                        let _ = out.send(Cmd::PodcastFetched(fetch_and_store_podcast(&url)));
                    });
                }
            }
            Msg::PodcastRefresh(id) => {
                if let Ok(Some(url)) = self.library.podcast_feed_url(id) {
                    self.toast(&gettext("Updating feed …"));
                    sender.spawn_command(move |out| {
                        let _ = out.send(Cmd::PodcastFetched(fetch_and_store_podcast(&url)));
                    });
                }
            }
            Msg::OpenPodcast(id) => {
                if let Some((_, title, _, _)) =
                    self.podcast_items.iter().find(|(pid, _, _, _)| *pid == id).cloned()
                {
                    self.open_podcast(&sender, id, &title);
                }
            }
            Msg::PodcastDelete(id) => {
                let _ = self.library.delete_podcast(id);
                self.reload_podcasts(&sender);
                self.toast(&gettext("Podcast removed"));
            }
            Msg::ToggleEpisode { url, title } => {
                if self.playing_episode_url.as_deref() == Some(url.as_str()) {
                    // Bereits geladene Episode → Pause/Weiter umschalten.
                    if self.playing {
                        self.player.pause();
                    } else {
                        self.player.resume();
                    }
                    self.playing = !self.playing;
                    self.mpris.set_playing(self.playing);
                    self.refresh_queue_icons();
                } else {
                    // Andere/keine Episode → diese starten.
                    self.play_episode(&url, &title);
                }
            }
            Msg::EpisodeSeekTo { url, title, ms } => {
                if self.playing_episode_url.as_deref() == Some(url.as_str()) {
                    // Läuft schon → direkt an die Stelle springen.
                    if self.player.seek_ms(ms).is_ok() {
                        self.position_ms = ms;
                        self.save_episode_progress();
                    }
                } else {
                    // Sonst die Episode an der Sprungmarke starten.
                    self.play_episode_at(&url, &title, ms);
                }
            }
            Msg::SetPodcastView(view) => self.podcast_view = view,
            Msg::ShowEpisodeDetail(index) => self.open_episode_detail(root, &sender, index),
            Msg::ShowPodcastEpisodeDetail { podcast_id, index } => {
                self.open_podcast_episode_detail(root, &sender, podcast_id, index)
            }
            Msg::ShowPodcastDetail(id) => self.open_podcast_detail(root, &sender, id),
            Msg::CtxEqualizer => self.open_eq_dialog(root, &sender),
            Msg::CtxShare => self.open_share_dialog(root, &sender),
            Msg::ShareHost => {
                self.open_sync_dialog(root, &sender);
                self.start_sync_server(&sender);
            }
            Msg::ShareScan => {
                self.open_sync_dialog(root, &sender);
                self.start_sync_scan(&sender);
            }
            Msg::SyncStartServer => self.start_sync_server(&sender),
            Msg::SyncStartScan => self.start_sync_scan(&sender),
            Msg::SyncQrDecoded(url) => self.handle_sync_qr(&url, &sender),
            Msg::SyncDialogClosed => self.teardown_sync(),
            Msg::TrackFinished => {
                if self.playing_episode_url.is_some() && self.queue.is_empty() {
                    // Eine gestreamte Episode ist zu Ende (keine Warteschlange
                    // dahinter): Wiedergabestand zurücksetzen, Markierung lösen.
                    self.playing = false;
                    self.playing_episode_url = None;
                    self.mpris.set_playing(false);
                    self.refresh_queue_icons();
                } else {
                    // Bis zum Ende gehört → Hör-Sitzung als „durchgehört" abschließen,
                    // bevor das nachfolgende play_current eine neue Sitzung startet.
                    self.finalize_play_session(true);
                    // Titel zu Ende gehört → Resume vergessen, nächstes Mal von vorn.
                    // `take()` verhindert, dass play_current die (End-)Position erneut
                    // als Resume-Punkt speichert.
                    if let Some(path) = self.playing_path.take() {
                        let _ = self.library.set_resume_path(&path.to_string_lossy(), 0);
                    }
                    *self.close_resume.borrow_mut() = None;
                    // War ein Einzellied dazwischengeschoben, jetzt die unterbrochene
                    // Warteschlange an ihrer Stelle fortsetzen.
                    if self.queue.len() == 1 && self.interrupted_queue.is_some() {
                        if let Some((q, pos)) = self.interrupted_queue.take() {
                            self.queue = q;
                            self.queue_pos = pos;
                            self.play_current();
                        }
                    } else {
                        // Eine neue (mehrteilige) Wiedergabe verwirft eine evtl.
                        // gemerkte Unterbrechung.
                        self.interrupted_queue = None;
                        self.play_next();
                    }
                }
            }
            Msg::SetStatsPeriod(period) => {
                self.stats_period = period;
                self.refresh_stats(&sender);
            }
            Msg::RefreshStats => self.refresh_stats(&sender),
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
                    // Resume-Position einer laufenden Podcast-Episode fortschreiben.
                    if self.playing_episode_url.is_some() {
                        self.save_episode_progress();
                    }
                    // Aktuelles Kapitel unter dem Titel mitführen (außer beim Hover).
                    self.update_current_chapter();
                    // Gehörte Zeit der Statistik-Sitzung weiterzählen (Wanduhr, nur
                    // während „Playing"; ~1 s je Tick). Die Dauer ggf. nachziehen,
                    // falls sie beim Start noch nicht feststand.
                    let dur = self.track_duration_ms;
                    if let Some(s) = self.play_session.as_mut() {
                        s.played_ms += 1000;
                        if s.duration_ms == 0 {
                            s.duration_ms = dur;
                        }
                    }
                    if let Some(cs) = self.close_session.borrow_mut().as_mut() {
                        if let Some(s) = self.play_session.as_ref() {
                            cs.2 = s.played_ms;
                            cs.3 = s.duration_ms;
                        }
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
                        self.finalize_play_session(false);
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
            Msg::ToggleShuffle => {
                self.shuffle = !self.shuffle;
                // Beim Einschalten eine frische Zufalls-Reihenfolge der ganzen
                // Queue aufbauen (laufender Titel zuerst).
                if self.shuffle {
                    self.rebuild_shuffle_order();
                }
            }
            Msg::ToggleRepeat => {
                self.repeat = !self.repeat;
                let _ = self
                    .library
                    .set_setting("repeat", if self.repeat { "1" } else { "0" });
            }
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
            Msg::HideEnrichBanner => self.enrich_banner_hidden = true,
            Msg::OpenSettings => self.open_settings(root, &sender),
            Msg::CheckForUpdates => {
                if crate::core::update::in_flatpak() {
                    self.toast(&gettext("Checking for updates …"));
                    sender.spawn_oneshot_command(|| {
                        Cmd::UpdateChecked(crate::core::update::check())
                    });
                } else {
                    self.toast(&gettext("Updates are only available in the Flatpak version."));
                }
            }
            Msg::InstallFlatpakUpdate => {
                self.toast(&gettext(
                    "Update started – it runs in the background. Please restart Emilia afterwards.",
                ));
                let sender2 = sender.clone();
                // Über das Flatpak-Portal anstoßen (Hauptthread; Fortschritt via Signal).
                if let Err(e) =
                    crate::core::update::install(move |res| sender2.input(Msg::FlatpakUpdateFinished(res)))
                {
                    tracing::warn!("Flatpak update failed to start: {e}");
                    sender.input(Msg::FlatpakUpdateFinished(Err(e.to_string())));
                }
            }
            Msg::FlatpakUpdateFinished(res) => match res {
                Ok(()) => self.toast(&gettext("Update installed. Please restart Emilia.")),
                Err(_) => self.toast(&gettext_f(
                    "Update failed. You can update manually: {cmd}",
                    &[("cmd", &crate::core::update::manual_command())],
                )),
            },
            Msg::OpenGlobalEq => self.open_global_eq(root, &sender),
            Msg::OpenCurrentEq => {
                if let Some(path) = self.queue.get(self.queue_pos).cloned() {
                    let key = path.to_string_lossy().into_owned();
                    let name = Self::track_display_name(&path);
                    self.open_eq_editor(root, &sender, "the track", &name, None, "track", key);
                }
            }
            Msg::ShowQueue => self.open_queue_dialog(root, &sender),
            Msg::QueueRemove(idx) => {
                if idx < self.queue.len() {
                    let was_current = idx == self.queue_pos;
                    self.queue.remove(idx);
                    if self.queue_pos > idx {
                        self.queue_pos -= 1;
                    }
                    if was_current {
                        // Lief der entfernte Titel → den nun an dieser Stelle
                        // stehenden spielen (oder stoppen, wenn leer).
                        if self.queue.is_empty() {
                            self.player.stop();
                            self.playing = false;
                            self.now_playing = None;
                            self.playing_path = None;
                            self.position_ms = 0;
                            self.track_duration_ms = 0;
                            *self.close_resume.borrow_mut() = None;
                            self.mpris.set_stopped();
                        } else {
                            self.queue_pos = self.queue_pos.min(self.queue.len() - 1);
                            self.play_current();
                        }
                    }
                    self.reload_queue_list(&sender);
                    self.refresh_queue_icons();
                    self.save_queue();
                }
            }
            Msg::QueueClear => {
                self.player.stop();
                self.queue.clear();
                self.queue_pos = 0;
                self.shuffle_order.clear();
                self.shuffle_idx = 0;
                self.playing = false;
                self.now_playing = None;
                self.playing_path = None;
                self.position_ms = 0;
                self.track_duration_ms = 0;
                *self.close_resume.borrow_mut() = None;
                self.mpris.set_stopped();
                self.reload_queue_list(&sender);
                self.refresh_queue_icons();
                self.save_queue();
                self.toast(&gettext("Queue cleared"));
            }
            Msg::QueueMove { from, to } => {
                let len = self.queue.len();
                if from < len && to < len && from != to {
                    let item = self.queue.remove(from);
                    self.queue.insert(to, item);
                    // queue_pos so anpassen, dass derselbe Titel weiterläuft.
                    let cur = self.queue_pos;
                    self.queue_pos = if cur == from {
                        to
                    } else {
                        let mut p = cur;
                        if from < cur {
                            p -= 1;
                        }
                        if to <= p {
                            p += 1;
                        }
                        p
                    };
                    self.reload_queue_list(&sender);
                    self.refresh_queue_icons();
                    self.save_queue();
                }
            }
            Msg::SetMusicDir(path) => {
                let dir = path.to_string_lossy().into_owned();
                if let Err(e) = self.library.set_setting("music_dir", &dir) {
                    tracing::error!("Failed to save music folder: {e}");
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
            Msg::UploadCover => self.open_cover_upload_dialog(root, &sender),
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
            Msg::SetLanguage(lang) => {
                if lang != self.ui_language {
                    self.ui_language = lang.clone();
                    let _ = self.library.set_setting("ui_language", &lang);
                    // gettext liest die Sprache nur beim Start; daher die App
                    // neu starten, damit die ganze Oberfläche umschaltet.
                    if let Ok(exe) = std::env::current_exe() {
                        let _ = std::process::Command::new(exe).spawn();
                    }
                    std::process::exit(0);
                }
            }
            Msg::SetColorScheme(scheme) => {
                apply_color_scheme(&scheme);
                let _ = self.library.set_setting("color_scheme", &scheme);
            }
            Msg::SetAreas { scope, key, value } => {
                if let Err(e) = self.library.set_category(scope, &key, Some(&value)) {
                    tracing::error!("Failed to save properties: {e}");
                }
                // Sichtbarkeit/Zuordnung kann sich überall geändert haben →
                // Ansichten neu laden. Konzerte/Hörbücher werden dabei live aus
                // den Eigenschaften abgeleitet (kein separater Abgleich nötig).
                self.reload_albums();
                self.reload_artists();
                self.load_concerts(&sender);
                self.load_audiobooks(&sender);
                self.load_dir(&sender);
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
                    self.toast(&gettext("No music folder set"));
                    return;
                };
                let existing = self.library.concert_paths().unwrap_or_default();
                self.toast(&gettext("Searching for concerts …"));
                sender.spawn_oneshot_command(move || {
                    Cmd::Candidates(crate::core::concert::scan_candidates(&root, &existing))
                });
            }
            Msg::ConcertDismissHint => {
                self.concert_hint_dismissed = true;
                let _ = self.library.set_setting("concert_hint_dismissed", "1");
            }
            Msg::ConcertHideSection => {
                self.set_section_visible("concerts", false);
                self.toast(&gettext("Hid the Concerts menu item"));
            }
            Msg::ConcertAdd(items) => {
                let n = items.len();
                for (path, title, is_dir) in &items {
                    // Tabelle: nur für die Kandidaten-Filterung beim nächsten Import.
                    let _ = self.library.add_concert(path, title, *is_dir);
                    // Anzeige/Verwaltung über die Eigenschaften: den Bereich
                    // „Konzerte" auf den enthaltenen Alben/Titeln markieren, damit
                    // das Konzert auch wieder darüber entfernt werden kann.
                    let entries = if *is_dir {
                        self.folder_albums_and_tracks(path)
                    } else {
                        vec![("track".to_string(), path.clone(), title.clone(), false)]
                    };
                    for (scope, key, _, _) in entries {
                        let _ = self.library.add_category_area(
                            &scope,
                            &key,
                            crate::core::category::Area::Concerts,
                        );
                    }
                }
                self.load_concerts(&sender);
                self.toast(&ngettext_n(
                    "Added {n} concert",
                    "Added {n} concerts",
                    n as u32,
                ));
            }
            Msg::PlayConcert(index) => {
                if let Some((scope, key, _, is_dir)) = self.concert_items.get(index).cloned() {
                    self.play_entry(&scope, &key, is_dir);
                }
            }
            Msg::SetSectionVisible { section, visible } => {
                self.set_section_visible(section, visible);
            }
            Msg::MoveSection { from, to } => {
                if from < self.section_order.len() && to < self.section_order.len() && from != to {
                    let name = self.section_order.remove(from);
                    self.section_order.insert(to, name);
                    let value = self.section_order.join(",");
                    let _ = self.library.set_setting("section_order", &value);
                    // Reihenfolge auf die vorhandenen Schaltflächen anwenden.
                    self.apply_section_order();
                }
            }
            Msg::UnhideEntry { scope, key } => {
                // Festlegung löschen → zurück auf Standard (wieder sichtbar).
                let _ = self.library.set_category(&scope, &key, None);
                self.reload_albums();
                self.reload_artists();
                self.load_concerts(&sender);
                self.load_audiobooks(&sender);
                self.load_dir(&sender);
                self.toast(&gettext("Shown again"));
            }
            Msg::ToggleFavorite => {
                if let Some(target) = self.context_target.clone() {
                    let (scope, key, title, is_dir) = self.favorite_ref(&target);
                    let on = !self.library.is_favorite(scope, &key);
                    let _ = self.library.set_favorite(scope, &key, &title, is_dir, on);
                    self.load_favorites(&sender);
                    self.toast(&if on {
                        gettext("Added to favorites")
                    } else {
                        gettext("Removed from favorites")
                    });
                }
            }
            Msg::PlayFavorite(index) => {
                if let Some((scope, key, _, is_dir)) = self.favorite_items.get(index).cloned() {
                    // Läuft genau dieser Titel bereits, nur Play/Pause umschalten
                    // (Klick auf das eingeblendete Pause-Zeichen pausiert), statt
                    // ihn neu zu starten.
                    let is_current = scope == "track"
                        && self
                            .playing_path
                            .as_ref()
                            .is_some_and(|p| p.to_string_lossy().as_ref() == key.as_str());
                    if is_current {
                        if self.playing {
                            self.save_resume();
                            self.player.pause();
                            self.playing = false;
                        } else {
                            self.player.resume();
                            self.playing = true;
                        }
                        self.mpris.set_playing(self.playing);
                        self.refresh_queue_icons();
                    } else if scope == "track" {
                        // Ganze Favoriten-Titelliste als Queue (vorherige leeren),
                        // ab dem angeklickten Titel.
                        let tracks: Vec<PathBuf> = self
                            .favorite_items
                            .iter()
                            .filter(|(s, _, _, _)| s == "track")
                            .map(|(_, k, _, _)| PathBuf::from(k))
                            .collect();
                        let pos = tracks.iter().position(|p| *p == PathBuf::from(&key)).unwrap_or(0);
                        self.shuffle = false;
                        self.queue = tracks;
                        self.queue_pos = pos;
                        self.play_current();
                        self.refresh_queue_icons();
                    } else {
                        self.play_entry(&scope, &key, is_dir);
                    }
                    // Aktiv-Markierung (Play-/Pause-Icon) in der Favoritenliste aktualisieren.
                    self.load_favorites(&sender);
                }
            }
            Msg::ShowFavoriteDetail(index) => {
                if let Some((scope, key, _, is_dir)) = self.favorite_items.get(index).cloned() {
                    self.context_target = Some(self.entry_target(&scope, &key, is_dir));
                    self.open_context_menu(root, &sender);
                }
            }
            Msg::MoveFavorite { from, to } => {
                if from < self.favorite_items.len() && to < self.favorite_items.len() && from != to {
                    let item = self.favorite_items.remove(from);
                    self.favorite_items.insert(to, item);
                    let order: Vec<(String, String)> = self
                        .favorite_items
                        .iter()
                        .map(|(s, k, _, _)| (s.clone(), k.clone()))
                        .collect();
                    let _ = self.library.set_favorite_order(&order);
                    self.load_favorites(&sender);
                }
            }
            Msg::PlayAudiobook(index) => {
                if let Some((scope, key, _, is_dir)) = self.audiobook_items.get(index).cloned() {
                    self.play_entry(&scope, &key, is_dir);
                }
            }
            Msg::ShowAudiobookDetail(index) => {
                if let Some((scope, key, _, is_dir)) = self.audiobook_items.get(index).cloned() {
                    self.context_target = Some(self.entry_target(&scope, &key, is_dir));
                    self.open_context_menu(root, &sender);
                }
            }
            Msg::TogglePlay => {
                if self.playing {
                    self.save_resume();
                    self.player.pause();
                    self.playing = false;
                } else if self.playing_path.is_some() {
                    // Pausiert → fortsetzen.
                    self.player.resume();
                    self.playing = true;
                } else if !self.queue.is_empty() {
                    // Wiedergabe war beendet → von der aktuellen Position (nach dem
                    // Ende auf 0 zurückgespult) neu starten. play_current setzt
                    // playing/MPRIS/Icons selbst.
                    self.play_current();
                    return;
                } else {
                    return;
                }
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
            Cmd::EnrichDone => {
                self.enriching = false;
                self.reload_albums();
                self.reload_artists();
            }
            Cmd::ReloadViews => {
                self.reload_albums();
                self.reload_artists();
            }
            Cmd::ScanDone { then_enrich } => {
                // Bibliothek ist eingelesen → Ansichten aktualisieren.
                self.reload_albums();
                self.reload_artists();
                // Danach automatisch online nachladen – ohne Zutun des Nutzers,
                // sofern gewünscht, nicht schon ein Abruf läuft und überhaupt eine
                // Verbindung besteht (auf jeder Verbindung, auch getaktet). Der
                // lokale Scan lief schon, daher hier ohne erneutes Einlesen.
                if then_enrich
                    && self.auto_enrich
                    && !self.enriching
                    && self.root_dir.is_some()
                    && online_available()
                {
                    // Automatischer Lauf (ohne erneuten Tag-Scan).
                    self.run_enrich(&sender, false);
                }
            }
            Cmd::Candidates(candidates) => {
                if candidates.is_empty() {
                    self.toast(&gettext("No new concert candidates found"));
                } else {
                    self.open_concert_import_dialog(root, &sender, candidates);
                }
            }
            Cmd::PodcastFetched(title) => {
                self.reload_podcasts(&sender);
                match title {
                    Some(t) => self.toast(&gettext_f("Subscribed: {t}", &[("t", &t)])),
                    None => self.toast(&gettext("Could not load feed")),
                }
            }
            Cmd::PodcastSearchResults(results) => {
                self.podcast_search_results = results;
                self.rebuild_podcast_search_results(&sender);
            }
            Cmd::PodcastSearchCoversReady => self.rebuild_podcast_search_results(&sender),
            Cmd::ReloadPodcasts => self.reload_podcasts(&sender),
            Cmd::Sync(ev) => self.on_sync_event(ev, &sender),
            Cmd::UpdateChecked(result) => match result {
                crate::core::update::CheckResult::UpToDate => self.toast(&gettext_f(
                    "Emilia is up to date (version {v}).",
                    &[("v", env!("CARGO_PKG_VERSION"))],
                )),
                crate::core::update::CheckResult::Unknown => {
                    self.toast(&gettext("Could not check for updates (offline?)."))
                }
                crate::core::update::CheckResult::Available => {
                    // Rückfrage vor dem Einspielen – installiert über das Portal.
                    let confirm = adw::AlertDialog::new(
                        Some(&gettext("Update available")),
                        Some(&gettext("A newer version of Emilia is available. Install it now?")),
                    );
                    confirm.add_response("cancel", &gettext("Cancel"));
                    confirm.add_response("update", &gettext("Update"));
                    confirm.set_response_appearance("update", adw::ResponseAppearance::Suggested);
                    confirm.set_default_response(Some("update"));
                    confirm.set_close_response("cancel");
                    let sender = sender.clone();
                    confirm.connect_response(None, move |_, resp| {
                        if resp == "update" {
                            sender.input(Msg::InstallFlatpakUpdate);
                        }
                    });
                    confirm.present(Some(root));
                }
            },
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

/// Ob ein Online-Abruf sinnvoll ist: schlicht, ob überhaupt eine Verbindung
/// besteht. Bewusst **ohne** Taktungs-Prüfung – der Sync läuft auf jeder
/// Verbindung (Wunsch des Nutzers). Die Offline-Prüfung bleibt, damit im
/// Funkloch nicht reihenweise „Fehlversuche" verbucht werden (die einen Eintrag
/// dauerhaft sperren würden). Grundlage: `gio::NetworkMonitor` (NetworkManager).
pub(crate) fn online_available() -> bool {
    use gtk::gio::prelude::NetworkMonitorExt;
    gtk::gio::NetworkMonitor::default().is_network_available()
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
    parts.push(ngettext_n("{n} song", "{n} songs", track_count as u32));
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

    // Eigenschaften: Dateien ausblenden, die nicht im Bereich „Dateisystem"
    // sichtbar sind (geerbt von Album/Interpret). Dateien ohne DB-Eintrag bleiben
    // sichtbar. Ordner werden nicht gefiltert (bleiben navigierbar).
    let lib = Library::open().ok();
    let mut out = Vec::with_capacity(dirs.len() + files.len());
    // Ordner ausblenden, deren Ordner-Eigenschaft „Dateisystem" nicht enthält
    // (vererbt von übergeordneten Ordnern).
    for d in dirs {
        let visible = match &lib {
            Some(lib) => lib
                .folder_areas(&d.to_string_lossy())
                .contains(&crate::core::category::Area::Filesystem),
            None => true,
        };
        if visible {
            out.push(FsEntry::dir(d));
        }
    }
    for f in files {
        let visible = match &lib {
            Some(lib) => match lib.track_by_path(&f.to_string_lossy()).ok().flatten() {
                Some(t) => lib
                    .resolve_areas(t.artist.as_deref(), t.album.as_deref(), &t.path)
                    .contains(&crate::core::category::Area::Filesystem),
                None => true,
            },
            None => true,
        };
        if visible {
            out.push(FsEntry::file(f));
        }
    }
    out
}

impl App {
    /// Schmaler (mobiler) Modus? Identisch zur eingeklappten Seitenleiste, die
    /// der Breakpoint bei geringer Fensterbreite setzt.
    pub(crate) fn is_mobile(&self) -> bool {
        self.split.is_collapsed()
    }

    /// Detail-Dialoge auf dem Phone über die **volle Breite** zeigen
    /// (Bottom-Sheet); auf dem Desktop schwebend wie gehabt (Auto).
    pub(crate) fn adapt_detail_dialog(&self, dialog: &adw::Dialog) {
        if self.is_mobile() {
            dialog.set_presentation_mode(adw::DialogPresentationMode::BottomSheet);
        }
    }

    /// Lädt das **Foto des gerade geöffneten Interpreten** sofort im Hintergrund
    /// nach – damit zuerst erscheint, was der Nutzer ansieht (Vorrang vor dem
    /// laufenden Massen-Sync). Tut nichts ohne Netz, bei bereits zugeordnetem Foto
    /// oder nach zu vielen erfolglosen Versuchen. Nach Erfolg werden die Ansichten
    /// neu geladen (`Cmd::ReloadViews`).
    pub(crate) fn fetch_focus_artist(&self, sender: &ComponentSender<Self>, name: &str) {
        let name = name.trim().to_string();
        if name.is_empty() || !online_available() {
            return;
        }
        let matched = self
            .library
            .get_artist_meta(&name)
            .ok()
            .flatten()
            .is_some_and(|m| m.status == "matched");
        if matched || self.library.artist_attempts(&name) >= crate::ui::enrich::MAX_ATTEMPTS {
            return;
        }
        sender.spawn_oneshot_command(move || {
            if let Ok(lib) = Library::open() {
                let client = crate::core::online::OnlineClient::new();
                let (image, errored) = match client.fetch_artist_image(&name) {
                    Ok(img) => (img, false),
                    Err(_) => (None, true),
                };
                let meta = crate::core::online::store_artist_image(&name, image, errored);
                let _ = lib.upsert_artist_meta(&meta);
            }
            Cmd::ReloadViews
        });
    }

    /// Wie [`Self::fetch_focus_artist`], nur für das **Cover des gerade geöffneten
    /// Albums**. Übersprungen, wenn bereits ein Cover vorliegt, kein Netz besteht
    /// oder zu viele Versuche erfolglos blieben.
    pub(crate) fn fetch_focus_album(&self, sender: &ComponentSender<Self>, artist: &str, album: &str) {
        let artist = artist.trim().to_string();
        let album = album.trim().to_string();
        if artist.is_empty() || album.is_empty() || !online_available() {
            return;
        }
        let has_cover = self
            .library
            .get_album_meta(&artist, &album)
            .ok()
            .flatten()
            .is_some_and(|m| m.cover_path.as_deref().is_some_and(|p| !p.trim().is_empty()));
        if has_cover || self.library.album_attempts(&artist, &album) >= crate::ui::enrich::MAX_ATTEMPTS {
            return;
        }
        sender.spawn_oneshot_command(move || {
            if let Ok(lib) = Library::open() {
                let client = crate::core::online::OnlineClient::new();
                let _ = crate::core::online::enrich_album(&client, &lib, &artist, &album);
            }
            Cmd::ReloadViews
        });
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
            None => gettext("No music folder – please set one in settings"),
        }
    }

    /// Blendet einen Navigations-Menüpunkt ein/aus: aktualisiert den Zustand,
    /// speichert ihn, schaltet alle zugehörigen Schaltflächen (Seitenleiste +
    /// obere Leiste) und wechselt beim Ausblenden des aktiven Punkts auf den
    /// ersten sichtbaren.
    pub(crate) fn set_section_visible(&mut self, section: &str, visible: bool) {
        // Mindestens ein Menüpunkt muss sichtbar bleiben.
        if !visible {
            let visible_count = SECTIONS
                .iter()
                .filter(|(n, _, _)| !self.hidden_sections.contains(*n))
                .count();
            if visible_count <= 1 {
                return;
            }
        }
        if visible {
            self.hidden_sections.remove(section);
        } else {
            self.hidden_sections.insert(section.to_string());
        }
        let value = SECTIONS
            .iter()
            .map(|(n, _, _)| *n)
            .filter(|n| self.hidden_sections.contains(*n))
            .collect::<Vec<_>>()
            .join(",");
        let _ = self.library.set_setting("hidden_sections", &value);

        for (name, _is_sidebar, btn) in &self.nav_buttons {
            if *name == section {
                btn.set_visible(visible);
            }
        }

        // Wird der gerade sichtbare Bereich ausgeblendet, auf den ersten
        // sichtbaren Menüpunkt (in der gewählten Reihenfolge) wechseln.
        if !visible {
            let cur = self.view_stack.visible_child_name();
            if cur.as_deref() == Some(section) {
                if let Some(next) = self
                    .section_order
                    .iter()
                    .copied()
                    .find(|n| !self.hidden_sections.contains(*n))
                {
                    self.view_stack.set_visible_child_name(next);
                }
            }
        }
    }

    /// Wendet `section_order` auf die Navigations-Container an, indem die
    /// vorhandenen Schaltflächen umsortiert werden (Seitenleisten-Knöpfe vor dem
    /// Abstandshalter + „Einstellungen", die unberührt am Ende bleiben).
    pub(crate) fn apply_section_order(&self) {
        for sidebar in [true, false] {
            let container = if sidebar { &self.sidebar_nav } else { &self.top_nav };
            let mut prev: Option<gtk::Widget> = None;
            for &name in &self.section_order {
                if let Some((_, _, btn)) = self
                    .nav_buttons
                    .iter()
                    .find(|(n, s, _)| *n == name && *s == sidebar)
                {
                    container.reorder_child_after(btn, prev.as_ref());
                    prev = Some(btn.clone().upcast());
                }
            }
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
