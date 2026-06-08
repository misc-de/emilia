//! YouTube transport + yt-dlp/settings glue that stays on `App`. The YouTube
//! *page* (lists, dialogs, search, downloads) lives in [`crate::ui::yt_page`];
//! what remains here is what belongs to the shared transport and the yt-dlp
//! installation/settings: playing a video/channel/playlist (which mutates the
//! single player/mini/mpris/queue/stats), the now-playing markers
//! (`playing_video_id`/`video_titles`/`playing_playlist`), the recent-play
//! logging/enrichment, and the yt-dlp download/auto-update/status. After these
//! touch the page's data (e.g. logging a recent play) they tell the component to
//! reload via [`YtInput`].

use std::path::PathBuf;

use adw::prelude::*;
use relm4::{adw, ComponentController, ComponentSender};

use crate::core::db::Library;
use crate::core::youtube;
use crate::i18n::{gettext, gettext_f};
use crate::ui::app::{App, Msg};
use crate::ui::yt_page::YtInput;

/// Re-download the managed yt-dlp when it is at least this old (it breaks as
/// YouTube changes, so a weekly refresh keeps the feature working hands-off).
const YTDLP_AUTO_UPDATE_AGE: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 60 * 60);

impl App {
    // ---- recent logging + enrichment -------------------------------------

    /// Logs a played video to the "Recent" history and enriches it (artist +
    /// cover) from the online DB in the background. Called from `play_current`
    /// for every `yt:` track.
    pub(crate) fn note_youtube_play(&self, video_id: &str, title: &str) {
        let _ = self.library.set_yt_title(video_id, title);
        if !self.youtube.playing_playlist {
            let _ = self.library.add_recent_video(video_id, title, None);
        }
        let input = self.input.clone();
        let (vid, t) = (video_id.to_string(), title.to_string());
        std::thread::spawn(move || {
            let lib = Library::open().ok();
            let stored = lib
                .as_ref()
                .and_then(|l| l.yt_video_info(&vid).ok().flatten());
            let meta = if stored.is_none() {
                youtube::video_meta(&vid).ok()
            } else {
                None
            };
            if let (Some(l), Some(d)) = (lib.as_ref(), meta.as_ref().and_then(|m| m.duration)) {
                let _ = l.set_yt_meta(&vid, &t, Some(d));
            }
            let artist = stored
                .map(|(c, _, _)| c)
                .or_else(|| meta.as_ref().and_then(|m| m.uploader.clone()))
                .map(|c| youtube::clean_channel_name(&c))
                .filter(|s| !s.trim().is_empty());
            let cover = artist
                .as_deref()
                .and_then(|a| crate::core::online::track_cover(a, &t))
                .and_then(|(bytes, _album)| crate::core::online::store_youtube_cover(&vid, &bytes))
                .or_else(|| {
                    crate::core::online::cache_youtube_thumb(&youtube::thumbnail_url(&vid))
                });
            let _ = input.send(Msg::YtEnriched {
                video_id: vid,
                artist,
                cover,
            });
        });
    }

    /// Online enrichment (artist + cover) for a played video finished.
    pub(crate) fn yt_enriched(
        &mut self,
        video_id: String,
        artist: Option<String>,
        cover: Option<String>,
    ) {
        let _ = self
            .library
            .set_recent_meta(&video_id, artist.as_deref(), cover.as_deref());
        if self.youtube.playing_video_id.as_deref() == Some(video_id.as_str()) {
            if let Some(now) = self.mini.now_playing.clone() {
                self.mpris
                    .set_metadata(0, &now, artist.as_deref(), None, None, cover.as_deref());
            }
        }
        self.yt_page.emit(YtInput::ReloadRecent);
    }

    // ---- playback --------------------------------------------------------

    /// Plays a subscribed channel's cached videos as the queue.
    pub(crate) fn yt_play_channel(&mut self, id: i64) {
        let videos = self.library.channel_videos(id).unwrap_or_default();
        if videos.is_empty() {
            self.toast(&gettext("No videos"));
            return;
        }
        self.youtube.playing_playlist = false;
        self.youtube.video_titles.clear();
        let mut queue = Vec::with_capacity(videos.len());
        for v in videos {
            let _ = self.library.set_yt_title(&v.video_id, &v.title);
            self.youtube
                .video_titles
                .insert(v.video_id.clone(), v.title);
            queue.push(PathBuf::from(youtube::yt_path(&v.video_id)));
        }
        self.transport.queue = queue;
        self.transport.queue_pos = 0;
        self.play_current();
    }

