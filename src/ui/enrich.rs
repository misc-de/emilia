//! Hintergrund-Worker für die Online-Anreicherung (Cover, Interpreten-/Album-
//! Bilder, Fingerprint-Titelerkennung). Läuft in einem relm4-Command-Thread und
//! meldet den Fortschritt über [`Cmd`] zurück an die Wurzel-Komponente.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::core::db::Library;
use crate::core::{online, scanner};
use crate::ui::app::Cmd;

/// Hintergrund-Worker für die Online-Anreicherung. Öffnet eine eigene
/// DB-Verbindung, liest die Tags unter `root` ein (read-only auf den Dateien!)
/// und reichert **priorisiert von oben nach unten** an – zuerst Interpreten,
/// dann Cover:
///   1. **Interpreten-Fotos** → Deezer (höchste Priorität)
///   2. Cover aus lokalen Dateien (eingebettet/Ordnerbild) – offline, sofort
///   3. **Album-Cover** → MusicBrainz + Cover Art Archive
/// Galerien (fanart.tv / Cover Art Archive) und die Fingerprint-Titelerkennung
/// (AcoustID) laufen **nicht** hier, sondern bedarfsgesteuert beim Öffnen der
/// Detailansicht bzw. beim Abspielen – siehe [`crate::ui::app::App::fetch_focus_artist`],
/// [`fetch_focus_album`](crate::ui::app::App::fetch_focus_album) und
/// [`fetch_focus_track`](crate::ui::app::App::fetch_focus_track).
/// Zwischen den Netz-Anfragen wird pausiert (Rate-Limit); ein vorübergehendes
/// Server-Limit (HTTP 429/503) wird in [`online`] abgefangen (Backoff + Retry)
/// und zählt **nicht** als Fehlversuch. Erst nach so vielen *echten* erfolglosen
/// Versuchen wird ein Eintrag nicht erneut abgefragt – weder beim automatischen
/// Sync noch beim manuellen Abruf –, damit dauerhaft Erfolgloses nicht endlos
/// wiederholt wird.
pub(crate) const MAX_ATTEMPTS: i64 = 3;

///
/// `light`: **leichter Hintergrund-Nachzug** (periodischer Timer). Lädt nur die
/// gut „skip-fähigen" Phasen – **Interpreten-Fotos (1)** und **Online-Cover (3)** –
/// und überspringt das lokale Cover-Scannen (Phase 2, bereits beim Scan erledigt)
/// sowie die Galerien/Fingerprint (Phasen 4–6, die bei jedem Lauf erneut laden
/// würden). Läuft leise: `ReloadViews` wird nur gesendet, wenn sich tatsächlich
/// etwas geändert hat – sonst würde die UI im Minutentakt grundlos neu aufbauen.
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

    // Tags in die Bibliothek einlesen (verändert die Dateien nicht). Beim
    // automatischen Lauf entfällt das – der lokale Scan lief bereits.
    if scan_first {
        if let Err(e) = scanner::scan_into(&lib, &root) {
            tracing::warn!("Scan before online fetch failed: {e}");
        }
    }

    let client = online::OnlineClient::new();
    // Hat dieser Lauf irgendetwas Neues gespeichert? Steuert beim leichten Lauf,
    // ob die Ansichten am Ende überhaupt neu geladen werden.
    let mut any_change = false;
    let stopped = || cancel.load(Ordering::Relaxed);

    'work: {
        // Phase 1: Interpreten-Fotos (Deezer) – **höchste Priorität** (Wunsch:
        // erst Interpreten, dann Cover), parallel, kleine Bilder. Bereits
        // zugeordnete und dauerhaft erfolglose überspringen, nur den Rest laden.
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
        // Beim leichten Lauf nur neu laden, wenn neue Fotos kamen (sonst grundloser
        // UI-Neuaufbau im Minutentakt).
        if !light || new_artists > 0 {
            let _ = out.send(Cmd::ReloadViews);
        }
        if stopped() {
            break 'work;
        }

        // Phase 2: Cover aus den lokalen Dateien (eingebettet/Ordnerbild) – offline
        // und schnell, daher direkt nach den Interpreten und vor dem Online-Cover.
        // Beim leichten Nachzug ausgelassen: das lief bereits beim Scan; ein erneutes
        // Datei-Scannen aller cover-losen Alben im Minutentakt wäre reine Plattenlast.
        if !light {
            let missing = lib.albums_missing_cover().unwrap_or_default();
            for (artist, album, path) in missing.iter() {
                if stopped() {
                    break 'work;
                }
                if let Some(cover_path) = online::local_album_cover(artist, album, path) {
                    let mut m = crate::model::AlbumMeta::pending(artist, album);
                    m.cover_path = Some(cover_path);
                    m.status = "local".to_string();
                    let _ = lib.upsert_album_meta(&m);
                    any_change = true;
                }
            }
            let _ = out.send(Cmd::ReloadViews);
        }

        // Phase 3: Online-Cover nur noch für Alben ganz ohne Bild (Lückenfüller).
        let still_missing = lib.albums_missing_cover().unwrap_or_default();
        let mut new_covers = 0usize;
        for (artist, album, _) in still_missing.iter() {
            if stopped() {
                break 'work;
            }
            // Nach zu vielen erfolglosen Versuchen überspringen (auch manuell).
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

        // Galerien (mehrere Fotos/Cover je Interpret bzw. Album) und die
        // Fingerprint-Titelerkennung laufen **nicht** mehr im Sweep, sondern
        // bedarfsgesteuert: Galerien beim Öffnen der Detailansicht, der Fingerprint
        // beim Abspielen eines Titels ohne Metadaten. Beides würde hier sonst bei
        // jedem Lauf erneut abgefragt (Galerien haben keine „schon vorhanden"-Prüfung).
    }

    // Voller Lauf: immer neu laden (wie bisher – u. a. zeigt das die Ergebnisse der
    // Fingerprint-Phase). Leichter Lauf: nur, wenn wirklich etwas dazukam.
    let _ = out.send(Cmd::EnrichDone { changed: !light || any_change });
}

/// Lädt Künstlerfotos **parallel** (mehrere Netz-Threads), schreibt die
/// Ergebnisse aber serialisiert über die eine DB-Verbindung des Koordinators.
/// Gibt die Anzahl neu zugeordneter Interpreten zurück.
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
    drop(tx); // nur die Thread-Klone halten den Sender → rx endet, wenn alle fertig

    // Koordinator: Ergebnisse serialisiert in die DB schreiben. Zwischendurch die
    // Ansichten auffrischen, damit neue Fotos schon während des Laufs erscheinen.
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
