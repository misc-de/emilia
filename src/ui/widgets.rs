//! Shared UI helpers.

use std::cell::RefCell;
use std::collections::HashMap;

use adw::prelude::*;
use relm4::{adw, gtk};

/// Edge length of the cached list thumbnails. The cards show 48 px; 128 px
/// covers HiDPI and keeps the cache small (≈64 KB instead of ≈1 MB per full-size cover).
const THUMB_PX: i32 = 128;

/// Upper bound on cached thumbnails. Each entry is a 128 px texture (≈64 KB),
/// so a full cache stays well under ~70 MB. Without a bound the map grew for the
/// whole process lifetime — one entry per cover ever shown.
const THUMB_CACHE_MAX: usize = 1024;
/// How many least-recently-used entries to drop once the cap is hit, so the
/// O(n) eviction scan runs only once every `THUMB_CACHE_EVICT` inserts past the
/// cap instead of on every insert.
const THUMB_CACHE_EVICT: usize = 256;

/// Size-bounded LRU map: each value carries the access `tick` of its last use.
struct ThumbCache {
    map: HashMap<String, (gtk::gdk::Texture, u64)>,
    tick: u64,
}

thread_local! {
    /// Process-wide, **size-bounded** cache of decoded list thumbnails
    /// (file path → texture). Used exclusively on the UI thread (card
    /// `init_model`/`update_cmd`), so `thread_local` without locks suffices.
    /// Prevents repeated decoding and the flashing of placeholders on every list
    /// rebuild; evicts the least-recently-used entries once it exceeds the cap.
    static THUMB_CACHE: RefCell<ThumbCache> =
        RefCell::new(ThumbCache { map: HashMap::new(), tick: 0 });
}

/// Already cached thumbnail (if present) – immediately, without decoding.
/// A cache hit refreshes the entry's recency so it survives eviction longer.
pub fn cached_thumb(path: &str) -> Option<gtk::gdk::Texture> {
    THUMB_CACHE.with(|c| {
        let mut c = c.borrow_mut();
        c.tick += 1;
        let tick = c.tick;
        c.map.get_mut(path).map(|e| {
            e.1 = tick;
            e.0.clone()
        })
    })
}

/// Thumbnail from the cache or – on a cache miss – decoded **synchronously**
/// downscaled and cached. Intended for short lists opened on demand
/// (artist/album subpages); long list cards instead load their cover
/// asynchronously via [`cover_frame`] + [`set_cover_texture`].
pub fn thumb_cached(path: &str) -> Option<gtk::gdk::Texture> {
    if let Some(texture) = cached_thumb(path) {
        return Some(texture);
    }
    let texture = decode_thumb(path)?;
    store_thumb(path.to_string(), texture.clone());
    Some(texture)
}

/// Stores a decoded thumbnail in the cache, evicting the least-recently-used
/// entries in one batch once the size cap is exceeded.
pub fn store_thumb(path: String, texture: gtk::gdk::Texture) {
    THUMB_CACHE.with(|c| {
        let mut c = c.borrow_mut();
        c.tick += 1;
        let tick = c.tick;
        c.map.insert(path, (texture, tick));
        if c.map.len() > THUMB_CACHE_MAX {
            // Drop the `THUMB_CACHE_EVICT` entries with the oldest access tick.
            // Ticks are unique per entry (every access/store bumps the counter),
            // so the cutoff removes exactly that many.
            let mut ticks: Vec<u64> = c.map.values().map(|(_, t)| *t).collect();
            let cut = THUMB_CACHE_EVICT.min(ticks.len().saturating_sub(1));
            ticks.select_nth_unstable(cut);
            let cutoff = ticks[cut];
            c.map.retain(|_, (_, t)| *t > cutoff);
        }
    });
}

/// Decodes an image file **downscaled** so the longer edge is at most `px`,
/// preserving the aspect ratio. Much faster and lighter than decoding the full
/// resolution when only a small widget shows the image. `None` on a
/// missing/faulty file.
pub fn decode_scaled(path: &str, px: i32) -> Option<gtk::gdk::Texture> {
    let pixbuf = gtk::gdk_pixbuf::Pixbuf::from_file_at_scale(path, px, px, true).ok()?;
    Some(gtk::gdk::Texture::for_pixbuf(&pixbuf))
}

