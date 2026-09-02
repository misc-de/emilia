//! Free helpers of the YouTube page ([`crate::ui::yt_page`]): the channel
//! subscribe/refresh workers (Atom-feed check + yt-dlp listing, worker threads
//! with their own DB), publication-date and duration formatting, and the small
//! row widgets the lists share. Split out of `yt_page.rs` so that file holds
//! the component itself.

use relm4::gtk;

use gtk::prelude::*;

use crate::core::db::Library;
use crate::core::youtube::{self, YtResult};
use crate::i18n::{gettext, ngettext_n};

/// How many newest videos to cache per channel on subscribe/refresh.
pub(crate) const CHANNEL_VIDEO_LIMIT: usize = 30;

/// A non-selectable results-list row with a spinner and the "Searching …"
/// label, shown as the list's only row while a search is in flight. Building
/// the spinner and appending the row into the already-visible list keeps it
/// mapped, so the animation actually runs — unlike a separate box that starts
/// hidden and is only toggled visible later (where the spinner never spins up).
pub(super) fn search_spinner_row() -> gtk::ListBoxRow {
    let inner = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .margin_top(24)
        .margin_bottom(24)
        .build();
    let spinner = gtk::Spinner::builder()
        .width_request(36)
        .height_request(36)
        .build();
    spinner.set_spinning(true);
    inner.append(&spinner);
    inner.append(
        &gtk::Label::builder()
            .label(gettext("Searching …"))
            .css_classes(["dim-label"])
            .build(),
    );
    let row = gtk::ListBoxRow::builder()
        .selectable(false)
        .activatable(false)
        .build();
    row.set_child(Some(&inner));
    row
}

/// Converts a search/listing hit into a storable video row.
fn to_model_video(r: YtResult) -> crate::model::YtVideo {
    crate::model::YtVideo {
        video_id: r.id,
        title: r.title,
        url: r.url,
        duration: r.duration,
        published: None,
        thumbnail: r.thumbnail,
    }
}

/// Sortable key `YYYYMMDDHHMMSS` from an ISO-8601 publication timestamp.
pub(super) fn yt_pubdate_key(published: Option<&str>) -> i64 {
    let Some(s) = published.filter(|s| !s.trim().is_empty()) else {
        return 0;
    };
    let Ok(dt) = gtk::glib::DateTime::from_iso8601(s, None) else {
        return 0;
    };
    (((((dt.year() as i64 * 100 + dt.month() as i64) * 100 + dt.day_of_month() as i64) * 100
        + dt.hour() as i64)
        * 100
        + dt.minute() as i64)
        * 100)
        + dt.seconds() as i64
}

/// Formats an ISO-8601 publication timestamp as `DD.MM.YYYY HH:MM`.
pub(super) fn fmt_published(iso: &str) -> String {
    gtk::glib::DateTime::from_iso8601(iso, None)
        .ok()
        .and_then(|dt| dt.format("%d.%m.%Y %H:%M").ok())
        .map(|g| g.to_string())
        .unwrap_or_else(|| iso.to_string())
}

/// A right-aligned, subtle duration label for a video row.
pub(super) fn duration_chip(secs: i64) -> gtk::Label {
    let lbl = gtk::Label::new(Some(&fmt_duration(secs)));
    lbl.set_valign(gtk::Align::Center);
    lbl.set_css_classes(&["dim-label", "numeric"]);
    lbl
}

/// Formats a duration in **seconds** as `M:SS` or `H:MM:SS` (display only) —
/// YouTube reports lengths in seconds, the rest of the app in milliseconds.
pub(crate) fn fmt_duration(secs: i64) -> String {
    crate::ui::app_helpers::fmt_duration(secs.saturating_mul(1000))
}

/// Stores the subscription itself and returns its DB id (worker thread, own DB).
/// Deliberately separate from [`fill_channel_videos`]: writing the row takes
/// milliseconds, while filling its video cache is a yt-dlp run plus a listing's
/// worth of thumbnails — the user should see the channel appear immediately
/// rather than watch a spinner until all of that is done.
pub(crate) fn store_channel(
    channel_id: &str,
    title: &str,
    url: &str,
    thumbnail: Option<&str>,
) -> Option<i64> {
    Library::open()
        .ok()?
        .subscribe_channel(channel_id, title, url, thumbnail)
        .ok()
}

