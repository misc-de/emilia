//! Unit tests of the core MCP tool layer ([`super`]).

use super::*;
use crate::core::mcp::{McpContext, state};
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
    let req: RpcRequest =
        serde_json::from_value(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
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
fn delete_memo_category_requires_confirm() {
    let (ctx, log) = ctx_recording();
    let err = dispatch(&ctx, "delete_memo_category", &json!({ "category_id": 1 }))
        .unwrap_err()
        .to_string();
    assert!(err.contains("confirm"), "{err}");
    dispatch(
        &ctx,
        "delete_memo_category",
        &json!({ "category_id": 1, "with_memos": true, "confirm": true }),
    )
    .unwrap();
    assert_eq!(
        log.lock().unwrap().as_slice(),
        &[McpCommand::DeleteMemoCategory {
            id: 1,
            with_memos: true
        }]
    );
}

/// A context whose control sink answers memo commands the way the app does:
/// through the snapshot's memo event counter.
fn ctx_memo(mic_ok: bool) -> McpContext {
    let now = state::new_handle();
    let snap = now.clone();
    McpContext {
        now,
        control: Arc::new(move |c| {
            let mut np = snap.lock().unwrap();
            np.memo_seq += 1;
            match c {
                McpCommand::StartMemo { .. } if mic_ok => {
                    np.memo_recording = true;
                    np.memo_error = None;
                }
                McpCommand::StartMemo { .. } => {
                    np.memo_error = Some("microphone not available".into());
                }
                McpCommand::StopMemo { title, .. } => {
                    np.memo_recording = false;
                    np.memo_error = None;
                    np.last_memo = Some(state::SavedMemo {
                        id: 7,
                        title: title.unwrap_or_default(),
                        path: "/m/7.ogg".into(),
                        duration_ms: 3000,
                        category_id: None,
                    });
                }
                _ => {}
            }
        }),
        jobs: Arc::new(crate::core::mcp::jobs::Jobs::default()),
        sync: state::new_sync_handle(),
    }
}

#[test]
fn record_memo_starts_stops_and_reports_the_saved_memo() {
    let ctx = ctx_memo(true);
    assert!(dispatch(&ctx, "record_memo", &json!({ "action": "stop" })).is_err());
    assert!(
        dispatch(
            &ctx,
            "record_memo",
            &json!({ "action": "start", "duration_s": 0 })
        )
        .is_err()
    );
    let started = dispatch(&ctx, "record_memo", &json!({ "action": "start" })).unwrap();
    assert_eq!(started["recording"], json!(true));
    // A second start is refused while one runs.
    assert!(dispatch(&ctx, "record_memo", &json!({ "action": "start" })).is_err());
    let stopped = dispatch(
        &ctx,
        "record_memo",
        &json!({ "action": "stop", "title": "Einkauf" }),
    )
    .unwrap();
    assert_eq!(stopped["memo"]["id"], json!(7));
    assert_eq!(stopped["memo"]["title"], json!("Einkauf"));
    let status = dispatch(&ctx, "record_memo", &json!({ "action": "status" })).unwrap();
    assert_eq!(status["recording"], json!(false));
    assert_eq!(status["last_memo"]["id"], json!(7));
}

#[test]
fn record_memo_reports_a_missing_microphone() {
    let ctx = ctx_memo(false);
    let err = dispatch(&ctx, "record_memo", &json!({ "action": "start" }))
        .unwrap_err()
        .to_string();
    assert!(err.contains("microphone"), "{err}");
}

#[test]
fn live_items_refuse_seek_and_speed() {
    let (ctx, log) = ctx_recording();
    ctx.now.lock().unwrap().kind = Some("youtube_live");
    assert!(dispatch(&ctx, "seek", &json!({ "position_ms": 1000 })).is_err());
    assert!(dispatch(&ctx, "set_playback_speed", &json!({ "rate": 1.5 })).is_err());
    assert!(log.lock().unwrap().is_empty());
    let np = dispatch(&ctx, "now_playing", &json!({})).unwrap();
    assert_eq!(np["live"], json!(true));
    assert_eq!(np["kind"], json!("youtube_live"));
}

#[test]
fn transport_modes_map_to_commands() {
    let (ctx, log) = ctx_recording();
    dispatch(&ctx, "set_shuffle", &json!({ "on": true })).unwrap();
    dispatch(&ctx, "set_repeat", &json!({ "on": false })).unwrap();
    // Snapped to the app's quarter steps.
    let out = dispatch(&ctx, "set_playback_speed", &json!({ "rate": 1.3 })).unwrap();
    assert_eq!(out["rate"], json!(1.25));
    assert!(dispatch(&ctx, "set_playback_speed", &json!({ "rate": 3.0 })).is_err());
    dispatch(&ctx, "clear_queue", &json!({})).unwrap();
    assert_eq!(
        log.lock().unwrap().as_slice(),
        &[
            McpCommand::SetShuffle(true),
            McpCommand::SetRepeat(false),
            McpCommand::SetPlaybackRate(1.25),
            McpCommand::ClearQueue,
        ]
    );
}

#[test]
fn get_queue_shows_the_upcoming_part() {
    let (ctx, _) = ctx_recording();
    {
        let mut np = ctx.now.lock().unwrap();
        np.queue = vec!["/a".into(), "/b".into(), "/c".into()];
        np.queue_pos = 1;
        np.shuffle = true;
    }
    let q = dispatch(&ctx, "get_queue", &json!({})).unwrap();
    assert_eq!(q["queue_length"], json!(3));
    assert_eq!(q["from_position"], json!(["/b", "/c"]));
    assert_eq!(q["shuffle"], json!(true));
}

#[test]
fn set_equalizer_validates_and_builds_keys() {
    let (ctx, log) = ctx_recording();
    // Wrong band count / unknown scope / missing key are refused.
    assert!(
        dispatch(
            &ctx,
            "set_equalizer",
            &json!({ "scope": "global", "bands": [1, 2] })
        )
        .is_err()
    );
    assert!(
        dispatch(
            &ctx,
            "set_equalizer",
            &json!({ "scope": "room", "reset": true })
        )
        .is_err()
    );
    assert!(
        dispatch(
            &ctx,
            "set_equalizer",
            &json!({ "scope": "artist", "reset": true })
        )
        .is_err()
    );
    assert!(log.lock().unwrap().is_empty());
    let bands = json!([20, 0, 0, 0, 0, 0, 0, 0, 0, -3]);
    dispatch(
        &ctx,
        "set_equalizer",
        &json!({ "scope": "album", "artist": "A", "album": "B", "bands": bands }),
    )
    .unwrap();
    dispatch(
        &ctx,
        "set_equalizer",
        &json!({ "scope": "stream", "key": 7, "reset": true }),
    )
    .unwrap();
    let mut expected = [0.0; 10];
    expected[0] = 12.0; // clamped
    expected[9] = -3.0;
    assert_eq!(
        log.lock().unwrap().as_slice(),
        &[
            McpCommand::SetEqualizer {
                scope: "album".into(),
                key: "A\u{1}B".into(),
                bands: Some(expected),
            },
            McpCommand::SetEqualizer {
                scope: "stream".into(),
                key: "7".into(),
                bands: None,
            },
        ]
    );
}

#[test]
fn play_entry_builds_album_keys_and_checks_scope() {
    let (ctx, log) = ctx_recording();
    assert!(dispatch(&ctx, "play_entry", &json!({ "scope": "genre", "key": "x" })).is_err());
    dispatch(
        &ctx,
        "play_entry",
        &json!({ "scope": "album", "artist": "A", "album": "B" }),
    )
    .unwrap();
    dispatch(
        &ctx,
        "play_entry",
        &json!({ "scope": "folder", "key": "/m/x" }),
    )
    .unwrap();
    assert_eq!(
        log.lock().unwrap().as_slice(),
        &[
            McpCommand::PlayEntry {
                scope: "album".into(),
                key: "A\u{1}B".into(),
                is_dir: false,
            },
            McpCommand::PlayEntry {
                scope: "folder".into(),
                key: "/m/x".into(),
                is_dir: true,
            },
        ]
    );
}

#[test]
fn gated_youtube_tools_are_advertised() {
    let list = tool_list();
    let names: std::collections::HashSet<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for n in YOUTUBE_TOOLS
        .iter()
        .chain(super::super::tools_ext::YOUTUBE_TOOLS_EXT.iter())
    {
        assert!(names.contains(n), "gated tool {n} is not in the list");
    }
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
