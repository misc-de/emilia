//! Gemeinsame UI-Helfer.

use std::cell::RefCell;
use std::collections::HashMap;

use adw::prelude::*;
use relm4::{adw, gtk};


/// Kantenlänge der gecachten Listen-Thumbnails. Die Karten zeigen 48 px; 128 px
/// deckt HiDPI ab und hält den Cache klein (≈64 KB statt ≈1 MB je Vollbild-Cover).
const THUMB_PX: i32 = 128;

thread_local! {
    /// Prozessweiter Cache dekodierter Listen-Thumbnails (Dateipfad → Textur).
    /// Wird ausschließlich auf dem UI-Thread benutzt (Karten-`init_model`/`update_cmd`),
    /// daher genügt `thread_local` ohne Sperren. Verhindert wiederholtes Dekodieren
    /// und das Aufblitzen der Platzhalter bei jedem Listen-Neuaufbau.
    static THUMB_CACHE: RefCell<HashMap<String, gtk::gdk::Texture>> = RefCell::new(HashMap::new());
}

/// Bereits gecachtes Thumbnail (falls vorhanden) – sofort, ohne Dekodieren.
pub fn cached_thumb(path: &str) -> Option<gtk::gdk::Texture> {
    THUMB_CACHE.with(|c| c.borrow().get(path).cloned())
}

/// Thumbnail aus dem Cache oder – bei Cache-Miss – **synchron** herunterskaliert
/// dekodiert und gecacht. Gedacht für bedarfsweise geöffnete, kurze Listen
/// (Interpreten-/Album-Unterseiten); lange Listenkarten laden ihr Cover
/// stattdessen asynchron über [`cover_frame`] + [`set_cover_texture`].
pub fn thumb_cached(path: &str) -> Option<gtk::gdk::Texture> {
    if let Some(texture) = cached_thumb(path) {
        return Some(texture);
    }
    let texture = decode_thumb(path)?;
    store_thumb(path.to_string(), texture.clone());
    Some(texture)
}

/// Legt ein dekodiertes Thumbnail im Cache ab.
pub fn store_thumb(path: String, texture: gtk::gdk::Texture) {
    THUMB_CACHE.with(|c| {
        c.borrow_mut().insert(path, texture);
    });
}

/// Dekodiert eine Bilddatei **herunterskaliert** auf Thumbnail-Größe und erzeugt
/// daraus eine Textur. Gedacht für den Hintergrund-Thread (kein Widget-/UI-Bezug);
/// liefert `None` bei fehlender/fehlerhafter Datei. Skaliertes Dekodieren ist
/// schneller als das Vollbild und hält den Cache speicherschonend.
pub fn decode_thumb(path: &str) -> Option<gtk::gdk::Texture> {
    let pixbuf = gtk::gdk_pixbuf::Pixbuf::from_file_at_scale(path, THUMB_PX, THUMB_PX, true).ok()?;
    Some(gtk::gdk::Texture::for_pixbuf(&pixbuf))
}

/// Leerer, quadratischer und abgerundeter Bildrahmen in Karten-Optik mit
/// Platzhalter-Icon. Das eigentliche Cover/Foto wird – sofern vorhanden –
/// asynchron dekodiert und per [`set_cover_texture`] nachgereicht, damit der
/// UI-Thread beim Aufbau langer Listen nicht durch das Bild-Dekodieren blockiert.
///
/// `AspectFrame` erzwingt 1:1, `content_fit = Cover` schneidet das Bild quadratisch
/// zu, `overflow = Hidden` rundet die Ecken.
pub fn cover_frame(placeholder_icon: &str, size: i32) -> gtk::AspectFrame {
    // Großes Detail-Cover: AspectFrame schneidet das Bild formatfüllend auf ein
    // Quadrat zu. (Für kleine Listen-Thumbnails siehe `thumb_frame`.)
    let frame = gtk::AspectFrame::new(0.0, 0.5, 1.0, false);
    frame.set_size_request(size, size);
    frame.set_overflow(gtk::Overflow::Hidden);
    frame.set_halign(gtk::Align::Start);
    frame.set_valign(gtk::Align::Center);
    frame.set_hexpand(false);
    frame.set_vexpand(false);
    frame.add_css_class("card");
    set_cover_placeholder(&frame, placeholder_icon, size);
    frame
}

