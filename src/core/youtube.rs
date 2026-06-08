//! YouTube integration via the `yt-dlp` zipapp.
//!
//! The Flatpak ships a **pinned, checksum-verified** `yt-dlp` in `/app/bin` (see
//! the manifest), so the feature works out of the box once the user enables
//! YouTube — no unverified runtime download. Because YouTube frequently breaks
//! older `yt-dlp` versions, a newer copy can be fetched on demand into the app
//! data dir ([`download_ytdlp`]); that copy then **takes precedence** over the
//! bundled baseline. Outside the Flatpak a `yt-dlp` on `PATH` is used (or the
//! on-demand download). The managed zipapp is run via `python3` (provided by the
//! GNOME runtime); a `PATH` binary runs via its own shebang.
//!
//! Nothing here ever reads or writes the user's audio files. Streaming hands a
//! direct `https` audio URL (resolved via `yt-dlp -g -f bestaudio`) to
//! `playbin3`; offline copies land under the data dir. The module shells out to
//! the binary exactly like [`crate::core::fingerprint`] (fpcalc) and
//! [`crate::core::output`] (pactl). All operations are **blocking** – call them
//! only from worker/background threads.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Result};
use serde::Deserialize;

/// Official "latest" zipapp asset (a self-contained Python program; needs the
/// runtime's `python3`, which the GNOME Platform provides).
const YTDLP_URL: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp";

/// `$XDG_DATA_HOME/emilia/bin` – where the managed `yt-dlp` zipapp lives.
pub fn ytdlp_dir() -> PathBuf {
    let mut dir = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
    dir.push("emilia");
    dir.push("bin");
    dir
}

/// Path of the managed `yt-dlp` zipapp.
pub fn ytdlp_path() -> PathBuf {
    ytdlp_dir().join("yt-dlp")
}

/// `$XDG_DATA_HOME/emilia/youtube` – offline audio downloads (under the data
/// dir, next to the library DB, so the OS won't purge them like `~/.cache`).
pub fn yt_download_dir() -> PathBuf {
    let mut dir = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
    dir.push("emilia");
    dir.push("youtube");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// A `Command` invoking `yt-dlp`. Prefers the user-updated copy in the data dir
/// (written by [`download_ytdlp`]); when that is absent it falls back to a
/// `yt-dlp` on `PATH` — in the Flatpak the bundled `/app/bin/yt-dlp`, natively a
/// system install. The managed zipapp is run via `python3`; the `PATH` variant
/// is self-executable via its shebang. Callers that parse output add
/// `--ignore-config`/`--no-warnings`.
fn ytdlp() -> Command {
    let managed = ytdlp_path();
    if managed.exists() {
        let mut c = Command::new("python3");
        c.arg(managed);
        c
    } else {
        Command::new("yt-dlp")
    }
}

/// Whether a usable `yt-dlp` is present — the user-updated copy, or one on
/// `PATH` (e.g. the bundled `/app/bin/yt-dlp` in the Flatpak).
pub fn available() -> bool {
    version().is_some()
}

/// The installed `yt-dlp` version string (e.g. `2026.03.17`), or `None` if it is
/// not installed/runnable. Spawns the binary – cheap, but prefer a worker thread.
pub fn version() -> Option<String> {
    let out = ytdlp().arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!v.is_empty()).then_some(v)
}

/// Downloads (or replaces) the latest `yt-dlp` zipapp. Writes to a temporary
/// `*.part` file, makes it executable and renames on success, then verifies it
/// runs. Returns the installed version. **Network – worker threads only.**
pub fn download_ytdlp() -> Result<String> {
    let dir = ytdlp_dir();
    std::fs::create_dir_all(&dir)?;
    let dest = ytdlp_path();
    let tmp = dest.with_extension("part");

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .build();
    let written = {
        let mut reader = agent
            .get(YTDLP_URL)
            .call()?
            .into_reader()
            .take(64 * 1024 * 1024); // generous cap; the zipapp is only a few MB
        let mut file = std::fs::File::create(&tmp)?;
        let n = std::io::copy(&mut reader, &mut file)?;
        file.sync_all()?;
        n
    };
    if written == 0 {
        let _ = std::fs::remove_file(&tmp);
        return Err(anyhow!("downloaded yt-dlp is empty"));
    }
    std::fs::rename(&tmp, &dest)?;
    set_executable(&dest)?;

    version().ok_or_else(|| anyhow!("yt-dlp was downloaded but does not run"))
}

