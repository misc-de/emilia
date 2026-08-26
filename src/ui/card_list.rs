//! Virtualised card list for the library overviews (albums, singles,
//! compilations, artists).
//!
//! These lists are the only ones in the app that grow with the whole library
//! rather than with one folder, so they are the ones that must not build a
//! widget per entry. A `gtk::ListBox` (and relm4's `FactoryVecDeque` on top of
//! it) materialises every row up front, which costs ~220 µs each — a quarter of
//! a second at 1 000 albums on a fast desktop, several seconds on the phone this
//! app targets. A `gtk::ListView` recycles a handful of rows instead, so the
//! cost stops depending on the library size.
//!
//! The row itself is unchanged — the same `adw::ActionRow` with an image prefix
//! that the old factories built. What changes is that it is now *bound* to
//! changing data instead of created per entry, which brings three obligations:
//!
//! * **Reset on rebind.** A recycled row still shows the previous entry's cover;
//!   every bind sets (or clears) each property, never only the present ones.
//! * **Stale decodes.** A cover decode started for one entry may finish after
//!   its row was rebound to another, so the result is matched against the path
//!   the row currently wants before it is applied.
//! * **Positions move.** A gesture controller is created once per recycled row,
//!   so it cannot capture an index; it reads the position the row currently
//!   holds out of a cell.
//!
//! Section headings are drawn inside the row (a label above the `ActionRow`,
//! shown on the first row of each section) rather than through GTK's
//! `SectionModel`. That keeps the existing look — heading on the window
//! background, each section a rounded card via `emilia-sec-top`/`-bottom` — using
//! the per-row information the list already has.

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;
use relm4::{adw, gtk};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::ui::widgets;

/// One row's content, independent of whether it came from an album or an artist.
#[derive(Clone, Debug, Default)]
pub struct CardItem {
    pub title: String,
    pub subtitle: String,
    /// Cover/photo path; `None` shows the placeholder icon.
    pub image: Option<String>,
    /// Draw the red "source offline" badge over the image.
    pub offline: bool,
}

/// Callback invoked with a row's current position.
type PosFn = Rc<dyn Fn(usize)>;

mod imp {
    use super::*;

    /// The recycled row widget: heading label above an `adw::ActionRow`.
    pub struct CardRow {
        pub header: gtk::Label,
        pub row: adw::ActionRow,
        /// Square image frame the cover is set into.
        pub thumb: adw::Bin,
        /// Offline badge, toggled per item instead of rebuilt.
        pub badge: gtk::Image,
        /// Position this row currently displays — read by the gesture
        /// controllers, which outlive any single binding.
        pub pos: Cell<u32>,
        /// Image path this row currently wants, so a late decode belonging to a
        /// previous binding can be told apart and dropped.
        pub wants: RefCell<Option<String>>,
        /// Placeholder icon, needed to reset the frame on rebind.
        pub icon: RefCell<String>,
        /// Long press / right click handler, installed once by the factory.
        pub on_context: RefCell<Option<PosFn>>,
    }