/// Decodes an image file **downscaled** to thumbnail size and creates a texture
/// from it. Intended for the background thread (no widget/UI reference);
/// returns `None` for a missing/faulty file. Scaled decoding is faster than the
/// full size and keeps the cache memory-friendly.
pub fn decode_thumb(path: &str) -> Option<gtk::gdk::Texture> {
    decode_scaled(path, THUMB_PX)
}

/// Empty, square and rounded image frame in card style with a placeholder icon.
/// The actual cover/photo is – if present – decoded asynchronously and supplied
/// via [`set_cover_texture`], so that the UI thread is not blocked by image
/// decoding while building long lists.
///
/// `AspectFrame` enforces 1:1, `content_fit = Cover` crops the image to a square,
/// `overflow = Hidden` rounds the corners.
pub fn cover_frame(placeholder_icon: &str, size: i32) -> gtk::AspectFrame {
    // Large detail cover: AspectFrame crops the image to fill a square.
    // (For small list thumbnails see `thumb_frame`.)
    let frame = gtk::AspectFrame::new(0.0, 0.5, 1.0, false);
    frame.set_size_request(size, size);
    frame.set_overflow(gtk::Overflow::Hidden);
    // Large covers are only ever used centred in detail dialogs/carousels, so
    // centre by default. This keeps every detail view from having to override
    // the alignment itself — a step that kept getting forgotten on new/async
    // code paths and left the cover stuck to the left edge. (Small list/header
    // thumbnails use `thumb_frame`, which stays `Start`.)
    frame.set_halign(gtk::Align::Center);
    frame.set_valign(gtk::Align::Center);
    frame.set_hexpand(false);
    frame.set_vexpand(false);
    frame.add_css_class("card");
    set_cover_placeholder(&frame, placeholder_icon, size);
    frame
}

/// Fixed, square thumbnail frame for lists (`adw::Bin` follows the natural child
/// size and – unlike `AspectFrame` – does NOT grow with taller, multi-line
/// rows). Image is set via [`set_cover_thumb`].
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

/// Wraps a cover/photo in an overlay with a red "Disconnected" badge when the
/// associated source (Nextcloud) is currently offline. Otherwise the widget is
/// returned unchanged.
pub fn offline_overlay(child: &impl IsA<gtk::Widget>, offline: bool) -> gtk::Widget {
    let child = child.clone().upcast::<gtk::Widget>();
    if !offline {
        return child;
    }
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&child));
    let badge = gtk::Image::from_icon_name("network-offline-symbolic");
    badge.add_css_class("emilia-offline");
    badge.set_halign(gtk::Align::End);
    badge.set_valign(gtk::Align::Start);
    badge.set_pixel_size(14);
    overlay.add_overlay(&badge);
    overlay.upcast()
}

/// Stops a text field from being **auto-focused** when its dialog/page is shown
/// or switched to. On mobile an auto-focused entry immediately pops the
/// on-screen keyboard, which is disruptive when merely scrolling through the
/// settings or paging through dialogs. The field (and its delegate `GtkText`)
/// is made non-focusable; the first pointer press — handled in the capture
/// phase, before the entry itself reacts — restores focusability and focuses
/// it, so tapping a field to type still works exactly as before. Trade-off:
/// the field can no longer be reached by Tab until it has been clicked once.
pub fn no_autofocus<W: IsA<gtk::Widget> + IsA<gtk::Editable>>(field: &W) {
    let outer = field.clone().upcast::<gtk::Widget>();
    // For composite editables (gtk::Entry, adw::EntryRow, …) the real focus
    // target is the delegated GtkText; disabling only the outer widget would
    // leave GTK free to auto-focus the inner text.
    let inner: Option<gtk::Widget> = field
        .delegate()
        .and_then(|d| d.dynamic_cast::<gtk::Widget>().ok());
    outer.set_focusable(false);
    if let Some(t) = &inner {
        t.set_focusable(false);
    }
    let click = gtk::GestureClick::new();
    click.set_propagation_phase(gtk::PropagationPhase::Capture);
    {
        let outer = outer.clone();
        let inner = inner.clone();
        click.connect_pressed(move |_, _, _, _| {
            outer.set_focusable(true);
            match &inner {
                Some(t) => {
                    t.set_focusable(true);
                    t.grab_focus();
                }
                None => {
                    outer.grab_focus();
                }
            }
        });
    }
    outer.add_controller(click);
}