/// Re-downloads the latest `yt-dlp` (YouTube changes frequently break older
/// versions). Identical to [`download_ytdlp`]; kept separate for a clear call
/// site / intent at the UI layer.
pub fn update_ytdlp() -> Result<String> {
    download_ytdlp()
}

/// Age of the **managed** (app-downloaded) `yt-dlp` copy since it was last
/// written, or `None` when there is no managed copy. A system/Flatpak `yt-dlp`
/// on `PATH` is intentionally not reported here — it is not ours to update, so
/// the auto-updater leaves it alone.
pub fn managed_age() -> Option<Duration> {
    let modified = std::fs::metadata(ytdlp_path()).ok()?.modified().ok()?;
    SystemTime::now().duration_since(modified).ok()
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}
#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}

/// What a search/listing hit represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum YtKind {
    Video,
    Playlist,
    Channel,
}

/// A single search or listing result: enough to display it and act on it
/// (subscribe a channel, open a playlist, play/add a video).
#[derive(Debug, Clone)]
pub struct YtResult {
    pub kind: YtKind,
    /// Video id | playlist id | channel id (or handle) – stable identifier.
    pub id: String,
    /// Canonical watch/playlist/channel URL (what yt-dlp should be handed).
    pub url: String,
    pub title: String,
    /// Uploader/channel name (for videos and playlists).
    pub uploader: Option<String>,
    /// Duration in seconds (videos only).
    pub duration: Option<i64>,
    pub thumbnail: Option<String>,
}

/// Canonical watch URL for a video id.
pub fn watch_url(video_id: &str) -> String {
    format!("https://www.youtube.com/watch?v={video_id}")
}

/// Extracts the 11-character video id from a pasted YouTube **video** URL
/// (`watch?v=`, `youtu.be/<id>`, `shorts/<id>`, `embed/<id>`, `live/<id>`),
/// tolerating `www.`/`m.`/`music.` hosts and extra query parameters. Returns
/// `None` for non-URL search terms and for playlist/channel URLs – so the search
/// box can resolve a direct link to that exact video instead of running it as a
/// free-text query.
pub fn video_id_from_url(s: &str) -> Option<String> {
    let s = s.trim();
    let after = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))?;
    let (host, rest) = after.split_once('/').unwrap_or((after, ""));
    let host = host.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);

    // youtu.be/<id>[?…]
    if host == "youtu.be" {
        return clean_video_id(rest);
    }
    if !(host == "youtube.com" || host == "m.youtube.com" || host == "music.youtube.com") {
        return None;
    }
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    // Path-based players: /shorts/<id>, /embed/<id>, /live/<id>, /v/<id>.
    for prefix in ["shorts/", "embed/", "live/", "v/"] {
        if let Some(id) = path.strip_prefix(prefix) {
            return clean_video_id(id);
        }
    }
    // /watch?v=<id>
    if path == "watch" {
        return query
            .split('&')
            .find_map(|kv| kv.strip_prefix("v="))
            .and_then(clean_video_id);
    }
    None
}

/// Takes the leading run of valid YouTube-id characters and accepts it only when
/// it yields the canonical 11-character id (so trailing path/query bits are
/// ignored, and a too-short fragment is rejected).
fn clean_video_id(s: &str) -> Option<String> {
    let id: String = s
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .take(11)
        .collect();
    (id.len() == 11).then_some(id)
}

/// Normalises a channel/uploader name into an artist name: YouTube Music
/// auto-channels are `"<Artist> - Topic"` and official channels often end in
/// `"VEVO"` – strip both so the external-DB (Deezer/MusicBrainz) query matches.
pub fn clean_channel_name(name: &str) -> String {
    let mut s = name.trim();
    if let Some(stripped) = s.strip_suffix(" - Topic") {
        s = stripped.trim();
    }
    if let Some(stripped) = s.strip_suffix("VEVO") {
        s = stripped.trim_end();
    }
    s.to_string()
}

