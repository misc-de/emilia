//! List half of [`PodcastsPage`]'s inherent impl: the header sort control,
//! the subscription overview (list + gallery), the "Newest" and "Recently
//! played" episode lists and their progress lines. The struct, its messages,
//! the `Component` impl and the shared helpers stay in
//! [`crate::ui::podcasts_page`].

use std::collections::HashMap;

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::i18n::{gettext, gettext_f};
use crate::ui::app::{PodcastView, SortCrit};
use crate::ui::app_gallery::{gallery_cell, spawn_gallery_decode};
use crate::ui::app_helpers::{cover_widget, fill_progress_row, on_long_press, on_secondary_click};
use crate::ui::app_sort::sort_popover;
use crate::ui::app_views::natural_key;
use crate::ui::podcasts_page::{EpisodeRow, PodcastsInput, PodcastsOutput, PodcastsPage};

impl PodcastsPage {
    /// (Re)builds the header sort button: its direction icon and the criteria
    /// popover (name / episode count / latest episode) plus the grouping + gallery toggles. Called
    /// on init and whenever the sort/grouping/gallery changes.
    pub(super) fn rebuild_sort(&self, sender: &ComponentSender<Self>) {
        use crate::ui::app_sort::SortToggle;
        let (crit, desc) = self.overview_sort;
        let crits = [
            (SortCrit::Name, gettext("Name")),
            (SortCrit::Songs, gettext("Number of episodes")),
            (SortCrit::Release, gettext("Latest episode")),
        ];
        let input = sender.input_sender().clone();
        let group_input = input.clone();
        let gallery_input = input.clone();
        let desc_input = input.clone();
        let toggles = vec![
            SortToggle {
                label: gettext("Without grouping"),
                active: self.overview_no_group,
                on_toggle: Box::new(move |off| {
                    let _ = group_input.send(PodcastsInput::SetNoGroup(off));
                }),
                sub: false,
            },
            SortToggle {
                label: gettext("Gallery view"),
                active: self.gallery_on(),
                on_toggle: Box::new(move |on| {
                    let _ = gallery_input.send(PodcastsInput::SetGallery(on));
                }),
                sub: false,
            },
            SortToggle {
                label: gettext("Show description"),
                active: self.gallery_desc,
                on_toggle: Box::new(move |on| {
                    let _ = desc_input.send(PodcastsInput::SetGalleryDesc(on));
                }),
                sub: true,
            },
        ];
        let popover = sort_popover(
            &crits,
            crit,
            desc,
            move |crit, desc| {
                let _ = input.send(PodcastsInput::SetSort(crit, desc));
            },
            toggles,
        );
        // Only the subscription overview (with at least one entry) sorts; hand the
        // popover up to the shared title-bar button, or hide it otherwise.
        let visible = self.podcast_view == PodcastView::Overview && !self.podcast_items.is_empty();
        *self.sort_slot.borrow_mut() = visible.then_some((popover, desc));
        let _ = sender.output(PodcastsOutput::SortChanged);
    }

    /// Per-row alphabetical headings (by name) for the overview list; none for the
    /// episode-count sort or when grouping is off.
    fn overview_section_headers(&self) -> Option<Vec<String>> {
        if self.overview_no_group {
            return None;
        }
        match self.overview_sort.0 {
            SortCrit::Name => Some(
                self.podcast_items
                    .iter()
                    .map(|(_, title, _, _)| crate::ui::app_sort::alpha_header(title))
                    .collect(),
            ),
            _ => None,
        }
    }

    /// Orders the subscription overview by the chosen sort (shared by list +
    /// gallery, which both read `podcast_items`).
    fn sort_podcasts(&mut self) {
        let (crit, desc) = self.overview_sort;
        match crit {
            SortCrit::Songs => self.podcast_items.sort_by_key(|(_, _, _, count)| *count),
            // By the publication date of each podcast's newest episode.
            SortCrit::Release => {
                let mut latest: HashMap<i64, i64> = HashMap::new();
                for (id, published) in self.library.episode_pubdates().unwrap_or_default() {
                    let key = crate::core::podcast::pubdate_key(published.as_deref());
                    let e = latest.entry(id).or_insert(0);
                    *e = (*e).max(key);
                }
                self.podcast_items
                    .sort_by_key(|(id, _, _, _)| latest.get(id).copied().unwrap_or(0));
            }
            // Name is the remaining criterion.
            _ => self
                .podcast_items
                .sort_by_cached_key(|(_, title, _, _)| natural_key(title)),
        }
        if desc {
            self.podcast_items.reverse();
        }
    }

