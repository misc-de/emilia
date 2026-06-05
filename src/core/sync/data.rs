//! Bridge between the SQLite `Library` and the transferable [`LibraryExport`].
//!
//! Deliberately without network/server so that export↔import is testable standalone.
//! File paths are stored relative to the music folder so they can be
//! resolved against the target device's music folder.

use anyhow::Result;

use crate::core::db::Library;
use crate::core::sync::protocol::*;
use crate::core::sync::ImportStats;

/// Reads the configured music folder (empty if not set).
pub(crate) fn music_dir(lib: &Library) -> String {
    lib.get_setting("music_dir").ok().flatten().unwrap_or_default()
}

/// For which scopes is the `key` a file path (instead of an artist/album name)?
pub(crate) fn key_is_path(scope: &str) -> bool {
    matches!(scope, "track" | "folder")
}

/// Make an absolute path relative to the music folder (otherwise leave unchanged).
pub(crate) fn relativize(path: &str, base: &str) -> String {
    if base.is_empty() {
        return path.to_string();
    }
    let base = base.trim_end_matches('/');
    match path.strip_prefix(base) {
        Some(rest) => rest.trim_start_matches('/').to_string(),
        None => path.to_string(),
    }
}

/// Resolve a relative path against the local music folder.
pub(crate) fn resolve(rel: &str, base: &str) -> String {
    if rel.starts_with('/') || base.is_empty() {
        return rel.to_string();
    }
    format!("{}/{}", base.trim_end_matches('/'), rel)
}

// --- Per-facet export helpers (reused by `export_library` and `share`) -------

pub(crate) fn export_favorites(lib: &Library, base: &str) -> Result<Vec<FavoriteRec>> {
    Ok(lib
        .favorites()?
        .into_iter()
        .map(|(scope, key, title, is_dir)| {
            let key = if key_is_path(&scope) { relativize(&key, base) } else { key };
            FavoriteRec { scope, key, title, is_dir }
        })
        .collect())
}

/// User playlists only (origin IS NULL) — YT-mirror playlists are conveyed as YT
/// items, never as plain path playlists.
pub(crate) fn export_playlists_user(lib: &Library, base: &str) -> Result<Vec<PlaylistRec>> {
    let mut playlists = Vec::new();
    for (id, name, _count, origin) in lib.playlists_with_origin()? {
        if origin.is_some() {
            continue;
        }
        let paths = lib
            .playlist_paths(id)?
            .into_iter()
            .map(|p| relativize(&p, base))
            .collect();
        playlists.push(PlaylistRec { name, paths });
    }
    Ok(playlists)
}

pub(crate) fn export_podcasts(lib: &Library) -> Result<Vec<PodcastRec>> {
    let progress: std::collections::HashMap<String, i64> = lib
        .all_episode_progress()
        .unwrap_or_default()
        .into_iter()
        .collect();
    let mut podcasts = Vec::new();
    for (id, title, image_url, _count) in lib.podcasts()? {
        if let Some(feed_url) = lib.podcast_feed_url(id)? {
            let episodes = lib
                .episodes(id)
                .unwrap_or_default()
                .into_iter()
                .map(|e| EpisodeRec {
                    position_ms: progress.get(&e.audio_url).copied().unwrap_or(0),
                    guid: e.guid,
                    title: e.title,
                    audio_url: e.audio_url,
                    published: e.published,
                    duration: e.duration,
                    description: e.description,
                })
                .collect();
            podcasts.push(PodcastRec { title, feed_url, image_url, episodes });
        }
    }
    Ok(podcasts)
}

pub(crate) fn export_categories(lib: &Library, base: &str) -> Result<Vec<CategoryRec>> {
    Ok(lib
        .all_categories()?
        .into_iter()
        .map(|(scope, key, value)| {
            let key = if key_is_path(&scope) { relativize(&key, base) } else { key };
            CategoryRec { scope, key, value }
        })
        .collect())
}

pub(crate) fn export_eq(lib: &Library, base: &str) -> Result<Vec<EqRec>> {
    Ok(lib
        .all_eq_settings()?
        .into_iter()
        .map(|(output, scope, key, bands)| {
            let key = if scope == "track" { relativize(&key, base) } else { key };
            EqRec { output, scope, key, bands }
        })
        .collect())
}

// --- Per-facet import helpers (reused by `import_library` and `share::apply`) -

