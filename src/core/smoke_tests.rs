//! Live smoke tests against the external services Emilia depends on.
//!
//! These talk to the real network (and, for YouTube, the installed `yt-dlp`),
//! so they are `#[ignore]`d and never run with a plain `cargo test`. They exist
//! to notice early when a service changes its API or YouTube breaks extraction –
//! the unit tests only cover the parsers against fixed fixtures.
//!
//! Run them with: `cargo test smoke_ -- --ignored --test-threads=1`
//! (one thread keeps the rate-limited services, MusicBrainz above all, happy).
//! CI runs them weekly in the `smoke` job.
//!
//! The queries use long-lived, well-known items so a failure points at the
//! service or our client code, not at vanished content.

use crate::core::{online::OnlineClient, podcast, streaming, youtube};

/// A video that has been online for well over a decade.
const YT_VIDEO_ID: &str = "dQw4w9WgXcQ";
/// The official "YouTube" channel.
const YT_CHANNEL_ID: &str = "UCBR8-60-B28hp2BmDPdntcQ";

// ---- MusicBrainz / Cover Art Archive -------------------------------------

#[test]
#[ignore]
fn smoke_musicbrainz_release_and_tracks() {
    let client = OnlineClient::new();
    let m = client
        .match_release("Daft Punk", "Discovery")
        .expect("MusicBrainz release search failed")
        .expect("no release match for Daft Punk – Discovery");
    assert!(!m.mbid.is_empty());
    assert_eq!(m.year, Some(2001), "unexpected release year");

    let tracks = client
        .fetch_release_tracks(&m.mbid)
        .expect("MusicBrainz tracklist request failed");
    assert!(tracks.len() >= 10, "tracklist too short: {}", tracks.len());
}

#[test]
#[ignore]
fn smoke_cover_art_archive() {
    let client = OnlineClient::new();
    let m = client
        .match_release("Daft Punk", "Discovery")
        .expect("MusicBrainz release search failed")
        .expect("no release match for Daft Punk – Discovery");
    let cover = client
        .fetch_cover(&m)
        .expect("Cover Art Archive request failed")
        .expect("no cover for Daft Punk – Discovery");
    assert!(cover.len() > 1000, "cover suspiciously small");
}

// ---- Deezer ---------------------------------------------------------------

#[test]
#[ignore]
fn smoke_deezer_artist_image() {
    let client = OnlineClient::new();
    let img = client
        .fetch_artist_image("Daft Punk")
        .expect("Deezer request failed")
        .expect("no artist image for Daft Punk");
    assert!(img.len() > 1000, "artist image suspiciously small");
}

#[test]
#[ignore]
fn smoke_deezer_artist_popularity() {
    let client = OnlineClient::new();
    // "Queen" has tiny namesakes ranked above the band in the search.
    let pop = client
        .artist_popularity("Queen")
        .expect("Deezer request failed")
        .expect("no popularity for Queen");
    assert!(pop.fans > 1_000_000, "picked a namesake: {} fans", pop.fans);
    let hits = pop.hits();
    assert!(
        hits.iter().any(|h| h.key == "bohemian rhapsody"),
        "Bohemian Rhapsody missing from the hits"
    );
    let rank = client
        .track_rank("Queen", "Dragon Attack")
        .expect("Deezer track search failed");
    assert!(rank.is_some_and(|r| r > 0), "no rank for Dragon Attack");
}

// ---- LRCLIB ---------------------------------------------------------------

#[test]
#[ignore]
fn smoke_lrclib_lyrics() {
    let client = OnlineClient::new();
    let lyrics = client
        .fetch_lyrics("Queen", "Bohemian Rhapsody", None, None)
        .expect("LRCLIB request failed")
        .expect("no lyrics for Queen – Bohemian Rhapsody");
    assert!(lyrics.has_any());
}

// ---- Podcasts (iTunes search + feed) --------------------------------------

#[test]
#[ignore]
fn smoke_podcast_search_and_feed() {
    let hits = podcast::search_podcasts("NPR News Now").expect("iTunes podcast search failed");
    assert!(!hits.is_empty(), "podcast search returned nothing");

    // Fetch the first hit's feed: covers HTTP, redirects and the feed parser
    // against whatever the real world serves today.
    let feed = podcast::fetch_feed(&hits[0].feed_url).expect("podcast feed fetch failed");
    assert!(!feed.title.is_empty(), "feed has no title");
    assert!(!feed.episodes.is_empty(), "feed has no episodes");
}

// ---- Radio-Browser --------------------------------------------------------

#[test]
#[ignore]
fn smoke_radio_browser_search() {
    let hits = streaming::search_stations("BBC Radio").expect("Radio-Browser search failed");
    assert!(!hits.is_empty(), "station search returned nothing");
    assert!(
        hits.iter().all(|s| !s.url.is_empty()),
        "station without URL"
    );
}

// ---- YouTube --------------------------------------------------------------

#[test]
#[ignore]
fn smoke_youtube_channel_rss() {
    let published = youtube::channel_rss_published(YT_CHANNEL_ID);
    assert!(
        !published.is_empty(),
        "channel RSS feed returned no entries"
    );
}

#[test]
#[ignore]
fn smoke_ytdlp_search() {
    assert!(youtube::available(), "yt-dlp is not installed");
    let hits = youtube::search(
        "Rick Astley Never Gonna Give You Up",
        youtube::YtKind::Video,
        3,
    )
    .expect("yt-dlp search failed");
    assert!(!hits.is_empty(), "yt-dlp search returned nothing");
}

#[test]
#[ignore]
fn smoke_ytdlp_video_meta() {
    assert!(youtube::available(), "yt-dlp is not installed");
    let meta = youtube::video_meta(YT_VIDEO_ID).expect("yt-dlp metadata extraction failed");
    assert_eq!(meta.id, YT_VIDEO_ID);
    assert!(
        meta.duration.is_some_and(|d| d > 60),
        "no plausible duration"
    );
}

#[test]
#[ignore]
fn smoke_ytdlp_resolve_audio_url() {
    assert!(youtube::available(), "yt-dlp is not installed");
    let url = youtube::resolve_audio_url(YT_VIDEO_ID).expect("yt-dlp stream resolution failed");
    assert!(url.starts_with("https://"));
}
