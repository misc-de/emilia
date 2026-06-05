//! A row in the file browser: either a subfolder or an audio file.

use adw::prelude::*;
use relm4::factory::{DynamicIndex, FactoryComponent, FactorySender};
use relm4::{adw, gtk};
use std::path::{Path, PathBuf};

/// Escapes special characters (`&`, `<`, …) for display in Pango markup.
fn esc(s: &str) -> String {
    gtk::glib::markup_escape_text(s).to_string()
}

/// Splits a file name (without extension) at the last "-": before it the artist,
/// after it the track name. Without a "-" there is no artist and the whole
/// name is the title.
fn split_stem(path: &Path) -> (Option<String>, String) {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("?");
    split_stem_str(stem)
}

/// Like [`split_stem`], but for a (remote) file name as a string -
/// strips the extension first.
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
        /// Track title from the tags (if present).
        title: Option<String>,
        /// Artist from the tags (if present).
        artist: Option<String>,
        /// Play duration in milliseconds (if determinable).
        duration_ms: Option<i64>,
    },
    /// Folder of a remote source (Nextcloud/WebDAV). `rel_path` is relative
    /// to the source's music root (leading slash).
    RemoteDir {
        name: String,
        rel_path: String,
    },
    /// Audio file of a remote source. Tags are filled in later
    /// (see [`FsInput::SetTags`]); `downloaded` points to the local copy
    /// once the file is available offline.
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
        // Runs in the background thread (read_entries) - reading tags is ok here.
        let (title, artist, duration_ms) = crate::core::scanner::read_meta(&path);
        FsEntry::File {
            name,
            path,
            title,
            artist,
            duration_ms,
        }
    }

    /// Folder of a remote source.
    pub fn remote_dir(rel_path: String, name: String) -> Self {
        FsEntry::RemoteDir { name, rel_path }
    }

    /// Audio file of a remote source. Tags are passed in when they are already
    /// known from the DB (indexed source); otherwise they stay empty and are
    /// filled in later via [`FsInput::SetTags`].
    pub fn remote_file(
        rel_path: String,
        name: String,
        downloaded: Option<PathBuf>,
        title: Option<String>,
        artist: Option<String>,
        duration_ms: Option<i64>,
    ) -> Self {
        FsEntry::RemoteFile {
            name,
            rel_path,
            title,
            artist,
            duration_ms,
            downloaded,
        }
    }

    /// Path relative to the source's music root (remote entries only).
    pub fn rel_path(&self) -> Option<&str> {
        match self {
            FsEntry::RemoteDir { rel_path, .. } | FsEntry::RemoteFile { rel_path, .. } => {
                Some(rel_path)
            }
            _ => None,
        }
    }

    /// Is this a remote (Nextcloud/WebDAV) entry?
    pub fn is_remote(&self) -> bool {
        matches!(self, FsEntry::RemoteDir { .. } | FsEntry::RemoteFile { .. })
    }

    /// Local copy of a downloaded remote file (if present).
    pub fn downloaded(&self) -> Option<&PathBuf> {
        match self {
            FsEntry::RemoteFile { downloaded, .. } => downloaded.as_ref(),
            _ => None,
        }
    }

    /// Play duration as "M:SS" or "H:MM:SS"; empty for folders/without duration.
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

    /// Heading for the context menu: for files "Artist - Title"
    /// (artist is omitted when there is no tag); for folders the folder name.
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

    /// Artist from the tags, otherwise guessed from the file name (part before
    /// the last "-"); `None` for folders.
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

    /// Display name: track title from the tags, otherwise guessed from the file
    /// name (part after the last "-"); for folders the full name.
    pub fn display_title(&self) -> String {
        match self {
            FsEntry::Dir { name, .. } | FsEntry::RemoteDir { name, .. } => name.clone(),
            FsEntry::File { path, title, .. } => {
                title.clone().unwrap_or_else(|| split_stem(path).1)
            }
            FsEntry::RemoteFile { name, title, .. } => {
                title.clone().unwrap_or_else(|| split_filename(name).1)
            }
        }
    }

    /// Local filesystem path - only for local entries (`None` for remote ones).
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

/// Display options for a file row.
#[derive(Debug, Clone, Copy, Default)]
pub struct RowOpts {
    /// Show the artist as a second line (for "Mixed Albums").
    pub show_artist: bool,
}

pub struct FsRow {
    pub entry: FsEntry,
    pub opts: RowOpts,
    /// Is this track currently in the playback queue?
    pub queued: bool,
    /// Is this the currently playing track? Then shows a play/pause icon.
    pub active: bool,
    /// Is playback currently running (for play vs. pause icon of the active track)?
    pub playing: bool,
}

impl FsRow {
    /// Subtitle = artist, but only when the folder is "mixed".
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
    /// Update the queue marker.
    SetQueued(bool),
    /// Marker for "currently playing track" (+ whether playback is running).
    SetActive { active: bool, playing: bool },
    /// Apply tags that were read later for a remote file.
    SetTags {
        title: Option<String>,
        artist: Option<String>,
        duration_ms: Option<i64>,
    },
    /// A remote file was downloaded (remember the local copy).
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
            // #[watch], so that tags read later (remote files) update the
            // display.
            #[watch]
            set_title: &esc(&self.entry.display_title()),
            #[watch]
            set_subtitle: &esc(&self.subtitle()),
            set_activatable: true,
            add_prefix = &gtk::Image::from_icon_name(self.entry.prefix_icon()),

            // As in the artist view: duration right-aligned & subtle
            // (files only - folders have no play duration).
            add_suffix = &gtk::Label {
                #[watch]
                set_label: &self.entry.duration_label(),
                set_visible: !self.entry.is_dir(),
                set_css_classes: &["dim-label", "numeric"],
            },

            // Marker for remote files available offline (downloaded).
            add_suffix = &gtk::Image::from_icon_name("folder-download-symbolic") {
                #[watch]
                set_visible: self.entry.downloaded().is_some(),
                set_css_classes: &["dim-label"],
                set_tooltip_text: Some(&crate::i18n::gettext("Downloaded")),
            },

            // Play button (files only). Also reflects the state: track is
            // playing → pause (accented), paused/active → play (accented),
            // in the queue → queue icon, otherwise the regular play button.
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

            // Double-click: add the track to the queue / remove it again.
            add_controller = gtk::GestureClick {
                connect_pressed[sender, index] => move |gesture, n_press, _, _| {
                    if n_press == 2 {
                        gesture.set_state(gtk::EventSequenceState::Claimed);
                        let _ = sender.output(FsOutput::DoubleClick(index.clone()));
                    }
                },
            },

            // Long press: options menu.
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
