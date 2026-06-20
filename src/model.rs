//! Data models of the library.

#[derive(Debug, Clone, Default)]
pub struct Track {
    /// DB primary key. Currently everything is addressed internally via the
    /// (unique) path; the field remains for future use (e.g. playlists).
    #[allow(dead_code)]
    pub id: i64,
    pub path: String,
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    /// Genre from the file tags (for statistics); `None` if not set or the file
    /// has not yet been (re-)read.
    pub genre: Option<String>,
    pub track_no: Option<u32>,
    /// Disc/CD number for multi-CD albums (None = single CD).
    pub disc_no: Option<u32>,
    pub duration_ms: Option<i64>,
    pub resume_ms: i64,
    /// Release year from the file's date tag; `None` if untagged or not yet
    /// (re-)scanned. Stored in the DB so date sorting uses the embedded
    /// metadata, never the file's modification timestamp.
    pub year: Option<i32>,
}

/// An additional music source besides the primary `music_dir` folder.
/// Appears as its own tab in the file view. See [`crate::core::db`].
#[derive(Debug, Clone)]
pub struct Source {
    pub id: i64,
    /// `local` (second folder) | `webdav` (Nextcloud share).
    pub kind: String,
    /// Display name (tab label).
    pub name: String,
    /// Sort order of the tabs (only used in the DB: `ORDER BY position`).
    #[allow(dead_code)]
    pub position: i64,
    /// Local: root path in the file system.
    pub path: Option<String>,
    /// WebDAV: base URL, e.g. `https://cloud.example.com`.
    pub base_url: Option<String>,
    /// WebDAV: username.
    pub username: Option<String>,
    /// WebDAV: app password/token.
    pub password: Option<String>,
    /// WebDAV: subpath to the music, e.g. `/Music`.
    pub music_path: Option<String>,
}

/// Online-enriched album data (MusicBrainz + Cover Art Archive).
///
/// Kept exclusively in the database or the XDG cache – the audio files
/// themselves are never modified in the process.
#[derive(Debug, Clone)]
pub struct AlbumMeta {
    pub artist: String,
    pub album: String,
    /// MusicBrainz release ID (MBID), if matched.
    pub mbid: Option<String>,
    /// Path to the locally cached cover file.
    pub cover_path: Option<String>,
    pub year: Option<i32>,
    /// `pending` | `matched` | `notfound` | `error`
    pub status: String,
    /// Number of tracks of this album in the library (display only).
    pub track_count: i64,
    /// Summed playback length of the album's tracks in milliseconds; `None` when
    /// no track duration is known. Only filled for the album overview (sorting).
    pub total_duration_ms: Option<i64>,
}

impl AlbumMeta {
    /// Empty entry (not yet searched online).
    pub fn pending(artist: impl Into<String>, album: impl Into<String>) -> Self {
        Self {
            artist: artist.into(),
            album: album.into(),
            mbid: None,
            cover_path: None,
            year: None,
            status: "pending".to_string(),
            track_count: 0,
            total_duration_ms: None,
        }
    }
}

