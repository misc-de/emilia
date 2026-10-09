//! The HTTP client for the online sources ([`OnlineClient`]) plus the JSON
//! shapes of their answers (MusicBrainz, Cover Art Archive, fanart.tv, Deezer,
//! LRCLIB, AcoustID).

use std::io::Read;
use std::time::Duration;

use anyhow::Result;
use serde::Deserialize;

use super::{
    AcoustIdMatch, CanonicalTrack, IMAGE_RETRY_MAX, ReleaseMatch, TrackTags, USER_AGENT,
    escape_lucene, percent_encode, shrink_image,
};
use crate::core::fingerprint;
use crate::core::net;

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

    /// GET with defensive retry + backoff (transient transport errors, `5xx`,
    /// and rate limits honouring `Retry-After`); `404` yields `Ok(None)`. See
    /// [`crate::core::net::get_with_retry`]. Always sets our User-Agent.
    pub(super) fn call_get(&self, url: &str) -> Result<Option<ureq::Response>> {
        // Log only the part before '?' – the query string can carry an API key
        // (fanart `api_key`, AcoustID `client_key`).
        let safe_url = url.split('?').next().unwrap_or(url);
        crate::core::net::get_with_retry(&self.agent, url, Some(USER_AGENT), safe_url)
    }

    /// [`Self::call_get`] with the short image budget ([`IMAGE_RETRY_MAX`]).
    fn call_get_image(&self, url: &str) -> Result<Option<ureq::Response>> {
        let safe_url = url.split('?').next().unwrap_or(url);
        crate::core::net::get_with_retry_max(
            &self.agent,
            url,
            Some(USER_AGENT),
            safe_url,
            IMAGE_RETRY_MAX,
        )
    }

    /// Finds the best-matching MusicBrainz release for (artist, album).
    /// Returns `Ok(None)` if nothing sufficiently matching was found — including
    /// right away, without a request, when either side is only a placeholder
    /// (`release:"YouTube"`, `artist:"no artist"`): such a search cannot succeed,
    /// so it is not worth a request against a rate-limited service.
    pub fn match_release(&self, artist: &str, album: &str) -> Result<Option<ReleaseMatch>> {
        if crate::core::placeholder::is_placeholder(artist)
            || crate::core::placeholder::is_placeholder(album)
        {
            return Ok(None);
        }
        let query = format!(
            "artist:\"{}\" AND release:\"{}\"",
            escape_lucene(artist),
            escape_lucene(album)
        );
        let url = format!(
            "https://musicbrainz.org/ws/2/release?query={}&fmt=json&limit=25",
            percent_encode(&query)
        );

        let search: MbSearch = match self.call_get(&url)? {
            Some(resp) => net::json_capped(resp, net::MAX_JSON_BYTES)?,
            None => return Ok(None),
        };

        // MusicBrainz sorts by score; we take the best match, but require a
        // minimum quality to avoid mismatches.
        let Some(best) = search
            .releases
            .iter()
            .max_by_key(|r| r.score)
            .filter(|r| r.score >= 70)
        else {
            return Ok(None);
        };
        Ok(Some(ReleaseMatch {
            mbid: best.id.clone(),
            release_group: best.release_group.as_ref().map(|g| g.id.clone()),
            year: original_year(best, &search.releases),
        }))
    }

    /// Fetches the canonical tracklist of a MusicBrainz release
    /// (`inc=recordings`): one entry per track with its disc, position, title and
    /// length. `Ok(vec![])` when the release has no usable media (also for a 404).
    pub fn fetch_release_tracks(&self, mbid: &str) -> Result<Vec<CanonicalTrack>> {
        let url = format!(
            "https://musicbrainz.org/ws/2/release/{}?inc=recordings&fmt=json",
            percent_encode(mbid)
        );
        let rel: MbReleaseFull = match self.call_get(&url)? {
            Some(resp) => net::json_capped(resp, net::MAX_JSON_BYTES)?,
            None => return Ok(Vec::new()),
        };
        let mut out = Vec::new();
        for (idx, medium) in rel.media.iter().enumerate() {
            let disc = medium.position.unwrap_or((idx as u32) + 1).max(1);
            for t in &medium.tracks {
                let title = t.title.trim();
                let position = t.position.unwrap_or(0);
                if title.is_empty() || position == 0 {
                    continue;
                }
                out.push(CanonicalTrack {
                    disc,
                    position,
                    title: title.to_string(),
                    length_ms: t.length,
                });
            }
        }
        Ok(out)
    }

    /// Loads the front cover (max. 500 px) for a release. Tries the concrete
    /// release first, then falls back to the release group.
    /// `Ok(None)` = no cover exists.
    pub fn fetch_cover(&self, m: &ReleaseMatch) -> Result<Option<Vec<u8>>> {
        let release_url = format!("https://coverartarchive.org/release/{}/front-500", m.mbid);
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
        match self.call_get_image(url)? {
            Some(resp) => {
                let mut buf = Vec::new();
                // Cap against accidentally huge responses (10 MB).
                resp.into_reader()
                    .take(10 * 1024 * 1024)
                    .read_to_end(&mut buf)?;
                Ok(Some(shrink_image(buf)))
            }
            // No cover stored (404) – not an error.
            None => Ok(None),
        }
    }

    /// Searches for an artist photo on Deezer (no API key needed). Returns the
    /// raw image bytes – deliberately in a **small** resolution (for 48 px avatars
    /// `picture_medium` ~250 px is enough; saves a lot of bandwidth/time).
    pub fn fetch_artist_image(&self, name: &str) -> Result<Option<Vec<u8>>> {
        if crate::core::placeholder::is_placeholder(name) {
            return Ok(None);
        }
        let Some(artist) = self.find_deezer_artist(name, false)? else {
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

    /// The Deezer artist meant by `name`. Several artists share a name
    /// ("Queen" has tiny namesakes ranked above the band), so among the
    /// results named exactly like that the one with the most fans wins. With
    /// `exact_only` unset, a search without an exact match falls back to its
    /// top result (good enough for a photo, e.g. of a YouTube channel name).
    pub(super) fn find_deezer_artist(
        &self,
        name: &str,
        exact_only: bool,
    ) -> Result<Option<DzArtist>> {
        let url = format!(
            "https://api.deezer.com/search/artist?q={}&limit=10",
            percent_encode(name)
        );
        let search: DzSearch = match self.call_get(&url)? {
            Some(resp) => net::json_capped(resp, net::MAX_JSON_BYTES)?,
            None => return Ok(None),
        };
        Ok(pick_artist(search.data, name, exact_only))
    }

    /// URL of the artist photo a music DB (Deezer) has for `name`, without
    /// downloading it. Used where the URL itself is stored (a channel row's
    /// thumbnail), so the usual thumbnail cache handles the rest.
    pub fn artist_image_url(&self, name: &str) -> Result<Option<String>> {
        let Some(artist) = self.find_deezer_artist(name, false)? else {
            return Ok(None);
        };
        // Big enough for a channel avatar in a list or the gallery.
        Ok([
            artist.picture_big,
            artist.picture_xl,
            artist.picture_medium,
            artist.picture,
        ]
        .into_iter()
        .flatten()
        .find(|u| !u.is_empty()))
    }

    /// Looks up lyrics for a track on [LRCLIB](https://lrclib.net) – a free,
    /// key-less service returning plain and synchronized (`.lrc`) text.
    ///
    /// Tries the exact `/api/get` first (matches on artist+title+duration, so it
    /// is precise) and falls back to the fuzzy `/api/search`, preferring a hit
    /// that carries synchronized lyrics. Returns `Ok(None)` when nothing usable
    /// is found (incl. instrumentals).
    pub fn fetch_lyrics(
        &self,
        artist: &str,
        title: &str,
        album: Option<&str>,
        duration_secs: Option<u64>,
    ) -> Result<Option<crate::core::lyrics::Lyrics>> {
        use crate::core::lyrics::Lyrics;
        let artist = artist.trim();
        let title = title.trim();
        if artist.is_empty() || title.is_empty() {
            return Ok(None);
        }

        // Exact lookup: needs the duration (matched within a small tolerance).
        if let Some(dur) = duration_secs.filter(|d| *d > 0) {
            let mut url = format!(
                "https://lrclib.net/api/get?artist_name={}&track_name={}&duration={}",
                percent_encode(artist),
                percent_encode(title),
                dur
            );
            if let Some(al) = album.map(str::trim).filter(|s| !s.is_empty()) {
                url.push_str(&format!("&album_name={}", percent_encode(al)));
            }
            if let Some(resp) = self.call_get(&url)? {
                let r: LrcLibItem = net::json_capped(resp, net::MAX_JSON_BYTES)?;
                let lyr = Lyrics::from_parts(r.plain_lyrics, r.synced_lyrics);
                if lyr.has_any() {
                    return Ok(Some(lyr));
                }
                // Known instrumental → no point searching further.
                if r.instrumental {
                    return Ok(None);
                }
            }
        }

        // Fuzzy fallback: take the best hit, preferring synchronized lyrics.
        let url = format!(
            "https://lrclib.net/api/search?artist_name={}&track_name={}",
            percent_encode(artist),
            percent_encode(title)
        );
        let hits: Vec<LrcLibItem> = match self.call_get(&url)? {
            Some(resp) => net::json_capped(resp, net::MAX_JSON_BYTES).unwrap_or_default(),
            None => return Ok(None),
        };
        let has_synced = |h: &&LrcLibItem| {
            h.synced_lyrics
                .as_deref()
                .is_some_and(|s| !s.trim().is_empty())
        };
        let has_plain = |h: &&LrcLibItem| {
            h.plain_lyrics
                .as_deref()
                .is_some_and(|s| !s.trim().is_empty())
        };
        let best = hits
            .iter()
            .find(has_synced)
            .or_else(|| hits.iter().find(has_plain));
        match best {
            Some(h) => {
                let lyr = Lyrics::from_parts(h.plain_lyrics.clone(), h.synced_lyrics.clone());
                Ok(lyr.has_any().then_some(lyr))
            }
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
            if let Some(hit) = self
                .search_track_tags(artist, title)?
                .and_then(TrackTags::into_cover)
            {
                return Ok(Some(hit));
            }
            return Ok(self
                .search_track_tags(cleaned, title)?
                .and_then(TrackTags::into_cover));
        }
        Ok(self
            .search_track_tags(artist, title)?
            .and_then(TrackTags::into_cover))
    }

    /// One Deezer track search for (artist, title) → the database's own artist,
    /// title, album and cover. An empty `artist` searches by title alone.
    pub(super) fn search_track_tags(&self, artist: &str, title: &str) -> Result<Option<TrackTags>> {
        let q = if artist.trim().is_empty() {
            format!("track:\"{}\"", title.replace('"', " "))
        } else {
            format!(
                "artist:\"{}\" track:\"{}\"",
                artist.replace('"', " "),
                title.replace('"', " ")
            )
        };
        self.search_track_query(&q, title)
    }

    /// The top Deezer track hit for a raw search query (structured
    /// `artist:"…" track:"…"` or plain free text); `title` stands in when the
    /// hit carries none.
    pub(super) fn search_track_query(&self, q: &str, title: &str) -> Result<Option<TrackTags>> {
        let url = format!(
            "https://api.deezer.com/search/track?q={}&limit=1",
            percent_encode(q)
        );
        let search: DzTrackSearch = match self.call_get(&url)? {
            Some(resp) => net::json_capped(resp, net::MAX_JSON_BYTES)?,
            None => return Ok(None),
        };
        let Some(hit) = search.data.into_iter().next() else {
            return Ok(None);
        };
        let non_empty = |s: String| Some(s).filter(|s| !s.trim().is_empty());
        let album = hit.album;
        let cover_url = album
            .as_ref()
            .and_then(|a| {
                [&a.cover_big, &a.cover_medium, &a.cover]
                    .into_iter()
                    .flatten()
                    .find(|u| !u.is_empty())
            })
            .cloned();
        // The cover is a second request and may well fail — the tags are still
        // worth returning without it.
        let cover = match cover_url {
            Some(u) => self.get_image(&u)?,
            None => None,
        };
        Ok(Some(TrackTags {
            artist: hit.artist.and_then(|a| a.name).and_then(non_empty),
            title: hit
                .title
                .and_then(non_empty)
                .unwrap_or_else(|| title.to_string()),
            album: album.and_then(|a| a.title).and_then(non_empty),
            cover,
        }))
    }

    /// Loads several images of an album from the Cover Art Archive (front, back,
    /// booklet, …). Returns each as (bytes, kind). Empty list if there is nothing.
    pub fn fetch_album_gallery(&self, mbid: &str) -> Result<Vec<(Vec<u8>, String)>> {
        let url = format!("https://coverartarchive.org/release/{mbid}");
        let list: CaaList = match self.call_get(&url)? {
            Some(resp) => net::json_capped(resp, net::MAX_JSON_BYTES)?,
            None => return Ok(Vec::new()),
        };
        let mut out = Vec::new();
        for img in list.images.into_iter().take(MAX_GALLERY) {
            // Prefer the 500 px variant; otherwise large; otherwise the original.
            let u = img
                .thumbnails
                .n500
                .or(img.thumbnails.large)
                .unwrap_or(img.image);
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
            Some(resp) => net::json_capped(resp, net::MAX_JSON_BYTES)?,
            None => return Ok(None),
        };
        Ok(search
            .artists
            .into_iter()
            .find(|a| a.score >= 90)
            .map(|a| a.id))
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
            Some(resp) => net::json_capped(resp, net::MAX_JSON_BYTES)?,
            None => return Ok(Vec::new()),
        };
        let mut out = Vec::new();
        for t in fa.artistthumb.into_iter().chain(fa.artistbackground) {
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

    /// Lists fanart.tv artist-image **URLs** (thumbs then backgrounds) *without*
    /// downloading them — for offering image candidates to pick from, where the
    /// full byte fetch of [`fetch_artist_gallery`] would be wasteful. Capped at
    /// `limit` (and the gallery ceiling). Needs the (free) personal API key.
    pub fn artist_gallery_urls(
        &self,
        api_key: &str,
        mbid: &str,
        limit: usize,
    ) -> Result<Vec<String>> {
        let url = format!("https://webservice.fanart.tv/v3/music/{mbid}?api_key={api_key}");
        let fa: FanartArtist = match self.call_get(&url)? {
            Some(resp) => net::json_capped(resp, net::MAX_JSON_BYTES)?,
            None => return Ok(Vec::new()),
        };
        Ok(fa
            .artistthumb
            .into_iter()
            .chain(fa.artistbackground)
            .map(|t| t.url)
            .filter(|u| !u.is_empty())
            .take(limit.min(MAX_GALLERY))
            .collect())
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
            Some(resp) => net::json_capped(resp, net::MAX_JSON_BYTES)?,
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
    /// Original release date of the whole group (e.g. `1996-02-13`). Preferred
    /// over a specific release's `date`, which may be a reissue/remaster.
    #[serde(rename = "first-release-date", default)]
    first_release_date: Option<String>,
}

// ---- Full release with media/tracks (canonical tracklist) ----

#[derive(Deserialize)]
struct MbReleaseFull {
    #[serde(default)]
    media: Vec<MbMedium>,
}

#[derive(Deserialize)]
struct MbMedium {
    #[serde(default)]
    position: Option<u32>,
    #[serde(default)]
    tracks: Vec<MbTrack>,
}

#[derive(Deserialize)]
struct MbTrack {
    #[serde(default)]
    position: Option<u32>,
    #[serde(default)]
    title: String,
    /// Track length in milliseconds.
    #[serde(default)]
    length: Option<i64>,
}

// ---- Deezer JSON (artist photos) ----

/// See [`OnlineClient::find_deezer_artist`]: the exact-name hit with the most
/// fans, else (unless `exact_only`) the search's top result.
fn pick_artist(hits: Vec<DzArtist>, name: &str, exact_only: bool) -> Option<DzArtist> {
    let want = super::matching::normalize_name(name);
    let (exact, rest): (Vec<_>, Vec<_>) = hits
        .into_iter()
        .partition(|a| super::matching::normalize_name(&a.name) == want);
    match exact.into_iter().max_by_key(|a| a.nb_fan) {
        Some(a) => Some(a),
        None if exact_only => None,
        None => rest.into_iter().next(),
    }
}

#[derive(Deserialize)]
struct DzSearch {
    #[serde(default)]
    data: Vec<DzArtist>,
}

/// One LRCLIB result (shape is identical for `/api/get` and `/api/search`).
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct LrcLibItem {
    #[serde(default)]
    plain_lyrics: Option<String>,
    #[serde(default)]
    synced_lyrics: Option<String>,
    #[serde(default)]
    instrumental: bool,
}

#[derive(Deserialize)]
pub(super) struct DzArtist {
    #[serde(default)]
    pub(super) id: u64,
    #[serde(default)]
    pub(super) name: String,
    #[serde(default)]
    pub(super) nb_fan: u64,
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
    title: Option<String>,
    #[serde(default)]
    artist: Option<DzTrackArtist>,
    #[serde(default)]
    album: Option<DzAlbum>,
}

#[derive(Deserialize)]
struct DzTrackArtist {
    #[serde(default)]
    name: Option<String>,
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

/// The original year of a matched release. Prefers the release group's
/// `first-release-date` – but the search endpoint doesn't return that field,
/// and the best-scored hit is often a reissue/remaster (all editions score the
/// same), so otherwise the earliest date among the hits of the same release
/// group is taken.
fn original_year(best: &MbRelease, releases: &[MbRelease]) -> Option<i32> {
    let group = best.release_group.as_ref();
    if let Some(y) = group
        .and_then(|g| g.first_release_date.as_deref())
        .and_then(parse_year)
    {
        return Some(y);
    }
    let Some(gid) = group.map(|g| g.id.as_str()) else {
        return best.date.as_deref().and_then(parse_year);
    };
    releases
        .iter()
        .filter(|r| r.release_group.as_ref().is_some_and(|g| g.id == gid))
        .filter_map(|r| r.date.as_deref().and_then(parse_year))
        .min()
}

/// Reads the year from a MusicBrainz date (`2015`, `2015-11`, `2015-11-20`).
fn parse_year(date: &str) -> Option<i32> {
    date.get(0..4).and_then(|y| y.parse().ok())
}

#[cfg(test)]
mod tests {
    fn dz(name: &str, fans: u64) -> DzArtist {
        DzArtist {
            id: fans,
            name: name.to_string(),
            nb_fan: fans,
            picture: None,
            picture_medium: None,
            picture_big: None,
            picture_xl: None,
        }
    }

    #[test]
    fn pick_artist_prefers_the_exact_name_with_most_fans() {
        let hits = || {
            vec![
                dz("Queen", 168),
                dz("Queen", 402),
                dz("Queen(Ares)", 184),
                dz("Queen", 12_827_971),
            ]
        };
        assert_eq!(
            pick_artist(hits(), "queen", true).map(|a| a.nb_fan),
            Some(12_827_971)
        );
        assert_eq!(
            pick_artist(hits(), "Queen", false).map(|a| a.nb_fan),
            Some(12_827_971)
        );
    }

    #[test]
    fn pick_artist_falls_back_to_the_top_result_unless_exact_only() {
        let hits = || vec![dz("Alex Glasgow", 50), dz("Alpenglas Band", 9)];
        assert!(pick_artist(hits(), "Alpenglas", true).is_none());
        assert_eq!(
            pick_artist(hits(), "Alpenglas", false).map(|a| a.name),
            Some("Alex Glasgow".into())
        );
        assert!(pick_artist(Vec::new(), "Alpenglas", false).is_none());
    }

    use super::*;

    /// Shape of a real `/ws/2/release?query=` answer (Daft Punk – Discovery):
    /// all editions score 100, the release group carries no
    /// `first-release-date`, and the top hit is a reissue.
    #[test]
    fn original_year_is_the_earliest_edition_of_the_group() {
        let search: MbSearch = serde_json::from_str(
            r#"{"releases": [
                {"id": "a", "score": 100, "date": "2022",
                 "release-group": {"id": "rg1"}},
                {"id": "b", "score": 100, "date": "2001-03-12",
                 "release-group": {"id": "rg1"}},
                {"id": "c", "score": 100, "date": "2024-10-08",
                 "release-group": {"id": "rg1"}},
                {"id": "d", "score": 90, "date": "1990",
                 "release-group": {"id": "other"}}
            ]}"#,
        )
        .unwrap();
        let best = &search.releases[0];
        assert_eq!(original_year(best, &search.releases), Some(2001));
    }

    #[test]
    fn original_year_prefers_the_group_first_release_date() {
        let search: MbSearch = serde_json::from_str(
            r#"{"releases": [
                {"id": "a", "score": 100, "date": "2014",
                 "release-group": {"id": "rg1", "first-release-date": "2001-02-26"}}
            ]}"#,
        )
        .unwrap();
        assert_eq!(
            original_year(&search.releases[0], &search.releases),
            Some(2001)
        );
    }

    #[test]
    fn original_year_without_a_group_uses_the_release_date() {
        let search: MbSearch = serde_json::from_str(
            r#"{"releases": [
                {"id": "a", "score": 100, "date": "1999-05-01"},
                {"id": "b", "score": 90, "date": "1980"}
            ]}"#,
        )
        .unwrap();
        // Without a release group the other hits say nothing about the edition.
        assert_eq!(
            original_year(&search.releases[0], &search.releases),
            Some(1999)
        );
        let undated: MbSearch = serde_json::from_str(r#"{"releases": [{"id": "a"}]}"#).unwrap();
        assert_eq!(original_year(&undated.releases[0], &undated.releases), None);
    }

    #[test]
    fn original_year_ignores_an_unparsable_group_date() {
        let search: MbSearch = serde_json::from_str(
            r#"{"releases": [
                {"id": "a", "score": 100, "date": "2010",
                 "release-group": {"id": "rg1", "first-release-date": ""}},
                {"id": "b", "score": 100, "date": "2003",
                 "release-group": {"id": "rg1"}}
            ]}"#,
        )
        .unwrap();
        assert_eq!(
            original_year(&search.releases[0], &search.releases),
            Some(2003)
        );
    }

    #[test]
    fn parse_year_reads_the_leading_four_digits() {
        assert_eq!(parse_year("2015"), Some(2015));
        assert_eq!(parse_year("2015-11"), Some(2015));
        assert_eq!(parse_year("2015-11-20"), Some(2015));
        assert_eq!(parse_year(""), None);
        assert_eq!(parse_year("201"), None);
        assert_eq!(parse_year("abcd-01-01"), None);
    }

    #[test]
    fn caa_kind_prefers_front_then_back_then_the_first_type() {
        let types = vec!["Booklet".to_string(), "Medium".to_string()];
        assert_eq!(caa_kind(&types, true, true), "Front");
        assert_eq!(caa_kind(&types, false, true), "Back");
        assert_eq!(caa_kind(&types, false, false), "Booklet");
        assert_eq!(caa_kind(&[], false, false), "Image");
    }

    #[test]
    fn caa_list_reads_the_500px_thumbnail_key() {
        let list: CaaList = serde_json::from_str(
            r#"{"images": [
                {"image": "orig", "front": true,
                 "thumbnails": {"500": "t500", "large": "tl"}},
                {"image": "orig2"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(list.images[0].thumbnails.n500.as_deref(), Some("t500"));
        assert!(list.images[0].front);
        assert!(list.images[1].thumbnails.n500.is_none());
        assert!(list.images[1].types.is_empty());
    }

    #[test]
    fn lrclib_item_reads_camel_case_fields() {
        let item: LrcLibItem = serde_json::from_str(
            r#"{"plainLyrics": "la la", "syncedLyrics": "[00:01.00] la", "instrumental": false}"#,
        )
        .unwrap();
        assert_eq!(item.plain_lyrics.as_deref(), Some("la la"));
        assert_eq!(item.synced_lyrics.as_deref(), Some("[00:01.00] la"));
        let inst: LrcLibItem = serde_json::from_str(r#"{"instrumental": true}"#).unwrap();
        assert!(inst.instrumental && inst.plain_lyrics.is_none());
    }
}
