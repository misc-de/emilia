//! Online metadata from open/free sources:
//! - **MusicBrainz** – album matching (CC0)
//! - **Cover Art Archive** – album covers (CC0)
//! - **Deezer** – artist photos (no API key needed)
//! - **AcoustID** + **Chromaprint** – track detection via audio fingerprint
//!   (needs a free application key)
//!
//! Important: this module **never** reads audio files and certainly never writes
//! anything back into their tags. All data found ends up exclusively in the
//! database and in the XDG cache (`~/.cache/emilia`).

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use serde::Deserialize;

use crate::core::cover;
use crate::core::db::Library;
use crate::core::fingerprint;
use crate::model::{AlbumMeta, ArtistMeta, TrackMeta};

/// MusicBrainz requires a meaningful User-Agent with contact info.
const USER_AGENT: &str = "Emilia/0.1.0 ( https://cais.de )";

/// MusicBrainz policy: at most one request per second.
pub const RATE_LIMIT: Duration = Duration::from_millis(1100);
/// On a server-side rate limit (HTTP 429/503), retry this many times with a
/// pause before a real error is reported.
const RL_MAX_RETRIES: usize = 4;
/// First backoff pause on a rate limit (doubles per attempt, capped).
const RL_BASE_BACKOFF: Duration = Duration::from_millis(1500);
/// Upper bound of the backoff pause.
const RL_MAX_BACKOFF: Duration = Duration::from_secs(30);
/// Number of parallel fetches for artist photos (Deezer handles this well).
pub const ARTIST_FETCH_THREADS: usize = 8;

/// Directory for cached covers: `$XDG_CACHE_HOME/emilia/covers`.
pub fn cover_cache_dir() -> PathBuf {
    cache_subdir("covers")
}

/// Directory for artist photos: `$XDG_CACHE_HOME/emilia/artists`.
pub fn artist_cache_dir() -> PathBuf {
    cache_subdir("artists")
}

