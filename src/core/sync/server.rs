//! HTTPS-Server (TLS 1.3, `tiny_http`) für die Geräte-Synchronisierung.
//!
//! Läuft blockierend in einem eigenen Thread. Die Annahmeschleife prüft
//! regelmäßig ein Stop-Flag sowie Pairing-/Sitzungs-Timeouts. Jeder
//! authentifizierte Request verlängert die Sitzung (kein separater Ping nötig).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, Result};
use serde::Serialize;
use tiny_http::{Header, Method, Request, Response, Server, SslConfig};

use crate::core::db::Library;
use crate::core::sync::protocol::{self, PairRequest, PairResponse};
use crate::core::sync::{crypto, data, SyncEvent};
use crate::core::sync::{ACCEPT_POLL, PORT, PORT_ATTEMPTS, QR_TTL, SESSION_TIMEOUT};

/// Laufender Sync-Server mit frischer TLS-Identität und Sitzungstoken.
pub struct SyncServer {
    server: Server,
    identity: crypto::ServerIdentity,
    pairing_token: String,
    session_token: String,
    device_name: String,
    host: String,
    port: u16,
    expires_at: u64,
    stop: Arc<AtomicBool>,
}

enum Action {
    Continue,
    Stop,
}

impl SyncServer {
    /// Erzeugt die TLS-Identität/Token und bindet den HTTPS-Server (mit
    /// Port-Ausweichung). Der Server wartet noch nicht – siehe [`Self::run`].
    pub fn start(device_name: String, stop: Arc<AtomicBool>) -> Result<Self> {
        let identity = crypto::generate_identity()?;
        let cert = identity.cert_pem.clone().into_bytes();
        let key = identity.key_pem.clone().into_bytes();

        let mut bound: Option<(Server, u16)> = None;
        let mut port = PORT;
        for _ in 0..PORT_ATTEMPTS {
            let ssl = SslConfig {
                certificate: cert.clone(),
                private_key: key.clone(),
            };
            match Server::https(("0.0.0.0", port), ssl) {
                Ok(server) => {
                    bound = Some((server, port));
                    break;
                }
                Err(_) => port = port.wrapping_add(1),
            }
        }
        let (server, port) = bound.ok_or_else(|| anyhow!("Kein freier Port für den Server"))?;

        Ok(Self {
            server,
            identity,
            pairing_token: crypto::generate_token(32),
            session_token: crypto::generate_token(32),
            device_name,
            host: super::local_ip(),
            port,
            expires_at: super::now_unix() + QR_TTL.as_secs(),
            stop,
        })
    }

    /// QR-/Pairing-URL für die Anzeige.
    pub fn pair_url(&self) -> String {
        protocol::build_pair_url(
            &self.host,
            self.port,
            &self.identity.fingerprint,
            &self.pairing_token,
            self.expires_at,
        )
    }

    pub fn host(&self) -> &str {
        &self.host
    }
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Blockierende Annahmeschleife. Meldet Ereignisse über `emit`. Kehrt
    /// zurück, wenn das Stop-Flag gesetzt ist, ein Timeout greift oder die
    /// Gegenstelle die Verbindung trennt.
    pub fn run<F: FnMut(SyncEvent)>(self, mut emit: F) {
        let deadline = Instant::now() + QR_TTL; // bis zur Kopplung
        let mut paired = false;
        let mut session_deadline: Option<Instant> = None;
        let mut failed: u32 = 0;
        let mut peer_name = String::new();

        loop {
            if self.stop.load(Ordering::Relaxed) {
                break;
            }
            if !paired && Instant::now() > deadline {
                break; // niemand hat gekoppelt
            }
            if let Some(dl) = session_deadline {
                if Instant::now() > dl {
                    emit(SyncEvent::PeerDisconnected);
                    break;
                }
            }

            match self.server.recv_timeout(ACCEPT_POLL) {
                Ok(Some(req)) => {
                    match self.handle(req, &mut paired, &mut failed, &mut peer_name, &mut emit) {
                        Action::Stop => break,
                        Action::Continue => {
                            if paired {
                                session_deadline = Some(Instant::now() + SESSION_TIMEOUT);
                            }
                        }
                    }
                }
                Ok(None) => continue, // Timeout → Flags erneut prüfen
                Err(_) => break,
            }
        }
    }

