//! Minimaler, **lesender** WebDAV-Client (Nextcloud) über das blockierende
//! `ureq`. Kann Verzeichnisse auflisten (PROPFIND), Tags aus den ersten
//! Kilobytes einer Datei lesen (Range-GET) und Dateien herunterladen (GET).
//!
//! Bewusst schlank gehalten und ausschließlich aus Hintergrund-Workern
//! aufgerufen (siehe `src/ui/app_streaming` bzw. die `Cmd::Remote*`-Pfade).
//! Die Audiodateien in der Cloud werden dabei niemals verändert.

use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Result};
use base64::Engine;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::tag::Accessor;
use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, CONTROLS};

use crate::core::scanner;
use crate::model::Source;

/// Zu kodierende Zeichen in einem einzelnen Pfadsegment (ohne den Trenner `/`).
const PATH_SEGMENT: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'\\');

/// Zu kodierende Zeichen im User-Info-Teil (`user:pass@`) einer URL.
const USERINFO: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'/')
    .add(b':')
    .add(b'@')
    .add(b'\\')
    .add(b'[')
    .add(b']');

const PROPFIND_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:"><d:prop>
<d:resourcetype/><d:displayname/><d:getcontentlength/><d:getcontenttype/>
</d:prop></d:propfind>"#;

/// Zugangsdaten + Musik-Wurzel einer Nextcloud-/WebDAV-Quelle.
#[derive(Debug, Clone)]
pub struct Creds {
    /// Basis-URL ohne abschließenden Slash, z. B. `https://cloud.example.com`
    /// (darf einen Unterpfad enthalten, z. B. `https://host/nextcloud`).
    pub base_url: String,
    pub user: String,
    pub pass: String,
    /// Unterpfad zur Musik (normalisiert: führender Slash, ohne Schluss-Slash;
    /// leer = Cloud-Wurzel), z. B. `/Music`.
    pub music_path: String,
}

impl Creds {
    /// Aus einer `webdav`-Quelle. `None`, wenn Pflichtfelder fehlen.
    pub fn from_source(s: &Source) -> Option<Self> {
        Some(Self {
            base_url: s.base_url.clone()?.trim_end_matches('/').to_string(),
            user: s.username.clone()?,
            pass: s.password.clone()?,
            music_path: normalize_path(s.music_path.as_deref().unwrap_or("")),
        })
    }
}

/// Ein Eintrag aus einem WebDAV-Verzeichnis (Ordner oder Audiodatei).
#[derive(Debug, Clone)]
pub struct DavEntry {
    /// Pfad **relativ zur Musikwurzel** (führender Slash), z. B. `/Alben/X`.
    pub rel_path: String,
    /// Anzeigename (letztes Pfadsegment bzw. `displayname`).
    pub name: String,
    pub is_dir: bool,
}

// ---------------------------------------------------------------------------
// URL-/Pfad-Hilfen
// ---------------------------------------------------------------------------

fn normalize_path(p: &str) -> String {
    let p = p.trim();
    if p.is_empty() || p == "/" {
        return String::new();
    }
    let p = p.trim_end_matches('/');
    if p.starts_with('/') {
        p.to_string()
    } else {
        format!("/{p}")
    }
}

fn scheme_rest(base: &str) -> (&str, &str) {
    base.split_once("://").unwrap_or(("https", base))
}

/// Zerlegt `authority[/pfad]` in (authority, pfad) – pfad inkl. führendem Slash
/// bzw. leer.
fn authority_and_path(rest: &str) -> (&str, &str) {
    match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    }
}

/// Kodiert einen Pfad segmentweise (die `/`-Trenner bleiben erhalten).
fn encode_path(path: &str) -> String {
    path.split('/')
        .map(|seg| utf8_percent_encode(seg, PATH_SEGMENT).to_string())
        .collect::<Vec<_>>()
        .join("/")
}

/// DAV-Pfad-Suffix (kodiert) ab der Authority: `/remote.php/dav/files/USER/...`.
fn dav_suffix(c: &Creds, rel: &str) -> String {
    let enc_user = utf8_percent_encode(&c.user, PATH_SEGMENT).to_string();
    let full = format!("{}{}", c.music_path, rel);
    format!("/remote.php/dav/files/{}{}", enc_user, encode_path(&full))
}

/// Volle DAV-URL (für `ureq`; Authentifizierung läuft über einen Header).
fn url_for(c: &Creds, rel: &str) -> String {
    format!("{}{}", c.base_url, dav_suffix(c, rel))
}

