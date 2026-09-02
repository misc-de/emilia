//! A row in the file browser: either a subfolder or an audio file.

use adw::prelude::*;
use relm4::factory::{DynamicIndex, FactoryComponent, FactorySender};
use relm4::{adw, gtk};
use std::path::{Path, PathBuf};

use crate::ui::widgets::esc;

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

/// A folder recognised as a single album (file browser): lets its row show a
/// play button that plays the whole album. Filled in by `read_entries`.
#[derive(Debug, Clone)]
pub struct DirAlbum {
    pub artist: String,
    pub album: String,
}

#[derive(Debug, Clone)]
pub enum FsEntry {
    Dir {
        name: String,
        path: PathBuf,
        /// Set when the folder is a single album → row also shows a play button.
        album: Option<DirAlbum>,
        /// Summed runtime (ms) of all tracks under the folder; shown on the row
        /// for any folder that contains songs (artist folder or single album).
        total_ms: i64,
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
    RemoteDir { name: String, rel_path: String },
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
        Self::dir_album(path, None, 0)
    }

    /// Like [`Self::dir`] but with the folder's summed runtime and, when it is a
    /// recognised single album, its album info (for the play button).
    pub fn dir_album(path: PathBuf, album: Option<DirAlbum>, total_ms: i64) -> Self {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?")
            .to_string();
        FsEntry::Dir {
            name,
            path,
            album,
            total_ms,
        }
    }

