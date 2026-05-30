//! Online-Metadaten aus offenen/kostenlosen Quellen:
//! - **MusicBrainz** – Album-Zuordnung (CC0)
//! - **Cover Art Archive** – Album-Cover (CC0)
//! - **Deezer** – Künstlerfotos (kein API-Key nötig)
//! - **AcoustID** + **Chromaprint** – Titel-Erkennung per Audio-Fingerprint
//!   (benötigt einen kostenlosen Application-Key)
//!
//! Wichtig: Dieses Modul liest **niemals** Audiodateien und schreibt erst recht
//! nichts in deren Tags zurück. Sämtliche gefundenen Daten landen ausschließlich
//! in der Datenbank und im XDG-Cache (`~/.cache/emilia`).

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use serde::Deserialize;

use crate::core::cover;
use crate::core::db::Library;
use crate::core::fingerprint;
use crate::model::{AlbumMeta, ArtistMeta, TrackMeta};

/// MusicBrainz verlangt einen aussagekräftigen User-Agent mit Kontakt.
const USER_AGENT: &str = "Emilia/0.1.0 ( https://cais.de )";

/// MusicBrainz-Richtlinie: höchstens eine Anfrage pro Sekunde.
pub const RATE_LIMIT: Duration = Duration::from_millis(1100);
/// AcoustID erlaubt einige Anfragen/Sekunde – konservativ gedrosselt.
pub const ACOUSTID_DELAY: Duration = Duration::from_millis(350);
/// Anzahl paralleler Abrufe für Künstlerfotos (Deezer verträgt das gut).
pub const ARTIST_FETCH_THREADS: usize = 8;
/// Anzahl paralleler Abrufe für Album-Galerien (Cover Art Archive / archive.org).
pub const GALLERY_FETCH_THREADS: usize = 6;

/// Verzeichnis für zwischengespeicherte Cover: `$XDG_CACHE_HOME/emilia/covers`.
pub fn cover_cache_dir() -> PathBuf {
    cache_subdir("covers")
}

/// Verzeichnis für Künstlerfotos: `$XDG_CACHE_HOME/emilia/artists`.
pub fn artist_cache_dir() -> PathBuf {
    cache_subdir("artists")
}

