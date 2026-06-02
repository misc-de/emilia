//! GStreamer playback via `playbin3`.

use anyhow::{anyhow, Result};
use gstreamer as gst;
use gstreamer::prelude::*;

pub struct Player {
    playbin: gst::Element,
    /// 10-band equalizer as `audio-filter` (if the plugin is available).
    equalizer: Option<gst::Element>,
    /// Keeps the bus watch alive. `add_watch_local` returns a guard
    /// that **removes** the watch again when dropped – without holding
    /// onto it, an EOS would never arrive (no automatic advancing).
    bus_watch: std::cell::RefCell<Option<gst::bus::BusWatchGuard>>,
}

impl Player {
    pub fn new() -> Result<Self> {
        gst::init()?;
        let playbin = gst::ElementFactory::make("playbin3")
            .build()
            .map_err(|_| anyhow!("playbin3 unavailable – is gstreamer installed?"))?;

        // Hook in the equalizer as an audio filter (optional – only if available).
        let equalizer = gst::ElementFactory::make("equalizer-10bands").build().ok();
        match &equalizer {
            Some(eq) => playbin.set_property("audio-filter", eq),
            None => tracing::warn!("equalizer-10bands unavailable – EQ disabled"),
        }

        Ok(Self {
            playbin,
            equalizer,
            bus_watch: std::cell::RefCell::new(None),
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
        if resume_ms > 0 {
            // For a reliable seek the pipeline must first preroll:
            // briefly go to PAUSED, wait for the preroll (only a few
            // milliseconds for local files), then seek to the resume position.
            self.playbin
                .set_state(gst::State::Paused)
                .map_err(|e| anyhow!("Failed to prepare pipeline: {e}"))?;
            let _ = self.playbin.state(gst::ClockTime::from_seconds(5));
            let _ = self.seek_ms(resume_ms);
        }
        self.playbin
            .set_state(gst::State::Playing)
            .map_err(|e| anyhow!("Failed to start playback: {e}"))?;
        Ok(())
    }

    /// Plays an arbitrary URI (e.g. an http podcast episode). Unlike
    /// `play_file`, the URI is taken as-is (not a file path).
    /// `resume_ms > 0` seeks to the saved position after the preroll (provided
    /// the source is seekable – podcast hosts usually support ranges).
    pub fn play_uri(&self, uri: &str, resume_ms: i64) -> Result<()> {
        self.playbin
            .set_state(gst::State::Ready)
            .map_err(|e| anyhow!("Failed to reset pipeline: {e}"))?;
        self.playbin.set_property("uri", uri);
        if resume_ms > 0 {
            // Like play_file: briefly go to PAUSED, wait for the preroll
            // (a bit longer for streams), then seek to the resume position.
            self.playbin
                .set_state(gst::State::Paused)
                .map_err(|e| anyhow!("Failed to prepare pipeline: {e}"))?;
            let _ = self.playbin.state(gst::ClockTime::from_seconds(10));
            let _ = self.seek_ms(resume_ms);
        }
        self.playbin
            .set_state(gst::State::Playing)
            .map_err(|e| anyhow!("Failed to start playback: {e}"))?;
        Ok(())
    }

    /// Registers callbacks for the bus events: `on_eos` at track end (for
    /// advancing in the queue) and `on_title` on a
    /// title tag. For streams (internet radio), `playbin3` delivers the
    /// **currently playing track** as a tag via the ICY metadata – this lets
    /// us show "Now Playing" without opening a second connection. Runs in the
    /// main loop.
    pub fn connect_bus_events<E, T>(&self, on_eos: E, on_title: T)
    where
        E: Fn() + 'static,
        T: Fn(String) + 'static,
    {
        if let Some(bus) = self.playbin.bus() {
            let guard = bus.add_watch_local(move |_, msg| {
                match msg.view() {
                    gst::MessageView::Eos(_) => on_eos(),
                    gst::MessageView::Error(err) => {
                        tracing::error!(
                            "GStreamer error: {} ({:?})",
                            err.error(),
                            err.debug()
                        );
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
}
