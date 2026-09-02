use super::*;
// Types used only by tests (their production callers moved to submodules).
use crate::model::{AlbumMeta, ArtistMeta, Episode, Track};

fn track(path: &str, artist: Option<&str>, album: Option<&str>) -> Track {
    Track {
        id: 0,
        path: path.to_string(),
        title: "T".to_string(),
        artist: artist.map(String::from),
        album: album.map(String::from),
        genre: None,
        track_no: None,
        disc_no: None,
        duration_ms: Some(60_000),
        resume_ms: 0,
        year: None,
    }
}

/// The stats query resolves podcast episodes by `audio_url`, which is not
/// the primary key — without this index that scalar subquery degrades to a
/// full scan of `episode` per played path.
#[test]
fn episode_audio_url_is_indexed() {
    let lib = Library::open_in_memory().unwrap();
    let n: i64 = lib
        .conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'index' AND name = 'idx_episode_audio_url'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1, "idx_episode_audio_url must exist");

    // And the planner actually uses it for the stats-shaped lookup.
    let plan: String = lib
        .conn
        .query_row(
            "EXPLAIN QUERY PLAN
                 SELECT title FROM episode WHERE audio_url = 'x' LIMIT 1",
            [],
            |r| r.get(3),
        )
        .unwrap();
    assert!(
        plan.contains("idx_episode_audio_url"),
        "expected an index search, got: {plan}"
    );
}

#[test]
fn migrate_stamps_schema_version() {
    let lib = Library::open_in_memory().unwrap();
    let v: i64 = lib
        .conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, SCHEMA_VERSION as i64);
}

#[test]
fn migrate_refuses_newer_schema() {
    let lib = Library::open_in_memory().unwrap();
    // Simulate a DB written by a future build.
    lib.conn
        .execute_batch(&format!("PRAGMA user_version = {};", SCHEMA_VERSION + 1))
        .unwrap();
    assert!(lib.migrate().is_err());
}

#[test]
fn play_events_aggregate_into_stats() {
    let lib = Library::open_in_memory().unwrap();
    lib.upsert_track(&track("/m/a1.mp3", Some("Alice"), Some("Album X")))
        .unwrap();
    lib.upsert_track(&track(
        "/m/a2.mp3",
        Some("Alice feat. Bob"),
        Some("Album X"),
    ))
    .unwrap();
    lib.upsert_track(&track("/m/c1.mp3", Some("Carol"), Some("Album Y")))
        .unwrap();

    // Duration of the test tracks is 60 s → threshold effectively 30 s.
    let t0: i64 = 1_700_000_000;
    lib.log_play("/m/a1.mp3", t0, 45_000, 60_000, true, Some("queue"))
        .unwrap();
    lib.log_play("/m/a1.mp3", t0 + 100, 50_000, 60_000, true, None)
        .unwrap();
    lib.log_play("/m/a2.mp3", t0 + 200, 40_000, 60_000, false, None)
        .unwrap();
    lib.log_play("/m/c1.mp3", t0 + 300, 5_000, 60_000, false, None)
        .unwrap(); // skip

    let tot = lib.stats_totals(0).unwrap();
    assert_eq!(tot.plays, 3);
    assert_eq!(tot.skips, 1);
    assert_eq!(tot.total_played_ms, 45_000 + 50_000 + 40_000 + 5_000);
    assert_eq!(tot.distinct_tracks, 2); // a1, a2 (c1 only a skip)
                                        // stats_totals leaves distinct_artists/albums at 0 — the caller fills
                                        // them from the full top lists, whose lengths (1 and 1) are asserted
                                        // below: 1 artist (Alice, a2 folds onto her) and 1 album (Album X).
    assert_eq!(tot.distinct_artists, 0);
    assert_eq!(tot.distinct_albums, 0);

    let tracks = lib.stats_top_tracks(0, 10).unwrap();
    assert_eq!(tracks.len(), 2);
    assert_eq!(tracks[0].plays, 2); // a1 twice
    assert_eq!(tracks[0].detail, "Alice");

    let artists = lib.stats_top_artists(0, 10).unwrap();
    assert_eq!(artists.len(), 1);
    assert_eq!(artists[0].name, "Alice");
    assert_eq!(artists[0].plays, 3); // a1×2 + a2×1, folded

    let albums = lib.stats_top_albums(0, 10).unwrap();
    assert_eq!(albums.len(), 1);
    assert_eq!(albums[0].name, "Album X");
    assert_eq!(albums[0].plays, 3);
    assert_eq!(albums[0].detail, "Alice");

    // last_played is tracked (forward: the later event wins).
    let lp: Option<i64> = lib
        .conn
        .query_row(
            "SELECT last_played FROM track WHERE path = ?1",
            ["/m/a1.mp3"],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(lp, Some(t0 + 100));

    // Distributions preserve the total time (checked timezone-independently).
    assert_eq!(
        lib.stats_by_hour(0).unwrap().iter().sum::<i64>(),
        tot.total_played_ms
    );
    assert_eq!(
        lib.stats_by_weekday(0).unwrap().iter().sum::<i64>(),
        tot.total_played_ms
    );

    // since filter: from t0+250 only the skip (c1) remains.
    let recent = lib.stats_totals(t0 + 250).unwrap();
    assert_eq!(recent.plays, 0);
    assert_eq!(recent.skips, 1);
}

#[test]
fn meta_attempts_count_failures_and_reset_on_cover() {
    let lib = Library::open_in_memory().unwrap();
    let mut m = AlbumMeta::pending("A", "B");

    // A fruitless search counts up: this album is simply not in the database.
    m.status = "notfound".to_string();
    lib.upsert_album_meta(&m).unwrap();
    assert_eq!(lib.album_attempts("A", "B"), 1);

    // An error does not: the service being down or rate-limiting says
    // nothing about this album, and an outage would otherwise exhaust the
    // budget of the whole library in a single sweep.
    m.status = "error".to_string();
    lib.upsert_album_meta(&m).unwrap();
    assert_eq!(lib.album_attempts("A", "B"), 1);

    // A bare "matched" *without* a cover is still an unsuccessful cover
    // attempt – otherwise the cover-less album would be re-queried on every
    // sweep forever and never reach MAX_ATTEMPTS.
    m.status = "matched".to_string();
    lib.upsert_album_meta(&m).unwrap();
    assert_eq!(lib.album_attempts("A", "B"), 2);

    // Only an actual cover (matched online or extracted locally) resets it.
    m.cover_path = Some("/cache/cover.img".to_string());
    lib.upsert_album_meta(&m).unwrap();
    assert_eq!(lib.album_attempts("A", "B"), 0);

    // A fresh failure starts counting again.
    m.status = "notfound".to_string();
    m.cover_path = None;
    lib.upsert_album_meta(&m).unwrap();
    assert_eq!(lib.album_attempts("A", "B"), 1);

    // A locally found cover resets as well.
    m.status = "local".to_string();
    m.cover_path = Some("/cache/local.img".to_string());
    lib.upsert_album_meta(&m).unwrap();
    assert_eq!(lib.album_attempts("A", "B"), 0);
}

#[test]
fn podcast_subscribe_and_episodes() {
    let lib = Library::open_in_memory().unwrap();
    let id = lib
        .subscribe_podcast(
            "Mein Podcast",
            "https://feed.example/rss",
            Some("https://img"),
        )
        .unwrap();
    // Re-subscribing to the same feed → same ID (upsert), no duplicate.
    let id2 = lib
        .subscribe_podcast("Mein Podcast (neu)", "https://feed.example/rss", None)
        .unwrap();
    assert_eq!(id, id2);

    let eps = vec![
        Episode {
            guid: Some("g1".into()),
            title: "E1".into(),
            audio_url: "https://a/1.mp3".into(),
            published: Some("Mon, 01 Jan 2024".into()),
            duration: Some("10:00".into()),
            description: Some("Shownotes 1".into()),
        },
        Episode {
            guid: None,
            title: "E2".into(),
            audio_url: "https://a/2.mp3".into(),
            published: None,
            duration: None,
            description: None,
        },
    ];
    lib.set_episodes(id, &eps).unwrap();

    let got = lib.episodes(id).unwrap();
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].title, "E1");
    assert_eq!(got[0].description.as_deref(), Some("Shownotes 1"));
    assert_eq!(got[1].audio_url, "https://a/2.mp3");

    let list = lib.podcasts().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(
        (list[0].0, list[0].1.as_str(), list[0].3),
        (id, "Mein Podcast (neu)", 2)
    );

    lib.delete_podcast(id).unwrap();
    assert!(lib.podcasts().unwrap().is_empty());
    assert!(lib.episodes(id).unwrap().is_empty());
}