    pub(super) fn reload_podcasts(&mut self, sender: &ComponentSender<Self>) {
        self.podcast_items = self.library.podcasts().unwrap_or_default();
        self.sort_podcasts();
        *self.overview_headers.borrow_mut() = self.overview_section_headers();
        if self.gallery_on() {
            self.fill_podcast_gallery(sender);
        } else {
            while let Some(child) = self.podcasts_list.first_child() {
                self.podcasts_list.remove(&child);
            }
            for (id, title, image, count) in self.podcast_items.clone() {
                // Episode count in parentheses on the heading, as with albums/songs.
                let row = adw::ActionRow::builder()
                    .title(format!("{} ({count})", gtk::glib::markup_escape_text(&title)).as_str())
                    .activatable(true)
                    .build();
                row.add_css_class("emilia-flush");
                let cover = image
                    .as_deref()
                    .and_then(crate::core::online::podcast_image_path);
                row.add_prefix(&cover_widget(cover.as_deref(), "microphone-symbolic"));
                {
                    let sender = sender.clone();
                    row.connect_activated(move |_| sender.input(PodcastsInput::OpenPodcast(id)));
                }
                // Long press (touch) / right click (mouse) → subscription detail view.
                on_secondary_click(&row, {
                    let sender = sender.clone();
                    move || sender.input(PodcastsInput::ShowPodcastDetail(id))
                });
                let lp = gtk::GestureLongPress::new();
                {
                    let sender = sender.clone();
                    lp.connect_pressed(move |g, _, _| {
                        g.set_state(gtk::EventSequenceState::Claimed);
                        sender.input(PodcastsInput::ShowPodcastDetail(id));
                    });
                }
                row.add_controller(lp);
                self.podcasts_list.append(&row);
            }
            self.podcasts_list.invalidate_headers();
        }
        self.reload_newest(sender);
        self.reload_recent(sender);
        // The overview's contents (and thus the sort button's visibility) may
        // have changed → refresh the title-bar sort control.
        self.rebuild_sort(sender);
    }

    /// Gallery variant of the podcast overview: cover grid; tap opens the
    /// episodes, long-press the subscription detail view.
    fn fill_podcast_gallery(&self, sender: &ComponentSender<Self>) {
        let fb = &self.podcasts_gallery;
        crate::ui::widgets::reset_gallery_grid(fb, self.gallery_columns);

        let mut to_decode: Vec<(String, gtk::Picture)> = Vec::new();
        for (i, (_, title, image, _)) in self.podcast_items.iter().enumerate() {
            let cover = image
                .as_deref()
                .and_then(crate::core::online::podcast_image_path);
            let (cell, pic) = gallery_cell(
                cover.as_deref(),
                "microphone-symbolic",
                title,
                self.gallery_desc,
            );
            if let (Some(path), Some(pic)) = (cover.as_deref(), pic)
                && crate::ui::widgets::cached_thumb(path).is_none()
            {
                to_decode.push((path.to_string(), pic));
            }
            let click = gtk::GestureClick::new();
            {
                let sender = sender.clone();
                click.connect_released(move |g, n, _, _| {
                    if n == 1 {
                        g.set_state(gtk::EventSequenceState::Claimed);
                        sender.input(PodcastsInput::OpenPodcastAt(i));
                    }
                });
            }
            cell.add_controller(click);
            on_secondary_click(&cell, {
                let sender = sender.clone();
                move || sender.input(PodcastsInput::ShowPodcastDetailAt(i))
            });
            let long_press = gtk::GestureLongPress::new();
            {
                let sender = sender.clone();
                long_press.connect_pressed(move |g, _, _| {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    sender.input(PodcastsInput::ShowPodcastDetailAt(i));
                });
            }
            cell.add_controller(long_press);
            fb.append(&cell);
        }

        spawn_gallery_decode(to_decode);
    }

