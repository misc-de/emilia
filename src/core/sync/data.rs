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
fn music_dir(lib: &Library) -> String {
    lib.get_setting("music_dir").ok().flatten().unwrap_or_default()
}

/// For which scopes is the `key` a file path (instead of an artist/album name)?
fn key_is_path(scope: &str) -> bool {
    matches!(scope, "track" | "folder")
}

/// Make an absolute path relative to the music folder (otherwise leave unchanged).
fn relativize(path: &str, base: &str) -> String {
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
fn resolve(rel: &str, base: &str) -> String {
    if rel.starts_with('/') || base.is_empty() {
        return rel.to_string();
    }
    format!("{}/{}", base.trim_end_matches('/'), rel)
}

/// Assembles the full library export.
pub fn export_library(lib: &Library) -> Result<LibraryExport> {
    let base = music_dir(lib);
    let device_name = lib
        .get_setting("sync_device_name")
        .ok()
        .flatten()
        .unwrap_or_else(super::default_device_name);

    let favorites = lib
        .favorites()?
        .into_iter()
        .map(|(scope, key, title, is_dir)| {
            let key = if key_is_path(&scope) {
                relativize(&key, &base)
            } else {
                key
            };
            FavoriteRec {
                scope,
                key,
                title,
                is_dir,
            }
        })
        .collect();

    let mut playlists = Vec::new();
    for (id, name, _count) in lib.playlists()? {
        let paths = lib
            .playlist_paths(id)?
            .into_iter()
            .map(|p| relativize(&p, &base))
            .collect();
        playlists.push(PlaylistRec { name, paths });
    }

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
            podcasts.push(PodcastRec {
                title,
                feed_url,
                image_url,
                episodes,
            });
        }
    }

    let categories = lib
        .all_categories()?
        .into_iter()
        .map(|(scope, key, value)| {
            let key = if key_is_path(&scope) {
                relativize(&key, &base)
            } else {
                key
            };
            CategoryRec { scope, key, value }
        })
        .collect();

    let eq = lib
        .all_eq_settings()?
        .into_iter()
        .map(|(output, scope, key, bands)| {
            let key = if scope == "track" {
                relativize(&key, &base)
            } else {
                key
            };
            EqRec {
                output,
                scope,
                key,
                bands,
            }
        })
        .collect();

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
        favorites,
        playlists,
        podcasts,
        categories,
        eq,
        files,
    })
}

/// Applies a received export into the local library (additive/merging).
/// File paths are resolved against the local music folder. The audio files
/// themselves are transferred separately – only the metadata import counts here.
pub fn import_library(lib: &Library, exp: &LibraryExport) -> Result<ImportStats> {
    let base = music_dir(lib);
    let mut stats = ImportStats::default();

    for f in &exp.favorites {
        let key = if key_is_path(&f.scope) {
            resolve(&f.key, &base)
        } else {
            f.key.clone()
        };
        if lib.set_favorite(&f.scope, &key, &f.title, f.is_dir, true).is_ok() {
            stats.favorites += 1;
        }
    }

    let existing: Vec<String> = lib.playlists()?.into_iter().map(|(_, n, _)| n).collect();
    for pl in &exp.playlists {
        if existing.contains(&pl.name) {
            continue; // don't duplicate a playlist with the same name
        }
        let id = lib.create_playlist(&pl.name)?;
        let paths: Vec<String> = pl.paths.iter().map(|p| resolve(p, &base)).collect();
        lib.add_to_playlist(id, &paths)?;
        stats.playlists += 1;
    }

    for pc in &exp.podcasts {
        if let Ok(id) = lib.subscribe_podcast(&pc.title, &pc.feed_url, pc.image_url.as_deref()) {
            stats.podcasts += 1;
            // Take over episodes incl. show notes – but only if none exist
            // locally yet, so that existing/more recent episodes (from one's own
            // feed fetch) are not overwritten.
            if !pc.episodes.is_empty()
                && lib.episodes(id).map(|e| e.is_empty()).unwrap_or(false)
            {
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
            // Merge episode positions: the furthest position wins, so that on
            // every device you continue listening from where you were furthest along.
            for ep in &pc.episodes {
                if ep.position_ms > 0
                    && ep.position_ms > lib.episode_progress(&ep.audio_url).unwrap_or(0)
                {
                    let _ = lib.set_episode_progress(&ep.audio_url, ep.position_ms);
                }
            }
        }
    }

    for c in &exp.categories {
        let key = if key_is_path(&c.scope) {
            resolve(&c.key, &base)
        } else {
            c.key.clone()
        };
        if lib.set_category(&c.scope, &key, Some(&c.value)).is_ok() {
            stats.categories += 1;
        }
    }

    for e in &exp.eq {
        let key = if e.scope == "track" {
            resolve(&e.key, &base)
        } else {
            e.key.clone()
        };
        if lib.set_eq(&e.output, &e.scope, &key, &e.bands).is_ok() {
            stats.eq += 1;
        }
    }

    Ok(stats)
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
