//! The search half of [`YtPage`]'s inherent impl: the "+" choice modal, the
//! search dialog, the search worker and the results list. What a choice
//! searches for and how a hit is labelled are GTK-free free functions at the
//! bottom of this file (unit-tested).

use adw::prelude::*;
use relm4::prelude::*;
use relm4::{adw, gtk};

use crate::core::youtube::{self, YtKind, YtResult};
use crate::i18n::gettext;
use crate::ui::app::YtView;
use crate::ui::app_helpers::cover_widget;
use crate::ui::yt_channels::{fmt_duration, search_spinner_row};
use crate::ui::yt_page::{SearchKind, YtCmd, YtInput, YtOutput, YtPage};

/// Hits requested per search.
const SEARCH_LIMIT: usize = 25;

impl YtPage {
    /// The "+": a centered choice modal like the Files "+" — what to search
    /// for (songs, playlists, channels, live streams); the search itself
    /// opens as the second step.
    pub(super) fn open_youtube_search_dialog(&self, sender: &ComponentSender<Self>) {
        if !youtube::available() {
            let _ = sender.output(YtOutput::Toast(gettext(
                "Download yt-dlp in the settings first",
            )));
            return;
        }
        let Some(root) = self.window.clone() else {
            return;
        };
        let slot = self.search.clone();
        let (sender, win) = (sender.clone(), root.clone());
        let dialog = crate::ui::widgets::choice_modal(
            &gettext("Search YouTube"),
            &[
                // Labelled "Songs" (de "Lieder"): in this music app the video
                // search is used to find songs. It still searches YtKind::Video.
                ("video", gettext("Songs")),
                ("playlist", gettext("Playlists")),
                ("channel", gettext("Channels")),
                ("live", gettext("Live")),
            ],
            default_search_choice(self.yt_view),
            move |resp| {
                let Some((kind, heading)) = search_choice(resp) else {
                    return;
                };
                let sender = sender.clone();
                let (dialog, entry, results) = crate::ui::widgets::search_modal(
                    &heading,
                    &gettext("Search term …"),
                    move |term| sender.input(YtInput::Search(term, kind)),
                );
                *slot.borrow_mut() = Some((dialog.clone().upcast(), results));
                {
                    let slot = slot.clone();
                    dialog.connect_closed(move |_| {
                        *slot.borrow_mut() = None;
                    });
                }
                dialog.present(Some(&win));
                entry.grab_focus();
            },
        );
        dialog.present(Some(&root));
    }

    /// A search term was entered: bump the search sequence, show the spinner
    /// and run the search (then the hits' thumbnails) on a worker.
    pub(super) fn on_search(
        &mut self,
        sender: &ComponentSender<Self>,
        term: &str,
        kind: SearchKind,
    ) {
        let term = term.trim().to_string();
        if term.is_empty() {
            return;
        }
        self.search_seq = self.search_seq.wrapping_add(1);
        let seq = self.search_seq;
        self.search_kind = kind;
        self.show_youtube_search_spinner();
        sender.spawn_command(move |out| {
            let found = match kind {
                SearchKind::Yt(kind) => youtube::search(&term, kind, SEARCH_LIMIT),
                SearchKind::Live => youtube::search_live(&term, SEARCH_LIMIT),
            };
            let results = match found {
                Ok(r) => r,
                Err(_) => {
                    let _ = out.send(YtCmd::SearchFailed(seq));
                    return;
                }
            };
            let _ = out.send(YtCmd::SearchResults(seq, results.clone()));
            for r in &results {
                if let Some(t) = r.thumbnail.as_deref() {
                    crate::core::online::cache_youtube_thumb(t);
                }
            }
            let _ = out.send(YtCmd::SearchThumbsReady(seq));
        });
    }

    /// Worker result of the current search (`None` = it failed). The caller
    /// has already dropped results of an outdated search.
    pub(super) fn on_cmd_search_results(
        &mut self,
        sender: &ComponentSender<Self>,
        results: Option<Vec<YtResult>>,
    ) {
        self.search_failed = results.is_none();
        self.search_results = results.unwrap_or_default();
        self.rebuild_youtube_search_results(sender);
    }