/// Makes sure a subscribed channel has a picture to show: caches its YouTube
/// avatar, or — when the listing carried none — looks one up in a music DB by
/// channel name and stores that URL on the channel row. Without this a channel
/// without an avatar keeps a bare placeholder in the subscriptions list.
/// **Network – worker threads only.**
pub(crate) fn ensure_channel_image(
    lib: &Library,
    db_id: i64,
    title: &str,
    thumbnail: Option<&str>,
) {
    if let Some(t) = thumbnail.map(str::trim).filter(|t| !t.is_empty()) {
        crate::core::online::cache_youtube_thumb(t);
        return;
    }
    let name = youtube::clean_channel_name(title);
    if let Some(url) = crate::core::online::channel_image_url(None, &name) {
        crate::core::online::cache_youtube_thumb(&url);
        let _ = lib.set_channel_thumbnail(db_id, &url);
    }
}

/// Fills a freshly subscribed channel's video cache and thumbnails (worker
/// thread, own DB). Runs after [`store_channel`] has already made the channel
/// visible. **Network.**
pub(crate) fn fill_channel_videos(
    db_id: i64,
    channel_id: &str,
    title: &str,
    url: &str,
    thumbnail: Option<&str>,
) {
    let Ok(lib) = Library::open() else {
        return;
    };
    ensure_channel_image(&lib, db_id, title, thumbnail);
    let videos = list_channel_videos(url, Some(channel_id), None);
    let _ = lib.set_channel_videos(db_id, &videos);
}

/// Whether the channel feed lists a video the cache doesn't know yet — the
/// question a "check for new videos" actually asks. `dates` is a feed map
/// (`video_id → published`).
fn feed_has_unknown(
    dates: &std::collections::HashMap<String, String>,
    stored: &[crate::model::YtVideo],
) -> bool {
    let known: std::collections::HashSet<&str> =
        stored.iter().map(|v| v.video_id.as_str()).collect();
    dates.keys().any(|id| !known.contains(id.as_str()))
}

/// Fingerprint of a channel feed's current contents. Stored after a listing so
/// the next check can tell "the feed has not moved at all" from "there is
/// something new", **including entries the `/videos` listing never returns** —
/// a channel's Shorts and streams show up in the feed but not in that tab, so
/// comparing feed ids against the video cache alone would report new videos
/// forever and defeat the whole point of asking the feed first.
fn feed_signature(dates: &std::collections::HashMap<String, String>) -> String {
    use std::hash::{Hash, Hasher};
    let mut entries: Vec<(&str, &str)> = dates
        .iter()
        .map(|(id, p)| (id.as_str(), p.as_str()))
        .collect();
    entries.sort_unstable();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    entries.hash(&mut h);
    format!("{:x}", h.finish())
}

/// Setting key the feed fingerprint of a channel is stored under.
fn feed_sig_key(channel_db_id: i64) -> String {
    format!("yt_feed_sig_{channel_db_id}")
}

/// Fills in publication dates the cache is missing from a feed map, leaving
/// dates it already has untouched. Returns whether anything changed (so an
/// unchanged cache needn't be written back).
fn backfill_published(
    videos: &mut [crate::model::YtVideo],
    dates: &std::collections::HashMap<String, String>,
) -> bool {
    let mut changed = false;
    for v in videos.iter_mut() {
        if v.published.is_none() {
            if let Some(p) = dates.get(&v.video_id) {
                v.published = Some(p.clone());
                changed = true;
            }
        }
    }
    changed
}

