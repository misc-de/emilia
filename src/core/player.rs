//! GStreamer playback via `playbin3`.

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

pub struct Player {
    playbin: gst::Element,
    /// 10-band equalizer (lives inside the `audio-filter` chain if available).
    equalizer: Option<gst::Element>,
    /// Current playback rate (speed). Applied via a rate-seek and re-applied
    /// after each track load (a new segment always starts at 1.0). 1.0 = normal.
    rate: std::rc::Rc<std::cell::Cell<f64>>,
    /// A fresh track was just loaded → the bus watch re-applies `rate` once the
    /// pipeline has prerolled.
    fresh_load: std::rc::Rc<std::cell::Cell<bool>>,
    /// Keeps the bus watch alive. `add_watch_local` returns a guard
    /// that **removes** the watch again when dropped – without holding
    /// onto it, an EOS would never arrive (no automatic advancing).
    bus_watch: std::cell::RefCell<Option<gst::bus::BusWatchGuard>>,
    /// Resume position (ms) to seek to once the freshly loaded pipeline has
    /// prerolled (signalled by `AsyncDone` on the bus). `0` = none. This lets
    /// `play_file`/`play_uri` arm a resume and return immediately instead of
    /// **blocking the UI thread** for up to several seconds waiting on preroll.
    /// Shared with the bus-watch closure (single-threaded, main loop).
    pending_seek_ms: std::rc::Rc<std::cell::Cell<i64>>,
}

impl Player {
    pub fn new() -> Result<Self> {
        gst::init()?;
        let playbin = gst::ElementFactory::make("playbin3")
            .build()
            .map_err(|_| anyhow!("playbin3 unavailable – is gstreamer installed?"))?;

        // Audio-filter chain: scaletempo (pitch-preserving speed change) followed
        // by the 10-band equalizer. Each element is optional – the chain adapts to
        // whatever plugins are present.
        let scaletempo = gst::ElementFactory::make("scaletempo").build().ok();
        if scaletempo.is_none() {
            tracing::warn!("scaletempo unavailable – speed changes would alter pitch");
        }
        let equalizer = gst::ElementFactory::make("equalizer-10bands").build().ok();
        if equalizer.is_none() {
            tracing::warn!("equalizer-10bands unavailable – EQ disabled");
        }
        if let Some(filter) = build_audio_filter(scaletempo.as_ref(), equalizer.as_ref()) {
            playbin.set_property("audio-filter", &filter);
        }

        Ok(Self {
            playbin,
            equalizer,
            rate: std::rc::Rc::new(std::cell::Cell::new(1.0)),
            fresh_load: std::rc::Rc::new(std::cell::Cell::new(false)),
            bus_watch: std::cell::RefCell::new(None),
            pending_seek_ms: std::rc::Rc::new(std::cell::Cell::new(0)),
        })
    }

    /// Sets the 10 band gains (dB, each −24…+12) live.
    pub fn set_eq_bands(&self, bands: &[f64; 10]) {
        let Some(eq) = &self.equalizer else {
            return;
        };
        for (i, gain) in bands.iter().enumerate() {
            eq.set_property(&format!("band{i}"), gain.clamp(-24.0, 12.0));
        }
    }

    /// Loads a local file and starts playback. If `resume_ms > 0`,
    /// it seeks to that position before starting (resume for audio dramas).
    pub fn play_file(&self, path: &str, resume_ms: i64) -> Result<()> {
        let uri = gst::glib::filename_to_uri(path, None)
            .map_err(|e| anyhow!("Invalid path {path}: {e}"))?;
        // playbin3 only re-reads the `uri` on a state change – if a track is
        // already playing, the pipeline must first be reset, otherwise the
        // old track keeps playing.
        self.playbin
            .set_state(gst::State::Ready)
            .map_err(|e| anyhow!("Failed to reset pipeline: {e}"))?;
        self.playbin.set_property("uri", uri.as_str());
        self.start(resume_ms)
    }

    /// Plays an arbitrary URI (e.g. an http podcast episode). Unlike
    /// `play_file`, the URI is taken as-is (not a file path).
    /// `resume_ms > 0` seeks to the saved position after the preroll (provided
    /// the source is seekable – podcast hosts usually support ranges).
    pub fn play_uri(&self, uri: &str, resume_ms: i64) -> Result<()> {
        if !is_allowed_remote_uri(uri) {
            return Err(anyhow!("Refusing to play non-network URI: {uri}"));
        }
        self.playbin
            .set_state(gst::State::Ready)
            .map_err(|e| anyhow!("Failed to reset pipeline: {e}"))?;
        self.playbin.set_property("uri", uri);
        self.start(resume_ms)
    }

