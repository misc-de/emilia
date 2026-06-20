//! View/event handlers (on_*): activate, navigation, detail openers, refresh,
//! search actions and the background-command (Cmd) result handlers.
//! Split out of app_views.rs – pure reordering, no functional change.

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::i18n::{gettext, gettext_f};
use crate::ui::app::{online_available, ActiveSource, App, Cmd, CtxTarget, Msg};
use crate::ui::app_views::natural_key;
use crate::ui::fs_row::{FsEntry, FsInput, RowOpts};

impl App {
    /// Activate a file-browser row: descend into a folder, follow a remote
    /// (Nextcloud) entry, or play/toggle a tapped file.
    pub(crate) fn on_activate(&mut self, index: usize, sender: &ComponentSender<Self>) {
        let entry = self
            .libview
            .entries
            .guard()
            .get(index)
            .map(|r| r.entry.clone());
        let Some(entry) = entry else {
            return;
        };
        // Remote entries (Nextcloud) go through their own path.
        if let crate::ui::fs_row::FsEntry::RemoteDir { rel_path, .. } = &entry {
            self.files.remote_browse = Some(rel_path.clone());
            self.load_dir(sender);
            return;
        }
        if let crate::ui::fs_row::FsEntry::RemoteFile { rel_path, .. } = &entry {
            let rel = rel_path.clone();
            self.activate_remote(&rel);
            return;
        }
        {
            if entry.is_dir() {
                let Some(p) = entry.path().cloned() else {
                    return;
                };
                self.files.browse_dir = Some(p);
                self.load_dir(sender);
            } else {
                let Some(path) = entry.path().cloned() else {
                    return;
                };
                // Tapping the active song again → toggle playback
                // (pause/resume), instead of restarting.
                if !self.toggle_if_active_file(&path) {
                    // Is a real queue currently running? Then slip the
                    // single song in between and resume the queue
                    // afterwards at its spot (it stays intact).
                    if self.mini.playing
                        && self.transport.queue.len() > 1
                        && self.transport.interrupted_queue.is_none()
                    {
                        self.transport.interrupted_queue =
                            Some((self.transport.queue.clone(), self.transport.queue_pos));
                    }
                    self.transport.queue = vec![path];
                    self.transport.queue_pos = 0;
                    // A single tapped file is not an album play (see
                    // `PlaySession::source`); a running queue is only paused, not
                    // counted, and resumes with `next_source` already consumed.
                    self.transport.next_source = Some("single");
                    self.play_current();
                    self.refresh_queue_icons();
                }
            }
        }
    }

    /// Toggle membership of a file-browser row in the user queue (a second tap
    /// removes it again). Local and remote (nc:) paths are both supported.
    pub(crate) fn on_toggle_queue(&mut self, index: usize) {
        // Local files use their path, remote (NC) files their synthetic
        // nc: path (resolved via `entry_files`), so both can be queued.
        let entry = self
            .libview
            .entries
            .guard()
            .get(index)
            .filter(|r| !r.entry.is_dir())
            .map(|r| r.entry.clone());
        let path = entry.and_then(|e| self.entry_files(&e).into_iter().next());
        if let Some(path) = path {
            // Toggle membership in the user queue (never the active
            // context): a second tap removes it again.
            if let Some(pos) = self.transport.user_queue.iter().position(|p| *p == path) {
                self.transport.user_queue.remove(pos);
                self.toast(&gettext("Removed from queue"));
            } else {
                self.transport.user_queue.push(path);
                self.toast(&gettext("Will play next"));
            }
            self.reload_queue_list();
            self.refresh_queue_icons();
            self.save_queue();
        }
    }

    /// Open the detail/context menu for a file-browser row.
    pub(crate) fn on_show_context_menu(
        &mut self,
        index: usize,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let entry = self
            .libview
            .entries
            .guard()
            .get(index)
            .map(|r| CtxTarget::Fs(r.entry.clone()));
        if entry.is_some() {
            self.nav.context_target = entry;
            self.open_context_menu(root, sender);
        }
    }