#[test]
fn playlist_crud_and_items() {
    let lib = Library::open_in_memory().unwrap();
    let id = lib.create_playlist("Roadtrip").unwrap();
    assert_eq!(
        lib.playlists().unwrap(),
        vec![(id, "Roadtrip".to_string(), 0)]
    );

    // Appending preserves the order (across two calls).
    lib.add_to_playlist(id, &["/a.mp3".into(), "/b.mp3".into()])
        .unwrap();
    lib.add_to_playlist(id, &["/c.mp3".into()]).unwrap();
    assert_eq!(
        lib.playlist_paths(id).unwrap(),
        vec!["/a.mp3", "/b.mp3", "/c.mp3"]
    );
    assert_eq!(lib.playlists().unwrap()[0].2, 3); // track count

    lib.rename_playlist(id, "Tour").unwrap();
    assert_eq!(lib.playlists().unwrap()[0].1, "Tour");

    lib.remove_from_playlist(id, "/b.mp3").unwrap();
    assert_eq!(lib.playlist_paths(id).unwrap(), vec!["/a.mp3", "/c.mp3"]);

    lib.delete_playlist(id).unwrap();
    assert!(lib.playlists().unwrap().is_empty());
    assert!(lib.playlist_paths(id).unwrap().is_empty());
}

#[test]
fn youtube_channels_videos_downloads_and_progress() {
    let lib = Library::open_in_memory().unwrap();
    // Subscribe (idempotent on channel_id) and list.
    let cid = lib
        .subscribe_channel("UC123", "Some Channel", "https://yt/UC123", Some("t.jpg"))
        .unwrap();
    assert_eq!(
        lib.subscribe_channel("UC123", "Renamed", "https://yt/UC123", None)
            .unwrap(),
        cid
    );
    let channels = lib.channels().unwrap();
    assert_eq!(channels.len(), 1);
    assert_eq!(channels[0].1, "Renamed");

    // Replace cached videos and read them back in order.
    let videos = vec![
        crate::model::YtVideo {
            video_id: "v1".into(),
            title: "First".into(),
            url: "https://yt/watch?v=v1".into(),
            duration: Some(200),
            published: None,
            thumbnail: None,
        },
        crate::model::YtVideo {
            video_id: "v2".into(),
            title: "Second".into(),
            url: "https://yt/watch?v=v2".into(),
            duration: None,
            published: None,
            thumbnail: None,
        },
    ];
    lib.set_channel_videos(cid, &videos).unwrap();
    let read = lib.channel_videos(cid).unwrap();
    assert_eq!(
        read.iter().map(|v| v.video_id.as_str()).collect::<Vec<_>>(),
        ["v1", "v2"]
    );
    assert_eq!(lib.channels().unwrap()[0].4, 2); // video count
    assert_eq!(lib.all_videos().unwrap().len(), 2);

    // Deleting the channel removes its cached videos too.
    lib.delete_channel(cid).unwrap();
    assert!(lib.channels().unwrap().is_empty());
    assert!(lib.all_videos().unwrap().is_empty());
}

#[test]
fn yt_download_links_video_to_local_path_and_upserts() {
    let lib = Library::open_in_memory().unwrap();
    // Unknown video → no local copy (playback streams it).
    assert_eq!(lib.yt_download("vid").unwrap(), None);
    // Recording a download links the id to its on-disk path; this is what
    // makes a `yt:<id>` track play locally and hides "Add to library".
    lib.set_yt_download("vid", "/music/YouTube/A/song.mp3")
        .unwrap();
    assert_eq!(
        lib.yt_download("vid").unwrap().as_deref(),
        Some("/music/YouTube/A/song.mp3")
    );
    // Re-adding the same id (e.g. overwrite) updates the path, no duplicate.
    lib.set_yt_download("vid", "/music/YouTube/A/song-v2.mp3")
        .unwrap();
    assert_eq!(
        lib.yt_download("vid").unwrap().as_deref(),
        Some("/music/YouTube/A/song-v2.mp3")
    );
}

