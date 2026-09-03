//! **Read-only** SMB2/3 client for "SMB share" music sources (NAS boxes,
//! Windows shares, Samba) via the pure-Rust `smb` crate in its synchronous
//! `multi_threaded` model — no libsmbclient, no gvfs backend on the host, so it
//! works identically on the desktop, the phone and inside the Flatpak.
//!
//! A source is `smb://host[:port]/share` + a music subpath inside the share;
//! paths handed around the app stay `/`-separated and relative to that music
//! root (like the WebDAV source) and are turned into UNC paths here. One SMB
//! session is opened lazily per [`SmbCreds`] value and shared by its clones, so
//! a worker that reads the tags of forty files does not negotiate forty times.
//! Streaming goes through [`crate::core::media_proxy`], which asks for byte
//! ranges via [`open_range`]. Nothing on the share is ever written.

use std::fmt;
use std::io::Read;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Result};
use smb::{
    Client, ClientConfig, FileAccessMask, FileCreateArgs, FileDirectoryInformation, GetLen, ReadAt,
    Resource, UncPath,
};

use crate::core::remote::{clamp_range, normalize_music_path, RangeBody, RemoteEntry};
use crate::core::scanner;
use crate::model::Source;

/// Connect/negotiate/read timeout of the SMB session.
const TIMEOUT: Duration = Duration::from_secs(10);
/// Largest single read request; the server's negotiated maximum caps it further.
const READ_CHUNK: usize = 256 * 1024;

/// Credentials + music root of an SMB source.
#[derive(Clone)]
pub struct SmbCreds {
    pub host: String,
    /// Explicit port (`None` = 445).
    pub port: Option<u16>,
    pub share: String,
    /// `user`, `DOMAIN\user` or `user@domain` (parsed by the SSPI layer).
    pub user: String,
    pub pass: String,
    /// Subpath to the music inside the share (normalized: leading slash, no
    /// trailing slash; empty = share root), e.g. `/Music`.
    pub music_path: String,
    /// Lazily opened session, shared between clones of these credentials.
    conn: Arc<Mutex<Option<Arc<Conn>>>>,
}

impl fmt::Debug for SmbCreds {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SmbCreds")
            .field("location", &self.location())
            .field("user", &self.user)
            .field("music_path", &self.music_path)
            .finish_non_exhaustive()
    }
}

/// One connected share: the client plus the UNC root `\\host\share`.
struct Conn {
    client: Client,
    root: UncPath,
}

/// Where a share lives, as typed by the user or stored in `source.base_url`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub host: String,
    pub port: Option<u16>,
    pub share: String,
    /// Any path the user typed *behind* the share (`smb://nas/music/Alben` →
    /// `/Alben`), offered as the default music folder. Empty when absent.
    pub subpath: String,
}

impl Location {
    /// Canonical `smb://host[:port]/share` form (what the DB stores).
    pub fn to_url(&self) -> String {
        match self.port {
            Some(p) => format!("smb://{}:{p}/{}", self.host, self.share),
            None => format!("smb://{}/{}", self.host, self.share),
        }
    }
}

/// Parses `smb://host[:port]/share[/sub]`, `//host/share`, `\\host\share` or a
/// bare `host/share`. `None` without both a host and a share name.
pub fn parse_location(input: &str) -> Option<Location> {
    let s = input.trim();
    let s = s
        .strip_prefix("smb://")
        .or_else(|| s.strip_prefix("SMB://"))
        .or_else(|| s.strip_prefix("cifs://"))
        .unwrap_or(s);
    let s = s.replace('\\', "/");
    let s = s.trim_start_matches('/');
    let (host_port, rest) = s.split_once('/')?;
    let mut segs = rest.split('/').filter(|p| !p.is_empty());
    let share = segs.next()?.to_string();
    let subpath = segs.collect::<Vec<_>>().join("/");
    let (host, port) = match host_port.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => {
            (h.to_string(), p.parse::<u16>().ok())
        }
        _ => (host_port.to_string(), None),
    };
    if host.is_empty() {
        return None;
    }
    Some(Location {
        host,
        port,
        share,
        subpath: normalize_music_path(&subpath),
    })
}

impl SmbCreds {
    pub fn new(loc: &Location, user: String, pass: String, music_path: &str) -> Self {
        Self {
            host: loc.host.clone(),
            port: loc.port,
            share: loc.share.clone(),
            user,
            pass,
            music_path: normalize_music_path(music_path),
            conn: Arc::new(Mutex::new(None)),
        }
    }