    /// Album info if this folder is a recognised single album (file browser).
    pub fn album(&self) -> Option<&DirAlbum> {
        match self {
            FsEntry::Dir { album, .. } => album.as_ref(),
            _ => None,
        }
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

    /// Play duration as "M:SS" or "H:MM:SS". For files their own length; for a
    /// folder with songs (artist folder or album) its summed runtime; empty for
    /// plain folders/without duration.
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
            FsEntry::Dir { total_ms, .. } if *total_ms > 0 => {
                crate::ui::app::fmt_duration(*total_ms)
            }
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

    /// Raw runtime in milliseconds used for sorting: a folder's summed runtime, a
    /// file's own duration; 0 when unknown (plain folders, untagged remote files).
    pub fn runtime_ms(&self) -> i64 {
        match self {
            FsEntry::File { duration_ms, .. } | FsEntry::RemoteFile { duration_ms, .. } => {
                duration_ms.unwrap_or(0)
            }
            FsEntry::Dir { total_ms, .. } => *total_ms,
            FsEntry::RemoteDir { .. } => 0,
        }
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

// `Clone` so the playback state can be broadcast to every row at once.
#[derive(Debug, Clone)]
pub enum FsInput {
    /// New playback state: each row decides for itself whether it is the entry
    /// running or an enqueued one — it knows its entry, so the sender does not
    /// have to read the rows back to work that out for them.
    Playback(std::sync::Arc<crate::ui::play_mark::PlaybackState>),
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
    /// Play button of an album folder pressed.
    PlayDir(DynamicIndex),
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
            // Same row layout as the streaming/album lists: the icon sits in a
            // 48 px frame flush against the left edge instead of being a small
            // inline image (`emilia-flush` drops the header padding). Unlike a
            // cover it is drawn bare - no card background, icon 30 % smaller.
            add_css_class: "emilia-flush",
            // #[watch], so that tags read later (remote files) update the
            // display.
            #[watch]
            set_title: &esc(&self.entry.display_title()),
            #[watch]
            set_subtitle: &esc(&self.subtitle()),
            // Only folders activate on a tap (open the folder). Tracks play via
            // their play button; a tap on the row does nothing, and the detail
            // view opens on long press / right click.
            set_activatable: self.entry.is_dir(),
            add_prefix: &crate::ui::widgets::icon_frame(self.entry.prefix_icon(), 48),

            // Play button for an album folder (plays the whole album; the row
            // itself still opens the folder). Plain folders have none. Placed
            // before the duration so it sits to the *left* of the runtime.
            add_suffix = &gtk::Button {
                set_visible: self.entry.album().is_some(),
                set_icon_name: "media-playback-start-symbolic",
                set_valign: gtk::Align::Center,
                set_css_classes: &["flat"],
                set_tooltip_text: Some(&crate::i18n::gettext("Play album")),
                connect_clicked[sender, index] => move |_| {
                    let _ = sender.output(FsOutput::PlayDir(index.clone()));
                },
            },

            // As in the artist view: duration right-aligned & subtle. Files show
            // their own length; album folders their summed runtime.
            add_suffix = &gtk::Label {
                #[watch]
                set_label: &self.entry.duration_label(),
                set_visible: !self.entry.duration_label().is_empty(),
                set_css_classes: &["dim-label", "numeric"],
            },

            // Marker for remote files available offline (downloaded).
            add_suffix = &gtk::Image::from_icon_name("folder-download-symbolic") {
                #[watch]
                set_visible: self.entry.downloaded().is_some(),
                set_css_classes: &["dim-label"],
                set_tooltip_text: Some(&crate::i18n::gettext("Downloaded")),
            },

            // Play button (files only): a single click plays/toggles this track
            // while the list stays open. Also reflects the state: track is
            // playing → pause (accented), paused/active → play (accented),
            // in the queue → queue icon, otherwise the regular play button.
            add_suffix = &gtk::Button {
                set_visible: !self.entry.is_dir(),
                set_valign: gtk::Align::Center,
                set_tooltip_text: Some(&crate::i18n::gettext("Play")),
                #[watch]
                set_icon_name: if !self.active && self.queued {
                    "media-playlist-consecutive-symbolic"
                } else {
                    crate::ui::play_mark::icon_name(self.active, self.playing)
                },
                #[watch]
                set_css_classes: crate::ui::play_mark::classes(self.active),
                connect_clicked[sender, index] => move |_| {
                    let _ = sender.output(FsOutput::Activated(index.clone()));
                },
            },

            // Folders open on a tap (handled by ListBox activation).
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

            // Long press: options menu. A press on the play button must not also
            // open it — the play button only plays.
            add_controller = gtk::GestureLongPress {
                connect_pressed[sender, index] => move |gesture, x, y| {
                    if crate::ui::app_helpers::gesture_press_on_button(gesture, x, y) {
                        return;
                    }
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                    let _ = sender.output(FsOutput::LongPress(index.clone()));
                },
            },

            // Right click (classic mouse): the desktop counterpart of the long
            // press – opens the same options/detail menu (also skipped on a button).
            add_controller = gtk::GestureClick {
                set_button: gtk::gdk::BUTTON_SECONDARY,
                connect_pressed[sender, index] => move |gesture, _, x, y| {
                    if crate::ui::app_helpers::gesture_press_on_button(gesture, x, y) {
                        return;
                    }
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
            FsInput::Playback(state) => {
                let is_file = !self.entry.is_dir();
                self.queued = is_file
                    && self
                        .entry
                        .path()
                        .is_some_and(|p| state.queued.contains(p.as_path()));
                self.active = is_file
                    && match self.entry.path() {
                        Some(path) => state.path.as_deref() == Some(path.as_path()),
                        // Remote entry: marked via its path inside the source.
                        None => {
                            state.rel_path.is_some()
                                && self.entry.rel_path() == state.rel_path.as_deref()
                        }
                    };
                self.playing = state.playing;
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

/// The file list marks the running track and the enqueued ones. Its rows are a
/// relm4 factory, so the state travels as a message — one per row, which is why
/// it is shared rather than cloned.
impl crate::ui::play_mark::PlaybackSink for relm4::factory::FactoryVecDeque<FsRow> {
    fn apply_playback(&self, state: &crate::ui::play_mark::PlaybackState) {
        let state = std::sync::Arc::new(state.clone());
        self.broadcast(FsInput::Playback(state));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> Option<String> {
        Some(v.to_string())
    }

    #[test]
    fn split_stem_str_splits_at_the_last_dash() {
        assert_eq!(
            split_stem_str("Artist - Title"),
            (s("Artist"), "Title".into())
        );
        assert_eq!(
            split_stem_str("Artist-Title"),
            (s("Artist"), "Title".into())
        );
        assert_eq!(split_stem_str("A - B - C"), (s("A - B"), "C".into()));
        assert_eq!(
            split_stem_str("  Artist  -  Title  "),
            (s("Artist"), "Title".into())
        );
    }

    #[test]
    fn split_stem_str_without_dash_is_title_only() {
        assert_eq!(split_stem_str("Title"), (None, "Title".into()));
        assert_eq!(split_stem_str("01. Song"), (None, "01. Song".into()));
        // An en dash is not a separator.
        assert_eq!(split_stem_str("Zoë – Song"), (None, "Zoë – Song".into()));
        assert_eq!(split_stem_str(""), (None, String::new()));
    }

    #[test]
    fn split_stem_str_numbers_before_the_dash_count_as_artist() {
        assert_eq!(split_stem_str("01 - Song"), (s("01"), "Song".into()));
        assert_eq!(split_stem_str("12 - Song"), (s("12"), "Song".into()));
        assert_eq!(split_stem_str("101 - Song"), (s("101"), "Song".into()));
    }

    #[test]
    fn split_stem_str_handles_empty_halves() {
        // Empty artist half → no artist.
        assert_eq!(split_stem_str(" - Title"), (None, "Title".into()));
        // Empty title half → the whole stem stays the title.
        assert_eq!(split_stem_str("Title -"), (s("Title"), "Title -".into()));
        assert_eq!(split_stem_str("-"), (None, "-".into()));
        assert_eq!(split_stem_str(" - "), (None, " - ".into()));
    }

    #[test]
    fn split_filename_strips_the_extension_first() {
        assert_eq!(
            split_filename("Artist - Song.mp3"),
            (s("Artist"), "Song".into())
        );
        assert_eq!(split_filename("Song.flac"), (None, "Song".into()));
        assert_eq!(
            split_filename("Artist - Song.Part2.ogg"),
            (s("Artist"), "Song.Part2".into())
        );
        assert_eq!(split_filename("noext"), (None, "noext".into()));
        assert_eq!(split_filename(".hidden"), (None, ".hidden".into()));
        assert_eq!(
            split_filename("dir/Artist - Song.ogg"),
            (s("Artist"), "Song".into())
        );
        assert_eq!(split_filename(""), (None, String::new()));
    }

    #[test]
    fn split_stem_uses_the_file_stem_of_a_path() {
        assert_eq!(
            split_stem(Path::new("/music/Artist - Song.mp3")),
            (s("Artist"), "Song".into())
        );
        assert_eq!(
            split_stem(Path::new("/music/Song.mp3")),
            (None, "Song".into())
        );
        // No file name at all → placeholder.
        assert_eq!(split_stem(Path::new("/")), (None, "?".into()));
    }
}
