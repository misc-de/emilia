//! The subscription half of [`YtPage`]'s inherent impl: subscribing from a
//! search hit, a channel's videos subpage, its detail dialog and the
//! per-channel refreshes, plus their `on_cmd_*` worker-result handlers. The
//! feed/cache helpers they run on workers live in [`crate::ui::yt_channels`].

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::core::db::Library;
use crate::core::youtube::{self, YtKind};
use crate::i18n::{gettext, gettext_f, ngettext_n};
use crate::ui::app::YtView;
use crate::ui::widgets::{action_row, detail_box, present_detail_refreshable};
use crate::ui::yt_channels::{
    fill_channel_videos, fmt_published, refresh_channel_videos, store_channel,
};
use crate::ui::yt_page::{YtCmd, YtInput, YtOutput, YtPage};
use crate::ui::yt_page_lists::{count_title, ChannelItem};

/// Fetches the channel thumbnails not yet in the cache (worker thread —
/// network). Returns whether any came in, i.e. whether a redraw would show
/// something new.
pub(super) fn cache_missing_channel_thumbs() -> bool {
    let Ok(lib) = Library::open() else {
        return false;
    };
    lib.channels()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(_, _, _, thumb, _)| thumb)
        .filter(|url| crate::core::online::youtube_thumb_path(url).is_none())
        .filter(|url| crate::core::online::cache_youtube_thumb(url).is_some())
        .count()
        > 0
}

impl YtPage {
    /// The subscribed channel with this DB id (a copy of its overview entry).
    fn channel_item(&self, id: i64) -> Option<ChannelItem> {
        self.channel_items
            .iter()
            .find(|(cid, _, _, _, _)| *cid == id)
            .cloned()
    }

    /// A channel hit of the search was tapped: store the subscription, then
    /// fill its video cache on the worker.
    pub(super) fn on_subscribe_channel(&self, sender: &ComponentSender<Self>, url: &str) {
        let Some(r) = self
            .search_results
            .iter()
            .find(|r| r.url == url && r.kind == YtKind::Channel)
            .cloned()
        else {
            return;
        };
        let _ = sender.output(YtOutput::SetLoading(Some(gettext_f(
            "Subscribing to {t} …",
            &[("t", &r.title)],
        ))));
        sender.spawn_command(move |out| {
            let Some(db_id) = store_channel(&r.id, &r.title, &r.url, r.thumbnail.as_deref()) else {
                let _ = out.send(YtCmd::ChannelFetched(None));
                return;
            };
            // The subscription exists — show it now; its videos and
            // thumbnails keep loading in this worker.
            let _ = out.send(YtCmd::ChannelFetched(Some(r.title.clone())));
            fill_channel_videos(db_id, &r.id, &r.title, &r.url, r.thumbnail.as_deref());
            let _ = out.send(YtCmd::ChannelVideosReady);
        });
    }

    /// Opens a subscribed channel's videos subpage by its DB id.
    pub(super) fn on_open_channel(&self, sender: &ComponentSender<Self>, id: i64) {
        if let Some((_, title, _, _, _)) = self.channel_item(id) {
            self.open_channel(sender, id, &title);
        }
    }

    /// Re-fetches one channel's videos (worker), then reloads the overview.
    pub(super) fn on_refresh_channel(&self, sender: &ComponentSender<Self>, id: i64) {
        if let Some((_, title, url, _, _)) = self.channel_item(id) {
            let _ = sender.output(YtOutput::Toast(gettext("Refreshing …")));
            sender.spawn_command(move |out| {
                let fetched = refresh_channel_videos(id, &title, &url).map(|(title, _)| title);
                let _ = out.send(YtCmd::ChannelFetched(fetched));
            });
        }
    }

    /// Channel detail refresh: its videos plus the avatar, then reopen it.
    pub(super) fn on_refresh_channel_detail(&self, sender: &ComponentSender<Self>, id: i64) {
        let Some((_, title, url, thumb, _)) = self.channel_item(id) else {
            return;
        };
        let _ = sender.output(YtOutput::Toast(gettext("Refreshing …")));
        sender.spawn_command(move |out| {
            let _ = refresh_channel_videos(id, &title, &url);
            // The avatar again; a channel without one gets the artist
            // photo a music database has for its name.
            match thumb.as_deref() {
                Some(t) => {
                    let _ = crate::core::online::recache_youtube_thumb(t);
                }
                None => {
                    if let Some(u) = crate::core::online::channel_image_url(None, &title) {
                        if crate::core::online::recache_youtube_thumb(&u).is_some() {
                            if let Ok(lib) = Library::open() {
                                let _ = lib.set_channel_thumbnail(id, &u);
                            }
                        }
                    }
                }
            }
            let _ = out.send(YtCmd::ChannelDetailRefreshed(id));
        });
    }

    /// Worker result: a subscription/refresh finished (`Some(title)`) or failed.
    pub(super) fn on_cmd_channel_fetched(
        &mut self,
        sender: &ComponentSender<Self>,
        title: Option<String>,
    ) {
        let _ = sender.output(YtOutput::SetLoading(None));
        self.reload_channels(sender);
        match title {
            Some(t) => {
                self.yt_view = YtView::Channels;
                let _ = sender.output(YtOutput::Toast(gettext_f("Subscribed: {t}", &[("t", &t)])));
            }
            None => {
                let _ = sender.output(YtOutput::Toast(gettext("Could not load channel")));
            }
        }
    }