    /// Clears the open search dialog's results list and shows a single spinner
    /// row while the current search runs. Replaced by the real hits (or the
    /// "Nothing found" / error row) once the worker reports back.
    fn show_youtube_search_spinner(&self) {
        let guard = self.search.borrow();
        let Some((_, list)) = guard.as_ref() else {
            return;
        };
        while let Some(child) = list.first_child() {
            list.remove(&child);
        }
        list.set_visible(true);
        list.append(&search_spinner_row());
    }

    /// Redraws the results list in the open search dialog.
    pub(super) fn rebuild_youtube_search_results(&self, sender: &ComponentSender<Self>) {
        let guard = self.search.borrow();
        let Some((dialog, list)) = guard.as_ref() else {
            return;
        };
        while let Some(child) = list.first_child() {
            list.remove(&child);
        }
        list.set_visible(true);

        if self.search_results.is_empty() {
            let row = if self.search_failed {
                let r = adw::ActionRow::builder()
                    .title(gettext("YouTube unreachable"))
                    .subtitle(gettext(
                        "Check your connection, or update yt-dlp in the settings",
                    ))
                    .build();
                r.set_subtitle_lines(2);
                r
            } else {
                adw::ActionRow::builder()
                    .title(gettext("Nothing found"))
                    .build()
            };
            row.set_sensitive(false);
            list.append(&row);
            return;
        }

        let live = self.search_kind == SearchKind::Live;
        for (index, r) in self.search_results.iter().enumerate() {
            let subtitle = search_result_subtitle(r, live);
            let row = adw::ActionRow::builder()
                .title(gtk::glib::markup_escape_text(&r.title))
                .subtitle(gtk::glib::markup_escape_text(&subtitle))
                .activatable(true)
                .build();
            let cover = r
                .thumbnail
                .as_deref()
                .and_then(crate::core::online::youtube_thumb_path);
            row.add_prefix(&cover_widget(
                cover.as_deref(),
                search_result_icon(r.kind, live),
            ));
            if live {
                // Tapping a live hit saves it to the Live tab (like subscribing
                // to a channel); it is played from there, never downloaded.
                row.add_suffix(&gtk::Image::from_icon_name("list-add-symbolic"));
                let (sender, dialog) = (sender.clone(), dialog.clone());
                row.connect_activated(move |_| {
                    sender.input(YtInput::AddLive(index));
                    dialog.close();
                });
                list.append(&row);
                continue;
            }
            match r.kind {
                YtKind::Video => {
                    let btn = gtk::Button::builder()
                        .icon_name("list-add-symbolic")
                        .valign(gtk::Align::Center)
                        .css_classes(["flat"])
                        .tooltip_text(gettext("List as newest"))
                        .build();
                    let (sender, vid, title) = (sender.clone(), r.id.clone(), r.title.clone());
                    btn.connect_clicked(move |b| {
                        sender.input(YtInput::AddRecent {
                            video_id: vid.clone(),
                            title: title.clone(),
                        });
                        b.set_icon_name("object-select-symbolic");
                        b.set_sensitive(false);
                    });
                    row.add_suffix(&btn);
                }
                YtKind::Channel => {
                    row.add_suffix(&gtk::Image::from_icon_name("list-add-symbolic"));
                }
                YtKind::Playlist => {
                    row.add_suffix(&gtk::Image::from_icon_name("list-add-symbolic"));
                }
            }
            {
                let (sender, dialog) = (sender.clone(), dialog.clone());
                let (kind, url, vid, title) =
                    (r.kind, r.url.clone(), r.id.clone(), r.title.clone());
                row.connect_activated(move |_| {
                    match kind {
                        YtKind::Channel => sender.input(YtInput::SubscribeChannel(url.clone())),
                        YtKind::Playlist => sender.input(YtInput::ShowPlaylistDetail {
                            url: url.clone(),
                            title: title.clone(),
                        }),
                        YtKind::Video => sender.input(YtInput::ShowVideoDetail {
                            video_id: vid.clone(),
                            title: title.clone(),
                        }),
                    }
                    dialog.close();
                });
            }
            list.append(&row);
        }
    }
}

/// Preselected entry of the "+" choice modal: opened from the Live tab, live
/// streams are the default pick; otherwise songs (videos).
fn default_search_choice(view: YtView) -> &'static str {
    if view == YtView::Live {
        "live"
    } else {
        "video"
    }
}