/// Sets a placeholder icon (fills the square) into the frame.
pub fn set_cover_placeholder(frame: &gtk::AspectFrame, placeholder_icon: &str, size: i32) {
    let img = gtk::Image::from_icon_name(placeholder_icon);
    img.set_pixel_size(size);
    img.add_css_class("dim-label");
    frame.set_child(Some(&img));
}

/// Sets the (possibly background-decoded) image into the frame.
pub fn set_cover_texture(frame: &gtk::AspectFrame, texture: &gtk::gdk::Texture) {
    let pic = gtk::Picture::for_paintable(texture);
    pic.set_content_fit(gtk::ContentFit::Cover);
    pic.set_can_shrink(true);
    frame.set_child(Some(&pic));
}

/// Sets the image as a **fixed-size** thumbnail (via `gtk::Image` with
/// `pixel_size`). Unlike a `Picture`, it does not grow with the row height –
/// so list covers always stay the same size (e.g. 48 px), regardless of whether
/// the row is single- or two-line. The size is taken from the frame.
pub fn set_cover_thumb(bin: &adw::Bin, texture: &gtk::gdk::Texture) {
    let size = bin.height_request().max(1);
    // Downscale to a **square** display texture: cover-scale preserving the aspect
    // ratio (smaller side → `size`), then centre-crop to `size`×`size`. This keeps
    // non-square thumbnails (e.g. 16:9 YouTube covers) from being stretched, while
    // the fixed-size texture still stops a Paintable's natural size from growing
    // the frame on taller (multi-line) rows.
    // `pixbuf_get_from_texture` is deprecated since GTK 4.12; deliberately kept
    // until a deprecation-free downscale is visually verified (thumbnail size).
    #[allow(deprecated)]
    let square = gtk::gdk::pixbuf_get_from_texture(texture).map(|pb| {
        let (w, h) = (pb.width().max(1), pb.height().max(1));
        let scale = (size as f64 / w as f64).max(size as f64 / h as f64);
        let sw = ((w as f64 * scale).round() as i32).max(size);
        let sh = ((h as f64 * scale).round() as i32).max(size);
        let scaled = pb
            .scale_simple(sw, sh, gtk::gdk_pixbuf::InterpType::Bilinear)
            .unwrap_or(pb);
        let x = (scaled.width() - size).max(0) / 2;
        let y = (scaled.height() - size).max(0) / 2;
        scaled.new_subpixbuf(x, y, size, size)
    });
    let tex = square.map(|pb| gtk::gdk::Texture::for_pixbuf(&pb));
    let img = match &tex {
        Some(t) => gtk::Image::from_paintable(Some(t)),
        None => gtk::Image::from_paintable(Some(texture)),
    };
    img.set_pixel_size(size);
    bin.set_child(Some(&img));
}

/// Process-wide background decoder for list thumbnails. [`crate::ui::app::cover_widget`]
/// enqueues a `(path, target Bin)` on a cache miss; a single worker thread
/// decodes sequentially off the UI thread, and the texture is cached + applied
/// to every bin still waiting for that path. This keeps building long lists from
/// blocking on image decoding, without spawning a thread per cover.
struct CoverDecoder {
    tx: async_channel::Sender<String>,
    pending: std::rc::Rc<RefCell<HashMap<String, Vec<gtk::glib::WeakRef<adw::Bin>>>>>,
}

thread_local! {
    static COVER_DECODER: RefCell<Option<CoverDecoder>> = const { RefCell::new(None) };
}