#[test]
fn youtube_recent_history_orders_and_enriches() {
    let lib = Library::open_in_memory().unwrap();
    lib.add_recent_video("a", "First", None).unwrap();
    lib.add_recent_video("b", "Second", Some("http://thumb/b.jpg"))
        .unwrap();
    // Re-playing "a" moves it back to the top.
    lib.add_recent_video("a", "First", None).unwrap();
    let recent = lib.recent_videos(10).unwrap();
    assert_eq!(
        recent
            .iter()
            .map(|r| r.video_id.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
    // Enrichment fills the artist.
    lib.set_recent_meta("a", Some("The Artist"), Some("/cache/a.img"))
        .unwrap();
    let a = lib
        .recent_videos(10)
        .unwrap()
        .into_iter()
        .find(|r| r.video_id == "a")
        .unwrap();
    assert_eq!(a.artist.as_deref(), Some("The Artist"));
}

#[test]
fn yt_playlist_mirror_keeps_same_named_user_playlist() {
    let lib = Library::open_in_memory().unwrap();
    // A user's own playlist that happens to share the YouTube playlist's name.
    let user = lib.create_playlist("Mix").unwrap();
    lib.add_to_playlist(user, &["song/mine.mp3".to_string()])
        .unwrap();

    // Mirror a YouTube playlist (different identity: an origin URL) under the
    // same name. The user playlist must survive untouched.
    let url = "https://www.youtube.com/playlist?list=PL123";
    let mirror = lib
        .replace_yt_playlist(url, "Mix", &["yt:v1".into(), "yt:v2".into()])
        .unwrap();
    assert_ne!(mirror, user, "mirror must be a distinct playlist");
    assert_eq!(
        lib.playlist_paths(user).unwrap(),
        vec!["song/mine.mp3".to_string()]
    );
    assert_eq!(lib.yt_playlist_id(url).unwrap(), Some(mirror));
    // The user playlist has no origin, so it is never matched as a mirror.
    assert_eq!(lib.playlists().unwrap().len(), 2);

    // Re-mirroring the same URL refreshes the SAME mirror in place (no
    // duplicate, contents replaced) and still leaves the user playlist alone.
    let mirror2 = lib
        .replace_yt_playlist(url, "Mix", &["yt:v3".into()])
        .unwrap();
    assert_eq!(mirror2, mirror);
    assert_eq!(
        lib.playlist_paths(mirror).unwrap(),
        vec!["yt:v3".to_string()]
    );
    assert_eq!(
        lib.playlist_paths(user).unwrap(),
        vec!["song/mine.mp3".to_string()]
    );
    assert_eq!(lib.playlists().unwrap().len(), 2);
}

#[test]
fn add_artist_image_appends_and_dedups() {
    let lib = Library::open_in_memory().unwrap();
    let a = "Some Artist";
    // The fanart gallery (replace) plus a preserved old photo (append).
    lib.set_artist_images(a, &[("/g0.img".into(), "photo".into(), "fanart".into())])
        .unwrap();
    lib.add_artist_image(a, "/old.img", "photo", "local")
        .unwrap();
    // Re-adding the same path is a no-op (dedup).
    lib.add_artist_image(a, "/old.img", "photo", "local")
        .unwrap();
    assert_eq!(
        lib.artist_images(a).unwrap(),
        vec!["/g0.img".to_string(), "/old.img".to_string()]
    );
}

#[test]
fn yt_playlist_cache_roundtrips_and_upserts() {
    let lib = Library::open_in_memory().unwrap();
    let url = "https://www.youtube.com/playlist?list=PLcache";
    assert!(lib.yt_playlist_cache(url).unwrap().is_none());

    lib.set_yt_playlist_cache(url, "Mix", "[1,2,3]").unwrap();
    let (songs, fetched_at) = lib.yt_playlist_cache(url).unwrap().unwrap();
    assert_eq!(songs, "[1,2,3]");
    assert!(fetched_at > 0);

    // Re-caching the same url replaces the songs in place (no duplicate row).
    lib.set_yt_playlist_cache(url, "Mix", "[4]").unwrap();
    assert_eq!(lib.yt_playlist_cache(url).unwrap().unwrap().0, "[4]");
    // It is a plain cache, never a visible playlist.
    assert!(lib.playlists().unwrap().is_empty());
}

#[test]
fn area_filtering_hides_from_listings() {
    use crate::core::category::{album_key, areas_value, Area};
    let lib = Library::open_in_memory().unwrap();
    lib.upsert_track(&track("/x/1.mp3", Some("X"), Some("Y")))
        .unwrap();
    // Default: visible in albums and artists.
    assert_eq!(lib.albums_overview_with(None).unwrap().len(), 1);
    assert_eq!(lib.artists_overview_with(None).unwrap().len(), 1);

    // Take the album out of "Albums" (now only filesystem + artists).
    lib.set_category(
        "album",
        &album_key("X", "Y"),
        Some(&areas_value(&[Area::Filesystem, Area::Artists])),
    )
    .unwrap();
    assert!(lib.albums_overview_with(None).unwrap().is_empty());
    assert_eq!(lib.artists_overview_with(None).unwrap().len(), 1);

    // Hide the artist completely.
    lib.set_category("artist", "X", Some("")).unwrap();
    assert!(lib.artists_overview_with(None).unwrap().is_empty());
}

#[test]
fn album_inherits_parent_folder_area() {
    use crate::core::category::Area;
    let lib = Library::open_in_memory().unwrap();
    lib.upsert_track(&track("/musik/Live/1.mp3", Some("X"), Some("Konzert")))
        .unwrap();
    // Default: the album is visible in the "Albums" area.
    assert!(lib.album_areas("X", "Konzert").contains(&Area::Albums));
    // Hide the parent folder (empty area list).
    lib.set_category("folder", "/musik/Live", Some("")).unwrap();
    // The album without its own setting now inherits the folder → hidden.
    assert!(lib.album_areas("X", "Konzert").is_empty());
    // Its own album setting still wins (non-destructive).
    lib.set_category(
        "album",
        &crate::core::category::album_key("X", "Konzert"),
        Some("albums"),
    )
    .unwrap();
    assert!(lib.album_areas("X", "Konzert").contains(&Area::Albums));
}

#[test]
fn albums_overview_merges_feat_variants() {
    let lib = Library::open_in_memory().unwrap();
    for (path, artist) in [
        ("/1.mp3", "Beginner"),
        ("/2.mp3", "Beginner feat. Megaloh"),
        ("/3.mp3", "Beginner feat. Gzuz & Gentleman"),
    ] {
        lib.upsert_track(&track(path, Some(artist), Some("Advanced Chemistry")))
            .unwrap();
    }
    let albums = lib.albums_overview_with(None).unwrap();
    let ac: Vec<_> = albums
        .iter()
        .filter(|a| a.album == "Advanced Chemistry")
        .collect();
    // feat. variants of the same primary artist → exactly ONE card.
    assert_eq!(ac.len(), 1);
    assert_eq!(ac[0].artist, "Beginner");
    assert_eq!(ac[0].track_count, 3);
}

#[test]
fn albums_overview_uses_representative_cover_for_compilations() {
    let lib = Library::open_in_memory().unwrap();
    // Compilation: several artists with different covers. The card shows the
    // cover of the dominant artist (most tracks) instead of dropping it — a
    // representative image beats an empty placeholder and matches the cover
    // shown on the album detail page.
    for (path, artist, cover) in [
        ("/c1.mp3", "DJ A", "/covers/a.jpg"),
        ("/c2.mp3", "DJ A", "/covers/a.jpg"),
        ("/c3.mp3", "DJ B", "/covers/b.jpg"),
    ] {
        lib.upsert_track(&track(path, Some(artist), Some("Dancemix 2009")))
            .unwrap();
        let mut m = crate::model::AlbumMeta::pending(artist, "Dancemix 2009");
        m.cover_path = Some(cover.to_string());
        m.status = "local".to_string();
        lib.upsert_album_meta(&m).unwrap();
    }
    let dm = lib
        .albums_overview_with(None)
        .unwrap()
        .into_iter()
        .find(|a| a.album == "Dancemix 2009")
        .unwrap();
    // DJ A has the most tracks → its cover represents the compilation.
    assert_eq!(dm.artist, "DJ A");
    assert_eq!(dm.cover_path.as_deref(), Some("/covers/a.jpg"));

    // Real album by one artist → cover is retained.
    lib.upsert_track(&track("/d1.mp3", Some("Solo"), Some("Werk")))
        .unwrap();
    let mut m = crate::model::AlbumMeta::pending("Solo", "Werk");
    m.cover_path = Some("/covers/werk.jpg".to_string());
    m.status = "local".to_string();
    lib.upsert_album_meta(&m).unwrap();
    let werk = lib
        .albums_overview_with(None)
        .unwrap()
        .into_iter()
        .find(|a| a.album == "Werk")
        .unwrap();
    assert_eq!(werk.cover_path.as_deref(), Some("/covers/werk.jpg"));
}

#[test]
fn albums_overview_groups_by_name_ignoring_artist() {
    let lib = Library::open_in_memory().unwrap();
    // Same album name, different artists → exactly ONE card.
    for (path, artist) in [
        ("/a1.mp3", "Artist A"),
        ("/a2.mp3", "Artist A"),
        ("/b1.mp3", "Artist B"),
    ] {
        lib.upsert_track(&track(path, Some(artist), Some("Live")))
            .unwrap();
    }
    let live: Vec<_> = lib
        .albums_overview_with(None)
        .unwrap()
        .into_iter()
        .filter(|a| a.album == "Live")
        .collect();
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].track_count, 3);
    // Display artist = the one with the most tracks (A: 2 > B: 1).
    assert_eq!(live[0].artist, "Artist A");
}