/// Drops a trailing tag group like "(Official Video)", "[Lyrics]" or
/// "(Official Audio)" from a video title. Only groups that clearly look like a
/// production tag (by keyword) are removed, so real bracketed names survive.
fn strip_title_noise(raw: &str) -> String {
    const NOISE: &[&str] = &[
        "official",
        "video",
        "audio",
        "lyric",
        "visualizer",
        "remaster",
        "explicit",
    ];
    let mut s = raw.trim().to_string();
    loop {
        let trimmed = s.trim_end();
        let (open, close) = match trimmed.chars().last() {
            Some(')') => ('(', ')'),
            Some(']') => ('[', ']'),
            _ => break,
        };
        let Some(open_idx) = trimmed.rfind(open) else {
            break;
        };
        // `open`/`close` are ASCII (1 byte), so these byte slices are valid.
        let inner = trimmed[open_idx + 1..trimmed.len() - close.len_utf8()].to_lowercase();
        if NOISE.iter().any(|k| inner.contains(k)) {
            s = trimmed[..open_idx].trim_end().to_string();
        } else {
            break;
        }
    }
    s
}

/// Splits a YouTube video title into `(artist, album, title)` for display in the
/// detail view. Music titles are usually `"Artist - Title"`, sometimes
/// `"Artist - Album - Title"`; Topic/auto uploads often carry just the song
/// name, so the channel (cleaned of "- Topic"/"VEVO") is the artist fallback.
/// En/em dashes are treated like the `" - "` separator; trailing tag groups
/// (e.g. "(Official Video)") are dropped first.
pub fn split_title(raw: &str, channel: Option<&str>) -> (Option<String>, Option<String>, String) {
    let cleaned = strip_title_noise(raw);
    // YouTube uses a spaced hyphen / en dash / em dash between artist and title;
    // splitting on the *spaced* form avoids breaking names like "Twenty-One".
    let normalized = cleaned.replace(" – ", " - ").replace(" — ", " - ");
    let parts: Vec<&str> = normalized
        .split(" - ")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let chan_artist = channel
        .map(clean_channel_name)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    match parts.as_slice() {
        [] => (chan_artist, None, cleaned),
        [only] => (chan_artist, None, (*only).to_string()),
        [artist, title] => (Some((*artist).to_string()), None, (*title).to_string()),
        [artist, album, rest @ ..] => (
            Some((*artist).to_string()),
            Some((*album).to_string()),
            rest.join(" - "),
        ),
    }
}

/// Deterministic thumbnail URL for a video id. Unlike the per-resolution URLs
/// from the listing (whose `maxresdefault` variant 404s for many videos),
/// `hqdefault.jpg` always exists – so caching it reliably succeeds.
pub fn thumbnail_url(video_id: &str) -> String {
    format!("https://i.ytimg.com/vi/{video_id}/hqdefault.jpg")
}

/// Synthetic library path of a YouTube track: `yt:<video_id>`. The video id is
/// globally unique, so – unlike the WebDAV `nc:<source_id>:<rel>` scheme – no
/// source id is needed. Resolved to a stream/file in `start_track_playback`.
pub fn yt_path(video_id: &str) -> String {
    format!("yt:{video_id}")
}

/// Splits a synthetic path `yt:<video_id>` into the video id.
pub fn parse_yt_path(path: &str) -> Option<String> {
    let id = path.strip_prefix("yt:")?.trim();
    (!id.is_empty()).then(|| id.to_string())
}

