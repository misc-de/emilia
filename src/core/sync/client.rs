//! HTTPS client for device sync.
//!
//! Uses the existing `ureq` stack with a rustls configuration that
//! accepts only the certificate fingerprint pinned from the QR code
//! (see [`crypto::pinned_client_config`]).

use std::io::Read;
use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, Result};

use crate::core::sync::protocol::{
    self, Capabilities, LibraryExport, PairRequest, PairResponse, PairingInfo,
};
use crate::core::sync::share::{ShareDecision, ShareManifest};
use crate::core::sync::{crypto, ImportStats};

/// Upper bound for a JSON response body from the peer. The peer is pinned and
/// authenticated, but a bug or a compromised peer must not be able to OOM us
/// with an unbounded body — metadata exports are tiny in practice. ureq imposes
/// no limit of its own, so we cap it here.
// Part of the metadata export/import direction (see `fetch_export`/`push_export`):
// the server endpoints exist but the client side is not wired into the UI yet.
#[allow(dead_code)]
const MAX_JSON_BYTES: u64 = 64 * 1024 * 1024;

/// Reads a JSON response body with a hard size cap and deserializes it.
#[allow(dead_code)]
fn read_json_capped<T: serde::de::DeserializeOwned>(resp: ureq::Response) -> Result<T> {
    let mut buf = Vec::new();
    resp.into_reader()
        .take(MAX_JSON_BYTES + 1)
        .read_to_end(&mut buf)?;
    if buf.len() as u64 > MAX_JSON_BYTES {
        return Err(anyhow!(
            "peer response exceeds the {MAX_JSON_BYTES}-byte limit"
        ));
    }
    Ok(serde_json::from_slice(&buf)?)
}

/// Paired client against a [`super::server::SyncServer`].
pub struct SyncClient {
    agent: ureq::Agent,
    base: String,
    device_id: String,
    device_name: String,
    /// Capabilities this device advertises to the peer.
    caps: Capabilities,
    session_token: Option<String>,
    /// Display name of the peer (set after successful pairing).
    pub peer_name: String,
    /// Capabilities the peer advertised (set after successful pairing).
    pub peer_caps: Capabilities,
}

impl SyncClient {
    /// Builds the client together with the fingerprint-pinned TLS configuration.
    pub fn new(
        info: &PairingInfo,
        device_id: String,
        device_name: String,
        caps: Capabilities,
    ) -> Self {
        let config = crypto::pinned_client_config(info.fingerprint.clone());
        let agent = ureq::AgentBuilder::new()
            .tls_config(config)
            .timeout_connect(Duration::from_secs(10))
            .build();
        Self {
            agent,
            base: format!("https://{}:{}", info.host, info.port),
            device_id,
            device_name,
            caps,
            session_token: None,
            peer_name: String::new(),
            peer_caps: Capabilities::default(),
        }
    }

    /// Pairing handshake. On success the session token + peer caps are stored.
    pub fn pair(&mut self, token: &str) -> Result<()> {
        let body = PairRequest {
            token: token.to_string(),
            device_id: self.device_id.clone(),
            device_name: self.device_name.clone(),
            caps: self.caps.clone(),
        };
        let resp: PairResponse = self
            .agent
            .post(&format!("{}/pair", self.base))
            .send_json(serde_json::to_value(&body)?)
            .map_err(|e| anyhow!("pairing failed: {e}"))?
            .into_json()?;

        if resp.ok {
            self.session_token = Some(resp.session_token);
            self.peer_caps = resp.caps;
            self.peer_name = if resp.device_name.is_empty() {
                "Device".to_string()
            } else {
                resp.device_name
            };
            Ok(())
        } else {
            Err(anyhow!(if resp.error.is_empty() {
                "pairing rejected".to_string()
            } else {
                resp.error
            }))
        }
    }

    fn bearer(&self) -> String {
        format!("Bearer {}", self.session_token.as_deref().unwrap_or(""))
    }

    /// Fetches the peer's library export.
    /// Not yet wired into the UI (the server endpoint already exists).
    #[allow(dead_code)]
    pub fn fetch_export(&self) -> Result<LibraryExport> {
        let resp = self
            .agent
            .get(&format!("{}/sync/export", self.base))
            .set("Authorization", &self.bearer())
            .call()
            .map_err(|e| anyhow!("fetch failed: {e}"))?;
        read_json_capped(resp)
    }

    /// Sends the local export to the peer and returns its import counters.
    /// Not yet wired into the UI (the server endpoint already exists).
    #[allow(dead_code)]
    pub fn push_export(&self, exp: &LibraryExport) -> Result<ImportStats> {
        let resp = self
            .agent
            .post(&format!("{}/sync/import", self.base))
            .set("Authorization", &self.bearer())
            .send_json(serde_json::to_value(exp)?)
            .map_err(|e| anyhow!("send failed: {e}"))?;
        read_json_capped(resp)
    }

