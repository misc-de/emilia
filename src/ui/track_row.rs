//! A track row in the library list (relm4 factory).

use adw::prelude::*;
use relm4::factory::{DynamicIndex, FactoryComponent, FactorySender};
use relm4::{adw, gtk};

use crate::model::Track;
use crate::ui::widgets::esc;

pub struct TrackItem {
    pub track: Track,
}

#[derive(Debug)]
pub enum TrackOutput {
    Play(DynamicIndex),
}

/// Track length for the row, empty when unknown. The formatting itself is the
/// app-wide [`crate::ui::app_helpers::fmt_duration`], so a track past the hour
/// reads `1:05:30` here too instead of `65:30`.
fn fmt_duration(ms: Option<i64>) -> String {
    match ms {
        Some(ms) if ms > 0 => crate::ui::app_helpers::fmt_duration(ms),
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
