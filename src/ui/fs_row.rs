//! Eine Zeile im Dateibrowser: entweder ein Unterordner oder eine Audiodatei.

use adw::prelude::*;
use relm4::factory::{DynamicIndex, FactoryComponent, FactorySender};
use relm4::{adw, gtk};
use std::path::PathBuf;

/// Escaped Sonderzeichen (`&`, `<`, …) für die Anzeige in Pango-Markup.
fn esc(s: &str) -> String {
    gtk::glib::markup_escape_text(s).to_string()
}

/// Zerlegt einen Dateinamen (ohne Endung) am letzten „-": davor der Interpret,
/// danach der Liedname. Ohne „-" gibt es keinen Interpreten und der ganze
/// Name ist der Titel.
fn split_stem(path: &PathBuf) -> (Option<String>, String) {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("?");
    match stem.rfind('-') {
        Some(i) => {
            let artist = stem[..i].trim();
            let title = stem[i + 1..].trim();
            let artist = (!artist.is_empty()).then(|| artist.to_string());
            let title = if title.is_empty() {
                stem.to_string()
            } else {
                title.to_string()
            };
            (artist, title)
        }
        None => (None, stem.to_string()),
    }
}

#[derive(Debug, Clone)]
pub enum FsEntry {
    Dir {
        name: String,
        path: PathBuf,
    },
    File {
        name: String,
        path: PathBuf,
        /// Liedtitel aus den Tags (falls vorhanden).
        title: Option<String>,
        /// Interpret aus den Tags (falls vorhanden).
        artist: Option<String>,
        /// Spieldauer in Millisekunden (falls ermittelbar).
        duration_ms: Option<i64>,
    },
}

impl FsEntry {
    pub fn dir(path: PathBuf) -> Self {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?")
            .to_string();
        FsEntry::Dir { name, path }
    }

    pub fn file(path: PathBuf) -> Self {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?")
            .to_string();
        // Läuft im Hintergrund-Thread (read_entries) – Tag-Lesen ist hier ok.
        let (title, artist, duration_ms) = crate::core::scanner::read_meta(&path);
        FsEntry::File {
            name,
            path,
            title,
            artist,
            duration_ms,
        }
    }

    /// Spieldauer als „M:SS" bzw. „H:MM:SS"; bei Ordnern/ohne Dauer leer.
    fn duration_label(&self) -> String {
        match self {
            FsEntry::File {
                duration_ms: Some(ms),
                ..
            } if *ms > 0 => crate::ui::app::fmt_duration(*ms),
            _ => String::new(),
        }
    }

    /// Überschrift fürs Kontextmenü: bei Dateien „Interpret - Titel"
    /// (Interpret entfällt, wenn kein Tag); bei Ordnern der Ordnername.
    pub fn heading(&self) -> String {
        match self {
            FsEntry::Dir { name, .. } => name.clone(),
            FsEntry::File { .. } => {
                let title = self.display_title();
                match self.effective_artist() {
                    Some(a) => format!("{a} - {title}"),
                    None => title,
                }
            }
        }
    }

    /// Interpret aus den Tags, sonst aus dem Dateinamen vermutet (Teil vor dem
    /// letzten „-"); bei Ordnern `None`.
    pub fn effective_artist(&self) -> Option<String> {
        match self {
            FsEntry::File { path, artist, .. } => {
                artist.clone().or_else(|| split_stem(path).0)
            }
            FsEntry::Dir { .. } => None,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            FsEntry::Dir { name, .. } | FsEntry::File { name, .. } => name,
        }
    }

    /// Anzeigename: Liedtitel aus den Tags, sonst aus dem Dateinamen vermutet
    /// (Teil hinter dem letzten „-"); bei Ordnern der volle Name.
    pub fn display_title(&self) -> String {
        match self {
            FsEntry::Dir { name, .. } => name.clone(),
            FsEntry::File { path, title, .. } => {
                title.clone().unwrap_or_else(|| split_stem(path).1)
            }
        }
    }

    pub fn path(&self) -> &PathBuf {
        match self {
            FsEntry::Dir { path, .. } | FsEntry::File { path, .. } => path,
        }
    }

    pub fn is_dir(&self) -> bool {
        matches!(self, FsEntry::Dir { .. })
    }

    fn prefix_icon(&self) -> &'static str {
        if self.is_dir() {
            "folder-symbolic"
        } else {
            "audio-x-generic-symbolic"
        }
    }

}

