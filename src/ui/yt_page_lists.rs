//! The list half of [`YtPage`]'s inherent impl: the title-bar sort control,
//! the subscriptions overview (list + gallery), the "Newest" and "Recent"
//! lists and the sort/grouping/gallery preference handlers. The GTK-free
//! ordering, grouping and subtitle logic behind them lives in free functions
//! at the bottom of this file (unit-tested).

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use std::collections::HashMap;

use crate::core::db::Library;
use crate::core::youtube;
use crate::i18n::{gettext, gettext_f, ngettext_n};
use crate::model::{YtRecent, YtVideoRef};
use crate::ui::app::{SortCrit, YtView};
use crate::ui::app_gallery::{gallery_cell, spawn_gallery_decode};
use crate::ui::app_helpers::{cover_widget, on_long_press, on_secondary_click};
use crate::ui::app_sort::sort_popover;
use crate::ui::app_views::natural_key;
use crate::ui::yt_channels::{fmt_duration, fmt_published, yt_pubdate_key};
use crate::ui::yt_page::{YtInput, YtOutput, YtPage};

/// (id, title, url, thumbnail, video count) of a subscribed channel, as
/// [`Library::channels`] returns it.
pub(super) type ChannelItem = (i64, String, String, Option<String>, i64);

/// How many videos the "Newest" list shows (across all channels).
const NEWEST_LIMIT: usize = 150;

impl YtPage {
    /// Effective gallery mode for the channels overview: the per-view override if
    /// set, else the global `gallery_view`.
    pub(super) fn gallery_on(&self) -> bool {
        self.gallery_override.unwrap_or(self.gallery_view)
    }

    /// (Re)builds the header sort button: direction icon + criteria popover
    /// (name / video count / latest video) plus the grouping + gallery toggles. Called on init
    /// and whenever the sort/grouping/gallery changes.
    pub(super) fn rebuild_sort(&self, sender: &ComponentSender<Self>) {
        use crate::ui::app_sort::SortToggle;
        let input = sender.input_sender().clone();
        // Subscriptions and Recent both sort; Newest stays date-grouped (no sort).
        let slot = match self.yt_view {
            YtView::Channels => {
                let (crit, desc) = self.channels_sort;
                let crits = [
                    (SortCrit::Name, gettext("Name")),
                    (SortCrit::Songs, gettext("Number of videos")),
                    (SortCrit::Release, gettext("Latest video")),
                ];
                let group_input = input.clone();
                let gallery_input = input.clone();
                let desc_input = input.clone();
                let toggles = vec![
                    SortToggle {
                        label: gettext("Without grouping"),
                        active: self.channels_no_group,
                        on_toggle: Box::new(move |off| {
                            let _ = group_input.send(YtInput::SetNoGroup(off));
                        }),
                        sub: false,
                    },
                    SortToggle {
                        label: gettext("Gallery view"),
                        active: self.gallery_on(),
                        on_toggle: Box::new(move |on| {
                            let _ = gallery_input.send(YtInput::SetGallery(on));
                        }),
                        sub: false,
                    },
                    SortToggle {
                        label: gettext("Show description"),
                        active: self.gallery_desc,
                        on_toggle: Box::new(move |on| {
                            let _ = desc_input.send(YtInput::SetGalleryDesc(on));
                        }),
                        sub: true,
                    },
                ];
                let popover = sort_popover(
                    &crits,
                    crit,
                    desc,
                    move |crit, desc| {
                        let _ = input.send(YtInput::SetSort(crit, desc));
                    },
                    toggles,
                );
                (!self.channel_items.is_empty()).then_some((popover, desc))
            }
            YtView::Recent => {
                let (crit, desc) = self.recent_sort;
                // Recent is a flat list (no grouping / gallery): name, date, length.
                let crits = [
                    (SortCrit::Name, gettext("Name")),
                    (SortCrit::Release, gettext("Date")),
                    (SortCrit::Length, gettext("Length")),
                ];
                let popover = sort_popover(
                    &crits,
                    crit,
                    desc,
                    move |crit, desc| {
                        let _ = input.send(YtInput::SetRecentSort(crit, desc));
                    },
                    vec![],
                );
                (!self.recent_items.is_empty()).then_some((popover, desc))
            }
            YtView::Newest | YtView::Live => None,
        };
        *self.sort_slot.borrow_mut() = slot;
        let _ = sender.output(YtOutput::SortChanged);
    }

