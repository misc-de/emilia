//! GStreamer playback via two `playbin3` **decks**.
//!
//! A single deck is the normal path (streams, podcasts, YouTube, explicit
//! plays). The second deck enables two features for sequential **local** queues
//! (albums / concerts / audiobooks):
//!
//! * **Gapless** — the active deck's `about-to-finish` signal hands the next
//!   track's URI to the *same* `playbin3`, which concatenates the decoded
//!   streams seamlessly. The app learns of the switch from the `STREAM_START`
//!   bus message and advances its own state to match.
//! * **Crossfade** — app-driven: the next track starts on the *idle* deck at
//!   volume 0 and the two decks' volumes ramp over a configurable window before
//!   the outgoing deck stops and the idle deck becomes active.
//!
//! Everything that queries or controls "the player" targets the **active** deck.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Result};
use gstreamer as gst;
use gstreamer::prelude::*;

/// Whether a remote-supplied URI (station / podcast episode / WebDAV stream) may
/// be handed to `playbin`. Restricts to network streaming schemes so a hostile
/// feed or station entry can never make the player open a **local** resource
/// (`file://`, `cdda://`, `resource://` …). Local files go through `play_file`,
/// which builds the `file://` URI itself.
fn is_allowed_remote_uri(uri: &str) -> bool {
    let scheme = uri
        .split_once(':')
        .map(|(s, _)| s)
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(
        scheme.as_str(),
        "http" | "https" | "rtsp" | "rtmp" | "rtmps" | "mms" | "mmsh" | "mmst"
    )
}

/// `file://` URI for a local path, or `None` if it can't be represented (used to
/// arm the next gapless track from the app side).
pub fn file_uri(path: &str) -> Option<String> {
    gst::glib::filename_to_uri(path, None)
        .ok()
        .map(|g| g.to_string())
}

/// Combines the available audio-filter elements (scaletempo, equalizer) into a
/// single element for `playbin`'s `audio-filter` property. With both present a
/// `Bin` (scaletempo → equalizer) with ghost pads is returned; with only one,
/// that element; with none, `None`.
fn build_audio_filter(
    scaletempo: Option<&gst::Element>,
    equalizer: Option<&gst::Element>,
) -> Option<gst::Element> {
    match (scaletempo, equalizer) {
        (Some(st), Some(eq)) => {
            let bin = gst::Bin::new();
            bin.add(st).ok()?;
            bin.add(eq).ok()?;
            st.link(eq).ok()?;
            let sink = st.static_pad("sink")?;
            let src = eq.static_pad("src")?;
            bin.add_pad(&gst::GhostPad::with_target(&sink).ok()?).ok()?;
            bin.add_pad(&gst::GhostPad::with_target(&src).ok()?).ok()?;
            Some(bin.upcast())
        }
        (Some(st), None) => Some(st.clone()),
        (None, Some(eq)) => Some(eq.clone()),
        (None, None) => None,
    }
}

/// Performs a pitch-preserving rate-change seek to `pos` at `rate` (scaletempo
/// reacts to the new segment rate). Uses `KEY_UNIT` rather than `ACCURATE`: a
/// frame-exact (`ACCURATE`) flush-seek on a slow HTTP source (a podcast
/// episode) forces a re-download to find the precise sample and blocked the GTK
/// main thread for seconds — the UI appeared to freeze on every speed change.
/// Snapping to the nearest keyframe is effectively instant and the sub-second
/// position drift is inaudible.
fn rate_seek(playbin: &gst::Element, rate: f64, pos: gst::ClockTime) {
    let _ = playbin.seek(
        rate,
        gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
        gst::SeekType::Set,
        pos,
        gst::SeekType::End,
        gst::ClockTime::ZERO,
    );
}

/// A cheap, cloneable handle to query the live playback state from the UI
/// without going through the 1 s `Tick` — used by the recording editor to keep
/// its timeline and waveform playhead in sync with the audio. Holds a clone of
/// the active `playbin` element; all methods must be called on the GTK main thread.
#[derive(Clone)]
pub struct PlaybackProbe {
    playbin: gst::Element,
}

