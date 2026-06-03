//! Background worker for online enrichment (covers, artist/album images,
//! fingerprint track recognition). Runs in a relm4 command thread and reports
//! progress via [`Cmd`] back to the root component.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::core::db::Library;
use crate::core::{online, scanner};
use crate::ui::app::Cmd;

/// Background worker for online enrichment. Opens its own DB connection, reads
/// the tags under `root` (read-only on the files!) and enriches **prioritized
/// from top to bottom** – artists first, then covers:
///   1. **Artist photos** → Deezer (highest priority)
///   2. Covers from local files (embedded/folder image) – offline, immediate
///   3. **Album covers** → MusicBrainz + Cover Art Archive
/// Galleries (fanart.tv / Cover Art Archive) and the fingerprint track recognition
/// (AcoustID) do **not** run here, but on demand when opening the detail view or
/// when playing – see [`crate::ui::app::App::fetch_focus_artist`],
/// [`fetch_focus_album`](crate::ui::app::App::fetch_focus_album) and
/// [`fetch_focus_track`](crate::ui::app::App::fetch_focus_track).
/// Between network requests there is a pause (rate limit); a temporary server
/// limit (HTTP 429/503) is caught in [`online`] (backoff + retry) and does **not**
/// count as a failed attempt. Only after this many *real* unsuccessful attempts
/// is an entry no longer queried again – neither during the automatic sync nor
/// on manual fetch –, so that persistently unsuccessful items are not retried
/// endlessly.
pub(crate) const MAX_ATTEMPTS: i64 = 3;

///
/// `light`: **lightweight background catch-up** (periodic timer). Loads only the
/// well "skippable" phases – **artist photos (1)** and **online covers (3)** –
/// and skips the local cover scanning (phase 2, already done during the scan)
/// as well as the galleries/fingerprint (phases 4–6, which would reload on every
/// run). Runs quietly: `ReloadViews` is only sent if something actually
/// changed – otherwise the UI would rebuild needlessly every minute.
pub(crate) fn enrich_worker(
    root: PathBuf,
    cancel: Arc<AtomicBool>,
    scan_first: bool,
    light: bool,
    out: &relm4::Sender<Cmd>,
) {
    let lib = match Library::open() {
        Ok(lib) => lib,
        Err(e) => {
            tracing::error!("Database unavailable for online fetch: {e}");
            let _ = out.send(Cmd::EnrichDone { changed: false });
            return;
        }
    };

    // Read the tags into the library (does not modify the files). During the
    // automatic run this is skipped – the local scan already ran.
    if scan_first {
        if let Err(e) = scanner::scan_into(&lib, &root) {
            tracing::warn!("Scan before online fetch failed: {e}");
        }
    }

    let client = online::OnlineClient::new();
    // Did this run save anything new? During the lightweight run this controls
    // whether the views are reloaded at the end at all.
    let mut any_change = false;
    let stopped = || cancel.load(Ordering::Relaxed);

    'work: {
        // Phase 1: artist photos (Deezer) – **highest priority** (preference:
        // artists first, then covers), in parallel, small images. Skip already
        // matched and permanently unsuccessful ones, load only the rest.
        let mut to_fetch = Vec::new();
        for name in lib.distinct_artists().unwrap_or_default() {
            let matched = matches!(
                lib.get_artist_meta(&name).ok().flatten(),
                Some(m) if m.status == "matched"
            );
            if !matched && lib.artist_attempts(&name) < MAX_ATTEMPTS {
                to_fetch.push(name);
            }
        }
        if stopped() {
            break 'work;
        }
        let new_artists = fetch_artists_parallel(&client, to_fetch, &cancel, &lib, out);
        if new_artists > 0 {
            any_change = true;
        }
        // During the lightweight run only reload if new photos arrived (otherwise
        // a needless UI rebuild every minute).
        if !light || new_artists > 0 {
            let _ = out.send(Cmd::ReloadViews);
        }
        if stopped() {
            break 'work;
        }

        // Phase 2: covers from the local files (embedded/folder image) – offline
        // and fast, hence right after the artists and before the online covers.
        // Skipped during the lightweight catch-up: this already ran during the scan;
        // re-scanning all cover-less albums every minute would be pure disk load.
        if !light {
            // Credentials of the remote sources, so embedded covers of indexed
            // Nextcloud albums/tracks (sample path `nc:<id>:<rel>`) can be pulled
            // over WebDAV – their `Path` is synthetic and has no local file.
            let sources = lib.list_sources().unwrap_or_default();
            let remote = |path: &str| -> Option<(crate::core::webdav::Creds, String)> {
                let (id, rel) = crate::core::webdav::parse_nc_path(path)?;
                let creds = sources
                    .iter()
                    .find(|s| s.id == id)
                    .and_then(crate::core::webdav::Creds::from_source)?;
                Some((creds, rel))
            };

            let missing = lib.albums_missing_cover().unwrap_or_default();
            for (artist, album, path) in missing.iter() {
                if stopped() {
                    break 'work;
                }
                let cover_path = match remote(path) {
                    Some((creds, rel)) => crate::core::webdav::fetch_cover(&creds, &rel)
                        .and_then(|b| online::store_album_cover_bytes(artist, album, &b)),
                    None => online::local_album_cover(artist, album, path),
                };
                if let Some(cover_path) = cover_path {
                    let mut m = crate::model::AlbumMeta::pending(artist, album);
                    m.cover_path = Some(cover_path);
                    m.status = "local".to_string();
                    let _ = lib.upsert_album_meta(&m);
                    any_change = true;
                }
            }
            let _ = out.send(Cmd::ReloadViews);

            // Covers for favorited single tracks that have none yet.
            for (scope, key, _title, _is_dir) in lib.favorites().unwrap_or_default() {
                if stopped() {
                    break 'work;
                }
                if scope != "track" || online::track_cover_cached(&key) {
                    continue;
                }
                // 1) Embedded cover. Remote (NC) tracks need a WebDAV fetch;
                //    local files are read directly (same as the display path).
                let embedded = match remote(&key) {
                    Some((creds, rel)) => crate::core::webdav::fetch_cover(&creds, &rel),
                    None => crate::core::cover::embedded_cover(std::path::Path::new(&key)),
                };
                if let Some(bytes) = embedded {
                    if online::store_track_cover_bytes(&key, &bytes).is_some() {
                        any_change = true;
                    }
                    continue;
                }
                // 2) No embedded art → an album-less single track has no album
                //    cover to fall back to, so fetch one online by artist+title
                //    (like other players do for e.g. converted downloads).
                if let Ok(Some(t)) = lib.track_by_path(&key) {
                    let album_less = t.album.as_deref().map_or(true, |a| a.trim().is_empty());
                    if album_less {
                        if let Some((bytes, _)) =
                            online::recording_cover(t.artist.as_deref().unwrap_or(""), &t.title)
                        {
                            if online::store_track_cover_bytes(&key, &bytes).is_some() {
                                any_change = true;
                            }
                        }
                        std::thread::sleep(online::RATE_LIMIT);
                    }
                }
            }
        }

        // Phase 3: online covers only for albums with no image at all (gap filler).
        let still_missing = lib.albums_missing_cover().unwrap_or_default();
        let mut new_covers = 0usize;
        for (artist, album, _) in still_missing.iter() {
            if stopped() {
                break 'work;
            }
            // Skip after too many unsuccessful attempts (also manually).
            let exhausted = lib.album_attempts(artist, album) >= MAX_ATTEMPTS;
            if !exhausted {
                if !artist.is_empty()
                    && online::enrich_album(&client, &lib, artist, album).cover_path.is_some()
                {
                    new_covers += 1;
                    any_change = true;
                }
                std::thread::sleep(online::RATE_LIMIT);
            }
        }
        if !light || new_covers > 0 {
            let _ = out.send(Cmd::ReloadViews);
        }

        // Galleries (multiple photos/covers per artist or album) and the
        // fingerprint track recognition no longer run in the sweep, but on demand:
        // galleries when opening the detail view, the fingerprint when playing a
        // track without metadata. Otherwise both would be queried again here on
        // every run (galleries have no "already present" check).
    }

    // Full run: always reload (as before – this shows, among other things, the
    // results of the fingerprint phase). Lightweight run: only if something was
    // actually added.
    let _ = out.send(Cmd::EnrichDone { changed: !light || any_change });
}