/// Anzeigeoptionen für eine Dateizeile.
#[derive(Debug, Clone, Copy, Default)]
pub struct RowOpts {
    /// Interpret als zweite Zeile anzeigen (bei „Mixed Albums").
    pub show_artist: bool,
}

pub struct FsRow {
    pub entry: FsEntry,
    pub opts: RowOpts,
    /// Liegt dieser Titel aktuell in der Wiedergabe-Warteschlange?
    pub queued: bool,
    /// Ist dies der aktuell laufende Titel? Zeigt dann ein Play-/Pause-Icon.
    pub active: bool,
    /// Läuft die Wiedergabe gerade (für Play- vs. Pause-Icon des aktiven Titels)?
    pub playing: bool,
}

impl FsRow {
    /// Untertitel = Interpret, aber nur wenn der Ordner „gemischt" ist.
    fn subtitle(&self) -> String {
        if self.opts.show_artist {
            self.entry.effective_artist().unwrap_or_default()
        } else {
            String::new()
        }
    }
}

#[derive(Debug)]
pub enum FsInput {
    /// Warteschlangen-Markierung aktualisieren.
    SetQueued(bool),
    /// Markierung „aktuell laufender Titel" (+ ob die Wiedergabe gerade läuft).
    SetActive { active: bool, playing: bool },
}

#[derive(Debug)]
pub enum FsOutput {
    Activated(DynamicIndex),
    LongPress(DynamicIndex),
    DoubleClick(DynamicIndex),
}

#[relm4::factory(pub)]
impl FactoryComponent for FsRow {
    type Init = (FsEntry, RowOpts, bool);
    type Input = FsInput;
    type Output = FsOutput;
    type CommandOutput = ();
    type ParentWidget = gtk::ListBox;

    view! {
        adw::ActionRow {
            set_title: &esc(&self.entry.display_title()),
            set_subtitle: &esc(&self.subtitle()),
            set_activatable: true,
            add_prefix = &gtk::Image::from_icon_name(self.entry.prefix_icon()),

            // Wie in der Interpreten-Ansicht: Laufzeit rechtsbündig & dezent
            // (nur Dateien – Ordner haben keine Spieldauer).
            add_suffix = &gtk::Label {
                set_label: &self.entry.duration_label(),
                set_visible: !self.entry.is_dir(),
                set_css_classes: &["dim-label", "numeric"],
            },

            // Play-Button (nur Dateien). Spiegelt zugleich den Zustand: läuft der
            // Titel → Pause (akzentuiert), pausiert/aktiv → Play (akzentuiert),
            // in der Warteschlange → Queue-Symbol, sonst der reguläre Play-Button.
            add_suffix = &gtk::Image {
                set_visible: !self.entry.is_dir(),
                #[watch]
                set_icon_name: Some(if self.active {
                    if self.playing {
                        "media-playback-pause-symbolic"
                    } else {
                        "media-playback-start-symbolic"
                    }
                } else if self.queued {
                    "media-playlist-consecutive-symbolic"
                } else {
                    "media-playback-start-symbolic"
                }),
                #[watch]
                set_css_classes: if self.active { &["accent"] } else { &["dim-label"] },
            },

            connect_activated[sender, index] => move |_| {
                let _ = sender.output(FsOutput::Activated(index.clone()));
            },

            // Doppelklick: Titel in die Warteschlange legen / wieder entfernen.
            add_controller = gtk::GestureClick {
                connect_pressed[sender, index] => move |gesture, n_press, _, _| {
                    if n_press == 2 {
                        gesture.set_state(gtk::EventSequenceState::Claimed);
                        let _ = sender.output(FsOutput::DoubleClick(index.clone()));
                    }
                },
            },

            // Langes Drücken: Optionsmenü.
            add_controller = gtk::GestureLongPress {
                connect_pressed[sender, index] => move |gesture, _, _| {
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                    let _ = sender.output(FsOutput::LongPress(index.clone()));
                },
            },
        }
    }

    fn init_model(
        (entry, opts, queued): Self::Init,
        _index: &DynamicIndex,
        _sender: FactorySender<Self>,
    ) -> Self {
        Self {
            entry,
            opts,
            queued,
            active: false,
            playing: false,
        }
    }

    fn update(&mut self, msg: Self::Input, _sender: FactorySender<Self>) {
        match msg {
            FsInput::SetQueued(q) => self.queued = q,
            FsInput::SetActive { active, playing } => {
                self.active = active;
                self.playing = playing;
            }
        }
    }
}
