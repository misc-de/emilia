//! Playlists: overview list, track subpage and the dialogs for creating
//! or adding. Entries are paths (like the queue).

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::i18n::{gettext, gettext_f, ngettext_n};
use crate::ui::app::{App, Msg};

/// Content fingerprint of a cover file (length + first 64 KB), to de-duplicate
/// visually identical covers that live under different paths (e.g. per-track
/// embedded-art caches). Bounds the read so large folder images stay cheap.
/// `None` if the file cannot be read.
fn cover_content_hash(path: &str) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    use std::io::Read;
    let f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let mut head = Vec::new();
    f.take(64 * 1024).read_to_end(&mut head).ok()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    len.hash(&mut hasher);
    head.hash(&mut hasher);
    Some(hasher.finish())
}

impl App {
    /// Rebuilds the playlist list (name, track count, play, delete).
    pub(crate) fn reload_playlists(&mut self, sender: &ComponentSender<Self>) {
        self.playlists.playlist_items = self.library.playlists().unwrap_or_default();
        let durations = self.library.playlist_durations_ms().unwrap_or_default();

        while let Some(child) = self.playlists.playlists_list.first_child() {
            self.playlists.playlists_list.remove(&child);
        }
        for (id, name, count) in self.playlists.playlist_items.clone() {
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&name))
                .subtitle(ngettext_n("{n} track", "{n} tracks", count as u32))
                .activatable(true)
                .build();
            // Cover flush to the far left.
            row.add_css_class("emilia-flush");
            // Cover derived from the songs (chosen or first available), else the
            // generic playlist icon.
            let paths = self.library.playlist_paths(id).unwrap_or_default();
            let cover = self.playlist_display_cover(id, &paths);
            row.add_prefix(&crate::ui::app::cover_widget(
                cover.as_deref(),
                "view-list-symbolic",
            ));
            // Total runtime, then a play button on the far right (plays the whole
            // playlist). A normal tap on the row still opens it; long press →
            // detail view.
            let total_ms = durations.get(&id).copied().unwrap_or(0);
            if total_ms > 0 {
                row.add_suffix(&crate::ui::app::duration_label(total_ms));
            }
            let play_btn = gtk::Button::builder()
                .icon_name("media-playback-start-symbolic")
                .tooltip_text(gettext("Play playlist"))
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .build();
            {
                let sender = sender.clone();
                play_btn.connect_clicked(move |_| sender.input(Msg::PlayPlaylist(id)));
            }
            row.add_suffix(&play_btn);