    fn handle<F: FnMut(SyncEvent)>(
        &self,
        mut req: Request,
        paired: &mut bool,
        failed: &mut u32,
        peer_name: &mut String,
        emit: &mut F,
    ) -> Action {
        let method = req.method().clone();
        let url = req.url().to_string();
        let path = url.split('?').next().unwrap_or(&url).to_string();
        let query = url.splitn(2, '?').nth(1).unwrap_or("").to_string();

        match (&method, path.as_str()) {
            (Method::Post, "/pair") => {
                let body = read_body(&mut req);
                let parsed: Result<PairRequest, _> = serde_json::from_str(&body);
                match parsed {
                    Ok(pr) if crypto::constant_eq(&pr.token, &self.pairing_token) => {
                        *paired = true;
                        *peer_name = pr.device_name.clone();
                        emit(SyncEvent::PeerPaired {
                            peer_name: pr.device_name,
                        });
                        respond_json(
                            req,
                            200,
                            &PairResponse {
                                ok: true,
                                session_token: self.session_token.clone(),
                                device_name: self.device_name.clone(),
                                error: String::new(),
                            },
                        );
                        Action::Continue
                    }
                    _ => {
                        *failed += 1;
                        respond_json(
                            req,
                            403,
                            &PairResponse {
                                ok: false,
                                error: "ungültiges Token".into(),
                                ..Default::default()
                            },
                        );
                        if *failed >= super::MAX_FAILED_PAIR {
                            emit(SyncEvent::Error("Zu viele Kopplungsversuche".into()));
                            Action::Stop
                        } else {
                            Action::Continue
                        }
                    }
                }
            }

            // Ab hier: Bearer-Authentifizierung erforderlich.
            _ if !self.bearer_ok(&req) => {
                respond_status(req, 401);
                Action::Continue
            }

            (Method::Get, "/ping") => {
                respond_json(req, 200, &serde_json::json!({ "ok": true }));
                Action::Continue
            }

            (Method::Get, "/sync/export") => {
                match Library::open().and_then(|lib| data::export_library(&lib)) {
                    Ok(exp) => respond_json(req, 200, &exp),
                    Err(e) => {
                        respond_json(req, 500, &serde_json::json!({ "error": e.to_string() }))
                    }
                }
                Action::Continue
            }

            (Method::Post, "/sync/import") => {
                let body = read_body(&mut req);
                let result = serde_json::from_str(&body)
                    .map_err(anyhow::Error::from)
                    .and_then(|exp| {
                        let lib = Library::open()?;
                        data::import_library(&lib, &exp)
                    });
                match result {
                    Ok(stats) => {
                        emit(SyncEvent::ImportReceived {
                            stats: stats.clone(),
                        });
                        respond_json(req, 200, &stats);
                    }
                    Err(e) => {
                        respond_json(req, 400, &serde_json::json!({ "error": e.to_string() }))
                    }
                }
                Action::Continue
            }

            (Method::Get, "/files/list") => {
                match Library::open().and_then(|lib| data::export_library(&lib)) {
                    Ok(exp) => respond_json(req, 200, &serde_json::json!({ "files": exp.files })),
                    Err(e) => {
                        respond_json(req, 500, &serde_json::json!({ "error": e.to_string() }))
                    }
                }
                Action::Continue
            }

            (Method::Get, "/files/get") => {
                self.serve_file(req, &query);
                Action::Continue
            }

            (Method::Post, "/disconnect") => {
                respond_json(req, 200, &serde_json::json!({ "ok": true }));
                emit(SyncEvent::PeerDisconnected);
                Action::Stop
            }

            _ => {
                respond_status(req, 404);
                Action::Continue
            }
        }
    }

