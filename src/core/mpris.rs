//! MPRIS bridge: control via lock screen and media keys
//! (`org.mpris.MediaPlayer2`) over D-Bus.
//!
//! Everything runs on the glib main loop (the `mpris-server` `Player` tasks are
//! started with `spawn_future_local`), so the desktop commands arrive directly
//! on the UI thread – no thread bridging needed. If no D-Bus is reachable
//! (e.g. headless), setup fails silently and all calls are no-ops.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::glib;
use mpris_server::{LoopStatus, Metadata, PlaybackStatus, Player, Time, TrackId};

/// Command from the desktop to the app. Delivered on the main thread.
#[derive(Debug, Clone, Copy)]
pub enum MprisCommand {
    PlayPause,
    Play,
    Pause,
    Next,
    Prev,
    Stop,
    Raise,
    /// Relative jump by microseconds (can be negative).
    SeekBy(i64),
    /// Jump to an absolute position in microseconds.
    SetPosition(i64),
    /// Desktop toggled shuffle.
    SetShuffle(bool),
    /// Desktop changed the loop/repeat status (the app only has whole-queue
    /// repeat, so this collapses to on/off).
    SetRepeat(bool),
}

/// The `Player` is built up asynchronously; until then (or when no D-Bus
/// is present) the slot stays empty and all calls have no effect.
type Slot = Rc<RefCell<Option<Rc<Player>>>>;

/// Handle on the running MPRIS service for updating the state.
#[derive(Clone)]
pub struct Mpris {
    player: Slot,
}

impl Mpris {
    /// Starts the MPRIS service on the glib main loop. `on_cmd` is called on
    /// the main thread for every desktop command (play/pause/next/…).
    pub fn start<F>(on_cmd: F) -> Self
    where
        F: Fn(MprisCommand) + 'static,
    {
        let slot: Slot = Rc::new(RefCell::new(None));
        // Log every command at the D-Bus boundary, then hand it to the app — so
        // "the key never arrived" (routing/BlueZ issue, nothing logged) can be
        // told apart from "arrived but nothing happened" (logged). Visible with
        // RUST_LOG=emilia=debug.
        let on_cmd: Rc<dyn Fn(MprisCommand)> = {
            let inner = on_cmd;
            Rc::new(move |cmd| {
                tracing::debug!("MPRIS command: {cmd:?}");
                inner(cmd);
            })
        };
        // Unique bus name per process so a second manually started build or a
        // stale instance cannot fight over the same MPRIS name.
        let suffix = format!("Emilia.instance{}", std::process::id());

        let slot_for_task = slot.clone();
        glib::spawn_future_local(async move {
            // (Re)build the service in a loop: if `run()` ever returns — most
            // often because the session D-Bus connection dropped across a
            // suspend/resume on mobile — log it and rebuild, so the lock screen
            // and media keys keep working without an app restart.
            loop {
                let player = match Player::builder(&suffix)
                    .identity("Emilia")
                    .desktop_entry("de.cais.Emilia")
                    .can_play(true)
                    .can_pause(true)
                    .can_go_next(true)
                    .can_go_previous(true)
                    .can_seek(true)
                    .can_control(true)
                    .can_raise(true)
                    .shuffle(false)
                    .loop_status(LoopStatus::None)
                    .build()
                    .await
                {
                    Ok(p) => p,
                    Err(e) => {
                        // No D-Bus at all (e.g. headless): give up quietly.
                        tracing::warn!("MPRIS unavailable: {e}");
                        return;
                    }
                };

                macro_rules! forward {
                    ($connect:ident, $cmd:expr) => {{
                        let cb = on_cmd.clone();
                        player.$connect(move |_| cb($cmd));
                    }};
                }
                forward!(connect_play_pause, MprisCommand::PlayPause);
                forward!(connect_play, MprisCommand::Play);
                forward!(connect_pause, MprisCommand::Pause);
                forward!(connect_next, MprisCommand::Next);
                forward!(connect_previous, MprisCommand::Prev);
                forward!(connect_stop, MprisCommand::Stop);
                forward!(connect_raise, MprisCommand::Raise);
                {
                    let cb = on_cmd.clone();
                    player.connect_seek(move |_, offset: Time| {
                        cb(MprisCommand::SeekBy(offset.as_micros()))
                    });
                }
                {
                    let cb = on_cmd.clone();
                    player.connect_set_position(move |_, _track: &TrackId, pos: Time| {
                        cb(MprisCommand::SetPosition(pos.as_micros()))
                    });
                }
                {
                    let cb = on_cmd.clone();
                    player.connect_set_shuffle(move |_, shuffle: bool| {
                        cb(MprisCommand::SetShuffle(shuffle))
                    });
                }
                {
                    let cb = on_cmd.clone();
                    player.connect_set_loop_status(move |_, status: LoopStatus| {
                        cb(MprisCommand::SetRepeat(status != LoopStatus::None))
                    });
                }

                // Start serving method calls, then publish the player so the app
                // can push state. `run()` returns a 'static future, so the borrow
                // ends here and the player can move into the slot.
                let run_fut = player.run();
                *slot_for_task.borrow_mut() = Some(Rc::new(player));
                tracing::debug!("MPRIS service ready");
                let _ = run_fut.await;
                // Service ended (D-Bus dropped) → clear the slot and rebuild after
                // a short delay.
                tracing::warn!("MPRIS service stopped; reconnecting");
                slot_for_task.borrow_mut().take();
                glib::timeout_future(std::time::Duration::from_secs(2)).await;
            }
        });

        Mpris { player: slot }
    }