            {
                let sender = sender.clone();
                row.connect_activated(move |_| sender.input(Msg::OpenPlaylist(id)));
            }
            // Long press (touch) / right click (mouse): detail view (cover + actions).
            crate::ui::app::on_secondary_click(&row, {
                let sender = sender.clone();
                move || sender.input(Msg::ShowPlaylistDetail(id))
            });
            let long_press = gtk::GestureLongPress::new();
            {
                let sender = sender.clone();
                long_press.connect_pressed(move |g, _, _| {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    sender.input(Msg::ShowPlaylistDetail(id));
                });
            }
            row.add_controller(long_press);
            self.playlists.playlists_list.append(&row);
        }
    }

    /// Short tap on a playlist: a subpage that lists the playlist's
    /// **albums** (2+ tracks of the same album, expandable) and then the
    /// standalone **songs**. Tapping a track plays the playlist from there.
    pub(crate) fn open_playlist(&self, sender: &ComponentSender<Self>, id: i64, name: &str) {
        // Stream recordings whose song now also exists locally are repointed to
        // the local file (keeps the recording), so the playlist plays the better
        // copy. No-op once everything is already local.
        let _ = self.library.relink_recordings_in_playlist(id);
        let paths = self.library.playlist_paths(id).unwrap_or_default();
        let (albums, singles) = self.playlist_sections(&paths);

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();

        if paths.is_empty() {
            content.append(
                &adw::StatusPage::builder()
                    .icon_name("view-list-symbolic")
                    .title(gettext("No tracks yet"))
                    .description(gettext(
                        "Add tracks via the options of a song, album or artist.",
                    ))
                    .build(),
            );
        }

        // --- Albums first (like the artist view) ---
        if !albums.is_empty() {
            let group = adw::PreferencesGroup::builder()
                .title(format!("{} ({})", gettext("Albums"), albums.len()))
                .build();
            for (album, display_artist, tracks) in &albums {
                let album_meta = self
                    .library
                    .get_album_meta(display_artist, album)
                    .ok()
                    .flatten();
                let year = album_meta.as_ref().and_then(|m| m.year);
                let cover_path = album_meta
                    .as_ref()
                    .and_then(|m| m.cover_path.clone())
                    .or_else(|| self.album_cover_for(display_artist, album));

                let exp = adw::ExpanderRow::builder()
                    .title(gtk::glib::markup_escape_text(album))
                    .subtitle(crate::ui::app::album_subtitle(year, tracks.len()))
                    .build();
                exp.add_prefix(&crate::ui::app::cover_widget(
                    cover_path.as_deref(),
                    "media-optical-symbolic",
                ));
                // Play button: start the playlist at this album's first track.
                if let Some(first) = tracks.first() {
                    let play = gtk::Button::builder()
                        .icon_name("media-playback-start-symbolic")
                        .tooltip_text(gettext("Play"))
                        .valign(gtk::Align::Center)
                        .css_classes(["flat"])
                        .build();
                    let sender = sender.clone();
                    let path = first.path.clone();
                    play.connect_clicked(move |_| {
                        sender.input(Msg::PlayOneTrack {
                            path: path.clone(),
                            close: false,
                        });
                    });
                    exp.add_suffix(&play);
                }
                for t in tracks {
                    exp.add_row(&self.playlist_track_row(
                        sender,
                        &t.path,
                        "audio-x-generic-symbolic",
                    ));
                }
                group.add(&exp);
            }
            content.append(&group);
        }

        // --- Standalone songs ---
        if !singles.is_empty() {
            let group = adw::PreferencesGroup::builder()
                .title(format!("{} ({})", gettext("Songs"), singles.len()))
                .build();
            for t in &singles {
                group.add(&self.playlist_track_row(sender, &t.path, "audio-x-generic-symbolic"));
            }
            content.append(&group);
        }

        self.push_subpage(&gettext_f("Playlist – {name}", &[("name", name)]), &content);
    }

    /// Cover for a playlist track row: the YouTube thumbnail/cover for `yt:`
    /// items, otherwise the embedded track cover or its album cover.
    fn playlist_track_cover(&self, path: &str) -> Option<String> {
        if let Some(vid) = crate::core::youtube::parse_yt_path(path) {
            return crate::core::online::youtube_cover_path(&vid).or_else(|| {
                crate::core::online::youtube_thumb_path(&crate::core::youtube::thumbnail_url(&vid))
            });
        }
        if let Some(c) = crate::core::online::local_track_cover(path) {
            return Some(c);
        }
        let t = self.library.track_by_path(path).ok().flatten()?;
        let (artist, album) = (t.artist?, t.album?);
        self.library
            .get_album_meta(&artist, &album)
            .ok()
            .flatten()
            .and_then(|m| m.cover_path)
    }

    /// Distinct cover paths of a playlist's songs – embedded tag, album cover or
    /// YouTube thumbnail, whatever is available – in playlist order, only
    /// existing files. De-duplicated **by image content**, not just by path:
    /// per-track embedded covers are cached under one path per track, so several
    /// songs of the same album would otherwise yield the identical picture many
    /// times. Bounded (scan + result count) so a huge playlist does not read
    /// hundreds of files.
    fn playlist_cover_candidates(&self, paths: &[String]) -> Vec<String> {
        use std::collections::HashSet;
        let mut seen_paths: HashSet<String> = HashSet::new();
        let mut seen_content: HashSet<u64> = HashSet::new();
        let mut out: Vec<String> = Vec::new();
        for p in paths.iter().take(120) {
            if out.len() >= 24 {
                break;
            }
            let Some(c) = self.playlist_track_cover(p) else {
                continue;
            };
            if !seen_paths.insert(c.clone()) || !std::path::Path::new(&c).exists() {
                continue;
            }
            // Skip a cover whose bytes we have already seen under another path.
            if let Some(h) = cover_content_hash(&c) {
                if !seen_content.insert(h) {
                    continue;
                }
            }
            out.push(c);
        }
        out
    }

    /// The cover to show for a playlist: the user's chosen one (if it still
    /// exists), otherwise the first song cover available – `None` if the songs
    /// carry no covers at all.
    pub(crate) fn playlist_display_cover(&self, id: i64, paths: &[String]) -> Option<String> {
        if let Some(c) = self.library.playlist_cover(id).ok().flatten() {
            if std::path::Path::new(&c).exists() {
                return Some(c);
            }
        }
        paths.iter().find_map(|p| {
            self.playlist_track_cover(p)
                .filter(|c| std::path::Path::new(c).exists())
        })
    }

    /// Human label of a playlist entry's source ("Source: YouTube", "… Files" …),
    /// derived from the path scheme: `yt:` → YouTube, `nc:` → Nextcloud, an
    /// http(s) URL → a podcast episode or otherwise a stream, else a local file.
    fn playlist_source_label(&self, path: &str) -> String {
        let source = if crate::core::youtube::parse_yt_path(path).is_some() {
            "YouTube"
        } else if crate::core::webdav::parse_nc_path(path).is_some() {
            "Nextcloud"
        } else if path.starts_with("http://") || path.starts_with("https://") {
            if self.library.is_podcast_episode(path).unwrap_or(false) {
                "Podcasts"
            } else {
                "Streaming"
            }
        } else {
            "Files"
        };
        gettext_f("Source: {source}", &[("source", source)])
    }

    /// A single track row inside a playlist subpage: tap plays this track; a
    /// long press opens the song's detail view (like the album/artist lists).
    fn playlist_track_row(
        &self,
        sender: &ComponentSender<Self>,
        path: &str,
        icon: &str,
    ) -> adw::ActionRow {
        let display = self.display_name(std::path::Path::new(path));
        let row = adw::ActionRow::builder()
            .title(gtk::glib::markup_escape_text(&display))
            .subtitle(self.playlist_source_label(path))
            .activatable(true)
            .build();
        // Cover flush to the far left (like the album/artist track lists).
        row.add_css_class("emilia-flush");
        let cover = self.playlist_track_cover(path);
        row.add_prefix(&crate::ui::app::cover_widget(cover.as_deref(), icon));

        // Long press (touch) / right click (mouse): open the song's detail view
        // (YouTube tracks get the YouTube video detail, everything else the file
        // detail).
        {
            let open = {
                let sender = sender.clone();
                let path = path.to_string();
                let title = display.clone();
                move || {
                    if let Some(video_id) = crate::core::youtube::parse_yt_path(&path) {
                        sender.input(Msg::YtShowVideoDetail {
                            video_id,
                            title: title.clone(),
                        });
                    } else {
                        sender.input(Msg::ShowTrackDetail(path.clone()));
                    }
                }
            };
            let gesture = gtk::GestureLongPress::new();
            {
                let open = open.clone();
                gesture.connect_pressed(move |g, _, _| {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    open();
                });
            }
            row.add_controller(gesture);
            crate::ui::app::on_secondary_click(&row, open);
        }
        // Play button: plays only this track, keeps the list open.
        let play_btn = gtk::Button::builder()
            .icon_name("media-playback-start-symbolic")
            .tooltip_text(gettext("Play"))
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        {
            let sender = sender.clone();
            let path = path.to_string();
            play_btn.connect_clicked(move |_| {
                sender.input(Msg::PlayOneTrack {
                    path: path.clone(),
                    close: false,
                });
            });
        }
        row.add_suffix(&play_btn);
        // Short tap on the row: play this track and return to the main page.
        {
            let sender = sender.clone();
            let path = path.to_string();
            row.connect_activated(move |_| {
                sender.input(Msg::PlayOneTrack {
                    path: path.clone(),
                    close: true,
                });
            });
        }
        row
    }

    /// Long press on a playlist: a detail view (cover, name, totals and the
    /// playlist-wide actions). Styled like the artist/album context view –
    /// a bottom sheet on the phone (see [`App::adapt_detail_dialog`]).
    pub(crate) fn open_playlist_detail(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
        id: i64,
        name: &str,
    ) {
        // Repoint stream recordings to a local copy of the same song if one now
        // exists (see [`Library::relink_recordings_in_playlist`]).
        let _ = self.library.relink_recordings_in_playlist(id);
        let paths = self.library.playlist_paths(id).unwrap_or_default();
        // Total runtime.
        let total_ms = self.library.playlist_duration_ms(id).unwrap_or(0);
        // Cover candidates from the songs (embedded / album / YouTube – whatever
        // is available). The chosen (or first) cover leads the carousel so the
        // detail opens on the current cover.
        let mut covers = self.playlist_cover_candidates(&paths);
        let chosen = self.playlist_display_cover(id, &paths);
        if let Some(pos) = chosen
            .as_ref()
            .and_then(|c| covers.iter().position(|x| x == c))
        {
            let c = covers.remove(pos);
            covers.insert(0, c);
        }

        let dialog = adw::Dialog::builder()
            .title(gtk::glib::markup_escape_text(name))
            .build();
        // Wider detail dialog (was 360) so the cover and actions have room; the
        // height follows the content (scroller uses its natural height below).
        dialog.set_content_width(600);
        self.adapt_detail_dialog(&dialog);
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(6)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();

        // Cover: several distinct song covers → a swipe carousel with dots; the
        // one left showing when the dialog closes becomes the playlist cover.
        // One or none → a single image / the generic playlist icon.
        let decode = |p: &str| {
            crate::ui::widgets::decode_scaled(p, 360)
                .or_else(|| gtk::gdk::Texture::from_filename(p).ok())
        };
        if covers.len() > 1 {
            let carousel = adw::Carousel::new();
            carousel.set_halign(gtk::Align::Center);
            for path in &covers {
                let tex = decode(path);
                let img =
                    crate::ui::widgets::rounded_image(tex.as_ref(), "view-list-symbolic", 160);
                carousel.append(&img);
            }
            let dots = adw::CarouselIndicatorDots::new();
            dots.set_carousel(Some(&carousel));
            let gallery = gtk::Box::new(gtk::Orientation::Vertical, 6);
            gallery.set_halign(gtk::Align::Center);
            gallery.append(&crate::ui::widgets::carousel_with_arrows(&carousel));
            gallery.append(&dots);
            content.append(&gallery);
            // Adopt the cover shown last in the carousel as the playlist cover.
            let sender = sender.clone();
            let covers = covers.clone();
            dialog.connect_closed(move |_| {
                let idx = carousel.position().round().max(0.0) as usize;
                if let Some(path) = covers.get(idx) {
                    sender.input(Msg::SetPlaylistCover {
                        id,
                        path: path.clone(),
                    });
                }
            });
        } else {
            let tex = covers.first().and_then(|p| decode(p));
            let cover = crate::ui::widgets::rounded_image(tex.as_ref(), "view-list-symbolic", 160);
            cover.set_halign(gtk::Align::Center);
            content.append(&cover);
        }

        let title = gtk::Label::builder()
            .label(name)
            .css_classes(["title-2"])
            .wrap(true)
            .justify(gtk::Justification::Center)
            .build();
        content.append(&title);

        let mut meta = vec![ngettext_n("{n} track", "{n} tracks", paths.len() as u32)];
        if total_ms > 0 {
            meta.push(crate::ui::app::fmt_duration(total_ms));
        }
        content.append(
            &gtk::Label::builder()
                .label(meta.join(" · "))
                .css_classes(["dim-label"])
                .build(),
        );

        // Actions.
        let group = adw::PreferencesGroup::builder().margin_top(6).build();
        let empty = paths.is_empty();
        let row = |icon: &str, label: &str| -> adw::ActionRow {
            let r = adw::ActionRow::builder()
                .title(label)
                .activatable(true)
                .build();
            r.add_prefix(&gtk::Image::from_icon_name(icon));
            r
        };

        let play = row("media-playback-start-symbolic", &gettext("Play playlist"));
        play.set_sensitive(!empty);
        {
            let sender = sender.clone();
            let dialog = dialog.clone();
            play.connect_activated(move |_| {
                sender.input(Msg::PlayPlaylist(id));
                dialog.close();
            });
        }
        group.add(&play);

        let shuffle = row(
            "media-playlist-shuffle-symbolic",
            &gettext("Shuffle playlist"),
        );
        shuffle.set_sensitive(!empty);
        {
            let sender = sender.clone();
            let dialog = dialog.clone();
            shuffle.connect_activated(move |_| {
                sender.input(Msg::PlayPlaylistShuffled(id));
                dialog.close();
            });
        }
        group.add(&shuffle);

        let show = row("view-list-symbolic", &gettext("Show songs"));
        {
            let sender = sender.clone();
            let dialog = dialog.clone();
            show.connect_activated(move |_| {
                sender.input(Msg::OpenPlaylist(id));
                dialog.close();
            });
        }
        group.add(&show);

        let rename = row("document-edit-symbolic", &gettext("Rename"));
        {
            let sender = sender.clone();
            let dialog = dialog.clone();
            rename.connect_activated(move |_| {
                sender.input(Msg::PlaylistRenameDialog(id));
                dialog.close();
            });
        }
        group.add(&rename);

        let delete = row("user-trash-symbolic", &gettext("Delete playlist"));
        delete.add_css_class("error");
        {
            let sender = sender.clone();
            let dialog = dialog.clone();
            delete.connect_activated(move |_| {
                sender.input(Msg::PlaylistDelete(id));
                dialog.close();
            });
        }
        group.add(&delete);

        content.append(&group);

        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .vexpand(true)
            .child(&content)
            .build();
        toolbar.set_content(Some(&scroller));
        dialog.set_child(Some(&toolbar));
        dialog.present(Some(root));
    }

    /// Dialog: add the current context files to an existing playlist
    /// or create a new one.
    pub(crate) fn open_add_to_playlist_dialog(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let playlists = self.library.playlists().unwrap_or_default();

        let dialog = adw::Dialog::builder()
            .title(gettext("Add to playlist"))
            .build();
        // Roomier window, like the detail view (bottom sheet on the phone).
        dialog.set_content_width(400);
        dialog.set_content_height(560);
        self.adapt_detail_dialog(&dialog);
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();

        // Create a new playlist (enter a name, Enter confirms).
        let new_group = adw::PreferencesGroup::builder()
            .title(gettext("New playlist"))
            .build();
        let entry = adw::EntryRow::builder().title(gettext("Name")).build();
        crate::ui::widgets::no_autofocus(&entry);
        new_group.add(&entry);
        content.append(&new_group);
        {
            let sender = sender.clone();
            let entry2 = entry.clone();
            let dialog2 = dialog.clone();
            entry.connect_entry_activated(move |_| {
                if !entry2.text().trim().is_empty() {
                    sender.input(Msg::PlaylistCreateAddTo(entry2.text().to_string()));
                    dialog2.close();
                }
            });
        }

        // Existing playlists (tap = add).
        if !playlists.is_empty() {
            let group = adw::PreferencesGroup::builder()
                .title(gettext("Existing"))
                .build();
            for (id, name, count) in playlists {
                let row = adw::ActionRow::builder()
                    .title(gtk::glib::markup_escape_text(&name))
                    .subtitle(ngettext_n("{n} track", "{n} tracks", count as u32))
                    .activatable(true)
                    .build();
                row.add_prefix(&gtk::Image::from_icon_name("view-list-symbolic"));
                {
                    let sender = sender.clone();
                    let dialog2 = dialog.clone();
                    row.connect_activated(move |_| {
                        sender.input(Msg::PlaylistAddTo(id));
                        dialog2.close();
                    });
                }
                group.add(&row);
            }
            content.append(&group);
        }

        let scroller = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .child(&content)
            .build();
        toolbar.set_content(Some(&scroller));
        dialog.set_child(Some(&toolbar));
        dialog.present(Some(root));
    }

    /// Dialog: rename a playlist (name prefilled).
    pub(crate) fn open_rename_playlist_dialog(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
        id: i64,
    ) {
        let current = self
            .playlists
            .playlist_items
            .iter()
            .find(|(pid, _, _)| *pid == id)
            .map(|(_, n, _)| n.clone())
            .unwrap_or_default();
        let dialog = adw::AlertDialog::new(Some(&gettext("Rename playlist")), None);
        let entry = gtk::Entry::builder()
            .text(&current)
            .activates_default(true)
            .build();
        crate::ui::widgets::no_autofocus(&entry);
        dialog.set_extra_child(Some(&entry));
        dialog.add_responses(&[
            ("cancel", &gettext("Cancel")),
            ("rename", &gettext("Rename")),
        ]);
        dialog.set_response_appearance("rename", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("rename"));
        {
            let sender = sender.clone();
            dialog.connect_response(None, move |_, resp| {
                if resp == "rename" {
                    sender.input(Msg::PlaylistRename {
                        id,
                        name: entry.text().to_string(),
                    });
                }
            });
        }
        dialog.present(Some(root));
    }

    /// Adds the files of the current context target to a playlist.
    pub(crate) fn add_context_to_playlist(&mut self, id: i64, sender: &ComponentSender<Self>) {
        let Some(target) = self.nav.context_target.clone() else {
            return;
        };
        let files: Vec<String> = self
            .ctx_files(&target)
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        let n = files.len();
        let _ = self.library.add_to_playlist(id, &files);
        self.reload_playlists(sender);
        self.toast(&gettext_f(
            "Added {n} to the playlist",
            &[("n", &n.to_string())],
        ));
    }
}
