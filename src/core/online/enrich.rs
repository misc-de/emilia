//! Library enrichment: matches albums/artists/tracks online and stores the
//! results (covers, years, MBIDs, photos, galleries, fingerprint suggestions)
//! in the cache and the database.

use std::path::Path;

#[cfg(doc)]
use super::RATE_LIMIT;
use super::{save_artist_image, save_cover, save_gallery_image, OnlineClient};
use crate::core::db::Library;
use crate::core::fingerprint;
use crate::model::{AlbumMeta, ArtistMeta, TrackMeta};

/// Enriches a single album: search for the release, load the cover, store it in
/// the DB. Returns the resulting entry (with `status`).
///
/// Makes exactly one MusicBrainz request – the caller is responsible for
/// honoring the rate limit ([`RATE_LIMIT`]) between calls.
pub fn enrich_album(client: &OnlineClient, lib: &Library, artist: &str, album: &str) -> AlbumMeta {
    let mut meta = AlbumMeta::pending(artist, album);

    match client.match_release(artist, album) {
        Ok(Some(rel)) => {
            meta.mbid = Some(rel.mbid.clone());
            meta.year = rel.year;
            meta.status = "matched".to_string();

            match client.fetch_cover(&rel) {
                Ok(Some(bytes)) => match save_cover(&rel.mbid, &bytes) {
                    Ok(path) => meta.cover_path = Some(path.to_string_lossy().into_owned()),
                    Err(e) => tracing::warn!("Failed to save cover art: {e}"),
                },
                Ok(None) => {}
                Err(e) => tracing::warn!("Cover art fetch failed ({artist} – {album}): {e}"),
            }
        }
        Ok(None) => meta.status = "notfound".to_string(),
        Err(e) => {
            // Debug, not warn: during an outage this fires for every album of the
            // sweep. The caller reports the outage once and stops (see
            // `crate::ui::enrich`).
            tracing::debug!("MusicBrainz search failed ({artist} – {album}): {e}");
            meta.status = "error".to_string();
        }
    }

    if let Err(e) = lib.upsert_album_meta(&meta) {
        tracing::error!("Failed to save album_meta: {e}");
    }
    meta
}

/// Release-year backfill: looks the album up on MusicBrainz purely to obtain the
/// release year (and mbid), then stores it via [`Library::set_album_year`],
/// which preserves any existing cover/mbid and bounds the retries. One
/// MusicBrainz request — the caller honours [`RATE_LIMIT`] between calls.
///
/// Returns whether the lookup failed with a network/server error (as opposed to
/// simply finding nothing), so a sweep can stop while the service is down
/// instead of walking the whole library against it.
pub fn enrich_album_year(client: &OnlineClient, lib: &Library, artist: &str, album: &str) -> bool {
    let (year, mbid, errored) = match client.match_release(artist, album) {
        Ok(Some(rel)) => (rel.year, Some(rel.mbid), false),
        Ok(None) => (None, None, false),
        Err(e) => {
            tracing::debug!("MusicBrainz year lookup failed ({artist} – {album}): {e}");
            (None, None, true)
        }
    };
    if let Err(e) = lib.set_album_year(artist, album, year, mbid.as_deref()) {
        tracing::warn!("Failed to store album year ({artist} – {album}): {e}");
    }
    errored
}

/// Fetches the MusicBrainz id (and year) for an album **without** touching an
/// existing cover. Used to enable the online cover *gallery* (alternatives) for
/// albums that already show the user's embedded cover — so the embedded artwork
/// stays the primary and the online images are merely offered as a choice.
/// Returns the mbid (existing or freshly matched), or `None` if no match.
pub fn match_album_mbid(
    client: &OnlineClient,
    lib: &Library,
    artist: &str,
    album: &str,
) -> Option<String> {
    let mut meta = lib
        .get_album_meta(artist, album)
        .ok()
        .flatten()
        .unwrap_or_else(|| AlbumMeta::pending(artist, album));
    if meta.mbid.as_deref().is_some_and(|m| !m.is_empty()) {
        return meta.mbid;
    }
    match client.match_release(artist, album) {
        Ok(Some(rel)) => {
            meta.mbid = Some(rel.mbid.clone());
            if meta.year.is_none() {
                meta.year = rel.year;
            }
            // Keep a "local" status (embedded cover) so the cover is never
            // counted as an online match nor overwritten.
            if meta.status != "local" {
                meta.status = "matched".to_string();
            }
            if let Err(e) = lib.upsert_album_meta(&meta) {
                tracing::error!("Failed to save album_meta (mbid): {e}");
            }
            Some(rel.mbid)
        }
        _ => None,
    }
}

/// Persists an already (possibly in parallel) loaded artist photo: stores the
/// bytes in the cache and builds the meta entry. Does **not** write to the DB –
/// the caller does that serialized (a single SQLite connection).
pub fn store_artist_image(name: &str, image: Option<Vec<u8>>, errored: bool) -> ArtistMeta {
    let mut meta = ArtistMeta::pending(name);
    match image {
        Some(bytes) => match save_artist_image(name, &bytes) {
            Ok(path) => {
                meta.image_path = Some(path.to_string_lossy().into_owned());
                meta.status = "matched".to_string();
            }
            Err(e) => {
                tracing::warn!("Failed to save artist photo ({name}): {e}");
                meta.status = "error".to_string();
            }
        },
        None => {
            meta.status = if errored { "error" } else { "notfound" }.to_string();
        }
    }
    meta
}

