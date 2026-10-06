//! The YouTube page's "Live" tab: saved live streams (24/7 radio channels such
//! as lofi beats), their detail dialog (and its refresh) and the search-result
//! rows that save them. Live streams are only ever streamed — there is no download, no resume
//! position and no watch progress; the transport plays them like a radio
//! station (see `App::play_yt_live`).

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::core::youtube::{self, YtResult};
use crate::i18n::gettext;
use crate::ui::app::YtView;
use crate::ui::entry_row::EntryRow;
use crate::ui::widgets::{action_row, detail_box, present_detail_refreshable};
use crate::ui::yt_page::{YtCmd, YtInput, YtOutput, YtPage};

/// Fallback icon of a live stream without a cached thumbnail.
const LIVE_ICON: &str = "internet-radio-symbolic";

/// Cached thumbnail of a live stream (display path only; fetched with the search).
fn live_cover(thumbnail: Option<&str>) -> Option<String> {
    thumbnail.and_then(crate::core::online::youtube_thumb_path)
}

impl YtPage {
    /// Rebuilds the Live tab from the DB.
    pub(super) fn reload_live(&mut self, sender: &ComponentSender<Self>) {
        self.live_items = self.library.live_streams().unwrap_or_default();
        while let Some(child) = self.live_list.first_child() {
            self.live_list.remove(&child);
        }
        for item in &self.live_items {
            let cover = live_cover(item.thumbnail.as_deref());
            let (vid, title) = (item.video_id.clone(), item.title.clone());
            let row = EntryRow::new(&item.title)
                .subtitle(item.channel.as_deref().unwrap_or_default())
                .cover(cover.as_deref(), LIVE_ICON)
                .play_button(
                    &gettext("Play/Pause"),
                    self.playing_video_id.as_deref() == Some(vid.as_str()),
                    self.playing,
                    {
                        let (sender, vid, title) = (sender.clone(), vid.clone(), title.clone());
                        move || {
                            let _ = sender.output(YtOutput::PlayLive {
                                video_id: vid.clone(),
                                title: title.clone(),
                            });
                        }
                    },
                )
                // Shares the video marks: the transport reports a running live
                // stream under its video id, so the row icons follow it.
                .marked_in(&self.video_marks, vid.clone())
                .on_detail({
                    let sender = sender.clone();
                    move || sender.input(YtInput::ShowLiveDetail(vid.clone()))
                })
                .build();
            self.live_list.append(&row);
        }
        self.refresh_yt_icons();
    }

    /// Saves a live search hit to the Live tab and switches to it.
    pub(super) fn add_live(&mut self, sender: &ComponentSender<Self>, hit: YtResult) {
        let _ = self.library.add_live(
            &hit.id,
            &hit.title,
            hit.uploader.as_deref(),
            hit.thumbnail.as_deref(),
        );
        self.reload_live(sender);
        self.yt_view = YtView::Live;
        self.rebuild_sort(sender);
    }

    /// Detail dialog of a saved live stream: play/pause, equalizer and remove. Deliberately
    /// without the video dialog's download / add-to-library actions.
    pub(super) fn show_live_detail(&self, sender: &ComponentSender<Self>, video_id: &str) {
        let Some(root) = self.window.clone() else {
            return;
        };
        let Some(item) = self
            .live_items
            .iter()
            .find(|l| l.video_id == video_id)
            .cloned()
        else {
            return;
        };
        let dialog = adw::Dialog::builder().title(&item.title).build();
        self.adapt_detail_dialog(&dialog);
        let content = detail_box();

        let info = adw::PreferencesGroup::new();
        let head = adw::ActionRow::builder()
            .title(gtk::glib::markup_escape_text(&item.title))
            .subtitle(gtk::glib::markup_escape_text(
                &[Some(gettext("Live")), item.channel.clone()]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" · "),
            ))
            .build();
        head.set_title_lines(3);
        let cover = live_cover(item.thumbnail.as_deref());
        content.append(&crate::ui::widgets::detail_cover(
            cover.as_deref(),
            LIVE_ICON,
        ));
        info.add(&head);
        content.append(&info);

