//! Microphone recording for voice memos.
//!
//! Unlike [`crate::core::recorder`] (the stream timeshift, which keeps a
//! discardable ring buffer), a memo is a short **finalized** capture. The
//! GStreamer pipeline `autoaudiosrc ! audioconvert ! audioresample ! opusenc !
//! oggmux ! filesink` writes an Ogg/Opus file; on stop it is finalized by
//! sending EOS and **waiting** for it to reach the sink before tearing the
//! pipeline down — otherwise the Ogg is truncated/unseekable and reports a wrong
//! duration (which would also break the waveform editor).
//!
//! Ogg/Opus is deliberately the same container the editor's re-encode path
//! already handles ([`crate::core::waveform::cut`]), so a memo edits without any
//! new codec work.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;

/// Encoder + muxer for memos: Ogg/Opus (small, ideal for voice).
const ENC: &str = "opusenc ! oggmux";
/// Matching file extension.
const EXT: &str = "ogg";

/// Unique file-name counter so two quick recordings never collide.
static SEQ: AtomicU64 = AtomicU64::new(0);

/// Where voice memos are stored: `$XDG_DATA_HOME/emilia/memos` — inside the app
/// data dir (like podcast downloads), **not** the music library, so the library
/// scan never picks memos up as tracks.
pub fn memos_dir() -> PathBuf {
    let mut dir = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
    dir.push("emilia");
    dir.push("memos");
    dir
}

/// A running microphone recording. [`stop`](Self::stop) finalizes the file and
/// returns it; dropping without stopping (app quit / aborted) tears the pipeline
/// down and removes the then-unusable, unfinalized file.
pub struct MicRecorder {
    pipeline: gst::Pipeline,
    path: PathBuf,
    /// Set once [`stop`](Self::stop) has taken over teardown, so `Drop` does not
    /// also delete the (now finalized) file.
    stopped: bool,
    /// Live per-channel input level for the recording meter (see [`MicLevel`]).
    /// Fed from the pipeline's `level` element via a bus sync handler (off the UI
    /// thread); the UI polls it via [`level_handle`](Self::level_handle).
    level: MicLevel,
}

/// Live microphone input level for the recording meter. Lock-free: the mic
/// thread stores, the UI poll reads. `lr` packs the left channel (high 32 bits)
/// and right channel (low 32 bits) as `f32` bits, each normalized 0.0–1.0;
/// `channels` is the source channel count (1 = mono → the meter draws a single
/// bar, ≥2 = stereo → two bars).
#[derive(Clone)]
pub struct MicLevel {
    lr: Arc<AtomicU64>,
    channels: Arc<AtomicU32>,
}

impl MicLevel {
    fn new() -> Self {
        Self {
            lr: Arc::new(AtomicU64::new(0)),
            channels: Arc::new(AtomicU32::new(1)),
        }
    }

    fn store(&self, left: f32, right: f32, channels: u32) {
        let packed = (u64::from(left.to_bits()) << 32) | u64::from(right.to_bits());
        self.lr.store(packed, Ordering::Relaxed);
        self.channels.store(channels.max(1), Ordering::Relaxed);
    }

    /// `(left, right, channels)`. For mono `left == right` and `channels == 1`.
    pub fn read(&self) -> (f32, f32, u32) {
        let v = self.lr.load(Ordering::Relaxed);
        (
            f32::from_bits((v >> 32) as u32),
            f32::from_bits(v as u32),
            self.channels.load(Ordering::Relaxed),
        )
    }
}

impl MicRecorder {
    /// Starts capturing the default microphone into a new file in `dest_dir`.
    /// Returns immediately; audio flows on GStreamer's own threads. Errors only
    /// when the pipeline cannot be built or the microphone cannot be opened.
    pub fn start(dest_dir: &Path) -> Result<MicRecorder> {
        let _ = gst::init();
        std::fs::create_dir_all(dest_dir)
            .with_context(|| format!("creating {}", dest_dir.display()))?;

        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let path = unique_path(dest_dir, &format!("memo-{stamp}-{n}"), EXT);

        // `name=micsrc` so `stop` can send EOS straight to the source (most
        // reliable way to finalize a live capture). The `level` element posts
        // periodic peak/RMS messages that drive the recording animation; if that
        // plugin is unavailable we fall back to a plain pipeline (no animation,
        // but recording still works).
        let with_level = format!(
            "autoaudiosrc name=micsrc ! audioconvert ! level name=lvl interval=50000000 \
             post-messages=true ! audioresample ! {ENC} ! filesink name=sink"
        );
        let plain = format!(
            "autoaudiosrc name=micsrc ! audioconvert ! audioresample ! {ENC} ! filesink name=sink"
        );
        let pipeline = match gst::parse::launch(&with_level) {
            Ok(p) => p,
            Err(_) => {
                gst::parse::launch(&plain).context("building the microphone pipeline failed")?
            }
        }
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("not a pipeline"))?;
        pipeline
            .by_name("sink")
            .context("no filesink")?
            .set_property("location", path.to_string_lossy().as_ref());