    /// Play a single video (toggles if it is already the running one).
    pub(crate) fn yt_play_video(&mut self, video_id: String, title: String) {
        if self.youtube.playing_video_id.as_deref() == Some(video_id.as_str()) {
            if self.mini.playing {
                self.player.pause();
            } else {
                self.player.resume();
            }
            self.mini.playing = !self.mini.playing;
            self.mpris.set_playing(self.mini.playing);
            self.refresh_queue_icons();
        } else {
            self.youtube.playing_playlist = false;
            self.youtube.video_titles.clear();
            self.youtube
                .video_titles
                .insert(video_id.clone(), title.clone());
            let _ = self.library.set_yt_title(&video_id, &title);
            self.transport.queue = vec![PathBuf::from(youtube::yt_path(&video_id))];
            self.transport.queue_pos = 0;
            self.play_current();
        }
    }

    /// Resolve a playlist URL to its videos (yt-dlp) and start playing it.
    pub(crate) fn yt_start_playlist(
        &mut self,
        sender: &ComponentSender<Self>,
        url: String,
        title: String,
    ) {
        self.toast(&gettext_f(
            "Starting playlist “{title}” …",
            &[("title", &title)],
        ));
        sender.spawn_command(move |out| {
            let videos = youtube::list_playlist(&url, 200).unwrap_or_default();
            let total: i64 = videos.iter().filter_map(|v| v.duration).sum();
            let items: Vec<(String, String)> =
                videos.into_iter().map(|v| (v.id, v.title)).collect();
            let _ = out.send(crate::ui::app::Cmd::YtPlaylistStart {
                url,
                title,
                items,
                total_duration: (total > 0).then_some(total),
            });
        });
    }

    /// Play the (already-resolved) playlist videos as the queue, starting at
    /// `index`. The videos come from the component's session cache (it owns the
    /// playlist-songs subpage); the transport part lives here.
    pub(crate) fn yt_start_playlist_at(
        &mut self,
        url: String,
        title: String,
        index: usize,
        close: bool,
        videos: Vec<(String, String, Option<i64>)>,
    ) {
        if videos.is_empty() {
            return;
        }
        let index = index.min(videos.len() - 1);
        self.youtube.video_titles.clear();
        let mut queue = Vec::with_capacity(videos.len());
        for (id, vtitle, dur) in &videos {
            self.youtube.video_titles.insert(id.clone(), vtitle.clone());
            let _ = self.library.set_yt_meta(id, vtitle, *dur);
            queue.push(PathBuf::from(youtube::yt_path(id)));
        }
        self.youtube.playing_playlist = true;
        let total: i64 = videos.iter().filter_map(|(_, _, d)| *d).sum();
        let _ = self.library.add_recent_playlist(
            &url,
            &title,
            videos.len() as i64,
            (total > 0).then_some(total),
        );
        if let Some((id, _, _)) = videos.first() {
            let _ = self
                .library
                .set_recent_thumb(&url, &youtube::thumbnail_url(id));
        }
        self.transport.queue = queue;
        self.transport.queue_pos = index;
        self.play_current();
        self.refresh_queue_icons();
        self.yt_page.emit(YtInput::ReloadRecent);
        if close {
            self.nav.nav_view.pop();
        }
    }

    /// A resolved YouTube audio stream URL came back → start playback at `resume`.
    pub(crate) fn yt_stream_resolved(
        &mut self,
        sender: &ComponentSender<Self>,
        video_id: String,
        resume: i64,
        result: Result<String, String>,
    ) {
        if self.youtube.playing_video_id.as_deref() != Some(video_id.as_str()) {
            return;
        }
        match result {
            Ok(url) => match self.player.play_uri(&url, resume) {
                Ok(()) => {
                    let start = self.player.position_ms().unwrap_or(resume.max(0));
                    self.mpris.set_position(start);
                    self.start_play_session(PathBuf::from(youtube::yt_path(&video_id)), 0);
                }
                Err(e) => {
                    tracing::error!("yt play_uri failed: {e}");
                    self.youtube_playback_failed(sender);
                }
            },
            Err(e) => {
                tracing::warn!("yt resolve failed: {e}");
                self.youtube_playback_failed(sender);
            }
        }
    }

    /// Worker result: a YouTube playlist resolved → load its videos as the queue,
    /// log it as one "Recent" entry and mirror it into the Playlists section.
    pub(crate) fn on_cmd_yt_playlist_start(
        &mut self,
        url: String,
        title: String,
        items: Vec<(String, String)>,
        total_duration: Option<i64>,
        sender: &ComponentSender<Self>,
    ) {
        if items.is_empty() {
            self.toast(&gettext("Playlist is empty"));
        } else {
            self.youtube.video_titles.clear();
            let mut queue = Vec::with_capacity(items.len());
            let mut paths = Vec::with_capacity(items.len());
            for (id, vtitle) in &items {
                self.youtube.video_titles.insert(id.clone(), vtitle.clone());
                let _ = self.library.set_yt_title(id, vtitle);
                let p = youtube::yt_path(id);
                paths.push(p.clone());
                queue.push(PathBuf::from(p));
            }
            self.youtube.playing_playlist = true;
            let _ =
                self.library
                    .add_recent_playlist(&url, &title, items.len() as i64, total_duration);
            if let Some((id, _)) = items.first() {
                let _ = self
                    .library
                    .set_recent_thumb(&url, &youtube::thumbnail_url(id));
            }
            let _ = self.library.replace_yt_playlist(&url, &title, &paths);
            self.transport.queue = queue;
            self.transport.queue_pos = 0;
            self.play_current();
            self.reload_playlists(sender);
            self.yt_page.emit(YtInput::ReloadRecent);
            self.yt_page
                .emit(YtInput::SetView(crate::ui::app::YtView::Recent));
        }
    }

