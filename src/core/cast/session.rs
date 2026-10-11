//! One cast session: a worker thread that owns all network traffic to the
//! renderer, and a main-thread handle that sends it commands and reads the
//! state it publishes.
//!
//! The worker polls the renderer about once a second (transport state,
//! position, current URI) and turns what it sees into [`Event`]s: the item
//! started (`Ready`), it ran out (`Ended`), it could not be played (`Error`),
//! or someone else took the renderer over (`Lost`). The handle never blocks
//! — a renderer that hangs costs the worker time, not the UI.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};

use super::serve::{self, Enc, Item};
use super::upnp::{self, Meta};
use super::{Device, Kind};

const POLL_PLAYING: Duration = Duration::from_millis(1000);
const POLL_IDLE: Duration = Duration::from_secs(4);
/// A loaded item that has not started after this long is reported broken.
const START_TIMEOUT: Duration = Duration::from_secs(25);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Playing,
    Paused,
    Stopped,
    Transitioning,
    NoMedia,
    Unknown,
}

impl Transport {
    fn parse(s: &str) -> Self {
        match s {
            "PLAYING" => Transport::Playing,
            "PAUSED_PLAYBACK" | "PAUSED_RECORDING" => Transport::Paused,
            "STOPPED" => Transport::Stopped,
            "TRANSITIONING" => Transport::Transitioning,
            "NO_MEDIA_PRESENT" => Transport::NoMedia,
            _ => Transport::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// The loaded item started playing on the renderer.
    Ready,
    /// The item played to its end.
    Ended,
    /// The item could not be played.
    Error(String),
    /// Another controller took the renderer over (or it went away).
    Lost,
}

enum Cmd {
    Load {
        uri: String,
        resume_ms: i64,
        meta: Meta,
        play: bool,
    },
    Play,
    Pause,
    Stop,
    Seek(i64),
    Volume(u32),
    Quit,
}

/// State the worker publishes for the main thread.
#[derive(Debug, Default)]
struct Shared {
    state: Option<Transport>,
    /// Position as last read, with the instant of the read (interpolated
    /// while playing so the seek bar runs smoothly between polls).
    pos_ms: i64,
    pos_at: Option<Instant>,
    dur_ms: Option<i64>,
    volume: Option<u32>,
    events: Vec<Event>,
    /// The worker has given up on the renderer (unreachable for good).
    dead: bool,
}

pub struct Session {
    device: Device,
    tx: Sender<Cmd>,
    shared: Arc<Mutex<Shared>>,
}

impl Session {
    pub fn start(device: Device) -> Self {
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Mutex::new(Shared::default()));
        let worker = Worker {
            agent: upnp::agent(),
            dev: device.clone(),
            shared: shared.clone(),
            mimes: None,
            item: None,
            failures: 0,
            settle: 0,
        };
        let _ = std::thread::Builder::new()
            .name("cast-session".into())
            .spawn(move || worker.run(rx));
        Self { device, tx, shared }
    }

    pub fn device(&self) -> &Device {
        &self.device
    }

    /// Loads `uri` (as given to the player: `file://` or a network URL) and,
    /// with `play`, starts it at `resume_ms`.
    pub fn load(&self, uri: &str, resume_ms: i64, meta: Meta, play: bool) {
        if let Ok(mut s) = self.shared.lock() {
            s.pos_ms = resume_ms.max(0);
            s.pos_at = None;
            s.dur_ms = meta.duration_ms;
            s.state = Some(Transport::Transitioning);
        }
        let _ = self.tx.send(Cmd::Load {
            uri: uri.to_string(),
            resume_ms,
            meta,
            play,
        });
    }

    pub fn play(&self) {
        let _ = self.tx.send(Cmd::Play);
    }

    pub fn pause(&self) {
        if let Ok(mut s) = self.shared.lock() {
            // Freeze the interpolated position right away.
            s.pos_ms = interpolated(&s);
            s.pos_at = None;
            s.state = Some(Transport::Paused);
        }
        let _ = self.tx.send(Cmd::Pause);
    }

    pub fn stop(&self) {
        let _ = self.tx.send(Cmd::Stop);
    }

    pub fn seek(&self, ms: i64) {
        if let Ok(mut s) = self.shared.lock() {
            s.pos_ms = ms.max(0);
            if s.pos_at.is_some() {
                s.pos_at = Some(Instant::now());
            }
        }
        let _ = self.tx.send(Cmd::Seek(ms.max(0)));
    }

    /// Renderer volume 0.0–1.0.
    pub fn set_volume(&self, vol: f64) {
        let v = (vol.clamp(0.0, 1.0) * 100.0).round() as u32;
        if let Ok(mut s) = self.shared.lock() {
            s.volume = Some(v);
        }
        let _ = self.tx.send(Cmd::Volume(v));
    }

    pub fn volume(&self) -> Option<f64> {
        self.shared
            .lock()
            .ok()?
            .volume
            .map(|v| f64::from(v) / 100.0)
    }

    pub fn position_ms(&self) -> i64 {
        self.shared.lock().map(|s| interpolated(&s)).unwrap_or(0)
    }

    pub fn duration_ms(&self) -> Option<i64> {
        self.shared.lock().ok()?.dur_ms
    }

    pub fn state(&self) -> Option<Transport> {
        self.shared.lock().ok()?.state
    }

    pub fn is_dead(&self) -> bool {
        self.shared.lock().map(|s| s.dead).unwrap_or(true)
    }

    pub fn take_events(&self) -> Vec<Event> {
        self.shared
            .lock()
            .map(|mut s| std::mem::take(&mut s.events))
            .unwrap_or_default()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Quit);
    }
}