#[test]
fn tracks_by_album_name_loads_only_that_album_case_insensitive() {
    let lib = Library::open_in_memory().unwrap();
    lib.upsert_track(&track("/a/1.mp3", Some("A"), Some("Live")))
        .unwrap();
    lib.upsert_track(&track("/a/2.mp3", Some("A"), Some("Other")))
        .unwrap();

    let paths: Vec<String> = lib
        .tracks_by_album_name("live")
        .unwrap()
        .into_iter()
        .map(|t| t.path)
        .collect();
    assert_eq!(paths, vec!["/a/1.mp3".to_string()]);
}

#[test]
fn album_track_paths_by_name_ignores_artist_credit() {
    let lib = Library::open_in_memory().unwrap();
    lib.upsert_track(&track("/b.mp3", Some("B"), Some("Shared")))
        .unwrap();
    lib.upsert_track(&track("/a.mp3", Some("A"), Some("Shared")))
        .unwrap();
    lib.upsert_track(&track("/x.mp3", Some("A"), Some("Other")))
        .unwrap();

    assert_eq!(
        lib.album_track_paths_by_name("Shared").unwrap(),
        vec!["/a.mp3".to_string(), "/b.mp3".to_string()]
    );
}

#[test]
fn multi_disc_tracks_ordered_by_disc_then_track() {
    let lib = Library::open_in_memory().unwrap();
    // Two CDs, deliberately inserted "the wrong way round".
    let rows = [
        ("/al/d2t2.mp3", 2u32, 2u32),
        ("/al/d1t1.mp3", 1, 1),
        ("/al/d2t1.mp3", 2, 1),
        ("/al/d1t2.mp3", 1, 2),
    ];
    for (path, disc, no) in rows {
        let mut t = track(path, Some("X"), Some("Doppelalbum"));
        t.disc_no = Some(disc);
        t.track_no = Some(no);
        lib.upsert_track(&t).unwrap();
    }
    let got: Vec<(Option<u32>, Option<u32>)> = lib
        .all_tracks()
        .unwrap()
        .into_iter()
        .map(|t| (t.disc_no, t.track_no))
        .collect();
    // First disc 1 (track 1,2), then disc 2 (track 1,2).
    assert_eq!(
        got,
        vec![
            (Some(1), Some(1)),
            (Some(1), Some(2)),
            (Some(2), Some(1)),
            (Some(2), Some(2)),
        ]
    );
}

#[test]
fn resume_roundtrip_by_path() {
    let lib = Library::open_in_memory().unwrap();
    lib.upsert_track(&track("/a/hoerspiel.mp3", Some("X"), Some("Y")))
        .unwrap();

    // A freshly scanned track has no resume position.
    let t = lib.track_by_path("/a/hoerspiel.mp3").unwrap().unwrap();
    assert_eq!(t.resume_ms, 0);

    // Store the position and read it back.
    lib.set_resume_path("/a/hoerspiel.mp3", 123_456).unwrap();
    let t = lib.track_by_path("/a/hoerspiel.mp3").unwrap().unwrap();
    assert_eq!(t.resume_ms, 123_456);

    // Reset (track listened to the end).
    lib.set_resume_path("/a/hoerspiel.mp3", 0).unwrap();
    assert_eq!(
        lib.track_by_path("/a/hoerspiel.mp3")
            .unwrap()
            .unwrap()
            .resume_ms,
        0
    );
}

#[test]
fn track_by_path_unknown_is_none_and_setresume_noop() {
    let lib = Library::open_in_memory().unwrap();
    assert!(lib.track_by_path("/nicht/da.mp3").unwrap().is_none());
    // Unknown path: no error, no effect.
    lib.set_resume_path("/nicht/da.mp3", 5000).unwrap();
    assert!(lib.track_by_path("/nicht/da.mp3").unwrap().is_none());
}

#[test]
fn area_cascade_resolution() {
    use crate::core::category::Area;
    let lib = Library::open_in_memory().unwrap();
    // Without a setting: default = filesystem/artists/albums.
    assert_eq!(
        lib.resolve_areas(Some("X"), Some("Y"), "/a/1.mp3"),
        Area::DEFAULT.to_vec()
    );

    // Artist level = audiobooks only → inherited by album and track.
    lib.set_category("artist", "X", Some("audiobooks")).unwrap();
    assert_eq!(
        lib.resolve_areas(Some("X"), Some("Y"), "/a/1.mp3"),
        vec![Area::Audiobooks]
    );
    assert_eq!(lib.album_areas("X", "Y"), vec![Area::Audiobooks]);

    // Track level wins: empty list = hidden.
    lib.set_category("track", "/a/1.mp3", Some("")).unwrap();
    assert!(lib
        .resolve_areas(Some("X"), Some("Y"), "/a/1.mp3")
        .is_empty());
    // album_areas/artist_areas ignore the track level.
    assert_eq!(lib.album_areas("X", "Y"), vec![Area::Audiobooks]);
}

// ---- Equalizer cascade ----

fn bands(v: f64) -> [f64; 10] {
    [v; 10]
}

#[test]
fn eq_none_when_unset() {
    let lib = Library::open_in_memory().unwrap();
    assert_eq!(lib.resolve_eq("", Some("X"), Some("Y"), "/a/1.mp3"), None);
    assert_eq!(
        lib.resolve_eq("sink1", Some("X"), Some("Y"), "/a/1.mp3"),
        None
    );
}

#[test]
fn eq_specificity_track_over_album_over_artist_over_global() {
    let lib = Library::open_in_memory().unwrap();
    let ak = crate::core::category::album_key("X", "Y");
    lib.set_eq("", "global", "", &bands(1.0)).unwrap();
    lib.set_eq("", "artist", "X", &bands(2.0)).unwrap();
    lib.set_eq("", "album", &ak, &bands(3.0)).unwrap();
    lib.set_eq("", "track", "/a/1.mp3", &bands(4.0)).unwrap();

    // The most specific level wins; after removal the next-higher one takes effect.
    let r = |l: &Library| l.resolve_eq("", Some("X"), Some("Y"), "/a/1.mp3");
    assert_eq!(r(&lib), Some(bands(4.0)));
    lib.clear_eq("", "track", "/a/1.mp3").unwrap();
    assert_eq!(r(&lib), Some(bands(3.0)));
    lib.clear_eq("", "album", &ak).unwrap();
    assert_eq!(r(&lib), Some(bands(2.0)));
    lib.clear_eq("", "artist", "X").unwrap();
    assert_eq!(r(&lib), Some(bands(1.0)));
    lib.clear_eq("", "global", "").unwrap();
    assert_eq!(r(&lib), None);
}

#[test]
fn eq_bypass_preserves_bands_and_resolves_flat() {
    let lib = Library::open_in_memory().unwrap();
    lib.set_eq("", "track", "/a/1.mp3", &bands(4.0)).unwrap();

    lib.set_eq_enabled("", "track", "/a/1.mp3", false).unwrap();
    assert_eq!(
        lib.get_eq("", "track", "/a/1.mp3").unwrap(),
        Some(bands(4.0))
    );
    assert_eq!(
        lib.resolve_eq("", Some("X"), Some("Y"), "/a/1.mp3"),
        Some(bands(0.0))
    );

    lib.set_eq_enabled("", "track", "/a/1.mp3", true).unwrap();
    assert_eq!(
        lib.resolve_eq("", Some("X"), Some("Y"), "/a/1.mp3"),
        Some(bands(4.0))
    );
}

