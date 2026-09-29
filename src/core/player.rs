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
/// The chapters of a container TOC as (start ms, title), sorted by start.
/// Editions only group chapters, and nested chapters (sub-chapters) count as
/// jump marks of their own.
fn toc_chapters(toc: &gst::Toc) -> Vec<(i64, String)> {
    fn collect(entries: Vec<gst::TocEntry>, out: &mut Vec<(i64, String)>) {
        for entry in entries {
            if entry.entry_type() == gst::TocEntryType::Chapter {
                if let Some((start, _)) = entry.start_stop_times() {
                    let title = entry
                        .tags()
                        .and_then(|t| t.get::<gst::tags::Title>().map(|v| v.get().to_string()))
                        .unwrap_or_default();
                    out.push((start.max(0) / 1_000_000, title));
                }
            }
            collect(entry.sub_entries(), out);
        }
    }
    let mut out = Vec::new();
    collect(toc.entries(), &mut out);
    out.sort_by_key(|(ms, _)| *ms);
    out.dedup_by_key(|(ms, _)| *ms);
    out
}

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

/// Whether a bus error was raised by the audio output (`autoaudiosink` or the
/// `pulsesink` / `alsasink` / `pipewiresink` inside it) rather than by the
/// source or a decoder. Walks up from the posting element, since the actual
/// sink sits inside `autoaudiosink` inside `playsink`.
fn is_audio_sink_error(msg: &gst::Message) -> bool {
    let mut obj = msg.src().cloned();
    while let Some(o) = obj {
        if let Some(factory) = o.downcast_ref::<gst::Element>().and_then(|e| e.factory()) {
            let klass = factory.klass();
            if klass.contains("Sink") && klass.contains("Audio") {
                return true;
            }
        }
        obj = o.parent();
    }
    false
}

/// Starts a deck again after its audio output was lost and the deck torn down
/// to `Null` (which makes `autoaudiosink` pick and connect an output afresh).
/// Resumes at `pos_ms` via the bus watch's armed seek, and only plays if the
/// app still wants audio - a paused deck comes back paused at its position.
fn reopen_deck(
    bin: &gst::Element,
    pos_ms: i64,
    play: bool,
    fresh_load: &Cell<bool>,
    pending_seek: &Cell<i64>,
) {
    // Marks the restart as an explicit load: its STREAM_START must not be read
    // as a gapless advance, and the rate is re-applied once prerolled.
    fresh_load.set(true);
    pending_seek.set(pos_ms.max(0));
    // With a position to restore, preroll paused and let `AsyncDone` seek and
    // start; the armed seek is what goes to PLAYING then.
    let target = if play && pos_ms <= 0 {
        gst::State::Playing
    } else {
        gst::State::Paused
    };
    // A live source (radio) does not preroll, so no `AsyncDone` would come to
    // start it.
    if let Ok(gst::StateChangeSuccess::NoPreroll) = bin.set_state(target) {
        if play {
            let _ = bin.set_state(gst::State::Playing);
        }
    }
}

/// How often a lost audio output is re-opened before the error is handed to
/// the app, and the delay step between attempts (attempt n waits n × step):
/// together about 20 s, enough for a sound server that is being swapped
/// (PulseAudio ⇄ PipeWire) or restarted to come back.
const SINK_RETRY_MAX: u32 = 8;
const SINK_RETRY_STEP_MS: u64 = 500;

/// Recovery state after the audio output went away. A sound-server connection
/// is opened once when the pipeline leaves `Null` and is never re-established
/// on its own, so a deck whose server disappeared stays silent until it is
/// torn down to `Null` and started again - which is what recovery does.
#[derive(Default)]
struct SinkRecovery {
    /// Attempts since the output last prerolled successfully.
    attempts: Cell<u32>,
    /// The pending restart (at most one; follow-up errors of the same failure
    /// are ignored while it waits).
    timer: RefCell<Option<gst::glib::SourceId>>,
}

impl SinkRecovery {
    /// Drops a pending restart and the attempt count. Explicit transport calls
    /// use it: the user's new intent replaces whatever recovery was restoring.
    fn cancel(&self) {
        if let Some(id) = self.timer.borrow_mut().take() {
            id.remove();
        }
        self.attempts.set(0);
    }
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
    /// This deck's transition factor (0…1): 1 in normal playback, ramped by the
    /// crossfade. Kept separate from the master/sleep gain so the two can be
    /// combined instead of overwriting each other – see `write_volume`.
    ramp: Rc<Cell<f64>>,
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
            ramp: Rc::new(Cell::new(1.0)),
        })
    }

    /// Sets this deck's transition factor and writes the resulting volume.
    fn set_ramp(&self, ramp: f64, gain: f64) {
        self.ramp.set(ramp.clamp(0.0, 1.0));
        write_volume(&self.bin, self.ramp.get(), gain);
    }

    /// Re-writes this deck's volume after a change to the master/sleep gain,
    /// keeping the transition factor it currently sits at.
    fn apply_gain(&self, gain: f64) {
        write_volume(&self.bin, self.ramp.get(), gain);
    }
}

