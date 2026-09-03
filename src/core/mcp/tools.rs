//! The shared tool layer: the registry of MCP tools, the [`dispatch`] that runs
//! one, and the JSON-RPC method routing ([`handle_rpc`]).
//!
//! **Both** backends call exactly this code. Reads open a fresh [`Library`]
//! connection per request (WAL makes that safe alongside the running UI); writes
//! either touch the library directly (playlists/favorites) or are forwarded as a
//! backend-agnostic [`McpCommand`] through the UI-installed control sink.
//!
//! Library model structs are intentionally not `serde::Serialize` (they are pure
//! domain types); tool results are therefore assembled here with `json!`.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use super::command::McpCommand;
use super::protocol::{
    RpcRequest, RpcResponse, INVALID_REQUEST, JSONRPC_VERSION, MCP_PROTOCOL_VERSION,
    METHOD_NOT_FOUND,
};
use super::McpContext;
use crate::core::db::Library;
use crate::core::mcp::state::SyncSnapshot;
use crate::core::sync::share::Selection;
use crate::model::{StreamItem, Track};

// ---- small argument helpers --------------------------------------------------

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
}

fn req_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    arg_str(args, key).ok_or_else(|| anyhow!("missing required string argument '{key}'"))
}

fn arg_i64(args: &Value, key: &str) -> Option<i64> {
    args.get(key).and_then(|v| v.as_i64())
}

fn req_i64(args: &Value, key: &str) -> Result<i64> {
    arg_i64(args, key).ok_or_else(|| anyhow!("missing required integer argument '{key}'"))
}

fn arg_bool(args: &Value, key: &str) -> Option<bool> {
    args.get(key).and_then(|v| v.as_bool())
}

/// Gate for destructive tools: the caller must pass `"confirm": true`, so a
/// model cannot delete something by reflex without an explicit acknowledgement.
fn require_confirm(args: &Value) -> Result<()> {
    if args.get("confirm").and_then(|v| v.as_bool()) == Some(true) {
        Ok(())
    } else {
        Err(anyhow!(
            "destructive action: pass \"confirm\": true to proceed"
        ))
    }
}

/// Human-readable `H:MM:SS` (or `M:SS`) rendering of a millisecond duration,
/// emitted alongside the raw `*_ms` value by the analysis tools.
fn fmt_hms(ms: i64) -> String {
    let secs = ms.max(0) / 1000;
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

fn track_json(t: &Track) -> Value {
    json!({
        "path": t.path,
        "title": t.title,
        "artist": t.artist,
        "album": t.album,
        "track_no": t.track_no,
        "duration_ms": t.duration_ms,
        "year": t.year,
    })
}

/// A string array argument (missing/invalid → empty; blank entries dropped).
fn arg_str_list(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// An integer array argument (missing/invalid → empty).
fn arg_i64_list(args: &Value, key: &str) -> Vec<i64> {
    args.get(key)
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_i64()).collect())
        .unwrap_or_default()
}

fn station_json(s: &StreamItem) -> Value {
    json!({
        "id": s.id,
        "name": s.name,
        "url": s.url,
        "tags": s.tags,
        "country": s.country,
        "has_logo": s.favicon.is_some(),
    })
}

fn station_by_id(lib: &Library, id: i64) -> Result<StreamItem> {
    lib.streams()?
        .into_iter()
        .find(|s| s.id == id)
        .ok_or_else(|| anyhow!("no station with id {id} (see list_stations)"))
}

/// A library track that lives on local disk. Tracks of network sources
/// (`nc:` paths) are read-only over MCP, and a path the library does not know
/// is refused so a tool can never touch arbitrary files.
fn local_track(lib: &Library, path: &str) -> Result<Track> {
    if crate::core::webdav::parse_nc_path(path).is_some() {
        return Err(anyhow!("tracks on network sources are read-only over MCP"));
    }
    lib.track_by_path(path)?.ok_or_else(|| {
        anyhow!("no library track at '{path}' (paths come from list_tracks/search_library)")
    })
}

/// Moves a file to the desktop trash — recoverable, never an outright delete.
fn trash_file(path: &std::path::Path) -> Result<()> {
    use gtk::gio;
    use gtk::prelude::FileExt;
    if !path.is_file() {
        return Err(anyhow!("{} is not a file", path.display()));
    }
    gio::File::for_path(path)
        .trash(gio::Cancellable::NONE)
        .map_err(|e| anyhow!("could not move {} to the trash: {e}", path.display()))
}

