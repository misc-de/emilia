//! One recipe for a media list row.
//!
//! Every list in the app renders the same anatomy — cover flush left, title
//! over a subtitle, the runtime and a play control on the right, the detail view
//! on long press or right click — and every list used to spell that out again,
//! which is how the escaping, the flush class, the badge size and the order of
//! the suffixes drifted apart between pages.
//!
//! What a call site still decides is *what* goes in and what the row does; the
//! order the parts are assembled in is fixed here, so the pages cannot drift
//! again: prefixes are cover then number, suffixes are the caller's extras,
//! then the runtime, then the offline badge, then the play control.
//!
//! Lists whose rows are not this shape stay hand-built on purpose: the podcast
//! and YouTube rows carry a progress line *under* the subtitle, for which an
//! `AdwActionRow` has no room, and the settings/dialog rows are not media
//! entries at all.

use adw::prelude::*;
use relm4::{adw, gtk};
use std::rc::Rc;

use crate::ui::app::{cover_widget, duration_label};
use crate::ui::play_mark::{self, Marks};
use crate::ui::widgets::esc;

/// A media list row under construction. See the module docs for the layout it
/// guarantees; finish with [`EntryRow::build`].
pub(crate) struct EntryRow {
    row: adw::ActionRow,
    duration_ms: i64,
    offline: bool,
    play: Option<gtk::Widget>,
}

impl EntryRow {
    /// Starts a row with its (escaped) title. Not activatable by default: most
    /// entries play from their play control, and the ones that open on a tap say
    /// so via [`Self::on_activate`].
    pub(crate) fn new(title: &str) -> Self {
        let row = adw::ActionRow::builder().title(esc(title)).build();
        // Cover flush against the left edge, like every other media list.
        row.add_css_class("emilia-flush");
        Self {
            row,
            duration_ms: 0,
            offline: false,
            play: None,
        }
    }

    /// Secondary line. An empty subtitle is left off, so a row without one is
    /// single-line instead of carrying a blank second line.
    pub(crate) fn subtitle(self, subtitle: &str) -> Self {
        if !subtitle.trim().is_empty() {
            self.row.set_subtitle(&esc(subtitle));
        }
        self
    }

    /// Cover thumbnail, falling back to `icon` when there is no image.
    pub(crate) fn cover(self, path: Option<&str>, icon: &str) -> Self {
        self.row.add_prefix(&cover_widget(path, icon));
        self
    }

    /// Track number, right-aligned next to the cover (album/disc listings).
    pub(crate) fn number(self, number: u32) -> Self {
        self.row.add_prefix(
            &gtk::Label::builder()
                .label(number.to_string())
                .width_chars(2)
                .xalign(1.0)
                .css_classes(["dim-label", "numeric"])
                .build(),
        );
        self
    }

    /// Anything else on the left (a drag handle, say).
    pub(crate) fn prefix(self, widget: &impl IsA<gtk::Widget>) -> Self {
        self.row.add_prefix(widget);
        self
    }

    /// Anything else on the right (a remove button, a download marker). Extras
    /// keep their call order and stay left of the runtime and play control.
    pub(crate) fn suffix(self, widget: &impl IsA<gtk::Widget>) -> Self {
        self.row.add_suffix(widget);
        self
    }

    /// Runtime of the entry; `0` (or less) shows none.
    pub(crate) fn duration(mut self, ms: i64) -> Self {
        self.duration_ms = ms;
        self
    }

    /// Red badge for an entry whose source is currently unreachable.
    pub(crate) fn offline(mut self, offline: bool) -> Self {
        self.offline = offline;
        self
    }

    /// Play control for a row that a tap *opens*: a button of its own.
    pub(crate) fn play_button(
        mut self,
        tooltip: &str,
        active: bool,
        playing: bool,
        on_play: impl Fn() + 'static,
    ) -> Self {
        let button = play_mark::button(tooltip, active, playing);
        button.connect_clicked(move |_| on_play());
        self.play = Some(button.upcast());
        self
    }

    /// Play control for a row that plays on a tap of the row itself: the icon is
    /// only a marker.
    pub(crate) fn play_marker(mut self, active: bool, playing: bool) -> Self {
        self.play = Some(play_mark::marker(active, playing).upcast());
        self
    }

    /// Registers this row's play control with its list, so a playback change
    /// elsewhere flips it without the list being rebuilt. No-op for a row
    /// without a play control.
    pub(crate) fn marked_in(self, marks: &Marks, key: impl Into<String>) -> Self {
        if let Some(play) = &self.play {
            marks.add(key, play);
        }
        self
    }

    /// Makes the row react to a tap (opening it, usually).
    pub(crate) fn on_activate(self, action: impl Fn() + 'static) -> Self {
        self.row.set_activatable(true);
        self.row.connect_activated(move |_| action());
        self
    }

    /// The detail/context view: long press on touch, right click with a mouse.
    /// A press that lands on a button is ignored, so the play control never also
    /// opens the detail view.
    pub(crate) fn on_detail(self, action: impl Fn() + 'static) -> Self {
        let action = Rc::new(action);
        crate::ui::app::on_long_press(&self.row, {
            let action = action.clone();
            move || action()
        });
        crate::ui::app::on_secondary_click(&self.row, move || action());
        self
    }

    /// Assembles the right-hand side in the app-wide order and hands the row over.
    pub(crate) fn build(self) -> adw::ActionRow {
        if self.duration_ms > 0 {
            self.row.add_suffix(&duration_label(self.duration_ms));
        }
        if self.offline {
            let badge = gtk::Image::from_icon_name("network-offline-symbolic");
            badge.add_css_class("emilia-offline");
            badge.set_pixel_size(14);
            badge.set_valign(gtk::Align::Center);
            self.row.add_suffix(&badge);
        }
        if let Some(play) = &self.play {
            self.row.add_suffix(play);
        }
        self.row
    }
}