fn cache_subdir(name: &str) -> PathBuf {
    let mut dir = dirs::cache_dir().unwrap_or_else(|| PathBuf::from("."));
    dir.push("emilia");
    dir.push(name);
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Whether fingerprint detection is possible (Chromaprint/`fpcalc` present).
pub fn fingerprint_available() -> bool {
    fingerprint::available()
}

/// Stable file name from an arbitrary string (for cache files).
fn name_hash(s: &str) -> String {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Result of a MusicBrainz release search.
pub struct ReleaseMatch {
    pub mbid: String,
    pub release_group: Option<String>,
    pub year: Option<i32>,
}

/// HTTP client with a shared connection pool and timeouts.
/// Cloneable (the `ureq::Agent` shares the pool/configuration) – so it can be
/// passed to multiple fetch threads.
#[derive(Clone)]
pub struct OnlineClient {
    agent: ureq::Agent,
}

impl Default for OnlineClient {
    fn default() -> Self {
        Self::new()
    }
}

impl OnlineClient {
    pub fn new() -> Self {
        // Short timeouts: a sluggish/blocking request shouldn't hold up the whole
        // run, but fail quickly and be skipped.
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(5))
            .timeout_read(Duration::from_secs(8))
            .timeout_write(Duration::from_secs(8))
            .build();
        Self { agent }
    }

    /// GET that handles rate limits politely: on `429`/`503` it **pauses and
    /// retries** – for as long as specified by `Retry-After`, otherwise by backoff –
    /// instead of failing immediately. This way a temporary limit doesn't abort the
    /// run, nor does it consume a "failed attempt". `404` yields `Ok(None)` (no
    /// content). Other errors – and a persistent limit after all attempts – are
    /// passed through. Always sets our User-Agent (tolerated by all services).
    fn call_get(&self, url: &str) -> Result<Option<ureq::Response>> {
        let mut backoff = RL_BASE_BACKOFF;
        let mut attempt = 0usize;
        loop {
            match self.agent.get(url).set("User-Agent", USER_AGENT).call() {
                Ok(resp) => return Ok(Some(resp)),
                // No content stored (404) – not an error.
                Err(ureq::Error::Status(404, _)) => return Ok(None),
                // Rate limit: pause and – up to the limit – retry.
                Err(ureq::Error::Status(code, resp)) if code == 429 || code == 503 => {
                    attempt += 1;
                    if attempt > RL_MAX_RETRIES {
                        return Err(ureq::Error::Status(code, resp).into());
                    }
                    let wait = resp
                        .header("Retry-After")
                        .and_then(|s| s.trim().parse::<u64>().ok())
                        .map(Duration::from_secs)
                        .unwrap_or(backoff)
                        .min(RL_MAX_BACKOFF);
                    // Log only the part before '?' – the query string can carry
                    // an API key (fanart `api_key`, AcoustID `client_key`).
                    let safe_url = url.split('?').next().unwrap_or(url);
                    tracing::debug!("Rate-limited ({code}) on {safe_url}; pausing {wait:?}");
                    std::thread::sleep(wait);
                    backoff = (backoff * 2).min(RL_MAX_BACKOFF);
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Finds the best-matching MusicBrainz release for (artist, album).
    /// Returns `Ok(None)` if nothing sufficiently matching was found.
    pub fn match_release(&self, artist: &str, album: &str) -> Result<Option<ReleaseMatch>> {
        let query = format!(
            "artist:\"{}\" AND release:\"{}\"",
            escape_lucene(artist),
            escape_lucene(album)
        );
        let url = format!(
            "https://musicbrainz.org/ws/2/release?query={}&fmt=json&limit=5",
            percent_encode(&query)
        );

        let search: MbSearch = match self.call_get(&url)? {
            Some(resp) => resp.into_json()?,
            None => return Ok(None),
        };

        // MusicBrainz sorts by score; we take the best match, but require a
        // minimum quality to avoid mismatches.
        let best = search
            .releases
            .into_iter()
            .max_by_key(|r| r.score)
            .filter(|r| r.score >= 70);

        Ok(best.map(|r| ReleaseMatch {
            mbid: r.id,
            release_group: r.release_group.map(|g| g.id),
            year: r.date.as_deref().and_then(parse_year),
        }))
    }

    /// Loads the front cover (max. 500 px) for a release. Tries the concrete
    /// release first, then falls back to the release group.
    /// `Ok(None)` = no cover exists.
    pub fn fetch_cover(&self, m: &ReleaseMatch) -> Result<Option<Vec<u8>>> {
        let release_url = format!(
            "https://coverartarchive.org/release/{}/front-500",
            m.mbid
        );
        if let Some(bytes) = self.get_image(&release_url)? {
            return Ok(Some(bytes));
        }
        if let Some(rg) = &m.release_group {
            let rg_url = format!("https://coverartarchive.org/release-group/{rg}/front-500");
            if let Some(bytes) = self.get_image(&rg_url)? {
                return Ok(Some(bytes));
            }
        }
        Ok(None)
    }

    pub(crate) fn get_image(&self, url: &str) -> Result<Option<Vec<u8>>> {
        match self.call_get(url)? {
            Some(resp) => {
                let mut buf = Vec::new();
                // Cap against accidentally huge responses (10 MB).
                resp.into_reader()
                    .take(10 * 1024 * 1024)
                    .read_to_end(&mut buf)?;
                Ok(Some(buf))
            }
            // No cover stored (404) – not an error.
            None => Ok(None),
        }
    }

    /// Searches for an artist photo on Deezer (no API key needed). Returns the
    /// raw image bytes – deliberately in a **small** resolution (for 48 px avatars
    /// `picture_medium` ~250 px is enough; saves a lot of bandwidth/time).
    pub fn fetch_artist_image(&self, name: &str) -> Result<Option<Vec<u8>>> {
        let url = format!(
            "https://api.deezer.com/search/artist?q={}&limit=1",
            percent_encode(name)
        );
        let search: DzSearch = match self.call_get(&url)? {
            Some(resp) => resp.into_json()?,
            None => return Ok(None),
        };
        let Some(artist) = search.data.into_iter().next() else {
            return Ok(None);
        };
        // Smallest usable size first (fast), skip placeholders.
        let pic = [
            artist.picture_medium,
            artist.picture_big,
            artist.picture,
            artist.picture_xl,
        ]
        .into_iter()
        .flatten()
        .find(|u| !u.is_empty());
        match pic {
            Some(u) => self.get_image(&u),
            None => Ok(None),
        }
    }

    /// Searches for the album cover on Deezer (no API key needed) for (artist, title)
    /// and returns `(image bytes, album name)`. For subsequently tagging a
    /// recording with the cover of the single/album.
    pub fn fetch_track_cover(
        &self,
        artist: &str,
        title: &str,
    ) -> Result<Option<(Vec<u8>, Option<String>)>> {
        // Try the artist as-is, then a cleaned variant: strip "(…)"/"[…]"
        // suffixes (e.g. "AnnenMayKantereit (Live in Berlin)") that converters
        // dump into the artist tag and that make the search miss.
        let cleaned = artist.split(['(', '[']).next().unwrap_or(artist).trim();
        if !cleaned.is_empty() && cleaned != artist.trim() {
            if let Some(hit) = self.search_track_cover(artist, title)? {
                return Ok(Some(hit));
            }
            return self.search_track_cover(cleaned, title);
        }
        self.search_track_cover(artist, title)
    }

    /// One Deezer track search for (artist, title) → (cover bytes, album name).
    fn search_track_cover(
        &self,
        artist: &str,
        title: &str,
    ) -> Result<Option<(Vec<u8>, Option<String>)>> {
        let q = if artist.trim().is_empty() {
            format!("track:\"{}\"", title.replace('"', " "))
        } else {
            format!(
                "artist:\"{}\" track:\"{}\"",
                artist.replace('"', " "),
                title.replace('"', " ")
            )
        };
        let url = format!(
            "https://api.deezer.com/search/track?q={}&limit=1",
            percent_encode(&q)
        );
        let search: DzTrackSearch = match self.call_get(&url)? {
            Some(resp) => resp.into_json()?,
            None => return Ok(None),
        };
        let Some(album) = search.data.into_iter().next().and_then(|t| t.album) else {
            return Ok(None);
        };
        let cover = [album.cover_big, album.cover_medium, album.cover]
            .into_iter()
            .flatten()
            .find(|u| !u.is_empty());
        let Some(cover_url) = cover else {
            return Ok(None);
        };
        Ok(self.get_image(&cover_url)?.map(|b| (b, album.title)))
    }

    /// Loads several images of an album from the Cover Art Archive (front, back,
    /// booklet, …). Returns each as (bytes, kind). Empty list if there is nothing.
    pub fn fetch_album_gallery(&self, mbid: &str) -> Result<Vec<(Vec<u8>, String)>> {
        let url = format!("https://coverartarchive.org/release/{mbid}");
        let list: CaaList = match self.call_get(&url)? {
            Some(resp) => resp.into_json()?,
            None => return Ok(Vec::new()),
        };
        let mut out = Vec::new();
        for img in list.images.into_iter().take(MAX_GALLERY) {
            // Prefer the 500 px variant; otherwise large; otherwise the original.
            let u = img.thumbnails.n500.or(img.thumbnails.large).unwrap_or(img.image);
            if u.is_empty() {
                continue;
            }
            if let Some(bytes) = self.get_image(&u)? {
                out.push((bytes, caa_kind(&img.types, img.front, img.back)));
            }
        }
        Ok(out)
    }

    /// Finds the MusicBrainz artist ID (for fanart.tv). `None` if unclear.
    pub fn artist_mbid(&self, name: &str) -> Result<Option<String>> {
        let query = format!("artist:\"{}\"", escape_lucene(name));
        let url = format!(
            "https://musicbrainz.org/ws/2/artist?query={}&fmt=json&limit=1",
            percent_encode(&query)
        );
        let search: MbArtistSearch = match self.call_get(&url)? {
            Some(resp) => resp.into_json()?,
            None => return Ok(None),
        };
        Ok(search.artists.into_iter().find(|a| a.score >= 90).map(|a| a.id))
    }

    /// Loads several artist images from fanart.tv (thumbs + backgrounds).
    /// Needs a (free) personal API key. Empty list = nothing.
    pub fn fetch_artist_gallery(
        &self,
        api_key: &str,
        mbid: &str,
    ) -> Result<Vec<(Vec<u8>, String)>> {
        let url = format!("https://webservice.fanart.tv/v3/music/{mbid}?api_key={api_key}");
        let fa: FanartArtist = match self.call_get(&url)? {
            Some(resp) => resp.into_json()?,
            None => return Ok(Vec::new()),
        };
        let mut out = Vec::new();
        for t in fa.artistthumb.into_iter().chain(fa.artistbackground.into_iter()) {
            if out.len() >= MAX_GALLERY {
                break;
            }
            if t.url.is_empty() {
                continue;
            }
            if let Some(bytes) = self.get_image(&t.url)? {
                out.push((bytes, "Photo".to_string()));
            }
        }
        Ok(out)
    }

    /// Queries AcoustID with a Chromaprint fingerprint and returns the best
    /// match (recording incl. artist/album, where available).
    pub fn acoustid_lookup(
        &self,
        client_key: &str,
        fp: &fingerprint::Fingerprint,
    ) -> Result<Option<AcoustIdMatch>> {
        let url = format!(
            "https://api.acoustid.org/v2/lookup?client={}&meta=recordings+releasegroups&duration={}&fingerprint={}",
            percent_encode(client_key),
            fp.duration as u64,
            percent_encode(&fp.fingerprint),
        );
        let resp: AcoustIdResp = match self.call_get(&url)? {
            Some(resp) => resp.into_json()?,
            None => return Ok(None),
        };

        // Best result (highest score) with at least one recording.
        let best = resp
            .results
            .into_iter()
            .filter(|r| !r.recordings.is_empty())
            .max_by(|a, b| a.score.total_cmp(&b.score));

        let Some(result) = best else {
            return Ok(None);
        };
        let Some(rec) = result.recordings.into_iter().find(|r| r.title.is_some()) else {
            return Ok(None);
        };

        Ok(Some(AcoustIdMatch {
            recording_mbid: rec.id,
            title: rec.title,
            artist: rec.artists.into_iter().next().map(|a| a.name),
            album: rec.releasegroups.into_iter().find_map(|g| g.title),
        }))
    }
}

/// Match of an AcoustID fingerprint search.
pub struct AcoustIdMatch {
    pub recording_mbid: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
}

/// Stores the cover bytes in the cache and returns the path.
fn save_cover(mbid: &str, bytes: &[u8]) -> Result<PathBuf> {
    let mut path = cover_cache_dir();
    path.push(format!("{mbid}.img"));
    std::fs::write(&path, bytes)?;
    Ok(path)
}

/// Determines a **local** album cover entirely without the network: prefers the
/// image embedded in the sample track's tags, otherwise a folder image
/// (`cover.jpg`, `folder.png`, …). Returns the path to the displayable cover
/// file. The audio file is only read in the process.
pub fn local_album_cover(artist: &str, album: &str, sample_path: &str) -> Option<String> {
    let p = Path::new(sample_path);

    // 1) Embedded tag image → write to the cache.
    if let Some(bytes) = cover::embedded_cover(p) {
        if let Ok(path) = save_local_cover(artist, album, &bytes) {
            return Some(path.to_string_lossy().into_owned());
        }
    }
    // 2) Folder image → use its path directly (no copying needed).
    if let Some(dir) = p.parent() {
        if let Some(img) = cover::find_cover_file(dir) {
            return Some(img.to_string_lossy().into_owned());
        }
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
    let bytes = OnlineClient::new().get_image(url).ok().flatten()?;
    let mut p = cover_cache_dir();
    p.push(format!("podcast_{}.img", name_hash(url)));
    std::fs::write(&p, &bytes).ok()?;
    Some(p.to_string_lossy().into_owned())
}

/// Local cache path of a station logo (key = image URL), **only if the file is
/// already present** – without network access (for display in the UI thread).
pub fn station_image_path(url: &str) -> Option<String> {
    if url.trim().is_empty() {
        return None;
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
    if url.trim().is_empty() {
        return None;
    }
    let bytes = OnlineClient::new().get_image(url).ok().flatten()?;
    let mut p = cover_cache_dir();
    p.push(format!("station_{}.img", name_hash(url)));
    std::fs::write(&p, &bytes).ok()?;
    Some(p.to_string_lossy().into_owned())
}

/// Fetches the cover (and album name) for (artist, title) – **network access**,
/// only call from worker/background threads. Best effort: `None` if nothing is
/// found. For subsequently tagging a streaming recording.
pub fn recording_cover(artist: &str, title: &str) -> Option<(Vec<u8>, Option<String>)> {
    if title.trim().is_empty() {
        return None;
    }
    OnlineClient::new()
        .fetch_track_cover(artist, title)
        .ok()
        .flatten()
}

/// Cache path for a locally extracted album cover (key: artist+album).
fn save_local_cover(artist: &str, album: &str, bytes: &[u8]) -> Result<PathBuf> {
    let mut path = cover_cache_dir();
    path.push(format!("local_{}.img", name_hash(&format!("{artist}\u{1}{album}"))));
    std::fs::write(&path, bytes)?;
    Ok(path)
}

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
            tracing::warn!("MusicBrainz search failed ({artist} – {album}): {e}");
            meta.status = "error".to_string();
        }
    }

    if let Err(e) = lib.upsert_album_meta(&meta) {
        tracing::error!("Failed to save album_meta: {e}");
    }
    meta
}

/// Stores an artist photo in the cache and returns the path.
fn save_artist_image(name: &str, bytes: &[u8]) -> Result<PathBuf> {
    let mut path = artist_cache_dir();
    path.push(format!("{}.img", name_hash(name)));
    std::fs::write(&path, bytes)?;
    Ok(path)
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

/// Maximum number of images per gallery (album/artist).
const MAX_GALLERY: usize = 8;

#[derive(serde::Deserialize)]
struct CaaList {
    #[serde(default)]
    images: Vec<CaaImage>,
}
#[derive(serde::Deserialize)]
struct CaaImage {
    #[serde(default)]
    image: String,
    #[serde(default)]
    front: bool,
    #[serde(default)]
    back: bool,
    #[serde(default)]
    types: Vec<String>,
    #[serde(default)]
    thumbnails: CaaThumbs,
}
#[derive(serde::Deserialize, Default)]
struct CaaThumbs {
    #[serde(rename = "500")]
    n500: Option<String>,
    large: Option<String>,
}
#[derive(serde::Deserialize)]
struct MbArtistSearch {
    #[serde(default)]
    artists: Vec<MbArtist>,
}
#[derive(serde::Deserialize)]
struct MbArtist {
    id: String,
    #[serde(default)]
    score: u32,
}
#[derive(serde::Deserialize)]
struct FanartArtist {
    #[serde(default)]
    artistthumb: Vec<FanartImage>,
    #[serde(default)]
    artistbackground: Vec<FanartImage>,
}
#[derive(serde::Deserialize)]
struct FanartImage {
    #[serde(default)]
    url: String,
}

/// Determines the "kind" of a CAA image (front/back/…) for display.
fn caa_kind(types: &[String], front: bool, back: bool) -> String {
    if front {
        "Front".to_string()
    } else if back {
        "Back".to_string()
    } else if let Some(t) = types.first() {
        t.clone()
    } else {
        "Image".to_string()
    }
}

/// Stores a gallery image in the cover cache and returns the path.
fn save_gallery_image(prefix: &str, key: &str, idx: usize, bytes: &[u8]) -> Result<PathBuf> {
    let mut path = cover_cache_dir();
    path.push(format!("{prefix}_{}_{idx}.img", name_hash(key)));
    std::fs::write(&path, bytes)?;
    Ok(path)
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
    for (i, (bytes, kind)) in imgs.iter().enumerate() {
        match save_gallery_image("albimg", &key, i, bytes) {
            Ok(pp) => {
                stored.push((pp.to_string_lossy().into_owned(), kind.clone(), "caa".to_string()))
            }
            Err(e) => tracing::warn!("Failed to save gallery image: {e}"),
        }
    }
    if !stored.is_empty() {
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
            Ok(pp) => {
                stored.push((pp.to_string_lossy().into_owned(), kind.clone(), "fanart".to_string()))
            }
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
pub fn enrich_album_gallery(client: &OnlineClient, lib: &Library, artist: &str, album: &str) -> usize {
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

// ---- MusicBrainz JSON ----

#[derive(Deserialize)]
struct MbSearch {
    #[serde(default)]
    releases: Vec<MbRelease>,
}

#[derive(Deserialize)]
struct MbRelease {
    id: String,
    #[serde(default)]
    score: i32,
    #[serde(default)]
    date: Option<String>,
    #[serde(rename = "release-group", default)]
    release_group: Option<MbReleaseGroup>,
}

#[derive(Deserialize)]
struct MbReleaseGroup {
    id: String,
}

// ---- Deezer JSON (artist photos) ----

#[derive(Deserialize)]
struct DzSearch {
    #[serde(default)]
    data: Vec<DzArtist>,
}

#[derive(Deserialize)]
struct DzArtist {
    #[serde(default)]
    picture: Option<String>,
    #[serde(default)]
    picture_medium: Option<String>,
    #[serde(default)]
    picture_big: Option<String>,
    #[serde(default)]
    picture_xl: Option<String>,
}

#[derive(Deserialize)]
struct DzTrackSearch {
    #[serde(default)]
    data: Vec<DzTrack>,
}

#[derive(Deserialize)]
struct DzTrack {
    #[serde(default)]
    album: Option<DzAlbum>,
}

#[derive(Deserialize)]
struct DzAlbum {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    cover: Option<String>,
    #[serde(default)]
    cover_medium: Option<String>,
    #[serde(default)]
    cover_big: Option<String>,
}

// ---- AcoustID JSON (fingerprint detection) ----

#[derive(Deserialize)]
struct AcoustIdResp {
    #[serde(default)]
    results: Vec<AcoustIdResult>,
}

#[derive(Deserialize)]
struct AcoustIdResult {
    #[serde(default)]
    score: f64,
    #[serde(default)]
    recordings: Vec<AcoustIdRecording>,
}

#[derive(Deserialize)]
struct AcoustIdRecording {
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    artists: Vec<AcoustIdArtist>,
    #[serde(default)]
    releasegroups: Vec<AcoustIdReleaseGroup>,
}

#[derive(Deserialize)]
struct AcoustIdArtist {
    name: String,
}

#[derive(Deserialize)]
struct AcoustIdReleaseGroup {
    #[serde(default)]
    title: Option<String>,
}

// ---- Helper functions ----

/// Reads the year from a MusicBrainz date (`2015`, `2015-11`, `2015-11-20`).
fn parse_year(date: &str) -> Option<i32> {
    date.get(0..4).and_then(|y| y.parse().ok())
}

/// Escapes Lucene special characters in free text, so the query stays valid.
fn escape_lucene(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '"' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Minimal percent-encoding for query strings (RFC 3986 unreserved is kept).
pub(crate) fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