    /// Builds the "Newest" list: newest episodes across **all** subscriptions,
    /// chronologically by publication date. The **play button** streams the
    /// episode; **long press / right click** opens the entry detail view.
    pub(super) fn reload_newest(&mut self, sender: &ComponentSender<Self>) {
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
        self.newest_items = eps;
        // Resume positions + finished flags of *all* episodes in one query — a
        // per-row lookup would mean 150 statements for a list this long.
        let progress: HashMap<String, (i64, bool)> = self
            .library
            .all_episode_progress()
            .unwrap_or_default()
            .into_iter()
            .map(|(url, pos, fin)| (url, (pos, fin)))
            .collect();
        while let Some(child) = self.newest_list.first_child() {
            self.newest_list.remove(&child);
        }

        // Sort by recency: Today / Yesterday / This week / This month.
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
        for (i, ep) in self.newest_items.iter().enumerate() {
            let b = bucket_of(crate::core::podcast::pubdate_key(ep.published.as_deref()));
            if cur_bucket != Some(b) {
                cur_bucket = Some(b);
                let g = adw::PreferencesGroup::builder()
                    .title(bucket_title(b))
                    .build();
                self.newest_list.append(&g);
                group = Some(g);
            }

            let (position_ms, finished) =
                progress.get(&ep.audio_url).copied().unwrap_or((0, false));
            let total_secs = ep
                .duration
                .as_deref()
                .and_then(crate::core::podcast::duration_secs)
                .filter(|s| *s > 0);

            let mut subtitle = ep.podcast_title.clone();
            if let Some(p) = ep.published.as_deref().filter(|p| !p.trim().is_empty()) {
                subtitle.push_str(" · ");
                subtitle.push_str(&crate::core::podcast::pubdate_short(p));
            }
            // Listening progress like "Recently": the elapsed time before a bar,
            // but only once more than 10 s have actually been listened to (or a
            // check once finished). When the feed states no length there is no
            // bar, so the elapsed time is appended to the subtitle instead.
            let heard = position_ms > 10_000;
            if heard && total_secs.is_none() && !finished {
                subtitle.push_str(" · ");
                subtitle.push_str(&gettext_f(
                    "{position} listened",
                    &[(
                        "position",
                        &crate::ui::app_helpers::fmt_duration(position_ms),
                    )],
                ));
            }

            // Not activatable: like a library track, the episode plays via its
            // play button; long press / right click opens the detail view.
            let cover = ep
                .podcast_image
                .as_deref()
                .and_then(crate::core::online::podcast_image_path);
            let card = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .build();
            // Laid out like an `emilia-flush` `AdwActionRow` (cover flush left,
            // 8 px to the text) so the row matches the streaming/album lists.
            let top = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(8)
                .margin_top(3)
                .margin_bottom(3)
                .margin_start(3)
                .margin_end(12)
                .build();
            top.append(&cover_widget(cover.as_deref(), "microphone-symbolic"));
            let text = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .hexpand(true)
                .valign(gtk::Align::Center)
                .build();
            let title = gtk::Label::builder()
                .label(&ep.title)
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .build();
            text.append(&title);
            let subtitle_lbl = gtk::Label::builder()
                .label(&subtitle)
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .build();
            subtitle_lbl.add_css_class("dim-label");
            text.append(&subtitle_lbl);
            // Listening progress like "Recently": finished shows a check;
            // otherwise the elapsed time before a bar, once > 10 s were listened
            // to and the feed states a length. Always built (hidden while there
            // is nothing to show) so the tick can fill it in as it plays.
            text.append(&self.episode_progress_row(
                &ep.audio_url,
                position_ms,
                total_secs,
                finished,
            ));
            top.append(&text);
            // Episode length as a subtle label, left of the play button.
            if let Some(d) = ep
                .duration
                .as_deref()
                .and_then(crate::core::podcast::format_duration)
            {
                let lbl = gtk::Label::new(Some(&d));
                lbl.set_valign(gtk::Align::Center);
                lbl.set_css_classes(&["dim-label", "numeric"]);
                top.append(&lbl);
            }
            top.append(&self.episode_play_button(sender, &ep.audio_url, &ep.title));
            card.append(&top);
            on_secondary_click(&card, {
                let sender = sender.clone();
                move || sender.input(PodcastsInput::ShowEpisodeDetail(i))
            });
            on_long_press(&card, {
                let sender = sender.clone();
                move || sender.input(PodcastsInput::ShowEpisodeDetail(i))
            });
            if let Some(g) = &group {
                g.add(&crate::ui::app_helpers::card_row(&card));
            }
        }
        self.refresh_episode_icons();
    }