/// Loads artist photos **in parallel** (multiple network threads), but writes the
/// results serialized through the coordinator's single DB connection.
/// Returns the number of newly matched artists.
fn fetch_artists_parallel(
    client: &online::OnlineClient,
    names: Vec<String>,
    cancel: &Arc<AtomicBool>,
    lib: &Library,
    out: &relm4::Sender<Cmd>,
) -> usize {
    use std::collections::VecDeque;
    use std::sync::mpsc;
    use std::sync::Mutex;

    let total = names.len();
    if total == 0 {
        return 0;
    }

    let jobs = Arc::new(Mutex::new(VecDeque::from(names)));
    let (tx, rx) = mpsc::channel::<(String, Option<Vec<u8>>, bool)>();
    let n_threads = total.min(online::ARTIST_FETCH_THREADS);

    let mut handles = Vec::with_capacity(n_threads);
    for _ in 0..n_threads {
        let client = client.clone();
        let jobs = jobs.clone();
        let cancel = cancel.clone();
        let tx = tx.clone();
        handles.push(std::thread::spawn(move || loop {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let Some(name) = jobs.lock().unwrap().pop_front() else {
                break;
            };
            let (image, errored) = match client.fetch_artist_image(&name) {
                Ok(img) => (img, false),
                Err(_) => (None, true),
            };
            if tx.send((name, image, errored)).is_err() {
                break;
            }
        }));
    }
    drop(tx); // only the thread clones hold the sender → rx ends when all are done

    // Coordinator: write results serialized into the DB. In between, refresh the
    // views so that new photos appear already during the run.
    let mut matched = 0usize;
    let mut done = 0usize;
    while let Ok((name, image, errored)) = rx.recv() {
        let meta = online::store_artist_image(&name, image, errored);
        if meta.status == "matched" {
            matched += 1;
        }
        let _ = lib.upsert_artist_meta(&meta);
        done += 1;
        if done % 16 == 0 {
            let _ = out.send(Cmd::ReloadViews);
        }
    }

    for h in handles {
        let _ = h.join();
    }
    matched
}
