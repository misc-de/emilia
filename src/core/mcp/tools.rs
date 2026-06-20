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
use crate::model::Track;

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
        "tools/list" => Some(RpcResponse::ok(id, json!({ "tools": tool_list() }))),
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

// ---- tool execution ----------------------------------------------------------

/// Runs a single tool and returns its structured result (the value that the
/// backend wraps into an MCP `content` block). Errors surface to the caller as a
/// tool execution error.
pub fn dispatch(ctx: &McpContext, name: &str, args: &Value) -> Result<Value> {
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
            Ok(json!({
                "artists": r.artists,
                "albums": r.albums.iter().map(|a| json!({
                    "album": a.album, "artist": a.artist, "year": a.year,
                })).collect::<Vec<_>>(),
                "songs": r.songs.iter().map(|s| json!({
                    "path": s.path, "title": s.title, "artist": s.artist, "album": s.album,
                })).collect::<Vec<_>>(),
            }))
        }

        "list_artists" => {
            let lib = Library::open()?;
            Ok(json!({ "artists": lib.distinct_artists()? }))
        }

        "list_albums" => {
            let lib = Library::open()?;
            let albums: Vec<Value> = match arg_str(args, "artist") {
                Some(artist) => lib
                    .albums_of_artist(artist)?
                    .into_iter()
                    .map(|album| json!({ "artist": artist, "album": album }))
                    .collect(),
                None => {
                    // Distinct (artist, album) over the whole library.
                    let mut seen = std::collections::BTreeSet::new();
                    for t in lib.all_tracks()? {
                        if let Some(album) = t.album.filter(|s| !s.is_empty()) {
                            seen.insert((t.artist.unwrap_or_default(), album));
                        }
                    }
                    seen.into_iter()
                        .map(|(artist, album)| json!({ "artist": artist, "album": album }))
                        .collect()
                }
            };
            Ok(json!({ "albums": albums }))
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
            let t = lib.stats_totals(since)?;
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

        other => Err(anyhow!("unknown tool '{other}'")),
    }
}

// ---- tool registry (advertised to the client) --------------------------------

/// The list of tool descriptors returned by `tools/list`. Schemas are kept
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
            "description": "List all distinct artists in the library.",
            "inputSchema": empty(),
        },
        {
            "name": "list_albums",
            "description": "List albums; optionally only those of a given artist.",
            "inputSchema": obj(
                json!({ "artist": { "type": "string", "description": "Restrict to this artist (optional)." } }),
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
    fn unknown_method_request_errors() {
        let (ctx, _) = ctx_recording();
        let req: RpcRequest =
            serde_json::from_value(json!({ "jsonrpc": "2.0", "id": 7, "method": "nope" })).unwrap();
        let resp = handle_rpc(&ctx, req).expect("error reply");
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["error"]["code"], json!(METHOD_NOT_FOUND));
    }
}
