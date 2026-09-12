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
#[derive(Debug, Clone)]
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
    /// Desktop moved the volume slider (0.0–1.0).
    SetVolume(f64),
    /// Desktop asked to play a URI (`OpenUri`) — a file manager's "open with",
    /// a browser handing over a stream. Only the advertised schemes arrive
    /// here in practice; the app decides what it can do with it.
    OpenUri(String),
}

/// The `Player` is built up asynchronously; until then (or when no D-Bus
/// is present) the slot stays empty and all calls have no effect.
type Slot = Rc<RefCell<Option<Rc<Player>>>>;

/// The metadata last published to the desktop. Kept around so a late-arriving
/// duration can be merged in: GStreamer only knows a track's length a moment
/// after playback starts, and for stations / streams the app never learns it
/// up front at all – without this, `mpris:length` would stay missing.
#[derive(Default)]
struct Current {
    index: usize,
    title: String,
    artist: Option<String>,
    album: Option<String>,
    length_ms: Option<i64>,
    art_uri: Option<String>,
    /// Counter behind `mpris:trackid`, bumped whenever the identifying fields
    /// change. Clients that spot track changes by ID (scrobblers) need it to
    /// move on every new song – the queue position alone stays 0 for stations,
    /// YouTube, podcasts and remote files.
    track_no: u64,
    /// Whether anything has been published yet (an empty title is legitimate).
    set: bool,
}

impl Current {
    /// Takes over a fresh set of metadata, bumping `track_no` whenever the
    /// identifying fields (queue position, title, artist, album) differ from
    /// what is currently published — that is what makes a track change visible
    /// to clients even where the queue position is always 0.
    fn update(
        &mut self,
        index: usize,
        title: &str,
        artist: Option<String>,
        album: Option<String>,
        length_ms: Option<i64>,
        art_uri: Option<String>,
    ) {
        let is_new = !self.set
            || self.index != index
            || self.title != title
            || self.artist != artist
            || self.album != album;
        *self = Current {
            index,
            title: title.to_owned(),
            artist,
            album,
            length_ms: length_ms.filter(|&ms| ms > 0),
            art_uri,
            track_no: self.track_no + u64::from(is_new),
            set: true,
        };
    }

    /// Merges a duration that only became known later. Returns whether the
    /// published metadata needs to be sent again.
    fn merge_length(&mut self, length_ms: i64) -> bool {
        if !self.set || length_ms <= 0 || self.length_ms == Some(length_ms) {
            return false;
        }
        self.length_ms = Some(length_ms);
        true
    }

    fn build(&self) -> Metadata {
        let mut b = Metadata::builder().title(&self.title);
        if let Ok(tid) = TrackId::try_from(format!("/de/cais/Emilia/track/{}", self.track_no)) {
            b = b.trackid(tid);
        }
        if let Some(a) = &self.artist {
            b = b.artist([a]);
        }
        if let Some(al) = &self.album {
            b = b.album(al);
        }
        if let Some(ms) = self.length_ms {
            b = b.length(Time::from_millis(ms));
        }
        if let Some(uri) = &self.art_uri {
            b = b.art_url(uri.clone());
        }
        b.build()
    }
}

