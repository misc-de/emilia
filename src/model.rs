//! Datenmodelle der Bibliothek.

#[derive(Debug, Clone)]
pub struct Track {
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