fn interpolated(s: &Shared) -> i64 {
    let mut pos = s.pos_ms;
    if s.state == Some(Transport::Playing)
        && let Some(at) = s.pos_at
    {
        pos += at.elapsed().as_millis() as i64;
    }
    if let Some(d) = s.dur_ms {
        pos = pos.min(d);
    }
    pos
}

/// The item currently on the renderer, as the worker tracks it.
struct Loaded {
    /// The player-side URI (to re-publish on a transcode seek).
    uri: String,
    meta: Meta,
    /// Token of the published item: the renderer's `TrackURI` contains it as
    /// long as nobody else took over.
    token: String,
    /// Set when we serve a transcode: positions the renderer reports are
    /// relative to this offset, and seeking means re-publishing.
    transcode: Option<(Enc, i64)>,
    live: bool,
    loaded_at: Instant,
    seen_playing: bool,
    /// Seek to do once the renderer plays (most cannot seek before that).
    pending_seek: Option<i64>,
    /// We asked for playback (not paused / stopped by us).
    want_play: bool,
}

struct Worker {
    agent: ureq::Agent,
    dev: Device,
    shared: Arc<Mutex<Shared>>,
    /// The renderer's accepted MIME types (`None`: it did not say).
    mimes: Option<Vec<String>>,
    item: Option<Loaded>,
    /// Consecutive failed polls; the renderer counts as gone after a few.
    failures: u32,
    /// Polls to skip position readings for after a seek: renderers report the
    /// old position for a moment, which would make the seek bar jump back.
    settle: u8,
}

impl Worker {
    fn run(mut self, rx: Receiver<Cmd>) {
        self.mimes = upnp::sink_mimes(&self.agent, &self.dev);
        if let Ok(v) = upnp::get_volume(&self.agent, &self.dev)
            && let Ok(mut s) = self.shared.lock()
        {
            s.volume = Some(v);
        }
        loop {
            let wait = if self.item.as_ref().is_some_and(|i| i.want_play) {
                POLL_PLAYING
            } else {
                POLL_IDLE
            };
            match rx.recv_timeout(wait) {
                Ok(Cmd::Quit) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(cmd) => self.command(cmd),
                Err(RecvTimeoutError::Timeout) => {}
            }
            self.poll();
        }
    }