/// Abspielbare URI mit eingebetteten Zugangsdaten (für GStreamer/`play_uri`).
pub fn stream_uri(c: &Creds, rel: &str) -> String {
    let (scheme, rest) = scheme_rest(&c.base_url);
    let enc_user = utf8_percent_encode(&c.user, USERINFO);
    let enc_pass = utf8_percent_encode(&c.pass, USERINFO);
    format!("{scheme}://{enc_user}:{enc_pass}@{rest}{}", dav_suffix(c, rel))
}

/// Erwarteter (dekodierter) Pfad der PROPFIND-Anfrage – Präfix der Kind-Hrefs.
fn req_path_decoded(c: &Creds, rel: &str) -> String {
    let (_, rest) = scheme_rest(&c.base_url);
    let (_authority, base_path) = authority_and_path(rest);
    format!(
        "{}/remote.php/dav/files/{}{}{}",
        base_path.trim_end_matches('/'),
        c.user,
        c.music_path,
        rel
    )
}

/// Extrahiert den (dekodierten) Pfad-Teil aus einem href (Pfad oder volle URL).
fn href_to_path(href: &str) -> String {
    let path = if href.starts_with("http") {
        href.split_once("://")
            .and_then(|(_, r)| r.find('/').map(|i| &r[i..]))
            .unwrap_or(href)
    } else {
        href
    };
    percent_decode_str(path).decode_utf8_lossy().to_string()
}

fn auth_header(c: &Creds) -> String {
    let token = base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", c.user, c.pass));
    format!("Basic {token}")
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(8))
        .timeout_read(Duration::from_secs(30))
        .build()
}

// ---------------------------------------------------------------------------
// PROPFIND – Verzeichnis auflisten
// ---------------------------------------------------------------------------

#[derive(Default)]
struct RawEntry {
    href: String,
    display_name: Option<String>,
    is_dir: bool,
}

/// Welcher Textwert gerade eingelesen wird (zwischen Start- und End-Tag).
#[derive(Clone, Copy)]
enum Field {
    Href,
    Display,
}