    fn bearer_ok(&self, req: &Request) -> bool {
        let expected = format!("Bearer {}", self.session_token);
        req.headers().iter().any(|h| {
            h.field.equiv("Authorization") && crypto::constant_eq(h.value.as_str(), &expected)
        })
    }

    /// Liefert eine Audiodatei aus dem Musikordner (mit Path-Traversal-Schutz).
    fn serve_file(&self, req: Request, query: &str) {
        let rel = query
            .split('&')
            .find_map(|p| p.strip_prefix("path="))
            .map(protocol::percent_decode)
            .unwrap_or_default();

        let music_dir = Library::open()
            .ok()
            .and_then(|lib| lib.get_setting("music_dir").ok().flatten())
            .unwrap_or_default();

        match self.resolve_safe(&rel, &music_dir) {
            Some(abs) => match std::fs::File::open(&abs) {
                Ok(file) => {
                    let header = Header::from_bytes(
                        &b"Content-Type"[..],
                        &b"application/octet-stream"[..],
                    )
                    .unwrap();
                    let _ = req.respond(Response::from_file(file).with_header(header));
                }
                Err(_) => respond_status(req, 404),
            },
            None => respond_status(req, 403),
        }
    }

    /// Löst einen relativen Pfad gegen den Musikordner auf und stellt sicher,
    /// dass das Ergebnis innerhalb des Musikordners liegt.
    fn resolve_safe(&self, rel: &str, music_dir: &str) -> Option<PathBuf> {
        if music_dir.is_empty() || rel.is_empty() || rel.starts_with('/') || rel.contains("..") {
            return None;
        }
        let base = std::fs::canonicalize(music_dir).ok()?;
        let abs = std::fs::canonicalize(Path::new(music_dir).join(rel)).ok()?;
        abs.starts_with(&base).then_some(abs)
    }
}

fn read_body(req: &mut Request) -> String {
    let mut body = String::new();
    let _ = req.as_reader().read_to_string(&mut body);
    body
}

fn respond_json<S: Serialize>(req: Request, status: u16, body: &S) {
    let json = serde_json::to_string(body).unwrap_or_else(|_| "{}".to_string());
    let header = Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap();
    let resp = Response::from_string(json)
        .with_status_code(status as i32)
        .with_header(header);
    let _ = req.respond(resp);
}

fn respond_status(req: Request, status: u16) {
    let _ = req.respond(Response::empty(status as i32));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::sync::client::SyncClient;

    /// End-to-End: echter TLS-Handshake + Fingerprint-Pinning + Token-Kopplung.
    #[test]
    fn pairing_handshake_with_pinning() {
        let stop = Arc::new(AtomicBool::new(false));
        let server = SyncServer::start("TestServer".to_string(), stop.clone())
            .expect("Server startet");
        let url = server.pair_url();
        let handle = std::thread::spawn(move || server.run(|_| {}));

        let info = protocol::parse_pair_url(&url, super::super::now_unix()).expect("URL");

        // Korrekter Fingerprint + Token → Kopplung gelingt.
        let mut client = SyncClient::new(&info, "dev-1".into(), "TestClient".into());
        client.pair(&info.token).expect("Kopplung erfolgreich");
        assert_eq!(client.peer_name, "TestServer");

        // Falscher Fingerprint → TLS-Pinning lehnt ab (scheitert vor dem Token).
        let mut bad = info.clone();
        bad.fingerprint = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into();
        let mut bad_client = SyncClient::new(&bad, "dev-2".into(), "Boese".into());
        assert!(bad_client.pair(&bad.token).is_err(), "MITM muss scheitern");

        stop.store(true, Ordering::Relaxed);
        client.disconnect();
        let _ = handle.join();
    }
}
