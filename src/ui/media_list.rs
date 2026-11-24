//! Virtualised list of episode/video cards (podcast episodes, "Newest",
//! "Recently").
//!
//! These lists used to be a `gtk::Box` of hand-built cards, one widget tree per
//! entry. GTK walks every one of them on every frame, so scrolling got slower
//! the longer the list: on the phone (Cairo renderer) a podcast with 816
//! episodes drew at ~4 fps, while the same page cut to 30 rows managed ~33 fps.
//! A `gtk::ListView` keeps only the rows on screen (plus a few) and rebinds
//! them, so the cost no longer depends on the list length.
//!
//! The card is laid out like the one it replaces — cover flush left, title,
//! dimmed subtitle, an optional listening-progress line under it, the runtime
//! and a play/pause button on the right — and styled through the same
//! `emilia-card-list` classes as the virtualised album list ([`crate::ui::card_list`]):
//! each section one rounded card, its heading drawn inside the first row.
//!
//! Recycling brings the same duties as there: every bind resets every part of
//! the row, a late cover decode is matched against the path the row currently
//! wants, and the gesture/button handlers read the row's *current* position.

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;
use relm4::{adw, gtk};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::ui::card_list::section_edges;
use crate::ui::widgets;
use crate::ui::widgets::set_class;

/// Listening/watching progress shown under the subtitle.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Progress {
    pub position_ms: i64,
    /// Length stated by the feed; `None` lets the player's duration fill in.
    pub total_secs: Option<i64>,
    pub finished: bool,
}

/// One card's content.
#[derive(Clone, Debug, Default)]
pub struct MediaItem {
    pub title: String,
    /// Dimmed line under the title; may hold a `\n` for a second line. Empty
    /// hides it.
    pub subtitle: String,
    /// Cover path; `None` shows the list's placeholder icon.
    pub image: Option<String>,
    /// Formatted runtime left of the play button; `None` hides it.
    pub duration: Option<String>,
    /// `Some` gives the card a progress line (hidden while there is nothing to
    /// show) that the live tick keeps moving; `None` leaves it out entirely.
    pub progress: Option<Progress>,
    /// What identifies the entry while it plays (episode URL, video id) — for
    /// the play/pause icon and the live progress.
    pub key: String,
    /// Placeholder icon while there is no cover; `None` uses the list's.
    pub placeholder: Option<&'static str>,
    /// A tap on the card runs the list's activate handler (see
    /// [`MediaList::connect_activate`]); otherwise a tap does nothing.
    pub activatable: bool,
}

/// Callback with a row's position and its item.
type ItemFn = Rc<dyn Fn(usize, &MediaItem)>;

/// State shared by the list and its recycled rows.
struct Shared {
    items: RefCell<Vec<MediaItem>>,
    /// Section heading per row (same length as `items`), or `None` for one
    /// untitled card.
    headers: RefCell<Option<Vec<String>>>,
    /// Key of the entry currently playing, and whether playback runs.
    active: RefCell<Option<String>>,
    playing: Cell<bool>,
    placeholder: String,
    on_context: ItemFn,
    on_play: ItemFn,
}

mod imp {
    use super::*;

    pub struct MediaRow {
        pub header: gtk::Label,
        /// One-row list box, so the card picks up the `emilia-card-list` row
        /// styling the album list uses.
        pub host: gtk::ListBox,
        pub card: gtk::ListBoxRow,
        pub thumb: adw::Bin,
        pub title: gtk::Label,
        pub subtitle: gtk::Label,
        pub progress: gtk::Box,
        pub duration: gtk::Label,
        pub play: gtk::Button,
        pub pos: Cell<u32>,
        /// Image path this row currently wants (drops late decodes).
        pub wants: RefCell<Option<String>>,
        /// Placeholder icon of the bound item.
        pub icon: RefCell<String>,
        /// Key of the bound item, to match playback and progress updates.
        pub key: RefCell<String>,
        pub(super) shared: RefCell<Option<Rc<Shared>>>,
    }