    /// From an `smb` source row. `None` if required fields are missing.
    pub fn from_source(s: &Source) -> Option<Self> {
        let loc = parse_location(s.base_url.as_deref()?)?;
        let pass = crate::core::secrets::resolve_source_password(s.id, s.password.as_deref()?)?;
        let user = crate::core::secrets::resolve_source_username(s.id, s.username.as_deref()?)?;
        Some(Self::new(
            &loc,
            user,
            pass,
            s.music_path.as_deref().unwrap_or(""),
        ))
    }

    /// `smb://host[:port]/share`.
    pub fn location(&self) -> String {
        Location {
            host: self.host.clone(),
            port: self.port,
            share: self.share.clone(),
            subpath: String::new(),
        }
        .to_url()
    }

    /// The open session, connecting on first use.
    fn conn(&self) -> Result<Arc<Conn>> {
        let mut guard = self
            .conn
            .lock()
            .map_err(|_| anyhow!("SMB session lock poisoned"))?;
        if let Some(c) = guard.as_ref() {
            return Ok(c.clone());
        }
        let c = Arc::new(connect(self)?);
        *guard = Some(c.clone());
        Ok(c)
    }

    /// Forgets the session (after a failure) so the next call reconnects.
    fn drop_conn(&self) {
        if let Ok(mut guard) = self.conn.lock() {
            *guard = None;
        }
    }

    /// Runs `f` on the session; a failure drops the session and retries once
    /// on a fresh one (covers a NAS that timed the idle session out).
    fn with_conn<T>(&self, f: impl Fn(&Conn) -> Result<T>) -> Result<T> {
        let conn = self.conn()?;
        match f(&conn) {
            Ok(v) => Ok(v),
            Err(first) => {
                self.drop_conn();
                let conn = self
                    .conn()
                    .map_err(|e| anyhow!("{first} (reconnect: {e})"))?;
                f(&conn).map_err(|e| anyhow!("{e}"))
            }
        }
    }

    /// UNC path of a music-root-relative path (`/Alben/X` →
    /// `\\host\share\Music\Alben\X`).
    fn unc(&self, root: &UncPath, rel: &str) -> UncPath {
        let full = format!("{}{}", self.music_path, rel);
        let p = full.trim_matches('/').replace('/', "\\");
        if p.is_empty() {
            root.clone()
        } else {
            root.clone().with_path(&p)
        }
    }
}

/// Opens the TCP session and connects the share.
fn connect(c: &SmbCreds) -> Result<Conn> {
    let mut cfg = ClientConfig::default();
    cfg.connection.timeout = Some(TIMEOUT);
    cfg.connection.port = c.port;
    let client = Client::new(cfg);
    let root = UncPath::new(&c.host)
        .and_then(|u| u.with_share(&c.share))
        .map_err(|e| anyhow!("invalid share location: {e}"))?;
    client
        .share_connect(&root, &c.user, c.pass.clone())
        .map_err(|e| anyhow!("SMB connect to {} failed: {e}", c.location()))?;
    Ok(Conn { client, root })
}

fn open_args() -> FileCreateArgs {
    FileCreateArgs::make_open_existing(FileAccessMask::new().with_generic_read(true))
}

/// Lists a folder (relative to the music root). Returns only subfolders and
/// audio files; `.`/`..` and hidden dot-entries are skipped.
pub fn list(c: &SmbCreds, rel: &str) -> Result<Vec<RemoteEntry>> {
    c.with_conn(|conn| {
        let path = c.unc(&conn.root, rel);
        let res = conn
            .client
            .create_file(&path, &open_args())
            .map_err(|e| anyhow!("open folder failed: {e}"))?;
        let Resource::Directory(dir) = &res else {
            return Err(anyhow!("not a folder: {rel}"));
        };
        let mut out = Vec::new();
        let iter = dir
            .query::<FileDirectoryInformation>("*")
            .map_err(|e| anyhow!("listing failed: {e}"))?;
        for entry in iter {
            let entry = entry.map_err(|e| anyhow!("listing failed: {e}"))?;
            let name = entry.file_name.to_string();
            if name == "." || name == ".." || name.starts_with('.') {
                continue;
            }
            let is_dir = entry.file_attributes.directory();
            if !is_dir && !scanner::is_audio(Path::new(&name)) {
                continue; // hide non-audio files
            }
            out.push(RemoteEntry {
                rel_path: format!("{rel}/{name}"),
                name,
                is_dir,
            });
        }
        let _ = dir.close();
        Ok(out)
    })
}

/// Connection test: connect the share and open the music root folder.
pub fn test_connection(c: &SmbCreds) -> Result<()> {
    c.with_conn(|conn| {
        let path = c.unc(&conn.root, "");
        let res = conn
            .client
            .create_file(&path, &open_args())
            .map_err(|e| anyhow!("open music folder failed: {e}"))?;
        match &res {
            Resource::Directory(d) => {
                let _ = d.close();
                Ok(())
            }
            _ => Err(anyhow!("the music path is not a folder")),
        }
    })
}