impl PlaybackProbe {
    /// Current playback position in milliseconds, if the pipeline can report one.
    pub fn position_ms(&self) -> Option<i64> {
        self.playbin
            .query_position::<gst::ClockTime>()
            .map(|t| t.mseconds() as i64)
    }

    /// Whether the pipeline is actually in the Playing state (not paused/buffering).
    pub fn is_playing(&self) -> bool {
        self.playbin.current_state() == gst::State::Playing
    }

    /// The URI currently loaded into `playbin` (`current-uri`), if any. Lets the
    /// editor tell whether *its* recording — rather than some other track the
    /// user started meanwhile — is the one playing.
    pub fn current_uri(&self) -> Option<String> {
        self.playbin
            .property_value("current-uri")
            .get::<Option<String>>()
            .ok()
            .flatten()
    }

    /// Seeks the running pipeline to `ms` (used to skip over pending cut ranges
    /// while previewing). Best effort; a failing seek is ignored.
    pub fn seek_ms(&self, ms: i64) {
        let _ = self.playbin.seek_simple(
            gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
            gst::ClockTime::from_mseconds(ms.max(0) as u64),
        );
    }
}

/// One playback deck: a `playbin3`, its (optional) equalizer and the per-deck
/// preroll bookkeeping read by the bus watch.
struct Deck {
    bin: gst::Element,
    /// 10-band equalizer inside this deck's `audio-filter` chain, if available.
    equalizer: Option<gst::Element>,
    /// A fresh track was just **explicitly** loaded on this deck → the bus watch
    /// re-applies the rate once prerolled, and a `STREAM_START` is *not* treated
    /// as a gapless auto-advance.
    fresh_load: Rc<Cell<bool>>,
    /// Resume position (ms) to seek to once this deck has prerolled (`AsyncDone`).
    pending_seek_ms: Rc<Cell<i64>>,
}

impl Deck {
    fn make() -> Result<Self> {
        let bin = gst::ElementFactory::make("playbin3")
            .build()
            .map_err(|_| anyhow!("playbin3 unavailable – is gstreamer installed?"))?;
        // Audio-filter chain: scaletempo (pitch-preserving speed change) then the
        // 10-band equalizer. Each element is optional – the chain adapts.
        let scaletempo = gst::ElementFactory::make("scaletempo").build().ok();
        let equalizer = gst::ElementFactory::make("equalizer-10bands").build().ok();
        if let Some(filter) = build_audio_filter(scaletempo.as_ref(), equalizer.as_ref()) {
            bin.set_property("audio-filter", &filter);
        }
        Ok(Self {
            bin,
            equalizer,
            fresh_load: Rc::new(Cell::new(false)),
            pending_seek_ms: Rc::new(Cell::new(0)),
        })
    }
}

pub struct Player {
    /// Two decks; `active` selects the one the player queries / controls.
    decks: [Deck; 2],
    /// Index (0/1) of the active deck. `Arc<Atomic>` because the `about-to-finish`
    /// signal closure (a GStreamer streaming thread) reads it.
    active: Arc<AtomicUsize>,
    /// Current playback rate (speed); re-applied after each load. Main thread only.
    rate: Rc<Cell<f64>>,
    /// Gapless enabled (sequential local queues continue without a gap).
    gapless: Arc<AtomicBool>,
    /// Crossfade length in milliseconds (0 = off). When > 0, the gapless
    /// `about-to-finish` continuation is suppressed and the app drives crossfades.
    crossfade_ms: Arc<AtomicU64>,
    /// URI the active deck's `about-to-finish` will continue into (gapless).
    /// Set by the app, consumed on the streaming thread → `Arc<Mutex>`.
    next_uri: Arc<Mutex<Option<String>>>,
    /// The running crossfade ramp timer (so a new transition can cancel it).
    fade_source: Rc<RefCell<Option<gst::glib::SourceId>>>,
    /// Keeps the per-deck bus watches alive.
    bus_watches: RefCell<Vec<gst::bus::BusWatchGuard>>,
}

