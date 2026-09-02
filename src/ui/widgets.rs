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
    // Never scale *up*. `from_file_at_scale` happily blows a 600 px cover up to
    // `px`, which costs (px/600)² the memory for no added detail — at the 2560 px
    // background size that is ~26 MB per decode, on the UI thread, for every
    // track change. `file_info` reads only the header, so capping the target at
    // the image's own longer edge is essentially free.
    let px = match gtk::gdk_pixbuf::Pixbuf::file_info(path) {
        Some((_, w, h)) if w > 0 && h > 0 => px.min(w.max(h)),
        _ => px,
    };
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
    // `xalign = 0.5`: when the frame is allocated wider than its 1:1 ratio
    // allows for the given height (narrow phone dialogs), the image would
    // otherwise sit at the left edge with bare card background to its right.
    let frame = gtk::AspectFrame::new(0.5, 0.5, 1.0, false);
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

/// Like [`thumb_frame`], but for rows that only ever show a symbolic icon
/// (file browser): no card background, and the icon is drawn 30 % smaller than
/// the frame. The frame keeps its size so the rows stay aligned with the cover
/// lists.
pub fn icon_frame(icon: &str, size: i32) -> adw::Bin {
    let bin = adw::Bin::new();
    bin.set_size_request(size, size);
    bin.set_halign(gtk::Align::Start);
    bin.set_valign(gtk::Align::Center);
    bin.set_hexpand(false);
    bin.set_vexpand(false);
    let img = gtk::Image::from_icon_name(icon);
    img.set_pixel_size(size * 7 / 10);
    img.add_css_class("dim-label");
    bin.set_child(Some(&img));
    bin
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
/// One frame waiting for a decoded cover, plus an optional veto: a **recycled**
/// list row may have been rebound to a different entry while its cover was in
/// the queue, and must then keep the new entry's image. The guard runs on the UI
/// thread right before the texture is applied and answers "does this widget
/// still want `path`?".
struct PendingTarget {
    bin: gtk::glib::WeakRef<adw::Bin>,
    still_wanted: Option<Box<dyn Fn(&str) -> bool>>,
}

struct CoverDecoder {
    tx: async_channel::Sender<String>,
    pending: std::rc::Rc<RefCell<HashMap<String, Vec<PendingTarget>>>>,
}

thread_local! {
    static COVER_DECODER: RefCell<Option<CoverDecoder>> = const { RefCell::new(None) };
}

/// Schedules `path` to be decoded in the background and set into `bin` once ready
/// (used by the list cover widgets on a cache miss).
pub fn enqueue_thumb_decode(path: &str, bin: &adw::Bin) {
    enqueue_decode(path, bin, None);
}

/// Like [`enqueue_thumb_decode`], but the texture is applied only while
/// `still_wanted(path)` holds. For the recycled rows of [`crate::ui::card_list`],
/// whose frame may belong to a different entry by the time the decode lands.
pub fn enqueue_thumb_decode_guarded(
    path: &str,
    bin: &adw::Bin,
    still_wanted: impl Fn(&str) -> bool + 'static,
) {
    enqueue_decode(path, bin, Some(Box::new(still_wanted)));
}

fn enqueue_decode(path: &str, bin: &adw::Bin, still_wanted: Option<Box<dyn Fn(&str) -> bool>>) {
    COVER_DECODER.with(|cell| {
        let mut slot = cell.borrow_mut();
        let dec = slot.get_or_insert_with(|| {
            let (tx, rx) = async_channel::unbounded::<String>();
            let (out_tx, out_rx) =
                async_channel::unbounded::<(String, Option<gtk::gdk::Texture>)>();
            // Worker thread: decode off the UI thread (path + texture are Send;
            // the Bin weak refs stay on the UI thread in `pending`).
            std::thread::spawn(move || {
                while let Ok(path) = rx.recv_blocking() {
                    // Report the failure too, instead of dropping it silently:
                    // the UI side keys `pending` by path and only ever removes
                    // an entry when a result arrives. Staying quiet on an
                    // undecodable file would pin that entry (and its weak refs)
                    // for the process lifetime *and* make the `is_new` dedup
                    // below swallow every later request for the same path.
                    let tex = decode_thumb(&path);
                    if out_tx.send_blocking((path, tex)).is_err() {
                        break;
                    }
                }
            });
            let pending: std::rc::Rc<RefCell<HashMap<String, Vec<PendingTarget>>>> =
                std::rc::Rc::new(RefCell::new(HashMap::new()));
            {
                let pending = pending.clone();
                gtk::glib::spawn_future_local(async move {
                    while let Ok((path, tex)) = out_rx.recv().await {
                        let Some(targets) = pending.borrow_mut().remove(&path) else {
                            continue;
                        };
                        let Some(tex) = tex else {
                            continue; // Undecodable file: entry dropped, nothing to show.
                        };
                        store_thumb(path.clone(), tex.clone());
                        for target in targets {
                            if target.still_wanted.as_ref().is_none_or(|f| f(&path)) {
                                if let Some(bin) = target.bin.upgrade() {
                                    set_cover_thumb(&bin, &tex);
                                }
                            }
                        }
                    }
                });
            }
            CoverDecoder { tx, pending }
        });
        let is_new = {
            let mut pend = dec.pending.borrow_mut();
            let entry = pend.entry(path.to_string()).or_default();
            let is_new = entry.is_empty();
            entry.push(PendingTarget {
                bin: bin.downgrade(),
                still_wanted,
            });
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

// ---------------------------------------------------------------------------
// Detail dialogs
//
// The building blocks every "tap an item, get a sheet of actions" dialog shares
// (Files/Memos, Podcasts, Streaming, YouTube). They used to live once per page
// as byte-identical private copies.
// ---------------------------------------------------------------------------

/// Content box for the detail dialogs (uniform margins).
pub fn detail_box() -> gtk::Box {
    gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(6)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build()
}

/// Activatable action row with an icon prefix (for the detail dialogs).
pub fn action_row(title: &str, icon: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(title)
        .activatable(true)
        .build();
    row.add_prefix(&gtk::Image::from_icon_name(icon));
    row
}

/// Embeds the content scrollably in a dialog with a header bar and shows it.
/// Uses the full width, but never more than 600 px (on narrow windows the
/// dialog shrinks to the window width by itself).
pub fn present_detail(dialog: &adw::Dialog, content: &gtk::Box, root: &adw::ApplicationWindow) {
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .vexpand(true)
        .child(content)
        .build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&scroller));
    dialog.set_child(Some(&toolbar));
    dialog.set_content_width(600);
    crate::ui::app_helpers::fit_dialog_on_expand(dialog);
    crate::ui::app_helpers::close_on_click_outside(dialog);
    dialog.present(Some(root));
}

/// On a phone a detail dialog is shown as a bottom sheet instead of a floating
/// window. Call before presenting.
pub fn adapt_dialog(dialog: &adw::Dialog, mobile: bool) {
    if mobile {
        dialog.set_presentation_mode(adw::DialogPresentationMode::BottomSheet);
    }
}

/// Empties a gallery flow box and (re-)applies its fixed grid: exactly
/// `columns` equally wide tiles per row. No reflow to fewer columns — the user
/// picks the grid, and each tile is kept square by its `SquareBin`.
pub fn reset_gallery_grid(fb: &gtk::FlowBox, columns: u32) {
    while let Some(c) = fb.first_child() {
        fb.remove(&c);
    }
    fb.set_min_children_per_line(columns);
    fb.set_max_children_per_line(columns);
    fb.set_homogeneous(true);
    fb.set_row_spacing(8);
    fb.set_column_spacing(8);
    fb.set_selection_mode(gtk::SelectionMode::None);
    fb.set_activate_on_single_click(false);
    if !fb.has_css_class("emilia-gallery") {
        fb.add_css_class("emilia-gallery");
    }
}

/// Escapes the Pango markup metacharacters (`&`, `<`, …) so a title/name is
/// displayed literally in a markup label.
pub fn esc(s: &str) -> String {
    gtk::glib::markup_escape_text(s).to_string()
}

/// Adds or removes a CSS class by a boolean, so callers can just state the
/// wanted end state instead of branching.
pub fn set_class(w: &impl IsA<gtk::Widget>, class: &str, on: bool) {
    if on {
        w.add_css_class(class);
    } else {
        w.remove_css_class(class);
    }
}

/// A small modal spinner dialog with a caption, for the short blocking waits
/// (resolving a song online, downloading a missing track). Returns the dialog
/// and its label, so the caller can present it, update the phase text and close
/// it when the work returns.
pub fn busy_dialog(text: &str, width: i32) -> (adw::Dialog, gtk::Label) {
    let dialog = adw::Dialog::builder().content_width(width).build();
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(16)
        .margin_top(28)
        .margin_bottom(28)
        .margin_start(28)
        .margin_end(28)
        .halign(gtk::Align::Center)
        .build();
    let spinner = gtk::Spinner::builder()
        .width_request(32)
        .height_request(32)
        .build();
    spinner.set_spinning(true);
    let label = gtk::Label::builder()
        .label(text)
        .wrap(true)
        .justify(gtk::Justification::Center)
        .build();
    content.append(&spinner);
    content.append(&label);
    dialog.set_child(Some(&content));
    (dialog, label)
}

#[cfg(test)]
mod tests {
    use super::esc;

    #[test]
    fn esc_escapes_markup_metacharacters() {
        assert_eq!(esc("Bonnie & Clyde"), "Bonnie &amp; Clyde");
        assert_eq!(esc("<b>bold</b>"), "&lt;b&gt;bold&lt;/b&gt;");
        assert_eq!(esc("say \"hi\""), "say &quot;hi&quot;");
        assert_eq!(esc("rock'n'roll"), "rock&apos;n&apos;roll");
    }

    #[test]
    fn esc_leaves_plain_text_alone() {
        assert_eq!(esc(""), "");
        assert_eq!(esc("Abbey Road"), "Abbey Road");
        assert_eq!(esc("Zoë – 01. Song"), "Zoë – 01. Song");
    }
}