    /// Header sort of the subscriptions changed → persist it and rebuild.
    pub(super) fn on_set_channels_sort(
        &mut self,
        sender: &ComponentSender<Self>,
        crit: SortCrit,
        desc: bool,
    ) {
        if self.channels_sort != (crit, desc) {
            self.channels_sort = (crit, desc);
            let _ = self.library.set_setting("sort_channels", crit.as_key());
            let _ = self
                .library
                .set_setting("sort_channels_desc", setting_flag(desc));
            self.reload_channels(sender);
        }
    }

    /// Header sort of the "Recent" list changed → persist it and rebuild.
    pub(super) fn on_set_recent_sort(
        &mut self,
        sender: &ComponentSender<Self>,
        crit: SortCrit,
        desc: bool,
    ) {
        if self.recent_sort != (crit, desc) {
            self.recent_sort = (crit, desc);
            let _ = self.library.set_setting("sort_yt_recent", crit.as_key());
            let _ = self
                .library
                .set_setting("sort_yt_recent_desc", setting_flag(desc));
            self.reload_yt_recent(sender);
        }
    }

    /// "Without grouping" toggled for the channels list.
    pub(super) fn on_set_no_group(&mut self, sender: &ComponentSender<Self>, off: bool) {
        if self.channels_no_group != off {
            self.channels_no_group = off;
            let _ = self
                .library
                .set_setting("nogroup_channels", setting_flag(off));
            self.reload_channels(sender);
        }
    }

    /// Per-view gallery override of the channels toggled.
    pub(super) fn on_set_gallery(&mut self, sender: &ComponentSender<Self>, on: bool) {
        if self.gallery_override != Some(on) {
            self.gallery_override = Some(on);
            let _ = self
                .library
                .set_setting("gallery_channels", setting_flag(on));
            self.reload_channels(sender);
        }
    }

    /// "Show description" (gallery tile titles) toggled.
    pub(super) fn on_set_gallery_desc(&mut self, sender: &ComponentSender<Self>, on: bool) {
        if self.gallery_desc != on {
            self.gallery_desc = on;
            let _ = self
                .library
                .set_setting("gallery_desc_channels", setting_flag(on));
            self.reload_channels(sender);
        }
    }

    /// Rebuilds the channel overview (+ "Newest"/"Recent" lists).
    pub(super) fn reload_channels(&mut self, sender: &ComponentSender<Self>) {
        self.channel_items = self.library.channels().unwrap_or_default();
        let (crit, desc) = self.channels_sort;
        // By the publication date of each channel's newest video (only the
        // date sort needs that query).
        let latest = if crit == SortCrit::Release {
            latest_pubdate_by_channel(self.library.video_pubdates().unwrap_or_default())
        } else {
            HashMap::new()
        };
        sort_channel_items(&mut self.channel_items, crit, desc, &latest);
        // Refresh the title-bar sort control (visibility depends on emptiness).
        self.rebuild_sort(sender);
        *self.channel_headers.borrow_mut() =
            channel_section_headers(&self.channel_items, crit, self.channels_no_group);
        if self.gallery_on() {
            self.fill_yt_gallery(sender);
        } else {
            while let Some(child) = self.channels_list.first_child() {
                self.channels_list.remove(&child);
            }
            for (id, title, _url, thumb, count) in self.channel_items.clone() {
                let row = adw::ActionRow::builder()
                    .title(count_title(&title, count).as_str())
                    .activatable(true)
                    .build();
                row.add_css_class("emilia-flush");
                let cover = thumb
                    .as_deref()
                    .and_then(crate::core::online::youtube_thumb_path);
                row.add_prefix(&cover_widget(cover.as_deref(), "avatar-default-symbolic"));
                {
                    let sender = sender.clone();
                    row.connect_activated(move |_| sender.input(YtInput::OpenChannel(id)));
                }
                on_secondary_click(&row, {
                    let sender = sender.clone();
                    move || sender.input(YtInput::ShowChannelDetail(id))
                });
                let lp = gtk::GestureLongPress::new();
                {
                    let sender = sender.clone();
                    lp.connect_pressed(move |g, _, _| {
                        g.set_state(gtk::EventSequenceState::Claimed);
                        sender.input(YtInput::ShowChannelDetail(id));
                    });
                }
                row.add_controller(lp);
                self.channels_list.append(&row);
            }
            self.channels_list.invalidate_headers();
        }
        self.reload_yt_newest(sender);
        self.reload_yt_recent(sender);
    }