#[test]
fn eq_concrete_output_cascade_beats_default_output() {
    let lib = Library::open_in_memory().unwrap();
    // Default output: specific track setting.
    lib.set_eq("", "track", "/a/1.mp3", &bands(4.0)).unwrap();
    // Concrete output: only a global setting.
    lib.set_eq("sink1", "global", "", &bands(9.0)).unwrap();
    // Documented behavior: the concrete output is resolved completely first
    // -- its global beats the track of the default output.
    assert_eq!(
        lib.resolve_eq("sink1", Some("X"), Some("Y"), "/a/1.mp3"),
        Some(bands(9.0))
    );
    // For the default output itself the track setting still applies.
    assert_eq!(
        lib.resolve_eq("", Some("X"), Some("Y"), "/a/1.mp3"),
        Some(bands(4.0))
    );
}

#[test]
fn eq_falls_back_to_default_output() {
    let lib = Library::open_in_memory().unwrap();
    lib.set_eq("", "global", "", &bands(1.0)).unwrap();
    // Concrete output has nothing → fall back to the default output.
    assert_eq!(
        lib.resolve_eq("sink1", Some("X"), Some("Y"), "/a/1.mp3"),
        Some(bands(1.0))
    );
}

#[test]
fn eq_stream_station_over_global_with_output_cascade() {
    let lib = Library::open_in_memory().unwrap();
    // A per-station setting wins over the global one.
    lib.set_eq("", "global", "", &bands(1.0)).unwrap();
    lib.set_eq("", "stream", "42", &bands(5.0)).unwrap();
    assert_eq!(lib.resolve_eq_stream("", "42"), Some(bands(5.0)));
    // A station without its own setting inherits the global.
    assert_eq!(lib.resolve_eq_stream("", "99"), Some(bands(1.0)));
    // Concrete output is resolved fully first (its global beats the default
    // output's station), then the default output is the basis.
    lib.set_eq("sink1", "global", "", &bands(7.0)).unwrap();
    assert_eq!(lib.resolve_eq_stream("sink1", "42"), Some(bands(7.0)));
    // An output with nothing of its own falls back to the default output's
    // global as the basis.
    assert_eq!(lib.resolve_eq_stream("sink2", "7"), Some(bands(1.0)));
}

#[test]
fn eq_stream_none_when_unset() {
    let lib = Library::open_in_memory().unwrap();
    // No stream and no global anywhere → neutral (None).
    assert_eq!(lib.resolve_eq_stream("sink1", "42"), None);
}

#[test]
fn eq_scope_migration_keeps_old_settings() {
    let lib = Library::open_in_memory().unwrap();
    // Rebuild eq_setting in the pre-podcast shape (CHECK without
    // 'podcast'/'episode') with an existing per-station setting …
    lib.conn
        .execute_batch(
            r#"
                DROP TABLE eq_setting;
                CREATE TABLE eq_setting (
                    output TEXT NOT NULL DEFAULT '',
                    scope  TEXT NOT NULL CHECK(scope IN ('global','artist','album','track','stream')),
                    key    TEXT NOT NULL,
                    bands  TEXT NOT NULL,
                    enabled INTEGER NOT NULL DEFAULT 1,
                    PRIMARY KEY (output, scope, key)
                );
                "#,
        )
        .unwrap();
    lib.set_eq("", "stream", "42", &bands(5.0)).unwrap();
    // … then re-run the migrations: the table is rebuilt with the new
    // CHECK, the old setting survives, and the new scopes are accepted.
    lib.migrate().unwrap();
    assert_eq!(lib.resolve_eq_stream("", "42"), Some(bands(5.0)));
    lib.set_eq("", "podcast", "7", &bands(3.0)).unwrap();
    lib.set_eq("", "episode", "https://example.org/e.mp3", &bands(4.0))
        .unwrap();
}

#[test]
fn eq_podcast_episode_over_podcast_over_global() {
    let lib = Library::open_in_memory().unwrap();
    let url = "https://example.org/ep1.mp3";
    lib.set_eq("", "global", "", &bands(1.0)).unwrap();
    lib.set_eq("", "podcast", "7", &bands(3.0)).unwrap();
    // Podcast level beats global; an episode of another podcast inherits global.
    assert_eq!(lib.resolve_eq_podcast("", Some("7"), url), Some(bands(3.0)));
    assert_eq!(lib.resolve_eq_podcast("", Some("8"), url), Some(bands(1.0)));
    // Episode level beats the podcast level.
    lib.set_eq("", "episode", url, &bands(5.0)).unwrap();
    assert_eq!(lib.resolve_eq_podcast("", Some("7"), url), Some(bands(5.0)));
    // Concrete output is resolved fully first, then the default output.
    lib.set_eq("sink1", "podcast", "7", &bands(7.0)).unwrap();
    assert_eq!(
        lib.resolve_eq_podcast("sink1", Some("7"), "https://example.org/ep2.mp3"),
        Some(bands(7.0))
    );
    // Nothing set anywhere → neutral (None).
    let empty = Library::open_in_memory().unwrap();
    assert_eq!(empty.resolve_eq_podcast("", Some("7"), url), None);
}

#[test]
fn eq_album_key_avoids_cross_artist_collision() {
    let lib = Library::open_in_memory().unwrap();
    let ak = crate::core::category::album_key("X", "Y");
    lib.set_eq("", "album", &ak, &bands(3.0)).unwrap();
    // Same album name, different artist → no match at the album level.
    assert_eq!(lib.resolve_eq("", Some("Z"), Some("Y"), "/a/1.mp3"), None);
    // Correct artist → match.
    assert_eq!(
        lib.resolve_eq("", Some("X"), Some("Y"), "/a/1.mp3"),
        Some(bands(3.0))
    );
}

#[test]
fn prune_removes_only_missing_files_under_root() {
    let lib = Library::open_in_memory().unwrap();
    lib.upsert_track(&track("/music/a.mp3", Some("A"), Some("X")))
        .unwrap();
    lib.upsert_track(&track("/music/gone.mp3", Some("A"), Some("X")))
        .unwrap();
    // A remote (Nextcloud) track and a track from another folder must survive.
    lib.upsert_track(&track("nc:7:Album/r.mp3", Some("A"), Some("X")))
        .unwrap();
    lib.upsert_track(&track("/other/b.mp3", Some("B"), Some("Y")))
        .unwrap();

    // Scan of /music found only a.mp3 (gone.mp3 was deleted on disk).
    let present = vec!["/music/a.mp3".to_string()];
    let removed = lib
        .prune_tracks_under(std::path::Path::new("/music"), &present)
        .unwrap();
    assert_eq!(removed, 1);
    assert!(lib.track_by_path("/music/a.mp3").unwrap().is_some());
    assert!(lib.track_by_path("/music/gone.mp3").unwrap().is_none());
    assert!(lib.track_by_path("nc:7:Album/r.mp3").unwrap().is_some());
    assert!(lib.track_by_path("/other/b.mp3").unwrap().is_some());
}

#[test]
fn prune_with_empty_scan_keeps_everything() {
    // Guards against a transiently unreadable/unmounted folder wiping the DB.
    let lib = Library::open_in_memory().unwrap();
    lib.upsert_track(&track("/music/a.mp3", Some("A"), Some("X")))
        .unwrap();
    let removed = lib
        .prune_tracks_under(std::path::Path::new("/music"), &[])
        .unwrap();
    assert_eq!(removed, 0);
    assert!(lib.track_by_path("/music/a.mp3").unwrap().is_some());
}

