//! Detail half of [`PodcastsPage`]'s inherent impl: the episode and
//! subscription detail dialogs, the episode list subpage of a podcast, the
//! subscribe/search dialogs, the episode play buttons with their progress and
//! the episode downloads. The struct, its messages, the `Component` impl and
//! the list half stay in [`crate::ui::podcasts_page`] /
//! [`crate::ui::podcasts_page_lists`].

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::core::db::Library;
use crate::i18n::{gettext, gettext_f, ngettext_n};
use crate::ui::app_helpers::{cover_widget, fill_progress_row, on_long_press, on_secondary_click};
use crate::ui::podcasts_page::{
    EpisodeDownload, PodcastsCmd, PodcastsInput, PodcastsOutput, PodcastsPage,
    fetch_and_store_podcast,
};
use crate::ui::widgets::{action_row, detail_box, present_detail_refreshable};

impl PodcastsPage {
    /// Detail view of an entry (episode) from the "Newest" list.
    pub(super) fn open_episode_detail(&self, sender: &ComponentSender<Self>, index: usize) {
        if let Some(ep) = self.newest_items.get(index).cloned() {
            self.show_episode_detail(sender, ep);
        }
    }

    /// Detail refresh of a podcast (or one of its episodes): fetch the feed
    /// again, then its cover; the result reopens the detail view.
    pub(super) fn refresh_detail(
        &self,
        sender: &ComponentSender<Self>,
        id: i64,
        episode: Option<String>,
    ) {
        let Ok(Some(feed)) = self.library.podcast_feed_url(id) else {
            return;
        };
        let _ = sender.output(PodcastsOutput::Toast(gettext("Refreshing …")));
        sender.spawn_command(move |out| {
            let ok = fetch_and_store_podcast(&feed).is_some();
            let image = Library::open().ok().and_then(|lib| {
                lib.podcasts()
                    .unwrap_or_default()
                    .into_iter()
                    .find(|(pid, _, _, _)| *pid == id)
                    .and_then(|(_, _, image, _)| image)
            });
            if let Some(url) = image {
                let _ = crate::core::online::recache_podcast_image(&url);
            }
            let _ = out.send(PodcastsCmd::DetailRefreshed {
                podcast_id: id,
                episode,
                ok,
            });
        });
    }

    /// Episode detail (incl. shownotes) of an episode from the episode list of
    /// an opened podcast (index = order in `episodes(id)`).
    pub(super) fn open_podcast_episode_detail(
        &self,
        sender: &ComponentSender<Self>,
        podcast_id: i64,
        index: usize,
    ) {
        let Some(ep) = self
            .library
            .episodes(podcast_id)
            .unwrap_or_default()
            .into_iter()
            .nth(index)
        else {
            return;
        };
        let (podcast_title, podcast_image) = self
            .podcast_items
            .iter()
            .find(|(pid, _, _, _)| *pid == podcast_id)
            .map(|(_, t, img, _)| (t.clone(), img.clone()))
            .unwrap_or_default();
        self.show_episode_detail(
            sender,
            crate::model::EpisodeRef {
                podcast_title,
                podcast_image,
                title: ep.title,
                audio_url: ep.audio_url,
                published: ep.published,
                duration: ep.duration,
                description: ep.description,
            },
        );
    }

    /// Like [`Self::open_podcast_episode_detail`] but identified by the episode's
    /// audio URL — used when the now-playing track is a podcast started from a
    /// playlist (no podcast id / index at hand). Resolves both from the URL.
    pub(super) fn open_episode_detail_by_url(&self, sender: &ComponentSender<Self>, url: &str) {
        let Some(podcast_id) = self.library.podcast_id_for_episode_url(url).ok().flatten() else {
            return;
        };
        let Some(index) = self
            .library
            .episodes(podcast_id)
            .unwrap_or_default()
            .iter()
            .position(|e| e.audio_url == url)
        else {
            return;
        };
        self.open_podcast_episode_detail(sender, podcast_id, index);
    }