/// Fester, quadratischer Thumbnail-Rahmen für Listen (`adw::Bin` folgt der
/// natürlichen Kindgröße und wächst – anders als `AspectFrame` – in höheren,
/// mehrzeiligen Zeilen NICHT mit). Bild wird per [`set_cover_thumb`] gesetzt.
pub fn thumb_frame(placeholder_icon: &str, size: i32) -> adw::Bin {
    let bin = adw::Bin::new();
    bin.set_size_request(size, size);
    bin.set_overflow(gtk::Overflow::Hidden);
    bin.set_halign(gtk::Align::Start);
    bin.set_valign(gtk::Align::Center);
    bin.set_hexpand(false);
    bin.set_vexpand(false);
    bin.add_css_class("card");
    let img = gtk::Image::from_icon_name(placeholder_icon);
    img.set_pixel_size(size);
    img.add_css_class("dim-label");
    bin.set_child(Some(&img));
    bin
}

/// Setzt ein Platzhalter-Icon (füllt das Quadrat) in den Rahmen.
pub fn set_cover_placeholder(frame: &gtk::AspectFrame, placeholder_icon: &str, size: i32) {
    let img = gtk::Image::from_icon_name(placeholder_icon);
    img.set_pixel_size(size);
    img.add_css_class("dim-label");
    frame.set_child(Some(&img));
}

/// Setzt das (ggf. im Hintergrund dekodierte) Bild in den Rahmen.
pub fn set_cover_texture(frame: &gtk::AspectFrame, texture: &gtk::gdk::Texture) {
    let pic = gtk::Picture::for_paintable(texture);
    pic.set_content_fit(gtk::ContentFit::Cover);
    pic.set_can_shrink(true);
    frame.set_child(Some(&pic));
}

/// Setzt das Bild als **fest dimensioniertes** Thumbnail (über `gtk::Image` mit
/// `pixel_size`). Anders als ein `Picture` wächst es nicht mit der Zeilenhöhe –
/// so bleiben Listen-Cover immer gleich groß (z. B. 48 px), egal ob die Zeile
/// ein- oder zweizeilig ist. Die Größe wird aus dem Rahmen übernommen.
pub fn set_cover_thumb(bin: &adw::Bin, texture: &gtk::gdk::Texture) {
    let size = bin.height_request().max(1);
    // Auf Anzeigegröße herunterskalieren: Ein Paintable behält sonst seine
    // Originalgröße als „natürliche" Größe, wodurch der Rahmen in höheren
    // (mehrzeiligen) Zeilen mitwächst und den Titeltext nach rechts schiebt.
    // `pixbuf_get_from_texture` ist seit GTK 4.12 deprecated; bewusst beibehalten,
    // bis ein deprecation-freier Downscale visuell verifiziert ist (Thumbnail-Größe).
    #[allow(deprecated)]
    let small = gtk::gdk::pixbuf_get_from_texture(texture)
        .and_then(|pb| pb.scale_simple(size, size, gtk::gdk_pixbuf::InterpType::Bilinear))
        .map(|pb| gtk::gdk::Texture::for_pixbuf(&pb));
    let img = match &small {
        Some(t) => gtk::Image::from_paintable(Some(t)),
        None => gtk::Image::from_paintable(Some(texture)),
    };
    img.set_pixel_size(size);
    bin.set_child(Some(&img));
}

/// Bild oder Platzhalter als **quadratisches**, abgerundetes Bild in Karten-Optik
/// – einheitlich für Cover/Fotos und ihre Platzhalter. Für Einzelbilder (z. B. die
/// Detailansicht), bei denen die Textur bereits vorliegt; Listenkarten laden ihr
/// Cover stattdessen asynchron über [`cover_frame`] + [`set_cover_texture`].
pub fn rounded_image(
    texture: Option<&gtk::gdk::Texture>,
    placeholder_icon: &str,
    size: i32,
) -> gtk::Widget {
    // Kleine Listen-Thumbnails: fester `adw::Bin`-Rahmen (wächst nicht mit der
    // Zeilenhöhe). Große Cover (Detailansicht): AspectFrame mit Zuschnitt.
    if size <= 64 {
        let bin = thumb_frame(placeholder_icon, size);
        if let Some(t) = texture {
            set_cover_thumb(&bin, t);
        }
        bin.upcast()
    } else {
        let frame = cover_frame(placeholder_icon, size);
        if let Some(t) = texture {
            set_cover_texture(&frame, t);
        }
        frame.upcast()
    }
}