    /// Sets the playback status (Playing/Paused).
    pub fn set_playing(&self, playing: bool) {
        self.set_status(if playing {
            PlaybackStatus::Playing
        } else {
            PlaybackStatus::Paused
        });
    }

    pub fn set_stopped(&self) {
        self.set_status(PlaybackStatus::Stopped);
    }

    fn set_status(&self, status: PlaybackStatus) {
        let Some(player) = self.player.borrow().clone() else {
            return;
        };
        glib::spawn_future_local(async move {
            let _ = player.set_playback_status(status).await;
        });
    }

    /// Updates the track metadata for the lock screen. `index` serves
    /// as a stable (session) track ID; `length_ms`/`art_path` are optional.
    pub fn set_metadata(
        &self,
        index: usize,
        title: &str,
        artist: Option<&str>,
        album: Option<&str>,
        length_ms: Option<i64>,
        art_path: Option<&str>,
    ) {
        let Some(player) = self.player.borrow().clone() else {
            return;
        };
        let mut b = Metadata::builder().title(title);
        if let Ok(tid) = TrackId::try_from(format!("/de/cais/Emilia/track/{index}")) {
            b = b.trackid(tid);
        }
        if let Some(a) = artist.filter(|s| !s.is_empty()) {
            b = b.artist([a]);
        }
        if let Some(al) = album.filter(|s| !s.is_empty()) {
            b = b.album(al);
        }
        if let Some(ms) = length_ms.filter(|&m| m > 0) {
            b = b.length(Time::from_millis(ms));
        }
        if let Some(uri) = art_path
            .filter(|s| !s.is_empty())
            .and_then(|p| glib::filename_to_uri(p, None).ok())
        {
            b = b.art_url(uri.to_string());
        }
        let metadata = b.build();
        glib::spawn_future_local(async move {
            let _ = player.set_metadata(metadata).await;
        });
    }

    /// Sets the current position (for clients' read queries). Synchronous and
    /// cheap – intended for regular updates.
    pub fn set_position(&self, pos_ms: i64) {
        if let Some(player) = self.player.borrow().as_ref() {
            player.set_position(Time::from_millis(pos_ms.max(0)));
        }
    }

    /// Reports a position jump (Seeked signal) after seeking.
    pub fn seeked(&self, pos_ms: i64) {
        let Some(player) = self.player.borrow().clone() else {
            return;
        };
        glib::spawn_future_local(async move {
            let _ = player.seeked(Time::from_millis(pos_ms.max(0))).await;
        });
    }

    /// Reflects the shuffle state to the desktop (lock screen toggle).
    pub fn set_shuffle(&self, shuffle: bool) {
        let Some(player) = self.player.borrow().clone() else {
            return;
        };
        glib::spawn_future_local(async move {
            let _ = player.set_shuffle(shuffle).await;
        });
    }

    /// Reflects the repeat state to the desktop. The app only has whole-queue
    /// repeat, so this maps to `Playlist`/`None`.
    pub fn set_repeat(&self, repeat: bool) {
        let Some(player) = self.player.borrow().clone() else {
            return;
        };
        let status = if repeat {
            LoopStatus::Playlist
        } else {
            LoopStatus::None
        };
        glib::spawn_future_local(async move {
            let _ = player.set_loop_status(status).await;
        });
    }
}