    /// Starts the freshly-set pipeline. For a resume (`resume_ms > 0`) we go to
    /// PAUSED and **arm** the seek; the bus watch performs it on `AsyncDone`
    /// (preroll complete) and only then starts playback — so the UI thread never
    /// blocks waiting for preroll and audio never briefly plays from 0:00.
    fn start(&self, resume_ms: i64) -> Result<()> {
        // A new segment starts at rate 1.0 – let the bus watch re-apply our rate
        // once the pipeline has prerolled.
        self.fresh_load.set(true);
        if resume_ms > 0 {
            self.pending_seek_ms.set(resume_ms);
            self.playbin
                .set_state(gst::State::Paused)
                .map_err(|e| anyhow!("Failed to prepare pipeline: {e}"))?;
        } else {
            self.pending_seek_ms.set(0);
            self.playbin
                .set_state(gst::State::Playing)
                .map_err(|e| anyhow!("Failed to start playback: {e}"))?;
        }
        Ok(())
    }

    /// Registers callbacks for the bus events: `on_eos` at track end (for
    /// advancing in the queue) and `on_title` on a
    /// title tag. For streams (internet radio), `playbin3` delivers the
    /// **currently playing track** as a tag via the ICY metadata – this lets
    /// us show "Now Playing" without opening a second connection. Runs in the
    /// main loop.
    pub fn connect_bus_events<E, T, R, A>(
        &self,
        on_eos: E,
        on_title: T,
        on_error: R,
        on_ready: A,
    ) where
        E: Fn() + 'static,
        T: Fn(String) + 'static,
        R: Fn() + 'static,
        A: Fn() + 'static,
    {
        if let Some(bus) = self.playbin.bus() {
            let playbin = self.playbin.clone();
            let pending_seek = self.pending_seek_ms.clone();
            let rate = self.rate.clone();
            let fresh_load = self.fresh_load.clone();
            let guard = bus.add_watch_local(move |_, msg| {
                match msg.view() {
                    gst::MessageView::Eos(_) => on_eos(),
                    gst::MessageView::AsyncDone(_) => {
                        // Preroll finished. Apply an armed resume seek and/or the
                        // current playback rate (a freshly loaded segment always
                        // starts at 1.0). Our own flush-seek posts another
                        // AsyncDone, but the armed values are already cleared, so
                        // it is a no-op.
                        let target = pending_seek.replace(0);
                        let fresh = fresh_load.replace(false);
                        // A freshly loaded pipeline just prerolled (buffered enough
                        // to start) → the track is ready to play. Used to clear the
                        // "loading" spinner for slow sources (Nextcloud/YouTube).
                        if fresh {
                            on_ready();
                        }
                        let r = rate.get();
                        let want_rate = (r - 1.0).abs() > 1e-3;
                        if target > 0 {
                            let pos = gst::ClockTime::from_mseconds(target.max(0) as u64);
                            if want_rate {
                                rate_seek(&playbin, r, pos);
                            } else {
                                let _ = playbin.seek_simple(
                                    gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                                    pos,
                                );
                            }
                            let _ = playbin.set_state(gst::State::Playing);
                        } else if fresh && want_rate {
                            let pos = playbin
                                .query_position::<gst::ClockTime>()
                                .unwrap_or(gst::ClockTime::ZERO);
                            rate_seek(&playbin, r, pos);
                        }
                    }
                    gst::MessageView::Error(err) => {
                        tracing::error!("GStreamer error: {} ({:?})", err.error(), err.debug());
                        // The current track can't be played (missing file/mount,
                        // unreachable Nextcloud, decode error) → let the app skip.
                        on_error();
                    }
                    gst::MessageView::Tag(tag) => {
                        // ICY "StreamTitle" (or file title) → report to the UI.
                        // The caller decides whether to use it (only for stations).
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
            // Hold onto the guard – otherwise the watch is removed again right
            // away and an EOS would never arrive (no automatic advancing).
            *self.bus_watch.borrow_mut() = guard.ok();
        }
    }

    pub fn pause(&self) {
        let _ = self.playbin.set_state(gst::State::Paused);
    }

    pub fn resume(&self) {
        let _ = self.playbin.set_state(gst::State::Playing);
    }

    pub fn stop(&self) {
        let _ = self.playbin.set_state(gst::State::Null);
    }

    pub fn position_ms(&self) -> Option<i64> {
        self.playbin
            .query_position::<gst::ClockTime>()
            .map(|t| t.mseconds() as i64)
    }

    pub fn duration_ms(&self) -> Option<i64> {
        self.playbin
            .query_duration::<gst::ClockTime>()
            .map(|t| t.mseconds() as i64)
    }

    /// Seeks to the given position (e.g. for resume in audio dramas).
    pub fn seek_ms(&self, ms: i64) -> Result<()> {
        self.playbin
            .seek_simple(
                gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                gst::ClockTime::from_mseconds(ms.max(0) as u64),
            )
            .map_err(|e| anyhow!("Seek failed: {e}"))?;
        Ok(())
    }

    /// Sets the playback speed (clamped to 0.25–2.0; pitch preserved via
    /// scaletempo). Applies to the current track immediately and persists across
    /// tracks in the session (re-applied after each load). A failing rate-seek
    /// (e.g. a non-seekable live stream) is ignored.
    pub fn set_rate(&self, rate: f64) {
        let rate = rate.clamp(0.25, 2.0);
        self.rate.set(rate);
        if let Some(pos) = self.playbin.query_position::<gst::ClockTime>() {
            rate_seek(&self.playbin, rate, pos);
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