impl Player {
    pub fn new() -> Result<Self> {
        gst::init()?;
        let d0 = Deck::make()?;
        let d1 = Deck::make()?;
        if d0.equalizer.is_none() {
            tracing::warn!("equalizer-10bands unavailable – EQ disabled");
        }
        Ok(Self {
            decks: [d0, d1],
            active: Arc::new(AtomicUsize::new(0)),
            rate: Rc::new(Cell::new(1.0)),
            gapless: Arc::new(AtomicBool::new(true)),
            crossfade_ms: Arc::new(AtomicU64::new(0)),
            next_uri: Arc::new(Mutex::new(None)),
            fade_source: Rc::new(RefCell::new(None)),
            bus_watches: RefCell::new(Vec::new()),
        })
    }

    /// The active deck's `playbin3`.
    fn cur(&self) -> &gst::Element {
        &self.decks[self.active.load(Ordering::Relaxed)].bin
    }

    /// The active deck.
    fn cur_deck(&self) -> &Deck {
        &self.decks[self.active.load(Ordering::Relaxed)]
    }

    // --- Configuration -----------------------------------------------------

    /// Enables/disables gapless continuation (default on).
    pub fn set_gapless(&self, on: bool) {
        self.gapless.store(on, Ordering::Relaxed);
    }

    /// Sets the crossfade window in seconds (0 = off).
    pub fn set_crossfade_secs(&self, secs: f64) {
        self.crossfade_ms
            .store((secs.max(0.0) * 1000.0) as u64, Ordering::Relaxed);
    }

    /// The crossfade window in seconds (0 = off).
    pub fn crossfade_secs(&self) -> f64 {
        self.crossfade_ms.load(Ordering::Relaxed) as f64 / 1000.0
    }

    /// Arms (or clears) the URI the active deck's `about-to-finish` will continue
    /// into for gapless playback. The app sets it to the next sequential **local**
    /// track, or `None` to fall back to the normal end-of-track path.
    pub fn arm_next_gapless(&self, uri: Option<String>) {
        if let Ok(mut g) = self.next_uri.lock() {
            *g = uri;
        }
    }

    /// Sets the 10 band gains (dB, each −24…+12) on the active deck live.
    pub fn set_eq_bands(&self, bands: &[f64; 10]) {
        let Some(eq) = &self.cur_deck().equalizer else {
            return;
        };
        for (i, gain) in bands.iter().enumerate() {
            eq.set_property(&format!("band{i}"), gain.clamp(-24.0, 12.0));
        }
    }

    /// Sets the linear output volume (0.0–1.0) on the active deck. Used by the
    /// sleep-timer fade-out; crossfading drives both decks' volumes directly.
    pub fn set_volume(&self, vol: f64) {
        self.cur().set_property("volume", vol.clamp(0.0, 1.0));
    }

    // --- Loading / playback ------------------------------------------------

    /// Loads a local file and starts playback on the active deck. If
    /// `resume_ms > 0`, it seeks there before starting (resume for audio dramas).
    pub fn play_file(&self, path: &str, resume_ms: i64) -> Result<()> {
        let uri = gst::glib::filename_to_uri(path, None)
            .map_err(|e| anyhow!("Invalid path {path}: {e}"))?;
        self.hard_load(uri.as_str(), resume_ms)
    }

    /// Plays an arbitrary network URI (e.g. an http podcast episode) on the
    /// active deck. Unlike `play_file`, the URI is taken as-is.
    pub fn play_uri(&self, uri: &str, resume_ms: i64) -> Result<()> {
        if !is_allowed_remote_uri(uri) {
            return Err(anyhow!("Refusing to play non-network URI: {uri}"));
        }
        self.hard_load(uri, resume_ms)
    }

    /// Explicit (user-initiated) load on the active deck: cancels any crossfade,
    /// silences/stops the idle deck and resets the active deck to the new URI.
    /// `playbin3` only re-reads `uri` on a state change, so the deck is reset to
    /// `Ready` first.
    fn hard_load(&self, uri: &str, resume_ms: i64) -> Result<()> {
        self.cancel_crossfade();
        // The playback context just changed – drop any armed gapless follow.
        self.arm_next_gapless(None);
        let cur = self.cur();
        cur.set_state(gst::State::Ready)
            .map_err(|e| anyhow!("Failed to reset pipeline: {e}"))?;
        cur.set_property("volume", 1.0_f64);
        cur.set_property("uri", uri);
        self.start(resume_ms)
    }