        let actions = adw::PreferencesGroup::new();
        let running = self.playing && self.playing_video_id.as_deref() == Some(video_id);
        let play = action_row(
            &if running {
                gettext("Pause")
            } else {
                gettext("Play")
            },
            if running {
                "media-playback-pause-symbolic"
            } else {
                "media-playback-start-symbolic"
            },
        );
        {
            let (sender, dialog) = (sender.clone(), dialog.clone());
            let (vid, title) = (item.video_id.clone(), item.title.clone());
            play.connect_activated(move |_| {
                let _ = sender.output(YtOutput::PlayLive {
                    video_id: vid.clone(),
                    title: title.clone(),
                });
                dialog.close();
            });
        }
        actions.add(&play);
        let eq = action_row(
            &gettext("Equalizer settings"),
            "multimedia-equalizer-symbolic",
        );
        {
            let (sender, dialog) = (sender.clone(), dialog.clone());
            let (vid, title) = (item.video_id.clone(), item.title.clone());
            eq.connect_activated(move |_| {
                let _ = sender.output(YtOutput::OpenLiveEq {
                    video_id: vid.clone(),
                    title: title.clone(),
                });
                dialog.close();
            });
        }
        actions.add(&eq);
        let remove = action_row(&gettext("Remove live stream"), "user-trash-symbolic");
        {
            let (sender, dialog, root) = (sender.clone(), dialog.clone(), root.clone());
            let vid = item.video_id.clone();
            remove.connect_activated(move |_| {
                dialog.close();
                let confirm =
                    adw::AlertDialog::new(Some(&gettext("Remove this live stream?")), None);
                confirm.add_response("cancel", &gettext("Cancel"));
                confirm.add_response("ok", &gettext("Remove"));
                confirm.set_response_appearance("ok", adw::ResponseAppearance::Destructive);
                confirm.set_default_response(Some("cancel"));
                confirm.set_close_response("cancel");
                let (sender, vid) = (sender.clone(), vid.clone());
                confirm.connect_response(None, move |_, resp| {
                    if resp == "ok" {
                        sender.input(YtInput::RemoveLive(vid.clone()));
                    }
                });
                confirm.present(Some(&root));
            });
        }
        actions.add(&remove);
        content.append(&actions);

        {
            let (sender, vid) = (sender.clone(), item.video_id.clone());
            present_detail_refreshable(&dialog, &content, &root, move || {
                sender.input(YtInput::RefreshLive(vid.clone()));
            });
        }
    }

    /// Live detail refresh: title/channel and the thumbnail again (worker),
    /// then reopen the detail.
    pub(super) fn on_refresh_live(&self, sender: &ComponentSender<Self>, video_id: String) {
        let thumb = self
            .live_items
            .iter()
            .find(|l| l.video_id == video_id)
            .and_then(|l| l.thumbnail.clone());
        let _ = sender.output(YtOutput::Toast(gettext("Refreshing …")));
        sender.spawn_command(move |out| {
            let details = youtube::video_details(&video_id).ok().map(|d| d.meta);
            let url = live_refresh_thumb_url(
                details.as_ref().and_then(|d| d.thumbnail.clone()),
                thumb,
                &video_id,
            );
            let thumbnail = crate::core::online::recache_youtube_thumb(&url).map(|_| url);
            let _ = out.send(YtCmd::LiveRefreshed {
                video_id,
                details,
                thumbnail,
            });
        });
    }

    /// Worker result of a live detail refresh: store the fresh metadata, drop
    /// the stale thumbnail, reload the tab and reopen the detail.
    pub(super) fn on_cmd_live_refreshed(
        &mut self,
        sender: &ComponentSender<Self>,
        video_id: String,
        details: Option<YtResult>,
        thumbnail: Option<String>,
    ) {
        if let Some(t) = thumbnail.as_deref() {
            crate::ui::widgets::forget_thumb(crate::core::online::youtube_thumb_path(t).as_deref());
        }
        if let Some(d) = details.as_ref() {
            let channel = d.uploader.as_deref().map(youtube::clean_channel_name);
            let _ = self.library.add_live(
                &video_id,
                &d.title,
                channel.as_deref().filter(|c| !c.trim().is_empty()),
                thumbnail.as_deref(),
            );
        } else if thumbnail.is_none() {
            let _ = sender.output(YtOutput::Toast(gettext("Nothing found")));
        }
        self.reload_live(sender);
        self.show_live_detail(sender, &video_id);
    }
}

/// Thumbnail URL a live refresh re-caches: the freshly fetched one, else the
/// saved one, else YouTube's standard thumbnail of the video id.
fn live_refresh_thumb_url(fresh: Option<String>, saved: Option<String>, video_id: &str) -> String {
    fresh
        .or(saved)
        .unwrap_or_else(|| youtube::thumbnail_url(video_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_refresh_prefers_fresh_then_saved_thumbnail() {
        let (fresh, saved) = (Some("fresh".to_string()), Some("saved".to_string()));
        assert_eq!(live_refresh_thumb_url(fresh, saved.clone(), "v"), "fresh");
        assert_eq!(live_refresh_thumb_url(None, saved, "v"), "saved");
        assert_eq!(
            live_refresh_thumb_url(None, None, "abc"),
            youtube::thumbnail_url("abc")
        );
    }
}
