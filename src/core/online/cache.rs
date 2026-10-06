//! Cache and download locations plus the on-disk image caches: album/track
//! covers, artist photos, podcast/station/YouTube images and recording covers.
//! Helpers that only look at the cache never touch the network (UI-thread
//! safe); those that fill it on demand are marked **Network**.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use anyhow::Result;

use super::{MAX_IMAGE_EDGE, THUMB_FETCH_THREADS, name_hash, shared_client};
use crate::core::cover;
#[cfg(doc)]
use crate::core::db::Library;

/// Directory for cached covers: `$XDG_CACHE_HOME/emilia/covers`.
pub fn cover_cache_dir() -> PathBuf {
    cache_subdir("covers")
}

/// Directory for artist photos: `$XDG_CACHE_HOME/emilia/artists`.
pub fn artist_cache_dir() -> PathBuf {
    cache_subdir("artists")
}

/// File the persistent **list thumbnail** of the image at `source` is kept in
/// (`$XDG_CACHE_HOME/emilia/thumbs`, see [`crate::ui::widgets::decode_thumb`]).
/// `stamp` identifies the source's current content (mtime + size), so a
/// replaced image gets a fresh thumbnail instead of the stale one.
pub fn thumb_cache_path(source: &str, stamp: &str) -> PathBuf {
    let mut p = cache_subdir("thumbs");
    p.push(format!("{}-{stamp}.img", name_hash(source)));
    p
}

