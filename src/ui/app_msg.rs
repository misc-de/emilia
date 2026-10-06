//! The root component's message types: [`Msg`] (UI input, with its per-domain
//! sub-enums) and [`Cmd`] (background-worker results). Split out of
//! [`crate::ui::app`], which re-exports both, so `crate::ui::app::Msg` /
//! `crate::ui::app::Cmd` keep working.

use std::path::PathBuf;

use crate::ui::app::{ActiveSource, SleepChoice};
use crate::ui::fs_row::FsEntry;

#[derive(Debug)]
pub enum Msg {
    Activate(usize),
    ToggleQueue(usize),
    ShowContextMenu(usize),
    ShowArtistDetail(usize),
    ShowAlbumDetail(usize),
    /// Open the detail page of an album via (artist, album) (from subpages).
    ShowAlbumDetailFor {
        artist: String,
        album: String,
    },
    /// Open the detail page of a single song via its path.
    ShowTrackDetail(String),
    /// Open the songs subpage of an album from the album overview (short tap).
    ShowAlbumTracks(usize),
    /// Singles / Compilations overviews — same behaviour as the album overview,
    /// indexing into their own factory/overview.
    ShowSingleTracks(usize),
    ShowSingleDetail(usize),
    ShowCompilationTracks(usize),
    ShowCompilationDetail(usize),
    /// Short tap on an artist: list its albums & songs.
    OpenArtistTracks(usize),
    /// Tap on an album in the artist subpage: list its tracks as
    /// a further subpage.
    OpenAlbumTracks {
        artist: String,
        album: String,
    },
    /// Tap on a greyed "missing" track row: confirm searching for it online and
    /// adding it to the album.
    ShowMissingTrack {
        artist: String,
        album: String,
        disc: u32,
        position: u32,
        title: String,
    },
    /// Confirmed: search the missing track online and offer the top hits so the
    /// user picks which version to add (search only — the download happens once
    /// a candidate is chosen, see [`Msg::DownloadMissingTrack`]).
    AddMissingTrack {
        artist: String,
        album: String,
        disc: u32,
        position: u32,
        title: String,
    },
    /// A YouTube candidate was picked from the missing-track chooser: download
    /// that video into the album folder, tag it and index it.
    DownloadMissingTrack {
        artist: String,
        album: String,
        disc: u32,
        position: u32,
        title: String,
        video_id: String,
    },
    /// Play a track from the artist overview (queue = all tracks
    /// of the artist, start at the tapped one). `close` pops the subpage
    /// back to the main view (row tap) vs. keeps it open (play button).
    PlayArtistTrack {
        name: String,
        path: String,
        close: bool,
    },
    /// Play a **single** selected track (from an album or playlist): only this
    /// track is enqueued, not its siblings. `close` pops the subpage back to the
    /// main view (row tap) vs. keeps it open (play button).
    PlayOneTrack {
        path: String,
        close: bool,
    },
    /// Tap on an album/folder entry in concerts/audiobooks: list its
    /// tracks as a subpage (instead of playing directly).
    OpenEntryTracks {
        scope: String,
        key: String,
    },
    /// Play a track of a folder audiobook/concert (queue = folder in
    /// order, start at the tapped one).
    PlayFolderTrack {
        folder: String,
        path: String,
        close: bool,
    },
    /// Play the whole album in track order (play button of the album row).
    PlayAlbum {
        artist: String,
        album: String,
    },
    /// Play the album folder at this file-browser row index (its play button).
    PlayFsAlbum(usize),
    /// Play button of an overview row (albums / singles / compilations): plays
    /// that album, or toggles pause while it is the one already running.
    PlayAlbumAt(usize),
    PlaySingleAt(usize),
    PlayCompilationAt(usize),
    /// Header sync icon → open the pairing / connection-status dialog (no item).
    OpenSync,
    // --- Device synchronization (handled by the SyncPage component) ---
    /// The sync component paired/disconnected → tint the header icon.
    /// A device-sync share started/ended (green spinning header icon).
    SyncBusy(bool),
    SyncConnected(bool),
    /// The sync component imported metadata → reload the affected views.
    SyncImported,
    /// Command from the lock screen / from media keys (MPRIS).
    Mpris(crate::core::mpris::MprisCommand),
    /// Command from the embedded MCP server (see [`crate::ui::app_mcp`]).
    Mcp(crate::core::mcp::McpCommand),
    /// MCP-server settings (backend mode / LAN exposure / bearer token)
    /// (see [`crate::ui::app_mcp`]).
    McpSetting(crate::ui::app_mcp::McpSettingMsg),
    /// Periodic, quiet background backfill: fetch missing artist photos (first)
    /// and online covers, without the user having to trigger it.
    AutoEnrichTick,
    /// On-demand fingerprint track recognition for the **just started**
    /// track without usable metadata (AcoustID), triggered on play.
    FingerprintCurrent(PathBuf),
    NavUp,
    FilesGoStart,
    Refresh,
    /// Cancel the running library scan (the import progress "Cancel" button).
    ScanCancel,
    OpenSettings,
    /// Set or clear the sleep timer (from the header zzz popover).
    SetSleepTimer(SleepChoice),
    /// Open the library search dialog (title-bar search icon).
    OpenSearch,
    /// A song hit of the search was activated → play it (close the dialog).
    SearchPlayTrack(String),
    /// An album hit of the search was activated → open its track list.
    SearchOpenAlbum(String),
    /// An artist hit of the search was activated → open the artist subpage.
    SearchOpenArtist(String),
    OpenGlobalEq,
    /// Open the equalizer for the currently running track.
    OpenCurrentEq,
    /// Open the equalizer of a YouTube live stream (from its detail dialog).
    OpenLiveEq {
        video_id: String,
        title: String,
    },
    /// Open the track-level equalizer for a specific path (e.g. a YouTube
    /// video from its detail view). `title` is only the header label.
    OpenTrackEq {
        path: String,
        title: String,
    },
    /// Back arrow in the shared header: pop the current subpage.
    NavBack,
    /// Music sources (Files tab bar: extra local folders / Nextcloud)
    /// (see [`crate::ui::app_views_sources`]).
    Source(crate::ui::app_views_sources::SourceMsg),
    /// Appearance / design: scaling, colours, background (see [`crate::ui::theme`]).
    Design(crate::ui::theme::DesignMsg),
    /// Desktop tray icon: settings + click actions (see [`crate::ui::app_tray`]).
    Tray(crate::ui::app_tray::TrayMsg),
    /// Sort + gallery: the title-bar sort popover, the global gallery view, and
    /// the page `*Changed` mirrors (see [`crate::ui::app_sort`]).
    Sort(crate::ui::app_sort::SortMsg),
    /// Equalizer: set / enable / clear bands per output × level
    /// (see [`crate::ui::app_eq`]).
    Eq(crate::ui::app_eq::EqMsg),
    // Playlists
    /// Playlists section: create / open / play / rename / delete + cover
    /// (see [`crate::ui::app_playlist`]).
    Playlist(crate::ui::app_playlist::PlaylistMsg),
    /// A podcast/YouTube "refresh all" advanced by one item → overlay bar.
    RefreshProgress {
        done: usize,
        total: usize,
        label: String,
    },
    /// A refresh reported its outcome → show it in the overlay for a moment.
    RefreshSummary(String),
    /// The summary's display time elapsed → clear the overlay.
    ClearRefreshSummary,
    /// Tap/click next to the loading overlay during a refresh/scan → hide it
    /// (the work continues; the refresh button reopens it).
    DismissOverlay,