    impl Default for CardRow {
        fn default() -> Self {
            Self {
                header: crate::ui::app_gallery::section_header_label(""),
                row: adw::ActionRow::new(),
                thumb: widgets::thumb_frame("media-optical-symbolic", 48),
                badge: gtk::Image::from_icon_name("network-offline-symbolic"),
                pos: Cell::new(0),
                wants: RefCell::new(None),
                icon: RefCell::new("media-optical-symbolic".to_string()),
                on_context: RefCell::new(None),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for CardRow {
        const NAME: &'static str = "EmiliaCardRow";
        type Type = super::CardRow;
        type ParentType = gtk::Box;
    }

    impl ObjectImpl for CardRow {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.set_orientation(gtk::Orientation::Vertical);

            self.header.set_visible(false);

            // The badge sits in a permanent overlay whose visibility is toggled,
            // so a rebind never has to rebuild the prefix widget.
            let overlay = gtk::Overlay::new();
            overlay.set_child(Some(&self.thumb));
            self.badge.add_css_class("emilia-offline");
            self.badge.set_halign(gtk::Align::End);
            self.badge.set_valign(gtk::Align::Start);
            self.badge.set_pixel_size(14);
            self.badge.set_visible(false);
            overlay.add_overlay(&self.badge);

            self.row.add_css_class("emilia-flush");
            self.row.add_css_class("emilia-card");
            self.row.add_prefix(&overlay);
            self.row.set_activatable(true);

            // Long press and right click both open the detail view. Installed
            // once for the recycled row; they read the position out of `pos`.
            let lp = gtk::GestureLongPress::new();
            lp.connect_pressed(glib::clone!(
                #[weak(rename_to = this)]
                obj,
                move |gesture, _, _| {
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                    this.fire_context();
                }
            ));
            self.row.add_controller(lp);

            let click = gtk::GestureClick::new();
            click.set_button(gtk::gdk::BUTTON_SECONDARY);
            click.connect_pressed(glib::clone!(
                #[weak(rename_to = this)]
                obj,
                move |gesture, _, _, _| {
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                    this.fire_context();
                }
            ));
            self.row.add_controller(click);

            obj.append(&self.header);
            obj.append(&self.row);
        }

        fn dispose(&self) {
            // The children are owned by this box; drop them explicitly so the
            // recycled rows do not keep widgets alive after the list is gone.
            while let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for CardRow {}
    impl BoxImpl for CardRow {}
}

glib::wrapper! {
    /// One recycled row of a [`CardList`].
    pub struct CardRow(ObjectSubclass<imp::CardRow>)
        @extends gtk::Box, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Orientable;
}

impl Default for CardRow {
    fn default() -> Self {
        glib::Object::new()
    }
}

impl CardRow {
    /// Sets the placeholder icon and the context handler. Called once per
    /// recycled row, from the factory's `setup`.
    fn configure(&self, icon: &str, on_context: PosFn) {
        let imp = self.imp();
        *imp.icon.borrow_mut() = icon.to_string();
        *imp.on_context.borrow_mut() = Some(on_context);
        self.reset_thumb();
    }

    fn fire_context(&self) {
        let cb = self.imp().on_context.borrow().clone();
        if let Some(cb) = cb {
            cb(self.imp().pos.get() as usize);
        }
    }

    /// Puts the placeholder icon back into a frame that may still hold a cover.
    fn reset_thumb(&self) {
        let imp = self.imp();
        let size = imp.thumb.height_request().max(1);
        let img = gtk::Image::from_icon_name(&imp.icon.borrow());
        img.set_pixel_size(size);
        img.add_css_class("dim-label");
        imp.thumb.set_child(Some(&img));
    }

    /// Fills the row with `card` at `position`, applying `headers` (one heading
    /// per row, same order) for the section markers.
    fn bind(&self, position: u32, total: u32, card: &CardItem, headers: Option<&[String]>) {
        let imp = self.imp();
        imp.pos.set(position);
        imp.row.set_title(&esc(&card.title));
        imp.row.set_subtitle(&esc(&card.subtitle));
        imp.badge.set_visible(card.offline);

        // A row starts a section when its heading differs from the previous
        // row's, and ends one when it differs from the next. At the list ends the
        // neighbour is `None`, which differs from the current heading and so
        // correctly marks the boundary.
        let (is_top, is_bottom) = section_edges(position as usize, total as usize, headers);
        match headers
            .and_then(|l| l.get(position as usize))
            .filter(|_| is_top)
        {
            Some(text) => {
                imp.header.set_text(text);
                imp.header.set_visible(true);
            }
            None => imp.header.set_visible(false),
        }
        set_class(&imp.row, "emilia-sec-top", is_top);
        set_class(&imp.row, "emilia-sec-bottom", is_bottom);

        // Image last: reset to the placeholder first, so a recycled row never
        // shows the previous entry's cover while a new one decodes.
        *imp.wants.borrow_mut() = card.image.clone();
        match card.image.as_deref() {
            Some(path) => match widgets::cached_thumb(path) {
                Some(texture) => widgets::set_cover_thumb(&imp.thumb, &texture),
                None => {
                    self.reset_thumb();
                    self.spawn_thumb_decode(path.to_string());
                }
            },
            None => self.reset_thumb(),
        }
    }

    /// Decodes one cover off the UI thread and applies it **only** if the row
    /// still wants that path — a row rebound meanwhile must keep its new image.
    fn spawn_thumb_decode(&self, path: String) {
        let (tx, rx) = async_channel::bounded(1);
        let decode_path = path.clone();
        std::thread::spawn(move || {
            let _ = tx.send_blocking(widgets::decode_thumb(&decode_path));
        });
        glib::spawn_future_local(glib::clone!(
            #[weak(rename_to = this)]
            self,
            async move {
                let Ok(Some(texture)) = rx.recv().await else {
                    return;
                };
                widgets::store_thumb(path.clone(), texture.clone());
                let imp = this.imp();
                if imp.wants.borrow().as_deref() == Some(path.as_str()) {
                    widgets::set_cover_thumb(&imp.thumb, &texture);
                }
            }
        ));
    }
}

/// A virtualised list of [`CardItem`] rows with optional section headings.
pub struct CardList {
    view: gtk::ListView,
    store: gtk::gio::ListStore,
    /// Section heading per row (same order/length as the items), or `None` when
    /// grouping is off for this section.
    headers: Rc<RefCell<Option<Vec<String>>>>,
    /// Row count, so a bound row knows whether it is the last one.
    n_items: Rc<Cell<u32>>,
}

impl CardList {
    /// Builds the list. `on_activate` fires on a short tap (open the entry),
    /// `on_context` on a long press or right click (detail view) — both with the
    /// row's current position.
    pub fn new(
        placeholder_icon: &str,
        on_activate: impl Fn(usize) + 'static,
        on_context: impl Fn(usize) + 'static,
    ) -> Self {
        let store = gtk::gio::ListStore::new::<glib::BoxedAnyObject>();
        let headers: Rc<RefCell<Option<Vec<String>>>> = Rc::new(RefCell::new(None));
        let on_context: PosFn = Rc::new(on_context);
        let icon = placeholder_icon.to_string();

        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(move |_, item| {
            let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let row = CardRow::default();
            row.configure(&icon, on_context.clone());
            item.set_child(Some(&row));
        });
        // The row count decides where the ungrouped list rounds off; kept in a
        // cell the bind closure can read without borrowing the store.
        let n_items = Rc::new(Cell::new(0u32));
        {
            let headers = headers.clone();
            let n_items = n_items.clone();
            factory.connect_bind(move |_, item| {
                let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                    return;
                };
                let Some(row) = item.child().and_downcast::<CardRow>() else {
                    return;
                };
                let Some(card) = item
                    .item()
                    .and_downcast::<glib::BoxedAnyObject>()
                    .map(|o| o.borrow::<CardItem>().clone())
                else {
                    return;
                };
                row.bind(
                    item.position(),
                    n_items.get(),
                    &card,
                    headers.borrow().as_deref(),
                );
            });
        }

        // `NoSelection`: these lists act on activation and carry no selection
        // state — a selection model would highlight a recycled row wrongly.
        let selection = gtk::NoSelection::new(Some(store.clone()));
        let view = gtk::ListView::new(Some(selection), Some(factory));
        view.set_single_click_activate(true);
        view.set_valign(gtk::Align::Start);
        view.add_css_class("emilia-card-list");
        view.connect_activate(move |_, position| on_activate(position as usize));

        Self {
            view,
            store,
            headers,
            n_items,
        }
    }

    /// The widget to put into a `ScrolledWindow`.
    pub fn widget(&self) -> &gtk::ListView {
        &self.view
    }

    /// Replaces the contents. `headers` holds one section heading per item (same
    /// order), or `None` for an ungrouped list.
    pub fn set_items(&self, items: Vec<CardItem>, headers: Option<Vec<String>>) {
        // Headings must be in place before the store change triggers a rebind.
        let sectioned = headers.is_some();
        *self.headers.borrow_mut() = headers;
        set_class(&self.view, "emilia-sectioned", sectioned);
        let objects: Vec<glib::BoxedAnyObject> =
            items.into_iter().map(glib::BoxedAnyObject::new).collect();
        self.n_items.set(objects.len() as u32);
        self.store.splice(0, self.store.n_items(), &objects);
    }
}

/// Where a row sits in its section: `(opens_section, closes_section)` — the
/// first drives the heading and the top rounding, the second the bottom
/// rounding.
///
/// With `headers`, a row opens a section when its heading differs from the
/// previous row's and closes one when it differs from the next; at the list ends
/// the neighbour is `None`, which differs from any heading and so correctly
/// marks the boundary. Without headings the whole list is a single card, so only
/// its first and last row are rounded.
fn section_edges(position: usize, total: usize, headers: Option<&[String]>) -> (bool, bool) {
    match headers {
        Some(labels) => {
            let cur = labels.get(position);
            let prev = position.checked_sub(1).and_then(|p| labels.get(p));
            let next = labels.get(position + 1);
            let top = cur.is_some() && (position == 0 || prev != cur);
            let bottom = cur.is_some() && next != cur;
            (top, bottom)
        }
        None => (position == 0, position + 1 >= total),
    }
}

fn set_class(w: &impl IsA<gtk::Widget>, class: &str, on: bool) {
    if on {
        w.add_css_class(class);
    } else {
        w.remove_css_class(class);
    }
}

fn esc(s: &str) -> String {
    glib::markup_escape_text(s).to_string()
}

#[cfg(test)]
mod tests {
    use super::section_edges;

    fn labels(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// Ungrouped: the list is one card, rounded only at its two ends.
    #[test]
    fn ungrouped_list_rounds_only_at_its_ends() {
        assert_eq!(section_edges(0, 5, None), (true, false));
        assert_eq!(section_edges(1, 5, None), (false, false));
        assert_eq!(section_edges(3, 5, None), (false, false));
        assert_eq!(section_edges(4, 5, None), (false, true));
        // A single row is both the first and the last.
        assert_eq!(section_edges(0, 1, None), (true, true));
    }

    /// Grouped: every change of heading closes one card and opens the next.
    #[test]
    fn each_section_is_its_own_card() {
        let l = labels(&["A", "A", "B", "C", "C", "C"]);
        let edges: Vec<_> = (0..l.len())
            .map(|i| section_edges(i, l.len(), Some(&l)))
            .collect();
        assert_eq!(
            edges,
            vec![
                (true, false),  // A opens
                (false, true),  // A closes
                (true, true),   // B alone: opens and closes
                (true, false),  // C opens
                (false, false), // C middle
                (false, true),  // C closes
            ]
        );
    }

    /// Two adjacent sections must not both round between the same pair of rows
    /// in a way that leaves a gap unaccounted for: every row belongs to exactly
    /// one section, and section count matches the number of openings.
    #[test]
    fn openings_and_closings_pair_up() {
        let l = labels(&["0-9", "A", "A", "A", "B", "B", "Z"]);
        let (opens, closes): (Vec<_>, Vec<_>) = (0..l.len())
            .map(|i| section_edges(i, l.len(), Some(&l)))
            .unzip();
        let n_open = opens.iter().filter(|b| **b).count();
        let n_close = closes.iter().filter(|b| **b).count();
        assert_eq!(n_open, n_close, "every section that opens must also close");
        assert_eq!(n_open, 4, "0-9, A, B, Z");
        // The list starts with an opening and ends with a closing.
        assert!(opens[0]);
        assert!(closes[l.len() - 1]);
    }

    /// A recycled row may be bound to a position past the end of a shorter
    /// heading list (the store and the headings are set together, but the
    /// binding must not panic or claim a section either way).
    #[test]
    fn position_beyond_the_headings_claims_no_section() {
        let l = labels(&["A", "A"]);
        assert_eq!(section_edges(5, 2, Some(&l)), (false, false));
        // Empty headings behave the same.
        assert_eq!(section_edges(0, 0, Some(&[])), (false, false));
    }

    /// Repeated headings that are *not* adjacent stay separate sections — the
    /// comparison is against the neighbour, never a global set.
    #[test]
    fn non_adjacent_repeats_are_separate_sections() {
        let l = labels(&["A", "B", "A"]);
        assert_eq!(section_edges(0, 3, Some(&l)), (true, true));
        assert_eq!(section_edges(1, 3, Some(&l)), (true, true));
        assert_eq!(section_edges(2, 3, Some(&l)), (true, true));
    }
}
