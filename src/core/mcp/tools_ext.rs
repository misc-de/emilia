//! The second half of the MCP tool layer: YouTube subscriptions and live
//! streams, the favorites / audiobooks / concerts collections, stream
//! recordings and "recently heard", memo categories, the play queue and
//! transport modes, the equalizer and lyrics.
//!
//! Split out of [`super::tools`] so neither file (nor its `json!` registry
//! literal) keeps growing: [`tool_list_ext`] is appended to the advertised list
//! and [`dispatch_ext`] is consulted for every name the core dispatch does not
//! know. Same conventions as there — reads open their own [`Library`], actions
//! go through [`McpCommand`] so the running UI stays in sync, destructive
//! actions need `"confirm": true`.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use super::command::McpCommand;
use super::tools::{arg_bool, arg_i64, arg_str, fmt_hms, req_i64, req_str, require_confirm};
use super::McpContext;
use crate::core::category::Area;
use crate::core::db::Library;

/// The YouTube-backed tools of this module (gated like the core ones).
pub(crate) const YOUTUBE_TOOLS_EXT: [&str; 11] = [
    "list_youtube_channels",
    "list_channel_videos",
    "list_youtube_newest",
    "subscribe_youtube_channel",
    "unsubscribe_youtube_channel",
    "refresh_youtube_channels",
    "play_youtube_channel",
    "list_live_streams",
    "add_live_stream",
    "remove_live_stream",
    "play_live_stream",
];

/// Labels of the ten equalizer bands, in slider order.
const EQ_BANDS: [&str; 10] = [
    "29 Hz", "59 Hz", "119 Hz", "237 Hz", "474 Hz", "947 Hz", "1.9 kHz", "3.8 kHz", "7.5 kHz",
    "15 kHz",
];

/// Equalizer levels and what their `key` is.
const EQ_SCOPES: &str = "global (no key), artist (artist name), album (needs artist + album), \
     track (library path), stream (station id), podcast (podcast id), episode (episode audio URL)";

fn yt_video_id(raw: &str) -> String {
    crate::core::youtube::video_id_from_url(raw).unwrap_or_else(|| raw.trim().to_string())
}

fn channel_by_id(lib: &Library, id: i64) -> Result<(i64, String, String, Option<String>, i64)> {
    lib.channels()?
        .into_iter()
        .find(|c| c.0 == id)
        .ok_or_else(|| anyhow!("no subscribed channel with id {id} (see list_youtube_channels)"))
}

/// One favorites/area row as JSON, with the scope-specific fields spelled out.
fn entry_json(scope: &str, key: &str, title: &str, is_dir: bool) -> Value {
    let mut v = json!({ "scope": scope, "key": key, "title": title, "is_dir": is_dir });
    if scope == "album" {
        let mut parts = key.splitn(2, '\u{1}');
        v["artist"] = json!(parts.next().unwrap_or(""));
        v["album"] = json!(parts.next().unwrap_or(""));
    }
    v
}

fn area_list(lib: &Library, area: Area) -> Vec<Value> {
    lib.area_entries(area, true, false)
        .iter()
        .map(|(scope, key, title, is_dir)| entry_json(scope, key, title, *is_dir))
        .collect()
}

/// The `key` of a favorite/area entry from the tool arguments: either `key`
/// verbatim, or for albums `artist` + `album`.
fn entry_key(args: &Value, scope: &str) -> Result<String> {
    if let Some(k) = arg_str(args, "key") {
        return Ok(k.to_string());
    }
    if scope == "album" {
        let album = req_str(args, "album")?;
        let artist = arg_str(args, "artist").unwrap_or("");
        return Ok(crate::core::category::album_key(artist, album));
    }
    Err(anyhow!("missing required string argument 'key'"))
}

/// The equalizer key for a scope from the tool arguments.
fn eq_key(args: &Value, scope: &str) -> Result<String> {
    match scope {
        "global" => Ok(String::new()),
        "album" => Ok(crate::core::category::album_key(
            arg_str(args, "artist").unwrap_or(""),
            req_str(args, "album")?,
        )),
        "artist" | "track" | "stream" | "podcast" | "episode" => {
            if let Some(k) = arg_str(args, "key") {
                return Ok(k.to_string());
            }
            // Numeric ids (station / podcast) may come as numbers.
            arg_i64(args, "key")
                .map(|n| n.to_string())
                .ok_or_else(|| anyhow!("scope '{scope}' needs a `key` ({EQ_SCOPES})"))
        }
        other => Err(anyhow!("unknown scope '{other}' — use one of: {EQ_SCOPES}")),
    }
}

