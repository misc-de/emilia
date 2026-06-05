//! Camera QR scanner: a GStreamer pipeline feeds raw grayscale frames to a
//! **pure-Rust** QR decoder ([`rqrr`]), with an optional live preview
//! (`gtk4paintablesink`).
//!
//! Decoding deliberately does **not** use the `zxing` GStreamer element: that
//! plugin is absent from the GNOME Flatpak runtime, so it is unavailable on
//! desktop Flatpak builds and on Halium phones (FuriOS, Droidian), where Emilia
//! runs as a Flatpak. Frames are pulled from an `appsink` and decoded in
//! process, which works wherever a camera *source* is available — no extra
//! plugin. The pull/decode runs on a short **main-thread** timer (the
//! component's relm4 sender is not `Send`, so it cannot cross to a streaming
//! thread); at 640×480 rqrr is cheap enough for that.
//!
//! Camera sources by platform (all read through the GNOME runtime's plugins):
//! - **Desktop / Librem 5**: `autovideosrc`/`v4l2src` over `/dev/video*` — the
//!   Librem 5 exposes its camera through the mainline V4L2/libcamera stack.
//! - **Halium (FuriOS, Droidian)**: the Android camera HAL is bridged to V4L2
//!   loopback devices (`/dev/video*`) by the host's `droidcam2v4l2` service, so
//!   `v4l2src`/`autovideosrc` reach it from inside the sandbox — `droidcamsrc`
//!   (a host-only gst-droid plugin) is neither present nor needed there.
//!
//! Set `EMILIA_CAMERA_SRC` to override the source for a specific port (e.g.
//! `v4l2src device=/dev/video2` or `libcamerasrc`).
//!
//! **Call only on the main thread** – pipeline, timer, bus watch and paintable
//! all hang off the GLib main loop or GDK.

use std::time::Duration;