    /// Open the detail/context menu for an artist (by overview index).
    pub(crate) fn on_show_artist_detail(
        &mut self,
        index: usize,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let meta = self
            .libview
            .artists
            .guard()
            .get(index)
            .map(|c| c.meta.clone())
            .or_else(|| self.libview.artists_overview.get(index).cloned());
        if let Some(meta) = meta {
            // Fetch the photo of the opened artist with priority.
            self.fetch_focus_artist(sender, &meta.name);
            self.nav.context_target = Some(CtxTarget::Artist(meta));
            self.open_context_menu(root, sender);
        }
    }

    /// Open the detail/context menu for an album (by overview index).
    pub(crate) fn on_show_album_detail(
        &mut self,
        index: usize,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let meta = self
            .libview
            .albums
            .guard()
            .get(index)
            .map(|c| c.meta.clone())
            .or_else(|| self.libview.albums_overview.get(index).cloned());
        if let Some(meta) = meta {
            // Fetch the cover of the opened album with priority.
            self.fetch_focus_album(sender, &meta.artist, &meta.album);
            self.nav.context_target = Some(CtxTarget::Album(meta));
            self.open_context_menu(root, sender);
        }
    }

    /// Open the detail page of an album via (artist, album) (from subpages).
    pub(crate) fn on_show_album_detail_for(
        &mut self,
        artist: String,
        album: String,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        self.fetch_focus_album(sender, &artist, &album);
        // Load album metadata (for cover/year), otherwise an empty entry.
        let meta = self
            .library
            .get_album_meta(&artist, &album)
            .ok()
            .flatten()
            .unwrap_or_else(|| crate::model::AlbumMeta::pending(artist, album));
        let mut meta = meta;
        if meta
            .cover_path
            .as_deref()
            .is_none_or(|p| p.trim().is_empty())
        {
            meta.cover_path = self.album_cover_for(&meta.artist, &meta.album);
        }
        self.nav.context_target = Some(CtxTarget::Album(meta));
        self.open_context_menu(root, sender);
    }

    /// Open the songs subpage of an album from the album overview (short tap).
    pub(crate) fn on_show_album_tracks(&mut self, index: usize, sender: &ComponentSender<Self>) {
        // Album overview: open by album name (artist irrelevant).
        let album = self
            .libview
            .albums
            .guard()
            .get(index)
            .map(|c| c.meta.album.clone())
            .or_else(|| {
                self.libview
                    .albums_overview
                    .get(index)
                    .map(|m| m.album.clone())
            });
        if let Some(album) = album {
            self.open_album_by_name(sender, &album);
        }
    }

    pub(crate) fn on_show_single_tracks(&mut self, index: usize, sender: &ComponentSender<Self>) {
        let album = self
            .libview
            .singles
            .guard()
            .get(index)
            .map(|c| c.meta.album.clone())
            .or_else(|| {
                self.libview
                    .singles_overview
                    .get(index)
                    .map(|m| m.album.clone())
            });
        if let Some(album) = album {
            self.open_album_by_name(sender, &album);
        }
    }

    pub(crate) fn on_show_compilation_tracks(
        &mut self,
        index: usize,
        sender: &ComponentSender<Self>,
    ) {
        let album = self
            .libview
            .compilations
            .guard()
            .get(index)
            .map(|c| c.meta.album.clone())
            .or_else(|| {
                self.libview
                    .compilations_overview
                    .get(index)
                    .map(|m| m.album.clone())
            });
        if let Some(album) = album {
            self.open_album_by_name(sender, &album);
        }
    }

    pub(crate) fn on_show_single_detail(
        &mut self,
        index: usize,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let meta = self
            .libview
            .singles
            .guard()
            .get(index)
            .map(|c| c.meta.clone())
            .or_else(|| self.libview.singles_overview.get(index).cloned());
        if let Some(meta) = meta {
            self.fetch_focus_album(sender, &meta.artist, &meta.album);
            self.nav.context_target = Some(CtxTarget::Album(meta));
            self.open_context_menu(root, sender);
        }
    }

