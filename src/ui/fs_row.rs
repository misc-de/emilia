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
    split_stem_str(stem)
}

/// Wie [`split_stem`], aber für einen (entfernten) Dateinamen als String –
/// entfernt zuvor die Endung.
fn split_filename(name: &str) -> (Option<String>, String) {
    let stem = std::path::Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(name);
    split_stem_str(stem)
}

fn split_stem_str(stem: &str) -> (Option<String>, String) {
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
    /// Ordner einer entfernten Quelle (Nextcloud/WebDAV). `rel_path` ist relativ
    /// zur Musikwurzel der Quelle (führender Slash).
    RemoteDir {
        name: String,
        rel_path: String,
    },
    /// Audiodatei einer entfernten Quelle. Tags werden nachträglich gefüllt
    /// (siehe [`FsInput::SetTags`]); `downloaded` zeigt auf die lokale Kopie,
    /// sobald die Datei offline verfügbar ist.
    RemoteFile {
        name: String,
        rel_path: String,
        title: Option<String>,
        artist: Option<String>,
        duration_ms: Option<i64>,
        downloaded: Option<PathBuf>,
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

    /// Ordner einer entfernten Quelle.
    pub fn remote_dir(rel_path: String, name: String) -> Self {
        FsEntry::RemoteDir { name, rel_path }
    }

    /// Audiodatei einer entfernten Quelle (Tags zunächst leer).
    pub fn remote_file(rel_path: String, name: String, downloaded: Option<PathBuf>) -> Self {
        FsEntry::RemoteFile {
            name,
            rel_path,
            title: None,
            artist: None,
            duration_ms: None,
            downloaded,
        }
    }

    /// Pfad relativ zur Musikwurzel der Quelle (nur entfernte Einträge).
    pub fn rel_path(&self) -> Option<&str> {
        match self {
            FsEntry::RemoteDir { rel_path, .. } | FsEntry::RemoteFile { rel_path, .. } => {
                Some(rel_path)
            }
            _ => None,
        }
    }

    /// Ist dies ein entfernter (Nextcloud/WebDAV) Eintrag?
    pub fn is_remote(&self) -> bool {
        matches!(self, FsEntry::RemoteDir { .. } | FsEntry::RemoteFile { .. })
    }

    /// Lokale Kopie einer heruntergeladenen entfernten Datei (falls vorhanden).
    pub fn downloaded(&self) -> Option<&PathBuf> {
        match self {
            FsEntry::RemoteFile { downloaded, .. } => downloaded.as_ref(),
            _ => None,
        }
    }

    /// Spieldauer als „M:SS" bzw. „H:MM:SS"; bei Ordnern/ohne Dauer leer.
    fn duration_label(&self) -> String {
        match self {
            FsEntry::File {
                duration_ms: Some(ms),
                ..
            }
            | FsEntry::RemoteFile {
                duration_ms: Some(ms),
                ..
            } if *ms > 0 => crate::ui::app::fmt_duration(*ms),
            _ => String::new(),
        }
    }

    /// Überschrift fürs Kontextmenü: bei Dateien „Interpret - Titel"
    /// (Interpret entfällt, wenn kein Tag); bei Ordnern der Ordnername.
    pub fn heading(&self) -> String {
        if self.is_dir() {
            self.name().to_string()
        } else {
            let title = self.display_title();
            match self.effective_artist() {
                Some(a) => format!("{a} - {title}"),
                None => title,
            }
        }
    }

    /// Interpret aus den Tags, sonst aus dem Dateinamen vermutet (Teil vor dem
    /// letzten „-"); bei Ordnern `None`.
    pub fn effective_artist(&self) -> Option<String> {
        match self {
            FsEntry::File { path, artist, .. } => artist.clone().or_else(|| split_stem(path).0),
            FsEntry::RemoteFile { name, artist, .. } => {
                artist.clone().or_else(|| split_filename(name).0)
            }
            FsEntry::Dir { .. } | FsEntry::RemoteDir { .. } => None,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            FsEntry::Dir { name, .. }
            | FsEntry::File { name, .. }
            | FsEntry::RemoteDir { name, .. }
            | FsEntry::RemoteFile { name, .. } => name,
        }
    }

    /// Anzeigename: Liedtitel aus den Tags, sonst aus dem Dateinamen vermutet
    /// (Teil hinter dem letzten „-"); bei Ordnern der volle Name.
    pub fn display_title(&self) -> String {
        match self {
            FsEntry::Dir { name, .. } | FsEntry::RemoteDir { name, .. } => name.clone(),
            FsEntry::File { path, title, .. } => title.clone().unwrap_or_else(|| split_stem(path).1),
            FsEntry::RemoteFile { name, title, .. } => {
                title.clone().unwrap_or_else(|| split_filename(name).1)
            }
        }
    }

    /// Lokaler Dateisystempfad – nur bei lokalen Einträgen (`None` bei entfernten).
    pub fn path(&self) -> Option<&PathBuf> {
        match self {
            FsEntry::Dir { path, .. } | FsEntry::File { path, .. } => Some(path),
            FsEntry::RemoteDir { .. } | FsEntry::RemoteFile { .. } => None,
        }
    }

    pub fn is_dir(&self) -> bool {
        matches!(self, FsEntry::Dir { .. } | FsEntry::RemoteDir { .. })
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
    /// Nachträglich gelesene Tags einer entfernten Datei übernehmen.
    SetTags {
        title: Option<String>,
        artist: Option<String>,
        duration_ms: Option<i64>,
    },
    /// Eine entfernte Datei wurde heruntergeladen (lokale Kopie merken).
    SetDownloaded(PathBuf),
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
            // #[watch], damit nachträglich gelesene Tags (entfernte Dateien) die
            // Anzeige aktualisieren.
            #[watch]
            set_title: &esc(&self.entry.display_title()),
            #[watch]
            set_subtitle: &esc(&self.subtitle()),
            set_activatable: true,
            add_prefix = &gtk::Image::from_icon_name(self.entry.prefix_icon()),

            // Wie in der Interpreten-Ansicht: Laufzeit rechtsbündig & dezent
            // (nur Dateien – Ordner haben keine Spieldauer).
            add_suffix = &gtk::Label {
                #[watch]
                set_label: &self.entry.duration_label(),
                set_visible: !self.entry.is_dir(),
                set_css_classes: &["dim-label", "numeric"],
            },

            // Marker für offline verfügbare (heruntergeladene) entfernte Dateien.
            add_suffix = &gtk::Image::from_icon_name("folder-download-symbolic") {
                #[watch]
                set_visible: self.entry.downloaded().is_some(),
                set_css_classes: &["dim-label"],
                set_tooltip_text: Some(&crate::i18n::gettext("Downloaded")),
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
            FsInput::SetTags {
                title: t,
                artist: a,
                duration_ms: d,
            } => {
                if let FsEntry::RemoteFile {
                    title,
                    artist,
                    duration_ms,
                    ..
                } = &mut self.entry
                {
                    *title = t;
                    *artist = a;
                    *duration_ms = d;
                }
            }
            FsInput::SetDownloaded(path) => {
                if let FsEntry::RemoteFile { downloaded, .. } = &mut self.entry {
                    *downloaded = Some(path);
                }
            }
        }
    }
}