/// Handle on the running MPRIS service for updating the state.
#[derive(Clone)]
pub struct Mpris {
    player: Slot,
    current: Rc<RefCell<Current>>,
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
                    .volume(1.0)
                    // What `OpenUri` accepts. Leaving these empty (the default)
                    // told clients the method was useless — and it was, until
                    // it got wired up below.
                    .supported_uri_schemes(["file", "http", "https"])
                    .supported_mime_types([
                        "audio/mpeg",
                        "audio/mp4",
                        "audio/aac",
                        "audio/flac",
                        "audio/ogg",
                        "audio/opus",
                        "audio/x-vorbis+ogg",
                        "audio/x-wav",
                        "audio/x-m4a",
                        "audio/x-ms-wma",
                        "audio/x-aiff",
                        "audio/x-matroska",
                        "audio/x-mpegurl",
                    ])
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
                {
                    let cb = on_cmd.clone();
                    player.connect_set_volume(move |_, vol: f64| cb(MprisCommand::SetVolume(vol)));
                }
                {
                    let cb = on_cmd.clone();
                    player.connect_open_uri(move |_, uri: &str| {
                        cb(MprisCommand::OpenUri(uri.to_owned()))
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

        Mpris {
            player: slot,
            current: Rc::new(RefCell::new(Current::default())),
        }
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

    /// Updates the track metadata for the lock screen. `index` (the queue
    /// position) takes part in identifying the track, but the published
    /// `mpris:trackid` is a counter of its own – see `Current::track_no`.
    /// `length_ms`/`art_path` are optional; a duration that only shows up later
    /// is merged in by `set_length`.
    pub fn set_metadata(
        &self,
        index: usize,
        title: &str,
        artist: Option<&str>,
        album: Option<&str>,
        length_ms: Option<i64>,
        art_path: Option<&str>,
    ) {
        let artist = artist.filter(|s| !s.is_empty()).map(str::to_owned);
        let album = album.filter(|s| !s.is_empty()).map(str::to_owned);
        let art_uri = art_path
            .filter(|s| !s.is_empty())
            .and_then(|p| glib::filename_to_uri(p, None).ok())
            .map(|uri| uri.to_string());
        let metadata = {
            let mut cur = self.current.borrow_mut();
            cur.update(index, title, artist, album, length_ms, art_uri);
            cur.build()
        };
        self.publish(metadata);
    }

    /// Merges a duration that only became known after playback started (the
    /// pipeline reports it a moment in, and the non-library sources have no
    /// duration at all when they start) into the published metadata. Cheap to
    /// call from the 1 s tick: it only republishes when the value actually
    /// changes.
    pub fn set_length(&self, length_ms: i64) {
        let metadata = {
            let mut cur = self.current.borrow_mut();
            if !cur.merge_length(length_ms) {
                return;
            }
            cur.build()
        };
        self.publish(metadata);
    }

    fn publish(&self, metadata: Metadata) {
        let Some(player) = self.player.borrow().clone() else {
            return;
        };
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

    /// Reflects the output volume to the desktop. The MPRIS setter only calls
    /// back – the property does not follow by itself – so every volume change,
    /// whether it came from the lock screen or from the app, is mirrored here.
    pub fn set_volume(&self, vol: f64) {
        let Some(player) = self.player.borrow().clone() else {
            return;
        };
        let vol = vol.clamp(0.0, 1.0);
        glib::spawn_future_local(async move {
            let _ = player.set_volume(vol).await;
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

#[cfg(test)]
mod tests {
    use super::Current;

    fn song(cur: &mut Current, index: usize, title: &str, length_ms: Option<i64>) {
        cur.update(index, title, None, None, length_ms, None);
    }

    /// The track ID only moves on a real track change — re-publishing the same
    /// track (the app updates its metadata more than once per song) must not
    /// look like a new one to a scrobbler.
    #[test]
    fn track_id_follows_track_changes_only() {
        let mut cur = Current::default();
        song(&mut cur, 0, "One", None);
        let first = cur.track_no;
        song(&mut cur, 0, "One", None);
        assert_eq!(cur.track_no, first, "identical metadata is the same track");
        // Same queue position, new title: what a station's ICY updates and
        // YouTube / podcasts / remote files look like (index stays 0).
        song(&mut cur, 0, "Two", None);
        assert_eq!(cur.track_no, first + 1);
        // Same title, new queue position: the same song twice in a queue.
        song(&mut cur, 1, "Two", None);
        assert_eq!(cur.track_no, first + 2);
    }

    #[test]
    fn length_is_merged_once_it_is_known() {
        let mut cur = Current::default();
        // Nothing published yet → nothing to merge into.
        assert!(!cur.merge_length(1000));
        song(&mut cur, 0, "Stream", None);
        assert!(cur.merge_length(180_000), "first duration is published");
        assert_eq!(cur.length_ms, Some(180_000));
        assert!(!cur.merge_length(180_000), "unchanged duration stays quiet");
        assert!(!cur.merge_length(0), "an unknown duration is not published");
        assert_eq!(cur.length_ms, Some(180_000));
        // A new track drops the old duration instead of carrying it over.
        song(&mut cur, 1, "Next", None);
        assert_eq!(cur.length_ms, None);
    }

    /// A zero/negative length from the library must not reach the desktop as
    /// `mpris:length: 0` — clients render that as a 0:00 track.
    #[test]
    fn zero_length_is_dropped() {
        let mut cur = Current::default();
        song(&mut cur, 0, "Untagged", Some(0));
        assert_eq!(cur.length_ms, None);
        song(&mut cur, 1, "Tagged", Some(4_000));
        assert_eq!(cur.length_ms, Some(4_000));
    }
}