    /// Starts the freshly-set active deck. For a resume (`resume_ms > 0`) we go to
    /// PAUSED and **arm** the seek; the bus watch performs it on `AsyncDone`
    /// (preroll complete) and only then starts playback — so the UI thread never
    /// blocks waiting for preroll and audio never briefly plays from 0:00.
    fn start(&self, resume_ms: i64) -> Result<()> {
        let deck = self.cur_deck();
        deck.fresh_load.set(true);
        if resume_ms > 0 {
            deck.pending_seek_ms.set(resume_ms);
            deck.bin
                .set_state(gst::State::Paused)
                .map_err(|e| anyhow!("Failed to prepare pipeline: {e}"))?;
        } else {
            deck.pending_seek_ms.set(0);
            deck.bin
                .set_state(gst::State::Playing)
                .map_err(|e| anyhow!("Failed to start playback: {e}"))?;
        }
        Ok(())
    }

    /// Starts a crossfade to `uri` over the configured window: the next track
    /// begins on the idle deck at volume 0, that deck becomes active immediately
    /// (so the UI tracks the incoming song), and a ramp fades the outgoing deck
    /// out / the incoming deck in before stopping the outgoing one. Falls back to
    /// a hard load when crossfade is off. `resume_ms` seeks the incoming track
    /// (normally 0 for a sequential advance).
    pub fn crossfade_to(&self, uri: &str, resume_ms: i64) -> Result<()> {
        let secs = self.crossfade_secs();
        if secs <= 0.0 {
            return self.hard_load(uri, resume_ms);
        }
        self.cancel_crossfade();
        let from = self.active.load(Ordering::Relaxed);
        let to = 1 - from;
        let in_deck = &self.decks[to];
        in_deck
            .bin
            .set_state(gst::State::Ready)
            .map_err(|e| anyhow!("Failed to reset crossfade deck: {e}"))?;
        in_deck.bin.set_property("volume", 0.0_f64);
        in_deck.bin.set_property("uri", uri);
        in_deck.fresh_load.set(true);
        in_deck.pending_seek_ms.set(resume_ms.max(0));
        in_deck
            .bin
            .set_state(gst::State::Playing)
            .map_err(|e| anyhow!("Failed to start crossfade deck: {e}"))?;
        // The incoming deck is now the one the app queries / controls.
        self.active.store(to, Ordering::Relaxed);
        self.start_fade_ramp(from, to, secs);
        Ok(())
    }

    /// Drives the crossfade volume ramp on the main loop (~50 ms steps).
    fn start_fade_ramp(&self, from: usize, to: usize, secs: f64) {
        let total_ms = ((secs * 1000.0) as u64).max(1);
        let step_ms = 50u64;
        let from_bin = self.decks[from].bin.clone();
        let to_bin = self.decks[to].bin.clone();
        let fade_source = self.fade_source.clone();
        let elapsed = Cell::new(0u64);
        let id = gst::glib::timeout_add_local(Duration::from_millis(step_ms), move || {
            let e = elapsed.get() + step_ms;
            elapsed.set(e);
            let t = (e as f64 / total_ms as f64).min(1.0);
            from_bin.set_property("volume", (1.0 - t).clamp(0.0, 1.0));
            to_bin.set_property("volume", t.clamp(0.0, 1.0));
            if t >= 1.0 {
                let _ = from_bin.set_state(gst::State::Null);
                from_bin.set_property("volume", 1.0_f64);
                *fade_source.borrow_mut() = None;
                gst::glib::ControlFlow::Break
            } else {
                gst::glib::ControlFlow::Continue
            }
        });
        *self.fade_source.borrow_mut() = Some(id);
    }

