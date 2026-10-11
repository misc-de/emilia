//! Routing the player to a network renderer (see [`crate::core::cast`]).
//!
//! While a cast session runs, the transport methods of [`Player`] hand over
//! to this module instead of driving the GStreamer decks: loads go to the
//! renderer, pause/seek/volume are forwarded, position and duration come
//! from the session. The renderer's events are turned into the same app
//! callbacks the bus watch fires (track ended, error, ready, output lost),
//! so the app's queue logic runs unchanged. Gapless, crossfade, EQ and
//! speed are local-pipeline features and simply do not apply.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use gstreamer as gst;
use gstreamer::prelude::*;

use super::{Player, reopen_deck};
use crate::core::cast::{self, Device, Event, Meta, Session};

/// The app callbacks a renderer event maps to (shared with the bus watch).
pub(super) struct CastCallbacks {
    pub eos: Rc<dyn Fn()>,
    pub error: Rc<dyn Fn()>,
    pub lost: Rc<dyn Fn()>,
    pub ready: Rc<dyn Fn()>,
}

pub(super) struct CastRoute {
    session: Session,
    /// The player-side URI of the current item (to resume locally later).
    uri: Option<String>,
    /// Load waiting for the next idle tick, so the app's metadata update that
    /// follows the load call in the same handler reaches the renderer too.
    pending: Option<(String, i64, bool)>,
    /// The app wants audio (mirrors `wants_playing` of the local decks).
    wants: bool,
    poll: Option<gst::glib::SourceId>,
}

impl Drop for CastRoute {
    fn drop(&mut self) {
        if let Some(id) = self.poll.take() {
            id.remove();
        }
    }
}

fn meta_now() -> Meta {
    let np = cast::now_playing().unwrap_or_default();
    Meta {
        title: if np.title.is_empty() {
            "Emilia".into()
        } else {
            np.title
        },
        artist: np.artist,
        album: np.album,
        art_path: np.art_path,
        duration_ms: np.length_ms.filter(|&d| d > 0),
    }
}

impl Player {
    pub(super) fn cast_active(&self) -> bool {
        self.cast.borrow().is_some()
    }

    /// The renderer playback runs on, if any.
    pub fn cast_device(&self) -> Option<Device> {
        self.cast
            .borrow()
            .as_ref()
            .map(|r| r.session.device().clone())
    }

    /// Renderer volume 0.0–1.0 (as last read / set).
    pub fn cast_volume(&self) -> Option<f64> {
        self.cast.borrow().as_ref()?.session.volume()
    }

    pub fn set_cast_volume(&self, vol: f64) {
        if let Some(r) = self.cast.borrow().as_ref() {
            r.session.set_volume(vol);
        }
    }

    /// Moves playback to `device`: the local decks stop, and whatever was
    /// loaded continues on the renderer at its position (or waits there,
    /// paused, until the next resume).
    pub fn start_cast(&self, device: Device) {
        // Switching from one renderer to another: stop the old one first.
        let carried = if let Some(old) = self.cast.borrow_mut().take() {
            let pos = old.session.position_ms();
            old.session.stop();
            old.uri.clone().map(|u| (u, pos, old.wants))
        } else {
            let uri = self
                .cur()
                .property::<Option<String>>("uri")
                .filter(|u| !u.is_empty() && self.cur().current_state() != gst::State::Null);
            let pos = self.position_ms().unwrap_or(0);
            let wants = self.wants_playing.get();
            uri.map(|u| (u, pos, wants))
        };
        // Silence the local side, keeping nothing armed behind our back.
        self.recovery.cancel();
        self.net.cancel();
        self.cancel_crossfade();
        self.arm_next_gapless(None);
        self.wants_playing.set(false);
        let _ = self.cur().set_state(gst::State::Null);

        tracing::info!("cast: playing on {} ({})", device.name, device.host);
        let session = Session::start(device);
        let mut route = CastRoute {
            session,
            uri: None,
            pending: None,
            wants: false,
            poll: None,
        };
        if let Some((uri, pos, play)) = carried {
            route.uri = Some(uri.clone());
            route.wants = play;
            route.session.load(&uri, pos, meta_now(), play);
        }
        route.poll = Some(self.start_cast_poll());
        *self.cast.borrow_mut() = Some(route);
    }

