//! An artist row with photo (relm4 factory).
//!
//! The photo comes from the XDG cache (loaded online via Deezer); if it is
//! missing, an avatar placeholder is shown. The card appears immediately with a
//! placeholder; the image is decoded in a background thread and only set
//! afterwards, so that long lists do not block the UI thread.

use adw::prelude::*;
use relm4::factory::{DynamicIndex, FactoryComponent, FactorySender};
use relm4::{adw, gtk};

use crate::model::ArtistMeta;

fn esc(s: &str) -> String {
    gtk::glib::markup_escape_text(s).to_string()
}

#[derive(Debug)]
pub enum ArtistOutput {
    /// Short tap: list the artist's albums & songs.
    Activated(DynamicIndex),
    /// Long press: open the detail view (like in the file browser).
    LongPress(DynamicIndex),
}

pub struct ArtistCard {
    pub meta: ArtistMeta,
    /// Secondary line: number of albums and songs (e.g. "3 albums · 41 songs").
    subtitle: String,
    /// Square photo frame; image is supplied asynchronously.
    avatar: adw::Bin,
    /// Prefix: photo, with a red "Disconnected" overlay when the source is offline.
    prefix: gtk::Widget,
}

#[relm4::factory(pub)]
impl FactoryComponent for ArtistCard {
    /// `(artist, is the source currently offline?, "N albums · M songs" subtitle)`.
    type Init = (ArtistMeta, bool, String);
    type Input = ();
    type Output = ArtistOutput;
    /// The photo decoded in the background (or `None` if the file is missing/faulty).
    type CommandOutput = Option<gtk::gdk::Texture>;
    type ParentWidget = gtk::ListBox;

    view! {
        adw::ActionRow {
            add_css_class: "emilia-flush",
            set_title: &esc(&self.meta.name),
            set_subtitle: &esc(&self.subtitle),
            set_activatable: true,
            add_prefix: &self.prefix,

            // Short tap: list the artist's albums & songs.
            connect_activated[sender, index] => move |_| {
                let _ = sender.output(ArtistOutput::Activated(index.clone()));
            },

            // Long press: detail view – like under "Filesystem".
            add_controller = gtk::GestureLongPress {
                connect_pressed[sender, index] => move |gesture, _, _| {
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                    let _ = sender.output(ArtistOutput::LongPress(index.clone()));
                },
            },

            // Right click (classic mouse): same detail view as the long press.
            add_controller = gtk::GestureClick {
                set_button: gtk::gdk::BUTTON_SECONDARY,
                connect_pressed[sender, index] => move |gesture, _, _, _| {
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                    let _ = sender.output(ArtistOutput::LongPress(index.clone()));
                },
            },
        }
    }

    fn init_model(init: Self::Init, _index: &DynamicIndex, sender: FactorySender<Self>) -> Self {
        use crate::ui::widgets;
        let (meta, offline, subtitle) = init;
        let avatar = widgets::thumb_frame("avatar-default-symbolic", 48);
        if let Some(path) = meta.image_path.clone() {
            // Already decoded? Then immediately from the cache – no flashing.
            if let Some(texture) = widgets::cached_thumb(&path) {
                widgets::set_cover_thumb(&avatar, &texture);
            } else {
                // Otherwise decode downscaled in the background – not on the
                // UI thread, so that building the list stays smooth.
                sender.spawn_oneshot_command(move || widgets::decode_thumb(&path));
            }
        }
        let prefix = widgets::offline_overlay(&avatar, offline);
        Self {
            meta,
            subtitle,
            avatar,
            prefix,
        }
    }

    fn update_cmd(&mut self, texture: Self::CommandOutput, _sender: FactorySender<Self>) {
        if let Some(texture) = texture {
            // Cache for next time, then set.
            if let Some(path) = &self.meta.image_path {
                crate::ui::widgets::store_thumb(path.clone(), texture.clone());
            }
            crate::ui::widgets::set_cover_thumb(&self.avatar, &texture);
        }
    }
}
