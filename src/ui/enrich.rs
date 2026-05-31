//! Hintergrund-Worker für die Online-Anreicherung (Cover, Interpreten-/Album-
//! Bilder, Fingerprint-Titelerkennung). Läuft in einem relm4-Command-Thread und
//! meldet den Fortschritt über [`Cmd`] zurück an die Wurzel-Komponente.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::core::db::Library;
use crate::core::{online, scanner};
use crate::i18n::gettext;
use crate::ui::app::Cmd;

/// Hintergrund-Worker für die Online-Anreicherung. Öffnet eine eigene
/// DB-Verbindung, liest die Tags unter `root` ein (read-only auf den Dateien!)
/// und reichert in drei Phasen an:
///   1. Alben  → MusicBrainz + Cover Art Archive (Cover)
///   2. Interpreten → Deezer (Fotos)
///   3. Titel  → Chromaprint/AcoustID (nur Dateien mit lückenhaften Tags;
///      benötigt einen AcoustID-Key, sonst wird die Phase übersprungen)
/// Nach so vielen erfolglosen Versuchen wird ein Eintrag **nicht** erneut online
/// abgefragt – weder beim automatischen Sync noch beim manuellen Abruf. So
/// werden dauerhaft erfolglose Cover/Fotos nicht endlos wiederholt.
const MAX_ATTEMPTS: i64 = 3;