    /// Builds the episode detail dialog (shared by "Newest" and a podcast's
    /// episode list): podcast, date, duration, actions + shownotes.
    fn show_episode_detail(&self, sender: &ComponentSender<Self>, ep: crate::model::EpisodeRef) {
        let Some(root) = self.window.clone() else {
            return;
        };
        // The podcast in the title bar, the episode on the row below the cover.
        let dialog = adw::Dialog::builder().title(&ep.podcast_title).build();
        self.adapt_detail_dialog(&dialog);
        let content = detail_box();

        let info = adw::PreferencesGroup::new();
        let pod = adw::ActionRow::builder()
            .title(gtk::glib::markup_escape_text(&ep.title))
            .subtitle(gtk::glib::markup_escape_text(&ep.podcast_title))
            .build();
        pod.set_title_lines(3);
        let cover = ep
            .podcast_image
            .as_deref()
            .and_then(crate::core::online::podcast_image_path);
        content.append(&crate::ui::widgets::detail_cover(
            cover.as_deref(),
            "microphone-symbolic",
        ));
        info.add(&pod);
        // Published and duration **side by side**, each about 50 % width.
        let pub_txt = ep
            .published
            .as_deref()
            .filter(|p| !p.trim().is_empty())
            .map(crate::core::podcast::pubdate_short);
        let dur_txt = ep
            .duration
            .as_deref()
            .filter(|d| !d.trim().is_empty())
            .map(|d| {
                crate::core::podcast::format_duration(d).unwrap_or_else(|| d.trim().to_string())
            });
        let meta = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .homogeneous(true)
            .spacing(12)
            .margin_top(10)
            .margin_bottom(10)
            .margin_start(14)
            .margin_end(14)
            .build();
        let cell = |title: &str, value: &str| {
            let b = gtk::Box::new(gtk::Orientation::Vertical, 2);
            b.append(
                &gtk::Label::builder()
                    .label(title)
                    .xalign(0.0)
                    .css_classes(["caption", "dim-label"])
                    .build(),
            );
            b.append(
                &gtk::Label::builder()
                    .label(value)
                    .xalign(0.0)
                    .wrap(true)
                    .build(),
            );
            b
        };
        if let Some(p) = &pub_txt {
            meta.append(&cell(&gettext("Published"), p));
        }
        if let Some(d) = &dur_txt {
            meta.append(&cell(&gettext("Duration"), d));
        }
        // Download column: "Download" heading over a tappable value label.
        let dl_cell = gtk::Box::new(gtk::Orientation::Vertical, 2);
        dl_cell.append(
            &gtk::Label::builder()
                .label(gettext("Download"))
                .xalign(0.0)
                .css_classes(["caption", "dim-label"])
                .build(),
        );
        let dl_value = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["accent"])
            .build();
        dl_cell.append(&dl_value);
        // Only shown while a download runs (and only with a known total size).
        let dl_bar = gtk::ProgressBar::builder()
            .fraction(0.0)
            .valign(gtk::Align::Center)
            .margin_top(4)
            .visible(false)
            .build();
        dl_bar.add_css_class("emilia-hourbar");
        dl_cell.append(&dl_bar);
        dl_cell.set_cursor_from_name(Some("pointer"));
        {
            let (sender, url, title) = (sender.clone(), ep.audio_url.clone(), ep.title.clone());
            let click = gtk::GestureClick::new();
            click.connect_released(move |g, _, _, _| {
                g.set_state(gtk::EventSequenceState::Claimed);
                sender.input(PodcastsInput::ToggleDownload {
                    url: url.clone(),
                    title: title.clone(),
                });
            });
            dl_cell.add_controller(click);
        }
        meta.append(&dl_cell);
        // `meta` joins `info` below the shownotes (added further down).
        content.append(&info);

        *self.ctx_episode_download.borrow_mut() = Some((dl_value, dl_bar, ep.audio_url.clone()));
        self.refresh_download_row();