pub(crate) fn import_favorites(lib: &Library, base: &str, favs: &[FavoriteRec]) -> usize {
    let mut n = 0;
    for f in favs {
        let key = if key_is_path(&f.scope) { resolve(&f.key, base) } else { f.key.clone() };
        if lib.set_favorite(&f.scope, &key, &f.title, f.is_dir, true).is_ok() {
            n += 1;
        }
    }
    n
}

pub(crate) fn import_playlists(lib: &Library, base: &str, pls: &[PlaylistRec]) -> usize {
    let existing: Vec<String> = lib
        .playlists()
        .unwrap_or_default()
        .into_iter()
        .map(|(_, name, _)| name)
        .collect();
    let mut n = 0;
    for pl in pls {
        if existing.contains(&pl.name) {
            continue; // don't duplicate a playlist with the same name
        }
        if let Ok(id) = lib.create_playlist(&pl.name) {
            let paths: Vec<String> = pl.paths.iter().map(|p| resolve(p, base)).collect();
            if lib.add_to_playlist(id, &paths).is_ok() {
                n += 1;
            }
        }
    }
    n
}

pub(crate) fn import_podcasts(lib: &Library, pcs: &[PodcastRec]) -> usize {
    let mut n = 0;
    for pc in pcs {
        if let Ok(id) = lib.subscribe_podcast(&pc.title, &pc.feed_url, pc.image_url.as_deref()) {
            n += 1;
            // Take over episodes incl. show notes – only if none exist locally yet,
            // so existing/more recent episodes (own feed fetch) aren't overwritten.
            if !pc.episodes.is_empty() && lib.episodes(id).map(|e| e.is_empty()).unwrap_or(false) {
                let eps: Vec<crate::model::Episode> = pc
                    .episodes
                    .iter()
                    .map(|e| crate::model::Episode {
                        guid: e.guid.clone(),
                        title: e.title.clone(),
                        audio_url: e.audio_url.clone(),
                        published: e.published.clone(),
                        duration: e.duration.clone(),
                        description: e.description.clone(),
                    })
                    .collect();
                let _ = lib.set_episodes(id, &eps);
            }
            // Merge episode positions: the furthest position wins.
            for ep in &pc.episodes {
                if ep.position_ms > 0
                    && ep.position_ms > lib.episode_progress(&ep.audio_url).unwrap_or(0)
                {
                    let _ = lib.set_episode_progress(&ep.audio_url, ep.position_ms);
                }
            }
        }
    }
    n
}

pub(crate) fn import_categories(lib: &Library, base: &str, cats: &[CategoryRec]) -> usize {
    let mut n = 0;
    for c in cats {
        let key = if key_is_path(&c.scope) { resolve(&c.key, base) } else { c.key.clone() };
        if lib.set_category(&c.scope, &key, Some(&c.value)).is_ok() {
            n += 1;
        }
    }
    n
}

pub(crate) fn import_eq(lib: &Library, base: &str, eqs: &[EqRec]) -> usize {
    let mut n = 0;
    for e in eqs {
        let key = if e.scope == "track" { resolve(&e.key, base) } else { e.key.clone() };
        if lib.set_eq(&e.output, &e.scope, &key, &e.bands).is_ok() {
            n += 1;
        }
    }
    n
}

/// Assembles the full library export (legacy whole-library path).
pub fn export_library(lib: &Library) -> Result<LibraryExport> {
    let base = music_dir(lib);
    let device_name = lib
        .get_setting("sync_device_name")
        .ok()
        .flatten()
        .unwrap_or_else(super::default_device_name);

    let files = lib
        .all_tracks()?
        .into_iter()
        .map(|t| {
            let size = std::fs::metadata(&t.path).map(|m| m.len()).unwrap_or(0);
            FileRec {
                path: relativize(&t.path, &base),
                title: t.title,
                artist: t.artist,
                album: t.album,
                duration_ms: t.duration_ms,
                size,
            }
        })
        .collect();

    Ok(LibraryExport {
        schema: SCHEMA_VERSION,
        device_name,
        favorites: export_favorites(lib, &base)?,
        playlists: export_playlists_user(lib, &base)?,
        podcasts: export_podcasts(lib)?,
        categories: export_categories(lib, &base)?,
        eq: export_eq(lib, &base)?,
        files,
    })
}