/// Stores an **already loaded** album gallery in the cache and in the DB.
/// (The network fetch happens separately – so multiple albums can load in
/// parallel and only the writing is serialized via the coordinator.)
pub fn store_album_gallery(
    lib: &Library,
    artist: &str,
    album: &str,
    imgs: &[(Vec<u8>, String)],
) -> usize {
    let key = format!("{artist}{}{album}", char::from(1u8));
    let mut stored = Vec::new();
    // Keep the current (embedded/local) cover as the first candidate so the
    // user can always pick the artwork they put into the files back in the
    // detail gallery — the online images are offered as alternatives, not as a
    // replacement.
    if let Some(local) = lib
        .get_album_meta(artist, album)
        .ok()
        .flatten()
        .and_then(|m| m.cover_path)
    {
        if std::path::Path::new(&local).exists() {
            stored.push((local, "front".to_string(), "local".to_string()));
        }
    }
    for (i, (bytes, kind)) in imgs.iter().enumerate() {
        match save_gallery_image("albimg", &key, i, bytes) {
            Ok(pp) => stored.push((
                pp.to_string_lossy().into_owned(),
                kind.clone(),
                "caa".to_string(),
            )),
            Err(e) => tracing::warn!("Failed to save gallery image: {e}"),
        }
    }
    // Only worth a gallery when there is an actual choice (more than the local
    // cover). With no online images, leave the single cover to `ctx_cover`.
    if !imgs.is_empty() && !stored.is_empty() {
        let _ = lib.set_album_images(artist, album, &stored);
    }
    stored.len()
}

/// Fetches & stores an artist's image gallery (fanart.tv) into the DB.
pub fn enrich_artist_gallery(
    client: &OnlineClient,
    lib: &Library,
    name: &str,
    api_key: &str,
) -> usize {
    let mbid = match client.artist_mbid(name) {
        Ok(Some(id)) => id,
        Ok(None) => return 0,
        Err(e) => {
            tracing::warn!("Artist MBID lookup failed ({name}): {e}");
            return 0;
        }
    };
    let imgs = match client.fetch_artist_gallery(api_key, &mbid) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("Artist gallery failed ({name}): {e}");
            return 0;
        }
    };
    let mut stored = Vec::new();
    for (i, (bytes, kind)) in imgs.iter().enumerate() {
        match save_gallery_image("artimg", name, i, bytes) {
            Ok(pp) => stored.push((
                pp.to_string_lossy().into_owned(),
                kind.clone(),
                "fanart".to_string(),
            )),
            Err(e) => tracing::warn!("Failed to save artist image: {e}"),
        }
    }
    if !stored.is_empty() {
        let _ = lib.set_artist_images(name, &stored);
    }
    stored.len()
}

/// Fetches & stores an album's cover gallery (Cover Art Archive) into the DB.
/// Requires the MBID already found in the album metadata (it is created during
/// the single-cover fetch [`enrich_album`]); without an MBID nothing happens. For
/// the on-demand fetch when opening the album detail view.
pub fn enrich_album_gallery(
    client: &OnlineClient,
    lib: &Library,
    artist: &str,
    album: &str,
) -> usize {
    let Some(mbid) = lib
        .get_album_meta(artist, album)
        .ok()
        .flatten()
        .and_then(|m| m.mbid)
    else {
        return 0;
    };
    let imgs = client.fetch_album_gallery(&mbid).unwrap_or_default();
    store_album_gallery(lib, artist, album, &imgs)
}

/// Detects a track via fingerprint (Chromaprint → AcoustID) and stores the
/// **suggested** metadata in the DB. The file is only read.
///
/// Makes a single AcoustID request; called on demand during playback (naturally
/// spread out by the playback pace), hence without its own throttle pause.
pub fn enrich_track_fingerprint(
    client: &OnlineClient,
    lib: &Library,
    client_key: &str,
    path: &Path,
) -> TrackMeta {
    let mut meta = TrackMeta::pending(path.to_string_lossy().into_owned());

    let fp = match fingerprint::compute(path) {
        Ok(fp) => fp,
        Err(e) => {
            tracing::warn!("Fingerprint failed ({}): {e}", path.display());
            meta.status = "error".to_string();
            let _ = lib.upsert_track_meta(&meta);
            return meta;
        }
    };

    match client.acoustid_lookup(client_key, &fp) {
        Ok(Some(m)) => {
            meta.recording_mbid = Some(m.recording_mbid);
            meta.title = m.title;
            meta.artist = m.artist;
            meta.album = m.album;
            meta.status = "matched".to_string();
        }
        Ok(None) => meta.status = "notfound".to_string(),
        Err(e) => {
            tracing::warn!("AcoustID fetch failed ({}): {e}", path.display());
            meta.status = "error".to_string();
        }
    }

    if let Err(e) = lib.upsert_track_meta(&meta) {
        tracing::error!("Failed to save track_meta: {e}");
    }
    meta
}