    impl Default for MediaRow {
        fn default() -> Self {
            Self {
                header: crate::ui::app_gallery::section_header_label(""),
                host: gtk::ListBox::new(),
                card: gtk::ListBoxRow::new(),
                thumb: widgets::thumb_frame("audio-x-generic-symbolic", 48),
                title: gtk::Label::new(None),
                subtitle: gtk::Label::new(None),
                progress: crate::ui::app_helpers::progress_row_box(),
                duration: gtk::Label::new(None),
                play: crate::ui::play_mark::button(
                    &crate::i18n::gettext("Play/Pause"),
                    false,
                    false,
                ),
                pos: Cell::new(0),
                wants: RefCell::new(None),
                icon: RefCell::new(String::new()),
                key: RefCell::new(String::new()),
                shared: RefCell::new(None),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for MediaRow {
        const NAME: &'static str = "EmiliaMediaRow";
        type Type = super::MediaRow;
        type ParentType = gtk::Box;
    }

    impl ObjectImpl for MediaRow {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.set_orientation(gtk::Orientation::Vertical);
            self.header.set_visible(false);

            // Laid out like an `emilia-flush` `AdwActionRow` (cover flush left,
            // 8 px to the text), as the hand-built cards were.
            let top = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(8)
                .margin_top(3)
                .margin_bottom(3)
                .margin_start(3)
                .margin_end(12)
                .build();
            top.append(&self.thumb);
            let text = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .hexpand(true)
                .valign(gtk::Align::Center)
                .build();
            for label in [&self.title, &self.subtitle] {
                label.set_xalign(0.0);
                label.set_ellipsize(gtk::pango::EllipsizeMode::End);
                text.append(label);
            }
            self.subtitle.add_css_class("dim-label");
            text.append(&self.progress);
            top.append(&text);
            self.duration.set_valign(gtk::Align::Center);
            self.duration.set_css_classes(&["dim-label", "numeric"]);
            top.append(&self.duration);
            self.play.connect_clicked(glib::clone!(
                #[weak(rename_to = this)]
                obj,
                move |_| this.fire(|s| s.on_play.clone())
            ));
            top.append(&self.play);

            // A tap on the card does nothing on purpose (playback is the play
            // button's job, so a stray tap while scrolling starts nothing); long
            // press and right click open the detail view.
            let lp = gtk::GestureLongPress::new();
            lp.connect_pressed(glib::clone!(
                #[weak(rename_to = this)]
                obj,
                move |gesture, x, y| {
                    if crate::ui::app_helpers::gesture_press_on_button(gesture, x, y) {
                        return;
                    }
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                    this.fire(|s| s.on_context.clone());
                }
            ));
            top.add_controller(lp);
            let click = gtk::GestureClick::new();
            click.set_button(gtk::gdk::BUTTON_SECONDARY);
            click.connect_pressed(glib::clone!(
                #[weak(rename_to = this)]
                obj,
                move |gesture, _, x, y| {
                    if crate::ui::app_helpers::gesture_press_on_button(gesture, x, y) {
                        return;
                    }
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                    this.fire(|s| s.on_context.clone());
                }
            ));
            top.add_controller(click);

            self.card.set_child(Some(&top));
            self.card.set_activatable(false);
            self.card.set_selectable(false);
            self.card.add_css_class("emilia-card");
            self.host.set_selection_mode(gtk::SelectionMode::None);
            self.host.add_css_class("emilia-card-host");
            self.host.append(&self.card);

            obj.append(&self.header);
            obj.append(&self.host);
        }

        fn dispose(&self) {
            while let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for MediaRow {}
    impl BoxImpl for MediaRow {}
}

glib::wrapper! {
    /// One recycled row of a [`MediaList`].
    pub struct MediaRow(ObjectSubclass<imp::MediaRow>)
        @extends gtk::Box, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Orientable;
}

impl MediaRow {
    fn new(shared: Rc<Shared>) -> Self {
        let row: Self = glib::Object::new();
        *row.imp().shared.borrow_mut() = Some(shared);
        row.reset_thumb();
        row
    }

    fn shared(&self) -> Option<Rc<Shared>> {
        self.imp().shared.borrow().clone()
    }

    /// Runs the handler `pick` selects with this row's position and item.
    fn fire(&self, pick: impl Fn(&Shared) -> ItemFn) {
        let Some(shared) = self.shared() else {
            return;
        };
        let pos = self.imp().pos.get() as usize;
        let item = shared.items.borrow().get(pos).cloned();
        if let Some(item) = item {
            pick(&shared)(pos, &item);
        }
    }

    fn reset_thumb(&self) {
        let imp = self.imp();
        let mut icon = imp.icon.borrow().clone();
        if icon.is_empty() {
            icon = self
                .shared()
                .map(|s| s.placeholder.clone())
                .unwrap_or_else(|| "audio-x-generic-symbolic".into());
        }
        let img = gtk::Image::from_icon_name(&icon);
        img.set_pixel_size(imp.thumb.height_request().max(1));
        img.add_css_class("dim-label");
        imp.thumb.set_child(Some(&img));
    }

    fn apply_playback(&self, active: Option<&str>, playing: bool) {
        let imp = self.imp();
        let is_active = active.is_some_and(|a| a == imp.key.borrow().as_str());
        crate::ui::play_mark::apply(&imp.play, is_active, playing);
    }

    /// Redraws the progress line; `duration_ms` (the player's) fills in when
    /// the item states no length.
    fn apply_progress(&self, p: Option<&Progress>, duration_ms: i64) {
        let row = &self.imp().progress;
        match p {
            Some(p) => {
                let total = p
                    .total_secs
                    .or_else(|| (duration_ms > 0).then_some(duration_ms / 1000));
                crate::ui::app_helpers::fill_progress_row(row, p.position_ms, total, p.finished);
            }
            None => row.set_visible(false),
        }
    }

    fn bind(&self, position: u32, shared: &Shared) {
        let imp = self.imp();
        imp.pos.set(position);
        let items = shared.items.borrow();
        let Some(item) = items.get(position as usize) else {
            return;
        };
        imp.title.set_label(&item.title);
        imp.subtitle.set_label(&item.subtitle);
        imp.subtitle.set_visible(!item.subtitle.trim().is_empty());
        match &item.duration {
            Some(d) => {
                imp.duration.set_label(d);
                imp.duration.set_visible(true);
            }
            None => imp.duration.set_visible(false),
        }
        *imp.key.borrow_mut() = item.key.clone();
        self.apply_playback(shared.active.borrow().as_deref(), shared.playing.get());
        self.apply_progress(item.progress.as_ref(), 0);

        let headers = shared.headers.borrow();
        let (is_top, is_bottom) = section_edges(position as usize, items.len(), headers.as_deref());
        match headers
            .as_ref()
            .and_then(|h| h.get(position as usize))
            .filter(|h| is_top && !h.is_empty())
        {
            Some(text) => {
                imp.header.set_text(text);
                imp.header.set_visible(true);
            }
            None => imp.header.set_visible(false),
        }
        set_class(&imp.card, "emilia-sec-top", is_top);
        set_class(&imp.card, "emilia-sec-bottom", is_bottom);

        // Image last: back to the placeholder first, so a recycled row never
        // shows the previous entry's cover while the new one decodes.
        *imp.icon.borrow_mut() = item
            .placeholder
            .map(str::to_string)
            .unwrap_or_else(|| shared.placeholder.clone());
        *imp.wants.borrow_mut() = item.image.clone();
        match item.image.as_deref() {
            Some(path) => match widgets::cached_thumb(path) {
                Some(texture) => widgets::set_cover_thumb(&imp.thumb, &texture),
                None => {
                    self.reset_thumb();
                    let row = self.downgrade();
                    widgets::enqueue_thumb_decode_guarded(path, &imp.thumb, move |path| {
                        row.upgrade()
                            .is_some_and(|r| r.imp().wants.borrow().as_deref() == Some(path))
                    });
                }
            },
            None => self.reset_thumb(),
        }
    }
}

/// A virtualised list of [`MediaItem`] cards with optional section headings.
pub struct MediaList {
    view: gtk::ListView,
    store: gtk::gio::ListStore,
    shared: Rc<Shared>,
}

impl MediaList {
    /// `on_context` fires on long press / right click (detail view), `on_play`
    /// on the play button — each with the row's position and item.
    pub fn new(
        placeholder_icon: &str,
        on_context: impl Fn(usize, &MediaItem) + 'static,
        on_play: impl Fn(usize, &MediaItem) + 'static,
    ) -> Self {
        let shared = Rc::new(Shared {
            items: RefCell::new(Vec::new()),
            headers: RefCell::new(None),
            active: RefCell::new(None),
            playing: Cell::new(false),
            placeholder: placeholder_icon.to_string(),
            on_context: Rc::new(on_context),
            on_play: Rc::new(on_play),
        });
        let factory = gtk::SignalListItemFactory::new();
        {
            let shared = shared.clone();
            factory.connect_setup(move |_, item| {
                if let Some(item) = item.downcast_ref::<gtk::ListItem>() {
                    item.set_activatable(false);
                    item.set_child(Some(&MediaRow::new(shared.clone())));
                }
            });
        }
        {
            let shared = shared.clone();
            factory.connect_bind(move |_, item| {
                let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                    return;
                };
                let activatable = shared
                    .items
                    .borrow()
                    .get(item.position() as usize)
                    .is_some_and(|i| i.activatable);
                item.set_activatable(activatable);
                if let Some(row) = item.child().and_downcast::<MediaRow>() {
                    row.bind(item.position(), &shared);
                }
            });
        }
        let store = gtk::gio::ListStore::new::<glib::BoxedAnyObject>();
        let selection = gtk::NoSelection::new(Some(store.clone()));
        let view = gtk::ListView::new(Some(selection), Some(factory));
        view.set_valign(gtk::Align::Start);
        view.set_margin_bottom(12);
        view.set_margin_start(12);
        view.set_margin_end(12);
        view.add_css_class("emilia-card-list");
        Self {
            view,
            store,
            shared,
        }
    }

    /// Runs `f` on a tap of an [`MediaItem::activatable`] card.
    pub fn connect_activate(&self, f: impl Fn(usize, &MediaItem) + 'static) {
        self.view.set_single_click_activate(true);
        let shared = self.shared.clone();
        self.view.connect_activate(move |_, position| {
            let item = shared.items.borrow().get(position as usize).cloned();
            if let Some(item) = item {
                f(position as usize, &item);
            }
        });
    }

    /// The widget to put **directly** into a `ScrolledWindow` — inside a box it
    /// would get its full height and realise every row again.
    pub fn widget(&self) -> &gtk::ListView {
        &self.view
    }

    /// Replaces the contents. `headers` holds one section heading per item
    /// (same order), or `None` for one untitled card.
    pub fn set_items(&self, items: Vec<MediaItem>, headers: Option<Vec<String>>) {
        let n = items.len();
        set_class(&self.view, "emilia-sectioned", headers.is_some());
        *self.shared.items.borrow_mut() = items;
        *self.shared.headers.borrow_mut() = headers;
        // The store only carries the row count; the rows read their data from
        // `shared.items` by position.
        let objects: Vec<glib::BoxedAnyObject> = (0..n).map(glib::BoxedAnyObject::new).collect();
        self.store.splice(0, self.store.n_items(), &objects);
    }

    /// The rows currently realised.
    fn rows(&self) -> Vec<MediaRow> {
        let mut out = Vec::new();
        let mut child = self.view.first_child();
        while let Some(c) = child {
            if let Some(row) = c
                .clone()
                .downcast::<MediaRow>()
                .ok()
                .or_else(|| c.first_child().and_downcast::<MediaRow>())
            {
                out.push(row);
            }
            child = c.next_sibling();
        }
        out
    }

    /// Which entry plays (its key) and whether playback runs. Only the
    /// realised rows are redrawn; rows scrolling in pick it up in `bind`.
    pub fn set_playback(&self, active: Option<&str>, playing: bool) {
        *self.shared.active.borrow_mut() = active.map(str::to_string);
        self.shared.playing.set(playing);
        for row in self.rows() {
            row.apply_playback(active, playing);
        }
    }

    /// Live progress of the entry `key` (transport tick): updates the stored
    /// item, so a row scrolled in later is current, and redraws realised rows.
    pub fn apply_progress(&self, key: &str, position_ms: i64, duration_ms: i64, finished: bool) {
        let mut changed = false;
        for item in self.shared.items.borrow_mut().iter_mut() {
            if item.key == key
                && let Some(p) = item.progress.as_mut()
            {
                p.position_ms = position_ms;
                p.finished = finished;
                changed = true;
            }
        }
        if !changed {
            return;
        }
        let items = self.shared.items.borrow();
        for row in self.rows() {
            if row.imp().key.borrow().as_str() != key {
                continue;
            }
            let pos = row.imp().pos.get() as usize;
            row.apply_progress(
                items.get(pos).and_then(|i| i.progress.as_ref()),
                duration_ms,
            );
        }
    }
}