    // ---- Voice memos ----
    /// Voice memos + categories (see [`crate::ui::app_memo`]).
    Memo(crate::ui::app_memo::MemoMsg),
    /// StreamMsg — see `crate::ui::app_streaming`.
    Stream(crate::ui::app_streaming::StreamMsg),
    /// EditMsg — see `crate::ui::app_rec_edit`.
    Edit(crate::ui::app_rec_edit::EditMsg),
    /// YtMsg — see `crate::ui::app_yt_glue`.
    Yt(crate::ui::app_yt_glue::YtMsg),
    /// PodcastMsg — see `crate::ui::app_episode_playback`.
    Podcast(crate::ui::app_episode_playback::PodcastMsg),
    /// LyricsMsg — see `crate::ui::app_lyrics`.
    Lyrics(crate::ui::app_lyrics::LyricsMsg),
    /// ConcertMsg — see `crate::ui::app_concert`.
    Concert(crate::ui::app_concert::ConcertMsg),
    /// FavoriteMsg — see `crate::ui::app_favorites`.
    Favorite(crate::ui::app_favorites::FavoriteMsg),
    /// CoverMsg — see `crate::ui::app_covers`.
    Cover(crate::ui::app_covers::CoverMsg),
    /// SettingMsg — see `crate::ui::app_settings`.
    Setting(crate::ui::app_settings::SettingMsg),
    /// CtxMsg — see `crate::ui::app_dialogs`.
    Ctx(crate::ui::app_dialogs::CtxMsg),
    /// TransportMsg — see `crate::ui::app_playback`.
    Transport(crate::ui::app_playback::TransportMsg),
}