    /// Resets the optimistic now-playing state after a failed resolve/stream.
    pub(crate) fn youtube_playback_failed(&mut self, _sender: &ComponentSender<Self>) {
        self.mini.playing = false;
        self.mini.loading = false;
        self.youtube.playing_video_id = None;
        self.mpris.set_playing(false);
        self.refresh_queue_icons();
        self.toast(&gettext("Could not play video"));
    }

    // ---- yt-dlp install / status -----------------------------------------

    /// Starts a yt-dlp download (or update) in the background.
    pub(crate) fn start_ytdlp_fetch(&mut self, update: bool, sender: &ComponentSender<Self>) {
        if self.youtube.ytdlp_busy {
            return;
        }
        self.youtube.ytdlp_busy = true;
        let msg = if update {
            gettext("Updating yt-dlp …")
        } else {
            gettext("Downloading yt-dlp …")
        };
        self.toast(&msg);
        self.refresh_ytdlp_status_label();
        sender.spawn_command(move |out| {
            let result = if update {
                youtube::update_ytdlp()
            } else {
                youtube::download_ytdlp()
            }
            .map_err(|e| e.to_string());
            let _ = out.send(crate::ui::app::Cmd::YtDlpReady(result));
        });
    }

    /// Background auto-update of the managed yt-dlp copy.
    pub(crate) fn maybe_auto_update_ytdlp(&mut self, sender: &ComponentSender<Self>) {
        if !self.youtube.enabled || self.youtube.ytdlp_busy {
            return;
        }
        match youtube::managed_age() {
            Some(age) if age >= YTDLP_AUTO_UPDATE_AGE => {}
            _ => return,
        }
        self.youtube.ytdlp_busy = true;
        self.refresh_ytdlp_status_label();
        sender.spawn_command(move |out| {
            let result = youtube::update_ytdlp().map_err(|e| e.to_string());
            let _ = out.send(crate::ui::app::Cmd::YtDlpAutoUpdated(result));
        });
    }

    /// Refreshes the yt-dlp status label (and button) in the open settings dialog.
    pub(crate) fn refresh_ytdlp_status_label(&self) {
        let installed = self.youtube.ytdlp_version.is_some();
        if let Some(row) = self.youtube.settings_status.borrow().as_ref() {
            let text = if self.youtube.ytdlp_busy {
                gettext("Working …")
            } else {
                match self.youtube.ytdlp_version.as_deref() {
                    Some(v) => gettext_f("Installed (version {v})", &[("v", v)]),
                    None => gettext("Not installed"),
                }
            };
            row.set_subtitle(&text);
        }
        if let Some(btn) = self.youtube.settings_dl_btn.borrow().as_ref() {
            btn.set_label(&if installed {
                gettext("Update")
            } else {
                gettext("Download")
            });
        }
    }

    /// Toggles the YouTube feature: persists the setting, shows/hides the section,
    /// and (when enabling) tells the component to load.
    pub(crate) fn set_youtube_enabled(&mut self, on: bool, _sender: &ComponentSender<Self>) {
        self.youtube.enabled = on;
        let _ = self
            .library
            .set_setting("youtube_enabled", if on { "1" } else { "0" });
        self.set_section_visible("youtube", on);
        if on {
            if youtube::available() {
                self.yt_page.emit(YtInput::Reload);
            } else {
                self.toast(&gettext("Download yt-dlp in the settings to use YouTube"));
            }
        }
    }

    // ---- add-to-library progress toast -----------------------------------

    /// Shows or updates the persistent "adding to library" progress toast.
    pub(crate) fn yt_progress(&self, msg: &str) {
        let mut slot = self.youtube.progress_toast.borrow_mut();
        match slot.as_ref() {
            Some(t) => t.set_title(msg),
            None => {
                let t = adw::Toast::new(msg);
                t.set_timeout(0);
                self.toast_overlay.add_toast(t.clone());
                *slot = Some(t);
            }
        }
    }

    /// Finishes the progress toast with a short final message.
    pub(crate) fn yt_progress_done(&self, msg: &str) {
        if let Some(t) = self.youtube.progress_toast.borrow_mut().take() {
            t.dismiss();
        }
        let t = adw::Toast::new(msg);
        t.set_timeout(3);
        self.toast_overlay.add_toast(t);
    }
}