    fn emit(&self, ev: Event) {
        if let Ok(mut s) = self.shared.lock() {
            s.events.push(ev);
        }
    }

    fn command(&mut self, cmd: Cmd) {
        let res = match cmd {
            Cmd::Load {
                uri,
                resume_ms,
                meta,
                play,
            } => match self.load(&uri, resume_ms, meta, play) {
                Ok(()) => Ok(()),
                Err(e) => {
                    self.item = None;
                    tracing::warn!("cast load failed: {e:#}");
                    self.emit(Event::Error(e.to_string()));
                    return;
                }
            },
            Cmd::Play => self.play(),
            Cmd::Pause => {
                if let Some(i) = &mut self.item {
                    i.want_play = false;
                }
                upnp::pause(&self.agent, &self.dev.avt)
            }
            Cmd::Stop => {
                self.item = None;
                upnp::stop(&self.agent, &self.dev.avt)
            }
            Cmd::Seek(ms) => self.seek(ms),
            Cmd::Volume(v) => upnp::set_volume(&self.agent, &self.dev, v),
            Cmd::Quit => Ok(()),
        };
        if let Err(e) = res {
            tracing::warn!("cast command failed: {e:#}");
        }
    }

    fn supports(&self, mime: &str) -> bool {
        let Some(list) = &self.mimes else {
            // Unknown: assume what practically every renderer plays.
            return matches!(mime, "audio/mpeg" | "audio/mp4" | "audio/flac");
        };
        let alias: &[&str] = match mime {
            "audio/flac" => &["audio/flac", "audio/x-flac"],
            "audio/mp4" => &["audio/mp4", "audio/m4a", "audio/x-m4a", "audio/aac"],
            "audio/aac" | "audio/aacp" => &["audio/aac", "audio/x-aac", "audio/aacp", "audio/mp4"],
            "audio/ogg" => &["audio/ogg", "application/ogg", "audio/x-ogg"],
            "audio/wav" => &["audio/wav", "audio/x-wav", "audio/wave"],
            other => return list.iter().any(|m| m == other || m == "*"),
        };
        list.iter().any(|m| alias.contains(&m.as_str()) || m == "*")
    }

    /// The transcode target, if this renderer takes it.
    fn transcode_target(&self) -> Option<Enc> {
        Some(Enc::Mp3).filter(|e| e.available() && self.supports(e.mime()))
    }

    /// Publishes the player-side URI on the LAN server. Returns (renderer URL,
    /// token, MIME, live, transcode).
    fn publish(
        &self,
        uri: &str,
        start_ms: i64,
    ) -> Result<(String, String, String, bool, Option<(Enc, i64)>)> {
        let host = &self.dev.host;
        if let Some(path) = uri
            .strip_prefix("file://")
            .and_then(|_| gstreamer::glib::filename_from_uri(uri).ok())
            .map(|(p, _)| p)
        {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let mime = serve::mime_for(&name).to_string();
            if self.supports(&mime) {
                let (url, token) = serve::publish(Item::File(path), &name, host)?;
                return Ok((url, token, mime, false, None));
            }
            let enc = self
                .transcode_target()
                .ok_or_else(|| anyhow!("{mime} is not supported by the renderer"))?;
            let (url, token) = serve::publish(
                Item::Transcode {
                    uri: uri.to_string(),
                    start_ms,
                    enc,
                },
                &name,
                host,
            )?;
            return Ok((url, token, enc.mime().into(), false, Some((enc, start_ms))));
        }
        // A network URL: look at what the origin serves first.
        let probe = probe(uri);
        let name = uri
            .split(['?', '#'])
            .next()
            .and_then(|p| p.rsplit('/').next())
            .filter(|n| !n.is_empty())
            .unwrap_or("stream")
            .to_string();
        let mime = probe
            .mime
            .clone()
            .filter(|m| m.starts_with("audio/") || m == "application/ogg")
            .unwrap_or_else(|| serve::mime_for(&name).to_string());
        let mime = if mime == "application/octet-stream" {
            "audio/mpeg".to_string()
        } else {
            mime
        };
        if self.supports(&mime) || self.mimes.is_none() && mime.starts_with("audio/") {
            let (url, token) = serve::publish(Item::Relay(uri.to_string()), &name, host)?;
            return Ok((url, token, mime, probe.live, None));
        }
        let enc = self
            .transcode_target()
            .ok_or_else(|| anyhow!("{mime} is not supported by the renderer"))?;
        let start = if probe.live { 0 } else { start_ms };
        let (url, token) = serve::publish(
            Item::Transcode {
                uri: uri.to_string(),
                start_ms: start,
                enc,
            },
            &name,
            host,
        )?;
        Ok((
            url,
            token,
            enc.mime().into(),
            probe.live,
            Some((enc, start)),
        ))
    }