/// What a "+" choice searches for, with the search dialog's heading; `None`
/// for an unknown response (the modal was dismissed).
fn search_choice(resp: &str) -> Option<(SearchKind, String)> {
    Some(match resp {
        "video" => (SearchKind::Yt(YtKind::Video), gettext("Search songs")),
        "playlist" => (
            SearchKind::Yt(YtKind::Playlist),
            gettext("Search playlists"),
        ),
        "channel" => (SearchKind::Yt(YtKind::Channel), gettext("Search channels")),
        "live" => (SearchKind::Live, gettext("Search live streams")),
        _ => return None,
    })
}

/// Subtitle of a search hit: its kind ("Live" for every hit of a live search),
/// then the uploader and the runtime when known (unescaped).
fn search_result_subtitle(r: &YtResult, live: bool) -> String {
    let mut subtitle = match r.kind {
        _ if live => gettext("Live"),
        YtKind::Video => gettext("Video"),
        YtKind::Playlist => gettext("Playlist"),
        YtKind::Channel => gettext("Channel"),
    };
    if let Some(u) = r.uploader.as_deref().filter(|s| !s.trim().is_empty()) {
        subtitle.push_str(" · ");
        subtitle.push_str(u);
    }
    if let Some(d) = r.duration {
        subtitle.push_str(" · ");
        subtitle.push_str(&fmt_duration(d));
    }
    subtitle
}

/// Fallback icon of a search hit without a cached thumbnail. A channel keeps
/// its avatar icon even in a live search.
fn search_result_icon(kind: YtKind, live: bool) -> &'static str {
    match kind {
        YtKind::Channel => "avatar-default-symbolic",
        _ if live => "internet-radio-symbolic",
        _ => "audio-x-generic-symbolic",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(kind: YtKind, uploader: Option<&str>, duration: Option<i64>) -> YtResult {
        YtResult {
            kind,
            id: "id".to_string(),
            url: "https://example.invalid".to_string(),
            title: "Title".to_string(),
            uploader: uploader.map(str::to_string),
            duration,
            thumbnail: None,
        }
    }

    #[test]
    fn live_tab_preselects_live_search() {
        assert_eq!(default_search_choice(YtView::Live), "live");
        assert_eq!(default_search_choice(YtView::Recent), "video");
        assert_eq!(default_search_choice(YtView::Channels), "video");
    }

    #[test]
    fn choices_map_to_search_kinds() {
        let kind = |r| search_choice(r).map(|(k, _)| k);
        assert_eq!(kind("video"), Some(SearchKind::Yt(YtKind::Video)));
        assert_eq!(kind("playlist"), Some(SearchKind::Yt(YtKind::Playlist)));
        assert_eq!(kind("channel"), Some(SearchKind::Yt(YtKind::Channel)));
        assert_eq!(kind("live"), Some(SearchKind::Live));
        assert_eq!(kind("cancel"), None);
        // The video search is presented as a song search.
        assert_eq!(search_choice("video").unwrap().1, "Search songs");
    }

    #[test]
    fn hit_subtitle_lists_kind_uploader_and_runtime() {
        assert_eq!(
            search_result_subtitle(&hit(YtKind::Video, Some("Band"), Some(185)), false),
            "Video · Band · 3:05"
        );
        // Blank uploader is skipped.
        assert_eq!(
            search_result_subtitle(&hit(YtKind::Playlist, Some(" "), None), false),
            "Playlist"
        );
        assert_eq!(
            search_result_subtitle(&hit(YtKind::Channel, None, None), false),
            "Channel"
        );
        // A live search labels every hit "Live", whatever its kind.
        assert_eq!(
            search_result_subtitle(&hit(YtKind::Video, Some("Lofi"), None), true),
            "Live · Lofi"
        );
    }

    #[test]
    fn hit_icons_follow_kind_then_live() {
        assert_eq!(
            search_result_icon(YtKind::Video, false),
            "audio-x-generic-symbolic"
        );
        assert_eq!(
            search_result_icon(YtKind::Playlist, false),
            "audio-x-generic-symbolic"
        );
        assert_eq!(
            search_result_icon(YtKind::Video, true),
            "internet-radio-symbolic"
        );
        assert_eq!(
            search_result_icon(YtKind::Channel, false),
            "avatar-default-symbolic"
        );
        assert_eq!(
            search_result_icon(YtKind::Channel, true),
            "avatar-default-symbolic"
        );
    }
}
