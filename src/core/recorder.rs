//! Timeshift-Mitschnitt für Streaming-Sender („Mini-DVR"). Ein Hintergrund-Thread
//! liest den Stream über eine **eigene** ICY-Verbindung (zusätzlich zur
//! GStreamer-Wiedergabe), hält die letzten N Minuten in einer **Ring-Datei** im
//! Cache und merkt sich die Songgrenzen (Wechsel von `StreamTitle`). Daraus
//! lassen sich Songs **rückwirkend** als Datei speichern – auch wenn man erst am
//! Songende auf „Aufnahme" drückt.
//!
//! Bewusst entkoppelt von der Wiedergabe: GStreamer spielt, dieser Worker puffert.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Result};

/// Ein im Puffer erkannter Song (Grenze zu Grenze).
#[derive(Debug, Clone)]
pub struct BufferedSong {
    /// Fortlaufender (absoluter, monotoner) Byte-Offset des Songanfangs.
    pub start: u64,
    /// Byte-Offset des Songendes (= Anfang des nächsten Songs); `None` = der Song
    /// läuft gerade noch.
    pub end: Option<u64>,
    /// „Interpret - Titel" aus den ICY-Metadaten.
    pub title: String,
    /// Liegt der **Anfang** noch im Puffer? Sonst wäre eine Aufnahme unvollständig.
    pub complete: bool,
}

/// Schnappschuss des Pufferzustands für die UI (Wiederholungs-Seite, Aufnahme).
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// Anfang des laufenden Songs (absoluter Offset), falls bekannt.
    pub current_start: Option<u64>,
    /// Erkannte Songs (neueste zuletzt), inkl. dem laufenden.
    pub songs: Vec<BufferedSong>,
    /// Worker beendet (Stream zu Ende/Fehler)?
    pub ended: bool,
}

struct Marker {
    offset: u64,
    title: String,
}

#[derive(Default)]
struct Shared {
    current_title: Option<String>,
    markers: Vec<Marker>,
    total: u64,
    cap: u64,
    ended: bool,
}

/// Eindeutige Pufferdateinamen, damit ein neuer Recorder die Datei eines gerade
/// noch auslaufenden alten Workers nicht überschreibt.
static BUFFER_SEQ: AtomicU64 = AtomicU64::new(0);

/// Steuert den Mitschnitt-Worker eines Senders. Beim Verwerfen (`Drop`) wird der
/// Worker gestoppt und die Pufferdatei entfernt.
pub struct Recorder {
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    buffer_path: PathBuf,
    /// Endung der gepufferten Audiodaten (z. B. „mp3"/„aac"). Wird vom Worker
    /// gesetzt, sobald der Content-Type bekannt ist – daher geteilt und beim
    /// Speichern frisch gelesen.
    ext: Arc<Mutex<String>>,
}

impl Recorder {
    /// Startet den Mitschnitt-Worker für `url` mit einem Puffer von `cap_minutes`
    /// Minuten. Liefert sofort zurück; der Worker läuft im Hintergrund.
    pub fn start(url: &str, cap_minutes: u32) -> Recorder {
        let n = BUFFER_SEQ.fetch_add(1, Ordering::Relaxed);
        let mut buffer_path = crate::core::online::cover_cache_dir();
        buffer_path.push(format!("stream_buffer_{}_{n}.dat", std::process::id()));

        let shared = Arc::new(Mutex::new(Shared::default()));
        let stop = Arc::new(AtomicBool::new(false));
        // Endung wird vom Worker gesetzt, sobald der Content-Type bekannt ist;
        // als sinnvoller Standard „mp3" (häufigster ICY-Codec).
        let ext = Arc::new(Mutex::new(String::from("mp3")));

        {
            let (url, shared, stop, buffer_path, ext) = (
                url.to_string(),
                shared.clone(),
                stop.clone(),
                buffer_path.clone(),
                ext.clone(),
            );
            std::thread::spawn(move || {
                if let Err(e) = run(&url, cap_minutes, &buffer_path, &shared, &stop, &ext) {
                    tracing::info!("Stream recorder ended: {e}");
                }
                shared.lock().unwrap().ended = true;
            });
        }

        Recorder {
            shared,
            stop,
            buffer_path,
            ext,
        }
    }