fn cache_subdir(name: &str) -> PathBuf {
    let mut dir = dirs::cache_dir().unwrap_or_else(|| PathBuf::from("."));
    dir.push("emilia");
    dir.push(name);
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Whether `path` points at an image file that is gone for good, so the pointer
/// stored for it in the database is stale and should be dropped (see
/// [`Library::prune_lost_images`]) instead of being taken as "this album/artist
/// already has an image".
///
/// Files inside the cache are disposable by definition — `~/.cache/emilia` may
/// be cleared at any time, and a Flatpak install has its own separate cache — so
/// a missing one always counts as lost. Outside the cache (e.g. a `folder.jpg`
/// next to the music) the containing folder has to exist as well: an unmounted
/// or briefly unreadable library must not get its cover pointers wiped, the same
/// caution [`Library::prune_tracks_under`] takes with the tracks themselves.
pub fn image_file_lost(path: &str) -> bool {
    let p = Path::new(path);
    if path.trim().is_empty() || p.exists() {
        return false;
    }
    if p.starts_with(cover_cache_dir()) || p.starts_with(artist_cache_dir()) {
        return true;
    }
    p.parent().is_some_and(|dir| dir.exists())
}

/// Directory for downloaded podcast episodes (offline playback):
/// `$XDG_DATA_HOME/emilia/podcasts`. Unlike the cover cache this lives under the
/// **data** dir (next to the library DB), so the OS won't purge offline
/// episodes the way it may clear `~/.cache`.
pub fn podcast_download_dir() -> PathBuf {
    let mut dir = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
    dir.push("emilia");
    dir.push("podcasts");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Destination file for an episode download (stable name derived from the audio
/// URL, original extension preserved when recognisable so the file is also
/// usable outside the app). The extension is cosmetic – playback uses content
/// type detection, not the suffix.
pub fn episode_download_dest(url: &str) -> PathBuf {
    let ext = episode_extension(url);
    let mut p = podcast_download_dir();
    p.push(format!("{}.{ext}", name_hash(url)));
    p
}

/// File extension for an episode download: the audio URL's own one (lower
/// case, 1–4 alphanumerics, query/fragment ignored), `audio` otherwise.
fn episode_extension(url: &str) -> String {
    url.split(['?', '#'])
        .next()
        .and_then(|p| p.rsplit('.').next())
        .map(|e| e.to_ascii_lowercase())
        .filter(|e| (1..=4).contains(&e.len()) && e.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or_else(|| "audio".to_string())
}

/// Stores the cover bytes in the cache and returns the path.
pub(super) fn save_cover(mbid: &str, bytes: &[u8]) -> Result<PathBuf> {
    let key = cover_file_key(mbid);
    let mut path = cover_cache_dir();
    path.push(format!("{key}.img"));
    std::fs::write(&path, bytes)?;
    Ok(path)
}

/// File name stem a release cover is cached under.
///
/// `mbid` comes from a remote MusicBrainz response and is used as a file name,
/// so it must never be trusted verbatim. A genuine MBID is a UUID (hex digits
/// plus hyphens); anything else (e.g. a malicious `../../x`) is replaced by a
/// safe hash so the write can never escape the cache directory.
fn cover_file_key(mbid: &str) -> String {
    if !mbid.is_empty() && mbid.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
        mbid.to_string()
    } else {
        name_hash(mbid)
    }
}

/// Caps an encoded image at [`MAX_IMAGE_EDGE`] px on its longer edge (aspect
/// kept): images within the cap come back untouched, larger ones re-encoded —
/// JPEG for opaque images, PNG when there is transparency to keep (logos).
/// Undecodable input is returned as is, so a broken download is stored and
/// fails the same way it would have anyway. Decoding happens at the reduced
/// size where the format allows (JPEG), so this is cheaper than a full decode.
pub fn shrink_image(bytes: Vec<u8>) -> Vec<u8> {
    use gtk::gdk_pixbuf::PixbufLoader;
    use gtk::prelude::*;
    use std::cell::Cell;
    use std::rc::Rc;

    let loader = PixbufLoader::new();
    let shrunk = Rc::new(Cell::new(false));
    {
        let shrunk = shrunk.clone();
        loader.connect_size_prepared(move |loader, w, h| {
            let edge = w.max(h);
            if edge > MAX_IMAGE_EDGE {
                let scale = f64::from(MAX_IMAGE_EDGE) / f64::from(edge);
                let dim = |v: i32| ((f64::from(v) * scale).round() as i32).max(1);
                loader.set_size(dim(w), dim(h));
                shrunk.set(true);
            }
        });
    }
    if loader.write(&bytes).is_err() || loader.close().is_err() {
        return bytes;
    }
    if !shrunk.get() {
        return bytes;
    }
    let Some(pixbuf) = loader.pixbuf() else {
        return bytes;
    };
    let encoded = if pixbuf.has_alpha() {
        pixbuf.save_to_bufferv("png", &[])
    } else {
        pixbuf.save_to_bufferv("jpeg", &[("quality", "90")])
    };
    encoded.unwrap_or(bytes)
}

/// Determines a **local** album cover entirely without the network: prefers the
/// image embedded in the sample track's tags, otherwise a folder image
/// (`cover.jpg`, `folder.png`, …) — but only with `folder_ok`, i.e. when the
/// caller made sure the folder belongs to this album: in a mixed folder the
/// image shows some other album. Returns the path to the displayable cover
/// file. The audio file is only read in the process.
pub fn local_album_cover(
    artist: &str,
    album: &str,
    sample_path: &str,
    folder_ok: bool,
) -> Option<String> {
    let p = Path::new(sample_path);

    // 1) Embedded tag image → write to the cache.
    if let Some(bytes) = cover::embedded_cover(p)
        && let Ok(path) = save_local_cover(artist, album, &bytes)
    {
        return Some(path.to_string_lossy().into_owned());
    }
    // 2) Folder image → use its path directly (no copying needed).
    if let Some(dir) = p.parent().filter(|_| folder_ok)
        && let Some(img) = cover::find_cover_file(dir)
    {
        return Some(img.to_string_lossy().into_owned());
    }
    None
}

/// Cover of a **single track** exclusively from the **embedded** tag image
/// (written to the cache once, key = track path). Deliberately **no** folder
/// image as a fallback: a single/guest track in a foreign album folder should
/// not inherit its `cover.jpg`. If an embedded image is missing, the function
/// returns `None` – the caller then falls back to the album or artist cover,
/// never to a foreign folder image. The audio file is only read.
pub fn local_track_cover(path: &str) -> Option<String> {
    let p = Path::new(path);

    let mut cache = cover_cache_dir();
    cache.push(format!("track_{}.img", name_hash(path)));
    if cache.exists() {
        return Some(cache.to_string_lossy().into_owned());
    }
    let bytes = cover::embedded_cover(p)?;
    std::fs::write(&cache, &bytes).ok()?;
    Some(cache.to_string_lossy().into_owned())
}

/// Stores album cover bytes (e.g. pulled from a remote source over WebDAV) in
/// the cache under the same key as [`local_album_cover`] and returns the path.
pub fn store_album_cover_bytes(artist: &str, album: &str, bytes: &[u8]) -> Option<String> {
    save_local_cover(artist, album, bytes)
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

/// Stores embedded cover bytes of a single track under the same key as
/// [`local_track_cover`], so the display picks it up without any network access.
pub fn store_track_cover_bytes(path: &str, bytes: &[u8]) -> Option<String> {
    let mut cache = cover_cache_dir();
    cache.push(format!("track_{}.img", name_hash(path)));
    std::fs::write(&cache, bytes).ok()?;
    Some(cache.to_string_lossy().into_owned())
}

/// Whether a per-track cover is already cached (no network – UI-thread safe).
pub fn track_cover_cached(path: &str) -> bool {
    let mut cache = cover_cache_dir();
    cache.push(format!("track_{}.img", name_hash(path)));
    cache.exists()
}

/// Local cache path of a podcast image (key = image URL), **only if the file is
/// already present** – without network access (for display in the UI thread).
pub fn podcast_image_path(url: &str) -> Option<String> {
    if url.trim().is_empty() {
        return None;
    }
    let mut p = cover_cache_dir();
    p.push(format!("podcast_{}.img", name_hash(url)));
    p.exists().then(|| p.to_string_lossy().into_owned())
}

/// Loads the podcast image (RSS/iTunes) into the cache on demand and returns the
/// local path. **Network access** – only call from worker/background threads.
/// Already cached images are not loaded again.
pub fn cache_podcast_image(url: &str) -> Option<String> {
    if let Some(p) = podcast_image_path(url) {
        return Some(p);
    }
    if url.trim().is_empty() {
        return None;
    }
    let bytes = shared_client().get_image(url).ok().flatten()?;
    let mut p = cover_cache_dir();
    p.push(format!("podcast_{}.img", name_hash(url)));
    std::fs::write(&p, &bytes).ok()?;
    Some(p.to_string_lossy().into_owned())
}

/// Downloads a podcast image again (detail refresh), replacing the cached copy.
/// Keeps the cached one when the download fails. **Network.**
pub fn recache_podcast_image(url: &str) -> Option<String> {
    if url.trim().is_empty() {
        return None;
    }
    if let Some(bytes) = shared_client().get_image(url).ok().flatten() {
        let mut p = cover_cache_dir();
        p.push(format!("podcast_{}.img", name_hash(url)));
        if std::fs::write(&p, &bytes).is_ok() {
            return Some(p.to_string_lossy().into_owned());
        }
    }
    podcast_image_path(url)
}

/// Prefix of a station "favicon" that points at a logo the user picked from a
/// local file (copied into [`station_logo_dir`]) instead of an image URL.
const LOCAL_LOGO_PREFIX: &str = "file://";

/// Directory for station logos picked from a local file:
/// `$XDG_DATA_HOME/emilia/station-logos`. Lives under the **data** dir so the OS
/// never purges a logo the user chose by hand.
fn station_logo_dir() -> PathBuf {
    let mut dir = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
    dir.push("emilia");
    dir.push("station-logos");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Whether a station favicon is a hand-picked local logo (`file://…`), which
/// only exists on this device.
pub fn is_local_station_logo(favicon: &str) -> bool {
    favicon.starts_with(LOCAL_LOGO_PREFIX)
}

/// Local cache path of a station logo (key = image URL), **only if the file is
/// already present** – without network access (for display in the UI thread).
/// A hand-picked local logo (`file://…`) resolves to its own file.
pub fn station_image_path(url: &str) -> Option<String> {
    if url.trim().is_empty() {
        return None;
    }
    if let Some(local) = url.strip_prefix(LOCAL_LOGO_PREFIX) {
        return Path::new(local).exists().then(|| local.to_string());
    }
    let mut p = cover_cache_dir();
    p.push(format!("station_{}.img", name_hash(url)));
    p.exists().then(|| p.to_string_lossy().into_owned())
}

/// Loads the station logo into the cache on demand and returns the local path.
/// **Network access** – only call from worker/background threads. Already
/// cached logos are not loaded again.
pub fn cache_station_image(url: &str) -> Option<String> {
    if let Some(p) = station_image_path(url) {
        return Some(p);
    }
    if url.trim().is_empty() || is_local_station_logo(url) {
        return None;
    }
    let bytes = shared_client().get_image(url).ok().flatten()?;
    store_station_image(url, &bytes)
}

/// Like [`cache_station_image`], but only accepts a download that really is a
/// decodable image of at least `min_edge` px (a soft-404 HTML page or a 16 px
/// favicon is rejected). Used when probing candidate logo URLs. **Network.**
pub fn cache_station_image_checked(url: &str, min_edge: i32) -> Option<String> {
    if let Some(p) = station_image_path(url) {
        return Some(p);
    }
    let bytes = shared_client().get_image(url).ok().flatten()?;
    let (w, h) = image_size(&bytes)?;
    if w.max(h) < min_edge {
        return None;
    }
    store_station_image(url, &bytes)
}

/// Pixel size of encoded image bytes, `None` if they don't decode.
fn image_size(bytes: &[u8]) -> Option<(i32, i32)> {
    use gtk::gdk_pixbuf::PixbufLoader;
    use gtk::prelude::*;
    let loader = PixbufLoader::new();
    if loader.write(bytes).is_err() || loader.close().is_err() {
        return None;
    }
    let pb = loader.pixbuf()?;
    Some((pb.width(), pb.height()))
}

fn store_station_image(url: &str, bytes: &[u8]) -> Option<String> {
    let mut p = cover_cache_dir();
    p.push(format!("station_{}.img", name_hash(url)));
    std::fs::write(&p, bytes).ok()?;
    Some(p.to_string_lossy().into_owned())
}

/// Copies a user-picked image file into [`station_logo_dir`] (downscaled like
/// any cover) and returns the favicon value to store for the station
/// (`file://…`). `None` if the file can't be read or isn't an image.
pub fn import_station_logo(src: &Path) -> Option<String> {
    let bytes = std::fs::read(src).ok()?;
    image_size(&bytes)?;
    let bytes = shrink_image(bytes);
    let mut p = station_logo_dir();
    // Content hash as name: picking the same image again reuses the file, a
    // different one gets a new path (so no stale thumbnail is shown).
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    p.push(format!("{:016x}.img", h.finish()));
    std::fs::write(&p, &bytes).ok()?;
    Some(format!("{LOCAL_LOGO_PREFIX}{}", p.to_string_lossy()))
}

/// Local cache path of an enriched YouTube cover (key = video id), if present.
pub fn youtube_cover_path(video_id: &str) -> Option<String> {
    let mut p = cover_cache_dir();
    p.push(format!("ytcover_{}.img", name_hash(video_id)));
    p.exists().then(|| p.to_string_lossy().into_owned())
}

/// Stores an enriched cover (looked up online) for a video id and returns the
/// path. Lets the recent list and a later library track show it.
pub fn store_youtube_cover(video_id: &str, bytes: &[u8]) -> Option<String> {
    let mut p = cover_cache_dir();
    p.push(format!("ytcover_{}.img", name_hash(video_id)));
    std::fs::write(&p, bytes).ok()?;
    Some(p.to_string_lossy().into_owned())
}

/// Local cache path of a YouTube thumbnail (key = image URL), **only if the
/// file is already present** – without network access (UI-thread safe).
pub fn youtube_thumb_path(url: &str) -> Option<String> {
    let url = normalize_image_url(url);
    if url.trim().is_empty() {
        return None;
    }
    let mut p = cover_cache_dir();
    p.push(format!("yt_{}.img", name_hash(&url)));
    p.exists().then(|| p.to_string_lossy().into_owned())
}

/// Gives a protocol-relative image URL (`//yt3.googleusercontent.com/…`, what
/// YouTube listings return for channel avatars) the scheme it is missing —
/// without one the request fails outright and the entry keeps a placeholder.
/// Applied on both the cache lookup and the fetch, so the two agree on the key.
fn normalize_image_url(url: &str) -> String {
    let u = url.trim();
    match u.starts_with("//") {
        true => format!("https:{u}"),
        false => u.to_string(),
    }
}

/// Loads a YouTube thumbnail into the cache on demand and returns the local
/// path. **Network access** – only call from worker/background threads. Already
/// cached thumbnails are not loaded again.
pub fn cache_youtube_thumb(url: &str) -> Option<String> {
    if let Some(p) = youtube_thumb_path(url) {
        return Some(p);
    }
    let url = normalize_image_url(url);
    if url.is_empty() {
        return None;
    }
    let bytes = shared_client().get_image(&url).ok().flatten()?;
    let mut p = cover_cache_dir();
    p.push(format!("yt_{}.img", name_hash(&url)));
    std::fs::write(&p, &bytes).ok()?;
    Some(p.to_string_lossy().into_owned())
}

/// Downloads a YouTube thumbnail/avatar again (detail refresh), replacing the
/// cached copy. Keeps the cached one when the download fails. **Network.**
pub fn recache_youtube_thumb(url: &str) -> Option<String> {
    let url = normalize_image_url(url);
    if url.is_empty() {
        return None;
    }
    if let Some(bytes) = shared_client().get_image(&url).ok().flatten() {
        let mut p = cover_cache_dir();
        p.push(format!("yt_{}.img", name_hash(&url)));
        if std::fs::write(&p, &bytes).is_ok() {
            return Some(p.to_string_lossy().into_owned());
        }
    }
    youtube_thumb_path(&url)
}

/// Picture for a subscribed channel: its own avatar when the listing gave one,
/// otherwise the artist photo a music DB has for the channel name — a channel
/// without an avatar would show a bare placeholder in the subscriptions list.
/// Returns the URL to store on the channel row (`None` if nothing was found).
pub fn channel_image_url(avatar: Option<&str>, channel_name: &str) -> Option<String> {
    if let Some(u) = avatar.map(normalize_image_url).filter(|u| !u.is_empty()) {
        return Some(u);
    }
    let name = channel_name.trim();
    if name.is_empty() {
        return None;
    }
    shared_client().artist_image_url(name).ok().flatten()
}

/// Caches a batch of YouTube thumbnails **in parallel** ([`THUMB_FETCH_THREADS`]
/// network threads). Already cached URLs cost nothing, so a refresh that brought
/// in two new videos only fetches those two. **Network – worker threads only.**
pub fn cache_youtube_thumbs(urls: &[String]) {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    let missing: VecDeque<String> = urls
        .iter()
        .filter(|u| !u.trim().is_empty() && youtube_thumb_path(u).is_none())
        .cloned()
        .collect();
    if missing.is_empty() {
        return;
    }
    let n_threads = missing.len().min(THUMB_FETCH_THREADS);
    let jobs = Mutex::new(missing);
    std::thread::scope(|s| {
        for _ in 0..n_threads {
            s.spawn(|| {
                loop {
                    let Some(url) = jobs.lock().unwrap_or_else(|e| e.into_inner()).pop_front()
                    else {
                        break;
                    };
                    cache_youtube_thumb(&url);
                }
            });
        }
    });
}

/// Cache file of a recording cover (key = artist+title).
pub(super) fn recording_cover_file(artist: &str, title: &str) -> PathBuf {
    let mut p = cover_cache_dir();
    p.push(format!(
        "rec_{}.img",
        name_hash(&format!("{artist}\u{1}{title}"))
    ));
    p
}

/// Stores a cover for (artist, title) where [`recording_cover_path`] finds it —
/// used when a detail refresh fetched a fresh cover for a recognized song or a
/// recording.
pub fn store_recording_cover(artist: &str, title: &str, bytes: &[u8]) -> Option<String> {
    let p = recording_cover_file(artist, title);
    std::fs::write(&p, bytes).ok()?;
    Some(p.to_string_lossy().into_owned())
}

/// Local cache path of a recording cover, **only if already present** – without
/// network access (for display in the UI thread).
pub fn recording_cover_path(artist: &str, title: &str) -> Option<String> {
    if title.trim().is_empty() {
        return None;
    }
    let p = recording_cover_file(artist, title);
    p.exists().then(|| p.to_string_lossy().into_owned())
}

/// Cache path for a locally extracted album cover (key: artist+album).
fn save_local_cover(artist: &str, album: &str, bytes: &[u8]) -> Result<PathBuf> {
    let mut path = cover_cache_dir();
    path.push(format!(
        "local_{}.img",
        name_hash(&format!("{artist}\u{1}{album}"))
    ));
    std::fs::write(&path, bytes)?;
    Ok(path)
}

/// Stores an artist photo in the cache and returns the path.
pub(super) fn save_artist_image(name: &str, bytes: &[u8]) -> Result<PathBuf> {
    let mut path = artist_cache_dir();
    path.push(format!("{}.img", name_hash(name)));
    std::fs::write(&path, bytes)?;
    Ok(path)
}

/// Stores a gallery image in the cover cache and returns the path.
pub(super) fn save_gallery_image(
    prefix: &str,
    key: &str,
    idx: usize,
    bytes: &[u8],
) -> Result<PathBuf> {
    let mut path = cover_cache_dir();
    path.push(format!("{prefix}_{}_{idx}.img", name_hash(key)));
    std::fs::write(&path, bytes)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gtk::gdk_pixbuf::{Colorspace, Pixbuf};

    fn encoded(w: i32, h: i32, alpha: bool, format: &str) -> Vec<u8> {
        let pb = Pixbuf::new(Colorspace::Rgb, alpha, 8, w, h).unwrap();
        pb.fill(0x80c0ffff);
        pb.save_to_bufferv(format, &[]).unwrap()
    }

    fn decoded(bytes: &[u8]) -> Pixbuf {
        use gtk::prelude::*;
        let loader = gtk::gdk_pixbuf::PixbufLoader::new();
        loader.write(bytes).unwrap();
        loader.close().unwrap();
        loader.pixbuf().unwrap()
    }

    #[test]
    fn shrink_image_caps_the_longer_edge_and_keeps_the_aspect() {
        let big = encoded(2 * MAX_IMAGE_EDGE, MAX_IMAGE_EDGE, false, "png");
        let out = shrink_image(big.clone());
        let pb = decoded(&out);
        assert_eq!(
            (pb.width(), pb.height()),
            (MAX_IMAGE_EDGE, MAX_IMAGE_EDGE / 2)
        );
        // Opaque → JPEG.
        assert!(out.starts_with(&[0xFF, 0xD8]), "expected JPEG");
    }

    #[test]
    fn shrink_image_keeps_transparency_as_png() {
        let out = shrink_image(encoded(MAX_IMAGE_EDGE + 400, 300, true, "png"));
        assert!(out.starts_with(b"\x89PNG"), "expected PNG");
        let pb = decoded(&out);
        assert!(pb.has_alpha());
        assert_eq!(pb.width(), MAX_IMAGE_EDGE);
    }

    #[test]
    fn shrink_image_leaves_small_and_undecodable_input_untouched() {
        let small = encoded(800, 600, false, "jpeg");
        assert_eq!(shrink_image(small.clone()), small);
        let exact = encoded(MAX_IMAGE_EDGE, 200, false, "png");
        assert_eq!(shrink_image(exact.clone()), exact);
        let junk = b"not an image at all".to_vec();
        assert_eq!(shrink_image(junk.clone()), junk);
    }

    #[test]
    fn episode_extension_keeps_a_plausible_suffix() {
        assert_eq!(episode_extension("https://cdn.example/ep/42.MP3"), "mp3");
        assert_eq!(
            episode_extension("https://cdn.example/a.m4a?token=x.y"),
            "m4a"
        );
        assert_eq!(episode_extension("https://cdn.example/a.opus#t=10"), "opus");
        // Too long, non-alphanumeric or missing → generic name.
        assert_eq!(episode_extension("https://cdn.example/a.mpeg3"), "audio");
        assert_eq!(episode_extension("https://cdn.example/ep/42"), "audio");
        assert_eq!(episode_extension("https://cdn.example/a.mp-3"), "audio");
        assert_eq!(episode_extension(""), "audio");
    }

    /// Documents current behaviour: a URL without a path takes the host's TLD
    /// as its "extension". Harmless (the suffix is cosmetic), but not intended.
    #[test]
    fn episode_extension_of_a_bare_host_is_its_tld() {
        assert_eq!(episode_extension("https://example.com"), "com");
    }

    #[test]
    fn cover_file_key_keeps_real_mbids_and_hashes_anything_else() {
        let mbid = "b1392450-e666-3926-a536-22c65f834433";
        assert_eq!(cover_file_key(mbid), mbid);
        for bad in ["../../etc/passwd", "", "abc/def", "xyz"] {
            let key = cover_file_key(bad);
            assert_eq!(key, name_hash(bad));
            assert!(!key.contains('/') && !key.contains('.'));
        }
    }

    #[test]
    fn normalize_image_url_adds_the_missing_scheme() {
        assert_eq!(
            normalize_image_url("//yt3.googleusercontent.com/a=s88"),
            "https://yt3.googleusercontent.com/a=s88"
        );
        assert_eq!(
            normalize_image_url("  https://i.ytimg.com/vi/x/hq.jpg "),
            "https://i.ytimg.com/vi/x/hq.jpg"
        );
        assert_eq!(normalize_image_url(" //host/p "), "https://host/p");
        assert_eq!(normalize_image_url("   "), "");
    }

    #[test]
    fn channel_image_url_prefers_the_avatar_without_a_lookup() {
        // An avatar (or a nameless channel) never reaches the network.
        assert_eq!(
            channel_image_url(Some("//yt3.example/a"), "Some Channel"),
            Some("https://yt3.example/a".to_string())
        );
        assert_eq!(channel_image_url(None, "   "), None);
        assert_eq!(channel_image_url(Some("  "), ""), None);
    }

    #[test]
    fn local_station_logos_are_file_urls() {
        assert!(is_local_station_logo("file:///home/u/logo.img"));
        assert!(!is_local_station_logo("https://radio.example/logo.png"));
        assert!(!is_local_station_logo(""));
    }

    #[test]
    fn lookups_of_empty_keys_answer_none() {
        assert_eq!(podcast_image_path("  "), None);
        assert_eq!(station_image_path(""), None);
        assert_eq!(youtube_thumb_path(" "), None);
        assert_eq!(recording_cover_path("Artist", "  "), None);
        assert!(!image_file_lost(""));
        assert!(!image_file_lost("   "));
    }

    #[test]
    fn image_size_reads_dimensions_and_rejects_junk() {
        assert_eq!(image_size(&encoded(40, 30, false, "png")), Some((40, 30)));
        assert_eq!(image_size(b"<html>404</html>"), None);
    }
}
