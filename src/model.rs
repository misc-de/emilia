//! Datenmodelle der Bibliothek.

#[derive(Debug, Clone)]
pub struct Track {
    /// DB-Primärschlüssel. Aktuell wird intern alles über den (eindeutigen)
    /// Pfad adressiert; das Feld bleibt für künftige Nutzung (z. B. Playlisten).
    #[allow(dead_code)]
    pub id: i64,
    pub path: String,
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    /// Genre aus den Datei-Tags (für die Statistik); `None`, wenn nicht gesetzt
    /// oder die Datei noch nicht (neu) eingelesen wurde.
    pub genre: Option<String>,
    pub track_no: Option<u32>,
    /// Disc-/CD-Nummer bei Mehr-CD-Alben (None = einzelne CD).
    pub disc_no: Option<u32>,
    pub duration_ms: Option<i64>,
    pub resume_ms: i64,
}

/// Eine zusätzliche Musikquelle neben dem primären `music_dir`-Ordner.
/// Erscheint als eigener Tab in der Dateiansicht. Siehe [`crate::core::db`].
#[derive(Debug, Clone)]
pub struct Source {
    pub id: i64,
    /// `local` (zweiter Ordner) | `webdav` (Nextcloud-Share).
    pub kind: String,
    /// Anzeigename (Tab-Beschriftung).
    pub name: String,
    /// Sortierreihenfolge der Tabs (nur in der DB genutzt: `ORDER BY position`).
    #[allow(dead_code)]
    pub position: i64,
    /// Lokal: Wurzelpfad im Dateisystem.
    pub path: Option<String>,
    /// WebDAV: Basis-URL, z. B. `https://cloud.example.com`.
    pub base_url: Option<String>,
    /// WebDAV: Benutzername.
    pub username: Option<String>,
    /// WebDAV: App-Passwort/Token.
    pub password: Option<String>,
    /// WebDAV: Unterpfad zur Musik, z. B. `/Music`.
    pub music_path: Option<String>,
}

/// Online angereicherte Albumdaten (MusicBrainz + Cover Art Archive).
///
/// Wird ausschließlich in der Datenbank bzw. im XDG-Cache gehalten – die
/// Audiodateien selbst werden dabei niemals verändert.
#[derive(Debug, Clone)]
pub struct AlbumMeta {
    pub artist: String,
    pub album: String,
    /// MusicBrainz-Release-ID (MBID), falls zugeordnet.
    pub mbid: Option<String>,
    /// Pfad zur lokal zwischengespeicherten Cover-Datei.
    pub cover_path: Option<String>,
    pub year: Option<i32>,
    /// `pending` | `matched` | `notfound` | `error`
    pub status: String,
    /// Anzahl der Titel dieses Albums in der Bibliothek (nur für die Anzeige).
    pub track_count: i64,
}

impl AlbumMeta {
    /// Leerer Eintrag (noch nicht online gesucht).
    pub fn pending(artist: impl Into<String>, album: impl Into<String>) -> Self {
        Self {
            artist: artist.into(),
            album: album.into(),
            mbid: None,
            cover_path: None,
            year: None,
            status: "pending".to_string(),
            track_count: 0,
        }
    }
}

/// Online angereicherte Interpretendaten (Foto via Deezer).
/// Nur in DB/Cache, niemals in die Audiodateien geschrieben.
#[derive(Debug, Clone)]
pub struct ArtistMeta {
    pub name: String,
    /// Pfad zum lokal zwischengespeicherten Künstlerfoto.
    pub image_path: Option<String>,
    /// `pending` | `matched` | `notfound` | `error`
    pub status: String,
}

impl ArtistMeta {
    pub fn pending(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            image_path: None,
            status: "pending".to_string(),
        }
    }
}

/// Per Audio-Fingerprint (Chromaprint → AcoustID) erkannte Titeldaten.
///
/// Dies ist eine **Vorschlags**-Schicht für Dateien mit fehlenden Tags: Die
/// Werte werden ausschließlich in der DB gehalten und niemals in die Datei
/// zurückgeschrieben.
#[derive(Debug, Clone)]
pub struct TrackMeta {
    pub path: String,
    pub recording_mbid: Option<String>,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    /// `pending` | `matched` | `notfound` | `error`
    pub status: String,
}

