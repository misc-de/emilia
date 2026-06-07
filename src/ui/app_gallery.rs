use adw::prelude::*;
use relm4::gtk;

use crate::ui::app::{App, Msg};

/// A gallery tile: square cover (or placeholder icon) with the title as a
/// semi-transparent band at the bottom (overlay). Click/long-press handlers are
/// added by the caller (FlowBox).
const GALLERY_TILE_DEFAULT: i32 = 110;

pub(crate) fn gallery_cell(
    cover_path: Option<&str>,
    icon: &str,
    title: &str,
) -> (gtk::Overlay, Option<gtk::Picture>) {
    let overlay = gtk::Overlay::new();
    overlay.set_hexpand(false);
    overlay.set_halign(gtk::Align::Start);
    overlay.set_valign(gtk::Align::Start);
    overlay.set_size_request(GALLERY_TILE_DEFAULT, GALLERY_TILE_DEFAULT);

    let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
    frame.set_overflow(gtk::Overflow::Hidden);
    frame.set_hexpand(false);
    frame.set_halign(gtk::Align::Fill);
    frame.set_valign(gtk::Align::Fill);
    frame.set_size_request(GALLERY_TILE_DEFAULT, GALLERY_TILE_DEFAULT);
    frame.add_css_class("card");

    let picture = match cover_path {
        Some(path) => {
            let pic = gtk::Picture::new();
            pic.set_content_fit(gtk::ContentFit::Cover);
            pic.set_can_shrink(true);
            pic.set_hexpand(true);
            pic.set_vexpand(true);
            pic.set_halign(gtk::Align::Fill);
            pic.set_valign(gtk::Align::Fill);
            if let Some(tex) = crate::ui::widgets::cached_thumb(path) {
                pic.set_paintable(Some(&tex));
            }
            frame.append(&pic);
            Some(pic)
        }
        None => {
            let img = gtk::Image::from_icon_name(icon);
            img.set_pixel_size(64);
            img.set_hexpand(true);
            img.set_vexpand(true);
            frame.append(&img);
            None
        }
    };
    overlay.set_child(Some(&frame));

    let label = gtk::Label::new(Some(title));
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    label.set_xalign(0.0);
    label.set_valign(gtk::Align::End);
    label.set_halign(gtk::Align::Fill);
    label.add_css_class("emilia-gallery-title");
    overlay.add_overlay(&label);
    (overlay, picture)
}

/// Decodes covers (path -> target `Picture`) in a background thread and
/// delivers textures progressively on the UI thread.
pub(crate) fn spawn_gallery_decode(items: Vec<(String, gtk::Picture)>) {
    if items.is_empty() {
        return;
    }
    let (tx, rx) = async_channel::bounded::<(usize, String, gtk::gdk::Texture)>(8);
    let paths: Vec<String> = items.iter().map(|(p, _)| p.clone()).collect();
    let targets: Vec<gtk::Picture> = items.into_iter().map(|(_, pic)| pic).collect();
    std::thread::spawn(move || {
        for (i, path) in paths.into_iter().enumerate() {
            if let Some(tex) = crate::ui::widgets::decode_thumb(&path) {
                if tx.send_blocking((i, path, tex)).is_err() {
                    break;
                }
            }
        }
    });
    gtk::glib::spawn_future_local(async move {
        while let Ok((i, path, tex)) = rx.recv().await {
            crate::ui::widgets::store_thumb(path, tex.clone());
            if let Some(pic) = targets.get(i) {
                pic.set_paintable(Some(&tex));
            }
        }
    });
}

fn gallery_width_hint(fb: &gtk::FlowBox) -> i32 {
    if fb.width() > 1 {
        return fb.width();
    }
    let mut ancestor = fb.parent();
    while let Some(w) = ancestor {
        if w.width() > 1 {
            let margins = fb.margin_start() + fb.margin_end();
            return (w.width() - margins).max(1);
        }
        ancestor = w.parent();
    }
    0
}

/// Sets each gallery tile to a square in column width.
pub(crate) fn size_gallery_tiles(fb: &gtk::FlowBox) {
    let cols = fb.max_children_per_line().max(1) as i32;
    let w = gallery_width_hint(fb);
    if w <= 1 {
        return;
    }
    let spacing = fb.column_spacing() as i32;
    let tile = ((w - spacing * cols) / cols).max(64);
    let mut child = fb.first_child();
    while let Some(c) = child {
        let next = c.next_sibling();
        if let Some(inner) = c
            .downcast_ref::<gtk::FlowBoxChild>()
            .and_then(|f| f.child())
        {
            inner.set_size_request(tile, tile);
            if let Some(frame) = inner.first_child() {
                frame.set_size_request(tile, tile);
            }
        }
        child = next;
    }
}

/// Like `size_gallery_tiles`, but tolerant of being called before the FlowBox
/// has a real allocation.
pub(crate) fn size_gallery_tiles_when_ready(fb: &gtk::FlowBox) {
    if fb.width() > 1 {
        size_gallery_tiles(fb);
        return;
    }
    let tries = std::cell::Cell::new(0u32);
    fb.add_tick_callback(move |fb, _| {
        if fb.width() > 1 {
            size_gallery_tiles(fb);
            return gtk::glib::ControlFlow::Break;
        }
        tries.set(tries.get() + 1);
        if tries.get() > 240 {
            gtk::glib::ControlFlow::Break
        } else {
            gtk::glib::ControlFlow::Continue
        }
    });
}

