//! Eine Interpreten-Zeile mit Foto (relm4-Factory).
//!
//! Das Foto stammt aus dem XDG-Cache (online via Deezer geladen); fehlt es,
//! wird ein Avatar-Platzhalter gezeigt. Die Karte erscheint sofort mit
//! Platzhalter; das Bild wird in einem Hintergrund-Thread dekodiert und erst
//! danach gesetzt, damit lange Listen den UI-Thread nicht blockieren.

use adw::prelude::*;
use relm4::factory::{DynamicIndex, FactoryComponent, FactorySender};
use relm4::{adw, gtk};

use crate::model::ArtistMeta;

fn esc(s: &str) -> String {
    gtk::glib::markup_escape_text(s).to_string()
}

#[derive(Debug)]
pub enum ArtistOutput {
    /// Kurzes Tippen: Alben & Lieder des Interpreten auflisten.
    Activated(DynamicIndex),
    /// Langes Drücken: Detailansicht öffnen (wie im Dateibrowser).
    LongPress(DynamicIndex),
}

pub struct ArtistCard {
    pub meta: ArtistMeta,
    /// Quadratischer Foto-Rahmen; Bild wird asynchron nachgereicht.
    avatar: adw::Bin,
    /// Präfix: Foto, bei getrennter Quelle mit rotem „Getrennt"-Overlay.
    prefix: gtk::Widget,
}

#[relm4::factory(pub)]
impl FactoryComponent for ArtistCard {
    /// `(Interpret, ist die Quelle gerade offline?)`.
    type Init = (ArtistMeta, bool);
    type Input = ();
    type Output = ArtistOutput;
    /// Das im Hintergrund dekodierte Foto (oder `None`, falls Datei fehlt/fehlerhaft).
    type CommandOutput = Option<gtk::gdk::Texture>;
    type ParentWidget = gtk::ListBox;

    view! {
        adw::ActionRow {
            add_css_class: "emilia-flush",
            set_title: &esc(&self.meta.name),
            set_activatable: true,
            add_prefix: &self.prefix,

            // Kurzes Tippen: Alben & Lieder des Interpreten auflisten.
            connect_activated[sender, index] => move |_| {
                let _ = sender.output(ArtistOutput::Activated(index.clone()));
            },

            // Langes Drücken: Detailansicht – wie unter „Dateisystem".
            add_controller = gtk::GestureLongPress {
                connect_pressed[sender, index] => move |gesture, _, _| {
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                    let _ = sender.output(ArtistOutput::LongPress(index.clone()));
                },
            },
        }
    }

    fn init_model(init: Self::Init, _index: &DynamicIndex, sender: FactorySender<Self>) -> Self {
        use crate::ui::widgets;
        let (meta, offline) = init;
        let avatar = widgets::thumb_frame("avatar-default-symbolic", 48);
        if let Some(path) = meta.image_path.clone() {
            // Bereits dekodiert? Dann sofort aus dem Cache – kein Aufblitzen.
            if let Some(texture) = widgets::cached_thumb(&path) {
                widgets::set_cover_thumb(&avatar, &texture);
            } else {
                // Sonst herunterskaliert im Hintergrund dekodieren – nicht auf dem
                // UI-Thread, damit der Listenaufbau flüssig bleibt.
                sender.spawn_oneshot_command(move || widgets::decode_thumb(&path));
            }
        }
        let prefix = widgets::offline_overlay(&avatar, offline);
        Self {
            meta,
            avatar,
            prefix,
        }
    }

    fn update_cmd(&mut self, texture: Self::CommandOutput, _sender: FactorySender<Self>) {
        if let Some(texture) = texture {
            // Fürs nächste Mal cachen, dann setzen.
            if let Some(path) = &self.meta.image_path {
                crate::ui::widgets::store_thumb(path.clone(), texture.clone());
            }
            crate::ui::widgets::set_cover_thumb(&self.avatar, &texture);
        }
    }
}
