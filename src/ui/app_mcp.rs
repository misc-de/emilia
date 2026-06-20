//! App-side glue for the embedded MCP server: starting/stopping the backend and
//! translating the abstract [`McpCommand`]s it issues into player actions.
//!
//! The server runs on its own thread (see [`crate::core::mcp`]). Its control
//! sink posts `Msg::Mcp(..)` into the relm4 main loop via `self.input`; the
//! handler below then runs on the UI thread with full access to the player,
//! queue and library — the same entry points the buttons use.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::core::mcp::{self, McpCommand, McpContext, McpMode};
use crate::ui::app::{App, McpState, Msg, SleepChoice};
use crate::ui::app_memo::MemoMsg;
use crate::ui::app_playlist::PlaylistMsg;

/// MCP-server settings messages, dispatched by [`App::update_mcp_setting`].
/// Grouped out of the flat `Msg` enum (see `app.rs`); each persists a setting and
/// restarts the embedded server. (The server's own commands are `Msg::Mcp`.)
#[derive(Debug)]
pub(crate) enum McpSettingMsg {
    /// Change the MCP backend (off / JSON-RPC / SDK) → persist + restart.
    SetMode(crate::core::mcp::McpMode),
    /// Toggle LAN exposure of the MCP server → persist + restart.
    SetPublic(bool),
    /// Store a freshly generated MCP bearer token → persist + restart
    /// (existing connections drop).
    SetToken(String),
}

impl App {
    /// Dispatch an [`McpSettingMsg`]. Split out of the monolithic `App::update`.
    pub(crate) fn update_mcp_setting(&mut self, msg: McpSettingMsg) {
        match msg {
            McpSettingMsg::SetMode(mode) => {
                let _ = self.library.set_setting("mcp_mode", mode.as_setting());
                self.start_mcp_if_enabled();
            }
            McpSettingMsg::SetPublic(on) => {
                let _ = self
                    .library
                    .set_setting("mcp_public", if on { "1" } else { "0" });
                self.start_mcp_if_enabled();
            }
            McpSettingMsg::SetToken(token) => {
                let _ = self.library.set_secret_setting("mcp_token", &token);
                // Restart so the new token takes effect and existing connections drop.
                self.start_mcp_if_enabled();
            }
        }
    }

