//! GStreamer-Playback über `playbin3`.

use anyhow::{anyhow, Result};
use gstreamer as gst;
use gstreamer::prelude::*;

pub struct Player {
    playbin: gst::Element,
    /// 10-Band-Equalizer als `audio-filter` (falls das Plugin vorhanden ist).
    equalizer: Option<gst::Element>,
}

impl Player {
    pub fn new() -> Result<Self> {
        gst::init()?;
        let playbin = gst::ElementFactory::make("playbin3")
            .build()
            .map_err(|_| anyhow!("playbin3 nicht verfügbar – ist gstreamer installiert?"))?;

        // Equalizer als Audio-Filter einhängen (optional – nur wenn verfügbar).
        let equalizer = gst::ElementFactory::make("equalizer-10bands").build().ok();
        match &equalizer {
            Some(eq) => playbin.set_property("audio-filter", eq),
            None => tracing::warn!("equalizer-10bands nicht verfügbar – EQ deaktiviert"),
        }

        Ok(Self { playbin, equalizer })
    }

    /// Setzt die 10 Band-Verstärkungen (dB, jeweils −24…+12) live.
    pub fn set_eq_bands(&self, bands: &[f64; 10]) {
        let Some(eq) = &self.equalizer else {
            return;
        };
        for (i, gain) in bands.iter().enumerate() {
            eq.set_property(&format!("band{i}"), gain.clamp(-24.0, 12.0));
        }
    }

    /// Lädt eine lokale Datei und startet die Wiedergabe. Ist `resume_ms > 0`,
    /// wird vor dem Start an diese Position gesprungen (Resume bei Hörspielen).
    pub fn play_file(&self, path: &str, resume_ms: i64) -> Result<()> {
        let uri = gst::glib::filename_to_uri(path, None)
            .map_err(|e| anyhow!("Ungültiger Pfad {path}: {e}"))?;
        // playbin3 liest die `uri` nur beim Zustandswechsel neu ein – läuft schon
        // ein Titel, muss die Pipeline erst zurückgesetzt werden, sonst spielt
        // der alte Titel weiter.
        self.playbin
            .set_state(gst::State::Ready)
            .map_err(|e| anyhow!("Konnte Pipeline nicht zurücksetzen: {e}"))?;
        self.playbin.set_property("uri", uri.as_str());
        if resume_ms > 0 {
            // Für einen zuverlässigen Sprung muss die Pipeline erst prerollen:
            // kurz nach PAUSED, auf den Preroll warten (bei lokalen Dateien nur
            // wenige Millisekunden), dann an die Resume-Position springen.
            self.playbin
                .set_state(gst::State::Paused)
                .map_err(|e| anyhow!("Konnte Pipeline nicht vorbereiten: {e}"))?;
            let _ = self.playbin.state(gst::ClockTime::from_seconds(5));
            let _ = self.seek_ms(resume_ms);
        }
        self.playbin
            .set_state(gst::State::Playing)
            .map_err(|e| anyhow!("Konnte nicht abspielen: {e}"))?;
        Ok(())
    }

    /// Registriert einen Callback, der bei Titelende (EOS) aufgerufen wird –
    /// für das Weiterschalten in der Warteschlange. Läuft im Main-Loop.
    pub fn connect_eos<F: Fn() + 'static>(&self, on_eos: F) {
        if let Some(bus) = self.playbin.bus() {
            let _ = bus.add_watch_local(move |_, msg| {
                match msg.view() {
                    gst::MessageView::Eos(_) => on_eos(),
                    gst::MessageView::Error(err) => {
                        tracing::error!(
                            "GStreamer-Fehler: {} ({:?})",
                            err.error(),
                            err.debug()
                        );
                    }
                    _ => {}
                }
                gst::glib::ControlFlow::Continue
            });
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

    /// Springt zur angegebenen Position (z. B. für Resume bei Hörspielen).
    pub fn seek_ms(&self, ms: i64) -> Result<()> {
        self.playbin
            .seek_simple(
                gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                gst::ClockTime::from_mseconds(ms.max(0) as u64),
            )
            .map_err(|e| anyhow!("Seek fehlgeschlagen: {e}"))?;
        Ok(())
    }
}