/// Parst eine WebDAV-`multistatus`-Antwort in rohe Einträge.
fn parse_propfind(xml: &str) -> Vec<RawEntry> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut out = Vec::new();
    let mut cur: Option<RawEntry> = None;
    let mut field: Option<Field> = None;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let name = local_name(e.name().as_ref());
                match name.as_str() {
                    "response" => cur = Some(RawEntry::default()),
                    "href" => field = Some(Field::Href),
                    "displayname" => field = Some(Field::Display),
                    "collection" => {
                        if let Some(c) = cur.as_mut() {
                            c.is_dir = true;
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(t)) => {
                if let (Some(c), Some(f)) = (cur.as_mut(), field) {
                    let val = t.unescape().unwrap_or_default().trim().to_string();
                    if !val.is_empty() {
                        match f {
                            Field::Href => c.href = val,
                            Field::Display => c.display_name = Some(val),
                        }
                    }
                }
            }
            Ok(Event::End(e)) => {
                let name = local_name(e.name().as_ref());
                if name == "response" {
                    if let Some(c) = cur.take() {
                        out.push(c);
                    }
                }
                field = None;
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }
    out
}

/// Lokaler Elementname ohne Namespace-Präfix (`d:href` → `href`).
fn local_name(qname: &[u8]) -> String {
    let s = String::from_utf8_lossy(qname);
    match s.rsplit_once(':') {
        Some((_, local)) => local.to_string(),
        None => s.to_string(),
    }
}

/// Listet ein Verzeichnis (Depth: 1) relativ zur Musikwurzel auf. Liefert nur
/// Ordner und Audiodateien; der Selbst-Eintrag wird herausgefiltert.
pub fn list(c: &Creds, rel: &str) -> Result<Vec<DavEntry>> {
    let url = url_for(c, rel);
    let body = agent()
        .request("PROPFIND", &url)
        .set("Depth", "1")
        .set("Authorization", &auth_header(c))
        .set("Content-Type", "application/xml")
        .send_string(PROPFIND_BODY)
        .map_err(|e| anyhow!("PROPFIND fehlgeschlagen: {e}"))?
        .into_string()
        .map_err(|e| anyhow!("Antwort nicht lesbar: {e}"))?;

    let prefix = req_path_decoded(c, rel);
    let prefix = prefix.trim_end_matches('/');
    let mut out = Vec::new();
    for raw in parse_propfind(&body) {
        let hp = href_to_path(&raw.href);
        let hp = hp.trim_end_matches('/');
        if hp == prefix {
            continue; // Selbst-Eintrag
        }
        let Some(rem) = hp.strip_prefix(prefix) else {
            continue;
        };
        let child = rem.trim_start_matches('/');
        if child.is_empty() {
            continue;
        }
        // Bei Depth:1 nur eine Ebene – zur Sicherheit erste Komponente nehmen.
        let child_name = child.split('/').next().unwrap_or(child).to_string();
        let name = raw.display_name.clone().unwrap_or_else(|| child_name.clone());
        if !raw.is_dir && !scanner::is_audio(Path::new(&name)) {
            continue; // Nicht-Audio-Dateien ausblenden
        }
        out.push(DavEntry {
            rel_path: format!("{rel}/{child_name}"),
            name,
            is_dir: raw.is_dir,
        });
    }
    Ok(out)
}

/// Verbindungstest: PROPFIND (Depth 0) auf die Musikwurzel. `Ok` = erreichbar
/// und authentifiziert.
pub fn test_connection(c: &Creds) -> Result<()> {
    agent()
        .request("PROPFIND", &url_for(c, ""))
        .set("Depth", "0")
        .set("Authorization", &auth_header(c))
        .set("Content-Type", "application/xml")
        .send_string(PROPFIND_BODY)
        .map_err(|e| anyhow!("{e}"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tags & Download
// ---------------------------------------------------------------------------

/// Liest Titel/Interpret/Dauer aus den ersten ~512 KB einer Datei (Range-GET)
/// in einen Speicherpuffer und lässt `lofty` darüber laufen. Best effort: bei
/// Formaten mit Metadaten am Dateiende (z. B. unoptimiertes MP4) schlägt das
/// fehl und liefert `None` – die Aufrufer fallen dann auf den Dateinamen zurück.
pub fn read_tags(c: &Creds, rel: &str) -> (Option<String>, Option<String>, Option<i64>) {
    let url = url_for(c, rel);
    let resp = agent()
        .get(&url)
        .set("Authorization", &auth_header(c))
        .set("Range", "bytes=0-524287")
        .call();
    let mut buf = Vec::new();
    match resp {
        Ok(r) => {
            if r.into_reader().take(600_000).read_to_end(&mut buf).is_err() {
                return (None, None, None);
            }
        }
        Err(_) => return (None, None, None),
    }
    // `lofty::read_from` erwartet eine `File`; über einen Speicherpuffer geht es
    // mit `Probe` (Read + Seek auf dem `Cursor`, rein lokal – kein HTTP-Seek).
    let tagged = match lofty::probe::Probe::new(Cursor::new(buf)).guess_file_type() {
        Ok(p) => match p.read() {
            Ok(t) => t,
            Err(_) => return (None, None, None),
        },
        Err(_) => return (None, None, None),
    };
    let duration_ms = match tagged.properties().duration().as_millis() {
        0 => None,
        ms => Some(ms as i64),
    };
    let (title, artist) = match tagged.primary_tag().or_else(|| tagged.first_tag()) {
        Some(tag) => (
            tag.title().map(|c| c.trim().to_string()).filter(|s| !s.is_empty()),
            tag.artist().map(|c| c.trim().to_string()).filter(|s| !s.is_empty()),
        ),
        None => (None, None),
    };
    (title, artist, duration_ms)
}

/// Vollständige Metadaten eines entfernten Titels (für die Indizierung in die
/// gleiche Datenbank wie lokale Lieder).
#[derive(Default)]
pub struct RemoteMeta {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub genre: Option<String>,
    pub track_no: Option<u32>,
    pub disc_no: Option<u32>,
    pub duration_ms: Option<i64>,
}

/// Wie [`read_tags`], liest aber **alle** für die Bibliothek nötigen Felder
/// (zusätzlich Album, Genre, Track-/CD-Nr.) aus den ersten ~512 KB der Datei.
pub fn read_meta(c: &Creds, rel: &str) -> RemoteMeta {
    let url = url_for(c, rel);
    let resp = agent()
        .get(&url)
        .set("Authorization", &auth_header(c))
        .set("Range", "bytes=0-524287")
        .call();
    let mut buf = Vec::new();
    match resp {
        Ok(r) => {
            if r.into_reader().take(600_000).read_to_end(&mut buf).is_err() {
                return RemoteMeta::default();
            }
        }
        Err(_) => return RemoteMeta::default(),
    }
    let tagged = match lofty::probe::Probe::new(Cursor::new(buf)).guess_file_type() {
        Ok(p) => match p.read() {
            Ok(t) => t,
            Err(_) => return RemoteMeta::default(),
        },
        Err(_) => return RemoteMeta::default(),
    };
    let duration_ms = match tagged.properties().duration().as_millis() {
        0 => None,
        ms => Some(ms as i64),
    };
    let mut m = RemoteMeta {
        duration_ms,
        ..Default::default()
    };
    if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
        let clean = |s: Option<std::borrow::Cow<str>>| {
            s.map(|c| c.trim().to_string()).filter(|s| !s.is_empty())
        };
        m.title = clean(tag.title());
        m.artist = clean(tag.artist());
        m.album = clean(tag.album());
        m.genre = clean(tag.genre());
        m.track_no = tag.track();
        m.disc_no = tag.disk();
    }
    m
}

/// Synthetischer Pfad eines entfernten Titels: `nc:<source_id>:<rel>`. So liegen
/// Cloud-Titel in derselben `track`-Tabelle wie lokale und verhalten sich 1:1.
pub fn nc_path(source_id: i64, rel: &str) -> String {
    format!("nc:{source_id}:{rel}")
}

/// Zerlegt einen synthetischen Pfad `nc:<id>:<rel>` in (Quellen-Id, rel).
pub fn parse_nc_path(path: &str) -> Option<(i64, String)> {
    let rest = path.strip_prefix("nc:")?;
    let (id, rel) = rest.split_once(':')?;
    Some((id.parse().ok()?, rel.to_string()))
}

/// Sammelt **rekursiv** alle Audiodatei-Pfade (relativ zur Musikwurzel) unter
/// `rel`. Defensiv gedeckelt (Verzeichnis-/Dateizahl), damit eine sehr große
/// Cloud nicht endlos läuft.
pub fn walk(c: &Creds, rel: &str) -> Vec<String> {
    const MAX_DIRS: usize = 4000;
    const MAX_FILES: usize = 50_000;
    let mut files = Vec::new();
    let mut stack = vec![rel.to_string()];
    let mut dirs_seen = 0usize;
    while let Some(dir) = stack.pop() {
        dirs_seen += 1;
        if dirs_seen > MAX_DIRS || files.len() >= MAX_FILES {
            tracing::warn!("WebDAV walk capped (dirs/files limit reached)");
            break;
        }
        let Ok(entries) = list(c, &dir) else {
            continue; // Verzeichnis nicht lesbar – überspringen
        };
        for e in entries {
            if e.is_dir {
                stack.push(e.rel_path);
            } else {
                files.push(e.rel_path);
            }
        }
    }
    files
}

/// Liest die komplette Musikbibliothek einer Quelle rekursiv ein und legt die
/// Titel in der Datenbank ab (synthetischer Pfad). Danach erscheinen sie wie
/// lokale Lieder in Interpreten/Alben. **Blockierend** – nur aus Worker-Threads.
/// Liefert die Anzahl indizierter Titel.
pub fn index_into(lib: &crate::core::db::Library, source: &Source) -> Result<usize> {
    let Some(c) = Creds::from_source(source) else {
        return Err(anyhow!("incomplete source credentials"));
    };
    let files = walk(&c, "");
    let mut n = 0;
    for rel in files {
        let meta = read_meta(&c, &rel);
        let name = rel.rsplit('/').next().unwrap_or(&rel);
        let title = meta.title.unwrap_or_else(|| {
            Path::new(name)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or(name)
                .to_string()
        });
        let track = crate::model::Track {
            id: 0,
            path: nc_path(source.id, &rel),
            title,
            artist: meta.artist,
            album: meta.album,
            genre: meta.genre,
            track_no: meta.track_no,
            disc_no: meta.disc_no,
            duration_ms: meta.duration_ms,
            resume_ms: 0,
        };
        if lib.upsert_track(&track).is_ok() {
            n += 1;
        }
    }
    Ok(n)
}

/// Lädt eine Datei vollständig nach `dest` (atomar über eine `.part`-Datei).
pub fn download(c: &Creds, rel: &str, dest: &Path) -> Result<()> {
    let url = url_for(c, rel);
    let resp = agent()
        .get(&url)
        .set("Authorization", &auth_header(c))
        .call()
        .map_err(|e| anyhow!("Download fehlgeschlagen: {e}"))?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = dest.with_extension("part");
    let mut file = std::fs::File::create(&tmp)?;
    std::io::copy(&mut resp.into_reader(), &mut file)?;
    file.sync_all().ok();
    std::fs::rename(&tmp, dest)?;
    Ok(())
}

/// Lokaler Cache-Pfad einer entfernten Datei:
/// `$XDG_DATA_HOME/emilia/cache/<source-id>/<rel-pfad>`.
pub fn cache_path(source_id: i64, rel: &str) -> PathBuf {
    let mut dir = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
    dir.push("emilia");
    dir.push("cache");
    dir.push(source_id.to_string());
    for seg in rel.split('/').filter(|s| !s.is_empty()) {
        dir.push(seg);
    }
    dir
}

// ---------------------------------------------------------------------------
// Nextcloud-Login-QR
// ---------------------------------------------------------------------------

/// Parst einen Nextcloud-Login-QR `nc://login/server:URL&user:USER&password:PW`
/// → `(server, user, password)`.
pub fn parse_nc_login(qr: &str) -> Option<(String, String, String)> {
    let rest = qr.trim().strip_prefix("nc://login/")?;
    let (mut server, mut user, mut password) = (None, None, None);
    for part in rest.split('&') {
        if let Some(v) = part.strip_prefix("server:") {
            server = Some(v.trim_end_matches('/').to_string());
        } else if let Some(v) = part.strip_prefix("user:") {
            user = Some(v.to_string());
        } else if let Some(v) = part.strip_prefix("password:") {
            password = Some(v.to_string());
        }
    }
    Some((server?, user?, password?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creds() -> Creds {
        Creds {
            base_url: "https://cloud.example.com".into(),
            user: "alice".into(),
            pass: "se cret".into(),
            music_path: "/My Music".into(),
        }
    }

    #[test]
    fn builds_dav_url_and_stream_uri() {
        let c = creds();
        assert_eq!(
            url_for(&c, "/Alben/X"),
            "https://cloud.example.com/remote.php/dav/files/alice/My%20Music/Alben/X"
        );
        assert_eq!(
            stream_uri(&c, "/Alben/X"),
            "https://alice:se%20cret@cloud.example.com/remote.php/dav/files/alice/My%20Music/Alben/X"
        );
    }

    #[test]
    fn strips_self_and_keeps_children() {
        let c = creds();
        let xml = r#"<?xml version="1.0"?>
        <d:multistatus xmlns:d="DAV:">
          <d:response><d:href>/remote.php/dav/files/alice/My%20Music/</d:href>
            <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat>
          </d:response>
          <d:response><d:href>/remote.php/dav/files/alice/My%20Music/Alben/</d:href>
            <d:propstat><d:prop><d:displayname>Alben</d:displayname>
            <d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat>
          </d:response>
          <d:response><d:href>/remote.php/dav/files/alice/My%20Music/song.mp3</d:href>
            <d:propstat><d:prop><d:displayname>song.mp3</d:displayname>
            <d:getcontentlength>123</d:getcontentlength>
            <d:resourcetype/></d:prop></d:propstat>
          </d:response>
        </d:multistatus>"#;
        // parse + Filterung wie in `list`, aber ohne Netz:
        let prefix = req_path_decoded(&c, "");
        let prefix = prefix.trim_end_matches('/');
        let names: Vec<(String, bool)> = parse_propfind(xml)
            .into_iter()
            .filter_map(|raw| {
                let hp = href_to_path(&raw.href);
                let hp = hp.trim_end_matches('/').to_string();
                if hp == prefix {
                    return None;
                }
                let rem = hp.strip_prefix(prefix)?.trim_start_matches('/').to_string();
                if rem.is_empty() {
                    return None;
                }
                Some((rem, raw.is_dir))
            })
            .collect();
        assert_eq!(
            names,
            vec![("Alben".to_string(), true), ("song.mp3".to_string(), false)]
        );
    }

    #[test]
    fn parses_nc_login() {
        let qr = "nc://login/server:https://cloud.example.com&user:alice&password:abc-123";
        assert_eq!(
            parse_nc_login(qr),
            Some((
                "https://cloud.example.com".into(),
                "alice".into(),
                "abc-123".into()
            ))
        );
    }
}
