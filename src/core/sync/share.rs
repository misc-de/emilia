//! Selective-share model: turning a user's [`Selection`] into a wire
//! [`ShareManifest`] (sender side), classifying it against the local library
//! (receiver side, [`review_files`]) and applying an accepted manifest
//! ([`apply_manifest`]). Network-free so it is unit-testable like [`super::data`].

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::core::category::Area;
use crate::core::db::Library;
use crate::core::sync::data;
use crate::core::sync::hash::quick_hash;
use crate::core::sync::protocol::{
    CategoryRec, EqRec, FavoriteRec, PlaylistRec, PodcastRec, SCHEMA_VERSION,
};
use crate::core::sync::ImportStats;
use crate::core::youtube;
use crate::model::YtVideo;

// ---------------------------------------------------------------------------
// Wire manifest
// ---------------------------------------------------------------------------

/// One audio file offered for transfer (paths relative to the sender's music dir).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestFile {
    pub rel_path: String,
    pub size: u64,
    /// Quick content hash (size + sha256 of the first 1 MiB), for dedup/collision.
    pub quick_hash: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub artist: Option<String>,
    #[serde(default)]
    pub album: Option<String>,
    #[serde(default)]
    pub duration_ms: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum YtKind {
    Channel,
    Playlist,
    Song,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct YtVideoRec {
    pub video_id: String,
    pub title: String,
    pub url: String,
    #[serde(default)]
    pub duration: Option<i64>,
}

/// A YouTube item — only ever included when the receiver has YouTube enabled.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestYt {
    pub kind: YtKind,
    /// Channel id / playlist origin url / video id (the receiver-applicable key).
    pub id: String,
    pub url: String,
    pub title: String,
    /// Members for channels/playlists (empty for a single song).
    #[serde(default)]
    pub items: Vec<YtVideoRec>,
}

/// Selectable library-data payloads (each `None` = not shared).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LibraryBlobs {
    #[serde(default)]
    pub favorites: Option<Vec<FavoriteRec>>,
    #[serde(default)]
    pub playlists: Option<Vec<PlaylistRec>>,
    #[serde(default)]
    pub podcasts: Option<Vec<PodcastRec>>,
    #[serde(default)]
    pub categories: Option<Vec<CategoryRec>>,
    #[serde(default)]
    pub eq: Option<Vec<EqRec>>,
}

/// The whole offer the receiver reviews.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ShareManifest {
    pub schema: u32,
    pub device_name: String,
    #[serde(default)]
    pub files: Vec<ManifestFile>,
    #[serde(default)]
    pub yt: Vec<ManifestYt>,
    #[serde(default)]
    pub library: LibraryBlobs,
    pub total_size: u64,
}

impl ShareManifest {
    /// Whether the offer carries nothing at all.
    #[allow(dead_code)] // kept as part of the manifest API; not called yet
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
            && self.yt.is_empty()
            && self.library.favorites.is_none()
            && self.library.playlists.is_none()
            && self.library.podcasts.is_none()
            && self.library.categories.is_none()
            && self.library.eq.is_none()
    }
}

/// The receiver's response to an offer.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ShareDecision {
    /// `false` = reject everything.
    pub accept: bool,
    /// Accepted file rel_paths.
    #[serde(default)]
    pub files: Vec<String>,
    /// Accepted YouTube item ids (`ManifestYt.id`).
    #[serde(default)]
    pub yt: Vec<String>,
    #[serde(default)]
    pub favorites: bool,
    #[serde(default)]
    pub playlists: bool,
    #[serde(default)]
    pub podcasts: bool,
    #[serde(default)]
    pub categories: bool,
    #[serde(default)]
    pub eq: bool,
}

// ---------------------------------------------------------------------------
// Sender: selection → manifest
// ---------------------------------------------------------------------------

/// What the sender ticked in the picker, before resolution to concrete files.
#[derive(Debug, Clone, Default)]
pub struct Selection {
    pub whole_library: bool,
    pub artists: Vec<String>,
    pub albums: Vec<(String, String)>,
    pub song_paths: Vec<String>,
    pub audiobooks: bool,
    pub concerts: bool,
    /// YouTube channel db ids.
    pub yt_channels: Vec<i64>,
    /// YouTube playlist origin urls.
    pub yt_playlists: Vec<String>,
    /// YouTube video ids.
    pub yt_songs: Vec<String>,
    pub include_favorites: bool,
    pub include_playlists: bool,
    pub include_podcasts: bool,
    pub include_eq: bool,
    pub include_categories: bool,
}