    /// Schnappschuss des Pufferzustands für die UI.
    pub fn snapshot(&self) -> Snapshot {
        let s = self.shared.lock().unwrap();
        let avail = s.total.saturating_sub(s.cap);
        let mut songs: Vec<BufferedSong> = Vec::new();
        for (i, m) in s.markers.iter().enumerate() {
            let end = s.markers.get(i + 1).map(|n| n.offset);
            songs.push(BufferedSong {
                start: m.offset,
                end,
                title: m.title.clone(),
                complete: m.offset >= avail,
            });
        }
        let current_start = s.markers.last().map(|m| m.offset);
        Snapshot {
            current_start,
            songs,
            ended: s.ended,
        }
    }

    /// Schneidet den Byte-Bereich `[start, end)` aus dem Ringpuffer und speichert
    /// ihn als getaggte Audiodatei in `dest_dir`. Gibt den Dateipfad zurück.
    /// `incomplete` markiert (nur Hinweis), dass der Anfang gefehlt haben kann.
    pub fn save_song(
        &self,
        start: u64,
        end: u64,
        artist: Option<&str>,
        title: &str,
        dest_dir: &Path,
    ) -> Result<PathBuf> {
        let cap = self.shared.lock().unwrap().cap;
        let avail = {
            let s = self.shared.lock().unwrap();
            s.total.saturating_sub(s.cap)
        };
        let start = start.max(avail);
        if end <= start {
            return Err(anyhow!("nothing buffered for this segment"));
        }
        let data = read_ring(&self.buffer_path, cap, start, end)?;
        std::fs::create_dir_all(dest_dir)?;
        let base = sanitize_filename(artist, title);
        let ext = self.ext.lock().unwrap().clone();
        let path = unique_path(dest_dir, &base, &ext);
        std::fs::write(&path, &data)?;
        tag_file(&path, artist, title);
        Ok(path)
    }

    /// Schneidet `[start, end)` in eine **temporäre** Datei (zum Probehören in der
    /// Wiederholung) und gibt deren Pfad zurück.
    pub fn extract_temp(&self, start: u64, end: u64) -> Result<PathBuf> {
        let cap = self.shared.lock().unwrap().cap;
        let avail = self.shared.lock().unwrap().total.saturating_sub(cap);
        let start = start.max(avail);
        if end <= start {
            return Err(anyhow!("nothing buffered for this segment"));
        }
        let data = read_ring(&self.buffer_path, cap, start, end)?;
        let ext = self.ext.lock().unwrap().clone();
        let mut path = std::env::temp_dir();
        let n = BUFFER_SEQ.fetch_add(1, Ordering::Relaxed);
        path.push(format!("emilia_replay_{}_{n}.{ext}", std::process::id()));
        std::fs::write(&path, &data)?;
        Ok(path)
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = std::fs::remove_file(&self.buffer_path);
    }
}