/// A cleared image cache must put the affected albums/artists back into the
/// enrichment queue — but a library that is merely unreachable must not lose
/// its pointers.
#[test]
fn prune_lost_images_drops_only_vanished_pointers() {
    let dir = std::env::temp_dir().join(format!("emilia-prune-images-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let here = dir.join("cover.jpg");
    std::fs::write(&here, b"jpeg").unwrap();
    let here = here.to_string_lossy().to_string();
    // Same folder, file deleted → the cache was cleared behind our back.
    let gone = dir.join("gone.jpg").to_string_lossy().to_string();
    // Whole folder away → an unmounted library, keeps its pointer.
    let unmounted = "/emilia-not-mounted/cover.jpg".to_string();

    let lib = Library::open_in_memory().unwrap();
    for (album, cover) in [("Here", &here), ("Gone", &gone), ("Unmounted", &unmounted)] {
        lib.upsert_track(&track(
            &format!("/music/{album}.mp3"),
            Some("A"),
            Some(album),
        ))
        .unwrap();
        let mut m = AlbumMeta::pending("A", album);
        m.cover_path = Some(cover.clone());
        m.status = "local".to_string();
        lib.upsert_album_meta(&m).unwrap();
    }
    for (name, image) in [("Here", &here), ("Gone", &gone)] {
        let mut m = ArtistMeta::pending(name);
        m.image_path = Some(image.clone());
        m.status = "matched".to_string();
        lib.upsert_artist_meta(&m).unwrap();
    }
    lib.set_album_images(
        "A",
        "Here",
        &[
            (here.clone(), "cover".to_string(), "test".to_string()),
            (gone.clone(), "cover".to_string(), "test".to_string()),
        ],
    )
    .unwrap();

    // One album cover, one artist photo, one gallery entry.
    assert_eq!(lib.prune_lost_images().unwrap(), 3);

    let cover_of = |album: &str| lib.get_album_meta("A", album).unwrap().unwrap().cover_path;
    assert_eq!(cover_of("Here"), Some(here.clone()));
    assert_eq!(cover_of("Gone"), None);
    assert_eq!(cover_of("Unmounted"), Some(unmounted));
    assert_eq!(lib.album_images("A", "Here").unwrap(), vec![here.clone()]);

    let artist = |name: &str| lib.get_artist_meta(name).unwrap().unwrap();
    assert_eq!(artist("Here").image_path, Some(here));
    assert_eq!(artist("Here").status, "matched");
    assert_eq!(artist("Gone").image_path, None);
    // Back to `pending`: the enrichment skips artists that are `matched`.
    assert_eq!(artist("Gone").status, "pending");

    // Only the album whose file vanished is queued for a new cover.
    let missing: Vec<String> = lib
        .albums_missing_cover()
        .unwrap()
        .into_iter()
        .map(|(_, album, _)| album)
        .collect();
    assert_eq!(missing, vec!["Gone".to_string()]);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn prune_escapes_like_metacharacters_in_root() {
    // A root containing `%`/`_` must match literally, not as LIKE wildcards.
    let lib = Library::open_in_memory().unwrap();
    lib.upsert_track(&track("/m%/keep.mp3", Some("A"), Some("X")))
        .unwrap();
    lib.upsert_track(&track("/mX/other.mp3", Some("A"), Some("X")))
        .unwrap();
    // Scan of "/m%" found nothing under it → keep.mp3 is an orphan there,
    // but "/mX/other.mp3" must NOT be touched (would match if `%` were a
    // wildcard).
    let removed = lib
        .prune_tracks_under(std::path::Path::new("/m%"), &["/m%/x.mp3".to_string()])
        .unwrap();
    assert_eq!(removed, 1);
    assert!(lib.track_by_path("/m%/keep.mp3").unwrap().is_none());
    assert!(lib.track_by_path("/mX/other.mp3").unwrap().is_some());
}

#[test]
fn albums_with_year_filters_by_artist_and_range() {
    let lib = Library::open_in_memory().unwrap();
    let yt = |path: &str, artist: &str, album: &str, year: Option<i32>| Track {
        year,
        ..track(path, Some(artist), Some(album))
    };
    lib.upsert_tracks(&[
        // "The Game": one track tagged 1980, a reissue track tagged 1991 →
        // MIN() must report the original 1980.
        yt("/q/game1.mp3", "Queen", "The Game", Some(1980)),
        yt("/q/game2.mp3", "Queen", "The Game", Some(1991)),
        yt("/q/works.mp3", "Queen", "The Works", Some(1984)),
        yt("/q/magic.mp3", "Queen", "A Kind of Magic", Some(1986)),
        yt("/q/untagged.mp3", "Queen", "Mystery", None),
        yt("/b/bowie.mp3", "David Bowie", "Let's Dance", Some(1983)),
    ])
    .unwrap();

    // Artist + inclusive range: "The Works" (1984) is in, the 1991 reissue
    // does not push "The Game" out (earliest year wins), 1986 is excluded,
    // and the untagged album is excluded because a year filter needs a year.
    let in_range = lib
        .albums_with_year(Some("Queen"), Some(1980), Some(1985))
        .unwrap();
    assert_eq!(
        in_range,
        vec![
            ("Queen".into(), "The Game".into(), Some(1980)),
            ("Queen".into(), "The Works".into(), Some(1984)),
        ]
    );

    // The artist filter is exact: David Bowie's 1983 album is not returned.
    assert!(in_range.iter().all(|(a, _, _)| a == "Queen"));

    // No filter at all keeps untagged albums (year = None) and orders by
    // artist then album.
    let all = lib.albums_with_year(None, None, None).unwrap();
    assert!(all.iter().any(|(_, al, y)| al == "Mystery" && y.is_none()));
    assert!(all.iter().any(|(a, _, _)| a == "David Bowie"));
}

#[test]
fn summaries_count_collaborations_and_runtime() {
    let lib = Library::open_in_memory().unwrap();
    let yt = |path: &str, artist: &str, album: &str, year: i32, dur: i64| Track {
        year: Some(year),
        duration_ms: Some(dur),
        ..track(path, Some(artist), Some(album))
    };
    lib.upsert_tracks(&[
        yt("/q/1.mp3", "Queen", "The Game", 1980, 200_000),
        yt("/q/2.mp3", "Queen", "The Game", 1980, 100_000),
        yt("/q/3.mp3", "Queen", "Hot Space", 1982, 300_000),
        // A collaboration: contributes to BOTH Queen and David Bowie.
        yt("/q/4.mp3", "Queen & David Bowie", "Hot Space", 1982, 60_000),
    ])
    .unwrap();

    // Queen: 2 distinct albums, all 4 songs (incl. the collab), full runtime.
    let (albums, songs, dur) = lib.artist_summary("queen").unwrap();
    assert_eq!((albums, songs), (2, 4));
    assert_eq!(dur, 660_000);
    // David Bowie picks up only the collaboration track.
    assert_eq!(lib.artist_summary("David Bowie").unwrap(), (1, 1, 60_000));

    // album_summary: "The Game" (disambiguated by artist) → 2 tracks, runtime,
    // earliest year.
    assert_eq!(
        lib.album_summary(Some("Queen"), "the game").unwrap(),
        (2, 300_000, Some(1980))
    );

    // Overview: 4 tracks, 2 individual artists (Queen, David Bowie via split),
    // 3 distinct (artist, album) pairs, summed runtime.
    let o = lib.library_overview().unwrap();
    assert_eq!((o.tracks, o.artists, o.albums), (4, 2, 3));
    assert_eq!(o.music_duration_ms, 660_000);
}

/// A non-ASCII artist name must count every casing of itself. SQLite's LIKE
/// folds ASCII only — `'BJÖRK' LIKE '%Björk%'` is false — while `norm_key`
/// lowercases with full Unicode rules, so the query-side prefilter has to
/// step aside for such a name and let the Unicode-aware match decide.
/// Otherwise the differently-cased tracks silently go missing from the count.
#[test]
fn artist_summary_counts_every_casing_of_a_non_ascii_name() {
    let lib = Library::open_in_memory().unwrap();
    let t = |path: &str, artist: &str, album: &str, year: i32| Track {
        duration_ms: Some(100_000),
        year: Some(year),
        ..track(path, Some(artist), Some(album))
    };
    lib.upsert_tracks(&[
        t("/b/1.mp3", "Björk", "Post", 1995),
        t("/b/2.mp3", "BJÖRK", "Post", 1995),
        t("/b/3.mp3", "björk", "Homogenic", 1997),
    ])
    .unwrap();

    for spelling in ["Björk", "BJÖRK", "björk"] {
        assert_eq!(
            lib.artist_summary(spelling).unwrap(),
            (2, 3, 300_000),
            "looking the artist up as {spelling:?} must find all three tracks"
        );
    }
    // The ASCII prefilter path stays exact for an ASCII name.
    assert_eq!(lib.artist_summary("Nobody").unwrap(), (0, 0, 0));
}

#[test]
fn album_classification_uses_primary_artist() {
    use crate::model::AlbumKind;
    let lib = Library::open_in_memory().unwrap();
    let yt = |path: &str, artist: &str, album: &str| Track {
        year: Some(2000),
        ..track(path, Some(artist), Some(album))
    };
    lib.upsert_tracks(&[
        // Compilation: one album name, several primary artists.
        yt("/c/1.mp3", "Al Hirt", "Kill Bill"),
        yt("/c/2.mp3", "Charlie Feathers", "Kill Bill"),
        yt("/c/3.mp3", "Santa Esmeralda", "Kill Bill"),
        // Solo album WITH a feat. guest → one primary, so NOT a compilation,
        // and the guest track must not split off the count (4 tracks total).
        yt("/b/1.mp3", "Beginner", "Bambule"),
        yt("/b/2.mp3", "Beginner feat. Samy Deluxe", "Bambule"),
        yt("/b/3.mp3", "Beginner", "Bambule"),
        yt("/b/4.mp3", "Beginner", "Bambule"),
        // Single: one artist, ≤3 tracks.
        yt("/s/1.mp3", "Nina Chuba", "Wildberry Lillet"),
        // Best-of with one guest credit: a second primary, but the main
        // artist owns ≥70% → a regular album (>3 tracks), NOT a compilation.
        yt("/d/1.mp3", "Die Ärzte", "Bäst of"),
        yt("/d/2.mp3", "Die Ärzte", "Bäst of"),
        yt("/d/3.mp3", "Die Ärzte", "Bäst of"),
        yt("/d/4.mp3", "Die Ärzte", "Bäst of"),
        yt("/d/5.mp3", "Götz Alsmann feat. Die Ärzte", "Bäst of"),
    ])
    .unwrap();

    let comps = lib.albums_classified(AlbumKind::Compilation).unwrap();
    assert_eq!(comps.len(), 1);
    assert_eq!(comps[0].album, "Kill Bill");
    assert_eq!(comps[0].artist, "Various Artists");
    assert_eq!(comps[0].tracks, 3);

    // "Wildberry Lillet" is a single; "Bambule" (4 via primary) and the
    // compilation are not.
    let singles = lib.albums_classified(AlbumKind::Single).unwrap();
    assert!(singles.iter().any(|a| a.album == "Wildberry Lillet"));
    assert!(singles
        .iter()
        .all(|a| a.album != "Bambule" && a.album != "Kill Bill"));

    // "Bambule" is a regular album; the compilation is excluded; and the
    // dominated best-of is the main artist's album, not a compilation.
    let albums = lib.albums_classified(AlbumKind::Album).unwrap();
    assert!(albums.iter().any(|a| a.album == "Bambule" && a.tracks == 4));
    assert!(albums.iter().all(|a| a.album != "Kill Bill"));
    assert!(albums
        .iter()
        .any(|a| a.album == "Bäst of" && a.artist == "Die Ärzte"));

    // Manual override wins over the heuristic: force "Kill Bill" to album,
    // and "Wildberry Lillet" (a single) to compilation.
    lib.set_album_kind("kill bill", AlbumKind::Album).unwrap();
    lib.set_album_kind("Wildberry Lillet", AlbumKind::Compilation)
        .unwrap();
    let comps = lib.albums_classified(AlbumKind::Compilation).unwrap();
    assert!(comps.iter().any(|a| a.album == "Wildberry Lillet"));
    assert!(comps.iter().all(|a| a.album != "Kill Bill"));
    let albums = lib.albums_classified(AlbumKind::Album).unwrap();
    assert!(albums.iter().any(|a| a.album == "Kill Bill"));

    // Reverting the overrides restores the heuristic.
    lib.clear_album_kind("kill bill").unwrap();
    lib.clear_album_kind("Wildberry Lillet").unwrap();
    let comps = lib.albums_classified(AlbumKind::Compilation).unwrap();
    assert!(comps.iter().any(|a| a.album == "Kill Bill"));

    // The area-based overview (covers/durations) agrees with the kind-aware
    // default: a single shows up under the Singles area; a regular album does
    // not. (Both still appear under Albums — Singles is an extra view.)
    use crate::core::category::Area;
    let single_cards = lib.albums_overview_in_area(Area::Singles, None).unwrap();
    assert!(single_cards.iter().any(|m| m.album == "Wildberry Lillet"));
    assert!(single_cards.iter().all(|m| m.album != "Bambule"));
    let album_cards = lib.albums_overview_in_area(Area::Albums, None).unwrap();
    assert!(album_cards.iter().any(|m| m.album == "Bambule"));
    assert!(album_cards.iter().any(|m| m.album == "Wildberry Lillet"));
}

#[test]
fn search_excludes_hidden_items() {
    use crate::core::category::album_key;
    let lib = Library::open_in_memory().unwrap();
    lib.upsert_tracks(&[
        // 4 tracks so "Visible Album" stays a real Album (> SINGLE_MAX) and
        // lands in `albums`, not the Singles bucket — search now files
        // album-like hits by category.
        track("/m/v.mp3", Some("Visible Artist"), Some("Visible Album")),
        track("/m/v2.mp3", Some("Visible Artist"), Some("Visible Album")),
        track("/m/v3.mp3", Some("Visible Artist"), Some("Visible Album")),
        track("/m/v4.mp3", Some("Visible Artist"), Some("Visible Album")),
        track("/m/h.mp3", Some("Hidden Artist"), Some("Hidden Album")),
    ])
    .unwrap();

    // Sanity: visible items are found.
    let r = lib.search_library("Visible", 50).unwrap();
    assert!(r.artists.iter().any(|a| a == "Visible Artist"));
    assert!(r.albums.iter().any(|a| a.album == "Visible Album"));

    // Hide the second item at every level that could surface it (empty list).
    lib.set_category("artist", "Hidden Artist", Some(""))
        .unwrap();
    lib.set_category(
        "album",
        &album_key("Hidden Artist", "Hidden Album"),
        Some(""),
    )
    .unwrap();
    lib.set_category("track", "/m/h.mp3", Some("")).unwrap();

    // A hidden artist/album must not appear in search results.
    let r = lib.search_library("Hidden", 50).unwrap();
    assert!(r.artists.is_empty(), "hidden artist leaked into search");
    assert!(r.albums.is_empty(), "hidden album leaked into search");

    // Nor the hidden track (title "T") via a title search — the visible one stays.
    let r = lib.search_library("T", 50).unwrap();
    assert!(r.songs.iter().all(|s| s.path != "/m/h.mp3"));
    assert!(r.songs.iter().any(|s| s.path == "/m/v.mp3"));
}

#[test]
fn search_files_album_hits_by_category() {
    use crate::core::category::{album_key, Area};
    let lib = Library::open_in_memory().unwrap();
    lib.upsert_tracks(&[
        // A 4-track album → stays a plain Album.
        track("/m/rec/1.mp3", Some("Band"), Some("Record")),
        track("/m/rec/2.mp3", Some("Band"), Some("Record")),
        track("/m/rec/3.mp3", Some("Band"), Some("Record")),
        track("/m/rec/4.mp3", Some("Band"), Some("Record")),
        // A 1-track album → classified Single by the heuristic.
        track("/m/sng/1.mp3", Some("Band"), Some("Record Single")),
        // An album re-filed as a Concert via an explicit override.
        track("/m/liv/1.mp3", Some("Band"), Some("Record Live")),
        track("/m/liv/2.mp3", Some("Band"), Some("Record Live")),
        track("/m/liv/3.mp3", Some("Band"), Some("Record Live")),
        track("/m/liv/4.mp3", Some("Band"), Some("Record Live")),
    ])
    .unwrap();
    lib.set_category("album", &album_key("Band", "Record Live"), Some("concerts"))
        .unwrap();

    let r = lib.search_library("Record", 50).unwrap();
    // Each album-like hit appears in exactly one bucket, by its category.
    assert!(r.albums.iter().any(|a| a.album == "Record"));
    assert!(r.albums.iter().all(|a| a.album != "Record Single"));
    assert!(r.singles.iter().any(|a| a.album == "Record Single"));
    assert!(r.concerts.iter().any(|a| a.album == "Record Live"));
    // The concert must not also leak into the generic Albums list.
    assert!(r.albums.iter().all(|a| a.album != "Record Live"));
    // Sanity on the override semantics used above.
    assert!(lib
        .album_areas("Band", "Record Live")
        .contains(&Area::Concerts));
}

#[test]
fn singles_area_reflects_kind_and_override() {
    use crate::core::category::{album_key, Area};
    let lib = Library::open_in_memory().unwrap();
    // A 1-track album → classified Single by the heuristic.
    lib.upsert_tracks(&[track("/s/1.mp3", Some("Solo"), Some("My Single"))])
        .unwrap();

    // Auto-classified single: in the Singles area (and Albums) by default.
    let singles = lib.albums_overview_in_area(Area::Singles, None).unwrap();
    assert!(singles.iter().any(|m| m.album == "My Single"));
    let albums = lib.albums_overview_in_area(Area::Albums, None).unwrap();
    assert!(albums.iter().any(|m| m.album == "My Single"));

    // An explicit "Available in" set that omits Singles removes it from that
    // view, while it stays under Albums.
    lib.set_category(
        "album",
        &album_key("Solo", "My Single"),
        Some("filesystem,artists,albums"),
    )
    .unwrap();
    let singles = lib.albums_overview_in_area(Area::Singles, None).unwrap();
    assert!(singles.iter().all(|m| m.album != "My Single"));
    let albums = lib.albums_overview_in_area(Area::Albums, None).unwrap();
    assert!(albums.iter().any(|m| m.album == "My Single"));

    // Conversely, a regular album can be filed into Singles explicitly.
    lib.upsert_tracks(&[
        track("/a/1.mp3", Some("Band"), Some("Big Album")),
        track("/a/2.mp3", Some("Band"), Some("Big Album")),
        track("/a/3.mp3", Some("Band"), Some("Big Album")),
        track("/a/4.mp3", Some("Band"), Some("Big Album")),
    ])
    .unwrap();
    let singles = lib.albums_overview_in_area(Area::Singles, None).unwrap();
    assert!(singles.iter().all(|m| m.album != "Big Album"));
    lib.set_category(
        "album",
        &album_key("Band", "Big Album"),
        Some("filesystem,artists,albums,singles"),
    )
    .unwrap();
    let singles = lib.albums_overview_in_area(Area::Singles, None).unwrap();
    assert!(singles.iter().any(|m| m.album == "Big Album"));
}

#[test]
fn album_tracklist_roundtrip_and_fetch_marker() {
    use crate::core::online::CanonicalTrack;
    let lib = Library::open_in_memory().unwrap();
    assert!(!lib.tracklist_fetched("A", "Alb"));
    lib.set_album_tracklist(
        "A",
        "Alb",
        &[
            CanonicalTrack {
                disc: 1,
                position: 1,
                title: "One".into(),
                length_ms: Some(1000),
            },
            CanonicalTrack {
                disc: 1,
                position: 2,
                title: "Two".into(),
                length_ms: None,
            },
        ],
    )
    .unwrap();
    assert!(lib.tracklist_fetched("A", "Alb"));
    let tl = lib.album_tracklist("A", "Alb").unwrap();
    assert_eq!(tl.len(), 2);
    assert_eq!(tl[0], (1, 1, "One".to_string(), Some(1000)));
    assert_eq!(tl[1].2, "Two");

    // An empty result still records the attempt, so a no-match album isn't
    // re-queried on every open.
    lib.set_album_tracklist("B", "Bl", &[]).unwrap();
    assert!(lib.tracklist_fetched("B", "Bl"));
    assert!(lib.album_tracklist("B", "Bl").unwrap().is_empty());
}

#[test]
fn upsert_tracks_batch_inserts_all() {
    let lib = Library::open_in_memory().unwrap();
    let batch = vec![
        track("/m/1.mp3", Some("A"), Some("X")),
        track("/m/2.mp3", Some("A"), Some("X")),
        track("/m/3.mp3", Some("B"), Some("Y")),
    ];
    assert_eq!(lib.upsert_tracks(&batch).unwrap(), 3);
    assert!(lib.track_by_path("/m/2.mp3").unwrap().is_some());
    // Re-running upserts (no duplicates, ON CONFLICT path).
    assert_eq!(lib.upsert_tracks(&batch).unwrap(), 3);
    assert_eq!(lib.all_tracks().unwrap().len(), 3);
}

#[test]
fn lyrics_cache_roundtrip_and_negative() {
    let lib = Library::open_in_memory().unwrap();
    // Nothing cached yet.
    assert!(lib.get_cached_lyrics("/m/song.mp3").is_none());
    assert!(!lib.lyrics_recently_missing("/m/song.mp3"));

    // Store synced lyrics → parsed back with timed lines.
    lib.store_lyrics(
        "/m/song.mp3",
        Some("line one\nline two"),
        Some("[00:01.00]line one\n[00:03.50]line two"),
        "lrclib",
    );
    let cached = lib.get_cached_lyrics("/m/song.mp3").expect("hit");
    assert!(cached.has_synced());
    assert_eq!(cached.synced.len(), 2);
    assert_eq!(cached.active_line(2000), Some(0));
    // A positive hit is not a "recent miss".
    assert!(!lib.lyrics_recently_missing("/m/song.mp3"));

    // Negative result is remembered and reported, but never as a hit.
    lib.store_lyrics("/m/inst.mp3", None, None, "none");
    assert!(lib.get_cached_lyrics("/m/inst.mp3").is_none());
    assert!(lib.lyrics_recently_missing("/m/inst.mp3"));
}