    pub(crate) fn on_show_compilation_detail(
        &mut self,
        index: usize,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let meta = self
            .libview
            .compilations
            .guard()
            .get(index)
            .map(|c| c.meta.clone())
            .or_else(|| self.libview.compilations_overview.get(index).cloned());
        if let Some(meta) = meta {
            self.fetch_focus_album(sender, &meta.artist, &meta.album);
            self.nav.context_target = Some(CtxTarget::Album(meta));
            self.open_context_menu(root, sender);
        }
    }

    /// Open the detail/context menu for a concert entry (by index).
    pub(crate) fn on_show_concert_detail(
        &mut self,
        index: usize,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        if let Some((scope, key, _, is_dir)) = self.concerts.concert_items.get(index).cloned() {
            self.nav.context_target = Some(self.entry_target(&scope, &key, is_dir));
            self.open_context_menu(root, sender);
        }
    }

    /// Short tap on an artist: open its albums & songs subpage.
    pub(crate) fn on_open_artist_tracks(&mut self, index: usize, sender: &ComponentSender<Self>) {
        let meta = self
            .libview
            .artists
            .guard()
            .get(index)
            .map(|c| c.meta.clone())
            .or_else(|| self.libview.artists_overview.get(index).cloned());
        if let Some(meta) = meta {
            // Fetch the photo of the opened artist with priority.
            self.fetch_focus_artist(sender, &meta.name);
            self.open_artist_tracks(sender, &meta);
        }
    }

    /// File browser: go one level up (local parent dir or remote rel segment).
    pub(crate) fn on_nav_up(&mut self, sender: &ComponentSender<Self>) {
        // Remote source: one rel segment up.
        if let Some(rel) = self.files.remote_browse.clone() {
            if !rel.is_empty() {
                let parent = match rel.rfind('/') {
                    Some(0) | None => String::new(),
                    Some(i) => rel[..i].to_string(),
                };
                self.files.remote_browse = Some(parent);
                self.load_dir(sender);
            }
            return;
        }
        if self.can_go_up() {
            if let Some(parent) = self.files.browse_dir.as_ref().and_then(|d| d.parent()) {
                self.files.browse_dir = Some(parent.to_path_buf());
                self.load_dir(sender);
            }
        }
    }

    /// File browser: jump back to the root of the active source.
    pub(crate) fn on_files_go_start(&mut self, sender: &ComponentSender<Self>) {
        // Remote source: back to the music root of the source.
        if self.files.remote_browse.is_some() {
            if self.files.remote_browse.as_deref() != Some("") {
                self.files.remote_browse = Some(String::new());
                self.load_dir(sender);
            }
            return;
        }
        if let Some(root) = self.files.root_dir.clone() {
            if self.files.browse_dir.as_ref() != Some(&root) {
                self.files.browse_dir = Some(root);
                self.load_dir(sender);
            }
        }
    }

    /// Pull-to-refresh: reload the current dir, rescan the library, re-index
    /// cloud sources and refresh podcast/YouTube subscriptions.
    pub(crate) fn on_refresh(&mut self, sender: &ComponentSender<Self>) {
        // The header "refresh" button is context-aware: a full library re-scan
        // (local files + cloud sources) only belongs to the library views. Every
        // other section refreshes just its own content, so e.g. a podcast refresh
        // no longer re-indexes the whole music library.
        match self.current_section().as_deref() {
            // Podcasts: pull new episodes for every subscribed feed. Reports its
            // worker start/finish via `PodcastRefreshStarted/Finished`, which
            // drive `refresh_pending` (the spinner) on their own.
            Some("podcasts") => {
                if online_available() {
                    self.podcasts_page
                        .emit(crate::ui::podcasts_page::PodcastsInput::RefreshAll);
                }
            }
            // YouTube: refresh the subscribed channels (start/finish via
            // `YtRefreshStarted/Finished`).
            Some("youtube") => {
                if online_available() && self.youtube.enabled {
                    self.yt_page.emit(crate::ui::yt_page::YtInput::RefreshAll);
                }
            }
            // Streaming: reload the saved station lists.
            Some("streaming") => {
                self.stream_page
                    .emit(crate::ui::stream_page::StreamInput::Reload);
            }
            // Files / Artists / Albums: re-scan local files + cloud sources.
            Some("files") | Some("artists") | Some("albums") => {
                self.refresh_library(sender);
            }
            // Any other view (favorites, concerts, audiobooks, playlists, memo,
            // stats): just reload the overviews from the DB — never a disk re-scan.
            _ => self.reload_library_overviews(),
        }
    }