/// Searches YouTube. For videos this uses the native `ytsearchN:` prefix; for
/// channels/playlists a YouTube search-results URL with the corresponding type
/// filter is handed to yt-dlp. Best effort – returns an empty list rather than
/// erroring on a search that yields nothing. **Network – worker threads only.**
pub fn search(query: &str, kind: YtKind, n: usize) -> Result<Vec<YtResult>> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(Vec::new());
    }
    // A pasted video link resolves to that exact video instead of being run as a
    // free-text search – regardless of the selected kind, since a watch link is
    // unambiguously a single video.
    if let Some(id) = video_id_from_url(query) {
        return Ok(vec![video_meta(&id)?]);
    }
    let n = n.clamp(1, 50);
    let source = match kind {
        YtKind::Video => format!("ytsearch{n}:{query}"),
        // YouTube search-results URL with the `sp` type filter (channels /
        // playlists). yt-dlp's search-URL extractor returns flat entries.
        YtKind::Channel => search_results_url(query, "EgIQAg%3D%3D"),
        YtKind::Playlist => search_results_url(query, "EgIQAw%3D%3D"),
    };
    let entries =
        dump_entries(&["--flat-playlist", "--playlist-end", &n.to_string(), "--", &source])?;
    Ok(entries
        .into_iter()
        .filter_map(|e| e.into_result())
        // Keep only the kind that was searched for (the channel/playlist filter
        // pages can still surface the odd video).
        .filter(|r| r.kind == kind)
        .take(n)
        .collect())
}

/// Lists the entries (videos) of a channel or playlist URL. For a channel the
/// uploads tab is targeted (`…/videos`). **Network – worker threads only.**
pub fn list_entries(url: &str, limit: usize) -> Result<Vec<YtResult>> {
    let limit = limit.clamp(1, 200);
    let target = channel_videos_url(url);
    let entries = dump_entries(&[
        "--flat-playlist",
        "--playlist-end",
        &limit.to_string(),
        "--",
        &target,
    ])?;
    Ok(entries
        .into_iter()
        .filter_map(|e| e.into_result())
        .filter(|r| r.kind == YtKind::Video)
        .take(limit)
        .collect())
}

/// Resolves a direct, playable `https` audio stream URL for a video (best audio
/// only – no ffmpeg muxing needed). The URL is short-lived (it expires), so it
/// is resolved fresh on every play and never cached. **Network – worker only.**
pub fn resolve_audio_url(video_id_or_url: &str) -> Result<String> {
    let url = to_url(video_id_or_url);
    let out = ytdlp()
        .args([
            "--ignore-config",
            "--no-warnings",
            "--no-playlist",
            "-f",
            "bestaudio/best",
            "-g",
        ])
        .arg("--")
        .arg(&url)
        .output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        note_extraction(false, &err);
        return Err(anyhow!("yt-dlp -g failed: {}", err.trim()));
    }
    note_extraction(true, "");
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("https://"))
        .map(str::to_string)
        .ok_or_else(|| anyhow!("yt-dlp returned no stream URL"))
}

/// Downloads the best audio of a video into [`yt_download_dir`] under the name
/// `<video_id>.<ext>` (yt-dlp picks the real extension) and returns the produced
/// file path. **Network – worker threads only.**
pub fn download_audio(video_id: &str) -> Result<PathBuf> {
    let dir = yt_download_dir();
    // Clear any stale fragment/previous copy for this id so the glob is unambiguous.
    if let Some(old) = find_download(video_id) {
        let _ = std::fs::remove_file(old);
    }
    let template = dir.join(format!("{video_id}.%(ext)s"));
    let status = ytdlp()
        .args([
            "--ignore-config",
            "--no-warnings",
            "--no-playlist",
            "-f",
            "bestaudio/best",
            "-o",
        ])
        .arg(&template)
        .arg("--")
        .arg(watch_url(video_id))
        .status()?;
    if !status.success() {
        return Err(anyhow!("yt-dlp download failed"));
    }
    find_download(video_id).ok_or_else(|| anyhow!("download produced no file"))
}

/// Locates an already downloaded offline audio file for a video id (any
/// extension), ignoring incomplete `*.part` files. Filesystem only – cheap.
pub fn find_download(video_id: &str) -> Option<PathBuf> {
    let dir = yt_download_dir();
    let prefix = format!("{video_id}.");
    std::fs::read_dir(&dir).ok()?.flatten().find_map(|e| {
        let p = e.path();
        let name = p.file_name()?.to_string_lossy().into_owned();
        (name.starts_with(&prefix) && !name.ends_with(".part")).then_some(p)
    })
}

