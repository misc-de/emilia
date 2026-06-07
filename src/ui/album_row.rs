//! An album row with cover (relm4 factory).
//!
//! The cover comes from the XDG cache (loaded online via the Cover Art Archive);
//! if it is missing, a placeholder icon is shown. The card appears immediately
//! with a placeholder; the image is decoded in a background thread and only set
//! afterwards, so that long lists do not block the UI thread.

use adw::prelude::*;
use relm4::factory::{DynamicIndex, FactoryComponent, FactorySender};
use relm4::{adw, gtk};

use crate::i18n::ngettext_n;
use crate::model::AlbumMeta;

fn esc(s: &str) -> String {
    gtk::glib::markup_escape_text(s).to_string()
}

/// Subtitle: "Artist · Year · N tracks" (available parts, joined with " · ").
fn subtitle(m: &AlbumMeta) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !m.artist.is_empty() {
        parts.push(m.artist.clone());
    }
    if let Some(year) = m.year {
        parts.push(year.to_string());
    }
    if m.track_count > 0 {
        parts.push(ngettext_n("{n} song", "{n} songs", m.track_count as u32));
    }
    parts.join(" · ")
}

#[derive(Debug)]
pub enum AlbumOutput {
    /// Short tap: open the album's song subpage.
    Activated(DynamicIndex),
    /// Long press: open the detail view (like in the file browser).
    LongPress(DynamicIndex),
}

pub struct AlbumCard {
    pub meta: AlbumMeta,
    /// Square cover frame; image is supplied asynchronously.
    cover: adw::Bin,
    /// Prefix: cover, with a red "Disconnected" overlay when the source is offline.
    prefix: gtk::Widget,
}

#[relm4::factory(pub)]
impl FactoryComponent for AlbumCard {
    /// `(album, is the source currently offline?)`.
    type Init = (AlbumMeta, bool);
    type Input = ();
    type Output = AlbumOutput;
    /// The cover decoded in the background (or `None` if the file is missing/faulty).
    type CommandOutput = Option<gtk::gdk::Texture>;
    type ParentWidget = gtk::ListBox;

    view! {
        adw::ActionRow {
            add_css_class: "emilia-flush",
            set_title: &esc(&self.meta.album),
            set_subtitle: &esc(&subtitle(&self.meta)),
            set_activatable: true,
            add_prefix: &self.prefix,

            // Short tap: list the album's songs.
            connect_activated[sender, index] => move |_| {
                let _ = sender.output(AlbumOutput::Activated(index.clone()));
            },

            // Long press: detail view – like under "Filesystem".
            add_controller = gtk::GestureLongPress {
                connect_pressed[sender, index] => move |gesture, _, _| {
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                    let _ = sender.output(AlbumOutput::LongPress(index.clone()));
                },
            },

            // Right click (classic mouse): same detail view as the long press.
            add_controller = gtk::GestureClick {
                set_button: gtk::gdk::BUTTON_SECONDARY,
                connect_pressed[sender, index] => move |gesture, _, _, _| {
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                    let _ = sender.output(AlbumOutput::LongPress(index.clone()));
                },
            },
        }
    }

    fn init_model(init: Self::Init, _index: &DynamicIndex, sender: FactorySender<Self>) -> Self {
        use crate::ui::widgets;
        let (meta, offline) = init;
        let cover = widgets::thumb_frame("media-optical-symbolic", 48);
        if let Some(path) = meta.cover_path.clone() {
            // Already decoded? Then immediately from the cache – no flashing.
            if let Some(texture) = widgets::cached_thumb(&path) {
                widgets::set_cover_thumb(&cover, &texture);
            } else {
                // Otherwise decode downscaled in the background – not on the
                // UI thread, so that building the list stays smooth.
                sender.spawn_oneshot_command(move || widgets::decode_thumb(&path));
            }
        }
        let prefix = crate::ui::widgets::offline_overlay(&cover, offline);
        Self {
            meta,
            cover,
            prefix,
        }
    }

    fn update_cmd(&mut self, texture: Self::CommandOutput, _sender: FactorySender<Self>) {
        if let Some(texture) = texture {
            // Cache for next time, then set.
            if let Some(path) = &self.meta.cover_path {
                crate::ui::widgets::store_thumb(path.clone(), texture.clone());
            }
            crate::ui::widgets::set_cover_thumb(&self.cover, &texture);
        }
    }
}
