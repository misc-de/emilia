//! Second half of [`YtPage`]'s inherent impl: the video/playlist detail
//! dialogs, the playlist-songs subpage, the video cards with their play button
//! and watch progress, the "refresh all" worker, the library-add progress
//! popup, the add-to-library / save-playlist flows and their `on_cmd_*`
//! worker-result handlers. The struct, its messages, the `Component` impl and
//! the list/search/channel half stay in [`crate::ui::yt_page`].

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::core::db::Library;
use crate::core::youtube::{self, YtResult};
use crate::i18n::{gettext, gettext_f, ngettext_n};
use crate::ui::app::YtView;
use crate::ui::app_helpers::{cover_widget, fill_progress_row, on_long_press, on_secondary_click};
use crate::ui::widgets::{action_row, detail_box, present_detail};
use crate::ui::yt_channels::{
    duration_chip, ensure_channel_image, fmt_duration, refresh_channel_videos, WatchRow,
};
use crate::ui::yt_page::{ProgressPopup, YtCmd, YtInput, YtOutput, YtPage};

/// Upper bound of videos indexed when adding a whole playlist to the collection.
const PLAYLIST_INDEX_LIMIT: usize = 200;
/// How long a cached browsed-playlist song list is served as-is before a
/// background refresh is kicked off on the next open (6 hours).
const PLAYLIST_CACHE_TTL_SECS: i64 = 6 * 60 * 60;