fn sync_snapshot(ctx: &McpContext) -> SyncSnapshot {
    ctx.sync.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Polls the sync snapshot (every 100 ms, up to `timeout`) until `f` yields.
fn sync_wait<T>(
    ctx: &McpContext,
    timeout: std::time::Duration,
    f: impl Fn(&SyncSnapshot) -> Option<T>,
) -> Option<T> {
    let start = std::time::Instant::now();
    loop {
        if let Some(v) = f(&sync_snapshot(ctx)) {
            return Some(v);
        }
        if start.elapsed() >= timeout {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

fn sync_json(s: &SyncSnapshot) -> Value {
    let role = if s.connected || s.listening {
        Some(if s.is_server { "server" } else { "client" })
    } else {
        None
    };
    json!({
        "connected": s.connected,
        "peer_name": s.peer_name,
        "role": role,
        "listening": s.listening,
        "pair_url": s.pair_url,
        "address": s.address,
        "phase": if s.phase.is_empty() { "idle" } else { s.phase.as_str() },
        "incoming_offer": s.incoming_offer.as_ref().map(|o| json!({
            "from": o.from,
            "files": o.files,
            "new_files": o.new_files,
            "total_size": o.total_size,
            "youtube_items": o.yt,
            "stations": o.stations,
            "recordings": o.recordings,
            "memos": o.memos,
            "favorites": o.favorites,
            "playlists": o.playlists,
            "podcasts": o.podcasts,
            "categories": o.categories,
            "eq": o.eq,
        })),
        "progress": s.progress.as_ref().map(|(done, total, name)| json!({
            "done": done, "total": total, "file": name,
        })),
        "last_transfer_files": s.last_transfer_files,
        "last_error": s.last_error,
    })
}

/// Builds a share [`Selection`] from the `sync_share` arguments. Podcast ids are
/// resolved to feed URLs (opening the library only when there are any).
fn selection_from_args(args: &Value) -> Result<Selection> {
    let mut albums = Vec::new();
    if let Some(list) = args.get("albums").and_then(|v| v.as_array()) {
        for a in list {
            let artist = a
                .get("artist")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            let album = a.get("album").and_then(|v| v.as_str()).unwrap_or("").trim();
            if album.is_empty() {
                return Err(anyhow!(
                    "each entry of `albums` needs an `album` (and `artist`)"
                ));
            }
            albums.push((artist.to_string(), album.to_string()));
        }
    }
    let podcast_ids = arg_i64_list(args, "podcast_ids");
    let mut podcast_feeds = Vec::new();
    if !podcast_ids.is_empty() {
        let lib = Library::open()?;
        for id in podcast_ids {
            let url = lib
                .podcast_feed_url(id)?
                .ok_or_else(|| anyhow!("no podcast with id {id} (see list_podcasts)"))?;
            podcast_feeds.push(url);
        }
    }
    Ok(Selection {
        whole_library: arg_bool(args, "whole_library").unwrap_or(false),
        artists: arg_str_list(args, "artists"),
        albums,
        song_paths: arg_str_list(args, "tracks"),
        audiobooks: arg_bool(args, "audiobooks").unwrap_or(false),
        concerts: arg_bool(args, "concerts").unwrap_or(false),
        stations: arg_i64_list(args, "station_ids"),
        recordings: arg_i64_list(args, "recording_ids"),
        memos: arg_i64_list(args, "memo_ids"),
        podcast_feeds,
        playlist_ids: arg_i64_list(args, "playlist_ids"),
        include_metadata: arg_bool(args, "include_metadata").unwrap_or(true),
        yt_channels: Vec::new(),
        yt_playlists: Vec::new(),
        yt_songs: arg_str_list(args, "yt_videos"),
        include_favorites: arg_bool(args, "include_favorites").unwrap_or(false),
        include_playlists: arg_bool(args, "include_playlists").unwrap_or(false),
        include_podcasts: arg_bool(args, "include_podcasts").unwrap_or(false),
        include_eq: arg_bool(args, "include_eq").unwrap_or(false),
        include_categories: arg_bool(args, "include_categories").unwrap_or(false),
    })
}

// ---- JSON-RPC routing --------------------------------------------------------

/// Routes one parsed JSON-RPC request. Returns `None` for notifications (no
/// `id`) and other no-reply cases; otherwise the response to send back.
pub fn handle_rpc(ctx: &McpContext, req: RpcRequest) -> Option<RpcResponse> {
    let id = req.id.clone();
    // Tolerate a missing `jsonrpc` tag (some clients omit it) but reject a wrong
    // one. A notification (no id) with a bad tag is silently dropped.
    if !req.jsonrpc.is_empty() && req.jsonrpc != JSONRPC_VERSION {
        return id.map(|id| {
            RpcResponse::error(Some(id), INVALID_REQUEST, "unsupported jsonrpc version")
        });
    }
    match req.method.as_str() {
        "initialize" => Some(RpcResponse::ok(
            id,
            json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": {
                    "name": "emilia",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            }),
        )),
        // Lifecycle notification after a successful initialize: no response.
        "notifications/initialized" | "notifications/cancelled" => None,
        "ping" => Some(RpcResponse::ok(id, json!({}))),
        "tools/list" => Some(RpcResponse::ok(id, json!({ "tools": tool_list_enabled() }))),
        "tools/call" => {
            let name = req
                .params
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let args = req
                .params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            // MCP convention: tool *execution* errors are reported inside a
            // successful response with `isError: true`, not as a JSON-RPC error.
            let body = match dispatch(ctx, name, &args) {
                Ok(result) => json!({
                    "content": [{
                        "type": "text",
                        "text": serde_json::to_string_pretty(&result).unwrap_or_default(),
                    }],
                    "isError": false,
                }),
                Err(e) => json!({
                    "content": [{ "type": "text", "text": format!("error: {e}") }],
                    "isError": true,
                }),
            };
            Some(RpcResponse::ok(id, body))
        }
        other => {
            // Unknown notification → silently ignore; unknown request → error.
            if id.is_none() {
                None
            } else {
                Some(RpcResponse::error(
                    id,
                    METHOD_NOT_FOUND,
                    format!("method '{other}' not found"),
                ))
            }
        }
    }
}

// ---- YouTube gating ----------------------------------------------------------

/// The YouTube-backed tools. They are hidden from the advertised tool list and
/// refused by [`dispatch`] when the integration is disabled, so a disabled
/// YouTube feature is neither visible nor functional over MCP.
pub(crate) const YOUTUBE_TOOLS: [&str; 4] = [
    "list_youtube",
    "play_youtube",
    "search_youtube",
    "download_youtube",
];

/// Reads the `youtube_enabled` UI setting (default: off, matching the app). Opens
/// its own short-lived read connection — `tool_list`/`dispatch` are already
/// per-request, so this adds at most one cheap WAL read.
fn youtube_enabled() -> bool {
    matches!(
        Library::open()
            .ok()
            .and_then(|l| l.get_setting("youtube_enabled").ok().flatten())
            .as_deref(),
        Some("1")
    )
}

// ---- tool execution ----------------------------------------------------------

/// Runs a single tool and returns its structured result (the value that the
/// backend wraps into an MCP `content` block). Errors surface to the caller as a
/// tool execution error.
pub fn dispatch(ctx: &McpContext, name: &str, args: &Value) -> Result<Value> {
    // A disabled YouTube integration is inert over MCP: its tools are not even
    // advertised (see `tool_list_enabled`), and invoking one directly is refused.
    if YOUTUBE_TOOLS.contains(&name) && !youtube_enabled() {
        return Err(anyhow!(
            "the YouTube integration is disabled in the app settings"
        ));
    }
    match name {
        // --- reads -----------------------------------------------------------
        "now_playing" => {
            let np = ctx.now.lock().unwrap_or_else(|e| e.into_inner()).clone();
            Ok(json!({
                "playing": np.playing,
                "title": np.title,
                "artist": np.artist,
                "album": np.album,
                "position_ms": np.position_ms,
                "duration_ms": np.duration_ms,
            }))
        }

        "search_library" => {
            let query = req_str(args, "query")?;
            let limit = arg_i64(args, "limit").unwrap_or(20).clamp(1, 200) as usize;
            let lib = Library::open()?;
            let r = lib.search_library(query, limit)?;
            // The library splits album-like hits into categories (album / single /
            // compilation / concert / audiobook). Re-merge them into one "albums"
            // list, tagging each with its `category`, so a caller still finds every
            // album-like hit in one place while learning what kind it is.
            let tag =
                |hits: &[crate::model::AlbumHit], cat: &str| -> Vec<Value> {
                    hits.iter()
                    .map(|a| json!({
                        "album": a.album, "artist": a.artist, "year": a.year, "category": cat,
                    }))
                    .collect()
                };
            let mut albums = tag(&r.albums, "album");
            albums.extend(tag(&r.singles, "single"));
            albums.extend(tag(&r.compilations, "compilation"));
            albums.extend(tag(&r.concerts, "concert"));
            albums.extend(tag(&r.audiobooks, "audiobook"));
            Ok(json!({
                "artists": r.artists,
                "albums": albums,
                "songs": r.songs.iter().map(|s| json!({
                    "path": s.path, "title": s.title, "artist": s.artist, "album": s.album,
                })).collect::<Vec<_>>(),
            }))
        }

        "list_artists" => {
            let lib = Library::open()?;
            let names = lib.distinct_artists()?;
            if arg_bool(args, "with_images") == Some(true) {
                // Per-artist `has_image` flag so a caller can find artists whose
                // photo is missing (e.g. to fill them in) over MCP alone.
                let with_img = lib.artist_image_names()?;
                let artists: Vec<Value> = names
                    .into_iter()
                    .map(|n| {
                        let has_image = with_img.contains(&n);
                        json!({ "name": n, "has_image": has_image })
                    })
                    .collect();
                Ok(json!({ "artists": artists }))
            } else {
                Ok(json!({ "artists": names }))
            }
        }

        "list_albums" => {
            // Cap the response so a large library never dumps its whole album list
            // into one call. `total`/`truncated` keep the cap transparent, so the
            // caller can narrow (artist, year range, kind) or raise `limit`.
            let limit = arg_i64(args, "limit").unwrap_or(100).clamp(1, 1000) as usize;
            let year_from = arg_i64(args, "year_from");
            let year_to = arg_i64(args, "year_to");
            let lib = Library::open()?;
            match arg_str(args, "kind") {
                // Classified view: albums / singles / compilations.
                Some(kind_str) => {
                    let kind = crate::model::AlbumKind::from_str(kind_str).ok_or_else(|| {
                        anyhow!("unknown kind '{kind_str}' (use album|single|compilation)")
                    })?;
                    let all: Vec<_> = lib
                        .albums_classified(kind)?
                        .into_iter()
                        .filter(|a| year_from.is_none_or(|f| a.year.is_some_and(|y| y >= f)))
                        .filter(|a| year_to.is_none_or(|t| a.year.is_some_and(|y| y <= t)))
                        .collect();
                    let total = all.len();
                    let albums: Vec<Value> = all
                        .into_iter()
                        .take(limit)
                        .map(|a| {
                            json!({
                                "artist": a.artist,
                                "album": a.album,
                                "year": a.year,
                                "tracks": a.tracks,
                                "kind": a.kind.as_str(),
                            })
                        })
                        .collect();
                    let truncated = total > albums.len();
                    Ok(json!({ "albums": albums, "total": total, "truncated": truncated }))
                }
                // Plain view: every (artist, album) pair, with its year.
                None => {
                    let all = lib.albums_with_year(arg_str(args, "artist"), year_from, year_to)?;
                    let total = all.len();
                    let albums: Vec<Value> = all
                        .into_iter()
                        .take(limit)
                        .map(|(artist, album, year)| {
                            json!({ "artist": artist, "album": album, "year": year })
                        })
                        .collect();
                    let truncated = total > albums.len();
                    Ok(json!({ "albums": albums, "total": total, "truncated": truncated }))
                }
            }
        }

        "list_tracks" => {
            let album = req_str(args, "album")?;
            let lib = Library::open()?;
            let tracks: Vec<Value> = lib
                .tracks_by_album_name(album)?
                .iter()
                .map(track_json)
                .collect();
            Ok(json!({ "tracks": tracks }))
        }

        "list_playlists" => {
            let lib = Library::open()?;
            let playlists: Vec<Value> = lib
                .playlists()?
                .into_iter()
                .map(|(id, name, count)| json!({ "id": id, "name": name, "tracks": count }))
                .collect();
            Ok(json!({ "playlists": playlists }))
        }

        "get_stats" => {
            let days = arg_i64(args, "days").unwrap_or(30).clamp(1, 36500);
            let since = crate::core::sync::now_unix() as i64 - days * 86_400;
            let lib = Library::open()?;
            let mut t = lib.stats_totals(since)?;
            // stats_totals leaves distinct_artists/_albums at 0 by contract --
            // the count is the length of the full (feat./album-name-folded)
            // ranking, so fill them from the top lists exactly as the GUI stats
            // page does (see stats_page.rs).
            t.distinct_artists = lib.stats_top_artists(since, usize::MAX)?.len() as i64;
            t.distinct_albums = lib.stats_top_albums(since, usize::MAX)?.len() as i64;
            Ok(json!({
                "since_days": days,
                "total_played_ms": t.total_played_ms,
                "plays": t.plays,
                "skips": t.skips,
                "distinct_tracks": t.distinct_tracks,
                "distinct_artists": t.distinct_artists,
                "distinct_albums": t.distinct_albums,
            }))
        }

        "library_overview" => {
            let lib = Library::open()?;
            let o = lib.library_overview()?;
            Ok(json!({
                "tracks": o.tracks,
                "artists": o.artists,
                "albums": o.albums,
                "music_duration_ms": o.music_duration_ms,
                "music_duration": fmt_hms(o.music_duration_ms),
                "playlists": o.playlists,
                "podcasts": o.podcasts,
                "episodes": o.episodes,
                "memos": o.memos,
                "memos_duration_ms": o.memos_duration_ms,
                "memos_duration": fmt_hms(o.memos_duration_ms),
                "youtube_channels": o.youtube_channels,
                "youtube_videos": o.youtube_videos,
            }))
        }

        "artist_info" => {
            let name = req_str(args, "name")?;
            let lib = Library::open()?;
            let (albums, songs, duration_ms) = lib.artist_summary(name)?;
            Ok(json!({
                "artist": name,
                "albums": albums,
                "songs": songs,
                "total_duration_ms": duration_ms,
                "total_duration": fmt_hms(duration_ms),
            }))
        }

        "album_info" => {
            let album = req_str(args, "album")?;
            let artist = arg_str(args, "artist");
            let lib = Library::open()?;
            let (tracks, duration_ms, year) = lib.album_summary(artist, album)?;
            Ok(json!({
                "album": album,
                "artist": artist,
                "tracks": tracks,
                "year": year,
                "total_duration_ms": duration_ms,
                "total_duration": fmt_hms(duration_ms),
            }))
        }

        "get_top" => {
            let kind = req_str(args, "kind")?;
            let days = arg_i64(args, "days").unwrap_or(30).clamp(1, 36500);
            let limit = arg_i64(args, "limit").unwrap_or(10).clamp(1, 100) as usize;
            let since = crate::core::sync::now_unix() as i64 - days * 86_400;
            let lib = Library::open()?;
            let entries = match kind {
                "tracks" => lib.stats_top_tracks(since, limit)?,
                "albums" => lib.stats_top_albums(since, limit)?,
                "artists" => lib.stats_top_artists(since, limit)?,
                "genres" => lib.stats_top_genres(since, limit)?,
                other => {
                    return Err(anyhow!(
                        "unknown top kind '{other}' (use tracks|albums|artists|genres)"
                    ))
                }
            };
            let items: Vec<Value> = entries
                .iter()
                .map(|e| {
                    json!({
                        "name": e.name,
                        "detail": e.detail,
                        "plays": e.plays,
                        "played_ms": e.played_ms,
                        "played": fmt_hms(e.played_ms),
                    })
                })
                .collect();
            Ok(json!({ "kind": kind, "since_days": days, "items": items }))
        }

        "list_podcasts" => {
            let lib = Library::open()?;
            let items: Vec<Value> = lib
                .podcasts()?
                .into_iter()
                .map(|(id, title, image_url, episodes)| {
                    json!({ "id": id, "title": title, "image_url": image_url, "episodes": episodes })
                })
                .collect();
            Ok(json!({ "podcasts": items }))
        }

        "list_episodes" => {
            let podcast_id = arg_i64(args, "podcast_id")
                .ok_or_else(|| anyhow!("missing required integer argument 'podcast_id'"))?;
            let limit = arg_i64(args, "limit").unwrap_or(50).clamp(1, 500) as usize;
            let lib = Library::open()?;
            let all = lib.episodes(podcast_id)?;
            let total = all.len();
            let items: Vec<Value> = all
                .into_iter()
                .take(limit)
                .map(|e| {
                    json!({
                        "title": e.title,
                        "url": e.audio_url,
                        "published": e.published,
                        "duration": e.duration,
                    })
                })
                .collect();
            let truncated = total > items.len();
            Ok(json!({ "episodes": items, "total": total, "truncated": truncated }))
        }

        "list_memos" => {
            let lib = Library::open()?;
            let items: Vec<Value> = lib
                .memos()?
                .into_iter()
                .map(|m| {
                    json!({
                        "id": m.id,
                        "title": m.title,
                        "path": m.path,
                        "recorded_at": m.recorded_at,
                        "duration_ms": m.duration_ms,
                        "duration": fmt_hms(m.duration_ms),
                    })
                })
                .collect();
            Ok(json!({ "memos": items }))
        }

        "list_youtube" => {
            let limit = arg_i64(args, "limit").unwrap_or(30).clamp(1, 200) as usize;
            let lib = Library::open()?;
            let items: Vec<Value> = lib
                .recent_videos(limit)?
                .into_iter()
                .map(|v| {
                    // Videos carry their own duration; playlists a summed runtime.
                    let secs = v.duration.or(v.total_duration).unwrap_or(0);
                    json!({
                        "id": v.video_id,
                        "title": v.title,
                        "artist": v.artist,
                        "kind": v.kind,
                        "count": v.count,
                        "duration_s": v.duration,
                        "total_duration_s": v.total_duration,
                        "duration": fmt_hms(secs * 1000),
                    })
                })
                .collect();
            Ok(json!({ "youtube": items }))
        }

        // --- playback control (forwarded to the UI) --------------------------
        "playback_control" => {
            let action = req_str(args, "action")?;
            let cmd = match action {
                "play" => McpCommand::Play,
                "pause" => McpCommand::Pause,
                "toggle" => McpCommand::TogglePlay,
                "next" => McpCommand::Next,
                "prev" | "previous" => McpCommand::Prev,
                other => return Err(anyhow!("unknown action '{other}'")),
            };
            (ctx.control)(cmd);
            Ok(json!({ "ok": true }))
        }

        "seek" => {
            let ms = arg_i64(args, "position_ms")
                .ok_or_else(|| anyhow!("missing required integer argument 'position_ms'"))?;
            (ctx.control)(McpCommand::Seek(ms.max(0)));
            Ok(json!({ "ok": true }))
        }

        "play_album" => {
            let artist = req_str(args, "artist")?.to_string();
            let album = req_str(args, "album")?.to_string();
            (ctx.control)(McpCommand::PlayAlbum { artist, album });
            Ok(json!({ "ok": true }))
        }

        "play_artist" => {
            let name = req_str(args, "name")?.to_string();
            (ctx.control)(McpCommand::PlayArtist(name));
            Ok(json!({ "ok": true }))
        }

        "play_track" => {
            let path = req_str(args, "path")?.to_string();
            (ctx.control)(McpCommand::PlayTrack(path));
            Ok(json!({ "ok": true }))
        }

        "play_episode" => {
            let url = req_str(args, "url")?.to_string();
            let title = arg_str(args, "title")
                .unwrap_or("Podcast episode")
                .to_string();
            (ctx.control)(McpCommand::PlayEpisode { url, title });
            Ok(json!({ "ok": true }))
        }

        "play_memo" => {
            let path = req_str(args, "path")?.to_string();
            (ctx.control)(McpCommand::PlayMemo(path));
            Ok(json!({ "ok": true }))
        }

        "play_youtube" => {
            let raw = req_str(args, "id")?;
            // Accept a full watch URL as well as a bare video id.
            let video_id =
                crate::core::youtube::video_id_from_url(raw).unwrap_or_else(|| raw.to_string());
            let title = match arg_str(args, "title") {
                Some(t) => t.to_string(),
                None => Library::open()?
                    .yt_title(&video_id)?
                    .unwrap_or_else(|| "YouTube".to_string()),
            };
            (ctx.control)(McpCommand::PlayYoutube {
                video_id: video_id.clone(),
                title,
            });
            Ok(json!({ "ok": true, "video_id": video_id }))
        }

        "set_sleep_timer" => {
            let minutes = arg_i64(args, "minutes").unwrap_or(0).clamp(0, 1440) as u32;
            (ctx.control)(McpCommand::SetSleepTimer(minutes));
            Ok(json!({ "ok": true, "minutes": minutes }))
        }

        // --- library writes --------------------------------------------------
        "create_playlist" => {
            let name = req_str(args, "name")?;
            let lib = Library::open()?;
            let id = lib.create_playlist(name)?;
            Ok(json!({ "id": id, "name": name }))
        }

        "add_to_playlist" => {
            let id = arg_i64(args, "playlist_id")
                .ok_or_else(|| anyhow!("missing required integer argument 'playlist_id'"))?;
            let paths: Vec<String> = args
                .get("paths")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            if paths.is_empty() {
                return Err(anyhow!("'paths' must be a non-empty array of track paths"));
            }
            let lib = Library::open()?;
            lib.add_to_playlist(id, &paths)?;
            Ok(json!({ "ok": true, "added": paths.len() }))
        }

        "toggle_favorite" => {
            let scope = req_str(args, "scope")?;
            let key = req_str(args, "key")?;
            let title = arg_str(args, "title").unwrap_or(key);
            let lib = Library::open()?;
            let now_on = !lib.is_favorite(scope, key);
            lib.set_favorite(scope, key, title, false, now_on)?;
            Ok(json!({ "favorite": now_on }))
        }

        // --- playlist actions (routed through the UI so it stays in sync) ----
        "play_playlist" => {
            let id = req_i64(args, "playlist_id")?;
            let shuffle = args
                .get("shuffle")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            (ctx.control)(McpCommand::PlayPlaylist { id, shuffle });
            Ok(json!({ "ok": true }))
        }

        "rename_playlist" => {
            let id = req_i64(args, "playlist_id")?;
            let name = req_str(args, "name")?.to_string();
            (ctx.control)(McpCommand::RenamePlaylist { id, name });
            Ok(json!({ "ok": true }))
        }

        "delete_playlist" => {
            let id = req_i64(args, "playlist_id")?;
            require_confirm(args)?;
            (ctx.control)(McpCommand::DeletePlaylist(id));
            Ok(json!({ "ok": true, "deleted": id }))
        }

        "set_playlist_cover" => {
            let id = req_i64(args, "playlist_id")?;
            let path = req_str(args, "path")?.to_string();
            (ctx.control)(McpCommand::SetPlaylistCover { id, path });
            Ok(json!({ "ok": true }))
        }

        // --- queue / item actions -------------------------------------------
        "enqueue" => {
            let paths: Vec<String> = args
                .get("paths")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            if paths.is_empty() {
                return Err(anyhow!("'paths' must be a non-empty array of track paths"));
            }
            let n = paths.len();
            (ctx.control)(McpCommand::Enqueue(paths));
            Ok(json!({ "ok": true, "enqueued": n }))
        }

        "toggle_episode" => {
            let url = req_str(args, "url")?.to_string();
            let title = arg_str(args, "title")
                .unwrap_or("Podcast episode")
                .to_string();
            (ctx.control)(McpCommand::ToggleEpisodeListened { url, title });
            Ok(json!({ "ok": true }))
        }

        "delete_memo" => {
            let id = req_i64(args, "memo_id")?;
            require_confirm(args)?;
            (ctx.control)(McpCommand::DeleteMemo(id));
            Ok(json!({ "ok": true, "deleted": id }))
        }

        "delete_recording" => {
            let id = req_i64(args, "recording_id")?;
            require_confirm(args)?;
            (ctx.control)(McpCommand::DeleteRecording(id));
            Ok(json!({ "ok": true, "deleted": id }))
        }

        "set_album_cover" => {
            let artist = req_str(args, "artist")?.to_string();
            let album = req_str(args, "album")?.to_string();
            let path = req_str(args, "path")?.to_string();
            (ctx.control)(McpCommand::SetAlbumCover {
                artist,
                album,
                path,
            });
            Ok(json!({ "ok": true }))
        }

        "set_artist_image" => {
            let name = req_str(args, "name")?.to_string();
            let path = req_str(args, "path")?.to_string();
            (ctx.control)(McpCommand::SetArtistImage { name, path });
            Ok(json!({ "ok": true }))
        }

        "list_artist_image_candidates" => {
            let artist = req_str(args, "artist")?;
            let limit = arg_i64(args, "limit").unwrap_or(5).clamp(1, 8) as usize;
            // Needs the user's (free) fanart.tv key — read from the per-request
            // library, same as the enrichment UI path.
            let key = Library::open()?
                .get_secret_setting("fanart_key")
                .ok()
                .flatten()
                .filter(|k| !k.is_empty())
                .ok_or_else(|| {
                    anyhow!("no fanart.tv API key configured — set one in the enrichment settings to look up artist images")
                })?;
            let client = crate::core::online::OnlineClient::new();
            let mbid = client
                .artist_mbid(artist)?
                .ok_or_else(|| anyhow!("no MusicBrainz match for artist '{artist}'"))?;
            let images = client.artist_gallery_urls(&key, &mbid, limit)?;
            Ok(json!({ "artist": artist, "count": images.len(), "images": images }))
        }

        "enrich_artist_images" => {
            let artist = req_str(args, "artist")?.to_string();
            let lib = Library::open()?;
            let key = lib
                .get_secret_setting("fanart_key")
                .ok()
                .flatten()
                .filter(|k| !k.is_empty())
                .ok_or_else(|| {
                    anyhow!("no fanart.tv API key configured — set one in the enrichment settings to fetch artist images")
                })?;
            let client = crate::core::online::OnlineClient::new();
            // Same path the enrichment UI uses: MBID → fanart gallery → store
            // (replacing any previously fetched gallery for this artist).
            let added = crate::core::online::enrich_artist_gallery(&client, &lib, &artist, &key);
            Ok(json!({ "artist": artist, "added": added }))
        }

        "set_properties" => {
            let scope = req_str(args, "scope")?;
            if !matches!(scope, "track" | "album" | "artist") {
                return Err(anyhow!("scope must be one of: track, album, artist"));
            }
            let key = req_str(args, "key")?.to_string();
            // Comma-separated area list (empty = hidden). Unknown areas are
            // dropped by the UI's own parser.
            let value: String = args
                .get("areas")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str())
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_default();
            (ctx.control)(McpCommand::SetAreas {
                scope: scope.to_string(),
                key,
                value,
            });
            Ok(json!({ "ok": true }))
        }

        "set_album_kind" => {
            let album = req_str(args, "album")?;
            let kind_str = req_str(args, "kind")?;
            let lib = Library::open()?;
            if kind_str == "auto" {
                lib.clear_album_kind(album)?;
                Ok(json!({ "ok": true, "album": album, "kind": "auto" }))
            } else {
                let kind = crate::model::AlbumKind::from_str(kind_str).ok_or_else(|| {
                    anyhow!("kind must be one of: album, single, compilation, auto")
                })?;
                lib.set_album_kind(album, kind)?;
                Ok(json!({ "ok": true, "album": album, "kind": kind.as_str() }))
            }
        }

        // --- online search (network; blocking, run off the async worker) ----
        "search_youtube" => {
            use crate::core::youtube::{self, YtKind};
            if !youtube::available() {
                return Err(anyhow!("yt-dlp is not available on this system"));
            }
            let query = req_str(args, "query")?;
            let limit = arg_i64(args, "limit").unwrap_or(15).clamp(1, 50) as usize;
            let kind = match arg_str(args, "kind") {
                Some("playlist") => YtKind::Playlist,
                Some("channel") => YtKind::Channel,
                _ => YtKind::Video,
            };
            let kind_str = |k: &YtKind| match k {
                YtKind::Video => "video",
                YtKind::Playlist => "playlist",
                YtKind::Channel => "channel",
            };
            let results = youtube::search(query, kind, limit)?;
            let items: Vec<Value> = results
                .iter()
                .map(|r| {
                    json!({
                        "id": r.id,
                        "url": r.url,
                        "title": r.title,
                        "uploader": r.uploader,
                        "duration_s": r.duration,
                        "kind": kind_str(&r.kind),
                    })
                })
                .collect();
            Ok(json!({ "results": items }))
        }

        "search_podcasts" => {
            let query = req_str(args, "query")?;
            let limit = arg_i64(args, "limit").unwrap_or(25).clamp(1, 50) as usize;
            let results = crate::core::podcast::search_podcasts(query)?;
            let items: Vec<Value> = results
                .iter()
                .take(limit)
                .map(|r| {
                    json!({
                        "title": r.title,
                        "author": r.author,
                        "feed_url": r.feed_url,
                    })
                })
                .collect();
            Ok(json!({ "results": items }))
        }

        // --- downloads (long-running → background job + list_jobs status) ----
        "download_youtube" => {
            use crate::core::youtube;
            if !youtube::available() {
                return Err(anyhow!("yt-dlp is not available on this system"));
            }
            let raw = req_str(args, "id")?;
            let video_id = youtube::video_id_from_url(raw).unwrap_or_else(|| raw.to_string());
            let music = Library::open()?
                .get_setting("music_dir")?
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| anyhow!("no music folder configured"))?;
            let jobs = ctx.jobs.clone();
            let job_id = jobs.start("youtube_download", &video_id);
            let vid = video_id.clone();
            std::thread::spawn(move || {
                let res =
                    youtube::add_to_library(&vid, &vid, None, &music, None, false).map(
                        |o| match o {
                            youtube::AddOutcome::Added => "added to library".to_string(),
                            youtube::AddOutcome::Exists(p) => {
                                format!("already present: {}", p.display())
                            }
                        },
                    );
                jobs.finish(job_id, res);
            });
            Ok(json!({ "ok": true, "job_id": job_id, "video_id": video_id }))
        }

        "download_episode" => {
            let url = req_str(args, "url")?.to_string();
            let jobs = ctx.jobs.clone();
            let job_id = jobs.start("episode_download", &url);
            std::thread::spawn(move || {
                let res = (|| -> Result<String> {
                    let dest = crate::core::online::episode_download_dest(&url);
                    crate::core::podcast::download_episode(&url, &dest)?;
                    let path = dest.to_string_lossy().into_owned();
                    Library::open()?.set_episode_download(&url, &path)?;
                    Ok(path)
                })()
                .map_err(|e| e.to_string());
                jobs.finish(job_id, res);
            });
            Ok(json!({ "ok": true, "job_id": job_id }))
        }

        "list_jobs" => {
            let items: Vec<Value> = ctx
                .jobs
                .snapshot()
                .iter()
                .map(|j| {
                    json!({
                        "id": j.id,
                        "kind": j.kind,
                        "label": j.label,
                        "state": j.state.as_str(),
                        "detail": j.detail,
                    })
                })
                .collect();
            Ok(json!({ "jobs": items }))
        }

        // --- podcasts: subscriptions -------------------------------------------
        "subscribe_podcast" => {
            let url = req_str(args, "feed_url")?;
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                return Err(anyhow!(
                    "`feed_url` must be an http(s) RSS feed URL (see search_podcasts)"
                ));
            }
            let lib = Library::open()?;
            let (id, title, _) = crate::core::podcast::subscribe_feed(&lib, url)?;
            let episodes = lib.episodes(id).map(|e| e.len()).unwrap_or(0);
            (ctx.control)(McpCommand::ReloadPodcasts);
            Ok(json!({ "id": id, "title": title, "episodes": episodes }))
        }
        "unsubscribe_podcast" => {
            require_confirm(args)?;
            let id = req_i64(args, "podcast_id")?;
            let lib = Library::open()?;
            let title = lib
                .podcast_title(id)?
                .ok_or_else(|| anyhow!("no podcast with id {id} (see list_podcasts)"))?;
            (ctx.control)(McpCommand::DeletePodcast(id));
            Ok(json!({ "ok": true, "id": id, "title": title }))
        }
        "refresh_podcasts" => {
            (ctx.control)(McpCommand::RefreshPodcasts);
            Ok(json!({ "ok": true }))
        }
        "delete_episode_download" => {
            require_confirm(args)?;
            let url = req_str(args, "url")?;
            let lib = Library::open()?;
            let path = lib
                .delete_episode_download(url)?
                .ok_or_else(|| anyhow!("this episode has no offline download"))?;
            let file_removed = std::fs::remove_file(&path).is_ok();
            (ctx.control)(McpCommand::ReloadPodcasts);
            Ok(json!({ "ok": true, "path": path, "file_removed": file_removed }))
        }

        // --- radio stations (live streams) ------------------------------------
        "list_stations" => {
            let lib = Library::open()?;
            let items: Vec<Value> = lib.streams()?.iter().map(station_json).collect();
            Ok(json!({ "stations": items }))
        }
        "search_stations" => {
            let query = req_str(args, "query")?;
            let limit = arg_i64(args, "limit").unwrap_or(15).clamp(1, 50) as usize;
            let hits = crate::core::streaming::search_stations(query)?;
            let items: Vec<Value> = hits
                .iter()
                .take(limit)
                .map(|r| {
                    json!({
                        "name": r.name,
                        "url": r.url,
                        "favicon": r.favicon,
                        "tags": r.tags,
                        "country": r.country,
                        "codec": r.codec,
                        "bitrate": r.bitrate,
                    })
                })
                .collect();
            Ok(json!({ "results": items }))
        }
        "add_station" => {
            let url = req_str(args, "url")?;
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                return Err(anyhow!("`url` must be an http(s) stream URL"));
            }
            let name = arg_str(args, "name")
                .map(str::to_string)
                .unwrap_or_else(|| crate::core::streaming::name_from_url(url));
            let lib = Library::open()?;
            let id = lib.add_stream(
                &name,
                url,
                arg_str(args, "favicon"),
                arg_str(args, "tags"),
                arg_str(args, "country"),
                arg_str(args, "codec"),
                arg_i64(args, "bitrate"),
            )?;
            (ctx.control)(McpCommand::ReloadStations);
            Ok(json!({ "id": id, "name": name, "url": url }))
        }
        "rename_station" => {
            let id = req_i64(args, "station_id")?;
            let name = req_str(args, "name")?;
            let lib = Library::open()?;
            station_by_id(&lib, id)?;
            lib.rename_stream(id, name)?;
            (ctx.control)(McpCommand::ReloadStations);
            Ok(json!({ "ok": true, "id": id, "name": name }))
        }
        "delete_station" => {
            require_confirm(args)?;
            let id = req_i64(args, "station_id")?;
            let lib = Library::open()?;
            let st = station_by_id(&lib, id)?;
            (ctx.control)(McpCommand::DeleteStation(id));
            Ok(json!({ "ok": true, "id": id, "name": st.name }))
        }
        "play_station" => {
            let id = req_i64(args, "station_id")?;
            let lib = Library::open()?;
            let st = station_by_id(&lib, id)?;
            (ctx.control)(McpCommand::PlayStation(id));
            Ok(json!({ "ok": true, "id": id, "name": st.name }))
        }
        "toggle_station_recording" => {
            let id = req_i64(args, "station_id")?;
            let lib = Library::open()?;
            station_by_id(&lib, id)?;
            (ctx.control)(McpCommand::ToggleStationRecording(id));
            Ok(json!({ "ok": true }))
        }

        // --- library: tag editing + deletion ------------------------------------
        "set_track_tags" => {
            let path = req_str(args, "path")?;
            // An empty string clears a text tag; `0` clears a numeric one.
            let text = |k: &str| args.get(k).and_then(|v| v.as_str()).map(str::to_string);
            let num = |k: &str| -> Result<Option<u32>> {
                match args.get(k) {
                    None | Some(Value::Null) => Ok(None),
                    Some(v) => v
                        .as_u64()
                        .map(|n| Some(n as u32))
                        .ok_or_else(|| anyhow!("`{k}` must be a non-negative integer")),
                }
            };
            let edit = crate::core::scanner::TagEdit {
                title: text("title"),
                artist: text("artist"),
                album: text("album"),
                album_artist: text("album_artist"),
                genre: text("genre"),
                year: num("year")?,
                track_no: num("track_no")?,
                disc_no: num("disc_no")?,
            };
            if edit.is_empty() {
                return Err(anyhow!(
                    "nothing to change: pass at least one of title, artist, album, album_artist, genre, year, track_no, disc_no"
                ));
            }
            let lib = Library::open()?;
            local_track(&lib, path)?;
            let file = std::path::Path::new(path);
            crate::core::scanner::write_tags(file, &edit)?;
            // Re-read the file so the row reflects exactly what was written.
            let track = crate::core::scanner::read_track(file)?;
            lib.upsert_track(&track)?;
            (ctx.control)(McpCommand::LibraryChanged);
            Ok(json!({ "ok": true, "track": track_json(&track) }))
        }
        "delete_track" => {
            require_confirm(args)?;
            let path = req_str(args, "path")?;
            let lib = Library::open()?;
            let track = local_track(&lib, path)?;
            trash_file(std::path::Path::new(path))?;
            lib.delete_track(path)?;
            (ctx.control)(McpCommand::LibraryChanged);
            Ok(json!({ "ok": true, "trashed": path, "title": track.title }))
        }
        "delete_album" => {
            require_confirm(args)?;
            let artist = req_str(args, "artist")?;
            let album = req_str(args, "album")?;
            let lib = Library::open()?;
            let paths = lib.album_track_paths(artist, album)?;
            if paths.is_empty() {
                return Err(anyhow!(
                    "no album '{album}' by '{artist}' in the library (see list_albums)"
                ));
            }
            let mut trashed = Vec::new();
            let mut skipped = Vec::new();
            for p in &paths {
                if crate::core::webdav::parse_nc_path(p).is_some() {
                    skipped.push(json!({ "path": p, "reason": "network source (read-only)" }));
                    continue;
                }
                match trash_file(std::path::Path::new(p)) {
                    Ok(()) => {
                        let _ = lib.delete_track(p);
                        trashed.push(p.clone());
                    }
                    Err(e) => skipped.push(json!({ "path": p, "reason": e.to_string() })),
                }
            }
            (ctx.control)(McpCommand::LibraryChanged);
            Ok(json!({ "ok": skipped.is_empty(), "trashed": trashed, "skipped": skipped }))
        }

        // --- sources + folder browsing ------------------------------------------
        "list_sources" => {
            let lib = Library::open()?;
            let music_dir = lib.get_setting("music_dir")?.unwrap_or_default();
            let mut items = vec![json!({
                "id": 0, "kind": "local", "name": "Music", "root": music_dir, "primary": true,
            })];
            for s in lib.list_sources()? {
                let root = if s.is_remote() {
                    format!("nc:{}:", s.id)
                } else {
                    s.path.clone().unwrap_or_default()
                };
                items.push(json!({
                    "id": s.id,
                    "kind": s.kind,
                    "name": s.name,
                    "root": root,
                    "location": s.base_url,
                    "music_path": s.music_path,
                    "primary": false,
                }));
            }
            Ok(json!({ "sources": items }))
        }
        "list_folder" => {
            let lib = Library::open()?;
            let path = match arg_str(args, "path") {
                Some(p) => p.to_string(),
                None => lib
                    .get_setting("music_dir")?
                    .ok_or_else(|| anyhow!("no music folder configured"))?,
            };
            let limit = arg_i64(args, "limit").unwrap_or(200).clamp(1, 2000) as usize;
            // A source root `nc:<id>:` is matched raw (its relative paths may or
            // may not start with a slash); anything else is a folder prefix.
            let is_source_root =
                crate::core::webdav::parse_nc_path(&path).is_some_and(|(_, rel)| rel.is_empty());
            let prefix = if is_source_root {
                path.clone()
            } else {
                format!("{}/", path.trim_end_matches('/'))
            };
            let tracks = lib.tracks_with_prefix(&prefix)?;
            let mut folders: std::collections::BTreeMap<String, (String, usize)> =
                std::collections::BTreeMap::new();
            let mut files = Vec::new();
            for t in &tracks {
                let rest = t.path[prefix.len()..].trim_start_matches('/');
                match rest.split_once('/') {
                    Some((dir, _)) => {
                        // The child's own path, taken from the real track path so
                        // the separator convention of the source is kept.
                        let head = t.path.len() - rest.len();
                        let child = format!("{}{}", &t.path[..head], dir);
                        folders.entry(dir.to_string()).or_insert((child, 0)).1 += 1;
                    }
                    None => files.push(t),
                }
            }
            let total = files.len();
            let items: Vec<Value> = files.iter().take(limit).map(|t| track_json(t)).collect();
            let folders: Vec<Value> = folders
                .into_iter()
                .map(|(name, (path, n))| json!({ "name": name, "path": path, "tracks": n }))
                .collect();
            Ok(json!({
                "path": path,
                "folders": folders,
                "tracks": items,
                "total_tracks": total,
                "truncated": total > limit,
            }))
        }

        // --- device sync ----------------------------------------------------------
        "sync_status" => Ok(sync_json(&sync_snapshot(ctx))),
        "sync_start_server" => {
            let snap = sync_snapshot(ctx);
            if snap.connected {
                return Err(anyhow!(
                    "already paired with {}",
                    snap.peer_name.unwrap_or_default()
                ));
            }
            if !snap.listening {
                (ctx.control)(McpCommand::SyncStartServer);
            }
            let ready = sync_wait(ctx, std::time::Duration::from_secs(3), |s| {
                s.pair_url.clone().map(|u| (u, s.address.clone()))
            });
            match ready {
                Some((pair_url, address)) => Ok(json!({
                    "ok": true,
                    "pair_url": pair_url,
                    "address": address,
                    "expires_in_s": crate::core::sync::QR_TTL.as_secs(),
                    "hint": "On the other device open Connect → scan the QR shown on this device, or paste this code there.",
                })),
                None => Err(anyhow!(
                    "{}",
                    sync_snapshot(ctx).last_error.unwrap_or_else(|| {
                        "the pairing server reported no code in time; poll sync_status".into()
                    })
                )),
            }
        }
        "sync_pair" => {
            let code = req_str(args, "code")?.trim().to_string();
            crate::core::sync::protocol::parse_pair_url(&code, crate::core::sync::now_unix())
                .map_err(|e| anyhow!("invalid or expired pairing code: {e}"))?;
            let snap = sync_snapshot(ctx);
            if snap.connected {
                return Err(anyhow!(
                    "already paired with {}",
                    snap.peer_name.unwrap_or_default()
                ));
            }
            if snap.listening {
                return Err(anyhow!(
                    "this device is offering a connection itself; run sync_disconnect first"
                ));
            }
            (ctx.control)(McpCommand::SyncPair(code));
            let paired = sync_wait(ctx, std::time::Duration::from_secs(8), |s| {
                if s.connected {
                    Some(s.peer_name.clone())
                } else {
                    None
                }
            });
            let snap = sync_snapshot(ctx);
            Ok(json!({
                "ok": true,
                "connected": paired.is_some(),
                "peer_name": paired.flatten(),
                "error": snap.last_error,
                "note": if snap.connected { Value::Null } else { json!("pairing still in progress — poll sync_status") },
            }))
        }
        "sync_share" => {
            let snap = sync_snapshot(ctx);
            if !snap.connected {
                return Err(anyhow!(
                    "not paired with another device — run sync_start_server (or sync_pair) first"
                ));
            }
            let sel = selection_from_args(args)?;
            if sel.is_empty() {
                return Err(anyhow!(
                    "nothing selected: pass artists, albums, tracks, playlist_ids, podcast_ids, station_ids, memo_ids, recording_ids, yt_videos or whole_library"
                ));
            }
            (ctx.control)(McpCommand::SyncShare(Box::new(sel)));
            Ok(json!({
                "ok": true,
                "peer_name": snap.peer_name,
                "note": "the offer is being prepared and sent; the other device has to accept it — poll sync_status for the phase and transfer progress",
            }))
        }
        "sync_respond" => {
            let accept = arg_bool(args, "accept")
                .ok_or_else(|| anyhow!("missing required boolean argument 'accept'"))?;
            let snap = sync_snapshot(ctx);
            if snap.incoming_offer.is_none() {
                return Err(anyhow!("no pending incoming offer (see sync_status)"));
            }
            (ctx.control)(McpCommand::SyncRespond { accept });
            Ok(json!({ "ok": true, "accepted": accept }))
        }
        "sync_disconnect" => {
            let snap = sync_snapshot(ctx);
            if !snap.connected && !snap.listening {
                return Err(anyhow!("no live pairing or pairing server"));
            }
            (ctx.control)(McpCommand::SyncDisconnect);
            Ok(json!({ "ok": true }))
        }

        other => Err(anyhow!("unknown tool '{other}'")),
    }
}