fn cache_subdir(name: &str) -> PathBuf {
    let mut dir = dirs::cache_dir().unwrap_or_else(|| PathBuf::from("."));
    dir.push("emilia");
    dir.push(name);
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Ob die Fingerprint-Erkennung möglich ist (Chromaprint/`fpcalc` vorhanden).
pub fn fingerprint_available() -> bool {
    fingerprint::available()
}

/// Stabiler Dateiname aus einem beliebigen String (für Cache-Dateien).
fn name_hash(s: &str) -> String {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Ergebnis einer MusicBrainz-Release-Suche.
pub struct ReleaseMatch {
    pub mbid: String,
    pub release_group: Option<String>,
    pub year: Option<i32>,
}

/// HTTP-Client mit gemeinsamem Connection-Pool und Timeouts.
/// Klonbar (der `ureq::Agent` teilt sich Pool/Konfiguration) – so kann er an
/// mehrere Fetch-Threads übergeben werden.
#[derive(Clone)]
pub struct OnlineClient {
    agent: ureq::Agent,
}

impl Default for OnlineClient {
    fn default() -> Self {
        Self::new()
    }
}

impl OnlineClient {
    pub fn new() -> Self {
        // Kurze Timeouts: eine zähe/blockierende Anfrage soll nicht den ganzen
        // Lauf aufhalten, sondern schnell scheitern und übersprungen werden.
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(5))
            .timeout_read(Duration::from_secs(8))
            .timeout_write(Duration::from_secs(8))
            .build();
        Self { agent }
    }

    /// Sucht das passendste MusicBrainz-Release zu (Interpret, Album).
    /// Liefert `Ok(None)`, wenn nichts hinreichend Passendes gefunden wurde.
    pub fn match_release(&self, artist: &str, album: &str) -> Result<Option<ReleaseMatch>> {
        let query = format!(
            "artist:\"{}\" AND release:\"{}\"",
            escape_lucene(artist),
            escape_lucene(album)
        );
        let url = format!(
            "https://musicbrainz.org/ws/2/release?query={}&fmt=json&limit=5",
            percent_encode(&query)
        );

        let search: MbSearch = self
            .agent
            .get(&url)
            .set("User-Agent", USER_AGENT)
            .call()?
            .into_json()?;

        // MusicBrainz sortiert nach Score; wir nehmen den besten Treffer,
        // verlangen aber eine Mindestgüte, um Fehlzuordnungen zu vermeiden.
        let best = search
            .releases
            .into_iter()
            .max_by_key(|r| r.score)
            .filter(|r| r.score >= 70);

        Ok(best.map(|r| ReleaseMatch {
            mbid: r.id,
            release_group: r.release_group.map(|g| g.id),
            year: r.date.as_deref().and_then(parse_year),
        }))
    }

    /// Lädt das Front-Cover (max. 500 px) zu einem Release. Versucht zuerst das
    /// konkrete Release, dann ersatzweise die Release-Gruppe.
    /// `Ok(None)` = es existiert kein Cover.
    pub fn fetch_cover(&self, m: &ReleaseMatch) -> Result<Option<Vec<u8>>> {
        let release_url = format!(
            "https://coverartarchive.org/release/{}/front-500",
            m.mbid
        );
        if let Some(bytes) = self.get_image(&release_url)? {
            return Ok(Some(bytes));
        }
        if let Some(rg) = &m.release_group {
            let rg_url = format!("https://coverartarchive.org/release-group/{rg}/front-500");
            if let Some(bytes) = self.get_image(&rg_url)? {
                return Ok(Some(bytes));
            }
        }
        Ok(None)
    }

    fn get_image(&self, url: &str) -> Result<Option<Vec<u8>>> {
        match self.agent.get(url).set("User-Agent", USER_AGENT).call() {
            Ok(resp) => {
                let mut buf = Vec::new();
                // Deckel gegen versehentlich riesige Antworten (10 MB).
                resp.into_reader()
                    .take(10 * 1024 * 1024)
                    .read_to_end(&mut buf)?;
                Ok(Some(buf))
            }
            // Kein Cover hinterlegt (404) – kein Fehler.
            Err(ureq::Error::Status(404, _)) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Sucht ein Künstlerfoto bei Deezer (kein API-Key nötig). Liefert die
    /// rohen Bildbytes – bewusst in **kleiner** Auflösung (für 48-px-Avatare
    /// reicht `picture_medium` ~250 px; spart enorm Bandbreite/Zeit).
    pub fn fetch_artist_image(&self, name: &str) -> Result<Option<Vec<u8>>> {
        let url = format!(
            "https://api.deezer.com/search/artist?q={}&limit=1",
            percent_encode(name)
        );
        let search: DzSearch = self.agent.get(&url).call()?.into_json()?;
        let Some(artist) = search.data.into_iter().next() else {
            return Ok(None);
        };
        // Kleinste brauchbare Größe zuerst (schnell), Platzhalter überspringen.
        let pic = [
            artist.picture_medium,
            artist.picture_big,
            artist.picture,
            artist.picture_xl,
        ]
        .into_iter()
        .flatten()
        .find(|u| !u.is_empty());
        match pic {
            Some(u) => self.get_image(&u),
            None => Ok(None),
        }
    }

    /// Lädt mehrere Bilder eines Albums aus dem Cover Art Archive (Front, Back,
    /// Booklet, …). Liefert je (Bytes, Art). Leere Liste, wenn nichts da ist.
    pub fn fetch_album_gallery(&self, mbid: &str) -> Result<Vec<(Vec<u8>, String)>> {
        let url = format!("https://coverartarchive.org/release/{mbid}");
        let list: CaaList = match self.agent.get(&url).set("User-Agent", USER_AGENT).call() {
            Ok(resp) => resp.into_json()?,
            Err(ureq::Error::Status(404, _)) => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for img in list.images.into_iter().take(MAX_GALLERY) {
            // Bevorzugt die 500-px-Variante; sonst large; sonst das Original.
            let u = img.thumbnails.n500.or(img.thumbnails.large).unwrap_or(img.image);
            if u.is_empty() {
                continue;
            }
            if let Some(bytes) = self.get_image(&u)? {
                out.push((bytes, caa_kind(&img.types, img.front, img.back)));
            }
        }
        Ok(out)
    }

    /// Sucht die MusicBrainz-Artist-ID (für fanart.tv). `None`, wenn unklar.
    pub fn artist_mbid(&self, name: &str) -> Result<Option<String>> {
        let query = format!("artist:\"{}\"", escape_lucene(name));
        let url = format!(
            "https://musicbrainz.org/ws/2/artist?query={}&fmt=json&limit=1",
            percent_encode(&query)
        );
        let search: MbArtistSearch = self
            .agent
            .get(&url)
            .set("User-Agent", USER_AGENT)
            .call()?
            .into_json()?;
        Ok(search.artists.into_iter().find(|a| a.score >= 90).map(|a| a.id))
    }

    /// Lädt mehrere Künstlerbilder von fanart.tv (Thumbs + Hintergründe).
    /// Benötigt einen (kostenlosen) persönlichen API-Key. Leere Liste = nichts.
    pub fn fetch_artist_gallery(
        &self,
        api_key: &str,
        mbid: &str,
    ) -> Result<Vec<(Vec<u8>, String)>> {
        let url = format!("https://webservice.fanart.tv/v3/music/{mbid}?api_key={api_key}");
        let fa: FanartArtist = match self.agent.get(&url).set("User-Agent", USER_AGENT).call() {
            Ok(resp) => resp.into_json()?,
            Err(ureq::Error::Status(404, _)) => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for t in fa.artistthumb.into_iter().chain(fa.artistbackground.into_iter()) {
            if out.len() >= MAX_GALLERY {
                break;
            }
            if t.url.is_empty() {
                continue;
            }
            if let Some(bytes) = self.get_image(&t.url)? {
                out.push((bytes, "Foto".to_string()));
            }
        }
        Ok(out)
    }

    /// Fragt AcoustID mit einem Chromaprint-Fingerprint ab und liefert den
    /// besten Treffer (Recording inkl. Interpret/Album, soweit vorhanden).
    pub fn acoustid_lookup(
        &self,
        client_key: &str,
        fp: &fingerprint::Fingerprint,
    ) -> Result<Option<AcoustIdMatch>> {
        let url = format!(
            "https://api.acoustid.org/v2/lookup?client={}&meta=recordings+releasegroups&duration={}&fingerprint={}",
            percent_encode(client_key),
            fp.duration as u64,
            percent_encode(&fp.fingerprint),
        );
        let resp: AcoustIdResp = self.agent.get(&url).call()?.into_json()?;

        // Bestes Result (höchster Score) mit mindestens einem Recording.
        let best = resp
            .results
            .into_iter()
            .filter(|r| !r.recordings.is_empty())
            .max_by(|a, b| a.score.total_cmp(&b.score));

        let Some(result) = best else {
            return Ok(None);
        };
        let Some(rec) = result.recordings.into_iter().find(|r| r.title.is_some()) else {
            return Ok(None);
        };

        Ok(Some(AcoustIdMatch {
            recording_mbid: rec.id,
            title: rec.title,
            artist: rec.artists.into_iter().next().map(|a| a.name),
            album: rec.releasegroups.into_iter().find_map(|g| g.title),
        }))
    }
}

/// Treffer einer AcoustID-Fingerprint-Suche.
pub struct AcoustIdMatch {
    pub recording_mbid: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
}

/// Speichert die Cover-Bytes im Cache und gibt den Pfad zurück.
fn save_cover(mbid: &str, bytes: &[u8]) -> Result<PathBuf> {
    let mut path = cover_cache_dir();
    path.push(format!("{mbid}.img"));
    std::fs::write(&path, bytes)?;
    Ok(path)
}

/// Ermittelt ein **lokales** Album-Cover ganz ohne Netz: bevorzugt das in den
/// Tags eingebettete Bild des Beispiel-Tracks, sonst ein Ordnerbild
/// (`cover.jpg`, `folder.png`, …). Gibt den Pfad zur anzeigbaren Cover-Datei
/// zurück. Die Audiodatei wird dabei nur gelesen.
pub fn local_album_cover(artist: &str, album: &str, sample_path: &str) -> Option<String> {
    let p = Path::new(sample_path);

    // 1) Eingebettetes Tag-Bild → in den Cache schreiben.
    if let Some(bytes) = cover::embedded_cover(p) {
        if let Ok(path) = save_local_cover(artist, album, &bytes) {
            return Some(path.to_string_lossy().into_owned());
        }
    }
    // 2) Ordnerbild → direkt dessen Pfad verwenden (kein Kopieren nötig).
    if let Some(dir) = p.parent() {
        if let Some(img) = cover::find_cover_file(dir) {
            return Some(img.to_string_lossy().into_owned());
        }
    }
    None
}

/// Cover eines **Einzeltitels** ausschließlich aus dem **eingebetteten** Tag-Bild
/// (einmalig in den Cache geschrieben, Schlüssel = Track-Pfad). Bewusst **kein**
/// Ordnerbild als Rückfall: Ein Einzel-/Gast-Titel in einem fremden Album-Ordner
/// soll nicht dessen `cover.jpg` erben. Fehlt ein eingebettetes Bild, liefert die
/// Funktion `None` – der Aufrufer fällt dann auf das Album- bzw. Interpret-Cover
/// zurück, nie auf ein fremdes Ordnerbild. Die Audiodatei wird nur gelesen.
pub fn local_track_cover(path: &str) -> Option<String> {
    let p = Path::new(path);

    let mut cache = cover_cache_dir();
    cache.push(format!("track_{}.img", name_hash(path)));
    if cache.exists() {
        return Some(cache.to_string_lossy().into_owned());
    }
    let bytes = cover::embedded_cover(p)?;
    std::fs::write(&cache, &bytes).ok()?;
    Some(cache.to_string_lossy().into_owned())
}

/// Cache-Pfad für ein lokal extrahiertes Album-Cover (Schlüssel: Interpret+Album).
fn save_local_cover(artist: &str, album: &str, bytes: &[u8]) -> Result<PathBuf> {
    let mut path = cover_cache_dir();
    path.push(format!("local_{}.img", name_hash(&format!("{artist}\u{1}{album}"))));
    std::fs::write(&path, bytes)?;
    Ok(path)
}

/// Reichert ein einzelnes Album an: Release suchen, Cover laden, in der DB
/// speichern. Gibt den resultierenden Eintrag zurück (mit `status`).
///
/// Macht genau eine MusicBrainz-Anfrage – der Aufrufer ist für das Einhalten
/// des Rate-Limits ([`RATE_LIMIT`]) zwischen den Aufrufen zuständig.
pub fn enrich_album(client: &OnlineClient, lib: &Library, artist: &str, album: &str) -> AlbumMeta {
    let mut meta = AlbumMeta::pending(artist, album);

    match client.match_release(artist, album) {
        Ok(Some(rel)) => {
            meta.mbid = Some(rel.mbid.clone());
            meta.year = rel.year;
            meta.status = "matched".to_string();

            match client.fetch_cover(&rel) {
                Ok(Some(bytes)) => match save_cover(&rel.mbid, &bytes) {
                    Ok(path) => meta.cover_path = Some(path.to_string_lossy().into_owned()),
                    Err(e) => tracing::warn!("Cover konnte nicht gespeichert werden: {e}"),
                },
                Ok(None) => {}
                Err(e) => tracing::warn!("Cover-Abruf fehlgeschlagen ({artist} – {album}): {e}"),
            }
        }
        Ok(None) => meta.status = "notfound".to_string(),
        Err(e) => {
            tracing::warn!("MusicBrainz-Suche fehlgeschlagen ({artist} – {album}): {e}");
            meta.status = "error".to_string();
        }
    }

    if let Err(e) = lib.upsert_album_meta(&meta) {
        tracing::error!("album_meta konnte nicht gespeichert werden: {e}");
    }
    meta
}

/// Speichert ein Künstlerfoto im Cache und gibt den Pfad zurück.
fn save_artist_image(name: &str, bytes: &[u8]) -> Result<PathBuf> {
    let mut path = artist_cache_dir();
    path.push(format!("{}.img", name_hash(name)));
    std::fs::write(&path, bytes)?;
    Ok(path)
}

/// Persistiert ein bereits (ggf. parallel) geladenes Künstlerfoto: speichert die
/// Bytes im Cache und baut den Meta-Eintrag. Schreibt **nicht** in die DB – das
/// übernimmt der Aufrufer serialisiert (eine SQLite-Verbindung).
pub fn store_artist_image(name: &str, image: Option<Vec<u8>>, errored: bool) -> ArtistMeta {
    let mut meta = ArtistMeta::pending(name);
    match image {
        Some(bytes) => match save_artist_image(name, &bytes) {
            Ok(path) => {
                meta.image_path = Some(path.to_string_lossy().into_owned());
                meta.status = "matched".to_string();
            }
            Err(e) => {
                tracing::warn!("Künstlerfoto nicht speicherbar ({name}): {e}");
                meta.status = "error".to_string();
            }
        },
        None => {
            meta.status = if errored { "error" } else { "notfound" }.to_string();
        }
    }
    meta
}

/// Maximale Anzahl Bilder je Galerie (Album/Interpret).
const MAX_GALLERY: usize = 8;

#[derive(serde::Deserialize)]
struct CaaList {
    #[serde(default)]
    images: Vec<CaaImage>,
}
#[derive(serde::Deserialize)]
struct CaaImage {
    #[serde(default)]
    image: String,
    #[serde(default)]
    front: bool,
    #[serde(default)]
    back: bool,
    #[serde(default)]
    types: Vec<String>,
    #[serde(default)]
    thumbnails: CaaThumbs,
}
#[derive(serde::Deserialize, Default)]
struct CaaThumbs {
    #[serde(rename = "500")]
    n500: Option<String>,
    large: Option<String>,
}
#[derive(serde::Deserialize)]
struct MbArtistSearch {
    #[serde(default)]
    artists: Vec<MbArtist>,
}
#[derive(serde::Deserialize)]
struct MbArtist {
    id: String,
    #[serde(default)]
    score: u32,
}
#[derive(serde::Deserialize)]
struct FanartArtist {
    #[serde(default)]
    artistthumb: Vec<FanartImage>,
    #[serde(default)]
    artistbackground: Vec<FanartImage>,
}
#[derive(serde::Deserialize)]
struct FanartImage {
    #[serde(default)]
    url: String,
}

/// Bestimmt die "Art" eines CAA-Bildes (Front/Back/…) für die Anzeige.
fn caa_kind(types: &[String], front: bool, back: bool) -> String {
    if front {
        "Front".to_string()
    } else if back {
        "Back".to_string()
    } else if let Some(t) = types.first() {
        t.clone()
    } else {
        "Bild".to_string()
    }
}

/// Speichert ein Galerie-Bild im Cover-Cache und liefert den Pfad.
fn save_gallery_image(prefix: &str, key: &str, idx: usize, bytes: &[u8]) -> Result<PathBuf> {
    let mut path = cover_cache_dir();
    path.push(format!("{prefix}_{}_{idx}.img", name_hash(key)));
    std::fs::write(&path, bytes)?;
    Ok(path)
}

/// Speichert eine **bereits geladene** Album-Galerie im Cache und in der DB.
/// (Der Netzabruf passiert getrennt – so koennen mehrere Alben parallel laden
/// und nur das Schreiben wird ueber den Koordinator serialisiert.)
pub fn store_album_gallery(
    lib: &Library,
    artist: &str,
    album: &str,
    imgs: &[(Vec<u8>, String)],
) -> usize {
    let key = format!("{artist}{}{album}", char::from(1u8));
    let mut stored = Vec::new();
    for (i, (bytes, kind)) in imgs.iter().enumerate() {
        match save_gallery_image("albimg", &key, i, bytes) {
            Ok(pp) => {
                stored.push((pp.to_string_lossy().into_owned(), kind.clone(), "caa".to_string()))
            }
            Err(e) => tracing::warn!("Galerie-Bild nicht speicherbar: {e}"),
        }
    }
    if !stored.is_empty() {
        let _ = lib.set_album_images(artist, album, &stored);
    }
    stored.len()
}

/// Holt & speichert die Bildergalerie eines Interpreten (fanart.tv) in die DB.
pub fn enrich_artist_gallery(
    client: &OnlineClient,
    lib: &Library,
    name: &str,
    api_key: &str,
) -> usize {
    let mbid = match client.artist_mbid(name) {
        Ok(Some(id)) => id,
        Ok(None) => return 0,
        Err(e) => {
            tracing::warn!("Artist-MBID fehlgeschlagen ({name}): {e}");
            return 0;
        }
    };
    let imgs = match client.fetch_artist_gallery(api_key, &mbid) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("Interpret-Galerie fehlgeschlagen ({name}): {e}");
            return 0;
        }
    };
    let mut stored = Vec::new();
    for (i, (bytes, kind)) in imgs.iter().enumerate() {
        match save_gallery_image("artimg", name, i, bytes) {
            Ok(pp) => {
                stored.push((pp.to_string_lossy().into_owned(), kind.clone(), "fanart".to_string()))
            }
            Err(e) => tracing::warn!("Interpret-Bild nicht speicherbar: {e}"),
        }
    }
    if !stored.is_empty() {
        let _ = lib.set_artist_images(name, &stored);
    }
    stored.len()
}

/// Erkennt einen Titel per Fingerprint (Chromaprint → AcoustID) und legt die
/// **vorgeschlagenen** Metadaten in der DB ab. Die Datei wird nur gelesen.
///
/// Macht eine AcoustID-Anfrage – der Aufrufer hält [`ACOUSTID_DELAY`] ein.
pub fn enrich_track_fingerprint(
    client: &OnlineClient,
    lib: &Library,
    client_key: &str,
    path: &Path,
) -> TrackMeta {
    let mut meta = TrackMeta::pending(path.to_string_lossy().into_owned());

    let fp = match fingerprint::compute(path) {
        Ok(fp) => fp,
        Err(e) => {
            tracing::warn!("Fingerprint fehlgeschlagen ({}): {e}", path.display());
            meta.status = "error".to_string();
            let _ = lib.upsert_track_meta(&meta);
            return meta;
        }
    };

    match client.acoustid_lookup(client_key, &fp) {
        Ok(Some(m)) => {
            meta.recording_mbid = Some(m.recording_mbid);
            meta.title = m.title;
            meta.artist = m.artist;
            meta.album = m.album;
            meta.status = "matched".to_string();
        }
        Ok(None) => meta.status = "notfound".to_string(),
        Err(e) => {
            tracing::warn!("AcoustID-Abruf fehlgeschlagen ({}): {e}", path.display());
            meta.status = "error".to_string();
        }
    }

    if let Err(e) = lib.upsert_track_meta(&meta) {
        tracing::error!("track_meta konnte nicht gespeichert werden: {e}");
    }
    meta
}

// ---- MusicBrainz-JSON ----

#[derive(Deserialize)]
struct MbSearch {
    #[serde(default)]
    releases: Vec<MbRelease>,
}

#[derive(Deserialize)]
struct MbRelease {
    id: String,
    #[serde(default)]
    score: i32,
    #[serde(default)]
    date: Option<String>,
    #[serde(rename = "release-group", default)]
    release_group: Option<MbReleaseGroup>,
}

#[derive(Deserialize)]
struct MbReleaseGroup {
    id: String,
}

// ---- Deezer-JSON (Künstlerfotos) ----

#[derive(Deserialize)]
struct DzSearch {
    #[serde(default)]
    data: Vec<DzArtist>,
}

#[derive(Deserialize)]
struct DzArtist {
    #[serde(default)]
    picture: Option<String>,
    #[serde(default)]
    picture_medium: Option<String>,
    #[serde(default)]
    picture_big: Option<String>,
    #[serde(default)]
    picture_xl: Option<String>,
}

// ---- AcoustID-JSON (Fingerprint-Erkennung) ----

#[derive(Deserialize)]
struct AcoustIdResp {
    #[serde(default)]
    results: Vec<AcoustIdResult>,
}

#[derive(Deserialize)]
struct AcoustIdResult {
    #[serde(default)]
    score: f64,
    #[serde(default)]
    recordings: Vec<AcoustIdRecording>,
}

#[derive(Deserialize)]
struct AcoustIdRecording {
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    artists: Vec<AcoustIdArtist>,
    #[serde(default)]
    releasegroups: Vec<AcoustIdReleaseGroup>,
}

#[derive(Deserialize)]
struct AcoustIdArtist {
    name: String,
}

#[derive(Deserialize)]
struct AcoustIdReleaseGroup {
    #[serde(default)]
    title: Option<String>,
}

// ---- Hilfsfunktionen ----

/// Liest das Jahr aus einem MusicBrainz-Datum (`2015`, `2015-11`, `2015-11-20`).
fn parse_year(date: &str) -> Option<i32> {
    date.get(0..4).and_then(|y| y.parse().ok())
}

/// Maskiert Lucene-Sonderzeichen in Freitext, damit die Query gültig bleibt.
fn escape_lucene(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '"' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Minimales Percent-Encoding für Query-Strings (RFC 3986 unreserved bleibt).
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