        // Mirror the `level` element's peak readings into a shared atomic. The
        // handler runs on GStreamer's thread, so it only does cheap, lock-free
        // work; the frequent level messages are dropped (not queued) while
        // EOS/Error still reach `stop`'s bus wait via `Pass`.
        let level = MicLevel::new();
        if let Some(bus) = pipeline.bus() {
            let level_w = level.clone();
            bus.set_sync_handler(move |_, msg| {
                if let gst::MessageView::Element(el) = msg.view() {
                    if let Some(s) = el.structure() {
                        if s.name() == "level" {
                            if let Ok(peaks) = s.get::<gst::glib::ValueArray>("peak") {
                                let p: &[gst::glib::Value] = &peaks;
                                let channels = p.len().max(1) as u32;
                                let left = p.first().map(norm_db).unwrap_or(0.0);
                                // Mono: mirror the single channel onto the right.
                                let right = p.get(1).map(norm_db).unwrap_or(left);
                                level_w.store(left, right, channels);
                            }
                            return gst::BusSyncReply::Drop;
                        }
                    }
                }
                gst::BusSyncReply::Pass
            });
        }

        // For a live source `Async` is a normal result; only a hard error means
        // the microphone is unavailable. Never panic — the caller toasts.
        if pipeline.set_state(gst::State::Playing).is_err() {
            let _ = pipeline.set_state(gst::State::Null);
            let _ = std::fs::remove_file(&path);
            bail!("could not start the microphone");
        }

        Ok(MicRecorder {
            pipeline,
            path,
            stopped: false,
            level,
        })
    }

    /// A cloneable handle to the live per-channel input level, so a UI poll
    /// timeout can read it without holding the recorder. Read it with
    /// [`MicLevel::read`]. Stays 0 while the `level` element is unavailable.
    pub fn level_handle(&self) -> MicLevel {
        self.level.clone()
    }

    /// Stops and **finalizes** the recording: sends EOS, waits for it to reach
    /// the sink (so the Ogg is fully written and seekable), then tears the
    /// pipeline down. Returns the file path and its probed duration (ms).
    /// **Blocking** (the EOS wait) — run off the UI thread. On failure the
    /// unfinalized file is removed and an error is returned.
    pub fn stop(mut self) -> Result<(PathBuf, i64)> {
        self.stopped = true; // we own teardown now; keep the file on success

        // EOS to the source flows downstream through encoder/muxer to the
        // filesink, which writes the trailing Ogg page. Fall back to the whole
        // pipeline if the named source is somehow absent.
        let eos = match self.pipeline.by_name("micsrc") {
            Some(src) => src.send_event(gst::event::Eos::new()),
            None => self.pipeline.send_event(gst::event::Eos::new()),
        };
        let res = if !eos {
            Err(anyhow!("could not signal end of recording"))
        } else {
            let bus = self.pipeline.bus().context("no bus")?;
            match bus.timed_pop_filtered(
                gst::ClockTime::from_seconds(5),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            ) {
                Some(msg) => match msg.view() {
                    gst::MessageView::Error(e) => Err(anyhow!("microphone error: {}", e.error())),
                    _ => Ok(()),
                },
                None => Err(anyhow!("finalizing the recording timed out")),
            }
        };

        let _ = self.pipeline.set_state(gst::State::Null);
        if let Err(e) = res {
            let _ = std::fs::remove_file(&self.path);
            return Err(e);
        }
        let duration_ms = crate::core::scanner::duration_secs(&self.path) as i64 * 1000;
        Ok((self.path.clone(), duration_ms))
    }
}

impl Drop for MicRecorder {
    fn drop(&mut self) {
        if self.stopped {
            return;
        }
        // Aborted, not stopped cleanly: the file is unfinalized and unusable, and
        // no DB row points at it yet — tear down and remove it.
        let _ = self.pipeline.set_state(gst::State::Null);
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Maps one `level` peak value (dBFS, ≤ 0) to a normalized 0.0–1.0 magnitude,
/// folding the usual ~-60 dB noise floor up to 0, so a quiet room reads near 0
/// and speech swings the meter clearly.
fn norm_db(v: &gst::glib::Value) -> f32 {
    const FLOOR_DB: f64 = -60.0;
    let db = match v.get::<f64>() {
        Ok(d) if d.is_finite() => d,
        _ => return 0.0,
    };
    (((db - FLOOR_DB) / -FLOOR_DB).clamp(0.0, 1.0)) as f32
}

/// Finds a free `<dir>/<base>.<ext>` (appends ` (2)`, … if needed), mirroring the
/// radio recorder's helper of the same purpose.
fn unique_path(dir: &Path, base: &str, ext: &str) -> PathBuf {
    let mut p = dir.join(format!("{base}.{ext}"));
    let mut i = 2;
    while p.exists() {
        p = dir.join(format!("{base} ({i}).{ext}"));
        i += 1;
    }
    p
}