    /// Stops a running crossfade ramp and the idle deck, restoring full volume on
    /// both decks. Safe to call when no crossfade is active.
    fn cancel_crossfade(&self) {
        if let Some(id) = self.fade_source.borrow_mut().take() {
            id.remove();
        }
        let idle = 1 - self.active.load(Ordering::Relaxed);
        let _ = self.decks[idle].bin.set_state(gst::State::Null);
        self.decks[idle].bin.set_property("volume", 1.0_f64);
        self.cur().set_property("volume", 1.0_f64);
    }

    /// Registers the per-deck bus watches and `about-to-finish` handlers.
    /// `on_eos` fires at the active deck's end (advance / stop), `on_title` on a
    /// title tag (ICY "now playing" for stations), `on_stream_start` when the
    /// active deck begins a **gapless** continuation (so the app advances its
    /// state to match). Runs on the main loop.
    pub fn connect_bus_events<E, T, R, A, S>(
        &self,
        on_eos: E,
        on_title: T,
        on_error: R,
        on_ready: A,
        on_stream_start: S,
    ) where
        E: Fn() + 'static,
        T: Fn(String) + 'static,
        R: Fn() + 'static,
        A: Fn() + 'static,
        S: Fn() + 'static,
    {
        let on_eos = Rc::new(on_eos);
        let on_title = Rc::new(on_title);
        let on_error = Rc::new(on_error);
        let on_ready = Rc::new(on_ready);
        let on_stream_start = Rc::new(on_stream_start);

        for idx in 0..self.decks.len() {
            let deck = &self.decks[idx];

            // Gapless continuation: hand the armed next URI to this deck.
            {
                let next_uri = self.next_uri.clone();
                let gapless = self.gapless.clone();
                let crossfade_ms = self.crossfade_ms.clone();
                let active = self.active.clone();
                deck.bin.connect("about-to-finish", false, move |vals| {
                    if gapless.load(Ordering::Relaxed)
                        && crossfade_ms.load(Ordering::Relaxed) == 0
                        && active.load(Ordering::Relaxed) == idx
                    {
                        if let Some(uri) = next_uri.lock().ok().and_then(|mut g| g.take()) {
                            if let Ok(bin) = vals[0].get::<gst::Element>() {
                                bin.set_property("uri", uri);
                            }
                        }
                    }
                    None
                });
            }

            let Some(bus) = deck.bin.bus() else {
                continue;
            };
            let bin = deck.bin.clone();
            let pending_seek = deck.pending_seek_ms.clone();
            let fresh_load = deck.fresh_load.clone();
            let rate = self.rate.clone();
            let active = self.active.clone();
            let on_eos = on_eos.clone();
            let on_title = on_title.clone();
            let on_error = on_error.clone();
            let on_ready = on_ready.clone();
            let on_stream_start = on_stream_start.clone();
            let guard = bus.add_watch_local(move |_, msg| {
                let is_active = active.load(Ordering::Relaxed) == idx;
                match msg.view() {
                    gst::MessageView::Eos(_) => {
                        if is_active {
                            on_eos();
                        }
                    }
                    gst::MessageView::StreamStart(_) => {
                        // A gapless continuation just began on the active deck
                        // (the app didn't explicitly load it → `fresh_load` is
                        // false). Explicit loads set `fresh_load` and are skipped.
                        if is_active && !fresh_load.get() {
                            on_stream_start();
                        }
                    }
                    gst::MessageView::AsyncDone(_) => {
                        // Preroll finished. Apply an armed resume seek and/or the
                        // current playback rate (a freshly loaded segment always
                        // starts at 1.0). Our own flush-seek posts another
                        // AsyncDone, but the armed values are already cleared.
                        let target = pending_seek.replace(0);
                        let fresh = fresh_load.replace(false);
                        if fresh && is_active {
                            on_ready();
                        }
                        let r = rate.get();
                        let want_rate = (r - 1.0).abs() > 1e-3;
                        if target > 0 {
                            let pos = gst::ClockTime::from_mseconds(target.max(0) as u64);
                            if want_rate {
                                rate_seek(&bin, r, pos);
                            } else {
                                let _ = bin.seek_simple(
                                    gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                                    pos,
                                );
                            }
                            let _ = bin.set_state(gst::State::Playing);
                        } else if fresh && want_rate {
                            let pos = bin
                                .query_position::<gst::ClockTime>()
                                .unwrap_or(gst::ClockTime::ZERO);
                            rate_seek(&bin, r, pos);
                        }
                    }
                    gst::MessageView::Error(err) => {
                        tracing::error!("GStreamer error: {} ({:?})", err.error(), err.debug());
                        if is_active {
                            on_error();
                        }
                    }
                    gst::MessageView::Tag(tag) => {
                        if is_active {
                            if let Some(title) = tag.tags().get::<gst::tags::Title>() {
                                let t = title.get().to_string();
                                if !t.trim().is_empty() {
                                    on_title(t);
                                }
                            }
                        }
                    }
                    _ => {}
                }
                gst::glib::ControlFlow::Continue
            });
            if let Ok(guard) = guard {
                self.bus_watches.borrow_mut().push(guard);
            }
        }
    }