/// Online-enriched artist data (photo via Deezer).
/// Only in DB/cache, never written into the audio files.
#[derive(Debug, Clone)]
pub struct ArtistMeta {
    pub name: String,
    /// Path to the locally cached artist photo.
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

/// Track data recognized via audio fingerprint (Chromaprint → AcoustID).
///
/// This is a **suggestion** layer for files with missing tags: the values are
/// kept exclusively in the DB and never written back into the file.
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

/// A podcast episode from an RSS feed (order = feed order).
/// Audio is streamed directly from `audio_url`, nothing is downloaded.
#[derive(Debug, Clone)]
pub struct Episode {
    pub guid: Option<String>,
    pub title: String,
    pub audio_url: String,
    /// Publication date as original text from the feed (display only).
    pub published: Option<String>,
    /// Duration as text (e.g. "00:42:13" or seconds), if given in the feed.
    pub duration: Option<String>,
    /// Description/show notes (HTML sanitized to plain text), if present.
    pub description: Option<String>,
}

/// An episode together with its podcast – for the cross-podcast "Latest" view
/// (newest entries from all subscriptions).
#[derive(Debug, Clone)]
pub struct EpisodeRef {
    pub podcast_title: String,
    pub podcast_image: Option<String>,
    pub title: String,
    pub audio_url: String,
    pub published: Option<String>,
    pub duration: Option<String>,
    /// Description/show notes (HTML sanitized to plain text), if present.
    pub description: Option<String>,
}

/// A YouTube video belonging to a subscribed channel (cached newest list,
/// replaced on refresh – like an [`Episode`]). Audio is streamed via a
/// freshly-resolved URL, or played from an offline download.
#[derive(Debug, Clone)]
pub struct YtVideo {
    pub video_id: String,
    pub title: String,
    /// Canonical watch URL.
    pub url: String,
    /// Duration in seconds, if known.
    pub duration: Option<i64>,
    /// Upload date as text from the listing (display only).
    pub published: Option<String>,
    pub thumbnail: Option<String>,
}

/// A recently played YouTube video (history). `artist`/`thumbnail` are filled
/// in by the on-play online enrichment.
#[derive(Debug, Clone)]
pub struct YtRecent {
    /// Video id (for `kind == "video"`) or playlist URL (for `"playlist"`).
    pub video_id: String,
    pub title: String,
    pub artist: Option<String>,
    /// `"video"` or `"playlist"`.
    pub kind: String,
    /// Number of songs (playlists only).
    pub count: i64,
    /// Representative thumbnail (a cached path or thumbnail URL). For playlists
    /// this is the cover derived from the first song; videos resolve their own
    /// cover from the id.
    pub thumbnail: Option<String>,
    /// Cached playback length in seconds (videos only; `None` for playlists or
    /// when not yet known).
    pub duration: Option<i64>,
    /// Summed runtime in seconds of all songs (playlists only; `None` for videos
    /// or when not yet known).
    pub total_duration: Option<i64>,
}

/// A recently (partly) heard podcast episode — an entry of the podcast
/// "Recently" list. Sourced from `episode_progress` (only an in-progress episode
/// carries a stored position), joined back to its episode/podcast for display.
#[derive(Debug, Clone)]
pub struct RecentEpisode {
    pub podcast_title: String,
    pub podcast_image: Option<String>,
    pub title: String,
    pub audio_url: String,
    /// Total length text from the feed (for the progress fraction + label).
    pub duration: Option<String>,
    /// Stored playback position in milliseconds (> 0).
    pub position_ms: i64,
}

/// A video together with its channel – for the cross-channel "Newest videos"
/// view (mirrors [`EpisodeRef`]).
#[derive(Debug, Clone)]
pub struct YtVideoRef {
    pub channel_title: String,
    pub channel_thumb: Option<String>,
    pub video_id: String,
    pub title: String,
    pub duration: Option<i64>,
    pub published: Option<String>,
}

/// A saved streaming station (internet radio). Playback directly via the stream
/// URL – nothing is downloaded.
#[derive(Debug, Clone)]
pub struct StreamItem {
    pub id: i64,
    pub name: String,
    pub url: String,
    /// Station logo (URL); cached locally like podcast covers.
    pub favicon: Option<String>,
    /// Genre/tags (comma-separated, from the Radio Browser API).
    pub tags: Option<String>,
    pub country: Option<String>,
}

/// A song recorded from a station (timeshift recording). Stored as a tagged
/// audio file at `path`.
#[derive(Debug, Clone)]
pub struct RecordingItem {
    pub id: i64,
    pub path: String,
    pub artist: Option<String>,
    pub title: String,
    /// Station that was recorded from.
    pub station: Option<String>,
    /// Recording time (Unix seconds).
    pub recorded_at: i64,
    /// Playback length in milliseconds (0 until probed/backfilled).
    pub duration_ms: i64,
    /// Beginning was missing (started too late) – marked as a hint only.
    pub incomplete: bool,
}

/// A song recognized from a station's ICY title while streaming — an entry in
/// the "Recently heard" history. Unlike a [`RecordingItem`] nothing is captured
/// to disk; this is purely metadata about what played. One row per song.
#[derive(Debug, Clone)]
pub struct HeardItem {
    pub id: i64,
    pub artist: Option<String>,
    pub title: String,
    /// Station it was last heard on.
    pub station: Option<String>,
    /// Last time it was heard (Unix seconds; newest first).
    pub heard_at: i64,
    /// How often it has been recognized (across stations).
    pub count: i64,
}

/// A user-created category for organising voice memos. Each memo has at most
/// one (optional); a memo without one falls back to "General" — represented by
/// a NULL `category_id`, only ever shown as a label, never a real row, so it
/// cannot be deleted. Deliberately separate from the `category` *areas*
/// (filesystem/artists/albums/…), which are an unrelated concept.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct MemoCategory {
    pub id: i64,
    /// Display name (free text, user-editable). Not run through gettext — the
    /// localized defaults are translated once at seed time, then stored as data.
    pub name: String,
    /// Manual sort order (`ORDER BY position`).
    pub position: i64,
    /// Creation time (Unix seconds), or `None` for rows seeded before the column
    /// existed. Shown in the category detail view ("Created").
    pub created_at: Option<i64>,
}

