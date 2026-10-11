//! Google Cast sender (CASTV2): TLS to the device's port 8009, length-framed
//! protobuf `CastMessage`s carrying JSON, and the **Default Media Receiver**
//! app (`CC1AD845`), which plays any URL it is handed — the same LAN server
//! items the DLNA path uses.
//!
//! Flow: CONNECT to `receiver-0` → LAUNCH the app → CONNECT to its transport
//! → LOAD on the media namespace; then PLAY/PAUSE/SEEK/STOP with the media
//! session id, SET_VOLUME on the receiver, and a PING/PONG heartbeat. Cast
//! devices present self-signed certificates (rooted in Google's device CA,
//! which a sender cannot check offline), so the certificate is not verified —
//! like every other Cast sender; the connection only carries control
//! messages for a device on the LAN.

use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

use super::backend::{Backend, LoadItem, Poll};
use super::session::Transport;

const APP_ID: &str = "CC1AD845";
const NS_CONN: &str = "urn:x-cast:com.google.cast.tp.connection";
const NS_HEART: &str = "urn:x-cast:com.google.cast.tp.heartbeat";
const NS_RECV: &str = "urn:x-cast:com.google.cast.receiver";
const NS_MEDIA: &str = "urn:x-cast:com.google.cast.media";
const SENDER: &str = "sender-0";
const RECEIVER: &str = "receiver-0";
const PING_EVERY: Duration = Duration::from_secs(5);
const MAX_FRAME: usize = 64 * 1024;

// --- CastMessage protobuf ------------------------------------------------------

/// A decoded CastMessage (string payloads only — binary ones are not used by
/// the namespaces we speak).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Message {
    pub source: String,
    pub dest: String,
    pub namespace: String,
    pub payload: String,
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

