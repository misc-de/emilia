//! The bodies of the root component's [`Component::update`] and
//! [`Component::update_cmd`]: one dispatch arm per message, delegating to the
//! domain `on_*` / `update_*` methods. Split out of [`crate::ui::app`] (whose
//! trait methods just forward here) – pure code movement.

use std::path::PathBuf;

use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::i18n::{gettext, gettext_f};
use crate::ui::app::{App, Cmd, CtxTarget, Msg};
use crate::ui::fs_row::{FsEntry, FsInput};

impl App {
    /// Body of [`Component::update`]: dispatches one UI message.
    pub(crate) fn handle_msg(
        &mut self,
        msg: Msg,
        sender: ComponentSender<Self>,
        root: &adw::ApplicationWindow,
    ) {
        match msg {
            Msg::Activate(index) => self.on_activate(index, &sender),
            Msg::ToggleQueue(index) => self.on_toggle_queue(index),
            Msg::ShowContextMenu(index) => self.on_show_context_menu(index, root, &sender),
            Msg::ShowArtistDetail(index) => self.on_show_artist_detail(index, root, &sender),
            Msg::ShowAlbumDetail(index) => self.on_show_album_detail(index, root, &sender),
            Msg::ShowAlbumDetailFor { artist, album } => {
                self.on_show_album_detail_for(artist, album, root, &sender)
            }
            Msg::ShowTrackDetail(path) => {
                self.nav.context_target = Some(CtxTarget::Fs(FsEntry::file(PathBuf::from(path))));
                self.open_context_menu(root, &sender);
            }
            Msg::ShowAlbumTracks(index) => self.on_show_album_tracks(index, &sender),
            Msg::PlayAlbumAt(index) => self.on_play_album_at("albums", index),
            Msg::PlaySingleAt(index) => self.on_play_album_at("singles", index),
            Msg::PlayCompilationAt(index) => self.on_play_album_at("compilations", index),
            Msg::ShowSingleTracks(index) => self.on_show_single_tracks(index, &sender),
            Msg::ShowSingleDetail(index) => self.on_show_single_detail(index, root, &sender),
            Msg::ShowCompilationTracks(index) => self.on_show_compilation_tracks(index, &sender),
            Msg::ShowCompilationDetail(index) => {
                self.on_show_compilation_detail(index, root, &sender)
            }
            Msg::OpenArtistTracks(index) => self.on_open_artist_tracks(index, &sender),
            Msg::OpenAlbumTracks { artist, album } => {
                self.fetch_focus_album(&sender, &artist, &album);
                self.open_album_tracks(&sender, &artist, &album);
            }
            Msg::ShowMissingTrack {
                artist,
                album,
                disc,
                position,
                title,
            } => self.show_missing_track(root, &sender, artist, album, disc, position, title),
            Msg::AddMissingTrack {
                artist,
                album,
                disc,
                position,
                title,
            } => self.add_missing_track(root, &sender, artist, album, disc, position, title),
            Msg::DownloadMissingTrack {
                artist,
                album,
                disc,
                position,
                title,
                video_id,
            } => self.download_missing_track(
                root, &sender, artist, album, disc, position, title, video_id,
            ),
            Msg::OpenEntryTracks { scope, key } => match scope.as_str() {
                "album" => {
                    // key = "Artist\u{1}Album"
                    let mut parts = key.splitn(2, '\u{1}');
                    let artist = parts.next().unwrap_or("").to_string();
                    let album = parts.next().unwrap_or("").to_string();
                    self.open_album_tracks(&sender, &artist, &album);
                }
                "folder" => self.open_folder_tracks(&sender, &key),
                _ => {}
            },
            Msg::PlayFolderTrack {
                folder,
                path,
                close,
            } => self.on_play_folder_track(folder, path, close),
            Msg::PlayArtistTrack { name, path, close } => {
                self.on_play_artist_track(name, path, close)
            }
            Msg::PlayOneTrack { path, close } => self.on_play_one_track(path, close),
            Msg::PlayAlbum { artist, album } => self.on_play_album(artist, album),
            Msg::PlayFsAlbum(idx) => {
                // The play button on an album folder in the file browser.
                let info = self
                    .libview
                    .entries
                    .guard()
                    .get(idx)
                    .and_then(|r| r.entry.album().cloned());
                if let Some(a) = info {
                    sender.input(Msg::PlayAlbum {
                        artist: a.artist,
                        album: a.album,
                    });
                }
            }
            Msg::Playlist(m) => self.update_playlist(m, root, &sender),
            // --- Voice memos ---
            Msg::Memo(m) => self.update_memo(m, root, &sender),
            Msg::RefreshProgress { done, total, label } => {
                self.refresh_summary = None;
                self.refresh_progress = Some((done, total, label));
            }
            Msg::RefreshSummary(text) => {
                self.refresh_progress = None;
                self.refresh_summary = Some(text);
                // Hold the outcome in the overlay just long enough to read it,
                // then let the view go back to the content.
                let input = sender.input_sender().clone();
                gtk::glib::timeout_add_local_once(
                    std::time::Duration::from_millis(2600),
                    move || {
                        let _ = input.send(Msg::ClearRefreshSummary);
                    },
                );
            }
            Msg::ClearRefreshSummary => self.refresh_summary = None,
            Msg::DismissOverlay => self.overlay_dismissed = true,
            Msg::OpenSync => {
                use crate::ui::sync_page::SyncInput;
                self.sync_page.emit(SyncInput::Open(root.clone()));
            }
            Msg::SyncConnected(connected) => self.sync_connected = connected,
            Msg::SyncBusy(busy) => self.sync_busy = busy,
            Msg::SyncImported => {
                self.load_favorites(&sender);
                self.reload_playlists(&sender);
                self.podcasts_page
                    .emit(crate::ui::podcasts_page::PodcastsInput::Reload);
                // Received audio files were indexed into the `track` table as they
                // arrived → rebuild the artist/album overviews so they show up.
                self.reload_library_overviews();
            }
            Msg::AutoEnrichTick => self.on_auto_enrich_tick(&sender),
            Msg::StartupOnline => self.on_startup_online(&sender),
            Msg::FingerprintCurrent(path) => self.fetch_focus_track(&sender, &path),
            Msg::Mpris(cmd) => self.handle_mpris(root, cmd),
            Msg::Mcp(cmd) => self.handle_mcp(cmd, root, &sender),
            Msg::McpSetting(m) => self.update_mcp_setting(m),
            Msg::NavUp => self.on_nav_up(&sender),
            Msg::FilesGoStart => self.on_files_go_start(&sender),
            Msg::Refresh => self.on_refresh(&sender),
            Msg::ScanCancel => {
                // The user's cancel also drops a queued follow-up scan.
                self.scan_restart = None;
                self.scan_cancel
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
            Msg::OpenSettings => self.open_settings(root, &sender),
            Msg::SetSleepTimer(choice) => self.on_set_sleep_timer(choice),
            Msg::OpenSearch => self.open_search_dialog(root, &sender),
            Msg::SearchPlayTrack(path) => self.on_search_play_track(path, &sender),
            Msg::SearchOpenAlbum(artist, album) => self.open_album_card(&sender, &artist, &album),
            Msg::SearchOpenArtist(name) => self.on_search_open_artist(name, &sender),
            Msg::OpenGlobalEq => self.open_global_eq(root, &sender),
            Msg::OpenCurrentEq => self.on_open_current_eq(root, &sender),
            Msg::OpenLiveEq { video_id, title } => {
                self.open_live_eq(root, &sender, &video_id, &title);
            }
            Msg::OpenTrackEq { path, title } => {
                self.open_eq_editor(root, &sender, "the track", &title, None, "track", path);
            }
            Msg::NavBack => {
                self.nav.nav_view.pop();
            }
            Msg::Source(m) => self.update_source(m, root, &sender),
            Msg::Design(m) => self.update_design(m, root, &sender),
            Msg::Tray(m) => self.update_tray(m, root, &sender),
            Msg::Sort(m) => self.update_sort(m, &sender),
            Msg::Eq(m) => self.update_eq(m),
            Msg::Stream(m) => self.update_stream(m, root, &sender),
            Msg::Edit(m) => self.update_edit(m, root, &sender),
            Msg::Yt(m) => self.update_yt(m, root, &sender),
            Msg::Podcast(m) => self.update_podcast(m, root, &sender),
            Msg::Lyrics(m) => self.update_lyrics(m, root, &sender),
            Msg::Concert(m) => self.update_concert(m, root, &sender),
            Msg::Favorite(m) => self.update_favorite(m, root, &sender),
            Msg::Cover(m) => self.update_cover(m, root, &sender),
            Msg::Setting(m) => self.update_setting(m, root, &sender),
            Msg::Ctx(m) => self.update_ctx(m, root, &sender),
            Msg::Transport(m) => self.update_transport(m, root, &sender),
        }
        // Suppress the per-second tick the moment the app goes idle (and resume
        // it when playback/recording starts) — see `tick_active`.
        self.sync_tick_active();
        self.sync_overlay_dismissable();
    }

    /// Body of [`Component::update_cmd`]: processes the results of the
    /// background workers.
    pub(crate) fn handle_cmd(
        &mut self,
        msg: Cmd,
        sender: ComponentSender<Self>,
        root: &adw::ApplicationWindow,
    ) {
        match msg {
            Cmd::Entries(entries) => self.on_cmd_entries(entries),
            Cmd::RemoteEntries(result, source, rel) => {
                self.on_cmd_remote_entries(result, source, rel, &sender)
            }
            Cmd::RemoteTags(tags) => self.on_cmd_remote_tags(tags),
            Cmd::RemoteDownloaded(result) => match result {
                Ok((rel, path)) => {
                    let idx = {
                        let guard = self.libview.entries.guard();
                        (0..guard.len()).find(|&i| {
                            guard.get(i).is_some_and(|r| {
                                matches!(&r.entry, FsEntry::RemoteFile { rel_path, .. } if *rel_path == rel)
                            })
                        })
                    };
                    if let Some(i) = idx {
                        self.libview.entries.send(i, FsInput::SetDownloaded(path));
                    }
                    self.toast(&gettext("Download complete"));
                }
                Err(e) => {
                    tracing::warn!("Download failed: {e}");
                    self.toast(&gettext("Download failed"));
                }
            },
            Cmd::EnrichDone { changed } => {
                self.enrich_state.enriching = false;
                // Only rebuild if the run changed something – the quiet
                // per-minute backfill otherwise runs empty and would re-render the
                // lists for no reason.
                if changed {
                    self.reload_library_overviews();
                }
            }
            Cmd::ReloadViews => {
                self.reload_library_overviews();
            }
            Cmd::ScanDone {
                then_enrich,
                manual,
            } => self.on_cmd_scan_done(then_enrich, manual, &sender),
            Cmd::ScanProgress {
                done,
                total,
                bytes,
                total_bytes,
            } => {
                self.scan_done = done;
                self.scan_total = total;
                self.scan_bytes = bytes;
                self.scan_total_bytes = total_bytes;
            }
            Cmd::CloudReindexed { manual } => self.on_cmd_cloud_reindexed(manual, &sender),
            Cmd::Candidates(candidates) => {
                if candidates.is_empty() {
                    self.toast(&gettext("No new concert candidates found"));
                } else {
                    self.open_concert_import_dialog(root, &sender, candidates);
                }
            }
            Cmd::YtDlpReady(result) => {
                self.youtube.ytdlp_busy = false;
                match result {
                    Ok(v) => {
                        self.youtube.ytdlp_version = Some(v.clone());
                        self.toast(&gettext_f("yt-dlp ready (version {v})", &[("v", &v)]));
                    }
                    Err(e) => {
                        tracing::warn!("yt-dlp setup failed: {e}");
                        self.toast(&gettext("yt-dlp download failed"));
                    }
                }
                self.refresh_ytdlp_status_label();
            }
            Cmd::YtDlpAutoUpdated(result) => {
                self.youtube.ytdlp_busy = false;
                match result {
                    // Silent on success (version label only) and on failure (just
                    // log) — an auto-update must not interrupt with toasts.
                    Ok(v) => self.youtube.ytdlp_version = Some(v),
                    Err(e) => tracing::debug!("yt-dlp auto-update skipped: {e}"),
                }
                self.refresh_ytdlp_status_label();
            }
            Cmd::YtDlpChecked(version) => {
                self.youtube.ytdlp_version = version;
                self.refresh_ytdlp_status_label();
            }
            Cmd::YtReload => self.yt_page.emit(crate::ui::yt_page::YtInput::Reload),
            Cmd::LyricsLoaded { path, lyrics } => self.on_lyrics_loaded(path, lyrics),
            Cmd::YtPlaylistStart {
                url,
                title,
                items,
                total_duration,
            } => self.on_cmd_yt_playlist_start(url, title, items, total_duration, &sender),
            Cmd::HeardResolved {
                video_id,
                title,
                artist,
                download,
            } => self.on_heard_resolved(video_id, title, artist, download),
            Cmd::AlbumTracklistFetched { artist, album } => {
                self.refill_album_page(&sender, &artist, &album);
            }
            Cmd::MissingTrackCandidates {
                artist,
                album,
                disc,
                position,
                title,
                results,
            } => self.show_missing_candidates(
                root, &sender, artist, album, disc, position, title, results,
            ),
            Cmd::MissingTrackDone {
                artist,
                album,
                ok,
                message,
            } => self.on_missing_track_done(&sender, artist, album, ok, message),
            Cmd::SourceStatus(status) => {
                self.checking_sources = false;
                let mut changed = false;
                for (id, ok) in status {
                    if ok {
                        changed |= self.offline_sources.remove(&id);
                    } else {
                        changed |= self.offline_sources.insert(id);
                    }
                }
                // Changed connection state → rebuild the views, so that the
                // red "Disconnected" hint appears/disappears.
                if changed {
                    self.reload_library_overviews();
                }
            }
        }
        self.sync_tick_active();
        self.sync_overlay_dismissable();
    }
}