impl YtPage {
    /// Rich detail page of a video (cover, info, play, add-to-library, EQ).
    pub(super) fn show_video_detail(
        &self,
        sender: &ComponentSender<Self>,
        video_id: &str,
        title: &str,
    ) {
        let Some(root) = self.window.clone() else {
            return;
        };
        let dialog = adw::Dialog::builder().title(title).build();
        self.adapt_detail_dialog(&dialog);
        let content = detail_box();

        let stored = self.library.yt_video_info(video_id).ok().flatten();
        let stored_channel = stored.as_ref().map(|(c, _, _)| c.clone());
        let stored_duration = stored.as_ref().and_then(|(_, d, _)| *d);
        let cover_path = crate::core::online::youtube_cover_path(video_id)
            .or_else(|| crate::core::online::youtube_thumb_path(&youtube::thumbnail_url(video_id)));

        let cover_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .halign(gtk::Align::Center)
            .margin_top(6)
            .margin_bottom(6)
            .build();
        let initial = cover_path
            .as_deref()
            .and_then(|p| gtk::gdk::Texture::from_filename(p).ok());
        let cover =
            crate::ui::widgets::rounded_image(initial.as_ref(), "audio-x-generic-symbolic", 200);
        cover_box.append(&cover);
        content.append(&cover_box);

        let (p_artist, p_album, p_title) = youtube::split_title(title, stored_channel.as_deref());
        let artist_from_title = p_artist.is_some();
        let info = adw::PreferencesGroup::new();
        let artist_row = adw::ActionRow::builder()
            .title(gettext("Artist"))
            .subtitle(p_artist.as_deref().unwrap_or("…"))
            .build();
        info.add(&artist_row);
        if let Some(album) = p_album.as_deref() {
            let album_row = adw::ActionRow::builder()
                .title(gettext("Album"))
                .subtitle(gtk::glib::markup_escape_text(album))
                .build();
            info.add(&album_row);
        }
        let title_row = adw::ActionRow::builder()
            .title(gettext("Title"))
            .subtitle(gtk::glib::markup_escape_text(&p_title))
            .build();
        title_row.set_subtitle_lines(3);
        info.add(&title_row);
        let duration_row = adw::ActionRow::builder()
            .title(gettext("Duration"))
            .subtitle(
                stored_duration
                    .map(fmt_duration)
                    .unwrap_or_else(|| "…".into()),
            )
            .build();
        info.add(&duration_row);
        content.append(&info);

        let actions = adw::PreferencesGroup::new();
        let play = action_row(&gettext("Play"), "media-playback-start-symbolic");
        {
            let (sender, dialog, vid, t) = (
                sender.clone(),
                dialog.clone(),
                video_id.to_string(),
                title.to_string(),
            );
            play.connect_activated(move |_| {
                let _ = sender.output(YtOutput::PlayVideo {
                    video_id: vid.clone(),
                    title: t.clone(),
                });
                dialog.close();
            });
        }
        self.ctx_video_play
            .replace(Some((play.clone(), video_id.to_string())));
        actions.add(&play);

        // Built inline (not via `action_row`) so we keep a handle on the prefix
        // icon: `refresh_yt_download_row` swaps title + icon + sensitivity to
        // reflect "Add to library" / "Adding …" / "Already in your library".
        let off = adw::ActionRow::builder()
            .title(gettext("Add to library"))
            .activatable(true)
            .build();
        let off_icon = gtk::Image::from_icon_name("list-add-symbolic");
        off.add_prefix(&off_icon);
        {
            let (sender, dialog, vid, t) = (
                sender.clone(),
                dialog.clone(),
                video_id.to_string(),
                title.to_string(),
            );
            off.connect_activated(move |_| {
                sender.input(YtInput::AddToLibrary {
                    video_id: vid.clone(),
                    title: t.clone(),
                    artist: None,
                });
                // Close the detail view; the progress popup takes over the feedback.
                dialog.close();
            });
        }
        actions.add(&off);
        let eq = action_row(
            &gettext("Equalizer settings"),
            "multimedia-equalizer-symbolic",
        );
        {
            let (sender, dialog, path, t) = (
                sender.clone(),
                dialog.clone(),
                youtube::yt_path(video_id),
                title.to_string(),
            );
            eq.connect_activated(move |_| {
                let _ = sender.output(YtOutput::OpenTrackEq {
                    path: path.clone(),
                    title: t.clone(),
                });
                dialog.close();
            });
        }
        actions.add(&eq);
        let share = action_row(&gettext("Share"), "emilia-share-symbolic");
        {
            let (sender, dialog, vid) = (sender.clone(), dialog.clone(), video_id.to_string());
            share.connect_activated(move |_| {
                let _ = sender.output(YtOutput::Share(crate::core::sync::share::Selection {
                    yt_songs: vec![vid.clone()],
                    ..Default::default()
                }));
                dialog.close();
            });
        }
        actions.add(&share);
        if self.library.is_recent(video_id).unwrap_or(false) {
            let remove = action_row(&gettext("Remove from recent"), "user-trash-symbolic");
            remove.add_css_class("error");
            let (sender, dialog, vid) = (sender.clone(), dialog.clone(), video_id.to_string());
            remove.connect_activated(move |_| {
                sender.input(YtInput::RemoveRecent(vid.clone()));
                dialog.close();
            });
            actions.add(&remove);
        }
        content.append(&actions);

        // Chapters + description (the YouTube counterpart of podcast shownotes)
        // are filled in below — right away when they are cached, otherwise when
        // the worker's `VideoMeta` arrives.
        let desc_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .build();
        content.append(&desc_box);
        self.ctx_video_desc
            .replace(Some((video_id.to_string(), title.to_string(), desc_box)));
        let cached_detail = self.library.yt_detail(video_id).ok().flatten();
        if let Some((description, chapters)) = cached_detail.as_ref() {
            self.fill_video_description(sender, video_id, description.as_deref(), chapters);
        }

        self.ctx_video_download.replace(Some((
            off.clone(),
            off_icon.clone(),
            video_id.to_string(),
        )));
        self.ctx_video_meta.replace(Some((
            video_id.to_string(),
            cover_box,
            artist_row,
            duration_row,
            artist_from_title,
        )));
        self.refresh_yt_download_row();
        self.refresh_yt_icons();

        if stored_channel.is_none()
            || stored_duration.is_none()
            || initial.is_none()
            || cached_detail.is_none()
        {
            let (sender, vid) = (sender.clone(), video_id.to_string());
            sender.spawn_command(move |out| {
                // One dump carries metadata, description and chapters; the
                // result is cached so opening the dialog again is instant.
                let details = youtube::video_details(&vid).ok();
                if let (Ok(lib), Some(d)) = (Library::open(), details.as_ref()) {
                    let _ = lib.set_yt_detail(&vid, d.description.as_deref(), &d.chapters);
                }
                let meta = details.as_ref().map(|d| &d.meta);
                let uploader = meta.and_then(|m| m.uploader.clone());
                let duration = meta.and_then(|m| m.duration);
                let cover = crate::core::online::youtube_cover_path(&vid).or_else(|| {
                    crate::core::online::cache_youtube_thumb(&youtube::thumbnail_url(&vid))
                });
                let (description, chapters) = details
                    .map(|d| (d.description, d.chapters))
                    .unwrap_or_default();
                let _ = out.send(YtCmd::VideoMeta {
                    video_id: vid,
                    uploader,
                    duration,
                    cover,
                    description,
                    chapters,
                });
            });
        }

        {
            let play_slot = self.ctx_video_play.clone();
            let dl_slot = self.ctx_video_download.clone();
            let meta_slot = self.ctx_video_meta.clone();
            let desc_slot = self.ctx_video_desc.clone();
            dialog.connect_closed(move |_| {
                *play_slot.borrow_mut() = None;
                *dl_slot.borrow_mut() = None;
                *meta_slot.borrow_mut() = None;
                *desc_slot.borrow_mut() = None;
            });
        }
        present_detail(&dialog, &content, &root);
    }