/// A styled section heading shared by the grouped library overviews — year
/// sections (date sort) and alphabetical sections (name sort) — in both the list
/// `set_header_func` and the gallery sections.
pub(crate) fn section_header_label(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.add_css_class("heading");
    label.set_margin_top(8);
    label.set_margin_bottom(2);
    label.set_margin_start(4);
    label
}


impl App {
    /// Fills `container` as a gallery, optionally split into labelled sections.
    /// `labels` (one per item, same order/length as `items`) groups consecutive
    /// equal labels under a [`section_header_label`] heading; `None` (or a
    /// length mismatch) renders a single grid using the reusable `single`
    /// FlowBox. Section FlowBoxes are throwaway (no resize hook); the
    /// click/detail indices are offset so they map back to the full overview.
    pub(crate) fn fill_sectioned_gallery(
        &self,
        container: &gtk::Box,
        single: &gtk::FlowBox,
        items: &[(Option<String>, &'static str, String)],
        labels: Option<&[String]>,
        activate: fn(usize) -> Msg,
        detail: fn(usize) -> Msg,
    ) {
        while let Some(c) = container.first_child() {
            container.remove(&c);
        }
        let Some(labels) = labels.filter(|l| l.len() == items.len()) else {
            container.append(single);
            self.fill_gallery(single, items, activate, detail);
            return;
        };
        let mut i = 0;
        while i < items.len() {
            let mut j = i;
            while j < items.len() && labels[j] == labels[i] {
                j += 1;
            }
            container.append(&section_header_label(&labels[i]));
            let fb = gtk::FlowBox::new();
            container.append(&fb);
            self.fill_gallery_into(&fb, &items[i..j], i, activate, detail, false);
            i = j;
        }
    }

    /// Fills a FlowBox as a gallery: tiles from `(cover, icon, title)`.
    pub(crate) fn fill_gallery(
        &self,
        fb: &gtk::FlowBox,
        items: &[(Option<String>, &'static str, String)],
        activate: fn(usize) -> Msg,
        detail: fn(usize) -> Msg,
    ) {
        self.fill_gallery_into(fb, items, 0, activate, detail, true);
    }

    /// Like [`Self::fill_gallery`], but the click/detail message indices are
    /// offset by `base` (so a year section maps to the full sorted list), and
    /// the one-time resize hook is only registered when `hook` is set (skipped
    /// for the throwaway per-year FlowBoxes of the date-grouped gallery).
    pub(crate) fn fill_gallery_into(
        &self,
        fb: &gtk::FlowBox,
        items: &[(Option<String>, &'static str, String)],
        base: usize,
        activate: fn(usize) -> Msg,
        detail: fn(usize) -> Msg,
        hook: bool,
    ) {
        while let Some(c) = fb.first_child() {
            fb.remove(&c);
        }
        fb.set_min_children_per_line(1);
        fb.set_max_children_per_line(self.libview.gallery_columns);
        fb.set_homogeneous(true);
        fb.set_row_spacing(8);
        fb.set_column_spacing(8);
        fb.set_selection_mode(gtk::SelectionMode::None);
        fb.set_activate_on_single_click(false);
        if !fb.has_css_class("emilia-gallery") {
            fb.add_css_class("emilia-gallery");
        }

        let mut to_decode: Vec<(String, gtk::Picture)> = Vec::new();
        for (i, (cover, icon, title)) in items.iter().enumerate() {
            let (cell, pic) = gallery_cell(cover.as_deref(), icon, title);
            if let (Some(path), Some(pic)) = (cover.as_deref(), pic) {
                if crate::ui::widgets::cached_thumb(path).is_none() {
                    to_decode.push((path.to_string(), pic));
                }
            }

            let idx = base + i;
            let click = gtk::GestureClick::new();
            {
                let input = self.input.clone();
                click.connect_released(move |g, n, _, _| {
                    if n == 1 {
                        g.set_state(gtk::EventSequenceState::Claimed);
                        let _ = input.send(activate(idx));
                    }
                });
            }
            cell.add_controller(click);

            // Right click (classic mouse): same detail view as the long press.
            crate::ui::app::on_secondary_click(&cell, {
                let input = self.input.clone();
                move || {
                    let _ = input.send(detail(idx));
                }
            });
            let long_press = gtk::GestureLongPress::new();
            {
                let input = self.input.clone();
                long_press.connect_pressed(move |g, _, _| {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    let _ = input.send(detail(idx));
                });
            }
            cell.add_controller(long_press);
            fb.append(&cell);
        }

        spawn_gallery_decode(to_decode);
        size_gallery_tiles_when_ready(fb);
        if hook
            && self
                .libview
                .gallery_hooked
                .borrow_mut()
                .insert(fb.as_ptr() as usize)
        {
            let pagesize_done = std::rc::Rc::new(std::cell::Cell::new(false));
            fb.connect_map(move |fb| {
                size_gallery_tiles_when_ready(fb);
                if pagesize_done.get() {
                    return;
                }
                let mut ancestor = fb.parent();
                while let Some(w) = ancestor {
                    if let Ok(sw) = w.clone().downcast::<gtk::ScrolledWindow>() {
                        let weak = fb.downgrade();
                        sw.hadjustment().connect_page_size_notify(move |_| {
                            if let Some(fb) = weak.upgrade() {
                                size_gallery_tiles(&fb);
                            }
                        });
                        pagesize_done.set(true);
                        break;
                    }
                    ancestor = w.parent();
                }
            });
        }
    }
}
