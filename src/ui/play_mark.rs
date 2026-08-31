//! The play/pause marker every media list row carries.
//!
//! Each list used to answer the same three questions itself — which icon, which
//! accent, and how to reach the icons again when playback changes elsewhere —
//! and answered them slightly differently in eight places. This module owns the
//! answers; a list only has to say *which* entry a control stands for.
//!
//! Rows exist in three shapes across the app (a relm4 factory in
//! [`crate::ui::fs_row`], a recycled `GObject` row in
//! [`crate::ui::card_list`], and hand-built `AdwActionRow`s everywhere else), so
//! this is deliberately a widget-level helper rather than a row type: all three
//! shapes can use it.

use adw::prelude::*;
use relm4::{adw, gtk};
use std::cell::RefCell;
use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;

/// Icon of a row: a pause while this entry is the one running, a play icon in
/// every other case.
pub(crate) fn icon_name(active: bool, playing: bool) -> &'static str {
    if active && playing {
        "media-playback-pause-symbolic"
    } else {
        "media-playback-start-symbolic"
    }
}

/// Draws a play control for the given state: the icon plus the accent that
/// marks the running entry. An inactive control keeps the plain foreground
/// colour — it used to carry `dim-label`, but at Adwaita's 55 % opacity the
/// play/pause icons read as grey rather than as part of the row. The accent on
/// the running entry is what distinguishes it, so the dimming bought nothing.
/// Only those two classes are touched, so a caller's own styling (`flat`,
/// `circular`, …) survives a redraw.
pub(crate) fn apply(widget: &impl IsA<gtk::Widget>, active: bool, playing: bool) {
    let widget = widget.as_ref();
    let icon = icon_name(active, playing);
    if let Some(button) = widget.downcast_ref::<gtk::Button>() {
        button.set_icon_name(icon);
    } else if let Some(image) = widget.downcast_ref::<gtk::Image>() {
        image.set_icon_name(Some(icon));
    }
    crate::ui::widgets::set_class(widget, "accent", active);
    // Clear the dimming a recycled row may still carry from an earlier build.
    crate::ui::widgets::set_class(widget, "dim-label", false);
}

/// The same styling as [`apply`], for rows built in a `view!` macro, where a
/// `#[watch]` can only feed a setter. Keeps those rows on this module's policy
/// instead of spelling the classes out again.
pub(crate) fn classes(active: bool) -> &'static [&'static str] {
    if active {
        &["flat", "accent"]
    } else {
        &["flat"]
    }
}

/// Play control for a row that a tap *opens* (album, playlist, audiobook): the
/// playback needs a button of its own.
pub(crate) fn button(tooltip: &str, active: bool, playing: bool) -> gtk::Button {
    let button = gtk::Button::builder()
        .tooltip_text(tooltip)
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build();
    apply(&button, active, playing);
    button
}

/// Play control for a row that plays on a tap of the row itself: the icon is
/// only a marker, not a button.
pub(crate) fn marker(active: bool, playing: bool) -> gtk::Image {
    let image = gtk::Image::new();
    image.set_valign(gtk::Align::Center);
    apply(&image, active, playing);
    image
}

/// The play controls of one list, kept by the entry they stand for, so a
/// playback change can flip them without the list rebuilding its rows — a
/// rebuild would cost a round of database lookups and throw away the scroll
/// position on every play/pause.
///
/// Controls of rows that have since been dropped are discarded on the next pass
/// (a widget that has left the window has no `root()` any more), so a list that
/// is rebuilt over and over does not grow this list forever.
#[derive(Default, Clone)]
pub(crate) struct Marks {
    entries: Rc<RefCell<Vec<(String, gtk::Widget)>>>,
}

impl Marks {
    /// Forgets every registered control — call before rebuilding a list.
    pub(crate) fn clear(&self) {
        self.entries.borrow_mut().clear();
    }

    /// Registers one row's play control under the key that identifies its entry
    /// (a path, an id, a `scope\u{1}key` pair — whatever the list matches on).
    pub(crate) fn add(&self, key: impl Into<String>, widget: &impl IsA<gtk::Widget>) {
        self.entries
            .borrow_mut()
            .push((key.into(), widget.clone().upcast()));
    }

    /// Redraws every live control, asking `is_active` which entry is running.
    pub(crate) fn apply_all(&self, playing: bool, is_active: impl Fn(&str) -> bool) {
        let mut entries = self.entries.borrow_mut();
        entries.retain(|(_, widget)| widget.root().is_some());
        for (key, widget) in entries.iter() {
            apply(widget, is_active(key), playing);
        }
    }
}

/// What is playing right now, in the terms the lists ask in.
///
/// Gathered once per change instead of every list digging through the transport
/// itself — that digging is where the answers used to drift apart (the album
/// overview asked a different question than the file list, and the player bar's
/// album was blank for single-track albums, so the Singles rows never matched).
#[derive(Debug, Clone)]
pub(crate) struct PlaybackState {
    /// Is playback actually running (as opposed to a loaded but paused track)?
    pub(crate) playing: bool,
    /// Local file currently loaded into the player.
    pub(crate) path: Option<PathBuf>,
    /// Its album — what the library overviews mark on.
    pub(crate) album: Option<String>,
    /// Remote (WebDAV) entry playing, by path relative to its source.
    pub(crate) rel_path: Option<String>,
    /// Podcast episode currently loaded (by audio URL).
    pub(crate) episode_url: Option<String>,
    /// YouTube video currently loaded (by video id).
    pub(crate) video_id: Option<String>,
    /// Explicitly enqueued tracks ("Add to queue") — drives the queue marker,
    /// which is deliberately *not* the same as the running playback context.
    pub(crate) queued: HashSet<PathBuf>,
}

/// A list that marks which of its rows is the one playing.
///
/// The mechanism differs per list type and has to: a relm4 factory takes a
/// message per row, recycled rows are redrawn where they sit, a hand-built list
/// keeps its controls in a [`Marks`] registry, and a component is only
/// reachable through its message channel. What they share — and what this trait
/// pins down — is that they are all fed the same [`PlaybackState`] from one
/// place, so no list can drift into asking its own question again.
pub(crate) trait PlaybackSink {
    fn apply_playback(&self, state: &PlaybackState);
}
