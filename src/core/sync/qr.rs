//! QR-Code-Erzeugung als `gdk::Texture` (zur Anzeige in einem `gtk::Picture`).
//!
//! **Nur im Main-Thread aufrufen** – erzeugt GDK-Objekte.

use anyhow::{anyhow, Result};
use gtk::gdk;
use gtk::glib;
use gtk::prelude::*;
use qrcode::{Color, QrCode};

/// Kantenlänge eines Moduls in Pixeln.
const SCALE: usize = 8;
/// Ruhezone (Module) rund um den Code.
const QUIET: usize = 4;

/// Rendert `text` als schwarz-weißen QR-Code-Textur.
pub fn render_qr(text: &str) -> Result<gdk::Texture> {
    let code = QrCode::new(text.as_bytes()).map_err(|e| anyhow!("QR-Code zu lang: {e}"))?;
    let modules = code.width();
    let colors = code.to_colors();
    let total = modules + 2 * QUIET;
    let size = total * SCALE;

    // RGB-Puffer, weißer Hintergrund.
    let mut buf = vec![255u8; size * size * 3];
    for my in 0..modules {
        for mx in 0..modules {
            if matches!(colors[my * modules + mx], Color::Dark) {
                for dy in 0..SCALE {
                    let py = (my + QUIET) * SCALE + dy;
                    for dx in 0..SCALE {
                        let px = (mx + QUIET) * SCALE + dx;
                        let idx = (py * size + px) * 3;
                        buf[idx] = 0;
                        buf[idx + 1] = 0;
                        buf[idx + 2] = 0;
                    }
                }
            }
        }
    }

    let bytes = glib::Bytes::from_owned(buf);
    let texture = gdk::MemoryTexture::new(
        size as i32,
        size as i32,
        gdk::MemoryFormat::R8g8b8,
        &bytes,
        size * 3,
    );
    Ok(texture.upcast())
}
