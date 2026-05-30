//! Eine Track-Zeile in der Bibliotheksliste (relm4-Factory).

use adw::prelude::*;
use relm4::factory::{DynamicIndex, FactoryComponent, FactorySender};
use relm4::{adw, gtk};

use crate::model::Track;

pub struct TrackItem {
    pub track: Track,
}

#[derive(Debug)]
pub enum TrackOutput {
    Play(DynamicIndex),
}

/// Escaped Sonderzeichen (`&`, `<`, …), damit Pango-Markup sie wörtlich anzeigt.
fn esc(s: &str) -> String {
    gtk::glib::markup_escape_text(s).to_string()
}

fn fmt_duration(ms: Option<i64>) -> String {
    match ms {
        Some(ms) if ms > 0 => {
            let secs = ms / 1000;
            format!("{}:{:02}", secs / 60, secs % 60)
        }
        _ => String::new(),
    }
}

#[relm4::factory(pub)]
impl FactoryComponent for TrackItem {
    type Init = Track;
    type Input = ();
    type Output = TrackOutput;
    type CommandOutput = ();
    type ParentWidget = gtk::ListBox;

    view! {
        adw::ActionRow {
            set_title: &esc(&self.track.title),
            set_subtitle: &esc(self.track.artist.as_deref().unwrap_or("")),
            set_activatable: true,

            add_suffix = &gtk::Label {
                set_label: &fmt_duration(self.track.duration_ms),
                set_css_classes: &["dim-label", "numeric"],
            },
            add_suffix = &gtk::Image::from_icon_name("media-playback-start-symbolic"),

            connect_activated[sender, index] => move |_| {
                let _ = sender.output(TrackOutput::Play(index.clone()));
            },
        }
    }

    fn init_model(track: Self::Init, _index: &DynamicIndex, _sender: FactorySender<Self>) -> Self {
        Self { track }
    }
}
