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
//! Camera access prefers the **XDG camera portal** (a PipeWire fd via
//! `pipewiresrc`), so the desktop Flatpak needs no `--device=all` (raw
//! `/dev/video*`). If the portal is unavailable/denied/has no camera, it falls
//! back to a direct source:
//! - **Desktop / Librem 5**: `autovideosrc`/`v4l2src` over `/dev/video*` — the
//!   Librem 5 exposes its camera through the mainline V4L2/libcamera stack.
//! - **Halium (FuriOS, Droidian)**: the Android camera HAL is bridged to V4L2
//!   loopback devices (`/dev/video*`) by the host's `droidcam2v4l2` service, so
//!   `v4l2src`/`autovideosrc` reach it from inside the sandbox — `droidcamsrc`
//!   (a host-only gst-droid plugin) is neither present nor needed there. The
//!   portal does **not** reliably expose these loopbacks, so that build sets
//!   `EMILIA_CAMERA_SRC` to bypass it (and keeps `--device=all`).
//!
//! Set `EMILIA_CAMERA_SRC` to bypass the portal and pick a direct source for a
//! specific port (e.g. `v4l2src device=/dev/video2` or `libcamerasrc`).
//!
//! **Call only on the main thread** – pipeline, timer, bus watch and paintable
//! all hang off the GLib main loop or GDK.

use std::collections::HashMap;
use std::os::fd::AsRawFd;
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
    /// Keeps the camera-portal PipeWire fd open for the pipeline's lifetime
    /// (`pipewiresrc` reads from it). `None` when the direct source is used.
    _camera_fd: Option<zbus::zvariant::OwnedFd>,
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

        // Prefer the XDG **camera portal**: it hands back a PipeWire fd, so the
        // desktop Flatpak needs no `--device=all` (raw `/dev/video*`). An explicit
        // `EMILIA_CAMERA_SRC` skips the portal and uses the direct source — needed
        // on Halium (FuriOS/Droidian), where the portal does not expose the V4L2
        // loopbacks. If the portal is absent, denied or has no camera, fall back to
        // the direct source (which still works under `--device=all`).
        let camera_fd = if std::env::var_os("EMILIA_CAMERA_SRC").is_some() {
            None
        } else {
            portal_camera_fd()
        };
        let src = match &camera_fd {
            Some(fd) => format!("pipewiresrc fd={}", fd.as_raw_fd()),
            None => camera_source(),
        };
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
                _camera_fd: camera_fd,
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

/// Requests camera access through the XDG **camera portal**
/// (`org.freedesktop.portal.Camera`) and returns an open PipeWire remote fd, fed
/// to `pipewiresrc fd=…`. Returns `None` if the portal is unavailable, no camera
/// is present, or access is denied/cancelled — the caller then falls back to the
/// direct source.
///
/// Uses the already-present `zbus` (no extra dependency) and its blocking API, so
/// it fits the synchronous, main-thread `Scanner::start`. The `Response` signal
/// is subscribed on the *predicted* request path **before** `AccessCamera` is
/// called, so the grant can never be missed (per the portal Request protocol).
/// On first use this blocks the main loop briefly while the permission dialog is
/// shown; later calls return from the remembered decision.
fn portal_camera_fd() -> Option<zbus::zvariant::OwnedFd> {
    use zbus::blocking::{Connection, Proxy};
    use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

    let conn = Connection::session().ok()?;

    // Predicted request path: /…/request/SENDER/TOKEN, SENDER = the unique name
    // without the leading ':' and with '.' → '_'.
    let token = "emilia_cam";
    let sender = conn
        .unique_name()?
        .trim_start_matches(':')
        .replace('.', "_");
    let req_path = format!("/org/freedesktop/portal/desktop/request/{sender}/{token}");

    let camera = Proxy::new(
        &conn,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.Camera",
    )
    .ok()?;

    // Don't prompt if there is no camera at all.
    if !camera
        .get_property::<bool>("IsCameraPresent")
        .unwrap_or(false)
    {
        return None;
    }

    // Subscribe to the Response signal first (race-free).
    let request = Proxy::new(
        &conn,
        "org.freedesktop.portal.Desktop",
        req_path.as_str(),
        "org.freedesktop.portal.Request",
    )
    .ok()?;
    let mut responses = request.receive_signal("Response").ok()?;

    let mut options: HashMap<&str, Value> = HashMap::new();
    options.insert("handle_token", Value::from(token));
    let _handle: OwnedObjectPath = camera.call("AccessCamera", &(options,)).ok()?;

    // Block until the portal answers (user grant or remembered decision).
    let msg = responses.next()?;
    let (response, _results): (u32, HashMap<String, OwnedValue>) = msg.body().deserialize().ok()?;
    if response != 0 {
        return None; // denied or cancelled
    }

    let opts: HashMap<&str, Value> = HashMap::new();
    camera.call("OpenPipeWireRemote", &(opts,)).ok()
}

/// The direct camera source sub-pipeline, used when the portal is unavailable or
/// explicitly bypassed. Honors `EMILIA_CAMERA_SRC` (ports/edge cases), otherwise
/// picks the first available element. `autovideosrc` already wraps the platform's
/// V4L2 source, which also covers the Halium `droidcam2v4l2` bridge and the
/// Librem 5 mainline camera.
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