pub(crate) fn enrich_worker(
    root: PathBuf,
    acoustid_key: Option<String>,
    fanart_key: Option<String>,
    cancel: Arc<AtomicBool>,
    scan_first: bool,
    out: &relm4::Sender<Cmd>,
) {
    let lib = match Library::open() {
        Ok(lib) => lib,
        Err(e) => {
            tracing::error!("Database unavailable for online fetch: {e}");
            let _ = out.send(Cmd::EnrichDone);
            return;
        }
    };

    // Tags in die Bibliothek einlesen (verändert die Dateien nicht). Beim
    // automatischen Lauf entfällt das – der lokale Scan lief bereits.
    if scan_first {
        if let Err(e) = scanner::scan_into(&lib, &root) {
            tracing::warn!("Scan before online fetch failed: {e}");
        }
    }

    let client = online::OnlineClient::new();
    let mut artists_matched = 0usize;
    let stopped = || cancel.load(Ordering::Relaxed);

    // Gesamtsummen für die Fortschrittsanzeige (gegen die ganze Bibliothek,
    // nicht nur gegen die Restmenge der jeweiligen Phase).
    let total_albums = lib.album_count().unwrap_or(0).max(0) as usize;

    'work: {
        // Phase 1: Cover aus den Dateien (eingebettet/Ordnerbild) – schnell, offline.
        let missing = lib.albums_missing_cover().unwrap_or_default();
        // Bereits mit Cover versehene Alben gelten als „erledigt" → der Zähler
        // startet dort und läuft bis zur Gesamtzahl (z. B. 4717/4726 … 4726/4726).
        let base = total_albums.saturating_sub(missing.len());
        for (i, (artist, album, path)) in missing.iter().enumerate() {
            if stopped() {
                break 'work;
            }
            if let Some(cover_path) = online::local_album_cover(artist, album, path) {
                let mut m = crate::model::AlbumMeta::pending(artist, album);
                m.cover_path = Some(cover_path);
                m.status = "local".to_string();
                let _ = lib.upsert_album_meta(&m);
            }
            let _ = out.send(Cmd::EnrichProgress {
                phase: gettext("Cover"),
                done: base + i + 1,
                total: total_albums,
            });
        }
        let _ = out.send(Cmd::ReloadViews);

        // Phase 2: Interpreten-Fotos (Deezer) – parallel, kleine Bilder --------
        // Bereits zugeordnete überspringen, nur den Rest parallel laden.
        let all_artists = lib.distinct_artists().unwrap_or_default();
        let total_artists = all_artists.len();
        let mut to_fetch = Vec::new();
        let mut artists_skipped = 0usize;
        for name in all_artists {
            match lib.get_artist_meta(&name).ok().flatten() {
                Some(m) if m.status == "matched" => artists_matched += 1,
                // Nach zu vielen Fehlversuchen nicht erneut anfragen (auch manuell).
                _ if lib.artist_attempts(&name) >= MAX_ATTEMPTS => artists_skipped += 1,
                _ => to_fetch.push(name),
            }
        }
        if stopped() {
            break 'work;
        }
        // Zugeordnete + übersprungene Interpreten zählen als „erledigt".
        let artists_base = artists_matched + artists_skipped;
        fetch_artists_parallel(&client, to_fetch, &cancel, &lib, artists_base, total_artists, out);
        let _ = out.send(Cmd::ReloadViews);
        if stopped() {
            break 'work;
        }

        // Phase 3: Online-Cover nur noch für Alben ganz ohne Bild (Lückenfüller).
        let still_missing = lib.albums_missing_cover().unwrap_or_default();
        let base = total_albums.saturating_sub(still_missing.len());
        for (i, (artist, album, _)) in still_missing.iter().enumerate() {
            if stopped() {
                break 'work;
            }
            // Nach zu vielen erfolglosen Versuchen überspringen (auch manuell).
            let exhausted = lib.album_attempts(artist, album) >= MAX_ATTEMPTS;
            if !exhausted {
                if !artist.is_empty() {
                    let _ = online::enrich_album(&client, &lib, artist, album);
                }
                std::thread::sleep(online::RATE_LIMIT);
            }
            let _ = out.send(Cmd::EnrichProgress {
                phase: gettext("Cover"),
                done: base + i + 1,
                total: total_albums,
            });
        }
        let _ = out.send(Cmd::ReloadViews);

        // Phase 4: Titel-Erkennung per Fingerprint -----------------------------
        if let Some(key) = acoustid_key.filter(|k| !k.is_empty()) {
            if online::fingerprint_available() {
                let candidates = lib.tracks_needing_id().unwrap_or_default();
                // Gesamtsumme = alle Titel; bereits vollständige zählen als erledigt.
                let total_tracks = lib.track_count().unwrap_or(0).max(0) as usize;
                let base = total_tracks.saturating_sub(candidates.len());
                for (i, track) in candidates.iter().enumerate() {
                    if stopped() {
                        break 'work;
                    }
                    let path = PathBuf::from(&track.path);
                    let already = lib.get_track_meta(&track.path).ok().flatten();
                    let matched =
                        already.as_ref().map(|m| m.status.as_str()) == Some("matched");
                    // Erschöpfte Titel nicht erneut anfragen (auch manuell).
                    let exhausted = lib.track_attempts(&track.path) >= MAX_ATTEMPTS;
                    if !matched && !exhausted {
                        let _ = online::enrich_track_fingerprint(&client, &lib, &key, &path);
                        std::thread::sleep(online::ACOUSTID_DELAY);
                    }
                    let _ = out.send(Cmd::EnrichProgress {
                        phase: gettext("Tracks"),
                        done: base + i + 1,
                        total: total_tracks,
                    });
                }
            } else {
                tracing::info!("Fingerprint phase skipped (fpcalc missing)");
            }
        }

        // Phase 5: Album-Galerien (mehrere Cover je Album, Cover Art Archive) –
        // parallel; der reine Bildabruf unterliegt keinem MusicBrainz-1/s-Limit.
        let gallery_albums: Vec<(String, String, String)> = lib
            .albums_overview()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|m| m.mbid.map(|id| (m.artist, m.album, id)))
            .collect();
        if stopped() {
            break 'work;
        }
        fetch_album_galleries_parallel(&client, gallery_albums, &cancel, &lib, out);
        let _ = out.send(Cmd::ReloadViews);

        // Phase 6: Interpreten-Galerien (mehrere Fotos via fanart.tv – nur mit Key).
        if let Some(fkey) = fanart_key.filter(|k| !k.is_empty()) {
            let names = lib.distinct_artists().unwrap_or_default();
            let fa_total = names.len();
            for (i, name) in names.iter().enumerate() {
                if stopped() {
                    break 'work;
                }
                online::enrich_artist_gallery(&client, &lib, name, &fkey);
                std::thread::sleep(online::RATE_LIMIT);
                let _ = out.send(Cmd::EnrichProgress {
                    phase: gettext("Artist photos"),
                    done: i + 1,
                    total: fa_total,
                });
            }
            let _ = out.send(Cmd::ReloadViews);
        }
    }

    let _ = out.send(Cmd::EnrichDone);
}

