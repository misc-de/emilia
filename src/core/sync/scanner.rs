//! Camera QR scanner via a GStreamer pipeline (`zxing` plugin) with
//! live preview (`gtk4paintablesink`).
//!
//! **Call only on the main thread** – pipeline, bus watch and paintable hang
//! off the GLib main loop or GDK.

use anyhow::{anyhow, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gtk::gdk;

/// Running scanner pipeline. Stopped cleanly when dropped.
pub struct Scanner {
    pipeline: gst::Pipeline,
    /// Keeps the bus watch alive (analogous to the player).
    _bus_watch: gst::bus::BusWatchGuard,
}

impl Scanner {
    /// Starts camera + decoder. `on_decode` is called on the main loop
    /// as soon as a QR code is detected (may fire multiple times). Returns the
    /// pipeline and – if available – a preview `Paintable` for a
    /// `gtk::Picture`.
    ///
    /// The live preview (`gtk4paintablesink` from gst-plugins-rs) is optional:
    /// if the plugin is missing, scanning happens without a camera image (the code is still
    /// detected as long as `zxing` and a camera source are present).
    pub fn start<F>(on_decode: F) -> Result<(Scanner, Option<gdk::Paintable>)>
    where
        F: Fn(String) + 'static,
    {
        gst::init()?;

        let have_preview = gst::ElementFactory::find("gtk4paintablesink").is_some();
        let desc = if have_preview {
            "autovideosrc ! videoconvert ! tee name=t \
             t. ! queue leaky=downstream max-size-buffers=2 ! videoconvert ! zxing ! fakesink sync=false \
             t. ! queue leaky=downstream max-size-buffers=2 ! gtk4paintablesink name=preview"
        } else {
            "autovideosrc ! videoconvert ! zxing ! fakesink sync=false"
        };

        let element = gst::parse::launch_full(desc, None, gst::ParseFlags::empty())
            .map_err(|e| anyhow!("camera/QR plugins not available: {e}"))?;
        let pipeline = element
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow!("pipeline could not be created"))?;

        let paintable = if have_preview {
            pipeline
                .by_name("preview")
                .map(|sink| sink.property::<gdk::Paintable>("paintable"))
        } else {
            None
        };

        let bus = pipeline.bus().ok_or_else(|| anyhow!("GStreamer bus missing"))?;
        let guard = bus
            .add_watch_local(move |_, msg| {
                if let Some(s) = msg.structure() {
                    if s.name().as_str() == "barcode" {
                        if let Ok(symbol) = s.get::<String>("symbol") {
                            on_decode(symbol);
                        }
                    }
                }
                gst::glib::ControlFlow::Continue
            })
            .map_err(|_| anyhow!("bus watch failed"))?;

        pipeline
            .set_state(gst::State::Playing)
            .map_err(|_| anyhow!("camera could not be started"))?;

        Ok((
            Scanner {
                pipeline,
                _bus_watch: guard,
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
        self.stop();
    }
}
