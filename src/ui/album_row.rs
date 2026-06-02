//! Eine Album-Zeile mit Cover (relm4-Factory).
//!
//! Das Cover stammt aus dem XDG-Cache (online via Cover Art Archive geladen);
//! fehlt es, wird ein Platzhalter-Icon gezeigt. Die Karte erscheint sofort mit
//! Platzhalter; das Bild wird in einem Hintergrund-Thread dekodiert und erst
//! danach gesetzt, damit lange Listen den UI-Thread nicht blockieren.

use adw::prelude::*;
use relm4::factory::{DynamicIndex, FactoryComponent, FactorySender};
use relm4::{adw, gtk};

use crate::i18n::ngettext_n;
use crate::model::AlbumMeta;

fn esc(s: &str) -> String {
    gtk::glib::markup_escape_text(s).to_string()
}

/// Untertitel: „Interpret · Jahr · N Titel" (vorhandene Teile, mit „ · " verbunden).
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
    /// Kurzes Tippen: Lieder-Unterseite des Albums öffnen.
    Activated(DynamicIndex),
    /// Langes Drücken: Detailansicht öffnen (wie im Dateibrowser).
    LongPress(DynamicIndex),
}

pub struct AlbumCard {
    pub meta: AlbumMeta,
    /// Quadratischer Cover-Rahmen; Bild wird asynchron nachgereicht.
    cover: adw::Bin,
    /// Präfix: Cover, bei getrennter Quelle mit rotem „Getrennt"-Overlay.
    prefix: gtk::Widget,
}

#[relm4::factory(pub)]
impl FactoryComponent for AlbumCard {
    /// `(Album, ist die Quelle gerade offline?)`.
    type Init = (AlbumMeta, bool);
    type Input = ();
    type Output = AlbumOutput;
    /// Das im Hintergrund dekodierte Cover (oder `None`, falls Datei fehlt/fehlerhaft).
    type CommandOutput = Option<gtk::gdk::Texture>;
    type ParentWidget = gtk::ListBox;

    view! {
        adw::ActionRow {
            add_css_class: "emilia-flush",
            set_title: &esc(&self.meta.album),
            set_subtitle: &esc(&subtitle(&self.meta)),
            set_activatable: true,
            add_prefix: &self.prefix,

            // Kurzes Tippen: Lieder des Albums auflisten.
            connect_activated[sender, index] => move |_| {
                let _ = sender.output(AlbumOutput::Activated(index.clone()));
            },

            // Langes Drücken: Detailansicht – wie unter „Dateisystem".
            add_controller = gtk::GestureLongPress {
                connect_pressed[sender, index] => move |gesture, _, _| {
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
            // Bereits dekodiert? Dann sofort aus dem Cache – kein Aufblitzen.
            if let Some(texture) = widgets::cached_thumb(&path) {
                widgets::set_cover_thumb(&cover, &texture);
            } else {
                // Sonst herunterskaliert im Hintergrund dekodieren – nicht auf dem
                // UI-Thread, damit der Listenaufbau flüssig bleibt.
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
            // Fürs nächste Mal cachen, dann setzen.
            if let Some(path) = &self.meta.cover_path {
                crate::ui::widgets::store_thumb(path.clone(), texture.clone());
            }
            crate::ui::widgets::set_cover_thumb(&self.cover, &texture);
        }
    }
}