use anyhow::{anyhow, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gtk::gdk;

/// How often the newest camera frame is polled and decoded (main thread).
const POLL_INTERVAL: Duration = Duration::from_millis(150);

/// Running scanner pipeline. Stopped cleanly when dropped.
pub struct Scanner {
    pipeline: gst::Pipeline,
    /// Keeps the bus watch alive (analogous to the player).
    _bus_watch: gst::bus::BusWatchGuard,
    /// The main-loop poll timer; removed on drop so it stops firing.
    decode_source: Option<gst::glib::SourceId>,
}

impl Scanner {
    /// Starts camera + decoder. `on_decode` is called on the main loop as soon
    /// as a QR code is detected (may fire repeatedly). Returns the pipeline and –
    /// if `gtk4paintablesink` is available – a preview `Paintable` for a
    /// `gtk::Picture`.
    ///
    /// Errors carry a human-readable reason (no camera / plugins missing) so the
    /// caller can show it instead of an empty window.
    pub fn start<F>(on_decode: F) -> Result<(Scanner, Option<gdk::Paintable>)>
    where
        F: FnMut(String) + 'static,
    {
        gst::init()?;

        let src = camera_source();
        let have_preview = gst::ElementFactory::find("gtk4paintablesink").is_some();

        // Down-scale + cap the framerate: QR codes decode fine at 640×480 and it
        // keeps per-frame CPU low on phones. The decode branch is single-plane
        // GRAY8 for rqrr; `drop=true max-buffers=1` keeps only the newest frame.
        let common = "videoconvert ! videoscale ! videorate ! \
                      video/x-raw,width=640,height=480,framerate=10/1";
        let decode_branch = "queue leaky=downstream max-size-buffers=2 ! videoconvert ! \
                             video/x-raw,format=GRAY8 ! \
                             appsink name=decode max-buffers=1 drop=true sync=false";
        let desc = if have_preview {
            format!(
                "{src} ! {common} ! tee name=t \
                 t. ! {decode_branch} \
                 t. ! queue leaky=downstream max-size-buffers=2 ! gtk4paintablesink name=preview"
            )
        } else {
            format!("{src} ! {common} ! {decode_branch}")
        };

        let pipeline = gst::parse::launch(&desc)
            .map_err(|e| anyhow!("camera/QR plugins not available: {e}"))?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow!("pipeline could not be created"))?;

        let appsink = pipeline
            .by_name("decode")
            .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
            .ok_or_else(|| anyhow!("appsink missing"))?;

        let paintable = if have_preview {
            pipeline
                .by_name("preview")
                .map(|sink| sink.property::<gdk::Paintable>("paintable"))
        } else {
            None
        };

        // Log fatal pipeline errors (no camera, permission denied) so a black
        // preview has a trace; start-time failures surface via the Err below.
        let bus = pipeline
            .bus()
            .ok_or_else(|| anyhow!("GStreamer bus missing"))?;
        let guard = bus
            .add_watch_local(move |_, msg| {
                if let gst::MessageView::Error(err) = msg.view() {
                    tracing::warn!("Camera pipeline error: {} ({:?})", err.error(), err.debug());
                }
                gst::glib::ControlFlow::Continue
            })
            .map_err(|_| anyhow!("bus watch failed"))?;

        pipeline
            .set_state(gst::State::Playing)
            .map_err(|_| anyhow!("camera could not be started (no accessible camera?)"))?;

        // Poll + decode the newest frame on the main thread.
        let mut on_decode = on_decode;
        let decode_source = gst::glib::timeout_add_local(POLL_INTERVAL, move || {
            if let Some(sample) = appsink.try_pull_sample(gst::ClockTime::ZERO) {
                if let Some(text) = decode_sample(&sample) {
                    if !text.is_empty() {
                        on_decode(text);
                    }
                }
            }
            gst::glib::ControlFlow::Continue
        });

        Ok((
            Scanner {
                pipeline,
                _bus_watch: guard,
                decode_source: Some(decode_source),
            },
            paintable,
        ))
    }

    /// Stops the pipeline (idempotent).
    pub fn stop(&self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

impl Drop for Scanner {
    fn drop(&mut self) {
        if let Some(id) = self.decode_source.take() {
            id.remove();
        }
        self.stop();
    }
}

/// The camera source sub-pipeline. Honors `EMILIA_CAMERA_SRC` (ports/edge
/// cases), otherwise picks the first available element. `autovideosrc` already
/// wraps the platform's V4L2 source, which also covers the Halium
/// `droidcam2v4l2` bridge and the Librem 5 mainline camera.
fn camera_source() -> String {
    if let Ok(s) = std::env::var("EMILIA_CAMERA_SRC") {
        let s = s.trim();
        if !s.is_empty() {
            return s.to_string();
        }
    }
    for factory in ["autovideosrc", "pipewiresrc", "v4l2src", "libcamerasrc"] {
        if gst::ElementFactory::find(factory).is_some() {
            return factory.to_string();
        }
    }
    "autovideosrc".to_string()
}

/// Tries to decode a QR code from a GRAY8 video sample. `None` if no readable
/// code is in the frame.
fn decode_sample(sample: &gst::Sample) -> Option<String> {
    let buffer = sample.buffer()?;
    let caps = sample.caps()?;
    let s = caps.structure(0)?;
    let width = s.get::<i32>("width").ok()? as usize;
    let height = s.get::<i32>("height").ok()? as usize;
    if width == 0 || height == 0 {
        return None;
    }
    let map = buffer.map_readable().ok()?;
    let data = map.as_slice();
    // GRAY8 rows may be padded; derive the real stride from the buffer size.
    let stride = data.len().checked_div(height)?;
    if stride < width {
        return None;
    }
    let mut img =
        rqrr::PreparedImage::prepare_from_greyscale(width, height, |x, y| data[y * stride + x]);
    for grid in img.detect_grids() {
        if let Ok((_meta, content)) = grid.decode() {
            if !content.is_empty() {
                return Some(content);
            }
        }
    }
    None
}