    /// Ends the cast session and continues locally where the renderer was.
    pub fn stop_cast(&self) {
        let Some(route) = self.cast.borrow_mut().take() else {
            return;
        };
        let pos = route.session.position_ms();
        route.session.stop();
        let (uri, play) = (route.uri.clone(), route.wants);
        drop(route);
        self.resume_locally(uri, pos, play);
    }

    /// Puts `uri` back on the active local deck at `pos`, playing or parked.
    fn resume_locally(&self, uri: Option<String>, pos: i64, play: bool) {
        let Some(uri) = uri else { return };
        let deck = self.cur_deck();
        let _ = deck.bin.set_state(gst::State::Null);
        deck.set_ramp(1.0, self.gain());
        deck.bin.set_property("uri", uri.as_str());
        self.last_pos_ms.set(pos.max(0));
        self.wants_playing.set(play);
        if play {
            reopen_deck(
                &deck.bin,
                pos,
                true,
                &deck.fresh_load,
                &deck.pending_seek_ms,
            );
        } else {
            // Parked: `resume()` sees `pending_seek > 0` on a Null deck and
            // starts it at the position.
            deck.fresh_load.set(true);
            deck.pending_seek_ms.set(pos.max(1));
        }
    }

    /// Drains the session's events on the main loop and fires the app
    /// callbacks.
    fn start_cast_poll(&self) -> gst::glib::SourceId {
        let cast = self.cast.clone();
        let cbs = self.cast_cbs.clone();
        let wants_playing = self.wants_playing.clone();
        let decks: Vec<_> = self
            .decks
            .iter()
            .map(|d| {
                (
                    d.bin.clone(),
                    d.fresh_load.clone(),
                    d.pending_seek_ms.clone(),
                )
            })
            .collect();
        let active = self.active.clone();
        let last_pos = self.last_pos_ms.clone();
        gst::glib::timeout_add_local(Duration::from_millis(300), move || {
            let (events, dead) = {
                let guard = cast.borrow();
                let Some(route) = guard.as_ref() else {
                    return gst::glib::ControlFlow::Break;
                };
                (route.session.take_events(), route.session.is_dead())
            };
            let cbs = cbs.borrow();
            for ev in events {
                match ev {
                    Event::Ready => {
                        if let Some(c) = cbs.as_ref() {
                            (c.ready)();
                        }
                    }
                    Event::Ended => {
                        if let Some(r) = cast.borrow_mut().as_mut() {
                            r.wants = false;
                        }
                        if let Some(c) = cbs.as_ref() {
                            (c.eos)();
                        }
                    }
                    Event::Error(e) => {
                        tracing::warn!("cast: {e}");
                        if let Some(r) = cast.borrow_mut().as_mut() {
                            r.wants = false;
                        }
                        if let Some(c) = cbs.as_ref() {
                            (c.error)();
                        }
                    }
                    Event::Lost => {
                        // Taken over or gone: end the session (without
                        // stopping the renderer — it is someone else's now)
                        // and park the item locally, paused.
                        let Some(mut route) = cast.borrow_mut().take() else {
                            break;
                        };
                        let pos = route.session.position_ms();
                        let uri = route.uri.take();
                        // The poll source is this closure: forget its id
                        // instead of removing it from inside itself.
                        route.poll = None;
                        drop(route);
                        tracing::info!("cast: session lost (dead: {dead})");
                        let idx = active.load(Ordering::Relaxed);
                        let (bin, fresh, pending) = &decks[idx];
                        if let Some(uri) = uri {
                            let _ = bin.set_state(gst::State::Null);
                            bin.set_property("uri", uri.as_str());
                            fresh.set(true);
                            pending.set(pos.max(1));
                            last_pos.set(pos.max(0));
                        }
                        wants_playing.set(false);
                        if let Some(c) = cbs.as_ref() {
                            (c.lost)();
                        }
                        return gst::glib::ControlFlow::Break;
                    }
                }
            }
            gst::glib::ControlFlow::Continue
        })
    }