/// Resolves a [`Selection`] against the local library into a [`ShareManifest`].
/// `peer_yt_enabled` gates whether YouTube items are included at all.
pub fn build_manifest(
    lib: &Library,
    sel: &Selection,
    peer_yt_enabled: bool,
) -> Result<ShareManifest> {
    let base = data::music_dir(lib);
    let tracks = lib.all_tracks()?;
    let by_path: HashMap<String, &crate::model::Track> =
        tracks.iter().map(|t| (t.path.clone(), t)).collect();

    // Collect the absolute track paths implied by the selection.
    let mut paths: BTreeSet<String> = BTreeSet::new();
    if sel.whole_library {
        paths.extend(tracks.iter().map(|t| t.path.clone()));
    }
    for artist in &sel.artists {
        for album in lib.albums_of_artist(artist).unwrap_or_default() {
            paths.extend(lib.album_track_paths(artist, &album).unwrap_or_default());
        }
    }
    for (artist, album) in &sel.albums {
        paths.extend(lib.album_track_paths(artist, album).unwrap_or_default());
    }
    paths.extend(sel.song_paths.iter().cloned());
    if sel.audiobooks {
        paths.extend(area_track_paths(lib, Area::Audiobooks, &tracks));
    }
    if sel.concerts {
        paths.extend(area_track_paths(lib, Area::Concerts, &tracks));
    }

    // Only real, hashable files end up in the manifest.
    let mut files = Vec::new();
    let mut total_size = 0u64;
    for abs in &paths {
        let Ok((size, hash)) = quick_hash(Path::new(abs)) else {
            continue; // missing/unreadable file – skip
        };
        let meta = by_path.get(abs);
        total_size += size;
        files.push(ManifestFile {
            rel_path: data::relativize(abs, &base),
            size,
            quick_hash: hash,
            title: meta.map(|t| t.title.clone()).unwrap_or_default(),
            artist: meta.and_then(|t| t.artist.clone()),
            album: meta.and_then(|t| t.album.clone()),
            duration_ms: meta.and_then(|t| t.duration_ms),
        });
    }

    // YouTube items (only if the peer can use them).
    let mut yt = Vec::new();
    if peer_yt_enabled {
        let channels = lib.channels().unwrap_or_default();
        for cid in &sel.yt_channels {
            if let Some((_, title, url, _thumb, _n)) = channels.iter().find(|c| &c.0 == cid) {
                let items = lib
                    .channel_videos(*cid)
                    .unwrap_or_default()
                    .into_iter()
                    .map(yt_video_to_rec)
                    .collect();
                let id = youtube::channel_id_from_url(url).unwrap_or_else(|| url.clone());
                yt.push(ManifestYt {
                    kind: YtKind::Channel,
                    id,
                    url: url.clone(),
                    title: title.clone(),
                    items,
                });
            }
        }
        let pls = lib.playlists_with_origin().unwrap_or_default();
        for origin in &sel.yt_playlists {
            if let Some((id, name, _count, _)) = pls.iter().find(|p| p.3.as_deref() == Some(origin))
            {
                let items = lib
                    .playlist_paths(*id)
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|p| youtube::parse_yt_path(p))
                    .map(|vid| yt_song_rec(lib, &vid))
                    .collect();
                yt.push(ManifestYt {
                    kind: YtKind::Playlist,
                    id: origin.clone(),
                    url: origin.clone(),
                    title: name.clone(),
                    items,
                });
            }
        }
        for vid in &sel.yt_songs {
            let rec = yt_song_rec(lib, vid);
            yt.push(ManifestYt {
                kind: YtKind::Song,
                id: vid.clone(),
                url: rec.url.clone(),
                title: rec.title.clone(),
                items: vec![rec],
            });
        }
    }

    // Library-data blobs.
    let library = LibraryBlobs {
        favorites: sel
            .include_favorites
            .then(|| data::export_favorites(lib, &base).unwrap_or_default()),
        playlists: sel
            .include_playlists
            .then(|| data::export_playlists_user(lib, &base).unwrap_or_default()),
        podcasts: sel
            .include_podcasts
            .then(|| data::export_podcasts(lib).unwrap_or_default()),
        categories: sel
            .include_categories
            .then(|| data::export_categories(lib, &base).unwrap_or_default()),
        eq: sel
            .include_eq
            .then(|| data::export_eq(lib, &base).unwrap_or_default()),
    };

    Ok(ShareManifest {
        schema: SCHEMA_VERSION,
        device_name: lib
            .get_setting("sync_device_name")
            .ok()
            .flatten()
            .unwrap_or_else(super::default_device_name),
        files,
        yt,
        library,
        total_size,
    })
}