    fn load(&mut self, uri: &str, resume_ms: i64, mut meta: Meta, play: bool) -> Result<()> {
        // Forget the old item first: the STOPPED this causes is not its end.
        self.item = None;
        let (url, token, mime, live, transcode) = self.publish(uri, resume_ms)?;
        if live {
            meta.duration_ms = None;
        }
        // Sonos plays an endless MP3 stream only under its radio scheme.
        let target = if live && self.dev.kind == Kind::Sonos && mime == "audio/mpeg" {
            url.replacen("http://", "x-rincon-mp3radio://", 1)
        } else {
            url.clone()
        };
        let art_url = meta
            .art_path
            .as_deref()
            .map(std::path::PathBuf::from)
            .filter(|p| p.is_file())
            .and_then(|p| {
                let name = p.file_name()?.to_string_lossy().into_owned();
                serve::publish(Item::File(p), &name, &self.dev.host).ok()
            })
            .map(|(u, _)| u);
        let didl = upnp::didl(&url, &mime, &meta, art_url.as_deref(), live);
        let _ = upnp::stop(&self.agent, &self.dev.avt);
        upnp::set_uri(&self.agent, &self.dev.avt, &target, &didl)?;
        if play {
            upnp::play(&self.agent, &self.dev.avt)?;
        }
        if let Ok(mut s) = self.shared.lock() {
            s.dur_ms = meta.duration_ms;
            s.pos_ms = resume_ms.max(0);
            s.pos_at = None;
        }
        self.item = Some(Loaded {
            uri: uri.to_string(),
            meta,
            token,
            pending_seek: (resume_ms > 0 && transcode.is_none() && !live).then_some(resume_ms),
            transcode,
            live,
            loaded_at: Instant::now(),
            seen_playing: false,
            want_play: play,
        });
        Ok(())
    }

    fn play(&mut self) -> Result<()> {
        let Some(i) = &mut self.item else {
            return Err(anyhow!("nothing loaded"));
        };
        i.want_play = true;
        if !i.seen_playing {
            // Loaded paused, never started: the start timeout counts from now.
            i.loaded_at = Instant::now();
        }
        upnp::play(&self.agent, &self.dev.avt)
    }

    fn seek(&mut self, ms: i64) -> Result<()> {
        let Some(i) = &self.item else {
            return Ok(());
        };
        if i.live {
            return Ok(());
        }
        if i.transcode.is_some() {
            // A transcode stream cannot seek: publish it again from `ms`.
            let (uri, meta, play) = (i.uri.clone(), i.meta.clone(), i.want_play);
            return self.load(&uri, ms, meta, play);
        }
        if !i.seen_playing {
            if let Some(i) = &mut self.item {
                i.pending_seek = Some(ms);
            }
            return Ok(());
        }
        self.settle = 2;
        upnp::seek(&self.agent, &self.dev.avt, ms)
    }