/// Transcodes a downloaded audio file to a tagged MP3 at `dest` via ffmpeg
/// (present in the GNOME runtime). Embeds title/artist/album so the library
/// scanner reads proper metadata. **Blocking – worker threads only.**
pub fn transcode_to_mp3(
    source: &Path,
    dest: &Path,
    title: &str,
    artist: Option<&str>,
    album: Option<&str>,
) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-y")
        .arg("-i")
        .arg(source)
        .arg("-vn")
        .args(["-c:a", "libmp3lame", "-q:a", "2", "-id3v2_version", "3"])
        .args(["-metadata", &format!("title={title}")]);
    if let Some(a) = artist.filter(|s| !s.trim().is_empty()) {
        cmd.args(["-metadata", &format!("artist={a}")]);
    }
    if let Some(al) = album.filter(|s| !s.trim().is_empty()) {
        cmd.args(["-metadata", &format!("album={al}")]);
    }
    cmd.arg(dest);
    let status = cmd.status()?;
    if !status.success() {
        return Err(anyhow!("ffmpeg transcode failed"));
    }
    Ok(())
}

/// Sanitizes a string into a safe single-path-component filename.
pub fn sanitize_filename(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if "/\\:*?\"<>|\0\n\r\t".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.').trim();
    let base = if trimmed.is_empty() {
        "untitled"
    } else {
        trimmed
    };
    base.chars().take(120).collect()
}

/// The `UC…` channel id contained in a `/channel/UC…` URL, if present.
pub fn channel_id_from_url(url: &str) -> Option<String> {
    let rest = url.split("/channel/").nth(1)?;
    let id: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    id.starts_with("UC").then_some(id)
}

/// Fetches the channel's Atom feed (`feeds/videos.xml`) and returns a map
/// `video_id → published` (ISO-8601). The feed carries publication timestamps
/// that `--flat-playlist` omits; it covers roughly the newest 15 videos. Best
/// effort – empty on any error or for non-`UC…` ids. **Network – worker only.**
pub fn channel_rss_published(channel_id: &str) -> std::collections::HashMap<String, String> {
    if !channel_id.starts_with("UC") {
        return std::collections::HashMap::new();
    }
    let url = format!("https://www.youtube.com/feeds/videos.xml?channel_id={channel_id}");
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(8))
        .build();
    let body = match agent.get(&url).call() {
        Ok(resp) => {
            let mut s = String::new();
            if resp
                .into_reader()
                .take(4 * 1024 * 1024)
                .read_to_string(&mut s)
                .is_err()
            {
                return std::collections::HashMap::new();
            }
            s
        }
        Err(_) => return std::collections::HashMap::new(),
    };
    parse_atom_published(&body)
}

