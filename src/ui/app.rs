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
use crate::model::{AlbumMeta, ArtistMeta, Source, Track};
use crate::ui::album_row::{AlbumCard, AlbumOutput};
use crate::ui::app_podcast::fetch_and_store_podcast;
use crate::ui::artist_row::{ArtistCard, ArtistOutput};
use crate::ui::fs_row::{FsEntry, FsInput, FsOutput, FsRow, RowOpts};

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

/// Aktuell in der Dateiansicht gewählte Quelle: das primäre `music_dir`
/// (impliziter erster Tab „Musik") oder eine zusätzliche Quelle per ID.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ActiveSource {
    /// Das primäre Musikverzeichnis (`music_dir`).
    Primary,
    /// Eine zusätzliche Quelle (lokaler Zweitordner oder WebDAV) per `source.id`.
    Source(i64),
}

/// Ein Titel der entfernten (Cloud-)Wiedergabe-Reihe. Eigenständig gehalten,
/// getrennt von der lokalen `PathBuf`-Warteschlange.
#[derive(Debug, Clone)]
pub(crate) struct RemoteTrack {
    /// Pfad relativ zur Musikwurzel der Quelle (führender Slash).
    pub(crate) rel_path: String,
    /// Anzeigename (für „Now Playing").
    pub(crate) title: String,
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
pub(crate) const SECTIONS: [(&str, &str, &str); 10] = [
    ("favorites", "Favorites", "emilia-favorite-symbolic"),
    ("files", "Files", "folder-symbolic"),
    ("artists", "Artists", "avatar-default-symbolic"),
    ("albums", "Albums", "media-optical-symbolic"),
    ("concerts", "Concerts", "emilia-concert-symbolic"),
    ("podcasts", "Podcasts", "microphone-symbolic"),
    ("streaming", "Streaming", "audio-x-generic-symbolic"),
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
/// Takt des leisen Hintergrund-Nachzugs fehlender Interpreten-Fotos & Cover.
/// Bewusst niedrig (~1 min), damit neue Nutzer rasch eine aufgewertete Übersicht
/// bekommen; der Worker drosselt die eigentlichen Netz-Anfragen selbst.
const AUTO_ENRICH_INTERVAL_SECS: u32 = 60;

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

/// Welche Ansicht die Streaming-Seite zeigt (Tab-Umschalter).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamView {
    /// Gespeicherte Sender/Kanäle.
    Channels,
    /// Timeshift-Mitschnitte.
    Recordings,
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
    /// Eigener Eingabe-Sender, um aus Methoden ohne `ComponentSender` (z. B.
    /// [`Self::play_current`]) Nachrichten an die Komponente zu schicken.
    pub(crate) input: relm4::Sender<Msg>,
    pub(crate) entries: FactoryVecDeque<FsRow>,
    pub(crate) albums: FactoryVecDeque<AlbumCard>,
    /// Galerie-Variante der Alben (Cover-Gitter), parallel zur Listen-Factory.
    pub(crate) albums_gallery: gtk::FlowBox,
    /// Album-Übersicht (gleiche Reihenfolge wie Factory/Galerie). Dient als
    /// Index-Auflösung für Klicks in der Galerie, wo die Factory leer bleibt.
    pub(crate) albums_overview: Vec<crate::model::AlbumMeta>,
    pub(crate) album_count: usize,
    pub(crate) artists: FactoryVecDeque<ArtistCard>,
    /// Galerie-Variante der Interpreten (Foto-Gitter).
    pub(crate) artists_gallery: gtk::FlowBox,
    /// Interpreten-Übersicht (gleiche Reihenfolge) – Index-Auflösung für Galerie.
    pub(crate) artists_overview: Vec<crate::model::ArtistMeta>,
    pub(crate) artist_count: usize,
    /// Läuft gerade ein Anreicherungs-Lauf? (verhindert parallele Läufe; ohne
    /// sichtbare Fortschrittsanzeige – der Abruf läuft still im Hintergrund).
    pub(crate) enriching: bool,
    /// Cover & Metadaten beim Start automatisch online nachladen (nur bei
    /// nicht-getakteter Verbindung; in den Einstellungen abschaltbar).
    pub(crate) auto_enrich: bool,
    /// Abbruch-Flag für den Anreicherungs-Worker.
    pub(crate) enrich_cancel: Arc<AtomicBool>,
    pub(crate) acoustid_key: Option<String>,
    pub(crate) fanart_key: Option<String>,
    /// Anzeigesprache: "system" (System-Locale), "de" oder "en". In den
    /// Einstellungen umschaltbar; greift nach einem Neustart der App.
    pub(crate) ui_language: String,
    /// Listen als **Galerie** (Cover-Gitter) statt als Liste darstellen.
    pub(crate) gallery_view: bool,
    /// Anzahl der Kacheln pro Reihe in der Galerie-Ansicht (2–8).
    pub(crate) gallery_columns: u32,
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
    /// Galerie-Variante der Konzerte (Cover-Gitter).
    pub(crate) concerts_gallery: gtk::FlowBox,
    pub(crate) concert_hint_dismissed: bool,
    /// Galerien (Interpret bzw. Album), für die in **dieser Sitzung** schon ein
    /// bedarfsgesteuerter Abruf lief – Schlüssel `"a\x01<name>"` bzw.
    /// `"b\x01<artist>\x01<album>"`. Verhindert, dass für Einträge ohne Galerie
    /// (die keine Versuchsgrenze haben) bei jedem Öffnen erneut angefragt wird.
    pub(crate) gallery_tried: std::cell::RefCell<std::collections::HashSet<String>>,
    /// Galerie-FlowBoxen, deren Resize-Hook (quadratische Kacheln) bereits
    /// einmalig verbunden wurde – verhindert, dass sich Handler aufsummieren.
    pub(crate) gallery_hooked: std::cell::RefCell<std::collections::HashSet<usize>>,
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
    /// Galerie-Variante der Hörbücher (Cover-Gitter).
    pub(crate) audiobooks_gallery: gtk::FlowBox,
    // Playlisten
    pub(crate) playlist_items: Vec<(i64, String, i64)>,
    pub(crate) playlists_list: gtk::ListBox,
    // Podcasts: (id, Titel, Bild-URL, Episodenzahl)
    pub(crate) podcast_items: Vec<(i64, String, Option<String>, i64)>,
    pub(crate) podcasts_list: gtk::ListBox,
    /// Galerie-Variante der Podcast-Übersicht (Cover-Gitter).
    pub(crate) podcasts_gallery: gtk::FlowBox,
    /// Welche Podcast-Ansicht sichtbar ist: neueste Episoden oder Abo-Übersicht.
    pub(crate) podcast_view: PodcastView,
    /// Welche Streaming-Ansicht sichtbar ist: Kanäle oder Aufnahmen.
    pub(crate) stream_view: StreamView,
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
    // Streaming (Internet-Radio): gespeicherte Sender.
    pub(crate) stream_items: Vec<crate::model::StreamItem>,
    pub(crate) streams_list: gtk::ListBox,
    /// Treffer der letzten Sendersuche (Radio-Browser), für den Hinzufügen-Dialog.
    pub(crate) stream_search_results: Vec<crate::core::streaming::StationResult>,
    /// Solange der Hinzufügen-Dialog offen ist: (Dialog, Trefferliste) – damit
    /// asynchron eintreffende Treffer in die bereits gezeigte Liste passen.
    pub(crate) stream_search:
        std::rc::Rc<std::cell::RefCell<Option<(adw::Dialog, gtk::ListBox)>>>,
    /// ID des gerade laufenden Senders (für die Detailseiten-Anzeige); `None`,
    /// wenn gerade Musik/Episode bzw. nichts läuft.
    pub(crate) playing_stream: Option<i64>,
    /// Aktuell laufender Titel des Senders (aus den ICY-Metadaten), für die
    /// „Now Playing"-Anzeige; `None`, solange (noch) kein Titel gemeldet wurde.
    pub(crate) stream_title: Option<String>,
    /// Timeshift-Mitschnitt des laufenden Senders (Ringpuffer); `None`, wenn kein
    /// Sender läuft bzw. der Puffer auf 0 Minuten steht.
    pub(crate) recorder: Option<crate::core::recorder::Recorder>,
    /// Aktive „Aufnahme" (Zustandsmaschine, die an den Songgrenzen speichert).
    pub(crate) record_state: Option<crate::ui::app_streaming::RecordState>,
    /// Größe des Timeshift-Puffers in Minuten (0 = aus, max. 60).
    pub(crate) recording_buffer_minutes: u32,
    // Aufnahmen (gespeicherte Timeshift-Mitschnitte).
    pub(crate) recording_items: Vec<crate::model::RecordingItem>,
    pub(crate) recordings_list: gtk::ListBox,
    /// Play/Pause-Knöpfe der Senderzeilen (Sender-Id → Knopf), zum Auffrischen des
    /// Icons beim Wechsel des Wiedergabestands. Abgehängte Knöpfe werden verworfen.
    pub(crate) stream_play_buttons: std::rc::Rc<std::cell::RefCell<Vec<(i64, gtk::Button)>>>,
    /// „Verbunden"-Liste der Nextcloud-Seite im offenen Einstellungsdialog, damit
    /// sie nach einem erfolgreichen Connect sofort aktualisiert werden kann.
    pub(crate) settings_nc_list: std::rc::Rc<std::cell::RefCell<Option<gtk::ListBox>>>,
    /// Quellen-Ids, die gerade **nicht erreichbar** sind (Nextcloud offline) –
    /// steuert den roten „Getrennt"-Hinweis auf deren Covern/Fotos/Liedern.
    pub(crate) offline_sources: std::collections::HashSet<i64>,
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
    /// Widget-Zustand des Nextcloud-Einrichtungsdialogs.
    pub(crate) cloud: crate::ui::app_cloud::CloudState,
    // Zusätzliche Musikquellen (lokaler Zweitordner / Nextcloud) als Tabs.
    /// Geladen aus der `source`-Tabelle (ohne das primäre `music_dir`).
    pub(crate) sources: Vec<Source>,
    /// In der Dateiansicht aktive Quelle (Primär = `music_dir`).
    pub(crate) active_source: ActiveSource,
    /// Tab-Leiste über der Dateiliste (linked ToggleButtons), nur sichtbar,
    /// wenn mindestens eine Zusatzquelle existiert.
    pub(crate) source_tabs: gtk::Box,
    /// Tab-Schaltflächen je Quelle (inkl. Primär) – zum Spiegeln des Aktivzustands.
    pub(crate) source_tab_buttons: Vec<(ActiveSource, gtk::ToggleButton)>,
    /// Aktueller Unterpfad in der entfernten Quelle (relativ zur Musikwurzel,
    /// führender Slash; `""` = Wurzel). Nur gesetzt, wenn eine WebDAV-Quelle aktiv ist.
    pub(crate) remote_browse: Option<String>,
    /// Entfernte (Cloud-)Wiedergabe-Reihe des zuletzt geöffneten Ordners.
    pub(crate) remote_queue: Vec<RemoteTrack>,
    pub(crate) remote_pos: usize,
    /// Läuft gerade eine entfernte Datei (statt lokaler Queue/Episode/Sender)?
    pub(crate) playing_remote: bool,
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
    /// Periodischer, leiser Hintergrund-Nachzug: fehlende Interpreten-Fotos (zuerst)
    /// und Online-Cover nachladen, ohne dass der Nutzer es anstoßen muss.
    AutoEnrichTick,
    /// Bedarfsgesteuerte Fingerprint-Titelerkennung für den **gerade gestarteten**
    /// Titel ohne brauchbare Metadaten (AcoustID), ausgelöst beim Abspielen.
    FingerprintCurrent(PathBuf),
    /// Sprung an eine Position (ms) durch Ziehen/Klicken der Seekleiste.
    Seek(i64),
    Next,
    Prev,
    ToggleShuffle,
    ToggleRepeat,
    NavUp,
    FilesGoStart,
    Refresh,
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
    /// In der Dateiansicht auf eine andere Quelle (Tab) umschalten.
    SelectSource(ActiveSource),
    /// Die Quellenliste hat sich geändert (im Einstellungsdialog hinzugefügt/
    /// entfernt) – Quellen neu laden und die Tab-Leiste aktualisieren.
    SourcesChanged,
    /// Erreichbarkeit der Nextcloud-Quellen prüfen (periodisch + beim Start).
    CheckSources,
    /// Den Nextcloud-Einrichtungsdialog (QR-Scan oder manuell) öffnen.
    AddCloudSource,
    /// Manuelle Eingabe auf-/zugeklappt: Kamera entsprechend aus-/einblenden.
    CloudManualToggle(bool),
    /// Der Nextcloud-Dialog wurde geschlossen (Kamera stoppen).
    CloudClosed,
    /// Ein QR-Code wurde im Nextcloud-Dialog dekodiert.
    CloudQrDecoded(String),
    /// Verbindungstest der eingegebenen Nextcloud-Daten.
    CloudTest,
    /// Die eingegebene Nextcloud-Quelle speichern.
    CloudSave,
    /// Eine entfernte Datei offline herunterladen (rel-Pfad in der aktiven Quelle).
    CtxDownloadRemote(String),
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
    /// Galerie-Ansicht (Cover-Gitter) an/aus; baut die Listen neu auf.
    SetGalleryView(bool),
    /// Kacheln pro Reihe in der Galerie-Ansicht (2–8); baut die Listen neu auf.
    SetGalleryColumns(u32),
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
    /// Galerie-Konzert (Index) öffnen: Album/Ordner → Titelliste, Track → Abspielen.
    OpenConcertEntry(usize),
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
    /// Galerie-Hörbuch (Index) öffnen: Album/Ordner → Titelliste, Track → Abspielen.
    OpenAudiobookEntry(usize),
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
    /// Galerie-Podcast (Index in `podcast_items`) öffnen → `OpenPodcast`.
    OpenPodcastAt(usize),
    /// Abo-Detail eines Galerie-Podcasts (Index in `podcast_items`) → `ShowPodcastDetail`.
    ShowPodcastDetailAt(usize),
    /// Podcast entfernen.
    PodcastDelete(i64),
    /// Feed eines Podcasts neu laden.
    PodcastRefresh(i64),
    /// Beitrag (Episode) umschalten: starten bzw. – wenn schon die laufende –
    /// pausieren/fortsetzen. Vom Antippen der Zeile und vom Play/Pause-Knopf.
    ToggleEpisode { url: String, title: String },
    /// Podcast-Ansicht umschalten (Neuste / Übersicht).
    SetPodcastView(PodcastView),
    /// Streaming-Ansicht umschalten (Kanäle/Aufnahmen).
    SetStreamView(StreamView),
    /// Detailansicht eines Beitrags (Episode) aus der „Neuste"-Liste (Index).
    ShowEpisodeDetail(usize),
    /// Detailansicht einer Episode aus der Episodenliste eines Podcasts.
    ShowPodcastEpisodeDetail { podcast_id: i64, index: usize },
    /// Klick auf eine Zeitsprungmarke in den Shownotes: an die Stelle springen
    /// (Episode bei Bedarf dort starten).
    EpisodeSeekTo { url: String, title: String, ms: i64 },
    /// Detailansicht/Verwaltung eines Abos (Podcast-Id) – Aktualisieren/Entfernen.
    ShowPodcastDetail(i64),
    // Streaming (Internet-Radio)
    /// Hinzufügen-Dialog (Suche + Stream-Adresse) öffnen.
    StreamAdd,
    /// Sender zu diesem Suchbegriff suchen (Radio-Browser, im Hintergrund).
    StreamSearch(String),
    /// Einen Suchtreffer (Index in `stream_search_results`) als Sender speichern.
    StreamAddResult(usize),
    /// Eine Stream-Adresse manuell als Sender speichern.
    StreamAddUrl(String),
    /// Sender antippen: startet ihn, bei laufendem Sender Pause/Weiter umschalten.
    ToggleStream(i64),
    /// Aufnahme-Knopf einer Senderzeile: startet/stoppt die Daueraufnahme.
    StreamRecordToggle(i64),
    /// Aufnahme-Knopf in der Player-Leiste: nimmt den laufenden Sender auf/stoppt.
    TransportRecordToggle,
    /// Titel-Tag aus der Wiedergabe (bei Sendern: der laufende ICY-Titel).
    StreamTitle(String),
    /// Detailseite eines Senders öffnen.
    OpenStream(i64),
    /// Einen Sender entfernen.
    StreamDelete(i64),
    // Aufnahme (Timeshift)
    /// Laufende Aufnahme stoppen.
    RecordStop,
    /// Wiederholungs-Unterseite eines Senders öffnen.
    OpenStreamReplay(i64),
    /// Einen gepufferten Song probehören (absoluter Byte-Bereich).
    ReplayPlay { start: u64, end: u64 },
    /// Einen gepufferten Song nachträglich speichern.
    ReplaySave {
        start: u64,
        end: u64,
        title: String,
    },
    /// Einen gespeicherten Mitschnitt abspielen (Pfad).
    PlayRecording(String),
    /// Einen Mitschnitt löschen (Id).
    RecordingDelete(i64),
    /// Größe des Timeshift-Puffers in Minuten setzen (0–60).
    SetRecordingBufferMinutes(u32),
}

/// Ergebnisse der Hintergrund-Worker (Ordner lesen bzw. Online-Anreicherung).
#[derive(Debug)]
pub enum Cmd {
    Entries(Vec<FsEntry>),
    /// Ergebnis eines WebDAV-Verzeichnislistings (Hintergrund-PROPFIND). Trägt die
    /// Quelle und den rel-Pfad mit, damit ein zwischenzeitlicher Quellen-/Ordner-
    /// wechsel das veraltete Ergebnis verwerfen kann.
    RemoteEntries(
        Result<Vec<crate::core::webdav::DavEntry>, String>,
        ActiveSource,
        String,
    ),
    /// Nachgeladene Tags entfernter Dateien: (rel-Pfad, Titel, Interpret, Dauer).
    RemoteTags(Vec<(String, Option<String>, Option<String>, Option<i64>)>),
    /// Eine entfernte Datei wurde heruntergeladen: (rel-Pfad, lokale Kopie) oder Fehler.
    RemoteDownloaded(Result<(String, PathBuf), String>),
    /// Ergebnis des Nextcloud-Verbindungstests.
    WebdavTested(Result<(), String>),
    /// Online-Anreicherung abgeschlossen; `changed` = es kam etwas Neues hinzu
    /// (steuert beim leisen Nachzug, ob die Ansichten neu geladen werden).
    EnrichDone { changed: bool },
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
    /// Treffer der Sendersuche (für den offenen Hinzufügen-Dialog).
    StreamSearchResults(Vec<crate::core::streaming::StationResult>),
    /// Logos der Suchtreffer sind gecacht → Trefferliste neu zeichnen.
    StreamSearchCoversReady,
    /// Senderliste neu aufbauen (z. B. nachdem Logos gecacht wurden).
    ReloadStreams,
    /// Eine Nextcloud-Quelle wurde indiziert → Alben/Interpreten neu laden.
    RemoteIndexed,
    /// Erreichbarkeit der Quellen (Quellen-Id → erreichbar?).
    SourceStatus(Vec<(i64, bool)>),
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