fn bands_json(bands: &[f64; 10]) -> Value {
    json!(EQ_BANDS
        .iter()
        .zip(bands)
        .map(|(f, g)| json!({ "band": f, "gain_db": g }))
        .collect::<Vec<_>>())
}

fn memo_status_json(np: &super::state::NowPlaying) -> Value {
    let started = np.memo_started_at;
    json!({
        "recording": np.memo_recording,
        "started_at": started,
        "elapsed_s": started.map(|t| crate::core::sync::now_unix() as i64 - t),
    })
}

fn saved_memo_json(m: &super::state::SavedMemo) -> Value {
    json!({
        "id": m.id,
        "title": m.title,
        "path": m.path,
        "category_id": m.category_id,
        "duration_ms": m.duration_ms,
        "duration": fmt_hms(m.duration_ms),
    })
}

/// Sends a memo command and waits (up to `timeout`) for the UI to report its
/// outcome through the snapshot's memo event counter.
fn memo_request(
    ctx: &McpContext,
    cmd: McpCommand,
    timeout: std::time::Duration,
) -> Result<super::state::NowPlaying> {
    let before = ctx.now.lock().unwrap_or_else(|e| e.into_inner()).memo_seq;
    (ctx.control)(cmd);
    let start = std::time::Instant::now();
    loop {
        let np = ctx.now.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if np.memo_seq != before {
            return match np.memo_error.clone() {
                Some(e) => Err(anyhow!("{e}")),
                None => Ok(np),
            };
        }
        if start.elapsed() >= timeout {
            return Err(anyhow!(
                "the app did not answer in time; check record_memo status"
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Runs one of this module's tools; `None` = not one of ours.
pub fn dispatch_ext(ctx: &McpContext, name: &str, args: &Value) -> Option<Result<Value>> {
    Some(match name {
        // --- YouTube: subscriptions ---------------------------------------------
        "list_youtube_channels" => (|| {
            let lib = Library::open()?;
            let items: Vec<Value> = lib
                .channels()?
                .into_iter()
                .map(|(id, title, url, _, videos)| {
                    json!({ "id": id, "title": title, "url": url, "videos": videos })
                })
                .collect();
            Ok(json!({ "channels": items }))
        })(),
        "list_channel_videos" => (|| {
            let id = req_i64(args, "channel_id")?;
            let limit = arg_i64(args, "limit").unwrap_or(30).clamp(1, 200) as usize;
            let lib = Library::open()?;
            let (_, title, ..) = channel_by_id(&lib, id)?;
            let items: Vec<Value> = lib
                .channel_videos(id)?
                .into_iter()
                .take(limit)
                .map(|v| {
                    json!({
                        "id": v.video_id,
                        "title": v.title,
                        "published": v.published,
                        "duration_s": v.duration,
                        "duration": fmt_hms(v.duration.unwrap_or(0) * 1000),
                    })
                })
                .collect();
            Ok(json!({ "channel": title, "videos": items }))
        })(),
        "list_youtube_newest" => (|| {
            let limit = arg_i64(args, "limit").unwrap_or(30).clamp(1, 150) as usize;
            let lib = Library::open()?;
            let mut all = lib.all_videos()?;
            // ISO-8601 dates sort chronologically as strings; undated last.
            all.sort_by(|a, b| b.published.cmp(&a.published));
            let items: Vec<Value> = all
                .into_iter()
                .take(limit)
                .map(|v| {
                    json!({
                        "id": v.video_id,
                        "title": v.title,
                        "channel": v.channel_title,
                        "published": v.published,
                        "duration_s": v.duration,
                    })
                })
                .collect();
            Ok(json!({ "videos": items }))
        })(),
        "subscribe_youtube_channel" => (|| {
            use crate::core::youtube;
            if !youtube::available() {
                return Err(anyhow!("yt-dlp is not available on this system"));
            }
            let url = req_str(args, "url")?.to_string();
            if !url.starts_with("https://") || !url.contains("youtube.com/") {
                return Err(anyhow!(
                    "`url` must be a YouTube channel URL (see search_youtube with kind=channel)"
                ));
            }
            let channel_id = arg_str(args, "channel_id")
                .map(str::to_string)
                .or_else(|| youtube::channel_id_from_url(&url))
                .unwrap_or_else(|| url.clone());
            let title = arg_str(args, "title")
                .map(str::to_string)
                .unwrap_or_else(|| channel_id.clone());
            let db_id = crate::ui::yt_channels::store_channel(&channel_id, &title, &url, None)
                .ok_or_else(|| anyhow!("could not store the subscription"))?;
            (ctx.control)(McpCommand::ReloadYoutube);
            // Filling the video cache is a yt-dlp run → background job.
            let jobs = ctx.jobs.clone();
            let job_id = jobs.start("youtube_channel", &title);
            let control = ctx.control.clone();
            let t = title.clone();
            std::thread::spawn(move || {
                crate::ui::yt_channels::fill_channel_videos(db_id, &channel_id, &t, &url, None);
                let n = Library::open()
                    .and_then(|l| l.channel_videos(db_id))
                    .map(|v| v.len())
                    .unwrap_or(0);
                control(McpCommand::ReloadYoutube);
                jobs.finish(job_id, Ok(format!("{n} videos cached")));
            });
            Ok(json!({ "ok": true, "id": db_id, "title": title, "job_id": job_id }))
        })(),
        "unsubscribe_youtube_channel" => (|| {
            require_confirm(args)?;
            let id = req_i64(args, "channel_id")?;
            let (_, title, ..) = channel_by_id(&Library::open()?, id)?;
            (ctx.control)(McpCommand::DeleteYoutubeChannel(id));
            Ok(json!({ "ok": true, "id": id, "title": title }))
        })(),
        "refresh_youtube_channels" => (|| {
            let id = arg_i64(args, "channel_id");
            if let Some(id) = id {
                channel_by_id(&Library::open()?, id)?;
            }
            (ctx.control)(McpCommand::RefreshYoutube(id));
            Ok(
                json!({ "ok": true, "note": "refreshing in the background; list_channel_videos shows the result" }),
            )
        })(),
        "play_youtube_channel" => (|| {
            let id = req_i64(args, "channel_id")?;
            let lib = Library::open()?;
            let (_, title, ..) = channel_by_id(&lib, id)?;
            if lib.channel_videos(id)?.is_empty() {
                return Err(anyhow!(
                    "no cached videos yet — run refresh_youtube_channels"
                ));
            }
            (ctx.control)(McpCommand::PlayYoutubeChannel(id));
            Ok(json!({ "ok": true, "channel": title }))
        })(),

        // --- YouTube: live streams ------------------------------------------------
        "list_live_streams" => (|| {
            let items: Vec<Value> = Library::open()?
                .live_streams()?
                .into_iter()
                .map(|l| json!({ "id": l.video_id, "title": l.title, "channel": l.channel }))
                .collect();
            Ok(json!({ "live_streams": items }))
        })(),
        "add_live_stream" => (|| {
            let id = yt_video_id(req_str(args, "id")?);
            let (title, channel) = match arg_str(args, "title") {
                Some(t) => (t.to_string(), arg_str(args, "channel").map(str::to_string)),
                // No title given → ask YouTube (also confirms the id exists).
                None => {
                    let meta = crate::core::youtube::video_meta(&id)?;
                    let title = crate::core::youtube::strip_live_timestamp(&meta.title);
                    (title, meta.uploader)
                }
            };
            let thumb = crate::core::youtube::thumbnail_url(&id);
            Library::open()?.add_live(&id, &title, channel.as_deref(), Some(&thumb))?;
            crate::core::online::cache_youtube_thumb(&thumb);
            (ctx.control)(McpCommand::ReloadYoutube);
            Ok(json!({ "ok": true, "id": id, "title": title }))
        })(),
        "remove_live_stream" => (|| {
            require_confirm(args)?;
            let id = yt_video_id(req_str(args, "id")?);
            let lib = Library::open()?;
            if !lib.live_streams()?.iter().any(|l| l.video_id == id) {
                return Err(anyhow!(
                    "no saved live stream '{id}' (see list_live_streams)"
                ));
            }
            lib.delete_live(&id)?;
            (ctx.control)(McpCommand::ReloadYoutube);
            Ok(json!({ "ok": true, "removed": id }))
        })(),
        "play_live_stream" => (|| {
            let id = yt_video_id(req_str(args, "id")?);
            let lib = Library::open()?;
            let title = match lib.live_streams()?.into_iter().find(|l| l.video_id == id) {
                Some(l) => l.title,
                None => arg_str(args, "title").unwrap_or("YouTube Live").to_string(),
            };
            (ctx.control)(McpCommand::PlayLive {
                video_id: id.clone(),
                title: title.clone(),
            });
            Ok(json!({ "ok": true, "id": id, "title": title }))
        })(),

        // --- favorites / audiobooks / concerts -----------------------------------
        "list_favorites" => (|| {
            let items: Vec<Value> = Library::open()?
                .favorites()?
                .iter()
                .map(|(scope, key, title, is_dir)| entry_json(scope, key, title, *is_dir))
                .collect();
            Ok(json!({ "favorites": items }))
        })(),
        "list_audiobooks" => {
            Library::open().map(|lib| json!({ "audiobooks": area_list(&lib, Area::Audiobooks) }))
        }
        "list_concerts" => {
            Library::open().map(|lib| json!({ "concerts": area_list(&lib, Area::Concerts) }))
        }
        "play_entry" => (|| {
            let scope = req_str(args, "scope")?;
            if !matches!(scope, "track" | "folder" | "album" | "artist") {
                return Err(anyhow!(
                    "scope must be one of: track, folder, album, artist"
                ));
            }
            let key = entry_key(args, scope)?;
            (ctx.control)(McpCommand::PlayEntry {
                scope: scope.to_string(),
                is_dir: scope == "folder",
                key,
            });
            Ok(json!({ "ok": true }))
        })(),

        // --- stream recordings + "recently heard" ----------------------------------
        "list_recordings" => (|| {
            let items: Vec<Value> = Library::open()?
                .recordings()?
                .into_iter()
                .map(|r| {
                    json!({
                        "id": r.id,
                        "path": r.path,
                        "artist": r.artist,
                        "title": r.title,
                        "station": r.station,
                        "recorded_at": r.recorded_at,
                        "duration_ms": r.duration_ms,
                        "duration": fmt_hms(r.duration_ms),
                        "incomplete": r.incomplete,
                    })
                })
                .collect();
            Ok(json!({ "recordings": items }))
        })(),
        "list_heard" => (|| {
            let limit = arg_i64(args, "limit").unwrap_or(50).clamp(1, 500) as usize;
            let items: Vec<Value> = Library::open()?
                .heard_songs()?
                .into_iter()
                .take(limit)
                .map(|h| {
                    json!({
                        "artist": h.artist,
                        "title": h.title,
                        "station": h.station,
                        "heard_at": h.heard_at,
                        "count": h.count,
                    })
                })
                .collect();
            Ok(json!({ "heard": items }))
        })(),

        // --- memo categories ---------------------------------------------------------
        "list_memo_categories" => (|| {
            let lib = Library::open()?;
            let memos = lib.memos()?;
            let count = |cat: Option<i64>| memos.iter().filter(|m| m.category_id == cat).count();
            let mut items = vec![json!({ "id": null, "name": "General", "memos": count(None) })];
            items.extend(lib.memo_categories()?.into_iter().map(|c| {
                json!({ "id": c.id, "name": c.name, "memos": count(Some(c.id)), "created_at": c.created_at })
            }));
            Ok(json!({ "categories": items }))
        })(),
        "create_memo_category" => (|| {
            let name = req_str(args, "name")?.trim().to_string();
            let id = Library::open()?.add_memo_category(&name)?;
            (ctx.control)(McpCommand::ReloadMemos);
            Ok(json!({ "ok": true, "id": id, "name": name }))
        })(),
        "rename_memo_category" => (|| {
            let id = req_i64(args, "category_id")?;
            let name = req_str(args, "name")?.trim().to_string();
            (ctx.control)(McpCommand::RenameMemoCategory { id, name });
            Ok(json!({ "ok": true }))
        })(),
        "delete_memo_category" => (|| {
            require_confirm(args)?;
            let id = req_i64(args, "category_id")?;
            let with_memos = arg_bool(args, "with_memos").unwrap_or(false);
            (ctx.control)(McpCommand::DeleteMemoCategory { id, with_memos });
            Ok(json!({ "ok": true, "memos_deleted": with_memos }))
        })(),
        "rename_memo" => (|| {
            let id = req_i64(args, "memo_id")?;
            let title = req_str(args, "title")?.trim().to_string();
            (ctx.control)(McpCommand::RenameMemo { id, title });
            Ok(json!({ "ok": true }))
        })(),
        "set_memo_category" => (|| {
            let id = req_i64(args, "memo_id")?;
            // Missing / null → "General".
            let category_id = arg_i64(args, "category_id");
            (ctx.control)(McpCommand::SetMemoCategory { id, category_id });
            Ok(json!({ "ok": true, "category_id": category_id }))
        })(),

        "record_memo" => (|| {
            let np = ctx.now.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let title = arg_str(args, "title").map(|t| t.trim().to_string());
            let category_id = arg_i64(args, "category_id");
            if let Some(id) = category_id {
                if !Library::open()?
                    .memo_categories()?
                    .iter()
                    .any(|c| c.id == id)
                {
                    return Err(anyhow!("no memo category {id} (see list_memo_categories)"));
                }
            }
            match req_str(args, "action")? {
                "status" => {
                    let mut v = memo_status_json(&np);
                    v["last_memo"] = np.last_memo.as_ref().map(saved_memo_json).into();
                    Ok(v)
                }
                "start" => {
                    if np.memo_recording {
                        return Err(anyhow!(
                            "a memo is already being recorded (action \"stop\" ends it)"
                        ));
                    }
                    let stop_after_s = match arg_i64(args, "duration_s") {
                        Some(s) if !(1..=3600).contains(&s) => {
                            return Err(anyhow!("duration_s must be 1…3600"))
                        }
                        s => s.map(|s| s as u32),
                    };
                    let np = memo_request(
                        ctx,
                        McpCommand::StartMemo {
                            stop_after_s,
                            title,
                            category_id,
                        },
                        std::time::Duration::from_secs(5),
                    )?;
                    let mut v = memo_status_json(&np);
                    v["ok"] = json!(true);
                    v["stops_after_s"] = json!(stop_after_s);
                    Ok(v)
                }
                "stop" => {
                    if !np.memo_recording {
                        return Err(anyhow!("no memo is being recorded"));
                    }
                    // Finalizing the file waits for the encoder to drain.
                    let np = memo_request(
                        ctx,
                        McpCommand::StopMemo { title, category_id },
                        std::time::Duration::from_secs(15),
                    )?;
                    let memo = np
                        .last_memo
                        .as_ref()
                        .map(saved_memo_json)
                        .ok_or_else(|| anyhow!("the recording was stopped but not saved"))?;
                    Ok(json!({ "ok": true, "memo": memo }))
                }
                other => Err(anyhow!("unknown action '{other}' (use start|stop|status)")),
            }
        })(),

        // --- queue + transport modes ---------------------------------------------------
        "get_queue" => {
            let np = ctx.now.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let limit = arg_i64(args, "limit").unwrap_or(100).clamp(1, 2000) as usize;
            // Show the upcoming part: from the running entry on.
            let upcoming: Vec<&String> = np.queue.iter().skip(np.queue_pos).take(limit).collect();
            Ok(json!({
                "queue_length": np.queue.len(),
                "position": np.queue_pos,
                "from_position": upcoming,
                "user_queue": np.user_queue,
                "shuffle": np.shuffle,
                "repeat": np.repeat,
                "playback_rate": np.playback_rate,
            }))
        }
        "clear_queue" => {
            (ctx.control)(McpCommand::ClearQueue);
            Ok(json!({ "ok": true }))
        }
        "set_shuffle" => arg_bool(args, "on")
            .ok_or_else(|| anyhow!("missing required boolean argument 'on'"))
            .map(|on| {
                (ctx.control)(McpCommand::SetShuffle(on));
                json!({ "ok": true, "shuffle": on })
            }),
        "set_repeat" => arg_bool(args, "on")
            .ok_or_else(|| anyhow!("missing required boolean argument 'on'"))
            .map(|on| {
                (ctx.control)(McpCommand::SetRepeat(on));
                json!({ "ok": true, "repeat": on })
            }),
        "set_playback_speed" => (|| {
            let rate = args
                .get("rate")
                .and_then(|v| v.as_f64())
                .ok_or_else(|| anyhow!("missing required number argument 'rate'"))?;
            if !(0.25..=2.0).contains(&rate) {
                return Err(anyhow!("rate must be between 0.25 and 2.0"));
            }
            if ctx.now.lock().unwrap_or_else(|e| e.into_inner()).is_live() {
                return Err(anyhow!("live streams always play at normal speed"));
            }
            // The app steps in quarters.
            let rate = (rate / 0.25).round() * 0.25;
            (ctx.control)(McpCommand::SetPlaybackRate(rate));
            Ok(json!({ "ok": true, "rate": rate }))
        })(),

        // --- equalizer ----------------------------------------------------------------
        "get_equalizer" => (|| {
            let lib = Library::open()?;
            let items: Vec<Value> = lib
                .all_eq_settings()?
                .into_iter()
                .map(|(output, scope, key, bands)| {
                    let enabled = lib.eq_enabled(&output, &scope, &key).unwrap_or(true);
                    let mut v = json!({
                        "output": if output.is_empty() { "default" } else { output.as_str() },
                        "scope": scope,
                        "key": key,
                        "enabled": enabled,
                        "bands": bands_json(&bands),
                    });
                    if scope == "album" {
                        let mut p = key.splitn(2, '\u{1}');
                        v["artist"] = json!(p.next().unwrap_or(""));
                        v["album"] = json!(p.next().unwrap_or(""));
                    }
                    v
                })
                .collect();
            Ok(json!({ "bands": EQ_BANDS, "settings": items }))
        })(),
        "set_equalizer" => (|| {
            let scope = req_str(args, "scope")?;
            let key = eq_key(args, scope)?;
            let bands = if arg_bool(args, "reset") == Some(true) {
                None
            } else {
                let list: Vec<f64> = args
                    .get("bands")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_f64()).collect())
                    .unwrap_or_default();
                let arr: [f64; 10] = list.try_into().map_err(|_| {
                    anyhow!("`bands` must be 10 numbers (dB, −12…12) for {EQ_BANDS:?}, or pass \"reset\": true")
                })?;
                Some(arr.map(|g| g.clamp(-12.0, 12.0)))
            };
            (ctx.control)(McpCommand::SetEqualizer {
                scope: scope.to_string(),
                key: key.clone(),
                bands,
            });
            Ok(json!({ "ok": true, "scope": scope, "key": key, "reset": bands.is_none() }))
        })(),

        // --- lyrics -------------------------------------------------------------------
        "get_lyrics" => (|| {
            let path = match arg_str(args, "path") {
                Some(p) => p.to_string(),
                None => {
                    let np = ctx.now.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    match (np.kind, np.id) {
                        (Some("track"), Some(p)) => p,
                        _ => return Err(anyhow!("no library track is playing; pass `path`")),
                    }
                }
            };
            let lib = Library::open()?;
            // Same preference as the lyrics view: cached synced lines, else the
            // file's own (plain) tag, else whatever the cache holds.
            let embedded = crate::core::webdav::parse_nc_path(&path)
                .is_none()
                .then(|| crate::core::scanner::read_lyrics(std::path::Path::new(&path)))
                .flatten();
            let lyrics = match (lib.get_cached_lyrics(&path), embedded) {
                (Some(c), _) if c.has_synced() => Some(c),
                (_, Some(text)) => Some(crate::core::lyrics::Lyrics::from_parts(Some(text), None)),
                (cached, None) => cached,
            };
            let Some(l) = lyrics else {
                return Ok(json!({ "path": path, "found": false }));
            };
            let synced: Vec<Value> = l
                .synced
                .iter()
                .map(|(ms, text)| json!({ "ms": ms, "text": text }))
                .collect();
            let plain = l.plain.clone().or_else(|| {
                (!l.synced.is_empty()).then(|| {
                    l.synced
                        .iter()
                        .map(|(_, t)| t.as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                })
            });
            Ok(json!({ "path": path, "found": true, "text": plain, "synced": synced }))
        })(),

        _ => return None,
    })
}

/// Descriptors of this module's tools, appended to the core list.
pub fn tool_list_ext() -> Vec<Value> {
    let obj = |props: Value, required: Value| json!({ "type": "object", "properties": props, "required": required });
    let empty = || obj(json!({}), json!([]));
    let confirm =
        || json!({ "type": "boolean", "description": "Must be true to proceed (destructive)." });
    let tool = |name: &str, description: &str, schema: Value| json!({ "name": name, "description": description, "inputSchema": schema });
    let id_arg = |d: &str| json!({ "type": "string", "description": d });
    vec![
        // YouTube: subscriptions
        tool("list_youtube_channels", "List the subscribed YouTube channels (id, title, url, cached video count).", empty()),
        tool("list_channel_videos", "List the cached videos of a subscribed channel, newest first.", obj(json!({
            "channel_id": { "type": "integer", "description": "Channel id from list_youtube_channels." },
            "limit": { "type": "integer", "minimum": 1, "maximum": 200 },
        }), json!(["channel_id"]))),
        tool("list_youtube_newest", "The newest videos across all subscribed channels (the YouTube page's \"Newest\" tab).", obj(json!({
            "limit": { "type": "integer", "minimum": 1, "maximum": 150 },
        }), json!([]))),
        tool("subscribe_youtube_channel", "Subscribe to a YouTube channel. Take `url`, `channel_id` and `title` from a search_youtube result with kind=channel. The channel's videos are fetched as a background job (list_jobs).", obj(json!({
            "url": { "type": "string", "description": "Channel URL." },
            "channel_id": { "type": "string", "description": "Channel id (UC…) or handle, from the search result." },
            "title": { "type": "string", "description": "Channel name." },
        }), json!(["url"]))),
        tool("unsubscribe_youtube_channel", "Remove a channel subscription (and its cached video list).", obj(json!({
            "channel_id": { "type": "integer" },
            "confirm": confirm(),
        }), json!(["channel_id", "confirm"]))),
        tool("refresh_youtube_channels", "Re-fetch the video lists of all subscribed channels, or of one (`channel_id`). Runs in the background.", obj(json!({
            "channel_id": { "type": "integer" },
        }), json!([]))),
        tool("play_youtube_channel", "Play a subscribed channel's cached videos as the queue.", obj(json!({
            "channel_id": { "type": "integer" },
        }), json!(["channel_id"]))),
        // YouTube: live streams
        tool("list_live_streams", "List the saved YouTube live streams (24/7 radio channels, the YouTube page's \"Live\" tab). They are only ever streamed, never downloaded.", empty()),
        tool("add_live_stream", "Save a YouTube live stream to the \"Live\" tab. Find streams with search_youtube kind=live.", obj(json!({
            "id": id_arg("Video id or watch URL of the live stream."),
            "title": { "type": "string", "description": "Optional; looked up on YouTube when missing." },
            "channel": { "type": "string" },
        }), json!(["id"]))),
        tool("remove_live_stream", "Remove a saved live stream from the \"Live\" tab.", obj(json!({
            "id": id_arg("Video id from list_live_streams."),
            "confirm": confirm(),
        }), json!(["id", "confirm"]))),
        tool("play_live_stream", "Play a YouTube live stream like a radio station (no queue, not seekable). Next/previous then step through the saved live streams.", obj(json!({
            "id": id_arg("Video id or watch URL (saved or not)."),
            "title": { "type": "string", "description": "Display title for a stream that is not saved." },
        }), json!(["id"]))),
        // Collections
        tool("list_favorites", "List the favorites in their manual order. Each has `scope` (track/folder/album/artist) and `key`; play one with play_entry.", empty()),
        tool("list_audiobooks", "List the entries of the Audiobooks section (albums, folders or tracks marked as audiobooks). Play one with play_entry.", empty()),
        tool("list_concerts", "List the entries of the Concerts section. Play one with play_entry.", empty()),
        tool("play_entry", "Play a favorite / audiobook / concert entry exactly as its row does (toggles pause if it is already the running one). For albums pass `key` from the list or `artist` + `album`.", obj(json!({
            "scope": { "type": "string", "enum": ["track", "folder", "album", "artist"] },
            "key": { "type": "string", "description": "Entry key from list_favorites / list_audiobooks / list_concerts (a path for track/folder, the name for artist)." },
            "artist": { "type": "string" },
            "album": { "type": "string" },
        }), json!(["scope"]))),
        // Streaming extras
        tool("list_recordings", "List the saved radio recordings (id for delete_recording, path for play_memo).", empty()),
        tool("list_heard", "The \"Recently heard\" list of songs recognized on radio stations, newest first.", obj(json!({
            "limit": { "type": "integer", "minimum": 1, "maximum": 500 },
        }), json!([]))),
        // Memo categories
        tool("record_memo", "Record a voice memo from the microphone. `start` begins recording (optionally stopping by itself after `duration_s`), `stop` ends it and returns the saved memo (id, path, duration), `status` tells whether one is running and shows the last saved memo. `title` / `category_id` name and file the memo when it is saved. The app shows the recording while it runs.", obj(json!({
            "action": { "type": "string", "enum": ["start", "stop", "status"] },
            "duration_s": { "type": "integer", "minimum": 1, "maximum": 3600, "description": "start only: stop and save automatically after this many seconds." },
            "title": { "type": "string", "description": "Memo title (default: date and time)." },
            "category_id": { "type": "integer", "description": "Category from list_memo_categories (default: General)." },
        }), json!(["action"]))),
        tool("list_memo_categories", "List the voice-memo categories with their memo counts. \"General\" (id null) holds memos without a category.", empty()),
        tool("create_memo_category", "Create a voice-memo category.", obj(json!({
            "name": { "type": "string" },
        }), json!(["name"]))),
        tool("rename_memo_category", "Rename a voice-memo category.", obj(json!({
            "category_id": { "type": "integer" },
            "name": { "type": "string" },
        }), json!(["category_id", "name"]))),
        tool("delete_memo_category", "Remove a voice-memo category. Its memos move to \"General\", or are deleted too with `with_memos`.", obj(json!({
            "category_id": { "type": "integer" },
            "with_memos": { "type": "boolean", "description": "Also delete the memos (files included)." },
            "confirm": confirm(),
        }), json!(["category_id", "confirm"]))),
        tool("rename_memo", "Rename a voice memo.", obj(json!({
            "memo_id": { "type": "integer" },
            "title": { "type": "string" },
        }), json!(["memo_id", "title"]))),
        tool("set_memo_category", "Move a voice memo to a category (omit `category_id` or pass null for \"General\").", obj(json!({
            "memo_id": { "type": "integer" },
            "category_id": { "type": ["integer", "null"] },
        }), json!(["memo_id"]))),
        // Queue + modes
        tool("get_queue", "The play queue from the running entry on, the explicitly enqueued tracks, and shuffle / repeat / playback speed.", obj(json!({
            "limit": { "type": "integer", "minimum": 1, "maximum": 2000 },
        }), json!([]))),
        tool("clear_queue", "Empty the play queue.", empty()),
        tool("set_shuffle", "Turn shuffle on or off.", obj(json!({ "on": { "type": "boolean" } }), json!(["on"]))),
        tool("set_repeat", "Turn repeat on or off.", obj(json!({ "on": { "type": "boolean" } }), json!(["on"]))),
        tool("set_playback_speed", "Set the playback speed (0.25–2.0, in steps of 0.25). Not for live streams.", obj(json!({
            "rate": { "type": "number", "minimum": 0.25, "maximum": 2.0 },
        }), json!(["rate"]))),
        // Equalizer
        tool("get_equalizer", "List every stored equalizer level (10 bands in dB, 29 Hz…15 kHz). Playback resolves track → album → artist → global (stations: stream → global; episodes: episode → podcast → global).", empty()),
        tool("set_equalizer", &format!("Save and apply an equalizer level for all outputs, or reset it with `reset` (it then inherits again). Levels: {EQ_SCOPES}."), obj(json!({
            "scope": { "type": "string", "enum": ["global", "artist", "album", "track", "stream", "podcast", "episode"] },
            "key": { "type": ["string", "integer"] },
            "artist": { "type": "string" },
            "album": { "type": "string" },
            "bands": { "type": "array", "items": { "type": "number" }, "minItems": 10, "maxItems": 10, "description": "Gains in dB (−12…12), low to high." },
            "reset": { "type": "boolean" },
        }), json!(["scope"]))),
        // Lyrics
        tool("get_lyrics", "Lyrics of a library track (default: the one playing), from the cache or the file's tags. Includes timed lines when synced lyrics exist.", obj(json!({
            "path": { "type": "string" },
        }), json!([]))),
    ]
}