/// Parses a YouTube channel Atom feed into `video_id → published` (ISO-8601).
fn parse_atom_published(body: &str) -> std::collections::HashMap<String, String> {
    use quick_xml::events::Event;
    let mut map = std::collections::HashMap::new();
    let mut reader = quick_xml::Reader::from_str(body);
    let mut in_entry = false;
    let mut field: Option<&'static str> = None;
    let (mut cur_id, mut cur_pub): (Option<String>, Option<String>) = (None, None);
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => match local_atom_name(e.name().as_ref()).as_str() {
                "entry" => {
                    in_entry = true;
                    cur_id = None;
                    cur_pub = None;
                }
                "videoId" if in_entry => field = Some("id"),
                "published" if in_entry => field = Some("published"),
                _ => {}
            },
            Ok(Event::Text(t)) if field.is_some() => {
                let val = t.unescape().unwrap_or_default().trim().to_string();
                match field {
                    Some("id") => cur_id = Some(val),
                    Some("published") => cur_pub = Some(val),
                    _ => {}
                }
            }
            Ok(Event::End(e)) => match local_atom_name(e.name().as_ref()).as_str() {
                "entry" => {
                    if let (Some(id), Some(p)) = (cur_id.take(), cur_pub.take()) {
                        map.insert(id, p);
                    }
                    in_entry = false;
                }
                "videoId" | "published" => field = None,
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_title_artist_and_title() {
        let (artist, album, title) = split_title("Daft Punk - Get Lucky", None);
        assert_eq!(artist.as_deref(), Some("Daft Punk"));
        assert_eq!(album, None);
        assert_eq!(title, "Get Lucky");
    }

    #[test]
    fn split_title_with_album() {
        let (artist, album, title) = split_title("Pink Floyd - The Wall - Hey You", None);
        assert_eq!(artist.as_deref(), Some("Pink Floyd"));
        assert_eq!(album.as_deref(), Some("The Wall"));
        assert_eq!(title, "Hey You");
    }

    #[test]
    fn split_title_strips_noise_and_uses_channel() {
        // Topic upload: title is just the song; artist comes from the channel.
        let (artist, album, title) =
            split_title("Get Lucky (Official Video)", Some("Daft Punk - Topic"));
        assert_eq!(artist.as_deref(), Some("Daft Punk"));
        assert_eq!(album, None);
        assert_eq!(title, "Get Lucky");
    }

    #[test]
    fn split_title_keeps_hyphenated_word() {
        // A spaced separator splits; an in-word hyphen must not.
        let (artist, _, title) = split_title("Twenty-One Pilots - Stressed Out", None);
        assert_eq!(artist.as_deref(), Some("Twenty-One Pilots"));
        assert_eq!(title, "Stressed Out");
    }

    #[test]
    fn video_id_from_url_handles_common_link_forms() {
        let id = "dQw4w9WgXcQ";
        assert_eq!(
            video_id_from_url("https://www.youtube.com/watch?v=dQw4w9WgXcQ").as_deref(),
            Some(id)
        );
        // Extra query params and a different host.
        assert_eq!(
            video_id_from_url("https://m.youtube.com/watch?v=dQw4w9WgXcQ&list=PL123&t=42s")
                .as_deref(),
            Some(id)
        );
        // Short link, shorts and youtu.be with trailing query.
        assert_eq!(
            video_id_from_url("https://youtu.be/dQw4w9WgXcQ?si=abc").as_deref(),
            Some(id)
        );
        assert_eq!(
            video_id_from_url("https://www.youtube.com/shorts/dQw4w9WgXcQ").as_deref(),
            Some(id)
        );
        // Not a video link → no id (plain search term, channel/playlist URL).
        assert_eq!(video_id_from_url("daft punk get lucky"), None);
        assert_eq!(
            video_id_from_url("https://www.youtube.com/@SomeChannel"),
            None
        );
        assert_eq!(
            video_id_from_url("https://www.youtube.com/playlist?list=PL123"),
            None
        );
    }

    #[test]
    fn parse_atom_extracts_video_dates() {
        // Trimmed YouTube channel feed: a channel-level <published> (must be
        // ignored) plus two <entry> elements with yt:videoId + published.
        let xml = r#"<?xml version="1.0"?>
        <feed xmlns:yt="http://www.youtube.com/xml/schemas/2015">
          <published>2015-03-18T15:36:55+00:00</published>
          <entry>
            <id>yt:video:AAA</id>
            <yt:videoId>AAA</yt:videoId>
            <title>First</title>
            <published>2026-06-04T06:49:58+00:00</published>
          </entry>
          <entry>
            <id>yt:video:BBB</id>
            <yt:videoId>BBB</yt:videoId>
            <title>Second</title>
            <published>2026-06-03T22:46:30+00:00</published>
          </entry>
        </feed>"#;
        let map = parse_atom_published(xml);
        assert_eq!(map.len(), 2);
        assert_eq!(
            map.get("AAA").map(String::as_str),
            Some("2026-06-04T06:49:58+00:00")
        );
        assert_eq!(
            map.get("BBB").map(String::as_str),
            Some("2026-06-03T22:46:30+00:00")
        );
        // The channel-level <published> must not leak in as an entry.
        assert!(!map.values().any(|v| v == "2015-03-18T15:36:55+00:00"));
    }

    #[test]
    fn channel_id_extracted_from_url() {
        assert_eq!(
            channel_id_from_url("https://www.youtube.com/channel/UCabc123/videos").as_deref(),
            Some("UCabc123")
        );
        assert_eq!(channel_id_from_url("https://www.youtube.com/@handle"), None);
    }
}

/// Local element name without namespace prefix (`yt:videoId` → `videoId`).
fn local_atom_name(qname: &[u8]) -> String {
    let s = String::from_utf8_lossy(qname);
    match s.rsplit_once(':') {
        Some((_, local)) => local.to_string(),
        None => s.into_owned(),
    }
}

/// Full metadata of a single video (title, uploader, duration, thumbnail) for
/// indexing it into the library with proper artist/title. **Network.**
pub fn video_meta(video_id_or_url: &str) -> Result<YtResult> {
    let url = to_url(video_id_or_url);
    // No `--flat-playlist`: a full single-video dump carries uploader/duration.
    let entries = dump_entries(&["--no-playlist", "--", &url])?;
    entries
        .into_iter()
        .next()
        .and_then(|e| e.into_result())
        .ok_or_else(|| anyhow!("no metadata for {url}"))
}

/// Lists a playlist's videos in playlist order (used by "add playlist to
/// collection"). Unlike [`list_entries`] the URL is taken as-is. **Network.**
pub fn list_playlist(url: &str, limit: usize) -> Result<Vec<YtResult>> {
    let limit = limit.clamp(1, 500);
    let entries =
        dump_entries(&["--flat-playlist", "--playlist-end", &limit.to_string(), "--", url])?;
    Ok(entries
        .into_iter()
        .filter_map(|e| e.into_result())
        .filter(|r| r.kind == YtKind::Video)
        .collect())
}

// ---------------------------------------------------------------------------
// internals
// ---------------------------------------------------------------------------

/// A YouTube watch URL for a bare id; otherwise the string is already a URL.
fn to_url(video_id_or_url: &str) -> String {
    let s = video_id_or_url.trim();
    if s.starts_with("http://") || s.starts_with("https://") {
        s.to_string()
    } else {
        watch_url(s)
    }
}

/// YouTube search-results page URL with a type `sp` filter (channel/playlist).
fn search_results_url(query: &str, sp: &str) -> String {
    format!(
        "https://www.youtube.com/results?search_query={}&sp={sp}",
        crate::core::online::percent_encode(query),
    )
}

/// Targets a channel's uploads tab. Playlist URLs (carrying `list=`) and URLs
/// that already point at a tab are left untouched.
fn channel_videos_url(url: &str) -> String {
    let u = url.trim_end_matches('/');
    if u.contains("list=") || u.contains("watch?") {
        return url.to_string();
    }
    if u.ends_with("/videos") || u.ends_with("/streams") || u.ends_with("/shorts") {
        return u.to_string();
    }
    format!("{u}/videos")
}

/// Runs `yt-dlp --dump-json <extra args>` and parses one JSON object per line.
/// Lines that fail to parse are skipped (yt-dlp may interleave non-JSON notes).
/// Process-wide flag: the last extraction attempt failed in a way that looks
/// like YouTube changed and the installed yt-dlp can no longer parse it (the
/// recurring "cat and mouse" breakage), as opposed to a plain network error.
/// Set from the worker threads, read by the UI to show a banner.
static EXTRACTION_BROKEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether YouTube extraction currently looks broken (see [`EXTRACTION_BROKEN`]).
pub fn extraction_broken() -> bool {
    EXTRACTION_BROKEN.load(std::sync::atomic::Ordering::Relaxed)
}

/// Heuristic: does this yt-dlp stderr indicate YouTube changed and yt-dlp needs
/// updating (vs. a transient network/availability error)? These are the typical
/// messages when the extractor breaks.
fn is_extraction_failure(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    [
        "unable to extract",
        "nsig extraction failed",
        "failed to extract any player response",
        "unable to download api page",
        "sign in to confirm",
        "precondition check failed",
        "requested format is not available",
        "http error 403",
    ]
    .iter()
    .any(|p| s.contains(p))
}

/// Records the outcome of an extraction attempt: a success clears the broken
/// flag, an extractor-style failure sets it. Transient errors leave it as is.
fn note_extraction(success: bool, stderr: &str) {
    use std::sync::atomic::Ordering::Relaxed;
    if success {
        EXTRACTION_BROKEN.store(false, Relaxed);
    } else if is_extraction_failure(stderr) {
        EXTRACTION_BROKEN.store(true, Relaxed);
    }
}

fn dump_entries(args: &[&str]) -> Result<Vec<RawEntry>> {
    if !available() {
        return Err(anyhow!("yt-dlp is not installed"));
    }
    let out = ytdlp()
        .args([
            "--ignore-config",
            "--no-warnings",
            "--ignore-errors",
            "--dump-json",
        ])
        .args(args)
        .output()?;
    // yt-dlp exits non-zero on partial failures (`--ignore-errors`); as long as
    // we got some parseable lines we use them.
    let stdout = String::from_utf8_lossy(&out.stdout);
    let entries: Vec<RawEntry> = stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<RawEntry>(l).ok())
        .collect();
    // Any parseable entry means extraction still works; otherwise inspect stderr.
    note_extraction(!entries.is_empty(), &String::from_utf8_lossy(&out.stderr));
    if entries.is_empty() && !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(anyhow!("yt-dlp failed: {}", err.trim()));
    }
    Ok(entries)
}