/// Worker-Schleife: liest den ICY-Stream, puffert Audio im Ring und führt die
/// Songgrenzen nach.
fn run(
    url: &str,
    cap_minutes: u32,
    buffer_path: &Path,
    shared: &Arc<Mutex<Shared>>,
    stop: &Arc<AtomicBool>,
    ext_out: &Arc<Mutex<String>>,
) -> Result<()> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(8))
        .timeout_read(Duration::from_secs(20))
        .build();
    let resp = agent
        .get(url)
        .set("Icy-MetaData", "1")
        .set("User-Agent", &format!("Emilia/{}", env!("CARGO_PKG_VERSION")))
        .call()?;

    let metaint: usize = resp
        .header("icy-metaint")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let ext = ext_from_content_type(resp.header("Content-Type"));
    *ext_out.lock().unwrap() = ext.to_string();
    // Pufferkapazität in Bytes aus der Bitrate ableiten (Standard 256 kbit/s).
    let br_kbps: u64 = resp
        .header("icy-br")
        .and_then(|s| s.split(',').next())
        .and_then(|s| s.trim().parse().ok())
        .filter(|&b: &u64| b > 0)
        .unwrap_or(256);
    let bytes_per_sec = (br_kbps * 1000 / 8).max(8_000);
    let cap = (cap_minutes as u64 * 60 * bytes_per_sec).max(bytes_per_sec * 10);
    shared.lock().unwrap().cap = cap;

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(true)
        .open(buffer_path)?;
    let mut reader = std::io::BufReader::new(resp.into_reader());
    let mut total: u64 = 0;
    let mut buf = vec![0u8; 16 * 1024];
    // Entprellung: Ein neuer `StreamTitle` gilt erst dann als echte Songgrenze,
    // wenn er mindestens `min_bytes` (≈ MIN_SONG_SECS Sekunden) lang anliegt. So
    // werden Sender-Kennungen, Werbe-Einblendungen und schnelle Hin-und-Her-
    // Wechsel der Anzeige ignoriert (sonst entstünden mehrere Dateien je Lied).
    let min_bytes = MIN_SONG_SECS * bytes_per_sec;
    // Kandidat für die nächste Songgrenze: (Startoffset, Titel).
    let mut pending: Option<(u64, String)> = None;

    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        // Audio-Anteil lesen (entweder bis zur nächsten Metadaten-Marke, oder –
        // ohne ICY-Metadaten – einfach blockweise).
        let chunk = if metaint > 0 { metaint } else { buf.len() };
        let mut remaining = chunk;
        while remaining > 0 {
            let want = remaining.min(buf.len());
            let n = reader.read(&mut buf[..want])?;
            if n == 0 {
                return Ok(());
            }
            write_ring(&mut file, cap, total, &buf[..n])?;
            total += n as u64;
            remaining -= n;
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }
        }
        {
            let mut s = shared.lock().unwrap();
            s.total = total;
            // Hält ein Kandidat lange genug an → als echte Songgrenze festschreiben.
            let commit = pending
                .as_ref()
                .filter(|(off, _)| total.saturating_sub(*off) >= min_bytes)
                .cloned();
            if let Some((off, title)) = commit {
                s.markers.push(Marker {
                    offset: off,
                    title: title.clone(),
                });
                s.current_title = Some(title);
                if s.markers.len() > 256 {
                    s.markers.remove(0);
                }
                pending = None;
            }
            prune_markers(&mut s);
        }

        if metaint == 0 {
            continue;
        }

        // Metadatenblock: 1 Längenbyte, dann len*16 Bytes Text.
        let mut lenb = [0u8; 1];
        if reader.read(&mut lenb)? == 0 {
            return Ok(());
        }
        let mlen = lenb[0] as usize * 16;
        if mlen > 0 {
            let mut meta = vec![0u8; mlen];
            reader.read_exact(&mut meta)?;
            if let Some(title) = parse_stream_title(&meta) {
                if !title.trim().is_empty() {
                    let current = shared.lock().unwrap().current_title.clone();
                    if current.as_deref() == Some(title.as_str()) {
                        // Zurück zum laufenden Lied (z. B. nach einer Einblendung)
                        // → Kandidat verwerfen, kein neuer Song.
                        pending = None;
                    } else if pending.as_ref().map(|(_, t)| t.as_str()) != Some(title.as_str()) {
                        // Neuer Kandidat – Startoffset = hier; muss sich erst halten.
                        pending = Some((total, title));
                    }
                    // Gleicher Kandidat: Startoffset beibehalten (nichts tun).
                }
            }
        }
    }
}

/// Mindestdauer (Sekunden), die ein Titel anliegen muss, um als eigener Song zu
/// gelten. Kürzere Einblendungen (Werbung, Sender-Kennung, schnelle Wechsel)
/// werden dem laufenden Song zugeschlagen statt eine zweite Datei zu erzeugen.
const MIN_SONG_SECS: u64 = 30;

/// Entfernt vollständig aus dem Puffer gelaufene Songgrenzen (deren Folge-Grenze
/// schon jenseits des verfügbaren Fensters liegt) – behält aber den ältesten
/// noch teilweise vorhandenen Song.
fn prune_markers(s: &mut Shared) {
    let avail = s.total.saturating_sub(s.cap);
    while s.markers.len() >= 2 && s.markers[1].offset <= avail {
        s.markers.remove(0);
    }
}

/// Schreibt `data` an die absolute Position `total` in die Ring-Datei (wrappt am
/// Kapazitätsende).
fn write_ring(file: &mut std::fs::File, cap: u64, total: u64, data: &[u8]) -> Result<()> {
    let pos = total % cap;
    let first = ((cap - pos) as usize).min(data.len());
    file.seek(SeekFrom::Start(pos))?;
    file.write_all(&data[..first])?;
    if first < data.len() {
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&data[first..])?;
    }
    Ok(())
}

/// Liest den absoluten Byte-Bereich `[start, end)` aus der Ring-Datei (mit
/// eigenem Lese-Handle; wrappt am Kapazitätsende).
fn read_ring(buffer_path: &Path, cap: u64, start: u64, end: u64) -> Result<Vec<u8>> {
    let len = (end - start) as usize;
    let mut out = vec![0u8; len];
    let mut file = std::fs::File::open(buffer_path)?;
    let pos = start % cap;
    let first = ((cap - pos) as usize).min(len);
    file.seek(SeekFrom::Start(pos))?;
    file.read_exact(&mut out[..first])?;
    if first < len {
        file.seek(SeekFrom::Start(0))?;
        file.read_exact(&mut out[first..])?;
    }
    Ok(out)
}