    /// Gallery variant of the channel overview (thumbnail grid).
    fn fill_yt_gallery(&self, sender: &ComponentSender<Self>) {
        let fb = &self.channels_gallery;
        crate::ui::widgets::reset_gallery_grid(fb, self.gallery_columns);
        let mut to_decode: Vec<(String, gtk::Picture)> = Vec::new();
        for (i, (_, title, _, thumb, _)) in self.channel_items.iter().enumerate() {
            let cover = thumb
                .as_deref()
                .and_then(crate::core::online::youtube_thumb_path);
            let (cell, pic) = gallery_cell(
                cover.as_deref(),
                "avatar-default-symbolic",
                title,
                self.gallery_desc,
            );
            if let (Some(path), Some(pic)) = (cover.as_deref(), pic) {
                if crate::ui::widgets::cached_thumb(path).is_none() {
                    to_decode.push((path.to_string(), pic));
                }
            }
            let click = gtk::GestureClick::new();
            {
                let sender = sender.clone();
                click.connect_released(move |g, n, _, _| {
                    if n == 1 {
                        g.set_state(gtk::EventSequenceState::Claimed);
                        sender.input(YtInput::OpenChannelAt(i));
                    }
                });
            }
            cell.add_controller(click);
            on_secondary_click(&cell, {
                let sender = sender.clone();
                move || sender.input(YtInput::ShowChannelDetailAt(i))
            });
            let long_press = gtk::GestureLongPress::new();
            {
                let sender = sender.clone();
                long_press.connect_pressed(move |g, _, _| {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    sender.input(YtInput::ShowChannelDetailAt(i));
                });
            }
            cell.add_controller(long_press);
            fb.append(&cell);
        }
        spawn_gallery_decode(to_decode);
    }

    /// Builds the "Newest videos" list across all subscribed channels.
    fn reload_yt_newest(&mut self, sender: &ComponentSender<Self>) {
        self.newest_items = newest_videos(self.library.all_videos().unwrap_or_default());
        while let Some(child) = self.newest_list.first_child() {
            self.newest_list.remove(&child);
        }
        if self.newest_items.is_empty() {
            return;
        }
        let (today, yesterday, week_start) = crate::core::podcast::recent_day_buckets();
        let buckets = DayBuckets {
            today,
            yesterday,
            week_start,
            month_start: crate::core::podcast::recent_cutoff_key(),
        };
        // Stored watch positions in one query, instead of one per row.
        let watched = self.library.all_yt_progress().unwrap_or_default();
        let mut cur_bucket: Option<usize> = None;
        let mut group: Option<adw::PreferencesGroup> = None;
        for (i, v) in self.newest_items.iter().enumerate() {
            let b = buckets.bucket_of(yt_pubdate_key(v.published.as_deref()));
            if cur_bucket != Some(b) {
                cur_bucket = Some(b);
                let g = adw::PreferencesGroup::builder()
                    .title(newest_bucket_title(b))
                    .build();
                self.newest_list.append(&g);
                group = Some(g);
            }
            let subtitle = newest_subtitle(&v.channel_title, v.published.as_deref());
            let cover = crate::core::online::youtube_cover_path(&v.video_id)
                .or_else(|| {
                    crate::core::online::youtube_thumb_path(&youtube::thumbnail_url(&v.video_id))
                })
                .or_else(|| {
                    v.channel_thumb
                        .as_deref()
                        .and_then(crate::core::online::youtube_thumb_path)
                });
            let row = self.video_card(
                sender,
                &v.video_id,
                &v.title,
                &subtitle,
                cover.as_deref(),
                v.duration,
                watched.get(&v.video_id),
                {
                    let sender = sender.clone();
                    move || sender.input(YtInput::ShowNewestDetail(i))
                },
            );
            if let Some(g) = &group {
                g.add(&row);
            }
        }
        self.refresh_yt_icons();
    }