/// Raw flat-playlist JSON entry (only the fields we need; everything optional).
#[derive(Deserialize)]
struct RawEntry {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    webpage_url: Option<String>,
    #[serde(default)]
    ie_key: Option<String>,
    #[serde(default, rename = "_type")]
    type_: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    uploader: Option<String>,
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    duration: Option<f64>,
    #[serde(default)]
    thumbnail: Option<String>,
    #[serde(default)]
    thumbnails: Vec<RawThumb>,
}

#[derive(Deserialize)]
struct RawThumb {
    #[serde(default)]
    url: Option<String>,
}

impl RawEntry {
    fn into_result(self) -> Option<YtResult> {
        let id = self.id.clone().filter(|s| !s.trim().is_empty())?;
        let url = self
            .webpage_url
            .clone()
            .or_else(|| self.url.clone())
            .filter(|s| !s.trim().is_empty());
        let kind = self.classify(url.as_deref());
        let title = self
            .title
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| id.clone());
        let canonical = match kind {
            YtKind::Video => watch_url(&id),
            YtKind::Playlist => url
                .clone()
                .filter(|u| u.contains("list="))
                .unwrap_or_else(|| format!("https://www.youtube.com/playlist?list={id}")),
            YtKind::Channel => url
                .clone()
                .unwrap_or_else(|| format!("https://www.youtube.com/channel/{id}")),
        };
        // For videos use the deterministic `hqdefault` thumbnail (always exists);
        // for channels/playlists take the largest from the listing.
        let thumbnail = match kind {
            YtKind::Video => Some(thumbnail_url(&id)),
            _ => self
                .thumbnails
                .iter()
                .rev()
                .find_map(|t| t.url.clone())
                .or(self.thumbnail.clone())
                .filter(|s| !s.trim().is_empty()),
        };
        Some(YtResult {
            kind,
            id,
            url: canonical,
            title,
            uploader: self
                .uploader
                .or(self.channel)
                .filter(|s| !s.trim().is_empty()),
            duration: self.duration.map(|d| d as i64),
            thumbnail,
        })
    }

    /// Classifies a flat entry as video/playlist/channel from its extractor key
    /// and URL shape.
    fn classify(&self, url: Option<&str>) -> YtKind {
        let u = url.unwrap_or("");
        let ie = self.ie_key.as_deref().unwrap_or("");
        if u.contains("list=")
            || ie.eq_ignore_ascii_case("youtubeplaylist")
            || self.type_.as_deref() == Some("playlist")
        {
            return YtKind::Playlist;
        }
        if u.contains("/channel/")
            || u.contains("/@")
            || u.contains("/user/")
            || u.contains("/c/")
            || self.type_.as_deref() == Some("channel")
        {
            return YtKind::Channel;
        }
        if ie.eq_ignore_ascii_case("youtubetab") {
            // A tab without a playlist marker is a channel.
            return YtKind::Channel;
        }
        YtKind::Video
    }
}