/// Liest den Wert von `StreamTitle='…'` aus einem ICY-Metadatenblock.
fn parse_stream_title(meta: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(meta);
    let start = text.find("StreamTitle=")? + "StreamTitle=".len();
    let rest = &text[start..];
    // Wert steht in einfachen Anführungszeichen: 'Interpret - Titel';
    let rest = rest.strip_prefix('\'').unwrap_or(rest);
    let endq = rest.find("';").or_else(|| rest.find('\'')).unwrap_or(rest.len());
    let value = rest[..endq].trim_matches(char::from(0)).trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

/// Teilt „Interpret - Titel" auf (für Tags/Dateiname). Ohne Trenner gilt alles
/// als Titel.
pub fn split_artist_title(stream_title: &str) -> (Option<String>, String) {
    if let Some((a, t)) = stream_title.split_once(" - ") {
        let a = a.trim();
        let t = t.trim();
        if !a.is_empty() && !t.is_empty() {
            return (Some(a.to_string()), t.to_string());
        }
    }
    (None, stream_title.trim().to_string())
}

fn ext_from_content_type(ct: Option<&str>) -> &'static str {
    match ct.map(|s| s.to_ascii_lowercase()) {
        Some(c) if c.contains("aac") => "aac",
        Some(c) if c.contains("ogg") || c.contains("opus") => "ogg",
        Some(c) if c.contains("flac") => "flac",
        _ => "mp3",
    }
}

/// Macht aus „Interpret - Titel" einen brauchbaren Dateinamensstamm.
fn sanitize_filename(artist: Option<&str>, title: &str) -> String {
    let raw = match artist {
        Some(a) if !a.trim().is_empty() => format!("{a} - {title}"),
        _ => title.to_string(),
    };
    let cleaned: String = raw
        .chars()
        .map(|c| if "/\\:*?\"<>|".contains(c) { '_' } else { c })
        .collect();
    let cleaned = cleaned.trim().trim_matches('.').trim();
    if cleaned.is_empty() {
        "Aufnahme".to_string()
    } else {
        cleaned.chars().take(120).collect()
    }
}

/// Findet einen freien Dateipfad (hängt bei Bedarf „ (2)" usw. an).
fn unique_path(dir: &Path, base: &str, ext: &str) -> PathBuf {
    let mut p = dir.join(format!("{base}.{ext}"));
    let mut i = 2;
    while p.exists() {
        p = dir.join(format!("{base} ({i}).{ext}"));
        i += 1;
    }
    p
}

/// Schreibt Interpret/Titel/Album **und ein Cover** in eine bereits gespeicherte
/// Aufnahme (best effort). Wird im Hintergrund aufgerufen, nachdem online ein
/// Cover gefunden wurde.
pub fn embed_cover(
    path: &Path,
    artist: Option<&str>,
    title: &str,
    album: Option<&str>,
    image: &[u8],
) {
    use lofty::config::WriteOptions;
    use lofty::picture::{MimeType, Picture, PictureType};
    use lofty::prelude::{Accessor, TagExt};
    use lofty::tag::{Tag, TagType};

    let mut tag = Tag::new(TagType::Id3v2);
    tag.set_title(title.to_string());
    if let Some(a) = artist.filter(|a| !a.trim().is_empty()) {
        tag.set_artist(a.to_string());
    }
    if let Some(al) = album.filter(|a| !a.trim().is_empty()) {
        tag.set_album(al.to_string());
    }
    let mime = if image.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
        MimeType::Png
    } else {
        MimeType::Jpeg
    };
    let pic = Picture::new_unchecked(PictureType::CoverFront, Some(mime), None, image.to_vec());
    tag.push_picture(pic);
    if let Err(e) = tag.save_to_path(path, WriteOptions::default()) {
        tracing::debug!("Could not embed cover into {}: {e}", path.display());
    }
}

/// Schreibt Interpret/Titel als Tag (best effort – schlägt es fehl, bleibt die
/// Datei eben untagged, der Dateiname trägt die Info bereits).
fn tag_file(path: &Path, artist: Option<&str>, title: &str) {
    use lofty::config::WriteOptions;
    use lofty::prelude::{Accessor, TagExt};
    use lofty::tag::{Tag, TagType};

    let mut tag = Tag::new(TagType::Id3v2);
    tag.set_title(title.to_string());
    if let Some(a) = artist.filter(|a| !a.trim().is_empty()) {
        tag.set_artist(a.to_string());
    }
    if let Err(e) = tag.save_to_path(path, WriteOptions::default()) {
        tracing::debug!("Could not tag recording {}: {e}", path.display());
    }
}