/// A deck's output volume is its transition factor (crossfade ramp) times the
/// gain shared by both decks (master volume × sleep-timer fade).
fn write_volume(bin: &gst::Element, ramp: f64, gain: f64) {
    bin.set_property("volume", (ramp * gain).clamp(0.0, 1.0));
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
    /// Master output volume (0…1), driven by the desktop's MPRIS volume slider.
    master: Rc<Cell<f64>>,
    /// Sleep-timer fade factor (0…1); 1 outside the final fade-out window.
    fade: Rc<Cell<f64>>,
    /// The running crossfade ramp timer (so a new transition can cancel it).
    fade_source: Rc<RefCell<Option<gst::glib::SourceId>>>,
    /// Keeps the per-deck bus watches alive.
    bus_watches: RefCell<Vec<gst::bus::BusWatchGuard>>,
    /// Whether the app *wants* audio right now. Only the explicit transport
    /// calls (`start`, `crossfade_to`, `resume`, `pause`, `stop`) move it, and
    /// the bus watch asks it before starting a deck on its own: a preroll that
    /// completes late - after the user paused, or after the audio sink was
    /// pulled away and the pipeline renegotiated - must never put the pipeline
    /// back into PLAYING behind the app's back. The app's own state would stay
    /// "paused" while sound comes out of the speaker, which is exactly what
    /// nobody can then stop: not the UI, not the lock screen, not a desktop
    /// service watching MPRIS.
    wants_playing: Rc<Cell<bool>>,
    /// Re-opening the audio output after the sound server went away.
    recovery: Rc<SinkRecovery>,
    /// Last position (ms) the app read from the active deck. A deck whose
    /// output just died may no longer answer a position query; recovery then
    /// resumes from here.
    last_pos_ms: Rc<Cell<i64>>,
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
            master: Rc::new(Cell::new(1.0)),
            fade: Rc::new(Cell::new(1.0)),
            fade_source: Rc::new(RefCell::new(None)),
            bus_watches: RefCell::new(Vec::new()),
            wants_playing: Rc::new(Cell::new(false)),
            recovery: Rc::new(SinkRecovery::default()),
            last_pos_ms: Rc::new(Cell::new(0)),
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

    /// Gain shared by both decks: master volume × sleep-timer fade.
    fn gain(&self) -> f64 {
        self.master.get() * self.fade.get()
    }

    /// Sets the sleep-timer fade factor (0.0–1.0) on the active deck. Multiplies
    /// with the master volume, so fading out does not lose the user's setting.
    pub fn set_fade(&self, factor: f64) {
        self.fade.set(factor.clamp(0.0, 1.0));
        self.cur_deck().apply_gain(self.gain());
    }

    /// Sets the master output volume (0.0–1.0), i.e. the desktop's volume
    /// slider. Applies to both decks so a running crossfade follows along.
    pub fn set_master_volume(&self, vol: f64) {
        self.master.set(vol.clamp(0.0, 1.0));
        let gain = self.gain();
        for deck in &self.decks {
            deck.apply_gain(gain);
        }
    }

    /// The master output volume (0.0–1.0).
    pub fn master_volume(&self) -> f64 {
        self.master.get()
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
        self.recovery.cancel();
        self.cancel_crossfade();
        // The playback context just changed – drop any armed gapless follow.
        self.arm_next_gapless(None);
        let cur = self.cur();
        cur.set_state(gst::State::Ready)
            .map_err(|e| anyhow!("Failed to reset pipeline: {e}"))?;
        self.cur_deck().set_ramp(1.0, self.gain());
        cur.set_property("uri", uri);
        self.last_pos_ms.set(resume_ms.max(0));
        self.start(resume_ms)
    }

    /// Starts the freshly-set active deck. For a resume (`resume_ms > 0`) we go to
    /// PAUSED and **arm** the seek; the bus watch performs it on `AsyncDone`
    /// (preroll complete) and only then starts playback — so the UI thread never
    /// blocks waiting for preroll and audio never briefly plays from 0:00.
    fn start(&self, resume_ms: i64) -> Result<()> {
        let deck = self.cur_deck();
        deck.fresh_load.set(true);
        // An explicit load is meant to play - including the resume path below,
        // which reaches PLAYING only from the bus watch.
        self.wants_playing.set(true);
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
        self.recovery.cancel();
        self.cancel_crossfade();
        let from = self.active.load(Ordering::Relaxed);
        let to = 1 - from;
        let in_deck = &self.decks[to];
        in_deck
            .bin
            .set_state(gst::State::Ready)
            .map_err(|e| anyhow!("Failed to reset crossfade deck: {e}"))?;
        in_deck.set_ramp(0.0, self.gain());
        in_deck.bin.set_property("uri", uri);
        in_deck.fresh_load.set(true);
        in_deck.pending_seek_ms.set(resume_ms.max(0));
        self.wants_playing.set(true);
        in_deck
            .bin
            .set_state(gst::State::Playing)
            .map_err(|e| anyhow!("Failed to start crossfade deck: {e}"))?;
        // The incoming deck is now the one the app queries / controls.
        self.active.store(to, Ordering::Relaxed);
        self.last_pos_ms.set(resume_ms.max(0));
        self.start_fade_ramp(from, to, secs);
        Ok(())
    }

    /// Drives the crossfade volume ramp on the main loop (~50 ms steps).
    fn start_fade_ramp(&self, from: usize, to: usize, secs: f64) {
        let total_ms = ((secs * 1000.0) as u64).max(1);
        let step_ms = 50u64;
        let from_bin = self.decks[from].bin.clone();
        let to_bin = self.decks[to].bin.clone();
        let from_ramp = self.decks[from].ramp.clone();
        let to_ramp = self.decks[to].ramp.clone();
        let master = self.master.clone();
        let fade = self.fade.clone();
        let fade_source = self.fade_source.clone();
        let elapsed = Cell::new(0u64);
        let id = gst::glib::timeout_add_local(Duration::from_millis(step_ms), move || {
            let e = elapsed.get() + step_ms;
            elapsed.set(e);
            let t = (e as f64 / total_ms as f64).min(1.0);
            // Re-read the gain every step: the volume slider may move mid-fade.
            let gain = master.get() * fade.get();
            from_ramp.set(1.0 - t);
            to_ramp.set(t);
            write_volume(&from_bin, 1.0 - t, gain);
            write_volume(&to_bin, t, gain);
            if t >= 1.0 {
                let _ = from_bin.set_state(gst::State::Null);
                from_ramp.set(1.0);
                write_volume(&from_bin, 1.0, gain);
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
        let gain = self.gain();
        self.decks[idle].set_ramp(1.0, gain);
        self.cur_deck().set_ramp(1.0, gain);
    }

    /// Registers the per-deck bus watches and `about-to-finish` handlers.
    /// `on_eos` fires at the active deck's end (advance / stop), `on_title` on a
    /// title tag (ICY "now playing" for stations), `on_stream_start` when the
    /// active deck begins a **gapless** continuation (so the app advances its
    /// state to match), `on_toc` with the chapters a file carries itself (m4b /
    /// ID3 `CHAP` / Matroska — an audiobook in one file). Runs on the main loop.
    pub fn connect_bus_events<E, T, R, A, S, C>(
        &self,
        on_eos: E,
        on_title: T,
        on_error: R,
        on_ready: A,
        on_stream_start: S,
        on_toc: C,
    ) where
        E: Fn() + 'static,
        T: Fn(String) + 'static,
        R: Fn() + 'static,
        A: Fn() + 'static,
        S: Fn() + 'static,
        C: Fn(Vec<(i64, String)>) + 'static,
    {
        let on_eos = Rc::new(on_eos);
        let on_title = Rc::new(on_title);
        let on_error = Rc::new(on_error);
        let on_ready = Rc::new(on_ready);
        let on_stream_start = Rc::new(on_stream_start);
        let on_toc = Rc::new(on_toc);

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
            let on_toc = on_toc.clone();
            let wants_playing = self.wants_playing.clone();
            let recovery = self.recovery.clone();
            let last_pos = self.last_pos_ms.clone();
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
                        if is_active {
                            // The output took the stream: a later loss starts
                            // counting its retries afresh.
                            recovery.attempts.set(0);
                        }
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
                            // Only if the app still wants audio. A sink that
                            // disappears (earbuds running flat) makes the
                            // pipeline preroll again, and starting it here
                            // would resume playback nobody asked for, on
                            // whichever output is left - the loudspeaker.
                            if wants_playing.get() {
                                let _ = bin.set_state(gst::State::Playing);
                            }
                        } else if fresh && want_rate {
                            let pos = bin
                                .query_position::<gst::ClockTime>()
                                .unwrap_or(gst::ClockTime::ZERO);
                            rate_seek(&bin, r, pos);
                        }
                    }
                    gst::MessageView::Error(err) => {
                        tracing::error!("GStreamer error: {} ({:?})", err.error(), err.debug());
                        if is_audio_sink_error(msg) {
                            if !is_active {
                                // An idle / outgoing crossfade deck: nothing to
                                // restore, just release the dead output.
                                let _ = bin.set_state(gst::State::Null);
                            } else if recovery.timer.borrow().is_some() {
                                // Follow-up error of a loss already being handled.
                            } else if recovery.attempts.get() < SINK_RETRY_MAX {
                                let attempt = recovery.attempts.get() + 1;
                                recovery.attempts.set(attempt);
                                let pos = bin
                                    .query_position::<gst::ClockTime>()
                                    .map(|t| t.mseconds() as i64)
                                    .filter(|&ms| ms > 0)
                                    .unwrap_or_else(|| last_pos.get());
                                tracing::warn!(
                                    "Audio output lost – reopening in {} ms (attempt {attempt}/{SINK_RETRY_MAX}, at {pos} ms)",
                                    SINK_RETRY_STEP_MS * attempt as u64
                                );
                                let _ = bin.set_state(gst::State::Null);
                                let timer = {
                                    let recovery = recovery.clone();
                                    let bin = bin.clone();
                                    let active = active.clone();
                                    let fresh_load = fresh_load.clone();
                                    let pending_seek = pending_seek.clone();
                                    let wants_playing = wants_playing.clone();
                                    gst::glib::timeout_add_local_once(
                                        Duration::from_millis(SINK_RETRY_STEP_MS * attempt as u64),
                                        move || {
                                            // Fired: forget the id without removing it.
                                            recovery.timer.borrow_mut().take();
                                            if active.load(Ordering::Relaxed) != idx {
                                                return;
                                            }
                                            reopen_deck(
                                                &bin,
                                                pos,
                                                wants_playing.get(),
                                                &fresh_load,
                                                &pending_seek,
                                            );
                                        },
                                    )
                                };
                                *recovery.timer.borrow_mut() = Some(timer);
                            } else {
                                tracing::error!("Audio output did not come back – giving up");
                                recovery.attempts.set(0);
                                let _ = bin.set_state(gst::State::Null);
                                on_error();
                            }
                        } else if is_active {
                            on_error();
                        }
                    }
                    gst::MessageView::Toc(toc) if is_active => {
                        let chapters = toc_chapters(&toc.toc().0);
                        if !chapters.is_empty() {
                            on_toc(chapters);
                        }
                    }
                    gst::MessageView::Tag(tag) if is_active => {
                        if let Some(title) = tag.tags().get::<gst::tags::Title>() {
                            let t = title.get().to_string();
                            if !t.trim().is_empty() {
                                on_title(t);
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
        self.wants_playing.set(false);
        let _ = self.cur().set_state(gst::State::Paused);
    }

    /// Resumes the active deck. Returns `false` when there is nothing loaded:
    /// `playbin` accepts the state change on an empty pipeline and then sits in
    /// Playing without content, which callers used to report to the desktop as
    /// "playing" (silently, forever).
    pub fn resume(&self) -> bool {
        if !self.has_content() {
            return false;
        }
        self.wants_playing.set(true);
        let started = self.cur().set_state(gst::State::Playing).is_ok();
        if !started {
            self.wants_playing.set(false);
        }
        started
    }

    /// Whether the active deck has a URI loaded, i.e. whether there is anything
    /// to resume or seek in. `stop()` tears the pipeline down, so this is false
    /// after it (and before the first track of a session).
    pub fn has_content(&self) -> bool {
        self.cur()
            .property::<Option<String>>("uri")
            .is_some_and(|uri| !uri.is_empty())
    }

    pub fn stop(&self) {
        self.recovery.cancel();
        self.cancel_crossfade();
        self.wants_playing.set(false);
        let _ = self.cur().set_state(gst::State::Null);
    }

    pub fn position_ms(&self) -> Option<i64> {
        // While a lost output is being reopened the deck sits at 0 (or in
        // `Null`) until the armed seek lands; report where it will resume, so
        // the resume point saved meanwhile is not reset to the start.
        if self.recovery.timer.borrow().is_some() || self.cur_deck().pending_seek_ms.get() > 0 {
            return Some(self.last_pos_ms.get());
        }
        let pos = self
            .cur()
            .query_position::<gst::ClockTime>()
            .map(|t| t.mseconds() as i64);
        if let Some(ms) = pos {
            self.last_pos_ms.set(ms);
        }
        pos
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

    /// Seeks the active deck to the given position (e.g. for resume). The
    /// target is clamped to the track: seeking past the end is accepted by
    /// `playbin` but leaves the pipeline running in silence — with a network
    /// source there is no EOS to end it, so the deck (and the audio sink) would
    /// stay awake with the position frozen or ticking on forever.
    pub fn seek_ms(&self, ms: i64) -> Result<()> {
        let mut target = ms.max(0);
        if let Some(dur) = self.duration_ms().filter(|&d| d > 0) {
            target = target.min(dur);
        }
        self.cur()
            .seek_simple(
                gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                gst::ClockTime::from_mseconds(target as u64),
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