    /// Builds the "Recent" list (recently played videos/playlists, newest first).
    pub(super) fn reload_yt_recent(&mut self, sender: &ComponentSender<Self>) {
        self.recent_items = self.library.recent_videos(150).unwrap_or_default();
        let (crit, desc) = self.recent_sort;
        sort_recent(&mut self.recent_items, crit, desc);
        // Refresh the title-bar sort control (visibility depends on emptiness);
        // before the early-return below so the empty case hides it too.
        self.rebuild_sort(sender);
        while let Some(child) = self.recent_list.first_child() {
            self.recent_list.remove(&child);
        }
        if self.recent_items.is_empty() {
            return;
        }
        let watched = self.library.all_yt_progress().unwrap_or_default();
        let group = adw::PreferencesGroup::new();
        for r in &self.recent_items {
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&r.title))
                .activatable(true)
                .build();
            row.add_css_class("emilia-flush");
            if r.kind == "playlist" {
                row.set_subtitle(&recent_playlist_subtitle(r.count, r.total_duration));
                let cover = r.thumbnail.as_deref().and_then(|t| {
                    if std::path::Path::new(t).exists() {
                        Some(t.to_string())
                    } else {
                        crate::core::online::youtube_thumb_path(t)
                    }
                });
                row.add_prefix(&cover_widget(cover.as_deref(), "view-list-symbolic"));
                let btn = gtk::Button::builder()
                    .icon_name("media-playback-start-symbolic")
                    .valign(gtk::Align::Center)
                    .tooltip_text(gettext("Start Playlist"))
                    .build();
                btn.add_css_class("flat");
                {
                    let (sender, url, t) = (sender.clone(), r.video_id.clone(), r.title.clone());
                    btn.connect_clicked(move |_| {
                        let _ = sender.output(YtOutput::StartPlaylist {
                            url: url.clone(),
                            title: t.clone(),
                        });
                    });
                }
                row.add_suffix(&btn);
                {
                    let (sender, url, t) = (sender.clone(), r.video_id.clone(), r.title.clone());
                    row.connect_activated(move |_| {
                        sender.input(YtInput::OpenRecentPlaylist {
                            url: url.clone(),
                            title: t.clone(),
                        });
                    });
                }
                on_secondary_click(&row, {
                    let (sender, url, t) = (sender.clone(), r.video_id.clone(), r.title.clone());
                    move || {
                        sender.input(YtInput::ShowPlaylistDetail {
                            url: url.clone(),
                            title: t.clone(),
                        });
                    }
                });
                on_long_press(&row, {
                    let (sender, url, t) = (sender.clone(), r.video_id.clone(), r.title.clone());
                    move || {
                        sender.input(YtInput::ShowPlaylistDetail {
                            url: url.clone(),
                            title: t.clone(),
                        })
                    }
                });
                group.add(&row);
                continue;
            }
            let cover = crate::core::online::youtube_cover_path(&r.video_id).or_else(|| {
                crate::core::online::youtube_thumb_path(&youtube::thumbnail_url(&r.video_id))
            });
            let card = self.video_card(
                sender,
                &r.video_id,
                &r.title,
                r.artist.as_deref().unwrap_or_default(),
                cover.as_deref(),
                r.duration,
                watched.get(&r.video_id),
                {
                    let (sender, vid, t) = (sender.clone(), r.video_id.clone(), r.title.clone());
                    move || {
                        sender.input(YtInput::ShowVideoDetail {
                            video_id: vid.clone(),
                            title: t.clone(),
                        });
                    }
                },
            );
            group.add(&card);
        }
        self.recent_list.append(&group);
        self.refresh_yt_icons();
    }
}

/// Persisted channels-overview choices: (without grouping, per-view gallery
/// override, show gallery description). Read once on init.
pub(super) fn read_channel_view_prefs(library: &Library) -> (bool, Option<bool>, bool) {
    let get = |key: &str| library.get_setting(key).ok().flatten();
    (
        parse_no_group(get("nogroup_channels").as_deref()),
        parse_gallery_override(get("gallery_channels").as_deref()),
        parse_gallery_desc(get("gallery_desc_channels").as_deref()),
    )
}

/// Settings value of a boolean preference (`"1"` / `"0"`).
fn setting_flag(on: bool) -> &'static str {
    if on {
        "1"
    } else {
        "0"
    }
}

/// "nogroup_channels": grouping is only off when explicitly stored as `"1"`.
fn parse_no_group(value: Option<&str>) -> bool {
    matches!(value, Some("1"))
}

