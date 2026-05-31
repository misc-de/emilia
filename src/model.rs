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
    pub track_no: Option<u32>,
    /// Disc-/CD-Nummer bei Mehr-CD-Alben (None = einzelne CD).
    pub disc_no: Option<u32>,
    pub duration_ms: Option<i64>,
    pub resume_ms: i64,
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