    /// Detail dialog of a playlist.
    pub(super) fn show_playlist_detail(
        &self,
        sender: &ComponentSender<Self>,
        url: &str,
        title: &str,
    ) {
        let Some(root) = self.window.clone() else {
            return;
        };
        let dialog = adw::Dialog::builder().title(title).build();
        self.adapt_detail_dialog(&dialog);
        let content = detail_box();
        let info = adw::PreferencesGroup::new();
        info.add(
            &adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(title))
                .subtitle(gettext("Playlist"))
                .build(),
        );
        content.append(&info);
        let actions = adw::PreferencesGroup::new();
        let start = action_row(&gettext("Start Playlist"), "media-playback-start-symbolic");
        {
            let (sender, dialog, u, t) = (
                sender.clone(),
                dialog.clone(),
                url.to_string(),
                title.to_string(),
            );
            start.connect_activated(move |_| {
                let _ = sender.output(YtOutput::StartPlaylist {
                    url: u.clone(),
                    title: t.clone(),
                });
                dialog.close();
            });
        }
        actions.add(&start);
        let save = action_row(&gettext("Add to Playlists"), "view-list-symbolic");
        {
            let (sender, dialog, u, t) = (
                sender.clone(),
                dialog.clone(),
                url.to_string(),
                title.to_string(),
            );
            save.connect_activated(move |_| {
                sender.input(YtInput::SavePlaylist {
                    url: u.clone(),
                    title: t.clone(),
                });
                dialog.close();
            });
        }
        actions.add(&save);
        let add = action_row(&gettext("Add to library"), "list-add-symbolic");
        {
            let (sender, dialog, u, t) = (
                sender.clone(),
                dialog.clone(),
                url.to_string(),
                title.to_string(),
            );
            add.connect_activated(move |_| {
                sender.input(YtInput::PlaylistToLibrary {
                    url: u.clone(),
                    title: t.clone(),
                });
                dialog.close();
            });
        }
        actions.add(&add);
        if self.library.is_recent(url).unwrap_or(false) {
            let remove = action_row(&gettext("Remove from recent"), "user-trash-symbolic");
            remove.add_css_class("error");
            let (sender, dialog, u) = (sender.clone(), dialog.clone(), url.to_string());
            remove.connect_activated(move |_| {
                sender.input(YtInput::RemoveRecent(u.clone()));
                dialog.close();
            });
            actions.add(&remove);
        }
        content.append(&actions);
        present_detail(&dialog, &content, &root);
    }

    /// Loads a (not locally mirrored) playlist's videos, then opens them as a
    /// song-list subpage.
    fn yt_open_playlist_songs(
        &mut self,
        sender: &ComponentSender<Self>,
        url: String,
        title: String,
    ) {
        let _ = sender.output(YtOutput::SetLoading(Some(gettext_f(
            "Loading “{title}” …",
            &[("title", &title)],
        ))));
        sender.spawn_command(move |out| {
            let result =
                youtube::list_playlist(&url, PLAYLIST_INDEX_LIMIT).map_err(|e| e.to_string());
            let _ = out.send(YtCmd::PlaylistSongs { url, title, result });
        });
    }

    /// Subpage listing a YouTube playlist's songs.
    fn show_yt_playlist_songs(
        &mut self,
        sender: &ComponentSender<Self>,
        url: &str,
        title: &str,
        videos: Vec<YtResult>,
    ) {
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        let group = adw::PreferencesGroup::builder()
            .title(
                format!(
                    "{} ({})",
                    gtk::glib::markup_escape_text(title),
                    videos.len()
                )
                .as_str(),
            )
            .build();
        if videos.is_empty() {
            group.add(
                &adw::ActionRow::builder()
                    .title(gettext("No videos"))
                    .build(),
            );
        }
        let mut pending: Vec<(String, adw::Bin)> = Vec::new();
        for (index, v) in videos.iter().enumerate() {
            let subtitle = v.duration.map(fmt_duration).unwrap_or_default();
            // Not activatable: the video plays from its play button, the detail
            // view opens on long press / right click.
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&v.title))
                .subtitle(gtk::glib::markup_escape_text(&subtitle))
                .build();
            row.add_css_class("emilia-flush");
            let thumb_url = youtube::thumbnail_url(&v.id);
            let cover = crate::core::online::youtube_cover_path(&v.id)
                .or_else(|| crate::core::online::youtube_thumb_path(&thumb_url));
            let frame = crate::ui::widgets::thumb_frame("audio-x-generic-symbolic", 48);
            match cover.as_deref().and_then(crate::ui::widgets::thumb_cached) {
                Some(tex) => crate::ui::widgets::set_cover_thumb(&frame, &tex),
                None => pending.push((thumb_url, frame.clone())),
            }
            row.add_prefix(&frame);

            let play = gtk::Button::builder()
                .icon_name("media-playback-start-symbolic")
                .valign(gtk::Align::Center)
                .tooltip_text(gettext("Play"))
                .css_classes(["flat"])
                .build();
            {
                let (sender, u, t) = (sender.clone(), url.to_string(), title.to_string());
                play.connect_clicked(move |_| {
                    sender.input(YtInput::PlayPlaylistAt {
                        url: u.clone(),
                        title: t.clone(),
                        index,
                        close: false,
                    });
                });
            }
            row.add_suffix(&play);
            on_secondary_click(&row, {
                let (sender, vid, t) = (sender.clone(), v.id.clone(), v.title.clone());
                move || {
                    sender.input(YtInput::ShowVideoDetail {
                        video_id: vid.clone(),
                        title: t.clone(),
                    });
                }
            });
            on_long_press(&row, {
                let (sender, vid, t) = (sender.clone(), v.id.clone(), v.title.clone());
                move || {
                    sender.input(YtInput::ShowVideoDetail {
                        video_id: vid.clone(),
                        title: t.clone(),
                    })
                }
            });
            group.add(&row);
        }
        content.append(&group);
        if let Some(first) = videos.first() {
            let _ = self
                .library
                .set_recent_thumb(url, &youtube::thumbnail_url(&first.id));
        }
        self.push_subpage(
            sender,
            gettext_f("Playlist – {title}", &[("title", title)]),
            content,
        );

        self.pl_cover_slots = pending;
        if !self.pl_cover_slots.is_empty() {
            let urls: Vec<String> = self.pl_cover_slots.iter().map(|(u, _)| u.clone()).collect();
            sender.spawn_command(move |out| {
                let threads = 8.min(urls.len().max(1));
                let chunk = (urls.len() / threads).max(1);
                std::thread::scope(|s| {
                    for part in urls.chunks(chunk) {
                        s.spawn(move || {
                            for u in part {
                                let _ = crate::core::online::cache_youtube_thumb(u);
                            }
                        });
                    }
                });
                let _ = out.send(YtCmd::PlaylistCoversReady);
            });
        }
    }

    /// Play/Pause button (suffix) for a video row.
    fn video_play_button(
        &self,
        sender: &ComponentSender<Self>,
        video_id: &str,
        title: &str,
    ) -> gtk::Button {
        let active = self.playing_video_id.as_deref() == Some(video_id);
        let btn = crate::ui::play_mark::button(&gettext("Play/Pause"), active, self.playing);
        {
            let (sender, vid, t) = (sender.clone(), video_id.to_string(), title.to_string());
            btn.connect_clicked(move |_| {
                let _ = sender.output(YtOutput::PlayVideo {
                    video_id: vid.clone(),
                    title: t.clone(),
                });
            });
        }
        self.video_marks.add(video_id.to_string(), &btn);
        btn
    }

    /// A video row built like the podcast lists: cover, title and subtitle in a
    /// text column with the progress line underneath (long-form items only),
    /// then the runtime and the play button. A `AdwActionRow` has no room under
    /// its subtitle, so the row is a plain box — the progress line then reads
    /// exactly like the one in "Newest"/"Recently" for podcasts. The box is
    /// wrapped in a list-box row and laid out like an `emilia-flush`
    /// `AdwActionRow` (cover flush left, 8 px to the text, plain title weight),
    /// so it looks the same as the streaming/album rows next to it.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn video_card(
        &self,
        sender: &ComponentSender<Self>,
        video_id: &str,
        title: &str,
        subtitle: &str,
        cover: Option<&str>,
        duration: Option<i64>,
        watched: Option<&(i64, bool)>,
        on_detail: impl Fn() + Clone + 'static,
    ) -> gtk::ListBoxRow {
        let card = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        let top = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .margin_top(3)
            .margin_bottom(3)
            .margin_start(3)
            .margin_end(12)
            .build();
        top.append(&cover_widget(cover, "audio-x-generic-symbolic"));
        let text = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .hexpand(true)
            .valign(gtk::Align::Center)
            .build();
        let title_lbl = gtk::Label::builder()
            .label(title)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        text.append(&title_lbl);
        if !subtitle.trim().is_empty() {
            let sub = gtk::Label::builder()
                .label(subtitle)
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .build();
            sub.add_css_class("dim-label");
            text.append(&sub);
        }
        if let Some(prow) = self.watch_progress_widget(video_id, duration, watched) {
            text.append(&prow);
        }
        top.append(&text);
        if let Some(d) = duration.filter(|d| *d > 0) {
            top.append(&duration_chip(d));
        }
        top.append(&self.video_play_button(sender, video_id, title));
        card.append(&top);
        // A tap on the card deliberately does nothing: playback is the play
        // button's job, so a stray tap while scrolling a feed cannot start a
        // video. Same rule as the playlist track rows. The detail view is on
        // long press / right click below.
        on_secondary_click(&card, on_detail.clone());
        on_long_press(&card, on_detail);
        crate::ui::app_helpers::card_row(&card)
    }

    /// Watch-progress suffix for a row, for **long-form** items only (talks,
    /// streams, podcasts — a song keeps its plain row). Returns `None` for
    /// anything shorter than [`youtube::LONGFORM_SECS`]; otherwise a widget that
    /// is registered for the live tick and starts out hidden until there is
    /// something to show.
    fn watch_progress_widget(
        &self,
        video_id: &str,
        total_secs: Option<i64>,
        progress: Option<&(i64, bool)>,
    ) -> Option<gtk::Box> {
        if !youtube::is_longform(total_secs) {
            return None;
        }
        let row = crate::ui::app_helpers::progress_row_box();
        let (position_ms, finished) = progress.copied().unwrap_or((0, false));
        fill_progress_row(&row, position_ms, total_secs, finished);
        self.watch_progress_rows.borrow_mut().push(WatchRow {
            video_id: video_id.to_string(),
            row: row.clone(),
            total_secs,
        });
        Some(row)
    }

    /// Refreshes the watch-progress widgets of every visible row of `video_id`
    /// (driven by the transport tick). Rows that left the widget tree are
    /// dropped along the way.
    pub(super) fn apply_watch_progress(
        &self,
        video_id: &str,
        position_ms: i64,
        duration_ms: i64,
        finished: bool,
    ) {
        let mut rows = self.watch_progress_rows.borrow_mut();
        rows.retain(|r| r.row.root().is_some());
        for entry in rows.iter().filter(|r| r.video_id == video_id) {
            let total = entry
                .total_secs
                .or_else(|| (duration_ms > 0).then_some(duration_ms / 1000));
            fill_progress_row(&entry.row, position_ms, total, finished);
        }
    }

    /// Updates the Play/Pause icons of visible video rows and the detail "Play"
    /// row from the mirrored playback state.
    pub(super) fn refresh_yt_icons(&self) {
        let active = self.playing_video_id.clone();
        let playing = self.playing;
        let is_active = |vid: &str| playing && active.as_deref() == Some(vid);
        self.video_marks
            .apply_all(playing, |vid| active.as_deref() == Some(vid));
        if let Some((row, vid)) = self.ctx_video_play.borrow().as_ref() {
            row.set_visible(!is_active(vid));
        }
    }

    /// Updates the "Add to library" row of an open video detail dialog to reflect
    /// the current state: already offline (greyed out with a checkmark, not
    /// addable again), downloading, or addable.
    pub(super) fn refresh_yt_download_row(&self) {
        let guard = self.ctx_video_download.borrow();
        let Some((row, icon, vid)) = guard.as_ref() else {
            return;
        };
        let is_local = self
            .library
            .yt_download(vid)
            .ok()
            .flatten()
            .map(|p| std::path::Path::new(&p).exists())
            .unwrap_or(false);
        if is_local {
            row.set_title(&gettext("Already in your library"));
            icon.set_icon_name(Some("object-select-symbolic"));
            row.set_sensitive(false);
        } else if self.downloading_videos.contains(vid) {
            row.set_title(&gettext("Adding to library …"));
            icon.set_icon_name(Some("list-add-symbolic"));
            row.set_sensitive(false);
        } else {
            row.set_title(&gettext("Add to library"));
            icon.set_icon_name(Some("list-add-symbolic"));
            row.set_sensitive(true);
        }
    }

    /// "Refresh all" from the header button: re-fetch the newest videos of every
    /// subscribed channel, one after another, reporting progress so the loading
    /// overlay shows a bar with the channel being fetched. The channel list comes
    /// from the DB rather than the (possibly not-yet-built) view state, and the
    /// cases that used to end in silence — no subscriptions, no network, no
    /// yt-dlp — now say what happened.
    pub(super) fn refresh_all_channels(&mut self, sender: &ComponentSender<Self>) {
        let channels = self.library.channels().unwrap_or_default();
        if channels.is_empty() {
            let _ = sender.output(YtOutput::RefreshSummary(gettext("No subscriptions")));
            return;
        }
        if !crate::ui::app_helpers::online_available() {
            let _ = sender.output(YtOutput::RefreshSummary(gettext("No internet connection")));
            return;
        }
        let total = channels.len();
        let _ = sender.output(YtOutput::RefreshStarted(true));
        let _ = sender.output(YtOutput::RefreshProgress {
            done: 0,
            total,
            label: channels[0].1.clone(),
        });
        sender.spawn_command(move |out| {
            if !youtube::available() {
                let _ = out.send(YtCmd::RefreshUnavailable);
                return;
            }
            let (mut updated, mut failed, mut new_videos) = (0usize, 0usize, 0usize);
            for (i, (id, title, url, thumb, _)) in channels.iter().enumerate() {
                let _ = out.send(YtCmd::RefreshProgress {
                    done: i,
                    total,
                    title: title.clone(),
                });
                if let Ok(lib) = Library::open() {
                    ensure_channel_image(&lib, *id, title, thumb.as_deref());
                }
                match refresh_channel_videos(*id, title, url) {
                    Some((_, fresh)) => {
                        updated += 1;
                        new_videos += fresh;
                    }
                    None => {
                        tracing::warn!("YouTube refresh returned no videos for {url}");
                        failed += 1;
                    }
                }
            }
            let _ = out.send(YtCmd::ChannelsRefreshed {
                updated,
                failed,
                new_videos,
            });
        });
    }

    /// Opens (replacing any earlier one) the non-blocking progress popup for a
    /// library add and parks its widgets so [`Self::update_progress_popup`] can
    /// drive them. The user may dismiss it; the download keeps running and the
    /// finishing command still lands (and its success toast still shows).
    fn show_progress_popup(&self, video_id: &str, title: &str) {
        let Some(root) = self.window.clone() else {
            return;
        };
        if let Some(prev) = self.progress_popup.borrow_mut().take() {
            prev.dialog.close();
        }
        let dialog = adw::Dialog::builder().title(title).build();
        self.adapt_detail_dialog(&dialog);
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .margin_top(24)
            .margin_bottom(24)
            .margin_start(24)
            .margin_end(24)
            .build();
        let label = gtk::Label::builder()
            .label(gettext("Preparing …"))
            .wrap(true)
            .xalign(0.0)
            .build();
        let bar = gtk::ProgressBar::builder().fraction(0.0).build();
        content.append(&label);
        content.append(&bar);
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&content));
        dialog.set_child(Some(&toolbar));
        dialog.set_content_width(360);
        dialog.present(Some(&root));
        *self.progress_popup.borrow_mut() = Some(ProgressPopup {
            dialog,
            bar,
            label,
            video_id: video_id.to_string(),
        });
    }

    /// Reflects an async [`youtube::AddProgress`] in the open progress popup, if
    /// it is still the one tracking `video_id`.
    pub(super) fn update_progress_popup(&self, video_id: &str, progress: youtube::AddProgress) {
        let guard = self.progress_popup.borrow();
        let Some(popup) = guard.as_ref() else {
            return;
        };
        if popup.video_id != video_id {
            return;
        }
        match progress {
            youtube::AddProgress::Preparing => {
                popup.label.set_label(&gettext("Preparing …"));
                popup.bar.set_fraction(0.0);
            }
            youtube::AddProgress::Downloading(pct) => {
                popup.label.set_label(&gettext_f(
                    "Downloading … {pct}%",
                    &[("pct", &pct.to_string())],
                ));
                popup.bar.set_fraction(pct as f64 / 100.0);
            }
            youtube::AddProgress::Processing => {
                popup.label.set_label(&gettext("Converting …"));
                popup.bar.set_fraction(1.0);
            }
        }
    }

    /// Closes the progress popup when it tracks `video_id` (or unconditionally
    /// when `video_id` is `None`). A no-op if none is open.
    pub(super) fn close_progress_popup(&self, video_id: Option<&str>) {
        let mut guard = self.progress_popup.borrow_mut();
        let close = match (guard.as_ref(), video_id) {
            (Some(_), None) => true,
            (Some(popup), Some(v)) => popup.video_id == v,
            (None, _) => false,
        };
        if close {
            if let Some(popup) = guard.take() {
                popup.dialog.close();
            }
        }
    }

    /// Fills an open video detail dialog with metadata that arrived async.
    /// Fills the open video dialog's chapters + description. Chapters become
    /// tappable rows (they start playback at that mark), and the description is
    /// shown with its timestamps linkified — the same treatment podcast
    /// shownotes get, so both media behave alike. A no-op when the dialog was
    /// closed meanwhile or shows a different video.
    pub(super) fn fill_video_description(
        &self,
        sender: &ComponentSender<Self>,
        video_id: &str,
        description: Option<&str>,
        chapters: &[(i64, String)],
    ) {
        let guard = self.ctx_video_desc.borrow();
        let Some((vid, title, container)) = guard.as_ref() else {
            return;
        };
        if vid != video_id {
            return;
        }
        while let Some(child) = container.first_child() {
            container.remove(&child);
        }
        let description = description.map(str::trim).filter(|d| !d.is_empty());
        if chapters.is_empty() && description.is_none() {
            return;
        }

        // Chapters: one row per mark, tapping it plays from there.
        if !chapters.is_empty() {
            let group = adw::PreferencesGroup::new();
            let expander = adw::ExpanderRow::builder()
                .title(gettext("Chapters"))
                .subtitle(ngettext_n(
                    "{n} chapter",
                    "{n} chapters",
                    chapters.len() as u32,
                ))
                .expanded(false)
                .build();
            for (ms, label) in chapters {
                let row = adw::ActionRow::builder()
                    .title(gtk::glib::markup_escape_text(label))
                    .subtitle(crate::ui::app_helpers::fmt_duration(*ms))
                    .activatable(true)
                    .build();
                let (sender, vid, t, ms) =
                    (sender.clone(), video_id.to_string(), title.clone(), *ms);
                row.connect_activated(move |_| {
                    let _ = sender.output(YtOutput::PlayVideoAt {
                        video_id: vid.clone(),
                        title: t.clone(),
                        ms,
                    });
                });
                expander.add_row(&row);
            }
            group.add(&expander);
            container.append(&group);
        }

        // Description: timestamps in the text stay tappable as well.
        if let Some(text) = description {
            let group = adw::PreferencesGroup::new();
            let label = gtk::Label::builder()
                .label(crate::core::podcast::linkify_timestamps(text))
                .use_markup(true)
                .wrap(true)
                // Wrap inside long unbreakable tokens (URLs) too, so a
                // description can never force the dialog wider than the screen.
                .wrap_mode(gtk::pango::WrapMode::WordChar)
                .xalign(0.0)
                .selectable(true)
                .build();
            label.add_css_class("body");
            {
                let (sender, vid, t) = (sender.clone(), video_id.to_string(), title.clone());
                label.connect_activate_link(move |_, uri| {
                    if let Some(ms) = uri
                        .strip_prefix("emilia-seek:")
                        .and_then(|s| s.parse::<i64>().ok())
                    {
                        let _ = sender.output(YtOutput::PlayVideoAt {
                            video_id: vid.clone(),
                            title: t.clone(),
                            ms,
                        });
                        return gtk::glib::Propagation::Stop;
                    }
                    gtk::glib::Propagation::Proceed
                });
            }
            let wrap = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(6)
                .margin_top(10)
                .margin_bottom(10)
                .margin_start(14)
                .margin_end(14)
                .build();
            wrap.append(&label);
            let expander = adw::ExpanderRow::builder()
                .title(gettext("Description"))
                .expanded(false)
                .build();
            expander.add_row(&wrap);
            group.add(&expander);
            container.append(&group);
        }
    }

    pub(super) fn apply_video_meta(
        &self,
        video_id: &str,
        uploader: Option<String>,
        duration: Option<i64>,
        cover: Option<String>,
    ) {
        let guard = self.ctx_video_meta.borrow();
        let Some((vid, cover_box, artist_row, duration_row, artist_from_title)) = guard.as_ref()
        else {
            return;
        };
        if vid != video_id {
            return;
        }
        if !*artist_from_title {
            let artist = uploader
                .as_deref()
                .map(youtube::clean_channel_name)
                .filter(|s| !s.trim().is_empty());
            artist_row.set_subtitle(artist.as_deref().unwrap_or("—"));
        }
        duration_row.set_subtitle(&duration.map(fmt_duration).unwrap_or_else(|| "—".into()));
        if let Some(tex) = cover
            .as_deref()
            .and_then(|p| gtk::gdk::Texture::from_filename(p).ok())
        {
            while let Some(ch) = cover_box.first_child() {
                cover_box.remove(&ch);
            }
            cover_box.append(&crate::ui::widgets::rounded_image(
                Some(&tex),
                "audio-x-generic-symbolic",
                200,
            ));
        }
    }

    /// "+" on a search result: list the video in "Recent" (no download/playback).
    pub(super) fn yt_add_recent(
        &mut self,
        sender: &ComponentSender<Self>,
        video_id: String,
        title: String,
    ) {
        let _ = self.library.add_recent_video(&video_id, &title, None);
        let _ = self.library.set_yt_title(&video_id, &title);
        self.reload_yt_recent(sender);
        self.yt_view = YtView::Recent;
        let vid = video_id;
        sender.spawn_command(move |out| {
            let cover = crate::core::online::cache_youtube_thumb(&youtube::thumbnail_url(&vid));
            let _ = out.send(YtCmd::RecentEnriched {
                video_id: vid,
                cover,
            });
        });
    }

    /// Adds a single video to the on-disk music library (background).
    pub(super) fn yt_add_video_to_library(
        &mut self,
        sender: &ComponentSender<Self>,
        video_id: String,
        title: String,
        artist: Option<String>,
        overwrite: bool,
    ) {
        if self.downloading_videos.contains(&video_id) {
            return;
        }
        let Some(music) = self.library.get_setting("music_dir").ok().flatten() else {
            let _ = sender.output(YtOutput::Toast(gettext(
                "Set a music folder in settings first",
            )));
            return;
        };
        self.downloading_videos.insert(video_id.clone());
        self.refresh_yt_download_row();
        self.show_progress_popup(&video_id, &title);
        let cover = crate::core::online::youtube_cover_path(&video_id);
        let vid = video_id;
        sender.spawn_command(move |out| {
            let progress_out = out.clone();
            let progress_vid = vid.clone();
            let on_progress = move |progress| {
                let _ = progress_out.send(YtCmd::AddLibProgress {
                    video_id: progress_vid.clone(),
                    progress,
                });
            };
            let cmd = match youtube::add_to_library_progress(
                &vid,
                &title,
                artist.as_deref(),
                &music,
                cover.as_deref(),
                overwrite,
                on_progress,
            ) {
                Ok(youtube::AddOutcome::Added) => YtCmd::LibraryAdded {
                    video_id: Some(vid),
                    result: Ok(1),
                },
                Ok(youtube::AddOutcome::Exists(dest)) => YtCmd::LibraryExists {
                    video_id: vid,
                    title,
                    artist,
                    dest: dest.to_string_lossy().into_owned(),
                },
                Err(e) => YtCmd::LibraryAdded {
                    video_id: Some(vid),
                    result: Err(e),
                },
            };
            let _ = out.send(cmd);
        });
    }

    /// Adds all videos of a playlist to the on-disk music library (background).
    pub(super) fn yt_playlist_to_library(
        &self,
        sender: &ComponentSender<Self>,
        url: String,
        title: String,
    ) {
        let Some(music) = self.library.get_setting("music_dir").ok().flatten() else {
            let _ = sender.output(YtOutput::Toast(gettext(
                "Set a music folder in settings first",
            )));
            return;
        };
        let _ = sender.output(YtOutput::Progress(gettext_f(
            "Adding playlist “{title}” to library …",
            &[("title", &title)],
        )));
        sender.spawn_command(move |out| {
            let r = (|| -> Result<usize, String> {
                let videos = youtube::list_playlist(&url, PLAYLIST_INDEX_LIMIT)
                    .map_err(|e| e.to_string())?;
                let total = videos.len();
                let mut n = 0;
                let _ = out.send(YtCmd::LibraryProgress { done: 0, total });
                for (i, v) in videos.into_iter().enumerate() {
                    let cover = crate::core::online::youtube_cover_path(&v.id);
                    if let Ok(youtube::AddOutcome::Added) = youtube::add_to_library(
                        &v.id,
                        &v.title,
                        None,
                        &music,
                        cover.as_deref(),
                        false,
                    ) {
                        n += 1;
                    }
                    let _ = out.send(YtCmd::LibraryProgress { done: i + 1, total });
                }
                Ok(n)
            })();
            let _ = out.send(YtCmd::LibraryAdded {
                video_id: None,
                result: r,
            });
        });
    }

    /// Saves a found playlist into the Playlists section (background).
    pub(super) fn yt_save_playlist(
        &self,
        sender: &ComponentSender<Self>,
        url: String,
        title: String,
    ) {
        let _ = sender.output(YtOutput::Progress(gettext_f(
            "Saving “{title}” to Playlists …",
            &[("title", &title)],
        )));
        sender.spawn_command(move |out| {
            let r = (|| -> Result<usize, String> {
                let videos = youtube::list_playlist(&url, PLAYLIST_INDEX_LIMIT)
                    .map_err(|e| e.to_string())?;
                let lib = Library::open().map_err(|e| e.to_string())?;
                let mut paths = Vec::with_capacity(videos.len());
                for v in &videos {
                    let _ = lib.set_yt_meta(&v.id, &v.title, v.duration);
                    paths.push(youtube::yt_path(&v.id));
                }
                lib.replace_yt_playlist(&url, &title, &paths)
                    .map_err(|e| e.to_string())?;
                Ok(paths.len())
            })();
            let _ = out.send(YtCmd::PlaylistSaved(r));
        });
    }

    /// Open a recent playlist's song list:
    /// saved DB mirror → session cache → **persistent DB cache** → fetch.
    /// Serving from the DB cache is instant (no YouTube round-trip); if that
    /// cache is stale it is refreshed in the background for the next open.
    pub(super) fn yt_open_recent_playlist(
        &mut self,
        sender: &ComponentSender<Self>,
        url: String,
        title: String,
    ) {
        // A "saved" playlist (Add to Playlists) opens its local mirror directly.
        if let Ok(Some(id)) = self.library.yt_playlist_id(&url) {
            let _ = sender.output(YtOutput::OpenPlaylist { id, name: title });
            return;
        }
        // Already fetched this session → show immediately.
        if let Some(videos) = self.playlist_songs_cache.get(&url).cloned() {
            self.show_yt_playlist_songs(sender, &url, &title, videos);
            return;
        }
        // Persisted from an earlier session → show instantly from the DB cache,
        // and refresh in the background if it has gone stale.
        if let Ok(Some((json, fetched_at))) = self.library.yt_playlist_cache(&url) {
            if let Ok(videos) = serde_json::from_str::<Vec<YtResult>>(&json) {
                self.playlist_songs_cache
                    .insert(url.clone(), videos.clone());
                self.show_yt_playlist_songs(sender, &url, &title, videos);
                if crate::ui::app_helpers::unix_now().saturating_sub(fetched_at)
                    > PLAYLIST_CACHE_TTL_SECS
                {
                    let (url, title) = (url.clone(), title.clone());
                    sender.spawn_command(move |out| {
                        let result = youtube::list_playlist(&url, PLAYLIST_INDEX_LIMIT)
                            .map_err(|e| e.to_string());
                        let _ = out.send(YtCmd::PlaylistCacheRefreshed { url, title, result });
                    });
                }
                return;
            }
        }
        // Never seen → fetch (the result is cached on arrival).
        self.yt_open_playlist_songs(sender, url, title);
    }

    /// Serializes a playlist's song list into the persistent DB cache (best
    /// effort: a serialization/DB error just skips the cache, never blocks).
    fn cache_playlist_songs(&self, url: &str, title: &str, videos: &[YtResult]) {
        if let Ok(json) = serde_json::to_string(videos) {
            if let Err(e) = self.library.set_yt_playlist_cache(url, title, &json) {
                tracing::warn!("caching playlist {url} failed: {e}");
            }
        }
    }

    /// Worker result: a library-add hit an existing file → ask before overwriting.
    pub(super) fn on_cmd_yt_library_exists(
        &mut self,
        sender: &ComponentSender<Self>,
        video_id: String,
        title: String,
        artist: Option<String>,
        dest: String,
    ) {
        self.close_progress_popup(Some(&video_id));
        self.downloading_videos.remove(&video_id);
        self.refresh_yt_download_row();
        let _ = sender.output(YtOutput::ProgressDone(gettext("Song already exists")));
        let Some(root) = self.window.clone() else {
            return;
        };
        let confirm = adw::AlertDialog::new(
            Some(&gettext("Overwrite existing song?")),
            Some(&gettext_f(
                "“{title}” is already saved at:\n{dest}",
                &[("title", &title), ("dest", &dest)],
            )),
        );
        confirm.add_response("skip", &gettext("Skip"));
        confirm.add_response("overwrite", &gettext("Overwrite"));
        confirm.set_response_appearance("overwrite", adw::ResponseAppearance::Destructive);
        confirm.set_default_response(Some("skip"));
        confirm.set_close_response("skip");
        {
            let sender = sender.clone();
            confirm.connect_response(None, move |_, resp| {
                if resp == "overwrite" {
                    sender.input(YtInput::AddToLibraryConfirmed {
                        video_id: video_id.clone(),
                        title: title.clone(),
                        artist: artist.clone(),
                    });
                }
            });
        }
        confirm.present(Some(&root));
    }

    /// Worker result: a playlist's song list resolved → cache + show subpage.
    pub(super) fn on_cmd_yt_playlist_songs(
        &mut self,
        sender: &ComponentSender<Self>,
        url: String,
        title: String,
        result: Result<Vec<YtResult>, String>,
    ) {
        let _ = sender.output(YtOutput::SetLoading(None));
        match result {
            Ok(videos) => {
                self.cache_playlist_songs(&url, &title, &videos);
                self.playlist_songs_cache
                    .insert(url.clone(), videos.clone());
                self.show_yt_playlist_songs(sender, &url, &title, videos);
            }
            Err(e) => {
                tracing::warn!("yt playlist load failed: {e}");
                let _ = sender.output(YtOutput::Toast(gettext("Could not load playlist")));
            }
        }
    }

    /// Worker result: a stale cached playlist's background refresh finished →
    /// update the persistent + session caches silently (no UI change; the fresh
    /// list shows on the next open).
    pub(super) fn on_cmd_yt_playlist_cache_refreshed(
        &mut self,
        url: String,
        title: String,
        result: Result<Vec<YtResult>, String>,
    ) {
        match result {
            Ok(videos) => {
                self.cache_playlist_songs(&url, &title, &videos);
                self.playlist_songs_cache.insert(url, videos);
            }
            Err(e) => tracing::warn!("yt playlist background refresh failed: {e}"),
        }
    }

    /// Worker result: pending playlist-songs cover thumbnails finished caching.
    pub(super) fn on_cmd_yt_playlist_covers_ready(&mut self) {
        self.pl_cover_slots.retain(|(thumb_url, frame)| {
            if frame.root().is_none() {
                return false;
            }
            match crate::core::online::youtube_thumb_path(thumb_url)
                .as_deref()
                .and_then(crate::ui::widgets::thumb_cached)
            {
                Some(tex) => {
                    crate::ui::widgets::set_cover_thumb(frame, &tex);
                    false
                }
                None => true,
            }
        });
    }
}