/// Results of the background workers (read folder or online enrichment).
#[derive(Debug)]
pub enum Cmd {
    Entries(Vec<FsEntry>),
    /// Result of a WebDAV directory listing (background PROPFIND). Carries the
    /// source and the rel path along, so that an intervening source/folder
    /// switch can discard the stale result.
    RemoteEntries(
        Result<Vec<crate::core::webdav::DavEntry>, String>,
        ActiveSource,
        String,
    ),
    /// Backfilled tags of remote files: (rel path, title, artist, duration).
    RemoteTags(Vec<(String, Option<String>, Option<String>, Option<i64>)>),
    /// A remote file was downloaded: (rel path, local copy) or error.
    RemoteDownloaded(Result<(String, PathBuf), String>),
    /// Online enrichment finished; `changed` = something new was added
    /// (controls during the quiet backfill whether the views are reloaded).
    EnrichDone {
        changed: bool,
    },
    /// Intermediate state: reload albums/artists view (e.g. after a phase).
    ReloadViews,
    /// Local library scan finished; `then_enrich` = possibly fetch online
    /// afterwards. `manual` = part of a user-triggered refresh (clears one slot
    /// of the refresh spinner on completion).
    ScanDone {
        then_enrich: bool,
        manual: bool,
    },
    /// Library-scan progress tick (throttled): files read / total + bytes.
    ScanProgress {
        done: usize,
        total: usize,
        bytes: u64,
        total_bytes: u64,
    },
    /// Found concert candidates (for the import dialog).
    Candidates(Vec<crate::core::concert::Candidate>),
    /// yt-dlp install/update/startup-check finished: the version on success,
    /// or an error message. Drives the settings status and `youtube.ytdlp_version`.
    YtDlpReady(Result<String, String>),
    /// Silent background yt-dlp auto-update finished (the version on success, or
    /// an error message). Unlike [`Cmd::YtDlpReady`] it never toasts: a routine
    /// refresh — or a failure while offline — must not nag the user.
    YtDlpAutoUpdated(Result<String, String>),
    /// Background yt-dlp version probe (opened settings) finished: `Some(v)` if a
    /// usable yt-dlp is present, `None` otherwise. Caches the result and refreshes
    /// the settings row without ever blocking the UI thread on the subprocess.
    YtDlpChecked(Option<String>),
    /// A playlist's videos were listed → start playing them, log the playlist to
    /// "Recent", and mirror it into the Playlists section. (Transport; the page
    /// requests it via `YtOutput::StartPlaylist`.)
    YtPlaylistStart {
        url: String,
        title: String,
        items: Vec<(String, String)>,
        /// Summed runtime (seconds) of the playlist, for the Recent row. `None`
        /// when no durations were available.
        total_duration: Option<i64>,
    },
    /// Startup background refresh finished → tell the YtPage component to reload.
    YtReload,
    /// A recognized song ("Recently heard") was resolved to a YouTube video:
    /// `video_id` is `None` when nothing matched. `download` distinguishes the
    /// two actions — play the stream, or import it into the library.
    HeardResolved {
        video_id: Option<String>,
        title: String,
        /// Artist as the song recognition reported it — a better metadata hint
        /// for the import than anything YouTube says.
        artist: Option<String>,
        download: bool,
    },
    /// The canonical (MusicBrainz) tracklist of an album finished fetching and
    /// was cached → refill the album page so missing tracks show up.
    AlbumTracklistFetched {
        artist: String,
        album: String,
    },
    /// Top YouTube hits for a missing track → present a chooser so the user
    /// picks which version to download.
    MissingTrackCandidates {
        artist: String,
        album: String,
        disc: u32,
        position: u32,
        title: String,
        results: Vec<crate::core::youtube::YtResult>,
    },
    /// A missing track finished downloading (or failed) → close the spinner,
    /// refill the album page, toast the outcome.
    MissingTrackDone {
        artist: String,
        album: String,
        ok: bool,
        message: String,
    },
    /// Reachability of the sources (source id → reachable?).
    SourceStatus(Vec<(i64, bool)>),
    /// Cloud sources were re-indexed → rebuild views + covers. `manual` = the
    /// user pressed refresh (force online enrichment regardless of the passive
    /// auto-enrich setting); `false` = silent background top-up at startup.
    CloudReindexed {
        manual: bool,
    },
    /// Background LRCLIB lookup for the running track finished. Carries the path
    /// it was started for (to ignore stale results) and the lyrics if found.
    LyricsLoaded {
        path: String,
        lyrics: Option<crate::core::lyrics::Lyrics>,
    },
}