    /// Runs a single MCP command on the UI thread.
    pub(crate) fn handle_mcp(&mut self, cmd: McpCommand) {
        match cmd {
            // Idempotent play/pause: only toggle when the state actually differs.
            McpCommand::Play => {
                if !self.mini.playing && self.mini.now_playing.is_some() {
                    self.on_toggle_play();
                }
            }
            McpCommand::Pause => {
                if self.mini.playing {
                    self.on_toggle_play();
                }
            }
            McpCommand::TogglePlay => self.on_toggle_play(),
            McpCommand::Next => self.skip_next(),
            McpCommand::Prev => self.skip_prev(),
            // Re-dispatch through the normal message path (same as the seek bar).
            McpCommand::Seek(ms) => {
                let _ = self.input.send(Msg::Seek(ms));
            }
            McpCommand::PlayAlbum { artist, album } => self.on_play_album(artist, album),
            McpCommand::PlayArtist(name) => self.mcp_play_artist(name),
            McpCommand::PlayTrack(path) => self.on_play_one_track(path, false),
            McpCommand::PlayEpisode { url, title } => self.play_episode(&url, &title),
            McpCommand::PlayMemo(path) => self.play_recording(path),
            McpCommand::PlayYoutube { video_id, title } => self.yt_play_video(video_id, title),
            // Route through the existing messages so the DB change and the UI
            // refresh (reload_playlists) both happen, exactly as in the menu.
            McpCommand::PlayPlaylist { id, shuffle } => {
                let _ = self.input.send(if shuffle {
                    Msg::Playlist(PlaylistMsg::PlayShuffled(id))
                } else {
                    Msg::Playlist(PlaylistMsg::Play(id))
                });
            }
            McpCommand::RenamePlaylist { id, name } => {
                let _ = self
                    .input
                    .send(Msg::Playlist(PlaylistMsg::Rename { id, name }));
            }
            McpCommand::DeletePlaylist(id) => {
                let _ = self
                    .input
                    .send(Msg::Playlist(PlaylistMsg::DeleteConfirmed(id)));
            }
            McpCommand::SetPlaylistCover { id, path } => {
                let _ = self
                    .input
                    .send(Msg::Playlist(PlaylistMsg::SetCover { id, path }));
            }
            McpCommand::Enqueue(paths) => self.mcp_enqueue(paths),
            McpCommand::ToggleEpisodeListened { url, title } => {
                let _ = self.input.send(Msg::ToggleEpisode { url, title });
            }
            McpCommand::DeleteMemo(id) => {
                let _ = self.input.send(Msg::Memo(MemoMsg::DeleteConfirmed(id)));
            }
            McpCommand::DeleteRecording(id) => {
                let _ = self.input.send(Msg::StreamRecordingReallyDelete(id));
            }
            McpCommand::SetAlbumCover {
                artist,
                album,
                path,
            } => {
                let _ = self.input.send(Msg::SetAlbumCover {
                    artist,
                    album,
                    path,
                });
            }
            McpCommand::SetArtistImage { name, path } => {
                let _ = self.input.send(Msg::SetArtistImage { name, path });
            }
            McpCommand::SetAreas { scope, key, value } => {
                // `Msg::SetAreas` needs a `&'static` scope; map the known ones.
                let scope: Option<&'static str> = match scope.as_str() {
                    "track" => Some("track"),
                    "album" => Some("album"),
                    "artist" => Some("artist"),
                    _ => None,
                };
                if let Some(scope) = scope {
                    let _ = self.input.send(Msg::SetAreas { scope, key, value });
                }
            }
            McpCommand::SetSleepTimer(minutes) => {
                let choice = if minutes == 0 {
                    SleepChoice::Off
                } else {
                    SleepChoice::Minutes(minutes as i64)
                };
                let _ = self.input.send(Msg::SetSleepTimer(choice));
            }
        }
        // Reflect any resulting playback change in the snapshot immediately.
        self.publish_now_playing();
    }

    /// Play all tracks of an artist (queue = every track in overview order),
    /// reusing the artist-track entry point so behaviour matches the UI.
    fn mcp_play_artist(&mut self, name: String) {
        let first = self
            .artist_albums(&name)
            .into_iter()
            .flat_map(|(_, tracks)| tracks)
            .map(|t| t.path)
            .next();
        if let Some(path) = first {
            self.on_play_artist_track(name, path, false);
        }
    }

    /// Appends tracks to the user queue (play next), mirroring the context-menu
    /// "Add to queue" action but addressed by path from MCP.
    fn mcp_enqueue(&mut self, paths: Vec<String>) {
        let mut paths: Vec<std::path::PathBuf> =
            paths.into_iter().map(std::path::PathBuf::from).collect();
        self.transport.user_queue.append(&mut paths);
        self.reload_queue_list();
        self.refresh_queue_icons();
        self.save_queue();
    }

    /// Copies the live mini-player state into the shared now-playing snapshot the
    /// server thread reads. Cheap; called from the tick and after commands.
    pub(crate) fn publish_now_playing(&self) {
        if let Ok(mut np) = self.mcp.now.lock() {
            np.playing = self.mini.playing;
            np.title = self.mini.now_playing.clone();
            np.album = self.mini.current_album.clone();
            np.position_ms = self.mini.position_ms;
            np.duration_ms = self.mini.track_duration_ms;
        }
    }

    /// Starts the configured MCP backend if `mcp_mode` is not `off`. Stops any
    /// server already running first, so it doubles as "restart with new settings".
    pub(crate) fn start_mcp_if_enabled(&mut self) {
        self.stop_mcp_server();

        let mode = self
            .library
            .get_setting("mcp_mode")
            .ok()
            .flatten()
            .map(|s| McpMode::from_setting(&s))
            .unwrap_or(McpMode::Off);
        if mode == McpMode::Off {
            return;
        }

        let public = self
            .library
            .get_setting("mcp_public")
            .ok()
            .flatten()
            .as_deref()
            == Some("1");
        let token = self.mcp_token();

        let input = self.input.clone();
        let control: mcp::ControlFn = Arc::new(move |cmd| {
            let _ = input.send(Msg::Mcp(cmd));
        });
        let ctx = Arc::new(McpContext {
            now: self.mcp.now.clone(),
            control,
            jobs: self.mcp.jobs.clone(),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let bind = if public { "0.0.0.0" } else { "127.0.0.1" };

        match mode {
            McpMode::JsonRpc => {
                match mcp::server_jsonrpc::JsonRpcServer::start(ctx, token, public, stop.clone()) {
                    Ok(server) => {
                        let port = server.port();
                        std::thread::spawn(move || server.run());
                        self.mcp.stop = Some(stop);
                        // Publish the actual port so it can be checked/matched.
                        let _ = self.library.set_setting("mcp_port", &port.to_string());
                        tracing::info!("MCP JSON-RPC server listening on {bind}:{port}");
                    }
                    Err(e) => tracing::error!("MCP server failed to start: {e}"),
                }
            }
            McpMode::Sdk => match mcp::server_sdk::start(ctx, token, public, stop.clone()) {
                Ok(port) => {
                    self.mcp.stop = Some(stop);
                    // Publish the actual port so it can be checked/matched.
                    let _ = self.library.set_setting("mcp_port", &port.to_string());
                    tracing::info!("MCP SDK (rmcp) server listening on {bind}:{port}");
                }
                Err(e) => tracing::error!("MCP SDK server failed to start: {e}"),
            },
            McpMode::Off => {}
        }
    }

    /// Stops a running MCP server (best effort; the thread exits on its next poll).
    pub(crate) fn stop_mcp_server(&mut self) {
        if let Some(stop) = self.mcp.stop.take() {
            stop.store(true, Ordering::Relaxed);
        }
    }

    /// The persisted bearer token, generated and stored on first use (Secret
    /// Service when available, like the other credentials).
    fn mcp_token(&self) -> String {
        if let Ok(Some(t)) = self.library.get_secret_setting("mcp_token") {
            if !t.is_empty() {
                return t;
            }
        }
        let token = crate::core::sync::crypto::generate_token(32);
        let _ = self.library.set_secret_setting("mcp_token", &token);
        token
    }
}

/// Constructs the initial (server-off) MCP state for the `App` literal.
impl McpState {
    pub(crate) fn new() -> Self {
        Self {
            now: mcp::new_handle(),
            jobs: std::sync::Arc::new(mcp::jobs::Jobs::default()),
            stop: None,
        }
    }
}