    /// Full library refresh used by the header refresh button on the library
    /// views: re-scan the local music folder and re-index the cloud sources.
    /// Each helper reports whether it spawned a background worker; we count
    /// those so the loading spinner stays up until the last one reports back
    /// (see the matching `Cmd::*` arms).
    pub(crate) fn refresh_library(&mut self, sender: &ComponentSender<Self>) {
        self.load_dir(sender);
        let mut pending = 0u32;
        // Re-index the cloud sources too, so their structure and covers update
        // (existing sources are only indexed when first added). On completion
        // this rebuilds the views and fetches covers. `manual` → fetch online
        // regardless of the auto-enrich setting.
        if self.reindex_cloud_sources(sender, true) {
            pending += 1;
        }
        // "Rescan" also updates the local library (artists/albums).
        if self.start_scan(sender, false, true) {
            pending += 1;
        }
        self.refresh_pending = pending;
    }

    /// Quiet background tick: backfill missing artist photos & online covers.
    pub(crate) fn on_auto_enrich_tick(&mut self, sender: &ComponentSender<Self>) {
        // Quiet backfill of missing artist photos & online covers in the
        // background (rate-limited in the worker). Only if desired, a
        // folder is set, no run is currently active and there is network.
        // If a (full) fetch is already running, the `enriching` lock takes effect and
        // this tick fizzles out – no pileup.
        if self.enrich_state.auto_enrich
            && !self.enrich_state.enriching
            && self.files.music_dir.is_some()
            && online_available()
        {
            self.run_enrich(sender, false, true);
        }
    }

    /// A song hit of the library search was activated: play it directly, or
    /// open its album for remote (`nc:`) hits that can't be played as a file.
    pub(crate) fn on_search_play_track(&mut self, path: String, sender: &ComponentSender<Self>) {
        // A real local file is played directly; remote (`nc:`) hits can't
        // be played as a file, so fall back to opening their album.
        if std::path::Path::new(&path).is_file() {
            self.play_path(&path, false);
        } else if let Some(album) = self
            .library
            .track_by_path(&path)
            .ok()
            .flatten()
            .and_then(|t| t.album)
            .filter(|a| !a.trim().is_empty())
        {
            self.open_album_by_name(sender, &album);
        }
    }

    /// An artist hit of the library search was activated: open its subpage.
    pub(crate) fn on_search_open_artist(&mut self, name: String, sender: &ComponentSender<Self>) {
        self.fetch_focus_artist(sender, &name);
        let meta = self
            .library
            .get_artist_meta(&name)
            .ok()
            .flatten()
            .unwrap_or_else(|| crate::model::ArtistMeta::pending(name.clone()));
        self.open_artist_tracks(sender, &meta);
    }

    /// Download a remote (active-source) file for offline playback.
    pub(crate) fn on_ctx_download_remote(&mut self, rel: String, sender: &ComponentSender<Self>) {
        let Some(creds) = self.active_webdav_creds() else {
            return;
        };
        let Some(dest) = self.remote_cache_path(&rel) else {
            return;
        };
        self.toast(&gettext("Downloading …"));
        sender.spawn_oneshot_command(move || {
            match crate::core::webdav::download(&creds, &rel, &dest) {
                Ok(()) => Cmd::RemoteDownloaded(Ok((rel, dest))),
                Err(e) => Cmd::RemoteDownloaded(Err(e.to_string())),
            }
        });
    }