/// "gallery_channels": `"1"`/`"0"` force the gallery on/off; anything else
/// (unset, garbage) follows the global gallery setting.
fn parse_gallery_override(value: Option<&str>) -> Option<bool> {
    match value {
        Some("1") => Some(true),
        Some("0") => Some(false),
        _ => None,
    }
}

/// "gallery_desc_channels": the tile titles show unless explicitly off (`"0"`).
fn parse_gallery_desc(value: Option<&str>) -> bool {
    value != Some("0")
}

/// `"<title> (<n>)"` with the title markup-escaped — the heading/row title of a
/// channel or playlist next to its item count.
pub(super) fn count_title(title: &str, n: impl std::fmt::Display) -> String {
    format!("{} ({n})", gtk::glib::markup_escape_text(title))
}

/// Orders the "Recent" list by the chosen sort. "Date" keeps the DB order
/// (recently played first), reversing it for ascending; the others sort by
/// title or runtime (videos use `duration`, playlists `total_duration`).
fn sort_recent(items: &mut [YtRecent], crit: SortCrit, desc: bool) {
    match crit {
        SortCrit::Name => items.sort_by_cached_key(|r| natural_key(&r.title)),
        SortCrit::Length => items.sort_by_key(|r| r.duration.or(r.total_duration).unwrap_or(0)),
        // Date (Release): the query already returns `played_at` descending.
        _ => {
            if !desc {
                items.reverse();
            }
            return;
        }
    }
    if desc {
        items.reverse();
    }
}

/// Newest publication key ([`yt_pubdate_key`]) per channel id, from
/// `(channel id, published)` pairs of all cached videos.
fn latest_pubdate_by_channel(pubdates: Vec<(i64, Option<String>)>) -> HashMap<i64, i64> {
    let mut latest: HashMap<i64, i64> = HashMap::new();
    for (id, published) in pubdates {
        let key = yt_pubdate_key(published.as_deref());
        let e = latest.entry(id).or_insert(0);
        *e = (*e).max(key);
    }
    latest
}

/// Orders the subscriptions overview by the chosen sort (shared by list +
/// gallery, which both read `channel_items`). `latest` is only consulted for
/// the date sort (see [`latest_pubdate_by_channel`]).
fn sort_channel_items(
    items: &mut [ChannelItem],
    crit: SortCrit,
    desc: bool,
    latest: &HashMap<i64, i64>,
) {
    match crit {
        SortCrit::Songs => items.sort_by_key(|(_, _, _, _, count)| *count),
        // By the publication date of each channel's newest video.
        SortCrit::Release => {
            items.sort_by_key(|(id, _, _, _, _)| latest.get(id).copied().unwrap_or(0))
        }
        // Name is the remaining criterion.
        _ => items.sort_by_cached_key(|(_, title, _, _, _)| natural_key(title)),
    }
    if desc {
        items.reverse();
    }
}

/// Per-row alphabetical headings (by name) for the channels list; none for the
/// video-count sort or when grouping is off.
fn channel_section_headers(
    items: &[ChannelItem],
    crit: SortCrit,
    no_group: bool,
) -> Option<Vec<String>> {
    if no_group {
        return None;
    }
    match crit {
        SortCrit::Name => Some(
            items
                .iter()
                .map(|(_, title, _, _, _)| crate::ui::app_sort::alpha_header(title))
                .collect(),
        ),
        _ => None,
    }
}

/// The "Newest" list: all cached videos, newest publication first, capped at
/// [`NEWEST_LIMIT`].
fn newest_videos(mut videos: Vec<YtVideoRef>) -> Vec<YtVideoRef> {
    videos.sort_by(|a, b| {
        yt_pubdate_key(b.published.as_deref()).cmp(&yt_pubdate_key(a.published.as_deref()))
    });
    videos.truncate(NEWEST_LIMIT);
    videos
}

/// Thresholds (publication keys at midnight) the "Newest" list is grouped by,
/// see [`crate::core::podcast::recent_day_buckets`].
#[derive(Debug, Clone, Copy)]
struct DayBuckets {
    today: i64,
    yesterday: i64,
    week_start: i64,
    month_start: i64,
}

impl DayBuckets {
    /// Group index of a publication key: 0 today, 1 yesterday, 2 this week,
    /// 3 this month, 4 older.
    fn bucket_of(&self, k: i64) -> usize {
        if k >= self.today {
            0
        } else if k >= self.yesterday {
            1
        } else if k >= self.week_start {
            2
        } else if k >= self.month_start {
            3
        } else {
            4
        }
    }
}

