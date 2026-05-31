//! Webcam-QR-Scanner über eine GStreamer-Pipeline (`zxing`-Plugin) mit
//! Live-Vorschau (`gtk4paintablesink`).
//!
//! **Nur im Main-Thread aufrufen** – Pipeline, Bus-Watch und Paintable hängen
//! am GLib-Main-Loop bzw. an GDK.

use anyhow::{anyhow, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gtk::gdk;

/// Laufende Scanner-Pipeline. Wird beim Verwerfen sauber gestoppt.
pub struct Scanner {
    pipeline: gst::Pipeline,
    /// Hält die Bus-Überwachung am Leben (analog zum Player).
    _bus_watch: gst::bus::BusWatchGuard,
}

impl Scanner {
    /// Startet Kamera + Dekoder. `on_decode` wird im Main-Loop aufgerufen,
    /// sobald ein QR-Code erkannt wird (kann mehrfach feuern). Liefert die
    /// Pipeline und – falls verfügbar – ein Vorschau-`Paintable` für ein
    /// `gtk::Picture`.
    ///
    /// Die Live-Vorschau (`gtk4paintablesink` aus gst-plugins-rs) ist optional:
    /// fehlt das Plugin, wird ohne Kamerabild gescannt (der Code wird trotzdem
    /// erkannt, solange `zxing` und eine Kameraquelle vorhanden sind).
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
            .map_err(|e| anyhow!("Kamera/QR-Plugins nicht verfügbar: {e}"))?;
        let pipeline = element
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow!("Pipeline konnte nicht erstellt werden"))?;

        let paintable = if have_preview {
            pipeline
                .by_name("preview")
                .map(|sink| sink.property::<gdk::Paintable>("paintable"))
        } else {
            None
        };

        let bus = pipeline.bus().ok_or_else(|| anyhow!("GStreamer-Bus fehlt"))?;
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
            .map_err(|_| anyhow!("Bus-Überwachung fehlgeschlagen"))?;

        pipeline
            .set_state(gst::State::Playing)
            .map_err(|_| anyhow!("Kamera konnte nicht gestartet werden"))?;

        Ok((
            Scanner {
                pipeline,
                _bus_watch: guard,
            },
            paintable,
        ))
    }

    /// Stoppt die Pipeline (idempotent).
    pub fn stop(&self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

impl Drop for Scanner {
    fn drop(&mut self) {
        self.stop();
    }
}