    /// Open the detail view of the currently running track (or YouTube video).
    pub(crate) fn on_open_now_playing(
        &mut self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        if let Some(path) = self.transport.queue.get(self.transport.queue_pos).cloned() {
            // A running YouTube video (synthetic `yt:<id>` path) needs its
            // own detail (channel / URL / thumbnail) – not the file-tag
            // based track info, which would be empty/wrong for it.
            if let Some(video_id) = path.to_str().and_then(crate::core::youtube::parse_yt_path) {
                let title = self
                    .youtube
                    .video_titles
                    .get(&video_id)
                    .cloned()
                    .or_else(|| self.library.yt_title(&video_id).ok().flatten())
                    .filter(|t| !t.trim().is_empty())
                    .or_else(|| self.mini.now_playing.clone())
                    .unwrap_or_else(|| video_id.clone());
                self.yt_page
                    .emit(crate::ui::yt_page::YtInput::ShowVideoDetail { video_id, title });
            } else if path
                .to_str()
                .and_then(|p| self.library.podcast_id_for_episode_url(p).ok().flatten())
                .is_some()
            {
                // A podcast episode (e.g. played from a playlist, where it lands
                // in the queue with no episode context) → its episode detail, not
                // the file-tag track info which is empty for a remote URL.
                self.podcasts_page.emit(
                    crate::ui::podcasts_page::PodcastsInput::ShowEpisodeDetailByUrl {
                        url: path.to_string_lossy().into_owned(),
                    },
                );
            } else {
                // Detail view of the running track (as a file entry).
                self.nav.context_target = Some(CtxTarget::Fs(FsEntry::file(path)));
                self.open_context_menu(root, sender);
            }
        }
    }

    /// Worker result: populate the file browser with a local folder's entries
    /// and restore the remembered scroll position.
    pub(crate) fn on_cmd_entries(&mut self, mut entries: Vec<FsEntry>) {
        // Apply the file browser's chosen sort (folders stay above files).
        self.sort_fs_entries(&mut entries);
        // Alphabetical headings (by name) for the list, like the library overviews.
        *self.libview.files_headers.borrow_mut() = self.files_section_headers(&entries);
        // "Mixed album": more than one distinct artist in the folder.
        let distinct: std::collections::HashSet<String> = entries
            .iter()
            .filter_map(|e| e.effective_artist())
            .collect();
        let opts = RowOpts {
            show_artist: distinct.len() > 1,
        };
        let queue = self.transport.queue.clone();
        let mut guard = self.libview.entries.guard();
        guard.clear();
        for e in entries {
            let queued = e.path().is_some_and(|ep| queue.iter().any(|p| p == ep));
            guard.push_back((e, opts, queued));
        }
        drop(guard);
        self.libview.entries.widget().invalidate_headers();
        self.libview.loading = false;

        // This folder is now shown; restore the remembered scroll position (from
        // the last visit) after the layout.
        self.files.shown_dir = self.files.browse_dir.clone();
        if let (Some(dir), Some(sc)) = (self.files.browse_dir.clone(), self.fs_scroller()) {
            if let Some(&value) = self.files.fs_scroll.borrow().get(&dir) {
                for delay in [50u64, 250] {
                    let sc = sc.clone();
                    gtk::glib::timeout_add_local_once(
                        std::time::Duration::from_millis(delay),
                        move || sc.vadjustment().set_value(value),
                    );
                }
            }
        }
    }