fn put_str(out: &mut Vec<u8>, field: u8, s: &str) {
    out.push((field << 3) | 2);
    put_varint(out, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

/// `CastMessage` → protobuf bytes (without the length prefix).
pub(crate) fn encode(m: &Message) -> Vec<u8> {
    let mut out = Vec::with_capacity(m.payload.len() + 96);
    out.extend_from_slice(&[0x08, 0x00]); // protocol_version = CASTV2_1_0
    put_str(&mut out, 2, &m.source);
    put_str(&mut out, 3, &m.dest);
    put_str(&mut out, 4, &m.namespace);
    out.extend_from_slice(&[0x28, 0x00]); // payload_type = STRING
    put_str(&mut out, 6, &m.payload);
    out
}

fn get_varint(b: &[u8], pos: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *b.get(*pos)?;
        *pos += 1;
        v |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

/// Protobuf bytes → `CastMessage`; unknown fields are skipped.
pub(crate) fn decode(b: &[u8]) -> Option<Message> {
    let mut m = Message {
        source: String::new(),
        dest: String::new(),
        namespace: String::new(),
        payload: String::new(),
    };
    let mut pos = 0;
    while pos < b.len() {
        let key = get_varint(b, &mut pos)?;
        match key & 7 {
            0 => {
                get_varint(b, &mut pos)?;
            }
            2 => {
                let len = get_varint(b, &mut pos)? as usize;
                let data = b.get(pos..pos + len)?;
                pos += len;
                let s = || String::from_utf8_lossy(data).into_owned();
                match key >> 3 {
                    2 => m.source = s(),
                    3 => m.dest = s(),
                    4 => m.namespace = s(),
                    6 => m.payload = s(),
                    _ => {}
                }
            }
            _ => return None,
        }
    }
    Some(m)
}

// --- TLS -----------------------------------------------------------------------

#[derive(Debug)]
struct AcceptAny(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AcceptAny {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

fn tls_config() -> Arc<rustls::ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .expect("rustls protocol versions")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAny(provider)))
        .with_no_client_auth();
    Arc::new(config)
}

// --- Connection + media state ----------------------------------------------------

pub(crate) struct CastBackend {
    tls: rustls::StreamOwned<rustls::ClientConnection, TcpStream>,
    rbuf: Vec<u8>,
    req: u64,
    /// The Default Media Receiver's transport id and session id, once running.
    transport: Option<String>,
    session_id: Option<String>,
    media_session: Option<i64>,
    /// Latest media status entry and the content id it played.
    media: Option<Value>,
    content_id: String,
    volume: Option<f64>,
    last_ping: Instant,
    /// The app went away (another sender took over, or it was stopped).
    gone: bool,
    /// LOAD_FAILED or an idle ERROR came in.
    failed: bool,
}

impl CastBackend {
    pub(crate) fn connect(host: &str, port: u16) -> Result<Self> {
        let ip: IpAddr = host
            .parse()
            .map_err(|_| anyhow!("bad Cast address {host}"))?;
        let tcp = TcpStream::connect_timeout(&SocketAddr::new(ip, port), Duration::from_secs(5))?;
        tcp.set_read_timeout(Some(Duration::from_millis(100)))?;
        tcp.set_write_timeout(Some(Duration::from_secs(5)))?;
        tcp.set_nodelay(true)?;
        let conn = rustls::ClientConnection::new(
            tls_config(),
            rustls::pki_types::ServerName::IpAddress(ip.into()),
        )?;
        let mut me = Self {
            tls: rustls::StreamOwned::new(conn, tcp),
            rbuf: Vec::new(),
            req: 0,
            transport: None,
            session_id: None,
            media_session: None,
            media: None,
            content_id: String::new(),
            volume: None,
            last_ping: Instant::now(),
            gone: false,
            failed: false,
        };
        me.send(NS_CONN, RECEIVER, json!({"type": "CONNECT"}))?;
        let id = me.next_id();
        me.send(
            NS_RECV,
            RECEIVER,
            json!({"type": "GET_STATUS", "requestId": id}),
        )?;
        me.wait(Duration::from_secs(5), |m, _| m.namespace == NS_RECV)?;
        Ok(me)
    }

    fn next_id(&mut self) -> u64 {
        self.req += 1;
        self.req
    }

    fn send(&mut self, ns: &str, dest: &str, payload: Value) -> Result<()> {
        let body = encode(&Message {
            source: SENDER.into(),
            dest: dest.into(),
            namespace: ns.into(),
            payload: payload.to_string(),
        });
        let mut frame = (body.len() as u32).to_be_bytes().to_vec();
        frame.extend_from_slice(&body);
        self.tls.write_all(&frame)?;
        self.tls.flush()?;
        Ok(())
    }

    /// Reads whatever arrived (up to the socket's short timeout) and returns
    /// the complete messages.
    fn read_messages(&mut self) -> Result<Vec<Message>> {
        let mut tmp = [0u8; 8192];
        match self.tls.read(&mut tmp) {
            Ok(0) => bail!("Cast connection closed"),
            Ok(n) => self.rbuf.extend_from_slice(&tmp[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(e) => return Err(e.into()),
        }
        let mut out = Vec::new();
        while self.rbuf.len() >= 4 {
            let len = u32::from_be_bytes([self.rbuf[0], self.rbuf[1], self.rbuf[2], self.rbuf[3]])
                as usize;
            if len > MAX_FRAME {
                bail!("Cast frame too large ({len})");
            }
            if self.rbuf.len() < 4 + len {
                break;
            }
            if let Some(m) = decode(&self.rbuf[4..4 + len]) {
                out.push(m);
            }
            self.rbuf.drain(..4 + len);
        }
        Ok(out)
    }

    /// Updates the state from one incoming message.
    fn handle(&mut self, m: &Message) -> Value {
        let v: Value = serde_json::from_str(&m.payload).unwrap_or(Value::Null);
        let ty = v["type"].as_str().unwrap_or("");
        match (m.namespace.as_str(), ty) {
            (NS_HEART, "PING") => {
                let _ = self.send(NS_HEART, &m.source, json!({"type": "PONG"}));
            }
            (NS_CONN, "CLOSE") if Some(&m.source) == self.transport.as_ref() => {
                self.gone = true;
                self.transport = None;
            }
            (NS_RECV, "RECEIVER_STATUS") => {
                if let Some(level) = v["status"]["volume"]["level"].as_f64() {
                    self.volume = Some(level);
                }
                let ours = v["status"]["applications"]
                    .as_array()
                    .and_then(|apps| apps.iter().find(|a| a["appId"] == APP_ID));
                match ours {
                    Some(app) => {
                        let sid = app["sessionId"].as_str().map(str::to_string);
                        if self.session_id.is_some() && sid != self.session_id {
                            // Someone relaunched it: our media session is gone.
                            self.gone = true;
                        }
                        self.transport = app["transportId"].as_str().map(str::to_string);
                        self.session_id = sid;
                    }
                    None => {
                        if self.transport.is_some() {
                            self.gone = true;
                        }
                        self.transport = None;
                        self.session_id = None;
                    }
                }
            }
            (NS_MEDIA, "MEDIA_STATUS") => match v["status"].as_array().and_then(|s| s.first()) {
                Some(st) => {
                    if let Some(id) = st["mediaSessionId"].as_i64() {
                        self.media_session = Some(id);
                    }
                    if let Some(c) = st["media"]["contentId"].as_str() {
                        self.content_id = c.to_string();
                    }
                    if st["playerState"] == "IDLE" && st["idleReason"] == "ERROR" {
                        self.failed = true;
                    }
                    self.media = Some(st.clone());
                }
                None => self.media = None,
            },
            (NS_MEDIA, "LOAD_FAILED" | "LOAD_CANCELLED" | "INVALID_REQUEST") => self.failed = true,
            _ => {}
        }
        v
    }

    /// Processes messages until `pred` accepts one or `timeout` passes.
    fn wait(
        &mut self,
        timeout: Duration,
        mut pred: impl FnMut(&Message, &Value) -> bool,
    ) -> Result<()> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            for m in self.read_messages()? {
                let v = self.handle(&m);
                if pred(&m, &v) {
                    return Ok(());
                }
            }
        }
        bail!("Cast device did not answer in time")
    }

    /// Makes sure the Default Media Receiver runs and we are connected to it.
    fn ensure_app(&mut self) -> Result<String> {
        if let Some(t) = self.transport.clone().filter(|_| !self.gone) {
            return Ok(t);
        }
        self.gone = false;
        let id = self.next_id();
        self.send(
            NS_RECV,
            RECEIVER,
            json!({"type": "LAUNCH", "appId": APP_ID, "requestId": id}),
        )?;
        let mut launched = false;
        self.wait(Duration::from_secs(15), |m, v| {
            if m.namespace == NS_RECV && v["type"] == "LAUNCH_ERROR" {
                return true;
            }
            launched = m.namespace == NS_RECV
                && v["status"]["applications"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|a| a["appId"] == APP_ID));
            launched
        })?;
        let transport = self
            .transport
            .clone()
            .filter(|_| launched)
            .ok_or_else(|| anyhow!("Default Media Receiver did not start"))?;
        self.send(NS_CONN, &transport, json!({"type": "CONNECT"}))?;
        Ok(transport)
    }

    fn media_cmd(&mut self, ty: &str, extra: Value) -> Result<()> {
        let (Some(t), Some(sid)) = (self.transport.clone(), self.media_session) else {
            return Ok(());
        };
        let id = self.next_id();
        let mut msg = json!({"type": ty, "mediaSessionId": sid, "requestId": id});
        if let (Some(obj), Value::Object(more)) = (msg.as_object_mut(), extra) {
            obj.extend(more);
        }
        self.send(NS_MEDIA, &t, msg)
    }
}

/// `playerState` → transport state.
fn transport_of(media: Option<&Value>) -> Transport {
    match media.and_then(|m| m["playerState"].as_str()) {
        Some("PLAYING") => Transport::Playing,
        Some("PAUSED") => Transport::Paused,
        Some("BUFFERING" | "LOADING") => Transport::Transitioning,
        Some("IDLE") => Transport::Stopped,
        _ => Transport::NoMedia,
    }
}

impl Backend for CastBackend {
    fn accepts(&mut self) -> Option<Vec<String>> {
        // The Default Media Receiver's documented audio formats.
        Some(
            [
                "audio/mpeg",
                "audio/mp4",
                "audio/aac",
                "audio/flac",
                "audio/ogg",
                "audio/webm",
                "audio/wav",
                "audio/opus",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        )
    }

    fn volume(&mut self) -> Result<u32> {
        self.volume
            .map(|v| (v * 100.0).round() as u32)
            .ok_or_else(|| anyhow!("no volume yet"))
    }

    fn set_volume(&mut self, v: u32) -> Result<()> {
        let level = f64::from(v.min(100)) / 100.0;
        self.volume = Some(level);
        let id = self.next_id();
        self.send(
            NS_RECV,
            RECEIVER,
            json!({"type": "SET_VOLUME", "volume": {"level": level}, "requestId": id}),
        )
    }

    fn load(&mut self, item: &LoadItem) -> Result<bool> {
        let transport = self.ensure_app()?;
        let mut metadata = json!({"metadataType": 3, "title": item.meta.title});
        if let Some(a) = &item.meta.artist {
            metadata["artist"] = json!(a);
        }
        if let Some(a) = &item.meta.album {
            metadata["albumName"] = json!(a);
        }
        if let Some(u) = item.art_url {
            metadata["images"] = json!([{"url": u}]);
        }
        let mut media = json!({
            "contentId": item.url,
            "contentUrl": item.url,
            "contentType": item.mime,
            "streamType": if item.live { "LIVE" } else { "BUFFERED" },
            "metadata": metadata,
        });
        if let Some(d) = item.meta.duration_ms.filter(|_| !item.live) {
            media["duration"] = json!(d as f64 / 1000.0);
        }
        let id = self.next_id();
        let session = self.session_id.clone().unwrap_or_default();
        self.failed = false;
        self.media = None;
        self.media_session = None;
        self.send(
            NS_MEDIA,
            &transport,
            json!({
                "type": "LOAD",
                "requestId": id,
                "sessionId": session,
                "media": media,
                "autoplay": item.play,
                "currentTime": item.start_ms.max(0) as f64 / 1000.0,
            }),
        )?;
        let url = item.url.to_string();
        let mut ok = false;
        self.wait(Duration::from_secs(20), |m, v| {
            if m.namespace != NS_MEDIA {
                return false;
            }
            if matches!(
                v["type"].as_str(),
                Some("LOAD_FAILED" | "LOAD_CANCELLED" | "INVALID_REQUEST")
            ) {
                return true;
            }
            ok = v["type"] == "MEDIA_STATUS"
                && v["status"]
                    .as_array()
                    .and_then(|s| s.first())
                    .is_some_and(|s| {
                        s["mediaSessionId"].is_i64()
                            && (s["media"]["contentId"] == url.as_str() || s["media"].is_null())
                    });
            ok
        })?;
        if !ok || self.failed {
            bail!("the Cast device could not load the item");
        }
        self.content_id = url;
        Ok(true)
    }

    fn play(&mut self) -> Result<()> {
        self.media_cmd("PLAY", json!({}))
    }

    fn pause(&mut self) -> Result<()> {
        self.media_cmd("PAUSE", json!({}))
    }

    fn stop(&mut self) -> Result<()> {
        // End the receiver app too, so the TV/speaker goes back to idle.
        let _ = self.media_cmd("STOP", json!({}));
        if let Some(sid) = self.session_id.take() {
            let id = self.next_id();
            self.send(
                NS_RECV,
                RECEIVER,
                json!({"type": "STOP", "sessionId": sid, "requestId": id}),
            )?;
        }
        self.transport = None;
        self.media_session = None;
        self.media = None;
        Ok(())
    }

    fn seek(&mut self, ms: i64) -> Result<()> {
        self.media_cmd("SEEK", json!({"currentTime": ms.max(0) as f64 / 1000.0}))
    }

    fn poll(&mut self, active: bool) -> Result<Poll> {
        if self.last_ping.elapsed() >= PING_EVERY {
            self.send(NS_HEART, RECEIVER, json!({"type": "PING"}))?;
            self.last_ping = Instant::now();
        }
        if active && let (Some(t), Some(sid)) = (self.transport.clone(), self.media_session) {
            let id = self.next_id();
            self.send(
                NS_MEDIA,
                &t,
                json!({"type": "GET_STATUS", "mediaSessionId": sid, "requestId": id}),
            )?;
        }
        // Collect what arrives for a moment (the status answer, heartbeats).
        let until = Instant::now() + Duration::from_millis(300);
        while Instant::now() < until {
            for m in self.read_messages()? {
                self.handle(&m);
            }
        }
        let media = self.media.as_ref();
        let idle_reason = media.and_then(|m| m["idleReason"].as_str()).unwrap_or("");
        Ok(Poll {
            state: transport_of(media),
            pos: media
                .and_then(|m| m["currentTime"].as_f64())
                .map(|s| (s * 1000.0) as i64),
            dur: media
                .and_then(|m| m["media"]["duration"].as_f64())
                .filter(|d| *d > 0.0)
                .map(|s| (s * 1000.0) as i64),
            current: self.content_id.clone(),
            failed: std::mem::take(&mut self.failed),
            // Another sender's item replaced ours, or the app was closed.
            lost: self.gone || matches!(idle_reason, "INTERRUPTED" | "CANCELLED"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protobuf_round_trip() {
        let m = Message {
            source: "sender-0".into(),
            dest: "receiver-0".into(),
            namespace: NS_RECV.into(),
            payload: "{\"type\":\"GET_STATUS\",\"requestId\":1,\"x\":\"".to_string()
                + &"ä".repeat(100)
                + "\"}",
        };
        let b = encode(&m);
        // A long payload needs a multi-byte length varint.
        assert!(m.payload.len() > 127);
        assert_eq!(decode(&b), Some(m));
    }

    #[test]
    fn varints_encode_like_protobuf() {
        let mut v = Vec::new();
        put_varint(&mut v, 300);
        assert_eq!(v, [0xAC, 0x02]);
        let mut pos = 0;
        assert_eq!(get_varint(&v, &mut pos), Some(300));
        assert_eq!(pos, 2);
    }

    #[test]
    fn player_states_map_to_transport() {
        assert_eq!(
            transport_of(Some(&json!({"playerState": "PLAYING"}))),
            Transport::Playing
        );
        assert_eq!(
            transport_of(Some(&json!({"playerState": "BUFFERING"}))),
            Transport::Transitioning
        );
        assert_eq!(
            transport_of(Some(&json!({"playerState": "IDLE"}))),
            Transport::Stopped
        );
        assert_eq!(transport_of(None), Transport::NoMedia);
    }
}