/// Opens a file for reading and returns it with its length.
fn open_file(c: &SmbCreds, conn: &Conn, rel: &str) -> Result<(smb::File, u64)> {
    let path = c.unc(&conn.root, rel);
    let res = conn
        .client
        .create_file(&path, &open_args())
        .map_err(|e| anyhow!("open file failed: {e}"))?;
    match res {
        Resource::File(f) => {
            let len = f.get_len().map_err(|e| anyhow!("file length: {e}"))?;
            Ok((f, len))
        }
        _ => Err(anyhow!("not a file: {rel}")),
    }
}

/// Reads up to `len` bytes from the start of a file.
pub fn fetch_prefix(c: &SmbCreds, rel: &str, len: u64) -> Result<Vec<u8>> {
    c.with_conn(|conn| {
        let (file, total) = open_file(c, conn, rel)?;
        let want = len.min(total) as usize;
        let mut buf = vec![0u8; want];
        let mut pos = 0usize;
        while pos < want {
            let chunk = (want - pos).min(READ_CHUNK);
            let n = file
                .read_at(&mut buf[pos..pos + chunk], pos as u64)
                .map_err(|e| anyhow!("read failed: {e}"))?;
            if n == 0 {
                break;
            }
            pos += n;
        }
        let _ = file.close();
        buf.truncate(pos);
        Ok(buf)
    })
}

/// Opens a byte range of a file for streaming.
pub fn open_range(c: &SmbCreds, rel: &str, start: u64, end: Option<u64>) -> Result<RangeBody> {
    let conn = c.conn()?;
    let (file, total) = match open_file(c, &conn, rel) {
        Ok(v) => v,
        Err(_) => {
            // Stale session → one reconnect, like `with_conn`.
            c.drop_conn();
            let conn = c.conn()?;
            open_file(c, &conn, rel)?
        }
    };
    let Some((start, end)) = clamp_range(total, start, end) else {
        let _ = file.close();
        return Err(anyhow!("range not satisfiable"));
    };
    Ok(RangeBody {
        total,
        start,
        end,
        body: Box::new(SmbReader {
            _conn: conn,
            file: Some(file),
            pos: start,
            end,
        }),
    })
}

/// Sequential reader over an open SMB file, `pos..=end`. Keeps the session
/// alive for as long as it is read; closes the handle on drop.
struct SmbReader {
    _conn: Arc<Conn>,
    file: Option<smb::File>,
    pos: u64,
    end: u64,
}

impl Read for SmbReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let Some(file) = self.file.as_ref() else {
            return Ok(0);
        };
        if self.pos > self.end || buf.is_empty() {
            return Ok(0);
        }
        let remaining = self.end - self.pos + 1;
        let want = (buf.len() as u64).min(remaining).min(READ_CHUNK as u64) as usize;
        let n = file
            .read_at(&mut buf[..want], self.pos)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Drop for SmbReader {
    fn drop(&mut self) {
        if let Some(f) = self.file.take() {
            let _ = f.close();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_share_locations() {
        let l = parse_location("smb://nas.local/music").unwrap();
        assert_eq!(
            l,
            Location {
                host: "nas.local".into(),
                port: None,
                share: "music".into(),
                subpath: String::new()
            }
        );
        assert_eq!(l.to_url(), "smb://nas.local/music");

        let l = parse_location(r"\\192.168.0.5\Media\Musik\Alben").unwrap();
        assert_eq!(l.host, "192.168.0.5");
        assert_eq!(l.share, "Media");
        assert_eq!(l.subpath, "/Musik/Alben");

        let l = parse_location("//nas:4455/share/").unwrap();
        assert_eq!(l.port, Some(4455));
        assert_eq!(l.to_url(), "smb://nas:4455/share");

        let l = parse_location("nas/share").unwrap();
        assert_eq!((l.host.as_str(), l.share.as_str()), ("nas", "share"));

        assert!(parse_location("nas").is_none());
        assert!(parse_location("smb://").is_none());
        assert!(parse_location("smb:///share").is_none());
    }

    #[test]
    fn builds_unc_paths_under_the_music_root() {
        let loc = parse_location("smb://nas/music").unwrap();
        let c = SmbCreds::new(&loc, "u".into(), "p".into(), "/My Music/");
        let root = UncPath::new("nas").unwrap().with_share("music").unwrap();
        assert_eq!(
            c.unc(&root, "/Alben/X/song.mp3").to_string(),
            r"\\nas\music\My Music\Alben\X\song.mp3"
        );
        assert_eq!(c.unc(&root, "").to_string(), r"\\nas\music\My Music");
        let c = SmbCreds::new(&loc, "u".into(), "p".into(), "");
        assert_eq!(c.unc(&root, "").to_string(), r"\\nas\music");
    }
}