    /// Worker result: populate the file browser with a remote (WebDAV) folder
    /// listing, then kick off background tag fetching. Stale results (the source
    /// or folder switched meanwhile) are discarded.
    pub(crate) fn on_cmd_remote_entries(
        &mut self,
        result: Result<Vec<crate::core::webdav::DavEntry>, String>,
        source: ActiveSource,
        rel: String,
        sender: &ComponentSender<Self>,
    ) {
        // Discard the stale result (source/folder switched in the meantime).
        if self.files.active_source != source
            || self.files.remote_browse.as_deref() != Some(rel.as_str())
        {
            return;
        }
        self.libview.loading = false;
        match result {
            Err(e) => {
                tracing::warn!("WebDAV listing failed: {e}");
                self.libview.entries.guard().clear();
                // Surface the actual reason persistently (not just a transient
                // toast) so the user can see *why* the folder is empty.
                self.files.remote_error = Some(e);
            }
            Ok(list) => {
                self.files.remote_error = None;
                let (mut dirs, mut files): (Vec<_>, Vec<_>) =
                    list.into_iter().partition(|e| e.is_dir);
                dirs.sort_by_key(|a| natural_key(&a.name));
                files.sort_by_key(|a| natural_key(&a.name));
                // Source id, to read already-indexed track metadata
                // (title/artist/duration) straight from the DB.
                let source_id = match &source {
                    ActiveSource::Source(id) => Some(*id),
                    _ => None,
                };
                let mut entries: Vec<FsEntry> = Vec::with_capacity(dirs.len() + files.len());
                for d in dirs {
                    entries.push(FsEntry::remote_dir(d.rel_path, d.name));
                }
                for f in files {
                    let cached = self.remote_cache_path(&f.rel_path).filter(|p| p.exists());
                    // If the source was indexed, the tags already live in
                    // the DB → show them at once instead of re-reading them
                    // over the network row by row.
                    let meta = source_id.and_then(|id| {
                        self.library
                            .track_by_path(&crate::core::webdav::nc_path(id, &f.rel_path))
                            .ok()
                            .flatten()
                    });
                    let (title, artist, duration_ms) = match meta {
                        Some(t) => (Some(t.title), t.artist, t.duration_ms),
                        None => (None, None, None),
                    };
                    entries.push(FsEntry::remote_file(
                        f.rel_path,
                        f.name,
                        cached,
                        title,
                        artist,
                        duration_ms,
                    ));
                }
                // Apply the file browser's chosen sort (folders stay above files);
                // overrides the default name order built above.
                self.sort_fs_entries(&mut entries);
                *self.libview.files_headers.borrow_mut() = self.files_section_headers(&entries);
                let distinct: std::collections::HashSet<String> = entries
                    .iter()
                    .filter_map(|e| e.effective_artist())
                    .collect();
                let opts = RowOpts {
                    show_artist: distinct.len() > 1,
                };
                {
                    let mut guard = self.libview.entries.guard();
                    guard.clear();
                    for e in entries {
                        guard.push_back((e, opts, false));
                    }
                }
                self.libview.entries.widget().invalidate_headers();
                self.refresh_queue_icons();
                // Fetch the tags of the remote files in the background.
                if let Some(src) = self.active_remote_source() {
                    self.start_remote_tag_fetch(sender, &src);
                }
            }
        }
    }

    /// Worker result: apply backfilled tags (title/artist/duration) to the
    /// matching remote file rows.
    pub(crate) fn on_cmd_remote_tags(
        &mut self,
        tags: Vec<(String, Option<String>, Option<String>, Option<i64>)>,
    ) {
        // rel path → factory index, then send tags to the respective row.
        let map: std::collections::HashMap<String, usize> = {
            let guard = self.libview.entries.guard();
            (0..guard.len())
                .filter_map(|i| {
                    guard.get(i).and_then(|r| match &r.entry {
                        FsEntry::RemoteFile { rel_path, .. } => Some((rel_path.clone(), i)),
                        _ => None,
                    })
                })
                .collect()
        };
        for (rel, title, artist, duration_ms) in tags {
            if let Some(&i) = map.get(&rel) {
                self.libview.entries.send(
                    i,
                    FsInput::SetTags {
                        title,
                        artist,
                        duration_ms,
                    },
                );
            }
        }
    }

    // ---- Missing-track recovery (greyed album entries) ----