/// Refreshes a subscribed channel's newest videos (worker thread, own DB).
/// Returns the channel title plus how many of the listed videos were **new**,
/// so a refresh can report what it actually brought in.
///
/// The channel's Atom feed answers "is there anything new?" in ~80 ms, while the
/// yt-dlp listing it would take otherwise costs well over a second (a `python3`
/// cold start plus several requests to YouTube). So the feed is asked first and
/// yt-dlp only runs when it actually reports a video we don't have — which for a
/// routine "check for new videos" is almost never.
pub(crate) fn refresh_channel_videos(
    channel_db_id: i64,
    title: &str,
    url: &str,
) -> Option<(String, usize)> {
    let lib = Library::open().ok()?;
    // The feed needs a real `UC…` id: the stored subscription key has it even
    // when the channel was added by its `/@handle` URL.
    let cid = youtube::channel_id_from_url(url).or_else(|| {
        lib.channel_yt_id(channel_db_id)
            .ok()
            .flatten()
            .filter(|c| c.starts_with("UC"))
    });
    let stored = lib.channel_videos(channel_db_id).unwrap_or_default();
    let sig_key = feed_sig_key(channel_db_id);
    let (mut feed_dates, mut feed_sig) = (None, None);
    if !stored.is_empty() {
        if let Some(cid) = cid.as_deref() {
            let dates = youtube::channel_rss_published(cid);
            if !dates.is_empty() {
                let sig = feed_signature(&dates);
                let seen_sig = lib.get_setting(&sig_key).ok().flatten();
                // Nothing new — either the feed has not moved since the last
                // listing, or everything it carries is already cached.
                if seen_sig.as_deref() == Some(sig.as_str()) || !feed_has_unknown(&dates, &stored) {
                    // Skip yt-dlp entirely; only backfill dates we are missing.
                    let mut videos = stored;
                    if backfill_published(&mut videos, &dates) {
                        let _ = lib.set_channel_videos(channel_db_id, &videos);
                    }
                    if seen_sig.as_deref() != Some(sig.as_str()) {
                        let _ = lib.set_setting(&sig_key, &sig);
                    }
                    return Some((title.to_string(), 0));
                }
                // There *is* something new: hand the dates to the listing so it
                // needn't fetch the same feed again, and remember the
                // fingerprint once that listing has actually run.
                feed_sig = Some(sig);
                feed_dates = Some(dates);
            }
        }
    }
    let mut videos = list_channel_videos(url, cid.as_deref(), feed_dates);
    if videos.is_empty() {
        return None;
    }
    let seen: std::collections::HashSet<String> =
        stored.iter().map(|v| v.video_id.clone()).collect();
    // Preserve upload dates the feed didn't return this time (e.g. a transient
    // feed failure): a refresh must not erase dates we already had.
    let known: std::collections::HashMap<String, String> = stored
        .into_iter()
        .filter_map(|v| v.published.map(|p| (v.video_id, p)))
        .collect();
    if !known.is_empty() {
        for v in videos.iter_mut() {
            if v.published.is_none() {
                v.published = known.get(&v.video_id).cloned();
            }
        }
    }
    let fresh = videos
        .iter()
        .filter(|v| !seen.contains(&v.video_id))
        .count();
    lib.set_channel_videos(channel_db_id, &videos).ok()?;
    // Only now: a feed whose listing failed must not be recorded as "handled".
    if let Some(sig) = feed_sig {
        let _ = lib.set_setting(&sig_key, &sig);
    }
    // A channel cached for the first time reports no "new" videos — the count
    // is meant for refreshes of an already-known channel.
    Some((title.to_string(), if seen.is_empty() { 0 } else { fresh }))
}

/// A watch-progress widget of a list row, kept so the transport tick can update
/// it in place while the video plays.
pub(super) struct WatchRow {
    /// Video the widget belongs to.
    pub(super) video_id: String,
    /// The widget itself (emptied and refilled on every update).
    pub(super) row: gtk::Box,
    /// Runtime in seconds, as the list knows it.
    pub(super) total_secs: Option<i64>,
}

/// One-line outcome of a "refresh all", shown briefly in the loading overlay:
/// how many channels were refreshed, what came in, and what failed.
pub(super) fn refresh_summary_text(updated: usize, failed: usize, new_videos: usize) -> String {
    let mut parts = Vec::new();
    if updated > 0 {
        parts.push(ngettext_n(
            "{n} channel updated",
            "{n} channels updated",
            updated as u32,
        ));
    }
    if new_videos > 0 {
        parts.push(ngettext_n(
            "{n} new video",
            "{n} new videos",
            new_videos as u32,
        ));
    }
    if failed > 0 {
        parts.push(ngettext_n(
            "{n} channel failed",
            "{n} channels failed",
            failed as u32,
        ));
    }
    if parts.is_empty() {
        return gettext("Nothing new");
    }
    parts.join(" · ")
}

/// Lists a channel's newest videos via yt-dlp and merges in publication dates.
/// `dates` is the channel's already-fetched Atom feed (`video_id → published`);
/// when `None` the feed is fetched here.
fn list_channel_videos(
    url: &str,
    channel_id: Option<&str>,
    dates: Option<std::collections::HashMap<String, String>>,
) -> Vec<crate::model::YtVideo> {
    let mut videos: Vec<crate::model::YtVideo> = youtube::list_entries(url, CHANNEL_VIDEO_LIMIT)
        .unwrap_or_default()
        .into_iter()
        .map(to_model_video)
        .collect();
    let dates = dates.or_else(|| channel_id.map(youtube::channel_rss_published));
    if let Some(dates) = dates.filter(|d| !d.is_empty()) {
        for v in videos.iter_mut() {
            if let Some(p) = dates.get(&v.video_id) {
                v.published = Some(p.clone());
            }
        }
    }
    let thumbs: Vec<String> = videos
        .iter()
        .map(|v| youtube::thumbnail_url(&v.video_id))
        .collect();
    crate::core::online::cache_youtube_thumbs(&thumbs);
    videos
}

