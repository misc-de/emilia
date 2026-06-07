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
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use anyhow::{anyhow, bail, Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;

/// Encoder + muxer for memos: Ogg/Opus (small, ideal for voice).
const ENC: &str = "opusenc ! oggmux";
/// Matching file extension.
const EXT: &str = "ogg";

/// Unique file-name counter so two quick recordings never collide.
static SEQ: AtomicU64 = AtomicU64::new(0);

/// Where voice memos are stored: `~/Music/Memos` (mirrors the radio
/// `recordings_dir` helper). User-visible on purpose, just like the radio
/// recordings.
pub fn memos_dir() -> PathBuf {
    let mut dir = dirs::audio_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    dir.push("Memos");
    dir
}

/// A running microphone recording. [`stop`](Self::stop) finalizes the file and
/// returns it; dropping without stopping (app quit / aborted) tears the pipeline
/// down and removes the then-unusable, unfinalized file.
pub struct MicRecorder {
    pipeline: gst::Pipeline,
    path: PathBuf,
    start: Instant,
    /// Set once [`stop`](Self::stop) has taken over teardown, so `Drop` does not
    /// also delete the (now finalized) file.
    stopped: bool,
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
        // reliable way to finalize a live capture).
        let desc = format!(
            "autoaudiosrc name=micsrc ! audioconvert ! audioresample ! {ENC} ! filesink name=sink"
        );
        let pipeline = gst::parse::launch(&desc)
            .context("building the microphone pipeline failed")?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow!("not a pipeline"))?;
        pipeline
            .by_name("sink")
            .context("no filesink")?
            .set_property("location", path.to_string_lossy().as_ref());

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
            start: Instant::now(),
            stopped: false,
        })
    }

    /// Elapsed recording time in milliseconds (for the live UI counter).
    pub fn elapsed_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
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