    // --- Transport hand-over (called from the `Player` methods) -------------

    pub(super) fn cast_load(&self, uri: &str, resume_ms: i64) -> anyhow::Result<()> {
        let mut guard = self.cast.borrow_mut();
        let Some(route) = guard.as_mut() else {
            return Ok(());
        };
        route.uri = Some(uri.to_string());
        route.wants = true;
        let first = route.pending.is_none();
        route.pending = Some((uri.to_string(), resume_ms, true));
        if first {
            let cast = self.cast.clone();
            gst::glib::idle_add_local_once(move || {
                if let Some(r) = cast.borrow_mut().as_mut()
                    && let Some((uri, pos, play)) = r.pending.take()
                {
                    r.session.load(&uri, pos, meta_now(), play);
                }
            });
        }
        Ok(())
    }

    pub(super) fn cast_pause(&self) {
        if let Some(r) = self.cast.borrow_mut().as_mut() {
            r.wants = false;
            if let Some(p) = r.pending.as_mut() {
                p.2 = false;
            }
            r.session.pause();
        }
    }

    pub(super) fn cast_resume(&self) -> bool {
        let mut guard = self.cast.borrow_mut();
        let Some(r) = guard.as_mut() else {
            return false;
        };
        let Some(uri) = r.uri.clone() else {
            return false;
        };
        r.wants = true;
        if let Some(p) = r.pending.as_mut() {
            p.2 = true;
            return true;
        }
        match r.session.state() {
            // The renderer dropped the item (ended, stopped, never loaded
            // because the switch happened while paused): load it again.
            Some(cast::Transport::Stopped | cast::Transport::NoMedia) | None => {
                let pos = r.session.position_ms();
                r.session.load(&uri, pos, meta_now(), true);
            }
            _ => r.session.play(),
        }
        true
    }

    pub(super) fn cast_has_content(&self) -> bool {
        self.cast.borrow().as_ref().is_some_and(|r| r.uri.is_some())
    }

    pub(super) fn cast_stop(&self) {
        if let Some(r) = self.cast.borrow_mut().as_mut() {
            r.wants = false;
            r.uri = None;
            r.pending = None;
            r.session.stop();
        }
    }

    pub(super) fn cast_position(&self) -> Option<i64> {
        let guard = self.cast.borrow();
        let r = guard.as_ref()?;
        if let Some((_, pos, _)) = r.pending {
            return Some(pos.max(0));
        }
        r.uri.as_ref()?;
        Some(r.session.position_ms())
    }

    pub(super) fn cast_duration(&self) -> Option<i64> {
        let guard = self.cast.borrow();
        let r = guard.as_ref()?;
        r.session.duration_ms().filter(|&d| d > 0)
    }

    pub(super) fn cast_seek(&self, ms: i64) {
        let mut guard = self.cast.borrow_mut();
        let Some(r) = guard.as_mut() else { return };
        if let Some(p) = r.pending.as_mut() {
            p.1 = ms;
            return;
        }
        if r.session
            .state()
            .is_some_and(|s| matches!(s, cast::Transport::Stopped | cast::Transport::NoMedia))
            && let Some(uri) = r.uri.clone()
        {
            // Nothing on the renderer to seek in (loaded while paused):
            // remember the position for the next resume.
            r.session.load(&uri, ms, meta_now(), false);
            return;
        }
        r.session.seek(ms);
    }
}

/// Shared slot type of the route, so the poll closure and idle loads can
/// reach it.
pub(super) type CastSlot = Rc<RefCell<Option<CastRoute>>>;
pub(super) type CastCbSlot = Rc<RefCell<Option<CastCallbacks>>>;