    /// Worker result of a channel detail refresh: drop the stale avatar from
    /// the thumbnail cache, reload, reopen the detail.
    pub(super) fn on_cmd_channel_detail_refreshed(
        &mut self,
        sender: &ComponentSender<Self>,
        id: i64,
    ) {
        if let Some(t) = self
            .channel_items
            .iter()
            .find(|c| c.0 == id)
            .and_then(|c| c.3.as_deref())
        {
            crate::ui::widgets::forget_thumb(crate::core::online::youtube_thumb_path(t).as_deref());
        }
        self.reload_channels(sender);
        self.open_channel_detail(sender, id);
    }

    /// Videos subpage of a subscribed channel.
    fn open_channel(&self, sender: &ComponentSender<Self>, id: i64, title: &str) {
        let videos = self.library.channel_videos(id).unwrap_or_default();
        let channel_thumb = self
            .channel_items
            .iter()
            .find(|(cid, _, _, _, _)| *cid == id)
            .and_then(|(_, _, _, t, _)| t.as_deref())
            .and_then(crate::core::online::youtube_thumb_path);

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        let group = adw::PreferencesGroup::builder()
            .title(count_title(title, videos.len()).as_str())
            .build();
        if videos.is_empty() {
            group.add(
                &adw::ActionRow::builder()
                    .title(gettext("No videos"))
                    .build(),
            );
        }
        let watched = self.library.all_yt_progress().unwrap_or_default();
        for v in &videos {
            // Subtitle: upload date (the runtime sits on the right, like the
            // podcast lists; repeating it here would be noise).
            let subtitle = channel_video_subtitle(v.published.as_deref());
            let cover = crate::core::online::youtube_cover_path(&v.video_id)
                .or_else(|| {
                    crate::core::online::youtube_thumb_path(&youtube::thumbnail_url(&v.video_id))
                })
                .or_else(|| channel_thumb.clone());
            let card = self.video_card(
                sender,
                &v.video_id,
                &v.title,
                &subtitle,
                cover.as_deref(),
                v.duration,
                watched.get(&v.video_id),
                {
                    let (sender, vid, t) = (sender.clone(), v.video_id.clone(), v.title.clone());
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
        content.append(&group);
        self.push_subpage(
            sender,
            gettext_f("Channel – {title}", &[("title", title)]),
            content,
        );
    }

    /// Subscription detail of a channel.
    pub(super) fn open_channel_detail(&self, sender: &ComponentSender<Self>, id: i64) {
        let Some(root) = self.window.clone() else {
            return;
        };
        let Some((_, title, url, thumb, count)) = self.channel_item(id) else {
            return;
        };
        let dialog = adw::Dialog::builder().title(&title).build();
        self.adapt_detail_dialog(&dialog);
        let content = detail_box();

        let info = adw::PreferencesGroup::new();
        let head = adw::ActionRow::builder()
            .title(gtk::glib::markup_escape_text(&title))
            .subtitle(ngettext_n("{n} video", "{n} videos", count as u32))
            .build();
        let cover = thumb
            .as_deref()
            .and_then(crate::core::online::youtube_thumb_path);
        content.append(&crate::ui::widgets::detail_cover(
            cover.as_deref(),
            "avatar-default-symbolic",
        ));
        info.add(&head);
        content.append(&info);

        let actions = adw::PreferencesGroup::new();
        let play = action_row(&gettext("Play"), "media-playback-start-symbolic");
        {
            let (sender, dialog) = (sender.clone(), dialog.clone());
            play.connect_activated(move |_| {
                let _ = sender.output(YtOutput::PlayChannel(id));
                dialog.close();
            });
        }
        actions.add(&play);
        let share = action_row(&gettext("Share"), "emilia-share-symbolic");
        {
            let (sender, dialog) = (sender.clone(), dialog.clone());
            share.connect_activated(move |_| {
                let _ = sender.output(YtOutput::Share(Box::new(
                    crate::core::sync::share::Selection {
                        yt_channels: vec![id],
                        ..Default::default()
                    },
                )));
                dialog.close();
            });
        }
        actions.add(&share);
        let bell = adw::SwitchRow::builder()
            .title(gettext("Notify of newest publications"))
            .active(true)
            .build();
        {
            let (sender, dialog) = (sender.clone(), dialog.clone());
            bell.connect_active_notify(move |s| {
                if !s.is_active() {
                    sender.input(YtInput::DeleteChannel(id));
                    dialog.close();
                }
            });
        }
        actions.add(&bell);
        let remove = action_row(&gettext("Remove"), "user-trash-symbolic");
        remove.add_css_class("error");
        {
            let (sender, dialog) = (sender.clone(), dialog.clone());
            remove.connect_activated(move |_| {
                sender.input(YtInput::DeleteChannel(id));
                dialog.close();
            });
        }
        actions.add(&remove);
        content.append(&actions);
        let _ = url;
        {
            let sender = sender.clone();
            present_detail_refreshable(&dialog, &content, &root, move || {
                sender.input(YtInput::RefreshChannelDetail(id));
            });
        }
    }
}

/// Subtitle of a row in a channel's videos subpage: just the upload date
/// (empty when unknown).
fn channel_video_subtitle(published: Option<&str>) -> String {
    published
        .filter(|p| !p.trim().is_empty())
        .map(fmt_published)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_video_subtitle_is_the_date_or_nothing() {
        assert_eq!(channel_video_subtitle(None), "");
        assert_eq!(channel_video_subtitle(Some(" ")), "");
        // Unparsable dates are shown verbatim (see `fmt_published`).
        assert_eq!(channel_video_subtitle(Some("soon")), "soon");
        assert!(channel_video_subtitle(Some("2024-05-01T12:30:00Z")).contains("2024"));
    }
}