    /// Confirm dialog for a greyed "missing" track: offer to search & add it.
    pub(crate) fn show_missing_track(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
        artist: String,
        album: String,
        disc: u32,
        position: u32,
        title: String,
    ) {
        let body = gettext_f(
            "“{title}” is missing from this album. Search for it online and add it to the album?",
            &[("title", &title)],
        );
        let dialog = adw::AlertDialog::new(Some(&gettext("Add missing track")), Some(&body));
        dialog.add_response("cancel", &gettext("Cancel"));
        dialog.add_response("add", &gettext("Search & add"));
        dialog.set_response_appearance("add", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("add"));
        dialog.set_close_response("cancel");
        // `connect_response` is `Fn`; build the message once and take it on use.
        let msg = std::cell::RefCell::new(Some(Msg::AddMissingTrack {
            artist,
            album,
            disc,
            position,
            title,
        }));
        let sender = sender.clone();
        dialog.connect_response(None, move |_, resp| {
            if resp == "add" {
                if let Some(m) = msg.borrow_mut().take() {
                    sender.input(m);
                }
            }
        });
        dialog.present(Some(root));
    }

    /// Start adding a missing track: search YouTube for the top hits and let the
    /// user choose which version to add. The actual download happens once a
    /// candidate is picked (see [`Self::download_missing_track`]).
    pub(crate) fn add_missing_track(
        &mut self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
        artist: String,
        album: String,
        disc: u32,
        position: u32,
        title: String,
    ) {
        if !self.youtube.enabled || !crate::core::youtube::available() {
            self.toast(&gettext("Enable YouTube in the settings to use this"));
            return;
        }
        // Fail early if no sibling track pins down the album folder.
        if self.missing_track_dest(&artist, &album, disc).is_none() {
            self.toast(&gettext("Cannot determine the album folder"));
            return;
        }
        let query = format!("{artist} {title}");

        self.show_missing_busy(root, &gettext("Searching online …"));

        sender.spawn_command(move |out| {
            let results =
                crate::core::youtube::search(&query, crate::core::youtube::YtKind::Video, 10)
                    .unwrap_or_default();
            let _ = out.send(Cmd::MissingTrackCandidates {
                artist,
                album,
                disc,
                position,
                title,
                results,
            });
        });
    }

    /// Where a missing track would be added: the album folder (a sibling track's
    /// directory, preferring the same disc) plus the album's year/cover for
    /// tagging. `None` when no sibling track pins down the folder.
    fn missing_track_dest(
        &self,
        artist: &str,
        album: &str,
        disc: u32,
    ) -> Option<(std::path::PathBuf, Option<i32>, Option<String>)> {
        let siblings = self.album_tracks_for_artist(artist, album);
        let dest_dir = siblings
            .iter()
            .find(|t| crate::ui::app_views::track_disc(t) == disc)
            .or_else(|| siblings.first())
            .and_then(|t| {
                std::path::Path::new(&t.path)
                    .parent()
                    .map(|p| p.to_path_buf())
            })?;
        let meta = self.library.get_album_meta(artist, album).ok().flatten();
        let year = meta.as_ref().and_then(|m| m.year);
        let cover = meta
            .as_ref()
            .and_then(|m| m.cover_path.clone())
            .or_else(|| self.album_cover_for(artist, album));
        Some((dest_dir, year, cover))
    }