/// A voice memo (microphone recording). Stored as an audio file at `path`; here
/// only the metadata/management. Kept separate from `recording` (radio
/// timeshift) and `track` (the music library).
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct MemoItem {
    pub id: i64,
    pub path: String,
    pub title: String,
    /// Assigned category, or `None` = unassigned ("General"). Assignable (and
    /// re-assignable) after recording.
    pub category_id: Option<i64>,
    /// Recording time (Unix seconds). Primary sort key (newest first).
    pub recorded_at: i64,
    /// Playback length in milliseconds (0 until probed/backfilled).
    pub duration_ms: i64,
}

/// Aggregated metrics of the listening statistics over a period. All computed
/// from the raw `play_event` table (see [`crate::core::db`]).
#[derive(Debug, Clone, Default)]
pub struct StatTotals {
    /// Actually listened time (sum of all events, including partial plays).
    pub total_played_ms: i64,
    /// Events counting as a play (above the threshold, Last.fm rule).
    pub plays: i64,
    /// Aborted/skipped events (below the threshold).
    pub skips: i64,
    pub distinct_tracks: i64,
    pub distinct_artists: i64,
    pub distinct_albums: i64,
}

/// An entry of a ranking (top tracks/albums/artists).
#[derive(Debug, Clone)]
pub struct StatEntry {
    /// Display name: track, album name or artist name.
    pub name: String,
    /// Extra: artist (for track/album), empty for artists.
    pub detail: String,
    /// Events counting as a play.
    pub plays: i64,
    /// Actually listened time (ms).
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

/// One song hit of the library search (title match).
#[derive(Debug, Clone)]
pub struct SongHit {
    pub path: String,
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
}

/// One album hit of the library search (album name or year match).
#[derive(Debug, Clone)]
pub struct AlbumHit {
    pub album: String,
    /// A representative artist of that album (display only).
    pub artist: String,
    pub year: Option<i32>,
}

/// Grouped result of the title-bar search. It spans the local music library
/// (artists, albums, songs) and the user's own local collections: timeshift
/// recordings and voice memos. Streaming stations and YouTube channels/videos
/// are deliberately excluded – they have their own dedicated sections. Each
/// group is capped. See [`crate::core::db::Library::search_library`].
#[derive(Debug, Clone, Default)]
pub struct SearchResults {
    pub artists: Vec<String>,
    pub albums: Vec<AlbumHit>,
    pub songs: Vec<SongHit>,
    /// Timeshift recordings (title/artist/station match).
    pub recordings: Vec<RecordingItem>,
    /// Voice memos (title match).
    pub memos: Vec<MemoItem>,
}

impl SearchResults {
    pub fn is_empty(&self) -> bool {
        self.artists.is_empty()
            && self.albums.is_empty()
            && self.songs.is_empty()
            && self.recordings.is_empty()
            && self.memos.is_empty()
    }
}