                    // Inhalt mit Lade-Overlay. Desktop: etwas Luft **zwischen
                    // Titelleiste und Inhalt** (oben); im schmalen (mobilen) Modus
                    // per Breakpoint wieder auf 0 (siehe `init`).
                    #[wrap(Some)]
                    #[name = "content_overlay"]
                    set_content = &gtk::Overlay {
                        set_margin_top: 10,
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

                                        // Quellen-Tabs (linked) – nur sichtbar, wenn neben dem
                                        // primären Musikordner mindestens eine Zusatzquelle
                                        // (SD-Karte/Nextcloud) eingerichtet ist. Befüllt in
                                        // `rebuild_source_tabs`.
                                        #[name = "source_tabs"]
                                        gtk::Box {
                                            set_orientation: gtk::Orientation::Horizontal,
                                            add_css_class: "linked",
                                            set_halign: gtk::Align::Center,
                                            set_margin_top: 6,
                                            // Etwas Luft unter dem Quellen-Tab-Menü.
                                            set_margin_bottom: 10,
                                            #[watch]
                                            set_visible: !model.sources.is_empty(),
                                        },

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
                                        set_visible: model.artist_count > 0 && !model.gallery_view,
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
                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.artist_count > 0 && model.gallery_view,
                                        #[local_ref]
                                        artists_gallery -> gtk::FlowBox {
                                            set_valign: gtk::Align::Start,
                                            set_margin_top: 0,
                                            set_margin_bottom: 12,
                                            set_margin_start: 12,
                                            set_margin_end: 12,
                                        },
                                    },
                                },
                            add_titled_with_icon[Some("albums"), &gettext("Albums"), "media-optical-symbolic"] =
                                &gtk::Box {
                                    set_orientation: gtk::Orientation::Vertical,

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
                                        set_visible: model.album_count > 0 && !model.gallery_view,
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
                                    // Galerie-Variante (Cover-Gitter)
                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.album_count > 0 && model.gallery_view,
                                        #[local_ref]
                                        albums_gallery -> gtk::FlowBox {
                                            set_valign: gtk::Align::Start,
                                            set_margin_top: 0,
                                            set_margin_bottom: 12,
                                            set_margin_start: 12,
                                            set_margin_end: 12,
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
                                        set_visible: !model.concert_items.is_empty() && !model.gallery_view,
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
                                    // Galerie-Variante der Konzerte
                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: !model.concert_items.is_empty() && model.gallery_view,
                                        #[local_ref]
                                        concerts_gallery -> gtk::FlowBox {
                                            set_valign: gtk::Align::Start,
                                            set_margin_top: 0,
                                            set_margin_bottom: 12,
                                            set_margin_start: 12,
                                            set_margin_end: 12,
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
                                        set_visible: model.podcast_view == PodcastView::Overview && !model.podcast_items.is_empty() && !model.gallery_view,
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
                                    // Galerie-Variante der Abo-Übersicht
                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.podcast_view == PodcastView::Overview && !model.podcast_items.is_empty() && model.gallery_view,
                                        #[local_ref]
                                        podcasts_gallery -> gtk::FlowBox {
                                            set_valign: gtk::Align::Start,
                                            set_margin_top: 10,
                                            set_margin_bottom: 12,
                                            set_margin_start: 12,
                                            set_margin_end: 12,
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
                            add_titled_with_icon[Some("streaming"), &gettext("Streaming"), "audio-x-generic-symbolic"] =
                                &gtk::Box {
                                    set_orientation: gtk::Orientation::Vertical,

                                    // Tab-Umschalter: Kanäle / Aufnahmen + „+" für einen neuen Kanal.
                                    gtk::Box {
                                        set_spacing: 6,
                                        set_margin_top: 6,
                                        set_margin_bottom: 6,
                                        set_margin_start: 12,
                                        set_margin_end: 12,
                                        add_css_class: "linked",
                                        gtk::ToggleButton {
                                            set_label: &gettext("Stations"),
                                            set_hexpand: true,
                                            #[watch]
                                            set_active: model.stream_view == StreamView::Channels,
                                            connect_clicked => Msg::SetStreamView(StreamView::Channels),
                                        },
                                        gtk::ToggleButton {
                                            set_label: &gettext("Recordings"),
                                            set_hexpand: true,
                                            #[watch]
                                            set_active: model.stream_view == StreamView::Recordings,
                                            connect_clicked => Msg::SetStreamView(StreamView::Recordings),
                                        },
                                        gtk::Button {
                                            set_icon_name: "list-add-symbolic",
                                            set_tooltip_text: Some(&gettext("Add station")),
                                            add_css_class: "flat",
                                            connect_clicked => Msg::StreamAdd,
                                        },
                                    },

                                    // Kanäle.
                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.stream_view == StreamView::Channels && !model.stream_items.is_empty(),
                                        #[local_ref]
                                        streams_list -> gtk::ListBox {
                                            set_valign: gtk::Align::Start,
                                            set_margin_top: 4,
                                            set_margin_bottom: 12,
                                            set_margin_start: 12,
                                            set_margin_end: 12,
                                            set_css_classes: &["boxed-list"],
                                        },
                                    },
                                    adw::StatusPage {
                                        set_icon_name: Some("audio-x-generic-symbolic"),
                                        set_title: &gettext("No stations"),
                                        set_description: Some(&gettext("Add a stream address or search for a station worldwide.")),
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.stream_view == StreamView::Channels && model.stream_items.is_empty(),
                                    },

                                    // Aufnahmen.
                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: model.stream_view == StreamView::Recordings && !model.recording_items.is_empty(),
                                        #[local_ref]
                                        recordings_list -> gtk::ListBox {
                                            set_valign: gtk::Align::Start,
                                            set_margin_top: 4,
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
                                        set_visible: model.stream_view == StreamView::Recordings && model.recording_items.is_empty(),
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
                                        set_visible: !model.audiobook_items.is_empty() && !model.gallery_view,
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
                                    // Galerie-Variante der Hörbücher
                                    gtk::ScrolledWindow {
                                        set_vexpand: true,
                                        #[watch]
                                        set_visible: !model.audiobook_items.is_empty() && model.gallery_view,
                                        #[local_ref]
                                        audiobooks_gallery -> gtk::FlowBox {
                                            set_valign: gtk::Align::Start,
                                            set_margin_top: 0,
                                            set_margin_bottom: 12,
                                            set_margin_start: 12,
                                            set_margin_end: 12,
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
                            // Lied/Interpret etwas tiefer setzen (kompaktere Leiste).
                            set_margin_top: 5,
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
                                // Aufnahme-Knopf direkt neben Play/Pause, etwas tiefer
                                // (~10px). Roter Punkt; blinkt während der Aufnahme.
                                // Nur sichtbar, wenn ein Sender läuft und der Puffer an ist.
                                gtk::Button {
                                    set_icon_name: "media-record-symbolic",
                                    set_tooltip_text: Some(&gettext("Record")),
                                    set_valign: gtk::Align::Start,
                                    set_margin_top: 10,
                                    #[watch]
                                    set_visible: model.playing_stream.is_some()
                                        && model.recording_buffer_minutes > 0,
                                    #[watch]
                                    set_css_classes: if model.record_state.is_some() {
                                        &["flat", "circular", "emilia-record-dot", "emilia-recording"]
                                    } else {
                                        &["flat", "circular", "emilia-record-dot"]
                                    },
                                    connect_clicked => Msg::TransportRecordToggle,
                                },
                                gtk::Button {
                                    set_icon_name: "media-skip-forward-symbolic",
                                    set_tooltip_text: Some(&gettext("Forward")),
                                    add_css_class: "flat",
                                    #[watch]
                                    set_sensitive: model.now_playing.is_some(),
                                    connect_clicked => Msg::Next,
                                },
                            },
                            // Rechts unten: Wiederholen (mittig zwischen „Vor" und der
                            // Warteschlange) und die Warteschlange.
                            #[wrap(Some)]
                            set_end_widget = &gtk::Box {
                                set_spacing: 18,
                                set_valign: gtk::Align::Center,
                                // Wiederholen (Loop): am Ende der Warteschlange bzw.
                                // des Einzeltitels von vorn. Aktiv = weiß, aus = grau.
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
                                gtk::Button {
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
                 button.emilia-record-dot image { color: @error_color; }\
                 @keyframes emilia-blink { 0% { opacity: 1; } 50% { opacity: 0.25; } 100% { opacity: 1; } }\
                 button.emilia-recording image { animation: emilia-blink 1.1s ease-in-out infinite; }\
                 button.emilia-nav-btn:checked image { color: @accent_color; }\
                 image.emilia-offline { color: white; background-color: @error_color; border-radius: 999px; padding: 2px; margin: 2px; }\
                 box.emilia-loading { background-color: alpha(@window_bg_color, 0.85); border-radius: 18px; padding: 22px 30px; }\
                 progressbar.emilia-hourbar, progressbar.emilia-hourbar > trough, progressbar.emilia-hourbar > trough > progress { min-width: 0px; }\
                 label.emilia-gallery-title { background-color: alpha(black, 0.55); color: white; padding: 3px 8px; border-bottom-left-radius: 6px; border-bottom-right-radius: 6px; }\
                 flowbox.emilia-gallery > flowboxchild { padding: 0px; border-radius: 6px; }\
                 flowbox.emilia-gallery > flowboxchild:selected { background: none; }",
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

        // Zusätzliche Musikquellen (lokaler Zweitordner / Nextcloud) für die Tabs.
        let sources = library.list_sources().unwrap_or_default();

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
        // Galerie-Ansicht (Standard: aus) und Kacheln/Reihe (Standard: 3, 2–8).
        let gallery_view = matches!(
            library.get_setting("gallery_view").ok().flatten().as_deref(),
            Some("1")
        );
        let gallery_columns = library
            .get_setting("gallery_columns")
            .ok()
            .flatten()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(3)
            .clamp(2, 8);
        // Timeshift-Puffer für Sender in Minuten (Standard 5, 0 = aus, max. 60).
        let recording_buffer_minutes = library
            .get_setting("recording_buffer_minutes")
            .ok()
            .flatten()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(5)
            .min(60);
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

        // Beim Titelende automatisch den nächsten Eintrag der Warteschlange spielen;
        // Titel-Tags (bei Sendern: der laufende ICY-Titel) als `StreamTitle` melden.
        {
            let sender = sender.clone();
            player.connect_bus_events(
                {
                    let sender = sender.clone();
                    move || sender.input(Msg::TrackFinished)
                },
                move |title| sender.input(Msg::StreamTitle(title)),
            );
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

        // Leiser Hintergrund-Nachzug: füllt nach und nach fehlende Interpreten-Fotos
        // (zuerst) und Online-Cover, ohne Zutun des Nutzers – damit auch ohne neuen
        // Scan (zurückkehrende Nutzer, Funkloch beim ersten Lauf, fehlgeschlagene
        // Einzelabrufe) die Übersicht aufgewertet wird. Der Worker ist rate-limitiert
        // und überspringt bereits Geladenes/dauerhaft Erfolgloses; ist nichts offen,
        // verpufft der Tick nahezu kostenlos (kein Netz, keine UI-Aktualisierung).
        {
            let sender = sender.clone();
            gtk::glib::timeout_add_seconds_local(AUTO_ENRICH_INTERVAL_SECS, move || {
                sender.input(Msg::AutoEnrichTick);
                gtk::glib::ControlFlow::Continue
            });
        }

        // Erreichbarkeit der Nextcloud-Quellen einmal beim Start und danach
        // regelmäßig prüfen (steuert den roten „Getrennt"-Hinweis).
        {
            let sender = sender.clone();
            sender.input(Msg::CheckSources);
            gtk::glib::timeout_add_seconds_local(45, move || {
                sender.input(Msg::CheckSources);
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
        let streams_list = gtk::ListBox::new();
        let recordings_list = gtk::ListBox::new();
        let newest_list = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let favorites_list = gtk::ListBox::new();
        let audiobooks_list = gtk::ListBox::new();
        let queue_list = gtk::ListBox::new();
        let stats_box = gtk::Box::new(gtk::Orientation::Vertical, 0);

        let mut model = App {
            library,
            player,
            mpris,
            input: sender.input_sender().clone(),
            entries,
            albums,
            albums_gallery: gtk::FlowBox::new(),
            albums_overview: Vec::new(),
            artists_gallery: gtk::FlowBox::new(),
            artists_overview: Vec::new(),
            album_count: 0,
            artists,
            artist_count: 0,
            enriching: false,
            auto_enrich,
            enrich_cancel: Arc::new(AtomicBool::new(false)),
            acoustid_key,
            fanart_key,
            ui_language,
            gallery_view,
            gallery_columns,
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
            concerts_gallery: gtk::FlowBox::new(),
            favorite_items: Vec::new(),
            favorites_list: favorites_list.clone(),
            audiobook_items: Vec::new(),
            audiobooks_list: audiobooks_list.clone(),
            audiobooks_gallery: gtk::FlowBox::new(),
            playlist_items: Vec::new(),
            playlists_list: playlists_list.clone(),
            podcast_items: Vec::new(),
            podcasts_list: podcasts_list.clone(),
            podcasts_gallery: gtk::FlowBox::new(),
            podcast_view: PodcastView::Newest,
            stream_view: StreamView::Channels,
            newest_items: Vec::new(),
            newest_list: newest_list.clone(),
            podcast_search_results: Vec::new(),
            podcast_search: std::rc::Rc::new(std::cell::RefCell::new(None)),
            playing_episode_url: None,
            stream_items: Vec::new(),
            streams_list: streams_list.clone(),
            stream_search_results: Vec::new(),
            stream_search: std::rc::Rc::new(std::cell::RefCell::new(None)),
            playing_stream: None,
            stream_title: None,
            recorder: None,
            record_state: None,
            recording_buffer_minutes,
            recording_items: Vec::new(),
            recordings_list: recordings_list.clone(),
            stream_play_buttons: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
            settings_nc_list: std::rc::Rc::new(std::cell::RefCell::new(None)),
            offline_sources: std::collections::HashSet::new(),
            episode_play_buttons: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
            ctx_episode_play: std::rc::Rc::new(std::cell::RefCell::new(None)),
            queue_list: queue_list.clone(),
            stats_box: stats_box.clone(),
            stats_period: StatsPeriod::All,
            concert_hint_dismissed,
            gallery_tried: std::cell::RefCell::new(std::collections::HashSet::new()),
            gallery_hooked: std::cell::RefCell::new(std::collections::HashSet::new()),
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
            cloud: crate::ui::app_cloud::CloudState::default(),
            sources,
            active_source: ActiveSource::Primary,
            source_tabs: gtk::Box::new(gtk::Orientation::Horizontal, 0),
            source_tab_buttons: Vec::new(),
            remote_browse: None,
            remote_queue: Vec::new(),
            remote_pos: 0,
            playing_remote: false,
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
            model.now_playing = Some(model.display_name(&q[q_pos]));
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
        model.reload_streams(&sender);
        model.reload_recordings(&sender);
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
        // Genauso die Sender-Logos einmalig im Hintergrund cachen.
        sender.spawn_oneshot_command(|| {
            if let Ok(lib) = Library::open() {
                for st in lib.streams().unwrap_or_default() {
                    if let Some(url) = st.favicon {
                        crate::core::online::cache_station_image(&url);
                    }
                }
            }
            Cmd::ReloadStreams
        });
        // Bibliothek beim Start automatisch einlesen und – bei WLAN/LAN und
        // aktiviertem Schalter – fehlende Cover/Metadaten im Hintergrund nachladen.
        model.start_scan(&sender, true);

        let entries_box = model.entries.widget();
        let albums_box = model.albums.widget();
        let artists_box = model.artists.widget();
        let albums_gallery = model.albums_gallery.clone();
        let artists_gallery = model.artists_gallery.clone();
        let concerts_gallery = model.concerts_gallery.clone();
        let audiobooks_gallery = model.audiobooks_gallery.clone();
        let podcasts_gallery = model.podcasts_gallery.clone();
        let widgets = view_output!();
        model.view_stack = widgets.view_stack.clone();
        model.nav_view = widgets.nav_view.clone();
        model.split = widgets.split.clone();
        model.seek_scale = widgets.seek_scale.clone();
        model.chapter_label = widgets.chapter_label.clone();
        model.source_tabs = widgets.source_tabs.clone();
        model.rebuild_source_tabs();

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
        // Der Desktop-Abstand zwischen Titelleiste und Inhalt entfällt im schmalen Modus.
        breakpoint.add_setter(&widgets.content_overlay, "margin-top", Some(&0i32.to_value()));
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
                // Aktiven Menüpunkt am Icon blau hervorheben (CSS `:checked`).
                btn.add_css_class("emilia-nav-btn");
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
                let Some(entry) = entry else {
                    return;
                };
                // Entfernte Einträge (Nextcloud) laufen über einen eigenen Pfad.
                if let crate::ui::fs_row::FsEntry::RemoteDir { rel_path, .. } = &entry {
                    self.remote_browse = Some(rel_path.clone());
                    self.load_dir(&sender);
                    return;
                }
                if let crate::ui::fs_row::FsEntry::RemoteFile { rel_path, .. } = &entry {
                    let rel = rel_path.clone();
                    self.activate_remote(&rel);
                    return;
                }
                {
                    if entry.is_dir() {
                        let Some(p) = entry.path().cloned() else {
                            return;
                        };
                        self.browse_dir = Some(p);
                        self.load_dir(&sender);
                    } else {
                        let Some(path) = entry.path().cloned() else {
                            return;
                        };
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
                // Entfernte Dateien wandern (noch) nicht in die lokale Queue –
                // `path()` ist dort `None`, der Doppelklick bleibt wirkungslos.
                let path = self
                    .entries
                    .guard()
                    .get(index)
                    .filter(|r| !r.entry.is_dir())
                    .and_then(|r| r.entry.path().cloned());
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
                let meta = self
                    .artists
                    .guard()
                    .get(index)
                    .map(|c| c.meta.clone())
                    .or_else(|| self.artists_overview.get(index).cloned());
                if let Some(meta) = meta {
                    // Foto des geöffneten Interpreten vorrangig nachladen.
                    self.fetch_focus_artist(&sender, &meta.name);
                    self.context_target = Some(CtxTarget::Artist(meta));
                    self.open_context_menu(root, &sender);
                }
            }
            Msg::ShowAlbumDetail(index) => {
                let meta = self
                    .albums
                    .guard()
                    .get(index)
                    .map(|c| c.meta.clone())
                    .or_else(|| self.albums_overview.get(index).cloned());
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
                let album = self
                    .albums
                    .guard()
                    .get(index)
                    .map(|c| c.meta.album.clone())
                    .or_else(|| self.albums_overview.get(index).map(|m| m.album.clone()));
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
                let meta = self
                    .artists
                    .guard()
                    .get(index)
                    .map(|c| c.meta.clone())
                    .or_else(|| self.artists_overview.get(index).cloned());
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
            Msg::OpenPodcastAt(index) => {
                if let Some(id) = self.podcast_items.get(index).map(|p| p.0) {
                    sender.input(Msg::OpenPodcast(id));
                }
            }
            Msg::ShowPodcastDetailAt(index) => {
                if let Some(id) = self.podcast_items.get(index).map(|p| p.0) {
                    sender.input(Msg::ShowPodcastDetail(id));
                }
            }
            Msg::PodcastDelete(id) => {
                let _ = self.library.delete_podcast(id);
                self.reload_podcasts(&sender);
                self.toast(&gettext("Podcast removed"));
            }
            // --- Streaming (Internet-Radio) ---
            Msg::StreamAdd => self.open_add_stream_dialog(root, &sender),
            Msg::StreamSearch(term) => {
                let term = term.trim().to_string();
                if !term.is_empty() {
                    self.toast(&gettext("Searching …"));
                    sender.spawn_command(move |out| {
                        let results =
                            crate::core::streaming::search_stations(&term).unwrap_or_default();
                        // Treffer sofort zeigen (noch ohne Logos) …
                        let _ = out.send(Cmd::StreamSearchResults(results.clone()));
                        // … und die Logos danach im Hintergrund nachladen.
                        for r in &results {
                            if let Some(img) = r.favicon.as_deref() {
                                crate::core::online::cache_station_image(img);
                            }
                        }
                        let _ = out.send(Cmd::StreamSearchCoversReady);
                    });
                }
            }
            Msg::StreamAddResult(index) => self.add_stream_result(&sender, index),
            Msg::StreamAddUrl(url) => {
                let url = url.trim().to_string();
                if !url.is_empty() {
                    let name = crate::core::streaming::name_from_url(&url);
                    match self
                        .library
                        .add_stream(&name, &url, None, None, None, None, None)
                    {
                        Ok(_) => {
                            self.reload_streams(&sender);
                            self.toast(&gettext("Station added"));
                        }
                        Err(_) => self.toast(&gettext("Could not add station")),
                    }
                }
            }
            Msg::ToggleStream(id) => {
                if self.playing_stream == Some(id) {
                    // Läuft schon → Pause/Weiter umschalten (Puffer läuft weiter).
                    if self.playing {
                        self.player.pause();
                        self.playing = false;
                    } else {
                        self.player.resume();
                        self.playing = true;
                    }
                    self.mpris.set_playing(self.playing);
                } else {
                    self.play_stream(id);
                }
                self.refresh_stream_icons();
            }
            Msg::StreamRecordToggle(id) => {
                if self.record_state.as_ref().map(|r| r.stream_id) == Some(id) {
                    // Läuft → stoppen.
                    sender.input(Msg::RecordStop);
                } else if self.recording_buffer_minutes == 0 {
                    self.toast(&gettext("Enable the recording buffer in the settings first"));
                } else {
                    // Sender (mit Puffer) sicherstellen, dann Daueraufnahme starten.
                    if self.playing_stream != Some(id) {
                        self.play_stream(id);
                    }
                    self.record_arm(id);
                    self.refresh_stream_icons();
                }
            }
            Msg::TransportRecordToggle => {
                if let Some(id) = self.playing_stream {
                    sender.input(Msg::StreamRecordToggle(id));
                }
            }
            Msg::StreamTitle(title) => {
                // Nur relevant, solange ein Sender läuft (Datei-/Episoden-Tags
                // werden ignoriert). Zeigt „Sender — Titel" im Mini-Player und
                // meldet den Titel an Sperrbildschirm/Medientasten.
                let title = title.trim().to_string();
                if let Some(id) = self.playing_stream {
                    if !title.is_empty() && self.stream_title.as_deref() != Some(title.as_str()) {
                        self.stream_title = Some(title.clone());
                        let station = self
                            .stream_items
                            .iter()
                            .find(|s| s.id == id)
                            .map(|s| s.name.clone());
                        self.now_playing = Some(match &station {
                            Some(name) => format!("{name} — {title}"),
                            None => title.clone(),
                        });
                        self.mpris
                            .set_metadata(0, &title, station.as_deref(), None, None, None);
                    }
                }
            }
            Msg::OpenStream(id) => self.open_stream(root, &sender, id),
            Msg::StreamDelete(id) => {
                if self.playing_stream == Some(id) {
                    self.player.stop();
                    self.playing = false;
                    self.playing_stream = None;
                    self.now_playing = None;
                    self.mpris.set_playing(false);
                    self.stop_recorder();
                }
                let _ = self.library.delete_stream(id);
                self.reload_streams(&sender);
                self.toast(&gettext("Station removed"));
            }
            // --- Aufnahme (Timeshift) ---
            Msg::RecordStop => {
                if self.record_state.take().is_some() {
                    self.toast(&gettext("Recording stopped"));
                    self.reload_recordings(&sender);
                }
            }
            Msg::OpenStreamReplay(id) => self.open_stream_replay(&sender, id),
            Msg::ReplayPlay { start, end } => {
                let temp = self.recorder.as_ref().and_then(|r| r.extract_temp(start, end).ok());
                match temp {
                    Some(path) => {
                        let p = path.to_string_lossy().to_string();
                        self.player.stop();
                        match self.player.play_file(&p, 0) {
                            Ok(()) => {
                                self.now_playing = Some(gettext("Replay"));
                                self.playing = true;
                                self.playing_path = Some(path);
                                self.playing_episode_url = None;
                                self.playing_stream = None;
                                self.mpris.set_playing(true);
                            }
                            Err(e) => tracing::error!("Replay failed: {e}"),
                        }
                    }
                    None => self.toast(&gettext("Could not extract from buffer")),
                }
            }
            Msg::ReplaySave { start, end, title } => {
                let (artist, t) = crate::core::recorder::split_artist_title(&title);
                let station = self
                    .playing_stream
                    .and_then(|id| self.stream_items.iter().find(|s| s.id == id))
                    .map(|s| s.name.clone());
                let dest = crate::ui::app_streaming::recordings_dir();
                let saved = self
                    .recorder
                    .as_ref()
                    .and_then(|r| r.save_song(start, end, artist.as_deref(), &t, &dest).ok());
                match saved {
                    Some(path) => {
                        let _ = self.library.add_recording(
                            &path.to_string_lossy(),
                            artist.as_deref(),
                            &t,
                            station.as_deref(),
                            false,
                        );
                        self.reload_recordings(&sender);
                        // Cover + Album online nachschlagen und einbetten (Hintergrund).
                        let (a, ti) = (artist.clone(), t.clone());
                        sender.spawn_command(move |_out| {
                            let aref = a.as_deref().unwrap_or("");
                            if let Some((bytes, album)) =
                                crate::core::online::recording_cover(aref, &ti)
                            {
                                crate::core::recorder::embed_cover(
                                    &path,
                                    a.as_deref(),
                                    &ti,
                                    album.as_deref(),
                                    &bytes,
                                );
                            }
                        });
                    }
                    None => {}
                }
            }
            Msg::PlayRecording(path) => {
                let p = PathBuf::from(&path);
                if p.exists() {
                    self.stop_recorder();
                    self.queue = vec![p];
                    self.queue_pos = 0;
                    self.play_current();
                } else {
                    self.toast(&gettext("File not found"));
                }
            }
            Msg::RecordingDelete(id) => {
                if let Ok(Some(path)) = self.library.delete_recording(id) {
                    let _ = std::fs::remove_file(&path);
                }
                self.reload_recordings(&sender);
                self.toast(&gettext("Recording deleted"));
            }
            Msg::SetRecordingBufferMinutes(n) => {
                self.recording_buffer_minutes = n.min(60);
                let _ = self.library.set_setting(
                    "recording_buffer_minutes",
                    &self.recording_buffer_minutes.to_string(),
                );
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
            Msg::SetStreamView(view) => self.stream_view = view,
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
                if self.playing_remote {
                    // Entfernte Reihe: zum nächsten Titel weiterschalten (bzw. am
                    // Ende stoppen). Läuft getrennt von der lokalen Warteschlange.
                    self.remote_next();
                } else if self.playing_episode_url.is_some() && self.queue.is_empty() {
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
                // Laufende Timeshift-Aufnahme an den Songgrenzen fortschreiben.
                if self.record_state.is_some() {
                    self.drive_recording(&sender);
                }
                // Play/Pause- und Aufnahme-Icons der Senderzeilen abgleichen.
                self.refresh_stream_icons();
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
            Msg::AutoEnrichTick => {
                // Leiser Nachzug fehlender Interpreten-Fotos & Online-Cover im
                // Hintergrund (rate-limitiert im Worker). Nur, wenn gewünscht, ein
                // Ordner gesetzt ist, gerade kein Lauf aktiv ist und Netz besteht.
                // Läuft schon ein (voller) Abruf, greift die `enriching`-Sperre und
                // dieser Tick verpufft – kein Aufstauen.
                if self.auto_enrich
                    && !self.enriching
                    && self.music_dir.is_some()
                    && online_available()
                {
                    self.run_enrich(&sender, false, true);
                }
            }
            Msg::FingerprintCurrent(path) => self.fetch_focus_track(&sender, &path),
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
                    M::Next => {
                        if self.playing_remote {
                            self.remote_next();
                        } else {
                            self.play_next();
                        }
                    }
                    M::Prev => {
                        if self.playing_remote {
                            self.remote_prev();
                        } else {
                            self.play_prev();
                        }
                    }
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
            Msg::Next => {
                if self.playing_remote {
                    self.remote_next();
                } else {
                    self.play_next();
                }
            }
            Msg::Prev => {
                if self.playing_remote {
                    self.remote_prev();
                } else {
                    self.play_prev();
                }
            }
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
                // Entfernte Quelle: ein rel-Segment nach oben.
                if let Some(rel) = self.remote_browse.clone() {
                    if !rel.is_empty() {
                        let parent = match rel.rfind('/') {
                            Some(0) | None => String::new(),
                            Some(i) => rel[..i].to_string(),
                        };
                        self.remote_browse = Some(parent);
                        self.load_dir(&sender);
                    }
                    return;
                }
                if self.can_go_up() {
                    if let Some(parent) = self.browse_dir.as_ref().and_then(|d| d.parent()) {
                        self.browse_dir = Some(parent.to_path_buf());
                        self.load_dir(&sender);
                    }
                }
            }
            Msg::FilesGoStart => {
                // Entfernte Quelle: zurück an die Musikwurzel der Quelle.
                if self.remote_browse.is_some() {
                    if self.remote_browse.as_deref() != Some("") {
                        self.remote_browse = Some(String::new());
                        self.load_dir(&sender);
                    }
                    return;
                }
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
                // Die Dateiansicht nur umrooten, wenn gerade der primäre Tab aktiv
                // ist – auf einer Zusatzquelle bliebe der Nutzer sonst stehen.
                if self.active_source == ActiveSource::Primary {
                    self.root_dir = Some(path.clone());
                    self.browse_dir = Some(path);
                    self.load_dir(&sender);
                }
                // Neuen Ordner einlesen und (WLAN + Schalter) automatisch nachladen.
                self.start_scan(&sender, true);
            }
            Msg::SelectSource(sel) => {
                if self.active_source != sel {
                    self.apply_source(sel, &sender);
                }
            }
            Msg::SourcesChanged => {
                self.sources = self.library.list_sources().unwrap_or_default();
                // Ist die aktive Quelle entfernt worden, zurück auf den primären Tab.
                let gone = match &self.active_source {
                    ActiveSource::Primary => false,
                    ActiveSource::Source(id) => !self.sources.iter().any(|s| s.id == *id),
                };
                if gone {
                    self.apply_source(ActiveSource::Primary, &sender);
                }
                self.rebuild_source_tabs();
                // Indizierte Cloud-Titel können dazugekommen/entfernt worden sein.
                self.reload_albums();
                self.reload_artists();
                // „Verbunden"-Liste der Nextcloud-Einstellungsseite auffrischen,
                // falls der Einstellungsdialog gerade offen ist.
                let nc_list = self.settings_nc_list.borrow().clone();
                if let Some(list) = nc_list {
                    if list.root().is_some() {
                        self.fill_nc_list(&list, &sender);
                    } else {
                        *self.settings_nc_list.borrow_mut() = None;
                    }
                }
            }
            Msg::CheckSources => {
                let webdavs: Vec<crate::model::Source> = self
                    .sources
                    .iter()
                    .filter(|s| s.kind == "webdav")
                    .cloned()
                    .collect();
                if !webdavs.is_empty() {
                    sender.spawn_command(move |out| {
                        let status: Vec<(i64, bool)> = webdavs
                            .iter()
                            .map(|s| {
                                let ok = crate::core::webdav::Creds::from_source(s)
                                    .map(|c| crate::core::webdav::test_connection(&c).is_ok())
                                    .unwrap_or(false);
                                (s.id, ok)
                            })
                            .collect();
                        let _ = out.send(Cmd::SourceStatus(status));
                    });
                }
            }
            Msg::AddCloudSource => self.open_cloud_dialog(root, &sender),
            Msg::CloudManualToggle(expanded) => {
                if expanded {
                    // Manuell aufgeklappt → Kamera anhalten und ausblenden.
                    self.cloud.scanner = None;
                    if let Some(cam) = &self.cloud.cam {
                        cam.set_visible(false);
                    }
                } else {
                    // Wieder zugeklappt → Kamera erneut starten.
                    self.start_cloud_scan(&sender);
                }
            }
            Msg::CloudClosed => {
                self.cloud.scanner = None;
                self.cloud.dialog = None;
            }
            Msg::CloudQrDecoded(code) => self.handle_cloud_qr(&code),
            Msg::CloudTest => self.test_cloud(&sender),
            Msg::CloudSave => self.save_cloud(&sender),
            Msg::CtxDownloadRemote(rel) => {
                let Some(creds) = self.active_webdav_creds() else {
                    return;
                };
                let Some(dest) = self.remote_cache_path(&rel) else {
                    return;
                };
                self.toast(&gettext("Downloading …"));
                sender.spawn_oneshot_command(move || {
                    match crate::core::webdav::download(&creds, &rel, &dest) {
                        Ok(()) => Cmd::RemoteDownloaded(Ok((rel, dest))),
                        Err(e) => Cmd::RemoteDownloaded(Err(e.to_string())),
                    }
                });
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
            Msg::SetGalleryView(on) => {
                self.gallery_view = on;
                let _ = self
                    .library
                    .set_setting("gallery_view", if on { "1" } else { "0" });
                self.rebuild_all_lists(&sender);
            }
            Msg::SetGalleryColumns(n) => {
                self.gallery_columns = n.clamp(2, 8);
                let _ = self
                    .library
                    .set_setting("gallery_columns", &self.gallery_columns.to_string());
                if self.gallery_view {
                    self.rebuild_all_lists(&sender);
                }
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
                // Konzert-Import bezieht sich auf die primäre Bibliothek.
                let Some(root) = self.music_dir.as_ref().map(PathBuf::from) else {
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
            Msg::OpenConcertEntry(index) => {
                // Galerie-Tipp: wie der Listen-Tipp – Album/Ordner öffnet die
                // Titelliste, ein einzelner Titel wird abgespielt.
                if let Some((scope, key, _, is_dir)) = self.concert_items.get(index).cloned() {
                    if scope == "track" {
                        self.play_entry(&scope, &key, is_dir);
                    } else {
                        sender.input(Msg::OpenEntryTracks { scope, key });
                    }
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
            Msg::OpenAudiobookEntry(index) => {
                // Galerie-Tipp: Album/Ordner öffnet die Titelliste, Einzeltitel spielt.
                if let Some((scope, key, _, is_dir)) = self.audiobook_items.get(index).cloned() {
                    if scope == "track" {
                        self.play_entry(&scope, &key, is_dir);
                    } else {
                        sender.input(Msg::OpenEntryTracks { scope, key });
                    }
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
                } else if self.playing_path.is_some()
                    || self.playing_stream.is_some()
                    || self.playing_episode_url.is_some()
                {
                    // Pausiert (Datei, Sender oder Episode) → fortsetzen.
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
                self.refresh_stream_icons();
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
                    let queued = e
                        .path()
                        .is_some_and(|ep| queue.iter().any(|p| p == ep));
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
            Cmd::RemoteEntries(result, source, rel) => {
                // Veraltetes Ergebnis verwerfen (Quelle/Ordner inzwischen gewechselt).
                if self.active_source != source
                    || self.remote_browse.as_deref() != Some(rel.as_str())
                {
                    return;
                }
                self.loading = false;
                match result {
                    Err(e) => {
                        tracing::warn!("WebDAV listing failed: {e}");
                        self.entries.guard().clear();
                        self.toast(&gettext("Could not load this folder"));
                    }
                    Ok(list) => {
                        use crate::ui::app_views::natural_key;
                        let (mut dirs, mut files): (Vec<_>, Vec<_>) =
                            list.into_iter().partition(|e| e.is_dir);
                        dirs.sort_by(|a, b| natural_key(&a.name).cmp(&natural_key(&b.name)));
                        files.sort_by(|a, b| natural_key(&a.name).cmp(&natural_key(&b.name)));
                        let mut entries: Vec<FsEntry> = Vec::with_capacity(dirs.len() + files.len());
                        for d in dirs {
                            entries.push(FsEntry::remote_dir(d.rel_path, d.name));
                        }
                        for f in files {
                            let cached = self.remote_cache_path(&f.rel_path).filter(|p| p.exists());
                            entries.push(FsEntry::remote_file(f.rel_path, f.name, cached));
                        }
                        let distinct: std::collections::HashSet<String> =
                            entries.iter().filter_map(|e| e.effective_artist()).collect();
                        let opts = RowOpts {
                            show_artist: distinct.len() > 1,
                        };
                        {
                            let mut guard = self.entries.guard();
                            guard.clear();
                            for e in entries {
                                guard.push_back((e, opts, false));
                            }
                        }
                        self.refresh_queue_icons();
                        // Tags der entfernten Dateien im Hintergrund nachladen.
                        if let Some(src) = self.active_remote_source() {
                            self.start_remote_tag_fetch(&sender, &src);
                        }
                    }
                }
            }
            Cmd::RemoteTags(tags) => {
                // rel-Pfad → Factory-Index, dann Tags an die jeweilige Zeile schicken.
                let map: std::collections::HashMap<String, usize> = {
                    let guard = self.entries.guard();
                    (0..guard.len())
                        .filter_map(|i| {
                            guard.get(i).and_then(|r| match &r.entry {
                                FsEntry::RemoteFile { rel_path, .. } => Some((rel_path.clone(), i)),
                                _ => None,
                            })
                        })
                        .collect()
                };
                for (rel, title, artist, duration_ms) in tags {
                    if let Some(&i) = map.get(&rel) {
                        self.entries.send(
                            i,
                            FsInput::SetTags {
                                title,
                                artist,
                                duration_ms,
                            },
                        );
                    }
                }
            }
            Cmd::RemoteDownloaded(result) => match result {
                Ok((rel, path)) => {
                    let idx = {
                        let guard = self.entries.guard();
                        (0..guard.len()).find(|&i| {
                            guard.get(i).is_some_and(|r| {
                                matches!(&r.entry, FsEntry::RemoteFile { rel_path, .. } if *rel_path == rel)
                            })
                        })
                    };
                    if let Some(i) = idx {
                        self.entries.send(i, FsInput::SetDownloaded(path));
                    }
                    self.toast(&gettext("Download complete"));
                }
                Err(e) => {
                    tracing::warn!("Download failed: {e}");
                    self.toast(&gettext("Download failed"));
                }
            },
            Cmd::WebdavTested(result) => self.on_webdav_tested(result),
            Cmd::EnrichDone { changed } => {
                self.enriching = false;
                // Nur neu aufbauen, wenn der Lauf etwas geändert hat – der leise
                // Minuten-Nachzug läuft sonst ins Leere und würde die Listen grundlos
                // neu rendern.
                if changed {
                    self.reload_albums();
                    self.reload_artists();
                }
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
                    && self.music_dir.is_some()
                    && online_available()
                {
                    // Automatischer Lauf (ohne erneuten Tag-Scan), voller Umfang.
                    self.run_enrich(&sender, false, false);
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
            Cmd::StreamSearchResults(results) => {
                self.stream_search_results = results;
                self.rebuild_stream_search_results(&sender);
            }
            Cmd::StreamSearchCoversReady => self.rebuild_stream_search_results(&sender),
            Cmd::ReloadStreams => self.reload_streams(&sender),
            Cmd::SourceStatus(status) => {
                let mut changed = false;
                for (id, ok) in status {
                    if ok {
                        changed |= self.offline_sources.remove(&id);
                    } else {
                        changed |= self.offline_sources.insert(id);
                    }
                }
                // Geänderter Verbindungsstand → Ansichten neu aufbauen, damit der
                // rote „Getrennt"-Hinweis erscheint/verschwindet.
                if changed {
                    self.reload_albums();
                    self.reload_artists();
                }
            }
            Cmd::RemoteIndexed => {
                // Cloud-Titel sind in der DB → Alben/Interpreten neu aufbauen und
                // (sofern erwünscht) Cover/Fotos online nachladen.
                self.reload_albums();
                self.reload_artists();
                if self.auto_enrich && !self.enriching && online_available() {
                    self.run_enrich(&sender, false, false);
                }
            }
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

/// Eine **Galerie-Kachel**: quadratisches Cover (oder Platzhalter-Icon) mit dem
/// Titel als halbtransparentem Band unten (Overlay). Klick-/Long-Press-Handler
/// werden vom Aufrufer (FlowBox) ergänzt.
///
/// Dekodiert **nicht** synchron: nur ein bereits gecachtes Cover wird sofort
/// gesetzt. Das zurückgegebene `Picture` (falls ein Cover-Pfad vorliegt) füllt
/// der Aufrufer per Hintergrund-Dekodierung ([`spawn_gallery_decode`]) nach.
/// Quadratische Standard-Kantenlänge einer Galerie-Kachel, bis
/// [`size_gallery_tiles`] die exakte Spaltenbreite kennt. Hält die Kachel von
/// Anfang an quadratisch (statt dem Querformat des Covers zu folgen).
const GALLERY_TILE_DEFAULT: i32 = 110;

pub(crate) fn gallery_cell(
    cover_path: Option<&str>,
    icon: &str,
    title: &str,
) -> (gtk::Overlay, Option<gtk::Picture>) {
    let overlay = gtk::Overlay::new();
    // Die exakte Kachelgröße (genau 1/Spaltenzahl der Breite) wird zentral per
    // [`size_gallery_tiles`] als `size_request` gesetzt. **Kein `hexpand`**: sonst
    // dehnt die FlowBox die Kacheln über ihren Anteil hinaus (z. B. bei wenigen
    // Einträgen würde eine Kachel mehr als 100%/Spalten der Breite einnehmen).
    // `halign: Start`, damit die Zelle nie über den `size_request` hinaus wächst.
    overlay.set_hexpand(false);
    overlay.set_halign(gtk::Align::Start);
    overlay.set_valign(gtk::Align::Start);
    // **Quadratische Default-Größe** schon ab Erstellung – damit die Kachel
    // während der ganzen Lade-/Layout-Phase quadratisch bleibt (nie Querformat
    // oder kollabiert), egal wann/ob asynchrone Cover eintreffen. [`size_gallery_tiles`]
    // verfeinert anschließend nur noch die exakte Pixelgröße (Spaltenbreite).
    overlay.set_size_request(GALLERY_TILE_DEFAULT, GALLERY_TILE_DEFAULT);
    // Quadratischer Kachelrahmen als simpler `Box`-Container. Seine Größe setzt
    // [`size_gallery_tiles`] hart auf das Quadrat (Breite = Höhe). Bewusst KEIN
    // `AspectFrame`: der ignorierte seinen `size_request` in der Höhe und ließ die
    // Zelle dem Querformat asynchron geladener Cover folgen. Eine `Box` respektiert
    // den `size_request` zuverlässig; das Cover füllt formatfüllend (`Cover`),
    // `overflow: Hidden` + `card` runden/beschneiden die Ecken.
    let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
    frame.set_overflow(gtk::Overflow::Hidden);
    frame.set_hexpand(false);
    frame.set_halign(gtk::Align::Fill);
    frame.set_valign(gtk::Align::Fill);
    frame.set_size_request(GALLERY_TILE_DEFAULT, GALLERY_TILE_DEFAULT);
    frame.add_css_class("card");
    let picture = match cover_path {
        Some(path) => {
            // Cover als `Picture`. **Nur** eine bereits gecachte Textur sofort
            // setzen (kein synchrones Dekodieren – das blockierte sonst Start und
            // Galerie-Aufbau). Sonst bleibt die Karte als Platzhalter, bis das
            // Cover im Hintergrund nachgereicht wird.
            let pic = gtk::Picture::new();
            pic.set_content_fit(gtk::ContentFit::Cover);
            pic.set_hexpand(true);
            pic.set_vexpand(true);
            pic.set_halign(gtk::Align::Fill);
            pic.set_valign(gtk::Align::Fill);
            if let Some(tex) = crate::ui::widgets::cached_thumb(path) {
                pic.set_paintable(Some(&tex));
            }
            frame.append(&pic);
            Some(pic)
        }
        None => {
            let img = gtk::Image::from_icon_name(icon);
            img.set_pixel_size(64);
            img.set_hexpand(true);
            img.set_vexpand(true);
            frame.append(&img);
            None
        }
    };
    overlay.set_child(Some(&frame));
    let label = gtk::Label::new(Some(title));
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    label.set_xalign(0.0);
    label.set_valign(gtk::Align::End);
    label.set_halign(gtk::Align::Fill);
    label.add_css_class("emilia-gallery-title");
    overlay.add_overlay(&label);
    (overlay, picture)
}

/// Dekodiert die Cover (Pfad → Ziel-`Picture`) **in einem Hintergrund-Thread**
/// und reicht die Texturen progressiv auf dem UI-Thread nach. Dadurch blockiert
/// weder App-Start noch Galerie-Aufbau das Bild-Dekodieren. Backpressure über
/// einen kleinen, begrenzten Kanal, damit der Thread nicht weit vorausläuft.
pub(crate) fn spawn_gallery_decode(items: Vec<(String, gtk::Picture)>) {
    if items.is_empty() {
        return;
    }
    let (tx, rx) = async_channel::bounded::<(usize, String, gtk::gdk::Texture)>(8);
    let paths: Vec<String> = items.iter().map(|(p, _)| p.clone()).collect();
    let targets: Vec<gtk::Picture> = items.into_iter().map(|(_, pic)| pic).collect();
    std::thread::spawn(move || {
        for (i, path) in paths.into_iter().enumerate() {
            if let Some(tex) = crate::ui::widgets::decode_thumb(&path) {
                // Bricht ab, sobald der Empfänger weg ist (Galerie neu aufgebaut).
                if tx.send_blocking((i, path, tex)).is_err() {
                    break;
                }
            }
        }
    });
    gtk::glib::spawn_future_local(async move {
        while let Ok((i, path, tex)) = rx.recv().await {
            crate::ui::widgets::store_thumb(path, tex.clone());
            if let Some(pic) = targets.get(i) {
                pic.set_paintable(Some(&tex));
            }
        }
    });
}

/// Setzt jede Galerie-Kachel auf ein **Quadrat in Spaltenbreite**. Nötig, weil
/// die `FlowBox` Kinder nicht über ihre natürliche Größe streckt: ohne festes
/// `size_request` blieben die Thumbnails im breiten Desktop-Mode klein (das Bild
/// „skaliert nicht mit"), während das Feld breiter wird. Wird bei jedem Befüllen
/// und bei jeder Breitenänderung des Fensters aufgerufen.
pub(crate) fn size_gallery_tiles(fb: &gtk::FlowBox) {
    let cols = fb.min_children_per_line().max(1) as i32;
    let w = fb.width();
    if w <= 1 {
        return; // noch nicht zugewiesen – Resize-Hook holt das nach
    }
    let spacing = fb.column_spacing() as i32;
    // `cols`-fachen Abstand abziehen (statt `cols-1`) als Sicherheitspuffer,
    // damit immer genau `cols` Kacheln je Zeile passen und nicht umbrechen.
    let tile = ((w - spacing * cols) / cols).max(64);
    let mut child = fb.first_child();
    while let Some(c) = child {
        let next = c.next_sibling();
        if let Some(inner) = c
            .downcast_ref::<gtk::FlowBoxChild>()
            .and_then(|f| f.child())
        {
            inner.set_size_request(tile, tile);
            // Auch den AspectFrame (Haupt-Kind des Overlays) hart auf das Quadrat
            // setzen – sonst folgt die Zellenhöhe dem Seitenverhältnis des (ggf.
            // quer-/hochformatigen) Covers statt der Breite.
            if let Some(frame) = inner.first_child() {
                frame.set_size_request(tile, tile);
            }
        }
        child = next;
    }
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
    /// Baut **alle** Listen neu auf (nach Umschalten Galerie/Liste oder der
    /// Spaltenzahl). Jede Reload-Funktion füllt – je nach `gallery_view` – die
    /// Listen- oder die Galerie-Variante.
    pub(crate) fn rebuild_all_lists(&mut self, sender: &ComponentSender<Self>) {
        self.reload_albums();
        self.reload_artists();
        self.load_dir(sender);
        self.load_favorites(sender);
        self.load_audiobooks(sender);
        self.load_concerts(sender);
        self.reload_podcasts(sender);
    }

    /// Füllt eine FlowBox als **Galerie**: Kacheln aus `(cover, icon, title)`,
    /// Spaltenzahl = `gallery_columns`. Einzelklick aktiviert (`activate(index)`),
    /// langes Drücken öffnet das Detail (`detail(index)`). Nachrichten gehen über
    /// den eigenen Input-Sender. Beim erneuten Aufruf werden alle Kacheln (samt
    /// ihrer Controller) entfernt – keine Mehrfach-Handler.
    pub(crate) fn fill_gallery(
        &self,
        fb: &gtk::FlowBox,
        items: &[(Option<String>, &'static str, String)],
        activate: fn(usize) -> Msg,
        detail: fn(usize) -> Msg,
    ) {
        while let Some(c) = fb.first_child() {
            fb.remove(&c);
        }
        fb.set_min_children_per_line(self.gallery_columns);
        fb.set_max_children_per_line(self.gallery_columns);
        // `homogeneous(true)` gibt **allen** Kacheln genau die per `size_request`
        // ([`size_gallery_tiles`]) gesetzte Größe (= 1/Spaltenzahl der Breite) und
        // streckt sie NICHT auf die Zeilenbreite. Ohne das verteilt die FlowBox
        // die Zeilenbreite auf die tatsächlich vorhandenen Kacheln – bei wenigen
        // Einträgen würde eine Kachel dann mehr als 100%/Spalten einnehmen.
        fb.set_homogeneous(true);
        fb.set_row_spacing(8);
        fb.set_column_spacing(8);
        fb.set_selection_mode(gtk::SelectionMode::None);
        // Die FlowBox selbst NICHT auf Einfachklick reagieren lassen – sonst
        // schluckt sie den Klick, bevor die Kachel-Geste ihn auswerten kann.
        fb.set_activate_on_single_click(false);
        if !fb.has_css_class("emilia-gallery") {
            fb.add_css_class("emilia-gallery");
        }
        // Nicht gecachte Cover sammeln und nach dem Aufbau im Hintergrund laden.
        let mut to_decode: Vec<(String, gtk::Picture)> = Vec::new();
        for (i, (cover, icon, title)) in items.iter().enumerate() {
            let (cell, pic) = gallery_cell(cover.as_deref(), icon, title);
            if let (Some(path), Some(pic)) = (cover.as_deref(), pic) {
                if crate::ui::widgets::cached_thumb(path).is_none() {
                    to_decode.push((path.to_string(), pic));
                }
            }
            // Einzeltipp → Unterseite **sofort** (`activate`), langes Drücken →
            // Detailansicht (`detail`) – exakt wie in der Listenansicht. Bewusst
            // KEIN Doppeltipp/keine Verzögerung, damit der Klick nicht hängt.
            let click = gtk::GestureClick::new();
            {
                let input = self.input.clone();
                click.connect_released(move |g, n, _, _| {
                    if n == 1 {
                        g.set_state(gtk::EventSequenceState::Claimed);
                        let _ = input.send(activate(i));
                    }
                });
            }
            cell.add_controller(click);
            let long_press = gtk::GestureLongPress::new();
            {
                let input = self.input.clone();
                long_press.connect_pressed(move |g, _, _| {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    let _ = input.send(detail(i));
                });
            }
            cell.add_controller(long_press);
            fb.append(&cell);
        }
        // Cover der noch nicht gecachten Kacheln im Hintergrund nachladen.
        spawn_gallery_decode(to_decode);
        // Kacheln sofort quadratisch auf Spaltenbreite bringen (greift, sobald die
        // FlowBox alloziert ist; beim ersten Befüllen im Init noch w=0).
        size_gallery_tiles(fb);
        // Einmalig je FlowBox an Größenänderungen koppeln. `connect_map` feuert
        // erst, wenn die FlowBox sichtbar **und im Baum alloziert** ist – dort
        // vermessen wir neu und koppeln (einmal) an die `page-size` der
        // umschließenden ScrolledWindow, damit die Kacheln im Desktop-Mode bei
        // Fensterbreiten-Änderung mitskalieren.
        if self.gallery_hooked.borrow_mut().insert(fb.as_ptr() as usize) {
            let pagesize_done = std::rc::Rc::new(std::cell::Cell::new(false));
            fb.connect_map(move |fb| {
                size_gallery_tiles(fb);
                if pagesize_done.get() {
                    return;
                }
                let mut ancestor = fb.parent();
                while let Some(w) = ancestor {
                    if let Ok(sw) = w.clone().downcast::<gtk::ScrolledWindow>() {
                        let weak = fb.downgrade();
                        sw.hadjustment().connect_page_size_notify(move |_| {
                            if let Some(fb) = weak.upgrade() {
                                size_gallery_tiles(&fb);
                            }
                        });
                        pagesize_done.set(true);
                        break;
                    }
                    ancestor = w.parent();
                }
            });
        }
    }

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
    /// laufenden Massen-Sync). Holt zusätzlich – falls ein fanart.tv-Key vorliegt –
    /// die **Bildergalerie** des Interpreten (mehrere Fotos), die es nur in der
    /// Detailansicht gibt und die deshalb erst hier (bedarfsgesteuert) geladen wird.
    /// Tut nichts ohne Netz; das Einzelfoto entfällt bei bereits zugeordnetem Foto
    /// oder nach zu vielen Versuchen, die Galerie, wenn sie schon vorliegt oder in
    /// dieser Sitzung bereits versucht wurde. Nach Erfolg: `Cmd::ReloadViews`.
    pub(crate) fn fetch_focus_artist(&self, sender: &ComponentSender<Self>, name: &str) {
        let name = name.trim().to_string();
        if name.is_empty() || !online_available() {
            return;
        }
        // (a) Einzelfoto (Deezer): überspringen, wenn schon zugeordnet oder erschöpft.
        let matched = self
            .library
            .get_artist_meta(&name)
            .ok()
            .flatten()
            .is_some_and(|m| m.status == "matched");
        let need_image =
            !matched && self.library.artist_attempts(&name) < crate::ui::enrich::MAX_ATTEMPTS;
        // (b) Galerie (fanart.tv): nur mit Key, wenn noch keine vorliegt und in dieser
        // Sitzung noch nicht versucht (Galerien haben keine Versuchsgrenze).
        let fkey = self.fanart_key.clone().filter(|k| !k.is_empty());
        let need_gallery = fkey.is_some()
            && self
                .library
                .artist_images(&name)
                .map(|imgs| imgs.is_empty())
                .unwrap_or(false)
            && self.gallery_tried.borrow_mut().insert(format!("a\u{1}{name}"));
        if !need_image && !need_gallery {
            return;
        }
        let fkey = fkey.filter(|_| need_gallery);
        sender.spawn_oneshot_command(move || {
            if let Ok(lib) = Library::open() {
                let client = crate::core::online::OnlineClient::new();
                if need_image {
                    let (image, errored) = match client.fetch_artist_image(&name) {
                        Ok(img) => (img, false),
                        Err(_) => (None, true),
                    };
                    let meta = crate::core::online::store_artist_image(&name, image, errored);
                    let _ = lib.upsert_artist_meta(&meta);
                }
                if let Some(key) = fkey {
                    let _ = crate::core::online::enrich_artist_gallery(&client, &lib, &name, &key);
                }
            }
            Cmd::ReloadViews
        });
    }

    /// Wie [`Self::fetch_focus_artist`], nur für das **gerade geöffnete Album**: lädt
    /// das Einzelcover (MusicBrainz + Cover Art Archive) und – falls noch keine da ist –
    /// die **Cover-Galerie** des Albums. Das Einzelcover entfällt, wenn schon eines
    /// vorliegt oder zu viele Versuche scheiterten; die Galerie, wenn sie schon
    /// vorliegt oder in dieser Sitzung versucht wurde. Sie braucht die beim
    /// Cover-Abruf gesetzte MBID – beim allerersten Öffnen entsteht diese gerade erst,
    /// die Galerie greift dann ggf. erst beim nächsten Öffnen.
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
        let need_cover = !has_cover
            && self.library.album_attempts(&artist, &album) < crate::ui::enrich::MAX_ATTEMPTS;
        let need_gallery = self
            .library
            .album_images(&artist, &album)
            .map(|imgs| imgs.is_empty())
            .unwrap_or(false)
            && self
                .gallery_tried
                .borrow_mut()
                .insert(format!("b\u{1}{artist}\u{1}{album}"));
        if !need_cover && !need_gallery {
            return;
        }
        sender.spawn_oneshot_command(move || {
            if let Ok(lib) = Library::open() {
                let client = crate::core::online::OnlineClient::new();
                // Erst das Cover (setzt die MBID), dann die Galerie, die sie nutzt.
                if need_cover {
                    let _ = crate::core::online::enrich_album(&client, &lib, &artist, &album);
                }
                if need_gallery {
                    let _ =
                        crate::core::online::enrich_album_gallery(&client, &lib, &artist, &album);
                }
            }
            Cmd::ReloadViews
        });
    }

    /// Bedarfsgesteuerte **Fingerprint-Titelerkennung** (Chromaprint → AcoustID) für
    /// den gerade gestarteten Titel. Läuft nur mit AcoustID-Key + `fpcalc` + Netz,
    /// nur für noch nicht zugeordnete und nicht erschöpfte Titel. Ersetzt den
    /// früheren Massen-Lauf: erkannt wird, was tatsächlich gespielt wird.
    pub(crate) fn fetch_focus_track(&self, sender: &ComponentSender<Self>, path: &std::path::Path) {
        if !online_available() {
            return;
        }
        let Some(key) = self.acoustid_key.clone().filter(|k| !k.is_empty()) else {
            return;
        };
        if !crate::core::online::fingerprint_available() {
            return;
        }
        let path_str = path.to_string_lossy().to_string();
        let matched = self
            .library
            .get_track_meta(&path_str)
            .ok()
            .flatten()
            .is_some_and(|m| m.status == "matched");
        if matched || self.library.track_attempts(&path_str) >= crate::ui::enrich::MAX_ATTEMPTS {
            return;
        }
        let path = path.to_path_buf();
        sender.spawn_oneshot_command(move || {
            if let Ok(lib) = Library::open() {
                let client = crate::core::online::OnlineClient::new();
                let _ = crate::core::online::enrich_track_fingerprint(&client, &lib, &key, &path);
            }
            Cmd::ReloadViews
        });
    }

    /// Nur nach oben, solange wir innerhalb des Startordners bleiben.
    pub(crate) fn can_go_up(&self) -> bool {
        // Entfernte Quelle: zurück möglich, solange nicht an der Musikwurzel.
        if let Some(rel) = &self.remote_browse {
            return !rel.is_empty();
        }
        match (&self.browse_dir, &self.root_dir) {
            (Some(cur), Some(root)) => cur != root && cur.starts_with(root),
            _ => false,
        }
    }

    /// Anzeigename der aktiven Quelle (für die Pfadleiste an der Wurzel).
    pub(crate) fn active_source_name(&self) -> String {
        match &self.active_source {
            ActiveSource::Primary => gettext("Music"),
            ActiveSource::Source(id) => self
                .sources
                .iter()
                .find(|s| s.id == *id)
                .map(|s| s.name.clone())
                .unwrap_or_default(),
        }
    }

    /// Beschriftung der Pfadleiste (aktueller Ordnername bzw. Hinweis).
    pub(crate) fn folder_label(&self) -> String {
        // Entfernte Quelle: letztes Pfadsegment bzw. Quellname an der Wurzel.
        if let Some(rel) = &self.remote_browse {
            if rel.is_empty() {
                return self.active_source_name();
            }
            return rel.rsplit('/').next().unwrap_or(rel).to_string();
        }
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