    /// Chooser for a missing track: list the top YouTube hits (title, uploader,
    /// duration) so the user decides which version to add. Selecting a row
    /// downloads that video into the album folder.
    pub(crate) fn show_missing_candidates(
        &mut self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
        artist: String,
        album: String,
        disc: u32,
        position: u32,
        title: String,
        results: Vec<crate::core::youtube::YtResult>,
    ) {
        // Close the "Searching …" spinner first.
        if let Some((d, _)) = self.libview.missing_busy.take() {
            d.close();
        }
        if results.is_empty() {
            self.toast(&gettext("Not found on YouTube"));
            return;
        }

        let dialog = adw::Dialog::builder()
            .title(gettext("Choose a version"))
            .content_width(420)
            .build();
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(16)
            .margin_bottom(16)
            .margin_start(16)
            .margin_end(16)
            .build();
        content.append(
            &gtk::Label::builder()
                .label(gettext_f(
                    "Pick the version of “{title}” to add",
                    &[("title", &title)],
                ))
                .wrap(true)
                .xalign(0.0)
                .css_classes(["dim-label"])
                .build(),
        );

        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        let rows = results.len() as i32;
        dialog.set_content_height((220 + rows * 64).min(720));

        for r in &results {
            let mut subtitle = String::new();
            if let Some(u) = r.uploader.as_deref().filter(|s| !s.trim().is_empty()) {
                subtitle.push_str(u);
            }
            if let Some(d) = r.duration {
                if !subtitle.is_empty() {
                    subtitle.push_str(" · ");
                }
                subtitle.push_str(&crate::ui::yt_page::fmt_duration(d));
            }
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&r.title))
                .subtitle(gtk::glib::markup_escape_text(&subtitle))
                .activatable(true)
                .build();
            row.add_prefix(&gtk::Image::from_icon_name("video-x-generic-symbolic"));
            {
                let (sender, dialog, video_id) = (sender.clone(), dialog.clone(), r.id.clone());
                let (artist, album, title) = (artist.clone(), album.clone(), title.clone());
                row.connect_activated(move |_| {
                    sender.input(Msg::DownloadMissingTrack {
                        artist: artist.clone(),
                        album: album.clone(),
                        disc,
                        position,
                        title: title.clone(),
                        video_id: video_id.clone(),
                    });
                    dialog.close();
                });
            }
            list.append(&row);
        }

        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&list)
            .build();
        content.append(&scroller);
        dialog.set_child(Some(&content));
        dialog.present(Some(root));
    }

    /// Download the chosen YouTube video into the album folder, tag + index it,
    /// then refresh the page. Shows a phase spinner.
    pub(crate) fn download_missing_track(
        &mut self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
        artist: String,
        album: String,
        disc: u32,
        position: u32,
        title: String,
        video_id: String,
    ) {
        let Some((dest_dir, year, cover)) = self.missing_track_dest(&artist, &album, disc) else {
            self.toast(&gettext("Cannot determine the album folder"));
            return;
        };

        self.show_missing_busy(root, &gettext("Downloading …"));

        sender.spawn_command(move |out| {
            let (ok, message) = match crate::core::youtube::add_video_to_album(
                &video_id,
                &dest_dir,
                &artist,
                &album,
                &title,
                position,
                Some(disc),
                year,
                cover.as_deref(),
            ) {
                Ok(_) => (true, gettext("Track added")),
                Err(e) => (false, e),
            };
            let _ = out.send(Cmd::MissingTrackDone {
                artist,
                album,
                ok,
                message,
            });
        });
    }

    /// Build + show the phase spinner for the missing-track download.
    fn show_missing_busy(&mut self, root: &adw::ApplicationWindow, text: &str) {
        if let Some((d, _)) = self.libview.missing_busy.take() {
            d.close();
        }
        let dialog = adw::Dialog::builder().content_width(300).build();
        let vb = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(16)
            .margin_top(28)
            .margin_bottom(28)
            .margin_start(28)
            .margin_end(28)
            .halign(gtk::Align::Center)
            .build();
        let spinner = gtk::Spinner::builder()
            .width_request(32)
            .height_request(32)
            .build();
        spinner.set_spinning(true);
        let label = gtk::Label::builder()
            .label(text)
            .wrap(true)
            .justify(gtk::Justification::Center)
            .build();
        vb.append(&spinner);
        vb.append(&label);
        dialog.set_child(Some(&vb));
        dialog.present(Some(root));
        self.libview.missing_busy = Some((dialog, label));
    }

    /// A missing-track download finished: close the spinner, refresh the album
    /// page (greyed entry → real track) and report the outcome.
    pub(crate) fn on_missing_track_done(
        &mut self,
        sender: &ComponentSender<Self>,
        artist: String,
        album: String,
        ok: bool,
        message: String,
    ) {
        if let Some((d, _)) = self.libview.missing_busy.take() {
            d.close();
        }
        if ok {
            self.refill_album_page(sender, &artist, &album);
        }
        self.toast(&message);
    }
}