/// Heading of a "Newest" group ([`DayBuckets::bucket_of`]).
fn newest_bucket_title(b: usize) -> String {
    match b {
        0 => gettext("Today"),
        1 => gettext("Yesterday"),
        2 => gettext("This week"),
        3 => gettext("This month"),
        _ => gettext("Older"),
    }
}

/// Subtitle of a "Newest" row: the channel, plus the upload date when known.
fn newest_subtitle(channel_title: &str, published: Option<&str>) -> String {
    let mut subtitle = channel_title.to_string();
    if let Some(p) = published.filter(|s| !s.trim().is_empty()) {
        subtitle.push_str(" · ");
        subtitle.push_str(&fmt_published(p));
    }
    subtitle
}

/// Subtitle of a playlist row in "Recent": song count, plus the summed runtime
/// when known.
fn recent_playlist_subtitle(count: i64, total_duration: Option<i64>) -> String {
    let mut subtitle = gettext_f(
        "Playlist · {n}",
        &[("n", &ngettext_n("{n} song", "{n} songs", count as u32))],
    );
    if let Some(total) = total_duration.filter(|d| *d > 0) {
        subtitle.push_str(" · ");
        subtitle.push_str(&fmt_duration(total));
    }
    subtitle
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recent(title: &str, duration: Option<i64>, total: Option<i64>) -> YtRecent {
        YtRecent {
            video_id: title.to_string(),
            title: title.to_string(),
            artist: None,
            kind: "video".to_string(),
            count: 0,
            thumbnail: None,
            duration,
            total_duration: total,
        }
    }

    fn titles(items: &[YtRecent]) -> Vec<&str> {
        items.iter().map(|r| r.title.as_str()).collect()
    }

    fn channel(id: i64, title: &str, count: i64) -> ChannelItem {
        (id, title.to_string(), String::new(), None, count)
    }

    fn channel_ids(items: &[ChannelItem]) -> Vec<i64> {
        items.iter().map(|c| c.0).collect()
    }

    fn video(id: &str, published: Option<&str>) -> YtVideoRef {
        YtVideoRef {
            channel_title: "Chan".to_string(),
            channel_thumb: None,
            video_id: id.to_string(),
            title: id.to_string(),
            duration: None,
            published: published.map(str::to_string),
        }
    }

    #[test]
    fn recent_date_sort_keeps_or_reverses_the_db_order() {
        let base = vec![recent("b", None, None), recent("a", None, None)];
        let mut items = base.clone();
        sort_recent(&mut items, SortCrit::Release, true);
        assert_eq!(titles(&items), ["b", "a"]);
        let mut items = base;
        sort_recent(&mut items, SortCrit::Release, false);
        assert_eq!(titles(&items), ["a", "b"]);
    }

    #[test]
    fn recent_name_sort_is_natural() {
        let mut items = vec![
            recent("Track 10", None, None),
            recent("Track 2", None, None),
            recent("Track 1", None, None),
        ];
        sort_recent(&mut items, SortCrit::Name, false);
        assert_eq!(titles(&items), ["Track 1", "Track 2", "Track 10"]);
        sort_recent(&mut items, SortCrit::Name, true);
        assert_eq!(titles(&items), ["Track 10", "Track 2", "Track 1"]);
    }

    #[test]
    fn recent_length_sort_uses_playlist_total_and_unknown_as_zero() {
        let mut items = vec![
            recent("video", Some(300), None),
            recent("playlist", None, Some(1200)),
            recent("unknown", None, None),
        ];
        sort_recent(&mut items, SortCrit::Length, false);
        assert_eq!(titles(&items), ["unknown", "video", "playlist"]);
    }

    #[test]
    fn channels_sort_by_count_name_and_latest_video() {
        let base = vec![
            channel(1, "beta", 5),
            channel(2, "Alpha", 9),
            channel(3, "gamma", 1),
        ];

        let mut items = base.clone();
        sort_channel_items(&mut items, SortCrit::Songs, false, &HashMap::new());
        assert_eq!(channel_ids(&items), [3, 1, 2]);

        let mut items = base.clone();
        sort_channel_items(&mut items, SortCrit::Name, false, &HashMap::new());
        assert_eq!(channel_ids(&items), [2, 1, 3]);

        // Channel 3 has no cached video → sorts as oldest.
        let latest = HashMap::from([(1, 20240101000000), (2, 20230101000000)]);
        let mut items = base;
        sort_channel_items(&mut items, SortCrit::Release, true, &latest);
        assert_eq!(channel_ids(&items), [1, 2, 3]);
    }

    #[test]
    fn latest_pubdate_takes_the_newest_video_per_channel() {
        let latest = latest_pubdate_by_channel(vec![
            (1, Some("2024-01-02T10:00:00Z".to_string())),
            (1, Some("2024-03-01T00:00:00Z".to_string())),
            (1, None),
            (2, Some("not a date".to_string())),
        ]);
        assert_eq!(latest[&1], 20240301000000);
        assert_eq!(latest[&2], 0);
    }

    #[test]
    fn section_headers_only_for_grouped_name_sort() {
        let items = vec![channel(1, "Alpha", 1), channel(2, "9 Lives", 1)];
        assert_eq!(
            channel_section_headers(&items, SortCrit::Name, false),
            Some(vec![
                crate::ui::app_sort::alpha_header("Alpha"),
                crate::ui::app_sort::alpha_header("9 Lives"),
            ])
        );
        assert_eq!(channel_section_headers(&items, SortCrit::Name, true), None);
        assert_eq!(
            channel_section_headers(&items, SortCrit::Songs, false),
            None
        );
    }

    #[test]
    fn newest_videos_are_date_ordered_and_capped() {
        let mut videos: Vec<YtVideoRef> = (0..NEWEST_LIMIT + 5)
            .map(|i| video(&format!("old{i}"), Some("2020-01-01T00:00:00Z")))
            .collect();
        videos.push(video("undated", None));
        videos.push(video("new", Some("2024-05-01T12:00:00Z")));
        let newest = newest_videos(videos);
        assert_eq!(newest.len(), NEWEST_LIMIT);
        assert_eq!(newest[0].video_id, "new");
        // Undated videos sort as oldest and fall off the end first.
        assert!(newest.iter().all(|v| v.video_id != "undated"));
    }

    #[test]
    fn day_buckets_group_by_threshold() {
        let b = DayBuckets {
            today: 40,
            yesterday: 30,
            week_start: 20,
            month_start: 10,
        };
        assert_eq!(b.bucket_of(45), 0);
        assert_eq!(b.bucket_of(40), 0);
        assert_eq!(b.bucket_of(39), 1);
        assert_eq!(b.bucket_of(20), 2);
        assert_eq!(b.bucket_of(10), 3);
        assert_eq!(b.bucket_of(0), 4);
        assert_eq!(newest_bucket_title(0), "Today");
        assert_eq!(newest_bucket_title(4), "Older");
        assert_eq!(newest_bucket_title(99), "Older");
    }

    #[test]
    fn subtitles_join_their_known_parts() {
        assert_eq!(newest_subtitle("Chan", None), "Chan");
        assert_eq!(newest_subtitle("Chan", Some("  ")), "Chan");
        // An unparsable date is shown verbatim (see `fmt_published`).
        assert_eq!(newest_subtitle("Chan", Some("soon")), "Chan · soon");
        assert_eq!(recent_playlist_subtitle(1, None), "Playlist · 1 song");
        assert_eq!(recent_playlist_subtitle(3, Some(0)), "Playlist · 3 songs");
        assert_eq!(
            recent_playlist_subtitle(3, Some(125)),
            "Playlist · 3 songs · 2:05"
        );
    }

    #[test]
    fn channel_view_prefs_parse_their_defaults() {
        assert!(!parse_no_group(None));
        assert!(parse_no_group(Some("1")));
        assert!(!parse_no_group(Some("true")));
        assert_eq!(parse_gallery_override(None), None);
        assert_eq!(parse_gallery_override(Some("1")), Some(true));
        assert_eq!(parse_gallery_override(Some("0")), Some(false));
        assert_eq!(parse_gallery_override(Some("x")), None);
        assert!(parse_gallery_desc(None));
        assert!(!parse_gallery_desc(Some("0")));
        assert_eq!(setting_flag(true), "1");
        assert_eq!(setting_flag(false), "0");
    }

    #[test]
    fn count_title_escapes_markup() {
        assert_eq!(count_title("Rock & Roll", 3), "Rock &amp; Roll (3)");
    }
}
