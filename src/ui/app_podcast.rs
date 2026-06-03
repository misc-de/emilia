//! Podcasts: overview list, episode subpage, subscription dialog, and the
//! background fetching of feeds. Episodes are streamed directly.

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::core::db::Library;
use crate::i18n::{gettext, gettext_f, ngettext_n};
use crate::ui::app::{App, Msg};

/// Fetches a feed and stores podcast + episodes (runs in the worker thread,
/// its own DB connection). Returns the podcast title on success.
pub(crate) fn fetch_and_store_podcast(feed_url: &str) -> Option<String> {
    let feed = crate::core::podcast::fetch_feed(feed_url).ok()?;
    let lib = Library::open().ok()?;
    let id = lib
        .subscribe_podcast(&feed.title, feed_url, feed.image_url.as_deref())
        .ok()?;
    let _ = lib.set_episodes(id, &feed.episodes);
    // Load the feed image into the cache (worker thread, no UI block).
    if let Some(img) = feed.image_url.as_deref() {
        crate::core::online::cache_podcast_image(img);
    }
    Some(feed.title)
}

/// Content box for the detail dialogs (uniform margins).
fn detail_box() -> gtk::Box {
    gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(6)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build()
}

/// Activatable action row with an icon prefix (for the detail dialogs).
fn action_row(title: &str, icon: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).activatable(true).build();
    row.add_prefix(&gtk::Image::from_icon_name(icon));
    row
}

/// Embeds the content scrollably in a dialog with a header bar and shows it.
fn present_detail(dialog: &adw::Dialog, content: &gtk::Box, root: &adw::ApplicationWindow) {
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .vexpand(true)
        .child(content)
        .build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&scroller));
    dialog.set_child(Some(&toolbar));
    // Use full width, but never wider than 600 px (on narrow windows the
    // dialog automatically shrinks to the window width).
    dialog.set_content_width(600);
    dialog.present(Some(root));
}