/// Absolute track paths belonging to an [`Area`] (concerts / audiobooks),
/// expanding folder/album/artist marks down to individual tracks.
fn area_track_paths(lib: &Library, area: Area, tracks: &[crate::model::Track]) -> Vec<String> {
    let mut out = BTreeSet::new();
    for (scope, key, _title, _is_dir) in lib.area_entries(area, true, true) {
        match scope.as_str() {
            "track" => {
                out.insert(key);
            }
            "folder" => {
                let prefix = key.trim_end_matches('/').to_string();
                for t in tracks {
                    if t.path.starts_with(&format!("{prefix}/")) {
                        out.insert(t.path.clone());
                    }
                }
            }
            "album" => {
                if let Some((artist, album)) = key.split_once('\u{1}') {
                    out.extend(lib.album_track_paths(artist, album).unwrap_or_default());
                } else {
                    out.extend(lib.album_track_paths_by_name(&key).unwrap_or_default());
                }
            }
            "artist" => {
                for album in lib.albums_of_artist(&key).unwrap_or_default() {
                    out.extend(lib.album_track_paths(&key, &album).unwrap_or_default());
                }
            }
            _ => {}
        }
    }
    out.into_iter().collect()
}

fn yt_video_to_rec(v: YtVideo) -> YtVideoRec {
    YtVideoRec {
        video_id: v.video_id,
        title: v.title,
        url: v.url,
        duration: v.duration,
    }
}

fn yt_song_rec(lib: &Library, video_id: &str) -> YtVideoRec {
    let title = lib
        .yt_title(video_id)
        .ok()
        .flatten()
        .unwrap_or_else(|| video_id.to_string());
    YtVideoRec {
        video_id: video_id.to_string(),
        title,
        url: youtube::watch_url(video_id),
        duration: lib.yt_duration(video_id).ok().flatten(),
    }
}

// ---------------------------------------------------------------------------
// Receiver: classification (dedup / collision)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileStatus {
    /// No local file at this path.
    New,
    /// Local file has the same quick-hash — the receiver already has it.
    AlreadyHave,
    /// A *different* local file exists at this path — accepting would overwrite it.
    Collision,
}

#[derive(Debug, Clone)]
pub struct FileReview {
    pub file: ManifestFile,
    pub status: FileStatus,
    /// Default selection: New = on, AlreadyHave/Collision = off.
    pub selected: bool,
}