    pub fn pause(&self) {
        // Pausing mid-crossfade would leave the outgoing deck playing → snap to
        // the active deck first.
        if self.fade_source.borrow().is_some() {
            self.cancel_crossfade();
        }
        let _ = self.cur().set_state(gst::State::Paused);
    }

    pub fn resume(&self) {
        let _ = self.cur().set_state(gst::State::Playing);
    }

    pub fn stop(&self) {
        self.cancel_crossfade();
        let _ = self.cur().set_state(gst::State::Null);
    }

    pub fn position_ms(&self) -> Option<i64> {
        self.cur()
            .query_position::<gst::ClockTime>()
            .map(|t| t.mseconds() as i64)
    }

    /// A cheap, cloneable view onto the active pipeline for live UI probing
    /// (the recording editor's timeline polls it ~20×/s).
    pub fn probe(&self) -> PlaybackProbe {
        PlaybackProbe {
            playbin: self.cur().clone(),
        }
    }

    pub fn duration_ms(&self) -> Option<i64> {
        self.cur()
            .query_duration::<gst::ClockTime>()
            .map(|t| t.mseconds() as i64)
    }

    /// Seeks the active deck to the given position (e.g. for resume).
    pub fn seek_ms(&self, ms: i64) -> Result<()> {
        self.cur()
            .seek_simple(
                gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                gst::ClockTime::from_mseconds(ms.max(0) as u64),
            )
            .map_err(|e| anyhow!("Seek failed: {e}"))?;
        Ok(())
    }

    /// Sets the playback speed (clamped to 0.25–2.0; pitch preserved via
    /// scaletempo) on the active deck. Persists across tracks in the session
    /// (re-applied after each load). A failing rate-seek is ignored.
    pub fn set_rate(&self, rate: f64) {
        let rate = rate.clamp(0.25, 2.0);
        self.rate.set(rate);
        if let Some(pos) = self.cur().query_position::<gst::ClockTime>() {
            rate_seek(self.cur(), rate, pos);
        }
    }

    /// Re-applies the stored rate to the active deck at its current position.
    /// Used after a gapless continuation (a new segment starts at rate 1.0).
    pub fn reapply_rate(&self) {
        let r = self.rate.get();
        if (r - 1.0).abs() <= 1e-3 {
            return;
        }
        if let Some(pos) = self.cur().query_position::<gst::ClockTime>() {
            rate_seek(self.cur(), r, pos);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_allowed_remote_uri;

    #[test]
    fn remote_uri_allowlist_blocks_local_schemes() {
        // Network streaming schemes are allowed (radio, podcasts, WebDAV).
        for ok in [
            "http://radio.example/stream",
            "https://cloud.example/remote.php/dav/x.mp3",
            "HTTPS://Cloud.Example/x",
            "rtsp://host/live",
            "mms://host/live",
        ] {
            assert!(is_allowed_remote_uri(ok), "{ok} should be allowed");
        }
        // Local-resource schemes a hostile feed/station must never reach.
        for bad in [
            "file:///etc/passwd",
            "cdda://1",
            "resource:///x",
            "/etc/passwd",
            "",
        ] {
            assert!(!is_allowed_remote_uri(bad), "{bad} should be blocked");
        }
    }
}