impl App {
    /// Rebuilds the overview of subscribed podcasts: cover, title, episode
    /// count. Tapping opens the episodes; **long press** opens the
    /// subscription detail view (refresh/remove). Afterwards also refreshes
    /// the "Newest" list.
    pub(crate) fn reload_podcasts(&mut self, sender: &ComponentSender<Self>) {
        self.podcasts.podcast_items = self.library.podcasts().unwrap_or_default();
        if self.gallery_view {
            // Gallery variant: cover grid; tap opens the episodes,
            // double-tap the subscription detail view.
            let tiles: Vec<(Option<String>, &'static str, String)> = self
                .podcasts
                .podcast_items
                .iter()
                .map(|(_, title, image, _)| {
                    let cover = image
                        .as_deref()
                        .and_then(crate::core::online::podcast_image_path);
                    (cover, "microphone-symbolic", title.clone())
                })
                .collect();
            self.fill_gallery(
                &self.podcasts.podcasts_gallery,
                &tiles,
                Msg::OpenPodcastAt,
                Msg::ShowPodcastDetailAt,
            );
        } else {
            while let Some(child) = self.podcasts.podcasts_list.first_child() {
                self.podcasts.podcasts_list.remove(&child);
            }
            for (id, title, image, count) in self.podcasts.podcast_items.clone() {
                // Episode count in parentheses on the heading, as with albums/songs;
                // no separate "N episodes" line.
                let row = adw::ActionRow::builder()
                    .title(format!("{} ({count})", gtk::glib::markup_escape_text(&title)).as_str())
                    .activatable(true)
                    .build();
                row.add_css_class("emilia-flush");
                // Cover from the RSS image (local cache); otherwise microphone placeholder.
                let cover = image
                    .as_deref()
                    .and_then(crate::core::online::podcast_image_path);
                row.add_prefix(&crate::ui::app::cover_widget(
                    cover.as_deref(),
                    "microphone-symbolic",
                ));
                {
                    let sender = sender.clone();
                    row.connect_activated(move |_| sender.input(Msg::OpenPodcast(id)));
                }
                // Long press → subscription detail view.
                let lp = gtk::GestureLongPress::new();
                {
                    let sender = sender.clone();
                    lp.connect_pressed(move |g, _, _| {
                        g.set_state(gtk::EventSequenceState::Claimed);
                        sender.input(Msg::ShowPodcastDetail(id));
                    });
                }
                row.add_controller(lp);
                self.podcasts.podcasts_list.append(&row);
            }
        }
        self.reload_newest(sender);
    }

    /// Builds the "Newest" list: newest episodes (entries) across **all**
    /// subscriptions, chronologically by publication date. Tapping streams;
    /// **long press** opens the entry detail view.
    pub(crate) fn reload_newest(&mut self, sender: &ComponentSender<Self>) {
        // Only show episodes from at most ~one month ago.
        let cutoff = crate::core::podcast::recent_cutoff_key();
        let mut eps: Vec<_> = self
            .library
            .all_episodes()
            .unwrap_or_default()
            .into_iter()
            .filter(|e| crate::core::podcast::pubdate_key(e.published.as_deref()) >= cutoff)
            .collect();
        eps.sort_by(|a, b| {
            crate::core::podcast::pubdate_key(b.published.as_deref())
                .cmp(&crate::core::podcast::pubdate_key(a.published.as_deref()))
        });
        eps.truncate(150);
        self.podcasts.newest_items = eps;
        while let Some(child) = self.podcasts.newest_list.first_child() {
            self.podcasts.newest_list.remove(&child);
        }

        // Sort by recency: Today / Yesterday / This week / This month. The
        // list is sorted in descending order, so the sections are contiguous;
        // each section gets its own group (with heading), and an entry appears
        // only in the topmost matching section (no duplication).
        let (today, yesterday, week_start) = crate::core::podcast::recent_day_buckets();
        let bucket_of = |k: i64| -> usize {
            if k >= today {
                0
            } else if k >= yesterday {
                1
            } else if k >= week_start {
                2
            } else {
                3
            }
        };
        let bucket_title = |b: usize| match b {
            0 => gettext("Today"),
            1 => gettext("Yesterday"),
            2 => gettext("This week"),
            _ => gettext("This month"),
        };

        let mut cur_bucket: Option<usize> = None;
        let mut group: Option<adw::PreferencesGroup> = None;
        for (i, ep) in self.podcasts.newest_items.iter().enumerate() {
            let b = bucket_of(crate::core::podcast::pubdate_key(ep.published.as_deref()));
            // New section → new group with heading (only when there is something).
            if cur_bucket != Some(b) {
                cur_bucket = Some(b);
                let g = adw::PreferencesGroup::builder().title(&bucket_title(b)).build();
                self.podcasts.newest_list.append(&g);
                group = Some(g);
            }

            let mut subtitle = ep.podcast_title.clone();
            if let Some(p) = ep.published.as_deref().filter(|p| !p.trim().is_empty()) {
                subtitle.push_str(" · ");
                subtitle.push_str(&crate::core::podcast::pubdate_short(p));
            }
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&ep.title))
                .subtitle(gtk::glib::markup_escape_text(&subtitle))
                .activatable(true)
                .build();
            row.add_css_class("emilia-flush");
            let cover = ep
                .podcast_image
                .as_deref()
                .and_then(crate::core::online::podcast_image_path);
            row.add_prefix(&crate::ui::app::cover_widget(
                cover.as_deref(),
                "microphone-symbolic",
            ));
            row.add_suffix(&self.episode_play_button(sender, &ep.audio_url, &ep.title));
            {
                let sender = sender.clone();
                let url = ep.audio_url.clone();
                let title = ep.title.clone();
                row.connect_activated(move |_| {
                    sender.input(Msg::ToggleEpisode {
                        url: url.clone(),
                        title: title.clone(),
                    });
                });
            }
            // Long press → entry detail view.
            let lp = gtk::GestureLongPress::new();
            {
                let sender = sender.clone();
                lp.connect_pressed(move |g, _, _| {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    sender.input(Msg::ShowEpisodeDetail(i));
                });
            }
            row.add_controller(lp);
            if let Some(g) = &group {
                g.add(&row);
            }
        }
        // Set the icons of the newly built rows to the current playback state
        // (and discard dead rows from the previous list).
        self.refresh_episode_icons();
    }

    /// Detail view of an entry (episode) from the "Newest" list: podcast,
    /// date, duration – with actions to play and to open the podcast.
    pub(crate) fn open_episode_detail(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
        index: usize,
    ) {
        if let Some(ep) = self.podcasts.newest_items.get(index).cloned() {
            self.show_episode_detail(root, sender, ep);
        }
    }

    /// Episode detail (incl. shownotes) of an episode from the episode list
    /// of an opened podcast (index = order in `episodes(id)`).
    pub(crate) fn open_podcast_episode_detail(
        &self,
        root: &adw::ApplicationWindow,
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
            .podcasts
            .podcast_items
            .iter()
            .find(|(pid, _, _, _)| *pid == podcast_id)
            .map(|(_, t, img, _)| (t.clone(), img.clone()))
            .unwrap_or_default();
        self.show_episode_detail(
            root,
            sender,
            crate::model::EpisodeRef {
                podcast_id,
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

    /// Builds the episode detail dialog (shared by "Newest" and the episode
    /// list of a podcast): podcast, date, duration, actions + shownotes.
    fn show_episode_detail(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
        ep: crate::model::EpisodeRef,
    ) {
        let dialog = adw::Dialog::builder()
            .title(gtk::glib::markup_escape_text(&ep.title))
            .build();
        self.adapt_detail_dialog(&dialog);
        let content = detail_box();

        let info = adw::PreferencesGroup::new();
        let pod = adw::ActionRow::builder()
            .title(&gettext("Podcast"))
            .subtitle(gtk::glib::markup_escape_text(&ep.podcast_title))
            .build();
        let cover = ep
            .podcast_image
            .as_deref()
            .and_then(crate::core::online::podcast_image_path);
        pod.add_prefix(&crate::ui::app::cover_widget(cover.as_deref(), "microphone-symbolic"));
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
            .map(|d| crate::core::podcast::format_duration(d).unwrap_or_else(|| d.trim().to_string()));
        if pub_txt.is_some() || dur_txt.is_some() {
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
                b.append(&gtk::Label::builder().label(value).xalign(0.0).wrap(true).build());
                b
            };
            if let Some(p) = &pub_txt {
                meta.append(&cell(&gettext("Published"), p));
            }
            if let Some(d) = &dur_txt {
                meta.append(&cell(&gettext("Duration"), d));
            }
            info.add(&meta);
        }
        content.append(&info);

        // Shownotes (if present) directly below "Duration", before the actions.
        // Timestamps (e.g. "12:34") become clickable jump markers.
        if let Some(notes) = ep.description.as_deref().filter(|s| !s.trim().is_empty()) {
            // Heading without an adw group title, so it sits at the same
            // indentation as the shownotes text (not to the left of it).
            let notes_group = adw::PreferencesGroup::new();
            let label = gtk::Label::builder()
                .label(&crate::core::podcast::linkify_timestamps(notes.trim()))
                .use_markup(true)
                .wrap(true)
                .xalign(0.0)
                .selectable(true)
                .build();
            label.add_css_class("body");
            // Click on a timestamp → jump to that position (start the episode
            // there if needed).
            {
                let sender = sender.clone();
                let url = ep.audio_url.clone();
                let title = ep.title.clone();
                label.connect_activate_link(move |_, uri| {
                    if let Some(ms) = uri
                        .strip_prefix("emilia-seek:")
                        .and_then(|s| s.parse::<i64>().ok())
                    {
                        sender.input(Msg::EpisodeSeekTo {
                            url: url.clone(),
                            title: title.clone(),
                            ms,
                        });
                        return gtk::glib::Propagation::Stop;
                    }
                    gtk::glib::Propagation::Proceed
                });
            }
            // Wrap in a padded box – so the shownotes (like the
            // Published/Duration row) appear as a framed card with inner
            // padding instead of sticking flush to the card edge.
            let wrap = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(6)
                .margin_top(10)
                .margin_bottom(10)
                .margin_start(14)
                .margin_end(14)
                .build();
            // Heading at the same indentation as the text (in the same box).
            let heading = gtk::Label::builder()
                .label(&gettext("Shownotes"))
                .xalign(0.0)
                .css_classes(["heading"])
                .build();
            wrap.append(&heading);
            wrap.append(&label);
            notes_group.add(&wrap);
            content.append(&notes_group);
        }

        present_detail(&dialog, &content, root);
    }

    /// Detail view/management of a subscription: cover, episode count, and
    /// actions to open, refresh, and remove (with confirmation).
    pub(crate) fn open_podcast_detail(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
        id: i64,
    ) {
        let Some((_, title, image, count)) =
            self.podcasts.podcast_items.iter().find(|(p, _, _, _)| *p == id).cloned()
        else {
            return;
        };
        let dialog = adw::Dialog::builder()
            .title(gtk::glib::markup_escape_text(&title))
            .build();
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
        head.add_prefix(&crate::ui::app::cover_widget(cover.as_deref(), "microphone-symbolic"));
        info.add(&head);
        content.append(&info);

        let actions = adw::PreferencesGroup::new();
        let open = action_row(&gettext("Open episodes"), "go-next-symbolic");
        {
            let (sender, dialog) = (sender.clone(), dialog.clone());
            open.connect_activated(move |_| {
                sender.input(Msg::OpenPodcast(id));
                dialog.close();
            });
        }
        actions.add(&open);
        let refresh = action_row(&gettext("Refresh feed"), "view-refresh-symbolic");
        {
            let (sender, dialog) = (sender.clone(), dialog.clone());
            refresh.connect_activated(move |_| {
                sender.input(Msg::PodcastRefresh(id));
                dialog.close();
            });
        }
        actions.add(&refresh);
        let remove = action_row(&gettext("Remove podcast"), "user-trash-symbolic");
        {
            let (sender, dialog, root) = (sender.clone(), dialog.clone(), root.clone());
            remove.connect_activated(move |_| {
                dialog.close();
                crate::ui::app::confirm_destructive(
                    &root,
                    &gettext("Remove this podcast?"),
                    &gettext("Remove"),
                    sender.clone(),
                    Msg::PodcastDelete(id),
                );
            });
        }
        actions.add(&remove);
        content.append(&actions);

        present_detail(&dialog, &content, root);
    }

    /// Episode subpage of a podcast (tap = stream episode).
    pub(crate) fn open_podcast(&self, sender: &ComponentSender<Self>, id: i64, title: &str) {
        let episodes = self.library.episodes(id).unwrap_or_default();
        // Determine the podcast cover once and show it in all episode rows.
        let cover = self
            .podcasts
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
        // Count directly in the heading (in parentheses); the separate
        // "N episodes" line would then be a duplication and is dropped.
        let group = adw::PreferencesGroup::builder()
            .title(format!("{} ({})", gtk::glib::markup_escape_text(title), episodes.len()).as_str())
            .build();

        if episodes.is_empty() {
            group.add(&adw::ActionRow::builder().title(&gettext("No episodes")).build());
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
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&ep.title))
                .subtitle(gtk::glib::markup_escape_text(&subtitle))
                .activatable(true)
                .build();
            row.add_css_class("emilia-flush");
            row.add_prefix(&crate::ui::app::cover_widget(
                cover.as_deref(),
                "microphone-symbolic",
            ));
            row.add_suffix(&self.episode_play_button(sender, &ep.audio_url, &ep.title));
            {
                let sender = sender.clone();
                let url = ep.audio_url.clone();
                let title = ep.title.clone();
                row.connect_activated(move |_| {
                    sender.input(Msg::ToggleEpisode {
                        url: url.clone(),
                        title: title.clone(),
                    });
                });
            }
            // Long press → episode detail (incl. shownotes).
            let lp = gtk::GestureLongPress::new();
            {
                let sender = sender.clone();
                lp.connect_pressed(move |g, _, _| {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    sender.input(Msg::ShowPodcastEpisodeDetail {
                        podcast_id: id,
                        index: i,
                    });
                });
            }
            row.add_controller(lp);
            group.add(&row);
        }
        content.append(&group);
        self.push_subpage(&gettext_f("Podcast – {title}", &[("title", title)]), &content);
        // Set the icons to the current playback state.
        self.refresh_episode_icons();
    }

    /// Dialog for subscribing: at the top a **search** (searches the iTunes
    /// podcast directory and shows tappable results), below it a field for the
    /// **feed address** (RSS) as the manual route. Both ultimately lead via
    /// [`Msg::PodcastSubscribeUrl`] to the usual subscription fetch.
    pub(crate) fn open_subscribe_podcast_dialog(
        &self,
        root: &adw::ApplicationWindow,
        sender: &ComponentSender<Self>,
    ) {
        let dialog = adw::Dialog::builder()
            .title(&gettext("Subscribe to podcast"))
            .build();
        self.adapt_detail_dialog(&dialog);
        let content = detail_box();

        // --- Search (iTunes directory) ---
        let search_group = adw::PreferencesGroup::builder()
            .title(&gettext("Search"))
            .description(&gettext("Find a podcast by name"))
            .build();
        let search_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .build();
        let search_entry = gtk::SearchEntry::builder()
            .placeholder_text(&gettext("Podcast name …"))
            .hexpand(true)
            .build();
        let search_btn = gtk::Button::builder().label(&gettext("Search")).build();
        search_btn.add_css_class("suggested-action");
        search_row.append(&search_entry);
        search_row.append(&search_btn);
        search_group.add(&search_row);
        content.append(&search_group);

        // Enter in the search field or clicking "Search" starts the search.
        {
            let (sender, entry) = (sender.clone(), search_entry.clone());
            search_entry.connect_activate(move |_| {
                let term = entry.text().to_string();
                if !term.trim().is_empty() {
                    sender.input(Msg::PodcastSearch(term));
                }
            });
        }
        {
            let (sender, entry) = (sender.clone(), search_entry.clone());
            search_btn.connect_clicked(move |_| {
                let term = entry.text().to_string();
                if !term.trim().is_empty() {
                    sender.input(Msg::PodcastSearch(term));
                }
            });
        }

        // Results list – initially empty/hidden, filled asynchronously by
        // `rebuild_podcast_search_results`.
        let results = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .build();
        results.add_css_class("boxed-list");
        results.set_visible(false);
        content.append(&results);

        // --- Manual: feed address (RSS) ---
        let url_group = adw::PreferencesGroup::builder()
            .title(&gettext("Or enter feed address"))
            .build();
        let url_entry = adw::EntryRow::builder()
            .title(&gettext("Feed address (RSS)"))
            .show_apply_button(true)
            .build();
        {
            let (sender, dialog) = (sender.clone(), dialog.clone());
            url_entry.connect_apply(move |e| {
                let url = e.text().to_string();
                if !url.trim().is_empty() {
                    sender.input(Msg::PodcastSubscribeUrl(url));
                    dialog.close();
                }
            });
        }
        url_group.add(&url_entry);
        content.append(&url_group);

        // Store dialog + results list so incoming results are drawn into the
        // open list; release it again when closing.
        *self.podcasts.podcast_search.borrow_mut() = Some((dialog.clone(), results.clone()));
        {
            let slot = self.podcasts.podcast_search.clone();
            dialog.connect_closed(move |_| {
                *slot.borrow_mut() = None;
            });
        }

        present_detail(&dialog, &content, root);
        search_entry.grab_focus();
    }

    /// Redraws the results list in the open subscription search dialog (from
    /// `self.podcasts.podcast_search_results`). Does nothing if the dialog is closed.
    /// Each result is tappable: tapping subscribes via the feed address and
    /// closes the dialog. Covers come from the local cache (otherwise a
    /// microphone placeholder).
    pub(crate) fn rebuild_podcast_search_results(&self, sender: &ComponentSender<Self>) {
        let guard = self.podcasts.podcast_search.borrow();
        let Some((dialog, list)) = guard.as_ref() else {
            return;
        };
        while let Some(child) = list.first_child() {
            list.remove(&child);
        }
        list.set_visible(true);

        if self.podcasts.podcast_search_results.is_empty() {
            let row = adw::ActionRow::builder().title(&gettext("No podcasts found")).build();
            row.set_sensitive(false);
            list.append(&row);
            // Compact height – only the search and address field plus a hint row.
            dialog.set_content_height(300);
            return;
        }

        // Make the dialog as tall as the results need (capped, then the list
        // scrolls). Roughly: fixed areas (header, search, address) + ~66 px
        // per result row.
        let rows = self.podcasts.podcast_search_results.len() as i32;
        dialog.set_content_height((320 + rows * 66).min(760));

        for r in &self.podcasts.podcast_search_results {
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&r.title))
                .activatable(true)
                .build();
            if let Some(a) = r.author.as_deref().filter(|a| !a.trim().is_empty()) {
                row.set_subtitle(&gtk::glib::markup_escape_text(a));
            }
            let cover = r.image_url.as_deref().and_then(crate::core::online::podcast_image_path);
            row.add_prefix(&crate::ui::app::cover_widget(cover.as_deref(), "microphone-symbolic"));
            row.add_suffix(&gtk::Image::from_icon_name("list-add-symbolic"));
            {
                let (sender, dialog, feed) =
                    (sender.clone(), dialog.clone(), r.feed_url.clone());
                row.connect_activated(move |_| {
                    sender.input(Msg::PodcastSubscribeUrl(feed.clone()));
                    dialog.close();
                });
            }
            list.append(&row);
        }
    }

    /// Play/Pause button (suffix) for an entry row: tap = toggle episode.
    /// Registered in `episode_play_buttons` so its icon can be updated when the
    /// playback state changes.
    fn episode_play_button(
        &self,
        sender: &ComponentSender<Self>,
        url: &str,
        title: &str,
    ) -> gtk::Button {
        let btn = gtk::Button::builder()
            .icon_name("media-playback-start-symbolic")
            .valign(gtk::Align::Center)
            .tooltip_text(&gettext("Play/Pause"))
            .build();
        btn.add_css_class("flat");
        {
            let (sender, url, title) = (sender.clone(), url.to_string(), title.to_string());
            btn.connect_clicked(move |_| {
                sender.input(Msg::ToggleEpisode {
                    url: url.clone(),
                    title: title.clone(),
                });
            });
        }
        self.podcasts.episode_play_buttons
            .borrow_mut()
            .push((url.to_string(), btn.clone()));
        btn
    }

    /// Updates the Play/Pause icons of all visible entry rows and the "Play"
    /// row of an open detail dialog. Detached rows (e.g. after leaving a
    /// subpage) are discarded in the process.
    pub(crate) fn refresh_episode_icons(&self) {
        let active = self.podcasts.playing_episode_url.clone();
        let playing = self.playing;
        let is_active = |url: &str| playing && active.as_deref() == Some(url);
        {
            let mut buttons = self.podcasts.episode_play_buttons.borrow_mut();
            buttons.retain(|(_, btn)| btn.root().is_some());
            for (url, btn) in buttons.iter() {
                btn.set_icon_name(if is_active(url) {
                    "media-playback-pause-symbolic"
                } else {
                    "media-playback-start-symbolic"
                });
            }
        }
        if let Some((row, url)) = self.podcasts.ctx_episode_play.borrow().as_ref() {
            row.set_visible(!is_active(url));
        }
    }

    /// Streams a podcast episode (replaces the current playback). Starts at
    /// the remembered position (resume) and first saves the position of a
    /// previously playing episode.
    pub(crate) fn play_episode(&mut self, url: &str, title: &str) {
        let resume = self.library.episode_progress(url).unwrap_or(0);
        self.play_episode_from(url, title, resume);
    }

    /// Like `play_episode`, but starts at a specific position (for the
    /// clickable jump markers in the shownotes).
    pub(crate) fn play_episode_at(&mut self, url: &str, title: &str, ms: i64) {
        self.play_episode_from(url, title, ms.max(0));
    }

    /// Sets the chapters of the current playback: seekbar markers **and** the
    /// shared chapter list for the hover display. Empty list = clear (e.g. for
    /// tracks without chapters). The markers reposition automatically once the
    /// duration is known (the tick updates the value range).
    pub(crate) fn set_chapters(&self, chapters: Vec<(i64, String)>) {
        self.seek_scale.clear_marks();
        for (ms, _) in &chapters {
            if *ms > 0 {
                self.seek_scale
                    .add_mark(*ms as f64, gtk::PositionType::Top, None);
            }
        }
        self.chapter_label.set_visible(false);
        *self.chapters.borrow_mut() = chapters;
    }

    /// Updates the chapter label to the chapter at the current playback
    /// position. No-op during a hover (then the mouse position takes
    /// precedence) and without chapters (the label stays hidden).
    pub(crate) fn update_current_chapter(&self) {
        if self.hovering_seek.get() {
            return;
        }
        let name = {
            let chaps = self.chapters.borrow();
            chaps
                .iter()
                .rev()
                .find(|(ms, _)| *ms <= self.position_ms)
                .map(|(_, n)| n.clone())
                .filter(|n| !n.is_empty())
        };
        match name {
            Some(n) => {
                self.chapter_label.set_text(&n);
                self.chapter_label.set_visible(true);
            }
            None => self.chapter_label.set_visible(false),
        }
    }

    fn play_episode_from(&mut self, url: &str, title: &str, resume: i64) {
        self.save_episode_progress();
        match self.player.play_uri(url, resume) {
            Ok(()) => {
                self.now_playing = Some(title.to_string());
                self.playing = true;
                self.playing_path = None;
                self.podcasts.playing_episode_url = Some(url.to_string());
                self.streaming.playing_stream = None;
                self.playing_remote = false;
                self.stop_recorder();
                self.queue.clear();
                self.queue_pos = 0;
                self.position_ms = resume.max(0);
                self.track_duration_ms = 0;
                *self.close_resume.borrow_mut() = None;
                self.mpris.set_metadata(0, title, None, None, None, None);
                self.mpris.set_playing(true);
                self.refresh_queue_icons();
                // Chapters (time + label) from the shownotes: set seekbar
                // markers and remember them for the hover display.
                let chapters = self
                    .library
                    .episode_description_by_url(url)
                    .ok()
                    .flatten()
                    .map(|d| crate::core::podcast::parse_chapters(&d))
                    .unwrap_or_default();
                self.set_chapters(chapters);
                // Show the current chapter (at the resume/start position) immediately.
                self.update_current_chapter();
            }
            Err(e) => tracing::error!("Failed to play episode: {e}"),
        }
    }
}