    fn poll(&mut self) {
        let state = match upnp::transport_info(&self.agent, &self.dev.avt) {
            Ok(s) => Transport::parse(&s),
            Err(e) => {
                self.failures += 1;
                tracing::debug!("cast poll failed ({}): {e:#}", self.failures);
                if self.failures == 5 {
                    if let Ok(mut s) = self.shared.lock() {
                        s.dead = true;
                    }
                    self.emit(Event::Lost);
                }
                return;
            }
        };
        self.failures = 0;
        let (pos, dur, track_uri) = if self.item.is_some() {
            upnp::position_info(&self.agent, &self.dev.avt).unwrap_or((None, None, String::new()))
        } else {
            (None, None, String::new())
        };
        let mut events = Vec::new();
        let mut seek_now = None;
        if let Some(i) = &mut self.item {
            let offset = i.transcode.map(|(_, o)| o).unwrap_or(0);
            // Someone else put their own item on the renderer.
            if !track_uri.is_empty()
                && !track_uri.contains(&i.token)
                && matches!(state, Transport::Playing | Transport::Transitioning)
            {
                tracing::info!("cast: renderer taken over ({track_uri})");
                self.item = None;
                events.push(Event::Lost);
            } else {
                match state {
                    Transport::Playing => {
                        if !i.seen_playing {
                            i.seen_playing = true;
                            events.push(Event::Ready);
                            seek_now = i.pending_seek.take();
                        }
                    }
                    Transport::Stopped | Transport::NoMedia if i.seen_playing && i.want_play => {
                        self.item = None;
                        events.push(Event::Ended);
                    }
                    _ if !i.seen_playing
                        && i.want_play
                        && i.loaded_at.elapsed() > START_TIMEOUT =>
                    {
                        self.item = None;
                        events.push(Event::Error("renderer did not start".into()));
                    }
                    _ => {}
                }
            }
            if let Ok(mut s) = self.shared.lock() {
                s.state = Some(state);
                if seek_now.is_none()
                    && self.settle == 0
                    && let Some(p) = pos
                {
                    s.pos_ms = p + offset;
                    s.pos_at = Some(Instant::now());
                }
                if let Some(d) = dur.filter(|_| offset == 0) {
                    s.dur_ms = Some(d);
                }
            }
        } else if let Ok(mut s) = self.shared.lock() {
            s.state = Some(state);
        }
        self.settle = self.settle.saturating_sub(1);
        if seek_now.is_some() {
            self.settle = 2;
        }
        if let Some(ms) = seek_now
            && let Err(e) = upnp::seek(&self.agent, &self.dev.avt, ms)
        {
            tracing::warn!("cast resume seek failed: {e:#}");
        }
        for ev in events {
            self.emit(ev);
        }
    }
}

struct Probe {
    mime: Option<String>,
    /// No length and no ranges: an endless stream (radio).
    live: bool,
}

/// One short GET to learn the origin's content type and whether it is a
/// finite file. The body is not read.
fn probe(url: &str) -> Probe {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .timeout_read(Duration::from_secs(8))
        .build();
    match agent
        .get(url)
        .set("User-Agent", crate::core::net::BROWSER_UA)
        .set("Range", "bytes=0-")
        .call()
    {
        Ok(r) => {
            let mime = r.header("Content-Type").map(|v| {
                v.split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_ascii_lowercase()
            });
            let finite = r.status() == 206 || r.header("Content-Length").is_some();
            Probe {
                mime,
                live: !finite,
            }
        }
        Err(_) => Probe {
            mime: None,
            live: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_states_parse() {
        assert_eq!(Transport::parse("PLAYING"), Transport::Playing);
        assert_eq!(Transport::parse("PAUSED_PLAYBACK"), Transport::Paused);
        assert_eq!(Transport::parse("NO_MEDIA_PRESENT"), Transport::NoMedia);
        assert_eq!(Transport::parse("weird"), Transport::Unknown);
    }

    #[test]
    fn interpolation_runs_only_while_playing_and_stops_at_the_end() {
        let mut s = Shared {
            state: Some(Transport::Paused),
            pos_ms: 1000,
            pos_at: Some(Instant::now() - Duration::from_secs(2)),
            dur_ms: Some(2500),
            ..Default::default()
        };
        assert_eq!(interpolated(&s), 1000);
        s.state = Some(Transport::Playing);
        assert_eq!(interpolated(&s), 2500);
    }
}
