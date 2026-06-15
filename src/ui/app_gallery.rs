use adw::prelude::*;
use relm4::gtk;

use crate::ui::app::{App, Msg};

/// A square container: it forces its single child into a 1:1 box by reporting a
/// height equal to the width it is handed (height-for-width). Gallery covers wrap
/// in this so they stay proportional with zero lag — the square size is decided
/// during layout itself, not via a deferred `size_request` that trails a frame.
mod square_bin {
    use gtk::glib;
    use gtk::prelude::*;
    use gtk::subclass::prelude::*;
    use relm4::gtk;
    use std::cell::RefCell;

    /// Preferred (natural) side before the FlowBox stretches the tile to the cell
    /// width; the child's own texture size is intentionally ignored.
    const DEFAULT_SIDE: i32 = 110;

    mod imp {
        use super::*;

        #[derive(Default)]
        pub struct SquareBin {
            pub child: RefCell<Option<gtk::Widget>>,
        }

        #[glib::object_subclass]
        impl ObjectSubclass for SquareBin {
            const NAME: &'static str = "EmiliaSquareBin";
            type Type = super::SquareBin;
            type ParentType = gtk::Widget;
        }

        impl ObjectImpl for SquareBin {
            fn dispose(&self) {
                if let Some(child) = self.child.borrow_mut().take() {
                    child.unparent();
                }
            }
        }

        impl WidgetImpl for SquareBin {
            fn request_mode(&self) -> gtk::SizeRequestMode {
                gtk::SizeRequestMode::HeightForWidth
            }

            fn measure(
                &self,
                orientation: gtk::Orientation,
                for_size: i32,
            ) -> (i32, i32, i32, i32) {
                if orientation == gtk::Orientation::Vertical {
                    // Height equals the width we are given → always square.
                    let side = for_size.max(0);
                    (side, side, -1, -1)
                } else {
                    // Width can shrink to nothing; prefer the default side. The
                    // child's texture size is ignored so it can't inflate cells.
                    (0, DEFAULT_SIDE, -1, -1)
                }
            }

            fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
                if let Some(child) = self.child.borrow().as_ref() {
                    child.allocate(width, height, baseline, None);
                }
            }
        }
    }

    glib::wrapper! {
        pub struct SquareBin(ObjectSubclass<imp::SquareBin>) @extends gtk::Widget;
    }

    impl SquareBin {
        pub fn new(child: &impl IsA<gtk::Widget>) -> Self {
            let obj: Self = glib::Object::new();
            child.set_parent(&obj);
            obj.imp().child.replace(Some(child.clone().upcast()));
            obj
        }
    }
}
use square_bin::SquareBin;

pub(crate) fn gallery_cell(
    cover_path: Option<&str>,
    icon: &str,
    title: &str,
) -> (SquareBin, Option<gtk::Picture>) {
    let overlay = gtk::Overlay::new();
    overlay.set_halign(gtk::Align::Fill);
    overlay.set_valign(gtk::Align::Fill);

    let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
    frame.set_overflow(gtk::Overflow::Hidden);
    frame.set_hexpand(true);
    frame.set_vexpand(true);
    frame.set_halign(gtk::Align::Fill);
    frame.set_valign(gtk::Align::Fill);
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

    let cell = SquareBin::new(&overlay);
    cell.set_hexpand(true);
    (cell, picture)
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

/// A `gtk::ListBox` `set_header_func` that draws the alphabetical/year section
/// headings from `labels` (one per row, same order as the rows) and turns each
/// heading block into its own rounded card: the heading sits on the window
/// background, and the first/last row of every section get the `emilia-sec-top`
/// / `emilia-sec-bottom` markers (CSS rounds them and the list gets the
/// `emilia-sectioned` class). `None`/out-of-range → no header, no markers (one
/// plain `boxed-list`). Shared by the library overviews and standalone components.
pub(crate) fn list_section_header_func(
    labels: std::rc::Rc<std::cell::RefCell<Option<Vec<String>>>>,
) -> impl Fn(&gtk::ListBoxRow, Option<&gtk::ListBoxRow>) {
    fn set_class(w: &gtk::ListBoxRow, class: &str, on: bool) {
        if on {
            w.add_css_class(class);
        } else {
            w.remove_css_class(class);
        }
    }
    move |row: &gtk::ListBoxRow, _before: Option<&gtk::ListBoxRow>| {
        let list = row.parent().and_downcast::<gtk::ListBox>();
        let guard = labels.borrow();
        let Some(labels) = guard.as_ref() else {
            // No grouping → a single plain boxed-list, no per-section cards.
            row.set_header(None::<&gtk::Widget>);
            set_class(row, "emilia-sec-top", false);
            set_class(row, "emilia-sec-bottom", false);
            if let Some(list) = &list {
                list.remove_css_class("emilia-sectioned");
            }
            return;
        };
        let i = row.index();
        if i < 0 {
            row.set_header(None::<&gtk::Widget>);
            return;
        }
        let i = i as usize;
        let cur = labels.get(i);
        let prev = i.checked_sub(1).and_then(|p| labels.get(p));
        let next = labels.get(i + 1);
        // First row of a section (heading + top rounding) / last row (bottom
        // rounding). At the list ends `prev`/`next` are `None`, which differs
        // from `cur` and so correctly marks the boundary.
        let is_top = cur.is_some() && (i == 0 || prev != cur);
        let is_bottom = cur.is_some() && next != cur;
        if let Some(list) = &list {
            list.add_css_class("emilia-sectioned");
        }
        set_class(row, "emilia-sec-top", is_top);
        set_class(row, "emilia-sec-bottom", is_bottom);
        if let (true, Some(cur)) = (is_top, cur) {
            let label = section_header_label(cur);
            // Drop the outer margins so the window-background strip bleeds fully
            // (no card colour peeking); spacing comes from CSS padding.
            label.set_margin_top(0);
            label.set_margin_bottom(0);
            label.set_margin_start(0);
            label.add_css_class("emilia-list-section");
            row.set_header(Some(&label));
        } else {
            row.set_header(None::<&gtk::Widget>);
        }
    }
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
            let header = section_header_label(&labels[i]);
            // The very first heading sits a touch low; pull it up 5px.
            if i == 0 {
                header.set_margin_top(3);
            }
            container.append(&header);
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
        // Fixed grid: exactly the configured number of columns per row, every
        // tile the same width. No reflow to fewer columns — the user picks the
        // grid, and each tile is kept square by `SquareBin`.
        fb.set_min_children_per_line(self.libview.gallery_columns);
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
        // `SquareBin` keeps every tile square during layout, so there is no
        // resize hook or deferred sizing to wire up anymore.
        let _ = hook;
    }
}
