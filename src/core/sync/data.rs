//! Brücke zwischen der SQLite-`Library` und dem übertragbaren [`LibraryExport`].
//!
//! Bewusst ohne Netzwerk/Server, damit Export↔Import eigenständig testbar ist.
//! Dateipfade werden relativ zum Musikordner abgelegt, damit sie auf dem
//! Zielgerät gegen dessen Musikordner aufgelöst werden können.

use anyhow::Result;

use crate::core::db::Library;
use crate::core::sync::protocol::*;
use crate::core::sync::ImportStats;

/// Liest den eingestellten Musikordner (leer, falls nicht gesetzt).
fn music_dir(lib: &Library) -> String {
    lib.get_setting("music_dir").ok().flatten().unwrap_or_default()
}

/// Für welche Ebenen ist der `key` ein Dateipfad (statt Interpret/Album-Name)?
fn key_is_path(scope: &str) -> bool {
    matches!(scope, "track" | "folder")
}

/// Absoluten Pfad relativ zum Musikordner machen (sonst unverändert lassen).
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

/// Relativen Pfad gegen den lokalen Musikordner auflösen.
fn resolve(rel: &str, base: &str) -> String {
    if rel.starts_with('/') || base.is_empty() {
        return rel.to_string();
    }
    format!("{}/{}", base.trim_end_matches('/'), rel)
}

/// Stellt den vollständigen Bibliotheks-Export zusammen.
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

    let mut podcasts = Vec::new();
    for (id, title, image_url, _count) in lib.podcasts()? {
        if let Some(feed_url) = lib.podcast_feed_url(id)? {
            podcasts.push(PodcastRec {
                title,
                feed_url,
                image_url,
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

/// Spielt einen empfangenen Export in die lokale Bibliothek ein (additiv/mergend).
/// Dateipfade werden gegen den lokalen Musikordner aufgelöst. Die Audiodateien
/// selbst werden separat übertragen – hier zählt nur der Metadaten-Import.
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
            continue; // gleichnamige Playlist nicht duplizieren
        }
        let id = lib.create_playlist(&pl.name)?;
        let paths: Vec<String> = pl.paths.iter().map(|p| resolve(p, &base)).collect();
        lib.add_to_playlist(id, &paths)?;
        stats.playlists += 1;
    }

    for pc in &exp.podcasts {
        if lib
            .subscribe_podcast(&pc.title, &pc.feed_url, pc.image_url.as_deref())
            .is_ok()
        {
            stats.podcasts += 1;
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
        // Pfade sind relativ exportiert.
        assert!(exp.files.iter().all(|f| !f.path.starts_with('/')));
        assert_eq!(exp.playlists[0].paths[0], "song.mp3");

        // Import in ein Zielgerät mit anderem Musikordner.
        let dst = Library::open_in_memory().unwrap();
        dst.set_setting("music_dir", "/data/Audio").unwrap();
        let stats = import_library(&dst, &exp).unwrap();
        assert_eq!(stats.favorites, 2);
        assert_eq!(stats.playlists, 1);
        assert_eq!(stats.eq, 1);

        // Track-Favorit gegen den lokalen Musikordner aufgelöst.
        assert!(dst.is_favorite("track", "/data/Audio/song.mp3"));
        assert!(dst.is_favorite("album", "Künstler\u{1}Album"));
        assert_eq!(
            dst.get_eq("", "track", "/data/Audio/song.mp3").unwrap(),
            Some([1.0; 10])
        );
    }
}