        // Per-episode equalizer (inherits podcast → global during playback).
        let actions = adw::PreferencesGroup::new();
        let eq = action_row(
            &gettext("Equalizer settings"),
            "multimedia-equalizer-symbolic",
        );
        {
            let (sender, dialog) = (sender.clone(), dialog.clone());
            let (url, title) = (ep.audio_url.clone(), ep.title.clone());
            eq.connect_activated(move |_| {
                let _ = sender.output(PodcastsOutput::OpenEpisodeEqualizer {
                    url: url.clone(),
                    title: title.clone(),
                });
                dialog.close();
            });
        }
        actions.add(&eq);
        // Share this episode over device sync (its podcast comes along so it
        // shows up on the other device, with this episode's progress).
        let share = action_row(&gettext("Share"), "emilia-share-symbolic");
        {
            let (sender, dialog, url) = (sender.clone(), dialog.clone(), ep.audio_url.clone());
            share.connect_activated(move |_| {
                let _ = sender.output(PodcastsOutput::Share(Box::new(
                    crate::core::sync::share::Selection {
                        podcast_episodes: vec![url.clone()],
                        ..Default::default()
                    },
                )));
                dialog.close();
            });
        }
        actions.add(&share);
        content.append(&actions);

        // Shownotes (if present): timestamps become clickable jump markers, web
        // addresses ordinary links (opened by the label's default handler).
        // They sit right under the episode, above "Published".
        if let Some(notes) = ep.description.as_deref().filter(|s| !s.trim().is_empty()) {
            // Always wrap, including inside long unbreakable tokens (URLs), so a
            // shownote can never force the dialog wider than the screen.
            let label = gtk::Label::builder()
                .label(crate::core::podcast::linkify_shownotes(notes.trim()))
                .use_markup(true)
                .wrap(true)
                .wrap_mode(gtk::pango::WrapMode::WordChar)
                .xalign(0.0)
                .selectable(true)
                .build();
            label.add_css_class("body");
            {
                let sender = sender.clone();
                let url = ep.audio_url.clone();
                let title = ep.title.clone();
                label.connect_activate_link(move |_, uri| {
                    if let Some(ms) = uri
                        .strip_prefix("emilia-seek:")
                        .and_then(|s| s.parse::<i64>().ok())
                    {
                        let _ = sender.output(PodcastsOutput::EpisodeSeekTo {
                            url: url.clone(),
                            title: title.clone(),
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
            // Collapsed by default: long shownotes would otherwise push the
            // dialog's actions out of view — one tap on the row unfolds them.
            let expander = adw::ExpanderRow::builder()
                .title(gettext("Shownotes"))
                .expanded(false)
                .build();
            expander.add_row(&wrap);
            // Unfolding makes the row tall, and GTK scrolls the focused row
            // fully into view — which lands at the *end* of the notes, with the
            // row (and its fold-away arrow) off screen. Scroll back to the row
            // once the unfold animation has settled.
            expander.connect_expanded_notify(|row| {
                if !row.is_expanded() {
                    return;
                }
                let row = row.clone();
                gtk::glib::timeout_add_local_once(
                    std::time::Duration::from_millis(300),
                    move || {
                        let Some(scroller) = row
                            .ancestor(gtk::ScrolledWindow::static_type())
                            .and_then(|w| w.downcast::<gtk::ScrolledWindow>().ok())
                        else {
                            return;
                        };
                        let Some(point) =
                            row.compute_point(&scroller, &gtk::graphene::Point::new(0.0, 0.0))
                        else {
                            return;
                        };
                        let adj = scroller.vadjustment();
                        adj.set_value((adj.value() + point.y() as f64 - 8.0).max(0.0));
                    },
                );
            });
            info.add(&expander);
        }
        info.add(&meta);

        {
            let (sender, url) = (sender.clone(), ep.audio_url.clone());
            present_detail_refreshable(&dialog, &content, &root, move || {
                sender.input(PodcastsInput::RefreshEpisodeDetail(url.clone()));
            });
        }
    }

    /// Detail view/management of a subscription: cover, episode count, and
    /// actions to open, refresh, and remove (with confirmation).
    pub(super) fn open_podcast_detail(&self, sender: &ComponentSender<Self>, id: i64) {
        let Some(root) = self.window.clone() else {
            return;
        };
        let Some((_, title, image, count)) = self
            .podcast_items
            .iter()
            .find(|(p, _, _, _)| *p == id)
            .cloned()
        else {
            return;
        };
        let dialog = adw::Dialog::builder().title(&title).build();
        self.adapt_detail_dialog(&dialog);
        let content = detail_box();

        let info = adw::PreferencesGroup::new();
        let head = adw::ActionRow::builder()
            .title(gtk::glib::markup_escape_text(&title))
            .subtitle(ngettext_n("{n} episode", "{n} episodes", count as u32))
            .build();
        let cover = image
            .as_deref()
            .and_then(crate::core::online::podcast_image_path);
        content.append(&crate::ui::widgets::detail_cover(
            cover.as_deref(),
            "microphone-symbolic",
        ));
        info.add(&head);
        content.append(&info);

        let actions = adw::PreferencesGroup::new();
        let open = action_row(&gettext("Open episodes"), "go-next-symbolic");
        {
            let (sender, dialog) = (sender.clone(), dialog.clone());
            open.connect_activated(move |_| {
                sender.input(PodcastsInput::OpenPodcast(id));
                dialog.close();
            });
        }
        actions.add(&open);
        let eq = action_row(
            &gettext("Equalizer settings"),
            "multimedia-equalizer-symbolic",
        );
        {
            let (sender, dialog) = (sender.clone(), dialog.clone());
            eq.connect_activated(move |_| {
                let _ = sender.output(PodcastsOutput::OpenPodcastEqualizer(id));
                dialog.close();
            });
        }
        actions.add(&eq);
        // Share the podcast (feed + episodes incl. show notes) over device sync.
        if let Some(feed) = self.library.podcast_feed_url(id).ok().flatten() {
            let share = action_row(&gettext("Share"), "emilia-share-symbolic");
            let (sender, dialog) = (sender.clone(), dialog.clone());
            share.connect_activated(move |_| {
                let _ = sender.output(PodcastsOutput::Share(Box::new(
                    crate::core::sync::share::Selection {
                        podcast_feeds: vec![feed.clone()],
                        ..Default::default()
                    },
                )));
                dialog.close();
            });
            actions.add(&share);
        }
        let remove = action_row(&gettext("Remove podcast"), "user-trash-symbolic");
        {
            let (sender, dialog) = (sender.clone(), dialog.clone());
            remove.connect_activated(move |_| {
                dialog.close();
                sender.input(PodcastsInput::Delete(id));
            });
        }
        actions.add(&remove);
        content.append(&actions);

        {
            let sender = sender.clone();
            present_detail_refreshable(&dialog, &content, &root, move || {
                sender.input(PodcastsInput::RefreshPodcastDetail(id));
            });
        }
    }

    /// Episode subpage of a podcast (play button = stream episode, long press =
    /// detail view).
    pub(super) fn open_podcast(&self, sender: &ComponentSender<Self>, id: i64, title: &str) {
        let episodes = self.library.episodes(id).unwrap_or_default();
        // Resume positions of *all* episodes in one query (like "Newest"),
        // to mark on each row how far it has already been listened to.
        let progress: HashMap<String, (i64, bool)> = self
            .library
            .all_episode_progress()
            .unwrap_or_default()
            .into_iter()
            .map(|(url, pos, fin)| (url, (pos, fin)))
            .collect();
        let cover = self
            .podcast_items
            .iter()
            .find(|(pid, _, _, _)| *pid == id)
            .and_then(|(_, _, img, _)| img.as_deref())
            .and_then(crate::core::online::podcast_image_path);

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
                    episodes.len()
                )
                .as_str(),
            )
            .build();

        if episodes.is_empty() {
            group.add(
                &adw::ActionRow::builder()
                    .title(gettext("No episodes"))
                    .build(),
            );
        }
        for (i, ep) in episodes.iter().enumerate() {
            let mut subtitle = String::new();
            if let Some(p) = &ep.published {
                subtitle.push_str(p.trim());
            }
            if let Some(d) = &ep.duration {
                if !subtitle.is_empty() {
                    subtitle.push_str(" · ");
                }
                subtitle.push_str(d.trim());
            }
            // Listening progress on its own line below published/duration, the
            // same wording as "Newest". Finished episodes read "Listened";
            // in-progress ones show elapsed [/ total].
            let (position_ms, finished) =
                progress.get(&ep.audio_url).copied().unwrap_or((0, false));
            if finished {
                if !subtitle.is_empty() {
                    subtitle.push('\n');
                }
                subtitle.push_str(&gettext("Listened"));
            } else if position_ms > 0 {
                let elapsed = crate::ui::app_helpers::fmt_duration(position_ms);
                let total_secs = ep
                    .duration
                    .as_deref()
                    .and_then(crate::core::podcast::duration_secs)
                    .filter(|s| *s > 0);
                if !subtitle.is_empty() {
                    subtitle.push('\n');
                }
                subtitle.push_str(&match total_secs {
                    Some(secs) => gettext_f(
                        "{position} of {total} listened",
                        &[
                            ("position", &elapsed),
                            ("total", &crate::ui::app_helpers::fmt_duration(secs * 1000)),
                        ],
                    ),
                    None => gettext_f("{position} listened", &[("position", &elapsed)]),
                });
            }
            // Not activatable: like a library track, the episode plays via its
            // play button; long press / right click opens the detail view.
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&ep.title))
                .subtitle(gtk::glib::markup_escape_text(&subtitle))
                .build();
            row.add_css_class("emilia-flush");
            if finished || position_ms > 0 {
                row.set_subtitle_lines(2);
            }
            row.add_prefix(&cover_widget(cover.as_deref(), "microphone-symbolic"));
            row.add_suffix(&self.episode_play_button(sender, &ep.audio_url, &ep.title));
            on_secondary_click(&row, {
                let sender = sender.clone();
                move || {
                    sender.input(PodcastsInput::ShowPodcastEpisodeDetail {
                        podcast_id: id,
                        index: i,
                    });
                }
            });
            on_long_press(&row, {
                let sender = sender.clone();
                move || {
                    sender.input(PodcastsInput::ShowPodcastEpisodeDetail {
                        podcast_id: id,
                        index: i,
                    });
                }
            });
            group.add(&row);
        }
        content.append(&group);
        // Park the built page and ask the parent to push it. The play/pause
        // icons are refreshed only *after* the parent has mounted the subpage
        // (it echoes `PlaybackStateChanged` back), because `refresh_episode_icons`
        // drops rows whose widgets aren't realized yet.
        *self.subpage_slot.borrow_mut() =
            Some((gettext_f("Podcast – {title}", &[("title", title)]), content));
        let _ = sender.output(PodcastsOutput::PushSubpage);
    }

    /// The "+": a centered choice modal like the Files "+" — search the
    /// podcast directory, or enter a feed address (RSS) by hand.
    pub(super) fn open_subscribe_podcast_dialog(&self, sender: &ComponentSender<Self>) {
        let Some(root) = self.window.clone() else {
            return;
        };
        let slot = self.podcast_search.clone();
        let (sender, win) = (sender.clone(), root.clone());
        let dialog = crate::ui::widgets::choice_modal(
            &gettext("Subscribe to podcast"),
            &[
                ("search", gettext("Search podcasts")),
                ("url", gettext("Enter feed address")),
            ],
            "search",
            move |resp| match resp {
                "search" => open_podcast_search_modal(&sender, &slot, &win),
                "url" => {
                    let sender = sender.clone();
                    let (dialog, _) = crate::ui::widgets::entry_modal(
                        &gettext("Enter feed address"),
                        &gettext("Feed address (RSS)"),
                        &gettext("Subscribe"),
                        move |url| sender.input(PodcastsInput::SubscribeUrl(url)),
                    );
                    dialog.present(Some(&win));
                }
                _ => {}
            },
        );
        dialog.present(Some(&root));
    }

    /// Redraws the results list in the open subscription search dialog.
    pub(super) fn rebuild_podcast_search_results(&self, sender: &ComponentSender<Self>) {
        let guard = self.podcast_search.borrow();
        let Some((dialog, list)) = guard.as_ref() else {
            return;
        };
        while let Some(child) = list.first_child() {
            list.remove(&child);
        }
        list.set_visible(true);

        if self.podcast_search_results.is_empty() {
            let row = if self.podcast_search_failed {
                let r = adw::ActionRow::builder()
                    .title(gettext("Search service unreachable"))
                    .subtitle(gettext("Check your connection and try again"))
                    .build();
                r.set_subtitle_lines(2);
                r
            } else {
                adw::ActionRow::builder()
                    .title(gettext("No podcasts found"))
                    .build()
            };
            row.set_sensitive(false);
            list.append(&row);
            return;
        }

        for r in &self.podcast_search_results {
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&r.title))
                .activatable(true)
                .build();
            if let Some(a) = r.author.as_deref().filter(|a| !a.trim().is_empty()) {
                row.set_subtitle(&gtk::glib::markup_escape_text(a));
            }
            let cover = r
                .image_url
                .as_deref()
                .and_then(crate::core::online::podcast_image_path);
            row.add_prefix(&cover_widget(cover.as_deref(), "microphone-symbolic"));
            row.add_suffix(&gtk::Image::from_icon_name("list-add-symbolic"));
            {
                let (sender, dialog, feed) = (sender.clone(), dialog.clone(), r.feed_url.clone());
                row.connect_activated(move |_| {
                    sender.input(PodcastsInput::SubscribeUrl(feed.clone()));
                    dialog.close();
                });
            }
            list.append(&row);
        }
    }

    /// Play/Pause button (suffix) for an entry row: tap = toggle episode.
    pub(super) fn episode_play_button(
        &self,
        sender: &ComponentSender<Self>,
        url: &str,
        title: &str,
    ) -> gtk::Button {
        let active = self.playing_url.as_deref() == Some(url);
        let btn = crate::ui::play_mark::button(&gettext("Play/Pause"), active, self.playing);
        {
            let (sender, url, title) = (sender.clone(), url.to_string(), title.to_string());
            btn.connect_clicked(move |_| {
                let _ = sender.output(PodcastsOutput::ToggleEpisode {
                    url: url.clone(),
                    title: title.clone(),
                });
            });
        }
        self.episode_marks.add(url.to_string(), &btn);
        btn
    }

    /// Refreshes the progress line of every visible row of `url` — driven by the
    /// transport's per-second tick, so the bar in "Newest"/"Recently"/a podcast's
    /// episode list moves along with playback instead of only after a rebuild.
    /// Rows whose widget left the tree are dropped along the way.
    pub(super) fn apply_episode_progress(
        &self,
        url: &str,
        position_ms: i64,
        duration_ms: i64,
        finished: bool,
    ) {
        let mut rows = self.episode_progress_rows.borrow_mut();
        rows.retain(|r| r.row.root().is_some());
        for entry in rows.iter().filter(|r| r.url == url) {
            // The feed's length wins (it is what the row shows elsewhere); the
            // player's duration fills in for feeds that state none.
            let total = entry
                .total_secs
                .or_else(|| (duration_ms > 0).then_some(duration_ms / 1000));
            fill_progress_row(&entry.row, position_ms, total, finished);
        }
    }

    /// Updates the Play/Pause icons of all visible entry rows and the "Play" row
    /// of an open detail dialog. Detached rows are discarded in the process.
    pub(super) fn refresh_episode_icons(&self) {
        let active = self.playing_url.clone();
        let playing = self.playing;
        let is_active = |url: &str| playing && active.as_deref() == Some(url);
        self.episode_marks
            .apply_all(playing, |url| active.as_deref() == Some(url));
        if let Some((row, url)) = self.ctx_episode_play.borrow().as_ref() {
            row.set_visible(!is_active(url));
        }
    }

    /// Updates the download row of an open episode detail dialog to reflect the
    /// offline state of its episode: while a download runs it shows the live
    /// percentage plus the estimated remaining time over a progress bar.
    pub(super) fn refresh_download_row(&self) {
        let guard = self.ctx_episode_download.borrow();
        let Some((label, bar, url)) = guard.as_ref() else {
            return;
        };
        let running = self.downloading_episodes.get(url);
        let downloaded =
            running.is_none() && self.library.episode_download(url).ok().flatten().is_some();
        match running {
            Some(dl) => {
                label.set_label(&dl.status_text());
                // Without a Content-Length there is nothing to fill the bar
                // with — the label then reports the downloaded size instead.
                match dl.fraction() {
                    Some(frac) => {
                        bar.set_fraction(frac);
                        bar.set_visible(true);
                    }
                    None => bar.set_visible(false),
                }
            }
            None => {
                bar.set_visible(false);
                label.set_label(&if downloaded {
                    gettext("Remove download")
                } else {
                    gettext("For offline listening")
                });
            }
        }
    }

    /// Download the episode for offline playback, or delete an existing copy.
    pub(super) fn toggle_episode_download(
        &mut self,
        sender: &ComponentSender<Self>,
        url: String,
        title: String,
    ) {
        if self.downloading_episodes.contains_key(&url) {
            return;
        }
        if let Some(path) = self.library.delete_episode_download(&url).unwrap_or(None) {
            let _ = std::fs::remove_file(&path);
            self.refresh_download_row();
            let _ = sender.output(PodcastsOutput::Toast(gettext("Download removed")));
            return;
        }
        self.downloading_episodes
            .insert(url.clone(), EpisodeDownload::new());
        self.refresh_download_row();
        let _ = sender.output(PodcastsOutput::Toast(gettext_f(
            "Downloading “{title}” …",
            &[("title", &title)],
        )));
        let dl_url = url.clone();
        sender.spawn_command(move |out| {
            let dest = crate::core::online::episode_download_dest(&dl_url);
            // Progress reports (throttled in `download_episode_progress`) drive
            // the percentage/remaining-time readout in the detail dialog.
            let progress = {
                let (out, url) = (out.clone(), dl_url.clone());
                move |p: crate::core::podcast::DownloadProgress| {
                    let _ = out.send(PodcastsCmd::DownloadProgress {
                        url: url.clone(),
                        done: p.done,
                        total: p.total,
                    });
                }
            };
            // On a panic still report back, or the episode stays in
            // `downloading_episodes` and can never be downloaded again.
            let result = crate::core::panic_guard::catch_or(
                "episode download",
                || match crate::core::podcast::download_episode_progress(&dl_url, &dest, progress) {
                    Ok(_) => {
                        let path = dest.to_string_lossy().into_owned();
                        if let Ok(lib) = Library::open() {
                            let _ = lib.set_episode_download(&dl_url, &path);
                        }
                        Ok(path)
                    }
                    Err(e) => Err(e.to_string()),
                },
                || Err("internal error".to_string()),
            );
            let _ = out.send(PodcastsCmd::Downloaded {
                url: dl_url.clone(),
                result,
            });
        });
    }
}

/// Second step of the podcasts "+": the directory search. Registers the dialog
/// in `slot` so the worker's results land in its list.
fn open_podcast_search_modal(
    sender: &ComponentSender<PodcastsPage>,
    slot: &Rc<RefCell<Option<(adw::Dialog, gtk::ListBox)>>>,
    root: &impl IsA<gtk::Widget>,
) {
    let sender = sender.clone();
    let (dialog, entry, results) = crate::ui::widgets::search_modal(
        &gettext("Search podcasts"),
        &gettext("Podcast name …"),
        move |term| sender.input(PodcastsInput::Search(term)),
    );
    let dialog: adw::Dialog = dialog.upcast();
    *slot.borrow_mut() = Some((dialog.clone(), results));
    {
        let slot = slot.clone();
        dialog.connect_closed(move |this| {
            // A dialog reopened during this one's close animation already owns
            // the slot; leave it alone.
            let is_current = slot.borrow().as_ref().is_some_and(|(d, _)| d == this);
            if is_current {
                *slot.borrow_mut() = None;
            }
        });
    }
    dialog.present(Some(root));
    entry.grab_focus();
}