    /// Downloads a file (relative path) and saves it to `dest`.
    pub fn download_file(&self, rel_path: &str, dest: &Path) -> Result<u64> {
        let url = format!(
            "{}/files/get?path={}",
            self.base,
            protocol::percent_encode(rel_path)
        );
        let resp = self
            .agent
            .get(&url)
            .set("Authorization", &self.bearer())
            .call()
            .map_err(|e| anyhow!("download failed: {e}"))?;

        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = dest.with_extension("part");
        let mut file = std::fs::File::create(&tmp)?;
        // Cap the transfer: a pinned peer is still untrusted enough that a buggy
        // or compromised one must not be able to fill the disk.
        let n = crate::core::net::copy_capped(
            resp.into_reader(),
            &mut file,
            crate::core::net::MAX_DOWNLOAD_BYTES,
        )?;
        file.sync_all().ok();
        if n == 0 {
            let _ = std::fs::remove_file(&tmp);
            return Err(anyhow!("downloaded file is empty"));
        }
        std::fs::rename(&tmp, dest)?;
        Ok(n)
    }

    /// Keep-alive ping (extends the server session while the user reviews).
    pub fn ping(&self) -> Result<()> {
        self.agent
            .get(&format!("{}/ping", self.base))
            .set("Authorization", &self.bearer())
            .call()
            .map_err(|e| anyhow!("ping failed: {e}"))?;
        Ok(())
    }

    // --- Selective share (offer / decision / upload) -----------------------

    /// Polls for an offer parked by the server-side user (server-as-sender).
    /// `Ok(None)` if nothing is on offer yet.
    pub fn fetch_offer(&self) -> Result<Option<ShareManifest>> {
        let resp = self
            .agent
            .get(&format!("{}/share/offer", self.base))
            .set("Authorization", &self.bearer())
            .call()
            .map_err(|e| anyhow!("fetch offer failed: {e}"))?;
        if resp.status() == 204 {
            return Ok(None);
        }
        Ok(Some(resp.into_json()?))
    }

    /// Sends our own offer to the server (client-as-sender).
    pub fn send_offer(&self, manifest: &ShareManifest) -> Result<()> {
        self.agent
            .post(&format!("{}/share/offer", self.base))
            .set("Authorization", &self.bearer())
            .send_json(serde_json::to_value(manifest)?)
            .map_err(|e| anyhow!("send offer failed: {e}"))?;
        Ok(())
    }

    /// Polls for the server-side user's decision on our offer (client-as-sender).
    pub fn fetch_decision(&self) -> Result<Option<ShareDecision>> {
        let resp = self
            .agent
            .get(&format!("{}/share/decision", self.base))
            .set("Authorization", &self.bearer())
            .call()
            .map_err(|e| anyhow!("fetch decision failed: {e}"))?;
        if resp.status() == 204 {
            return Ok(None);
        }
        Ok(Some(resp.into_json()?))
    }

    /// Sends our decision on the server's offer (server-as-sender → we receive).
    pub fn send_decision(&self, decision: &ShareDecision) -> Result<()> {
        self.agent
            .post(&format!("{}/share/decision", self.base))
            .set("Authorization", &self.bearer())
            .send_json(serde_json::to_value(decision)?)
            .map_err(|e| anyhow!("send decision failed: {e}"))?;
        Ok(())
    }

    /// Uploads a local file to the server at `rel_path` (client-as-sender). The
    /// body is streamed with an explicit Content-Length so the server can write
    /// it without buffering. Returns the number of bytes sent.
    pub fn upload_file(&self, rel_path: &str, src: &Path) -> Result<u64> {
        let file = std::fs::File::open(src)?;
        let size = file.metadata()?.len();
        let url = format!(
            "{}/files/put?path={}",
            self.base,
            protocol::percent_encode(rel_path)
        );
        self.agent
            .post(&url)
            .set("Authorization", &self.bearer())
            .set("Content-Length", &size.to_string())
            .set("Content-Type", "application/octet-stream")
            .send(file)
            .map_err(|e| anyhow!("upload failed: {e}"))?;
        Ok(size)
    }

    /// Tells the peer (server-side) that a share finished, so its UI can show the
    /// transfer-success screen too. The server is otherwise passive during a
    /// pull/upload and has no other way to learn the transfer completed.
    pub fn notify_complete(&self, files: usize) -> Result<()> {
        self.agent
            .post(&format!("{}/share/complete", self.base))
            .set("Authorization", &self.bearer())
            .send_json(serde_json::json!({ "files": files }))
            .map_err(|e| anyhow!("notify complete failed: {e}"))?;
        Ok(())
    }

    /// Cleanly drops the connection (errors are ignored).
    pub fn disconnect(&self) {
        let _ = self
            .agent
            .post(&format!("{}/disconnect", self.base))
            .set("Authorization", &self.bearer())
            .call();
    }
}