/// Lädt Künstlerfotos **parallel** (mehrere Netz-Threads), schreibt die
/// Ergebnisse aber serialisiert über die eine DB-Verbindung des Koordinators.
/// Gibt die Anzahl neu zugeordneter Interpreten zurück.
fn fetch_artists_parallel(
    client: &online::OnlineClient,
    names: Vec<String>,
    cancel: &Arc<AtomicBool>,
    lib: &Library,
    // Fortschritt gegen die Gesamtzahl: `done_base` = schon erledigte Interpreten,
    // `grand_total` = alle Interpreten der Bibliothek.
    done_base: usize,
    grand_total: usize,
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
    drop(tx); // nur die Thread-Klone halten den Sender → rx endet, wenn alle fertig

    // Koordinator: Ergebnisse serialisiert in die DB schreiben + Fortschritt.
    let mut matched = 0usize;
    let mut done = 0usize;
    while let Ok((name, image, errored)) = rx.recv() {
        let meta = online::store_artist_image(&name, image, errored);
        if meta.status == "matched" {
            matched += 1;
        }
        let _ = lib.upsert_artist_meta(&meta);
        done += 1;
        let _ = out.send(Cmd::EnrichProgress {
            phase: gettext("Artists"),
            done: done_base + done,
            total: grand_total,
        });
        if done % 16 == 0 {
            let _ = out.send(Cmd::ReloadViews);
        }
    }

    for h in handles {
        let _ = h.join();
    }
    matched
}

/// Lädt **mehrere** Album-Galerien parallel aus dem Cover Art Archive. Anders als
/// die MusicBrainz-Suche unterliegt der reine Bildabruf (die MBID ist bekannt)
/// keinem 1/s-Limit; nur die DB-Schreibzugriffe werden serialisiert (Koordinator).
fn fetch_album_galleries_parallel(
    client: &online::OnlineClient,
    albums: Vec<(String, String, String)>,
    cancel: &Arc<AtomicBool>,
    lib: &Library,
    out: &relm4::Sender<Cmd>,
) {
    use std::collections::VecDeque;
    use std::sync::mpsc;
    use std::sync::Mutex;

    let total = albums.len();
    if total == 0 {
        return;
    }

    let jobs = Arc::new(Mutex::new(VecDeque::from(albums)));
    let (tx, rx) = mpsc::channel::<(String, String, Vec<(Vec<u8>, String)>)>();
    let n_threads = total.min(online::GALLERY_FETCH_THREADS);

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
            let Some((artist, album, mbid)) = jobs.lock().unwrap().pop_front() else {
                break;
            };
            let imgs = client.fetch_album_gallery(&mbid).unwrap_or_default();
            if tx.send((artist, album, imgs)).is_err() {
                break;
            }
        }));
    }
    drop(tx); // nur die Thread-Klone halten den Sender → rx endet, wenn alle fertig

    // Koordinator: Ergebnisse serialisiert in den Cache + die DB schreiben.
    let mut done = 0usize;
    while let Ok((artist, album, imgs)) = rx.recv() {
        online::store_album_gallery(lib, &artist, &album, &imgs);
        done += 1;
        let _ = out.send(Cmd::EnrichProgress {
            phase: gettext("Album art"),
            done,
            total,
        });
        if done % 16 == 0 {
            let _ = out.send(Cmd::ReloadViews);
        }
    }

    for h in handles {
        let _ = h.join();
    }
}