    /// The progress line for a podcast episode, registered under `url` so the
    /// per-second transport tick can keep it live while the episode plays (see
    /// [`Self::apply_episode_progress`]). Always built — an episode that has not
    /// been started yet gets an empty, hidden row that fills in as it plays.
    fn episode_progress_row(
        &self,
        url: &str,
        position_ms: i64,
        total_secs: Option<i64>,
        finished: bool,
    ) -> gtk::Box {
        let prow = crate::ui::app_helpers::progress_row_box();
        fill_progress_row(&prow, position_ms, total_secs, finished);
        self.episode_progress_rows.borrow_mut().push(EpisodeRow {
            url: url.to_string(),
            row: prow.clone(),
            total_secs,
        });
        prow
    }

    /// Builds the "Recently" list: episodes you have started (those with a
    /// stored playback position), newest first, each with a progress bar that
    /// visualizes how far you have already listened. The play button resumes;
    /// long press / right click opens the episode detail.
    pub(super) fn reload_recent(&mut self, sender: &ComponentSender<Self>) {
        self.recent_items = self.library.recent_episodes(150).unwrap_or_default();
        while let Some(child) = self.recent_list.first_child() {
            self.recent_list.remove(&child);
        }
        // One group for the whole list, so the rows share a single card with
        // separators — the same look the streaming/album lists have. Only
        // attached when there is something to show, so an empty list stays empty
        // (the icon refresh below still has to run either way).
        let group = adw::PreferencesGroup::new();
        if !self.recent_items.is_empty() {
            self.recent_list.append(&group);
        }
        for ep in self.recent_items.clone() {
            let total_secs = ep
                .duration
                .as_deref()
                .and_then(crate::core::podcast::duration_secs)
                .filter(|s| *s > 0);

            let card = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(6)
                .build();
            let top = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(8)
                .margin_top(3)
                .margin_bottom(3)
                .margin_start(3)
                .margin_end(12)
                .build();
            let cover = ep
                .podcast_image
                .as_deref()
                .and_then(crate::core::online::podcast_image_path);
            top.append(&cover_widget(cover.as_deref(), "microphone-symbolic"));

            let text = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .hexpand(true)
                .valign(gtk::Align::Center)
                .build();
            let title = gtk::Label::builder()
                .label(&ep.title)
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .build();
            text.append(&title);
            // Subtitle: just the podcast name — the total length sits next to
            // the play button (like "Newest"). Without a known length there is
            // no bar, so the elapsed time is shown here instead (unless finished,
            // where the line below already says "Listened").
            let mut sub = ep.podcast_title.clone();
            if total_secs.is_none() && !ep.finished {
                sub.push_str(" · ");
                sub.push_str(&gettext_f(
                    "{position} listened",
                    &[(
                        "position",
                        &crate::ui::app_helpers::fmt_duration(ep.position_ms),
                    )],
                ));
            }
            let subtitle = gtk::Label::builder()
                .label(&sub)
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .build();
            subtitle.add_css_class("dim-label");
            text.append(&subtitle);

            // Progress line inside the text column, so it spans only the text
            // width — not under the cover or the play button. Finished (or < 30 s
            // left) shows a check; otherwise the elapsed time before a bar.
            text.append(&self.episode_progress_row(
                &ep.audio_url,
                ep.position_ms,
                total_secs,
                ep.finished,
            ));
            top.append(&text);

            // Episode length as a subtle label, left of the play button — the
            // same placement as in "Newest".
            if let Some(d) = ep
                .duration
                .as_deref()
                .and_then(crate::core::podcast::format_duration)
            {
                let lbl = gtk::Label::new(Some(&d));
                lbl.set_valign(gtk::Align::Center);
                lbl.set_css_classes(&["dim-label", "numeric"]);
                top.append(&lbl);
            }
            top.append(&self.episode_play_button(sender, &ep.audio_url, &ep.title));
            card.append(&top);

            let url = ep.audio_url.clone();
            on_secondary_click(&card, {
                let sender = sender.clone();
                let url = url.clone();
                move || sender.input(PodcastsInput::ShowEpisodeDetailByUrl { url: url.clone() })
            });
            on_long_press(&card, {
                let sender = sender.clone();
                move || sender.input(PodcastsInput::ShowEpisodeDetailByUrl { url: url.clone() })
            });
            group.add(&crate::ui::app_helpers::card_row(&card));
        }
        self.refresh_episode_icons();
    }
}