/// Classifies each manifest file against the local music folder.
pub fn review_files(lib: &Library, manifest: &ShareManifest) -> Vec<FileReview> {
    let base = data::music_dir(lib);
    manifest
        .files
        .iter()
        .map(|f| {
            let local = data::resolve(&f.rel_path, &base);
            let status = match quick_hash(Path::new(&local)) {
                Ok((_, h)) if h == f.quick_hash => FileStatus::AlreadyHave,
                Ok(_) => FileStatus::Collision, // exists, different content
                Err(_) => FileStatus::New,      // no local file
            };
            FileReview {
                file: f.clone(),
                selected: matches!(status, FileStatus::New),
                status,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Receiver: apply an accepted manifest
// ---------------------------------------------------------------------------

/// Applies an accepted manifest into `lib`. The audio bytes were already
/// transferred to the music folder by the worker; this applies the library-data
/// blobs (gated per facet by `decision`) and the YouTube items (only when local
/// YouTube is enabled). `decision.files` is honoured by the worker, not here.
pub fn apply_manifest(
    lib: &Library,
    manifest: &ShareManifest,
    decision: &ShareDecision,
) -> Result<ImportStats> {
    let base = data::music_dir(lib);
    let mut stats = ImportStats::default();

    if decision.favorites {
        if let Some(favs) = &manifest.library.favorites {
            stats.favorites = data::import_favorites(lib, &base, favs);
        }
    }
    if decision.playlists {
        if let Some(pls) = &manifest.library.playlists {
            stats.playlists = data::import_playlists(lib, &base, pls);
        }
    }
    if decision.podcasts {
        if let Some(pcs) = &manifest.library.podcasts {
            stats.podcasts = data::import_podcasts(lib, pcs);
        }
    }
    if decision.categories {
        if let Some(cats) = &manifest.library.categories {
            stats.categories = data::import_categories(lib, &base, cats);
        }
    }
    if decision.eq {
        if let Some(eqs) = &manifest.library.eq {
            stats.eq = data::import_eq(lib, &base, eqs);
        }
    }

    // YouTube only if this device has it enabled.
    let yt_enabled = lib.get_setting("youtube_enabled").ok().flatten().as_deref() == Some("1");
    if yt_enabled {
        for item in &manifest.yt {
            if !decision.yt.contains(&item.id) {
                continue;
            }
            apply_yt_item(lib, item);
        }
    }

    Ok(stats)
}

fn apply_yt_item(lib: &Library, item: &ManifestYt) {
    match item.kind {
        YtKind::Channel => {
            if let Ok(new_id) = lib.subscribe_channel(&item.id, &item.title, &item.url, None) {
                let videos: Vec<YtVideo> = item
                    .items
                    .iter()
                    .map(|v| YtVideo {
                        video_id: v.video_id.clone(),
                        title: v.title.clone(),
                        url: v.url.clone(),
                        duration: v.duration,
                        published: None,
                        thumbnail: None,
                    })
                    .collect();
                let _ = lib.set_channel_videos(new_id, &videos);
            }
        }
        YtKind::Playlist => {
            let paths: Vec<String> = item
                .items
                .iter()
                .map(|v| youtube::yt_path(&v.video_id))
                .collect();
            let _ = lib.replace_yt_playlist(&item.id, &item.title, &paths);
            for v in &item.items {
                let _ = lib.set_yt_meta(&v.video_id, &v.title, v.duration);
            }
        }
        YtKind::Song => {
            for v in &item.items {
                let _ = lib.set_yt_meta(&v.video_id, &v.title, v.duration);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Human-readable byte size (binary units, 1 decimal above KiB).
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", UNITS[i])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn music_dir_with(tag: &str, files: &[(&str, &[u8])]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("emilia-share-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (name, bytes) in files {
            let p = dir.join(name);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::File::create(&p).unwrap().write_all(bytes).unwrap();
        }
        dir
    }

    #[test]
    fn human_size_formats() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn build_manifest_selects_album_and_drops_yt_when_peer_disabled() {
        let dir = music_dir_with("album", &[("a/x.mp3", b"hello-x"), ("a/y.mp3", b"hello-y")]);
        let base = dir.to_string_lossy().to_string();
        let lib = Library::open_in_memory().unwrap();
        lib.set_setting("music_dir", &base).unwrap();
        for name in ["x", "y"] {
            let t = crate::model::Track {
                path: dir
                    .join(format!("a/{name}.mp3"))
                    .to_string_lossy()
                    .into_owned(),
                title: name.to_string(),
                artist: Some("Artist".into()),
                album: Some("Album".into()),
                ..Default::default()
            };
            lib.upsert_track(&t).unwrap();
        }
        let sel = Selection {
            albums: vec![("Artist".into(), "Album".into())],
            yt_songs: vec!["abc".into()],
            include_favorites: true,
            ..Default::default()
        };
        let m = build_manifest(&lib, &sel, false).unwrap();
        assert_eq!(m.files.len(), 2, "both album tracks");
        assert!(m.files.iter().all(|f| !f.rel_path.starts_with('/')));
        assert!(m.total_size > 0);
        assert!(m.yt.is_empty(), "YT dropped when peer has it disabled");
        assert!(m.library.favorites.is_some());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn review_marks_new_have_and_collision() {
        // Sender side files.
        let sdir = music_dir_with(
            "snd",
            &[
                ("song.mp3", b"the-same-bytes"),
                ("only.mp3", b"only-on-sender"),
            ],
        );
        let slib = Library::open_in_memory().unwrap();
        slib.set_setting("music_dir", &sdir.to_string_lossy())
            .unwrap();
        for name in ["song", "only"] {
            slib.upsert_track(&crate::model::Track {
                path: sdir
                    .join(format!("{name}.mp3"))
                    .to_string_lossy()
                    .into_owned(),
                title: name.into(),
                ..Default::default()
            })
            .unwrap();
        }
        let m = build_manifest(
            &slib,
            &Selection {
                whole_library: true,
                ..Default::default()
            },
            false,
        )
        .unwrap();

        // Receiver: has an identical song.mp3 (AlreadyHave) but a *different* file
        // would have to exist at only.mp3 to be a collision; here only.mp3 is new.
        let rdir = music_dir_with("rcv", &[("song.mp3", b"the-same-bytes")]);
        let rlib = Library::open_in_memory().unwrap();
        rlib.set_setting("music_dir", &rdir.to_string_lossy())
            .unwrap();
        let reviews = review_files(&rlib, &m);
        let by: HashMap<_, _> = reviews
            .iter()
            .map(|r| (r.file.rel_path.clone(), r))
            .collect();
        assert_eq!(by["song.mp3"].status, FileStatus::AlreadyHave);
        assert!(!by["song.mp3"].selected);
        assert_eq!(by["only.mp3"].status, FileStatus::New);
        assert!(by["only.mp3"].selected);

        // Now place a *different* file at song.mp3 → collision, unselected.
        std::fs::File::create(rdir.join("song.mp3"))
            .unwrap()
            .write_all(b"DIFFERENT")
            .unwrap();
        let reviews = review_files(&rlib, &m);
        let song = reviews
            .iter()
            .find(|r| r.file.rel_path == "song.mp3")
            .unwrap();
        assert_eq!(song.status, FileStatus::Collision);
        assert!(!song.selected, "collisions are not selected by default");

        let _ = std::fs::remove_dir_all(sdir);
        let _ = std::fs::remove_dir_all(rdir);
    }
}