// ---- tool registry (advertised to the client) --------------------------------

/// The advertised tool list, honoring the YouTube setting: when the integration
/// is disabled the YouTube tools are dropped so a client never sees them. Use
/// this (not [`tool_list`]) everywhere a list is returned to a client.
pub fn tool_list_enabled() -> Value {
    let mut list = tool_list();
    if !youtube_enabled() {
        if let Some(arr) = list.as_array_mut() {
            arr.retain(|t| {
                t.get("name")
                    .and_then(|n| n.as_str())
                    .is_none_or(|n| !YOUTUBE_TOOLS.contains(&n))
            });
        }
    }
    list
}

/// The full list of tool descriptors returned by `tools/list`. Schemas are kept
/// hand-written (small, stable set) rather than derived.
pub fn tool_list() -> Value {
    let obj = |props: Value, required: Value| json!({ "type": "object", "properties": props, "required": required });
    let empty = || obj(json!({}), json!([]));

    json!([
        {
            "name": "now_playing",
            "description": "Return the currently playing track and playback position.",
            "inputSchema": empty(),
        },
        {
            "name": "search_library",
            "description": "Search the local library (artists, albums, songs) by substring. A numeric query also matches an album release year.",
            "inputSchema": obj(
                json!({
                    "query": { "type": "string", "description": "Search text." },
                    "limit": { "type": "integer", "description": "Max hits per group (default 20).", "minimum": 1, "maximum": 200 },
                }),
                json!(["query"]),
            ),
        },
        {
            "name": "list_artists",
            "description": "List all distinct artists in the library. By default returns an array of names. Set `with_images` to true to instead return objects `{ name, has_image }`, where `has_image` is false for artists that have no photo yet — use this to find artists whose image is missing.",
            "inputSchema": obj(
                json!({
                    "with_images": { "type": "boolean", "description": "Return `{ name, has_image }` objects instead of plain names (default false)." },
                }),
                json!([]),
            ),
        },
        {
            "name": "list_albums",
            "description": "List albums, each with its release year (the earliest tagged track year). Optionally narrow by `artist` and/or an inclusive `year_from`/`year_to` range. Set `kind` to 'single' or 'compilation' (or 'album' for regular albums) to use the album-type classification — compilations are merged per name and carry a `tracks` count. Returns at most `limit` (default 100); the response carries the full `total` and a `truncated` flag.",
            "inputSchema": obj(
                json!({
                    "artist": { "type": "string", "description": "Restrict to this exact artist (optional; ignored when `kind` is set)." },
                    "kind": { "type": "string", "enum": ["album", "single", "compilation"], "description": "Album-type classification (optional)." },
                    "year_from": { "type": "integer", "description": "Earliest release year, inclusive (optional)." },
                    "year_to": { "type": "integer", "description": "Latest release year, inclusive (optional)." },
                    "limit": { "type": "integer", "description": "Max albums returned (default 100).", "minimum": 1, "maximum": 1000 },
                }),
                json!([]),
            ),
        },
        {
            "name": "list_tracks",
            "description": "List the tracks of an album (by album name).",
            "inputSchema": obj(
                json!({ "album": { "type": "string", "description": "Album name." } }),
                json!(["album"]),
            ),
        },
        {
            "name": "list_playlists",
            "description": "List the user's playlists with their track counts.",
            "inputSchema": empty(),
        },
        {
            "name": "get_stats",
            "description": "Aggregated listening statistics over the last N days (default 30).",
            "inputSchema": obj(
                json!({ "days": { "type": "integer", "description": "Look-back window in days.", "minimum": 1 } }),
                json!([]),
            ),
        },
        {
            "name": "library_overview",
            "description": "Whole-library inventory: counts and total runtime for tracks/artists/albums, playlists, podcasts & episodes, memos, and YouTube. Durations are given as raw `*_ms` and a human-readable string.",
            "inputSchema": empty(),
        },
        {
            "name": "artist_info",
            "description": "Tallies for one artist: number of albums and songs, plus total track runtime. Counts collaborations (\"feat.\") toward the named artist.",
            "inputSchema": obj(
                json!({ "name": { "type": "string", "description": "Artist name (case-insensitive)." } }),
                json!(["name"]),
            ),
        },
        {
            "name": "album_info",
            "description": "Tallies for one album: track count, total runtime, and release year. Pass `artist` to disambiguate an album name shared by several artists.",
            "inputSchema": obj(
                json!({
                    "album": { "type": "string", "description": "Album name (case-insensitive)." },
                    "artist": { "type": "string", "description": "Restrict to this artist (optional)." },
                }),
                json!(["album"]),
            ),
        },
        {
            "name": "get_top",
            "description": "Top-played rankings from the listening history over the last N days (default 30): most-played tracks, albums, artists, or genres.",
            "inputSchema": obj(
                json!({
                    "kind": { "type": "string", "enum": ["tracks", "albums", "artists", "genres"] },
                    "days": { "type": "integer", "description": "Look-back window in days (default 30).", "minimum": 1 },
                    "limit": { "type": "integer", "description": "Max entries (default 10).", "minimum": 1, "maximum": 100 },
                }),
                json!(["kind"]),
            ),
        },
        {
            "name": "list_podcasts",
            "description": "List subscribed podcasts with their episode counts. Use the returned `id` with `list_episodes`.",
            "inputSchema": empty(),
        },
        {
            "name": "list_episodes",
            "description": "List a podcast's episodes (newest first) by its `podcast_id` (from `list_podcasts`). Each carries a `url` usable to play it. Returns at most `limit` (default 50) with `total`/`truncated`.",
            "inputSchema": obj(
                json!({
                    "podcast_id": { "type": "integer", "description": "Podcast id from list_podcasts." },
                    "limit": { "type": "integer", "description": "Max episodes (default 50).", "minimum": 1, "maximum": 500 },
                }),
                json!(["podcast_id"]),
            ),
        },
        {
            "name": "list_memos",
            "description": "List voice memos / recordings with their playback length.",
            "inputSchema": empty(),
        },
        {
            "name": "list_youtube",
            "description": "List recently played YouTube videos and playlists (the library's YouTube 'Recently' list), with cached runtime.",
            "inputSchema": obj(
                json!({ "limit": { "type": "integer", "description": "Max entries (default 30).", "minimum": 1, "maximum": 200 } }),
                json!([]),
            ),
        },
        {
            "name": "playback_control",
            "description": "Control transport: play, pause, toggle, next, prev.",
            "inputSchema": obj(
                json!({ "action": { "type": "string", "enum": ["play", "pause", "toggle", "next", "prev"] } }),
                json!(["action"]),
            ),
        },
        {
            "name": "seek",
            "description": "Seek to an absolute position in the current track.",
            "inputSchema": obj(
                json!({ "position_ms": { "type": "integer", "description": "Absolute position in milliseconds.", "minimum": 0 } }),
                json!(["position_ms"]),
            ),
        },
        {
            "name": "play_album",
            "description": "Play a whole album in track order.",
            "inputSchema": obj(
                json!({
                    "artist": { "type": "string" },
                    "album": { "type": "string" },
                }),
                json!(["artist", "album"]),
            ),
        },
        {
            "name": "play_artist",
            "description": "Play all tracks of an artist.",
            "inputSchema": obj(
                json!({ "name": { "type": "string" } }),
                json!(["name"]),
            ),
        },
        {
            "name": "play_track",
            "description": "Play a single track by its library path.",
            "inputSchema": obj(
                json!({ "path": { "type": "string", "description": "Track path as listed by the library." } }),
                json!(["path"]),
            ),
        },
        {
            "name": "play_episode",
            "description": "Play a podcast episode by its audio URL (from list_episodes); resumes at the remembered position.",
            "inputSchema": obj(
                json!({
                    "url": { "type": "string", "description": "Episode audio URL (the `url` field from list_episodes)." },
                    "title": { "type": "string", "description": "Display title (optional)." },
                }),
                json!(["url"]),
            ),
        },
        {
            "name": "play_memo",
            "description": "Play a voice memo / recording by its file path (from list_memos).",
            "inputSchema": obj(
                json!({ "path": { "type": "string", "description": "Memo file path (the `path` field from list_memos)." } }),
                json!(["path"]),
            ),
        },
        {
            "name": "play_youtube",
            "description": "Play a YouTube video by its id or watch URL (from list_youtube). Resolves a fresh audio stream.",
            "inputSchema": obj(
                json!({
                    "id": { "type": "string", "description": "YouTube video id or watch URL." },
                    "title": { "type": "string", "description": "Display title (optional; looked up if omitted)." },
                }),
                json!(["id"]),
            ),
        },
        {
            "name": "set_sleep_timer",
            "description": "Arm the sleep timer for N minutes; 0 turns it off.",
            "inputSchema": obj(
                json!({ "minutes": { "type": "integer", "minimum": 0, "maximum": 1440 } }),
                json!(["minutes"]),
            ),
        },
        {
            "name": "create_playlist",
            "description": "Create a new (empty) playlist and return its id.",
            "inputSchema": obj(
                json!({ "name": { "type": "string" } }),
                json!(["name"]),
            ),
        },
        {
            "name": "add_to_playlist",
            "description": "Append track paths to a playlist.",
            "inputSchema": obj(
                json!({
                    "playlist_id": { "type": "integer" },
                    "paths": { "type": "array", "items": { "type": "string" } },
                }),
                json!(["playlist_id", "paths"]),
            ),
        },
        {
            "name": "toggle_favorite",
            "description": "Toggle a favorite. scope ∈ {track, folder, album, artist}; key = path | artist\\u0001album | artist name.",
            "inputSchema": obj(
                json!({
                    "scope": { "type": "string", "enum": ["track", "folder", "album", "artist"] },
                    "key": { "type": "string" },
                    "title": { "type": "string", "description": "Display name (optional)." },
                }),
                json!(["scope", "key"]),
            ),
        },
        {
            "name": "play_playlist",
            "description": "Play a playlist by its id (from list_playlists), optionally shuffled.",
            "inputSchema": obj(
                json!({
                    "playlist_id": { "type": "integer", "description": "Playlist id." },
                    "shuffle": { "type": "boolean", "description": "Shuffle playback (default false)." },
                }),
                json!(["playlist_id"]),
            ),
        },
        {
            "name": "rename_playlist",
            "description": "Rename a playlist.",
            "inputSchema": obj(
                json!({
                    "playlist_id": { "type": "integer", "description": "Playlist id." },
                    "name": { "type": "string", "description": "New name." },
                }),
                json!(["playlist_id", "name"]),
            ),
        },
        {
            "name": "delete_playlist",
            "description": "Delete a playlist. Destructive: requires `confirm: true`.",
            "inputSchema": obj(
                json!({
                    "playlist_id": { "type": "integer", "description": "Playlist id." },
                    "confirm": { "type": "boolean", "description": "Must be true to actually delete." },
                }),
                json!(["playlist_id", "confirm"]),
            ),
        },
        {
            "name": "set_playlist_cover",
            "description": "Set a playlist's cover image from a local file path.",
            "inputSchema": obj(
                json!({
                    "playlist_id": { "type": "integer", "description": "Playlist id." },
                    "path": { "type": "string", "description": "Image file path." },
                }),
                json!(["playlist_id", "path"]),
            ),
        },
        {
            "name": "enqueue",
            "description": "Append tracks (by library path) to the play-next queue, without interrupting playback.",
            "inputSchema": obj(
                json!({
                    "paths": { "type": "array", "items": { "type": "string" }, "description": "Track paths to enqueue." },
                }),
                json!(["paths"]),
            ),
        },
        {
            "name": "toggle_episode",
            "description": "Toggle a podcast episode's listened/unlistened state (by its audio URL).",
            "inputSchema": obj(
                json!({
                    "url": { "type": "string", "description": "Episode audio URL." },
                    "title": { "type": "string", "description": "Display title (optional)." },
                }),
                json!(["url"]),
            ),
        },
        {
            "name": "delete_memo",
            "description": "Delete a voice memo by id (from list_memos). Destructive: requires `confirm: true`.",
            "inputSchema": obj(
                json!({
                    "memo_id": { "type": "integer", "description": "Memo id." },
                    "confirm": { "type": "boolean", "description": "Must be true to actually delete." },
                }),
                json!(["memo_id", "confirm"]),
            ),
        },
        {
            "name": "delete_recording",
            "description": "Delete a stream recording by id. Destructive: requires `confirm: true`.",
            "inputSchema": obj(
                json!({
                    "recording_id": { "type": "integer", "description": "Recording id." },
                    "confirm": { "type": "boolean", "description": "Must be true to actually delete." },
                }),
                json!(["recording_id", "confirm"]),
            ),
        },
        {
            "name": "set_album_cover",
            "description": "Set an album's cover image from a local file path.",
            "inputSchema": obj(
                json!({
                    "artist": { "type": "string", "description": "Album artist." },
                    "album": { "type": "string", "description": "Album name." },
                    "path": { "type": "string", "description": "Image file path." },
                }),
                json!(["artist", "album", "path"]),
            ),
        },
        {
            "name": "set_artist_image",
            "description": "Set an artist's photo from a local file path.",
            "inputSchema": obj(
                json!({
                    "name": { "type": "string", "description": "Artist name." },
                    "path": { "type": "string", "description": "Image file path." },
                }),
                json!(["name", "path"]),
            ),
        },
        {
            "name": "list_artist_image_candidates",
            "description": "Find additional photo candidates for an artist on fanart.tv (matched via MusicBrainz). Returns a list of image URLs to choose from — does not download or set them. Requires a configured fanart.tv API key.",
            "inputSchema": obj(
                json!({
                    "artist": { "type": "string", "description": "Artist name." },
                    "limit": { "type": "integer", "description": "Max candidates to return (default 5, max 8).", "minimum": 1, "maximum": 8 },
                }),
                json!(["artist"]),
            ),
        },
        {
            "name": "enrich_artist_images",
            "description": "Fetch an artist's photo gallery from fanart.tv (matched via MusicBrainz) and store it as the artist's image gallery on the detail view, replacing any previously fetched gallery. Returns how many images were added. Requires a configured fanart.tv API key. Use list_artist_image_candidates first if the user should preview the photos before saving.",
            "inputSchema": obj(
                json!({
                    "artist": { "type": "string", "description": "Artist name." },
                }),
                json!(["artist"]),
            ),
        },
        {
            "name": "set_properties",
            "description": "Set the library areas an item appears in (its properties). `scope` ∈ {track, album, artist}; `key` is the track path, the artist\\u0001album key, or the artist name. `areas` is the list of areas to show it in (from: filesystem, artists, albums, singles, compilations, concerts, audiobooks; singles/compilations apply to albums); an empty list hides it.",
            "inputSchema": obj(
                json!({
                    "scope": { "type": "string", "enum": ["track", "album", "artist"] },
                    "key": { "type": "string", "description": "Item key (path | artist\\u0001album | artist name)." },
                    "areas": { "type": "array", "items": { "type": "string", "enum": ["filesystem", "artists", "albums", "singles", "compilations", "concerts", "audiobooks"] } },
                }),
                json!(["scope", "key", "areas"]),
            ),
        },
        {
            "name": "set_album_kind",
            "description": "Override an album's classification as 'single', 'compilation' or 'album' (or 'auto' to revert to the heuristic). Matches by album name (case-insensitive); affects list_albums with `kind`.",
            "inputSchema": obj(
                json!({
                    "album": { "type": "string", "description": "Album name." },
                    "kind": { "type": "string", "enum": ["album", "single", "compilation", "auto"] },
                }),
                json!(["album", "kind"]),
            ),
        },
        {
            "name": "search_youtube",
            "description": "Search YouTube online via yt-dlp for videos (default), playlists or channels. Returns id/url/title/uploader/duration — use play_youtube with an id to play one. Network call; may take a few seconds.",
            "inputSchema": obj(
                json!({
                    "query": { "type": "string", "description": "Search text." },
                    "kind": { "type": "string", "enum": ["video", "playlist", "channel"], "description": "What to search for (default video)." },
                    "limit": { "type": "integer", "description": "Max results (default 15).", "minimum": 1, "maximum": 50 },
                }),
                json!(["query"]),
            ),
        },
        {
            "name": "search_podcasts",
            "description": "Search for podcasts online (iTunes directory) by name. Returns title/author/feed_url. Network call.",
            "inputSchema": obj(
                json!({
                    "query": { "type": "string", "description": "Podcast name or keywords." },
                    "limit": { "type": "integer", "description": "Max results (default 25).", "minimum": 1, "maximum": 50 },
                }),
                json!(["query"]),
            ),
        },
        {
            "name": "download_youtube",
            "description": "Download a YouTube video (by id or watch URL) into the music library (transcode to mp3, tag, index). Long-running: returns a `job_id` immediately; poll `list_jobs` for progress.",
            "inputSchema": obj(
                json!({ "id": { "type": "string", "description": "YouTube video id or watch URL." } }),
                json!(["id"]),
            ),
        },
        {
            "name": "download_episode",
            "description": "Download a podcast episode for offline playback (by its audio URL, from list_episodes). Long-running: returns a `job_id`; poll `list_jobs`.",
            "inputSchema": obj(
                json!({ "url": { "type": "string", "description": "Episode audio URL." } }),
                json!(["url"]),
            ),
        },
        {
            "name": "list_jobs",
            "description": "List background download jobs and their state (running / done / error), newest first.",
            "inputSchema": empty(),
        },
        // --- podcasts: subscriptions ---
        {
            "name": "subscribe_podcast",
            "description": "Subscribe to a podcast by its RSS feed URL (from search_podcasts, or any feed URL): fetches the feed, stores the show and its episodes. Re-subscribing an existing feed refreshes it. Network call.",
            "inputSchema": obj(
                json!({ "feed_url": { "type": "string", "description": "RSS feed URL (the `feed_url` field from search_podcasts)." } }),
                json!(["feed_url"]),
            ),
        },
        {
            "name": "unsubscribe_podcast",
            "description": "Remove a podcast subscription and its episode list (by `podcast_id` from list_podcasts). Destructive: requires `confirm: true`.",
            "inputSchema": obj(
                json!({
                    "podcast_id": { "type": "integer", "description": "Podcast id." },
                    "confirm": { "type": "boolean", "description": "Must be true to actually remove." },
                }),
                json!(["podcast_id", "confirm"]),
            ),
        },
        {
            "name": "refresh_podcasts",
            "description": "Re-fetch every subscribed feed for new episodes (the podcasts page's refresh-all). Runs in the background; list_episodes reflects the result once done.",
            "inputSchema": empty(),
        },
        {
            "name": "delete_episode_download",
            "description": "Delete the offline download of a podcast episode (by its audio URL); the episode itself stays and can still be streamed. Destructive: requires `confirm: true`.",
            "inputSchema": obj(
                json!({
                    "url": { "type": "string", "description": "Episode audio URL (from list_episodes)." },
                    "confirm": { "type": "boolean", "description": "Must be true to actually delete." },
                }),
                json!(["url", "confirm"]),
            ),
        },
        // --- radio stations (live streams) ---
        {
            "name": "list_stations",
            "description": "List the saved internet-radio stations (live streams) with their id, stream URL, genre tags and country. Use the `id` with play_station / rename_station / delete_station.",
            "inputSchema": empty(),
        },
        {
            "name": "search_stations",
            "description": "Search the Radio Browser directory for internet-radio stations by name, genre or country. Returns name/url/tags/country/codec/bitrate — pass a hit's fields to add_station to save it. Network call.",
            "inputSchema": obj(
                json!({
                    "query": { "type": "string", "description": "Station name, genre or country." },
                    "limit": { "type": "integer", "description": "Max results (default 15).", "minimum": 1, "maximum": 50 },
                }),
                json!(["query"]),
            ),
        },
        {
            "name": "add_station",
            "description": "Save an internet-radio station (live stream) by its http(s) stream URL. `name` defaults to a name derived from the URL; the optional fields are usually copied from a search_stations hit. Saving an already-saved URL updates that station.",
            "inputSchema": obj(
                json!({
                    "url": { "type": "string", "description": "Stream URL (http/https)." },
                    "name": { "type": "string", "description": "Display name (optional)." },
                    "favicon": { "type": "string", "description": "Logo URL (optional)." },
                    "tags": { "type": "string", "description": "Comma-separated genre tags (optional)." },
                    "country": { "type": "string", "description": "Country (optional)." },
                    "codec": { "type": "string", "description": "Codec, e.g. MP3/AAC (optional)." },
                    "bitrate": { "type": "integer", "description": "Bitrate in kbit/s (optional)." },
                }),
                json!(["url"]),
            ),
        },
        {
            "name": "rename_station",
            "description": "Rename a saved station.",
            "inputSchema": obj(
                json!({
                    "station_id": { "type": "integer", "description": "Station id (from list_stations)." },
                    "name": { "type": "string", "description": "New name." },
                }),
                json!(["station_id", "name"]),
            ),
        },
        {
            "name": "delete_station",
            "description": "Remove a saved station (stops it first if it is playing). Its recordings are kept. Destructive: requires `confirm: true`.",
            "inputSchema": obj(
                json!({
                    "station_id": { "type": "integer", "description": "Station id (from list_stations)." },
                    "confirm": { "type": "boolean", "description": "Must be true to actually remove." },
                }),
                json!(["station_id", "confirm"]),
            ),
        },
        {
            "name": "play_station",
            "description": "Start a saved internet-radio station (live stream) by its id. Use playback_control to pause/resume it afterwards.",
            "inputSchema": obj(
                json!({ "station_id": { "type": "integer", "description": "Station id (from list_stations)." } }),
                json!(["station_id"]),
            ),
        },
        {
            "name": "toggle_station_recording",
            "description": "Start or stop the timeshift recording of a station (the station's record button). Needs the recording buffer enabled in the app settings; recorded songs appear in list_memos-like recordings and can be removed with delete_recording.",
            "inputSchema": obj(
                json!({ "station_id": { "type": "integer", "description": "Station id (from list_stations)." } }),
                json!(["station_id"]),
            ),
        },
        // --- library: tag editing + deletion ---
        {
            "name": "set_track_tags",
            "description": "Edit the metadata tags of a local library track in the audio file itself, then re-index it. Only the passed fields change; an empty string clears a text tag, `0` clears a numeric one. Tracks on network sources are read-only.",
            "inputSchema": obj(
                json!({
                    "path": { "type": "string", "description": "Track path (from list_tracks/search_library)." },
                    "title": { "type": "string" },
                    "artist": { "type": "string" },
                    "album": { "type": "string" },
                    "album_artist": { "type": "string" },
                    "genre": { "type": "string" },
                    "year": { "type": "integer", "minimum": 0 },
                    "track_no": { "type": "integer", "minimum": 0 },
                    "disc_no": { "type": "integer", "minimum": 0 },
                }),
                json!(["path"]),
            ),
        },
        {
            "name": "delete_track",
            "description": "Move a local library track's audio file to the desktop trash and drop it from the library. Recoverable from the trash. Tracks on network sources cannot be deleted. Destructive: requires `confirm: true`.",
            "inputSchema": obj(
                json!({
                    "path": { "type": "string", "description": "Track path (from list_tracks/search_library)." },
                    "confirm": { "type": "boolean", "description": "Must be true to actually delete." },
                }),
                json!(["path", "confirm"]),
            ),
        },
        {
            "name": "delete_album",
            "description": "Move every local track of an album (exact artist + album) to the desktop trash and drop them from the library. Tracks on network sources are skipped and reported. Destructive: requires `confirm: true`.",
            "inputSchema": obj(
                json!({
                    "artist": { "type": "string", "description": "Album artist exactly as listed." },
                    "album": { "type": "string", "description": "Album name exactly as listed." },
                    "confirm": { "type": "boolean", "description": "Must be true to actually delete." },
                }),
                json!(["artist", "album", "confirm"]),
            ),
        },
        // --- sources + folder browsing ---
        {
            "name": "list_sources",
            "description": "List the music sources shown as tabs in Files: the primary music folder plus any extra local folders and network shares (Nextcloud/WebDAV, SMB, Google Drive). Credentials are never returned. Each carries a `root` usable with list_folder.",
            "inputSchema": empty(),
        },
        {
            "name": "list_folder",
            "description": "Browse the indexed library by folder, like the Files tabs: returns the immediate subfolders (with track counts) and the tracks directly in `path`. Defaults to the primary music folder; pass a `root` from list_sources or a folder `path` from a previous call to descend.",
            "inputSchema": obj(
                json!({
                    "path": { "type": "string", "description": "Folder path or source root (optional; default: the music folder)." },
                    "limit": { "type": "integer", "description": "Max tracks returned (default 200).", "minimum": 1, "maximum": 2000 },
                }),
                json!([]),
            ),
        },
        // --- device sync ---
        {
            "name": "sync_status",
            "description": "State of the device-sync connection: whether another device is paired (and its name), whether this device is offering a pairing code, the current flow phase, a pending incoming offer (what the peer wants to send: file counts, size, library data), transfer progress, and the last error.",
            "inputSchema": empty(),
        },
        {
            "name": "sync_start_server",
            "description": "Offer a device-sync connection: starts the pairing server and shows the QR code in the app; returns the pairing code (an emilia://pair URL) that the other device scans or pastes. The code expires after about two minutes. Poll sync_status until `connected` is true.",
            "inputSchema": empty(),
        },
        {
            "name": "sync_pair",
            "description": "Connect to another device that is offering a connection, by pasting its pairing code (the emilia://pair URL from its QR / sync_start_server). Waits a few seconds for the pairing to complete.",
            "inputSchema": obj(
                json!({ "code": { "type": "string", "description": "Pairing code (emilia://pair?…)." } }),
                json!(["code"]),
            ),
        },
        {
            "name": "sync_share",
            "description": "Send content to the paired device (requires `connected` in sync_status). Select what to share: artists, albums, track paths, playlists, podcasts, stations, memos, recordings, YouTube videos, or the whole library, plus optional library data (favorites, playlists, podcasts, EQ, categories). The offer is sent without a confirmation step; the other device still has to accept it. Poll sync_status for progress.",
            "inputSchema": obj(
                json!({
                    "artists": { "type": "array", "items": { "type": "string" }, "description": "Artist names (all their albums)." },
                    "albums": { "type": "array", "items": { "type": "object", "properties": { "artist": { "type": "string" }, "album": { "type": "string" } }, "required": ["artist", "album"] } },
                    "tracks": { "type": "array", "items": { "type": "string" }, "description": "Track paths." },
                    "playlist_ids": { "type": "array", "items": { "type": "integer" } },
                    "podcast_ids": { "type": "array", "items": { "type": "integer" } },
                    "station_ids": { "type": "array", "items": { "type": "integer" } },
                    "memo_ids": { "type": "array", "items": { "type": "integer" } },
                    "recording_ids": { "type": "array", "items": { "type": "integer" } },
                    "yt_videos": { "type": "array", "items": { "type": "string" }, "description": "YouTube video ids (only delivered when the peer has YouTube enabled)." },
                    "whole_library": { "type": "boolean" },
                    "audiobooks": { "type": "boolean", "description": "All audiobooks." },
                    "concerts": { "type": "boolean", "description": "All concerts." },
                    "include_metadata": { "type": "boolean", "description": "Send the covers/photos/years and area assignments of the shared music (default true)." },
                    "include_favorites": { "type": "boolean" },
                    "include_playlists": { "type": "boolean" },
                    "include_podcasts": { "type": "boolean", "description": "All podcast subscriptions." },
                    "include_eq": { "type": "boolean" },
                    "include_categories": { "type": "boolean" },
                }),
                json!([]),
            ),
        },
        {
            "name": "sync_respond",
            "description": "Answer the pending incoming offer shown in sync_status: `accept: true` takes what the review would pre-select (new files only — nothing is overwritten — plus the offered library data), `false` rejects everything.",
            "inputSchema": obj(
                json!({ "accept": { "type": "boolean", "description": "Accept (true) or reject (false) the offer." } }),
                json!(["accept"]),
            ),
        },
        {
            "name": "sync_disconnect",
            "description": "End the live device-sync pairing (or stop offering a pairing code).",
            "inputSchema": empty(),
        },
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::mcp::{state, McpContext};
    use std::sync::{Arc, Mutex};

    /// Build a context whose control sink records the commands it receives, so
    /// command-tools can be asserted without a running UI.
    fn ctx_recording() -> (McpContext, Arc<Mutex<Vec<McpCommand>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let sink = log.clone();
        let ctx = McpContext {
            now: state::new_handle(),
            control: Arc::new(move |c| sink.lock().unwrap().push(c)),
            jobs: Arc::new(crate::core::mcp::jobs::Jobs::default()),
            sync: state::new_sync_handle(),
        };
        (ctx, log)
    }

    #[test]
    fn playback_control_maps_to_command() {
        let (ctx, log) = ctx_recording();
        let out = dispatch(&ctx, "playback_control", &json!({ "action": "next" })).unwrap();
        assert_eq!(out, json!({ "ok": true }));
        assert_eq!(log.lock().unwrap().as_slice(), &[McpCommand::Next]);
    }

    #[test]
    fn seek_forwards_clamped_position() {
        let (ctx, log) = ctx_recording();
        dispatch(&ctx, "seek", &json!({ "position_ms": -5 })).unwrap();
        assert_eq!(log.lock().unwrap().as_slice(), &[McpCommand::Seek(0)]);
    }

    #[test]
    fn unknown_action_is_an_error() {
        let (ctx, _) = ctx_recording();
        assert!(dispatch(&ctx, "playback_control", &json!({ "action": "boom" })).is_err());
    }

    #[test]
    fn now_playing_reads_the_snapshot() {
        let (ctx, _) = ctx_recording();
        {
            let mut np = ctx.now.lock().unwrap();
            np.playing = true;
            np.title = Some("Song".into());
        }
        let out = dispatch(&ctx, "now_playing", &json!({})).unwrap();
        assert_eq!(out["playing"], json!(true));
        assert_eq!(out["title"], json!("Song"));
    }

    #[test]
    fn initialize_advertises_tools_capability() {
        let (ctx, _) = ctx_recording();
        let req: RpcRequest =
            serde_json::from_value(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" }))
                .unwrap();
        let resp = handle_rpc(&ctx, req).expect("initialize replies");
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["result"]["protocolVersion"], json!(MCP_PROTOCOL_VERSION));
        assert!(v["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn notification_gets_no_reply() {
        let (ctx, _) = ctx_recording();
        let req: RpcRequest = serde_json::from_value(
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
        )
        .unwrap();
        assert!(handle_rpc(&ctx, req).is_none());
    }

    #[test]
    fn tools_list_is_non_empty_and_well_formed() {
        let list = tool_list();
        let arr = list.as_array().expect("array");
        assert!(arr.len() >= 10);
        for t in arr {
            assert!(t["name"].is_string());
            assert!(t["inputSchema"]["type"] == json!("object"));
        }
    }

    #[test]
    fn tool_names_are_unique() {
        let list = tool_list();
        let names: Vec<&str> = list
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        let set: std::collections::HashSet<&str> = names.iter().copied().collect();
        assert_eq!(names.len(), set.len(), "duplicate tool name");
    }

    #[test]
    fn destructive_tools_require_confirm() {
        let (ctx, log) = ctx_recording();
        for (tool, args) in [
            ("delete_track", json!({ "path": "/x.mp3" })),
            ("delete_album", json!({ "artist": "A", "album": "B" })),
            ("delete_station", json!({ "station_id": 1 })),
            ("unsubscribe_podcast", json!({ "podcast_id": 1 })),
            (
                "delete_episode_download",
                json!({ "url": "http://x/e.mp3" }),
            ),
        ] {
            let err = dispatch(&ctx, tool, &args).unwrap_err().to_string();
            assert!(err.contains("confirm"), "{tool}: {err}");
        }
        assert!(log.lock().unwrap().is_empty());
    }

    #[test]
    fn add_station_and_subscribe_reject_non_http_urls() {
        let (ctx, log) = ctx_recording();
        assert!(dispatch(&ctx, "add_station", &json!({ "url": "ftp://x/y" })).is_err());
        assert!(dispatch(&ctx, "subscribe_podcast", &json!({ "feed_url": "x" })).is_err());
        assert!(log.lock().unwrap().is_empty());
    }

    #[test]
    fn set_track_tags_needs_a_field() {
        let (ctx, _) = ctx_recording();
        let err = dispatch(&ctx, "set_track_tags", &json!({ "path": "/x.mp3" }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("nothing to change"));
        let err = dispatch(
            &ctx,
            "set_track_tags",
            &json!({ "path": "/x.mp3", "year": -1 }),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("year"));
    }

    #[test]
    fn sync_status_reports_the_snapshot() {
        let (ctx, _) = ctx_recording();
        let out = dispatch(&ctx, "sync_status", &json!({})).unwrap();
        assert_eq!(out["connected"], json!(false));
        assert_eq!(out["phase"], json!("idle"));
        {
            let mut s = ctx.sync.lock().unwrap();
            s.connected = true;
            s.peer_name = Some("Phone".into());
            s.is_server = true;
            s.phase = "offer_pending".into();
            s.incoming_offer = Some(state::OfferSummary {
                from: "Phone".into(),
                files: 3,
                new_files: 2,
                ..Default::default()
            });
        }
        let out = dispatch(&ctx, "sync_status", &json!({})).unwrap();
        assert_eq!(out["connected"], json!(true));
        assert_eq!(out["peer_name"], json!("Phone"));
        assert_eq!(out["role"], json!("server"));
        assert_eq!(out["incoming_offer"]["new_files"], json!(2));
    }

    #[test]
    fn sync_share_needs_a_pairing_and_a_selection() {
        let (ctx, log) = ctx_recording();
        let err = dispatch(&ctx, "sync_share", &json!({ "artists": ["Nirvana"] }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("not paired"));
        ctx.sync.lock().unwrap().connected = true;
        let err = dispatch(&ctx, "sync_share", &json!({}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("nothing selected"));
        assert!(log.lock().unwrap().is_empty());
    }

    #[test]
    fn sync_share_maps_the_selection() {
        let (ctx, log) = ctx_recording();
        ctx.sync.lock().unwrap().connected = true;
        let out = dispatch(
            &ctx,
            "sync_share",
            &json!({
                "artists": ["Nirvana"],
                "albums": [{ "artist": "Beck", "album": "Odelay" }],
                "tracks": ["/m/a.mp3"],
                "station_ids": [4],
                "include_metadata": false,
                "include_favorites": true,
            }),
        )
        .unwrap();
        assert_eq!(out["ok"], json!(true));
        let expected = Selection {
            artists: vec!["Nirvana".into()],
            albums: vec![("Beck".into(), "Odelay".into())],
            song_paths: vec!["/m/a.mp3".into()],
            stations: vec![4],
            include_metadata: false,
            include_favorites: true,
            ..Default::default()
        };
        assert_eq!(
            log.lock().unwrap().as_slice(),
            &[McpCommand::SyncShare(Box::new(expected))]
        );
    }

    #[test]
    fn sync_respond_needs_a_pending_offer() {
        let (ctx, log) = ctx_recording();
        assert!(dispatch(&ctx, "sync_respond", &json!({ "accept": true })).is_err());
        ctx.sync.lock().unwrap().incoming_offer = Some(state::OfferSummary::default());
        dispatch(&ctx, "sync_respond", &json!({ "accept": false })).unwrap();
        assert_eq!(
            log.lock().unwrap().as_slice(),
            &[McpCommand::SyncRespond { accept: false }]
        );
    }

    #[test]
    fn sync_pair_rejects_a_bad_code() {
        let (ctx, log) = ctx_recording();
        let err = dispatch(&ctx, "sync_pair", &json!({ "code": "not a code" }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("pairing code"));
        assert!(log.lock().unwrap().is_empty());
    }

    #[test]
    fn sync_disconnect_needs_something_to_end() {
        let (ctx, log) = ctx_recording();
        assert!(dispatch(&ctx, "sync_disconnect", &json!({})).is_err());
        ctx.sync.lock().unwrap().listening = true;
        dispatch(&ctx, "sync_disconnect", &json!({})).unwrap();
        assert_eq!(
            log.lock().unwrap().as_slice(),
            &[McpCommand::SyncDisconnect]
        );
    }

    #[test]
    fn unknown_method_request_errors() {
        let (ctx, _) = ctx_recording();
        let req: RpcRequest =
            serde_json::from_value(json!({ "jsonrpc": "2.0", "id": 7, "method": "nope" })).unwrap();
        let resp = handle_rpc(&ctx, req).expect("error reply");
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["error"]["code"], json!(METHOD_NOT_FOUND));
    }
}