/// Schedules `path` to be decoded in the background and set into `bin` once ready
/// (used by the list cover widgets on a cache miss).
pub fn enqueue_thumb_decode(path: &str, bin: &adw::Bin) {
    COVER_DECODER.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            let (tx, rx) = async_channel::unbounded::<String>();
            let (out_tx, out_rx) = async_channel::unbounded::<(String, gtk::gdk::Texture)>();
            // Worker thread: decode off the UI thread (path + texture are Send;
            // the Bin weak refs stay on the UI thread in `pending`).
            std::thread::spawn(move || {
                while let Ok(path) = rx.recv_blocking() {
                    if let Some(tex) = decode_thumb(&path) {
                        if out_tx.send_blocking((path, tex)).is_err() {
                            break;
                        }
                    }
                }
            });
            let pending: std::rc::Rc<RefCell<HashMap<String, Vec<gtk::glib::WeakRef<adw::Bin>>>>> =
                std::rc::Rc::new(RefCell::new(HashMap::new()));
            {
                let pending = pending.clone();
                gtk::glib::spawn_future_local(async move {
                    while let Ok((path, tex)) = out_rx.recv().await {
                        store_thumb(path.clone(), tex.clone());
                        if let Some(bins) = pending.borrow_mut().remove(&path) {
                            for weak in bins {
                                if let Some(bin) = weak.upgrade() {
                                    set_cover_thumb(&bin, &tex);
                                }
                            }
                        }
                    }
                });
            }
            *slot = Some(CoverDecoder { tx, pending });
        }
        let dec = slot.as_ref().unwrap();
        let is_new = {
            let mut pend = dec.pending.borrow_mut();
            let entry = pend.entry(path.to_string()).or_default();
            let is_new = entry.is_empty();
            entry.push(bin.downgrade());
            is_new
        };
        // Enqueue the path only once even if several rows want the same cover.
        if is_new {
            let _ = dec.tx.send_blocking(path.to_string());
        }
    });
}

/// Image or placeholder as a **square**, rounded image in card style –
/// consistently for covers/photos and their placeholders. For single images
/// (e.g. the detail view) where the texture is already available; list cards
/// instead load their cover asynchronously via [`cover_frame`] + [`set_cover_texture`].
pub fn rounded_image(
    texture: Option<&gtk::gdk::Texture>,
    placeholder_icon: &str,
    size: i32,
) -> gtk::Widget {
    // Small list thumbnails: fixed `adw::Bin` frame (does not grow with the row
    // height). Large covers (detail view): AspectFrame with cropping.
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

/// Wraps a cover carousel between two flat navigation arrows for mouse/keyboard
/// use (the swipe gesture keeps working). Returns a horizontal box
/// `[◀ carousel ▶]`; the arrows scroll one page and grey out at the start/end.
/// Indicator dots, if any, are added by the caller below this box. The carousel
/// is assumed to start on page 0 and to have more than one page.
pub(crate) fn carousel_with_arrows(carousel: &adw::Carousel) -> gtk::Box {
    let prev = gtk::Button::from_icon_name("go-previous-symbolic");
    let next = gtk::Button::from_icon_name("go-next-symbolic");
    for b in [&prev, &next] {
        b.add_css_class("flat");
        b.add_css_class("circular");
        b.set_valign(gtk::Align::Center);
    }
    {
        let carousel = carousel.clone();
        prev.connect_clicked(move |_| {
            let target = (carousel.position().round() as i32 - 1).max(0);
            carousel.scroll_to(&carousel.nth_page(target as u32), true);
        });
    }
    {
        let carousel = carousel.clone();
        next.connect_clicked(move |_| {
            let last = carousel.n_pages() as i32 - 1;
            let target = (carousel.position().round() as i32 + 1).min(last);
            carousel.scroll_to(&carousel.nth_page(target as u32), true);
        });
    }
    // Disable the arrow at the respective end; starts on page 0.
    prev.set_sensitive(false);
    next.set_sensitive(carousel.n_pages() > 1);
    {
        let (prev, next) = (prev.clone(), next.clone());
        carousel.connect_position_notify(move |c| {
            let pos = c.position().round() as i32;
            let last = c.n_pages() as i32 - 1;
            prev.set_sensitive(pos > 0);
            next.set_sensitive(pos < last);
        });
    }
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.set_halign(gtk::Align::Center);
    row.append(&prev);
    row.append(carousel);
    row.append(&next);
    row
}