#[cfg(test)]
mod tests {
    use super::{backfill_published, feed_has_unknown, feed_signature, refresh_summary_text};
    use crate::model::YtVideo;
    use std::collections::HashMap;

    fn video(id: &str, published: Option<&str>) -> YtVideo {
        YtVideo {
            video_id: id.to_string(),
            title: format!("Video {id}"),
            url: format!("https://www.youtube.com/watch?v={id}"),
            duration: Some(210),
            published: published.map(str::to_string),
            thumbnail: None,
        }
    }

    fn feed(entries: &[(&str, &str)]) -> HashMap<String, String> {
        entries
            .iter()
            .map(|(id, p)| (id.to_string(), p.to_string()))
            .collect()
    }

    #[test]
    fn a_feed_of_known_videos_means_nothing_to_fetch() {
        let stored = vec![video("a", None), video("b", None)];
        // The feed covers fewer videos than the cache – still nothing new.
        assert!(!feed_has_unknown(
            &feed(&[("a", "2026-08-01T10:00:00+00:00")]),
            &stored
        ));
        assert!(!feed_has_unknown(
            &feed(&[
                ("a", "2026-08-01T10:00:00+00:00"),
                ("b", "2026-07-01T10:00:00+00:00")
            ]),
            &stored
        ));
        // One unknown id is enough to warrant the expensive listing.
        assert!(feed_has_unknown(
            &feed(&[("c", "2026-08-30T10:00:00+00:00")]),
            &stored
        ));
    }

    #[test]
    fn empty_cache_always_counts_as_having_new_videos() {
        assert!(feed_has_unknown(
            &feed(&[("a", "2026-08-01T10:00:00+00:00")]),
            &[]
        ));
    }

    #[test]
    fn feed_signature_ignores_order_but_tracks_content() {
        let a = feed(&[
            ("a", "2026-08-01T10:00:00+00:00"),
            ("b", "2026-07-01T10:00:00+00:00"),
        ]);
        let b = feed(&[
            ("b", "2026-07-01T10:00:00+00:00"),
            ("a", "2026-08-01T10:00:00+00:00"),
        ]);
        assert_eq!(feed_signature(&a), feed_signature(&b));
        // A Short appearing in the feed changes it, even though the `/videos`
        // listing will never return that id.
        let c = feed(&[
            ("a", "2026-08-01T10:00:00+00:00"),
            ("b", "2026-07-01T10:00:00+00:00"),
            ("short", "2026-08-30T10:00:00+00:00"),
        ]);
        assert_ne!(feed_signature(&a), feed_signature(&c));
    }

    #[test]
    fn backfill_only_fills_missing_dates() {
        let mut videos = vec![
            video("a", None),
            video("b", Some("2026-01-01T00:00:00+00:00")),
        ];
        let dates = feed(&[
            ("a", "2026-08-01T10:00:00+00:00"),
            ("b", "2026-08-02T10:00:00+00:00"),
        ]);
        assert!(backfill_published(&mut videos, &dates));
        assert_eq!(
            videos[0].published.as_deref(),
            Some("2026-08-01T10:00:00+00:00")
        );
        // A date we already had is never overwritten by the feed.
        assert_eq!(
            videos[1].published.as_deref(),
            Some("2026-01-01T00:00:00+00:00")
        );
        // Nothing left to fill → no pointless DB write.
        assert!(!backfill_published(&mut videos, &dates));
    }

    #[test]
    fn summary_lists_only_the_parts_that_happened() {
        // Untranslated in the test binary, so the msgids come back verbatim.
        assert_eq!(refresh_summary_text(1, 0, 0), "1 channel updated");
        assert_eq!(
            refresh_summary_text(3, 0, 5),
            "3 channels updated · 5 new videos"
        );
        assert_eq!(
            refresh_summary_text(2, 1, 0),
            "2 channels updated · 1 channel failed"
        );
        // Nothing reached at all: still say something rather than show a blank.
        assert_eq!(refresh_summary_text(0, 0, 0), "Nothing new");
    }
}