/// Applies a received export into the local library (additive/merging).
/// File paths are resolved against the local music folder. The audio files
/// themselves are transferred separately – only the metadata import counts here.
pub fn import_library(lib: &Library, exp: &LibraryExport) -> Result<ImportStats> {
    let base = music_dir(lib);
    Ok(ImportStats {
        favorites: import_favorites(lib, &base, &exp.favorites),
        playlists: import_playlists(lib, &base, &exp.playlists),
        podcasts: import_podcasts(lib, &exp.podcasts),
        categories: import_categories(lib, &base, &exp.categories),
        eq: import_eq(lib, &base, &exp.eq),
        files: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_roundtrip() {
        let src = Library::open_in_memory().unwrap();
        src.set_setting("music_dir", "/home/a/Musik").unwrap();
        src.set_favorite("album", "Künstler\u{1}Album", "Album", true, true)
            .unwrap();
        src.set_favorite("track", "/home/a/Musik/song.mp3", "Song", false, true)
            .unwrap();
        let pid = src.create_playlist("Lieblinge").unwrap();
        src.add_to_playlist(pid, &["/home/a/Musik/song.mp3".to_string()])
            .unwrap();
        src.set_category("track", "/home/a/Musik/song.mp3", Some("podcast"))
            .unwrap();
        src.set_eq("", "track", "/home/a/Musik/song.mp3", &[1.0; 10])
            .unwrap();

        let exp = export_library(&src).unwrap();
        // Paths are exported relative.
        assert!(exp.files.iter().all(|f| !f.path.starts_with('/')));
        assert_eq!(exp.playlists[0].paths[0], "song.mp3");

        // Import into a target device with a different music folder.
        let dst = Library::open_in_memory().unwrap();
        dst.set_setting("music_dir", "/data/Audio").unwrap();
        let stats = import_library(&dst, &exp).unwrap();
        assert_eq!(stats.favorites, 2);
        assert_eq!(stats.playlists, 1);
        assert_eq!(stats.eq, 1);

        // Track favorite resolved against the local music folder.
        assert!(dst.is_favorite("track", "/data/Audio/song.mp3"));
        assert!(dst.is_favorite("album", "Künstler\u{1}Album"));
        assert_eq!(
            dst.get_eq("", "track", "/data/Audio/song.mp3").unwrap(),
            Some([1.0; 10])
        );
    }

    #[test]
    fn podcast_episodes_and_shownotes_roundtrip() {
        let src = Library::open_in_memory().unwrap();
        let pid = src
            .subscribe_podcast(
                "Mein Podcast",
                "https://example.com/feed.xml",
                Some("https://example.com/cover.jpg"),
            )
            .unwrap();
        src.set_episodes(
            pid,
            &[crate::model::Episode {
                guid: Some("ep-1".into()),
                title: "Folge 1".into(),
                audio_url: "https://example.com/1.mp3".into(),
                published: Some("Mon, 01 Jan 2024".into()),
                duration: Some("00:30:00".into()),
                description: Some("Die Shownotes der Folge.".into()),
            }],
        )
        .unwrap();
        // Saved playback position of the episode.
        src.set_episode_progress("https://example.com/1.mp3", 90_000)
            .unwrap();

        // Export contains the episode incl. show notes and position.
        let exp = export_library(&src).unwrap();
        assert_eq!(exp.podcasts.len(), 1);
        assert_eq!(exp.podcasts[0].episodes.len(), 1);
        assert_eq!(
            exp.podcasts[0].episodes[0].description.as_deref(),
            Some("Die Shownotes der Folge.")
        );
        assert_eq!(exp.podcasts[0].episodes[0].position_ms, 90_000);

        // Import into an empty target device: episodes incl. show notes + position.
        let dst = Library::open_in_memory().unwrap();
        let stats = import_library(&dst, &exp).unwrap();
        assert_eq!(stats.podcasts, 1);
        let did = dst.podcasts().unwrap()[0].0;
        let eps = dst.episodes(did).unwrap();
        assert_eq!(eps.len(), 1);
        assert_eq!(eps[0].title, "Folge 1");
        assert_eq!(
            eps[0].description.as_deref(),
            Some("Die Shownotes der Folge.")
        );
        assert_eq!(
            dst.episode_progress("https://example.com/1.mp3").unwrap(),
            90_000
        );
    }
}