/// Eine Podcast-Episode aus einem RSS-Feed (Reihenfolge = Feed-Reihenfolge).
/// Audio wird direkt von `audio_url` gestreamt, nichts wird heruntergeladen.
#[derive(Debug, Clone)]
pub struct Episode {
    pub guid: Option<String>,
    pub title: String,
    pub audio_url: String,
    /// Veröffentlichungsdatum als Originaltext aus dem Feed (nur Anzeige).
    pub published: Option<String>,
    /// Dauer als Text (z. B. „00:42:13" oder Sekunden), falls im Feed angegeben.
    pub duration: Option<String>,
    /// Beschreibung/Shownotes (HTML zu Klartext entschärft), falls vorhanden.
    pub description: Option<String>,
}

/// Eine Episode samt zugehörigem Podcast – für die podcastübergreifende
/// „Neuste"-Ansicht (neueste Beiträge aller Abos).
#[derive(Debug, Clone)]
pub struct EpisodeRef {
    pub podcast_id: i64,
    pub podcast_title: String,
    pub podcast_image: Option<String>,
    pub title: String,
    pub audio_url: String,
    pub published: Option<String>,
    pub duration: Option<String>,
    /// Beschreibung/Shownotes (HTML zu Klartext entschärft), falls vorhanden.
    pub description: Option<String>,
}

/// Ein gespeicherter Streaming-Sender (Internet-Radio). Wiedergabe direkt über
/// die Stream-URL – nichts wird heruntergeladen.
#[derive(Debug, Clone)]
pub struct StreamItem {
    pub id: i64,
    pub name: String,
    pub url: String,
    /// Sender-Logo (URL); lokal gecacht wie Podcast-Cover.
    pub favicon: Option<String>,
    /// Genre/Schlagworte (kommasepariert, aus der Radio-Browser-API).
    pub tags: Option<String>,
    pub country: Option<String>,
}

/// Ein mitgeschnittener Song aus einem Sender (Timeshift-Aufnahme). Liegt als
/// getaggte Audiodatei unter `path`.
#[derive(Debug, Clone)]
pub struct RecordingItem {
    pub id: i64,
    pub path: String,
    pub artist: Option<String>,
    pub title: String,
    /// Sender, aus dem mitgeschnitten wurde.
    pub station: Option<String>,
    /// Aufnahmezeitpunkt (Unix-Sekunden).
    pub recorded_at: i64,
    /// Anfang fehlte (zu spät begonnen) – nur als Hinweis markiert.
    pub incomplete: bool,
}

/// Aggregierte Kennzahlen der Hörstatistik über einen Zeitraum. Alles aus der
/// rohen `play_event`-Tabelle berechnet (siehe [`crate::core::db`]).
#[derive(Debug, Clone, Default)]
pub struct StatTotals {
    /// Tatsächlich gehörte Zeit (Summe aller Ereignisse, auch Teil-Wiedergaben).
    pub total_played_ms: i64,
    /// Als Wiedergabe zählende Ereignisse (über dem Schwellwert, Last.fm-Regel).
    pub plays: i64,
    /// Abgebrochene/übersprungene Ereignisse (unter dem Schwellwert).
    pub skips: i64,
    pub distinct_tracks: i64,
    pub distinct_artists: i64,
    pub distinct_albums: i64,
}

/// Ein Eintrag einer Rangliste (Top-Titel/-Alben/-Interpreten).
#[derive(Debug, Clone)]
pub struct StatEntry {
    /// Anzeigename: Titel, Albumname oder Interpretenname.
    pub name: String,
    /// Zusatz: Interpret (bei Titel/Album), bei Interpreten leer.
    pub detail: String,
    /// Als Wiedergabe zählende Ereignisse.
    pub plays: i64,
    /// Tatsächlich gehörte Zeit (ms).
    pub played_ms: i64,
}

impl TrackMeta {
    pub fn pending(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            recording_mbid: None,
            title: None,
            artist: None,
            album: None,
            status: "pending".to_string(),
        }
    }
}
