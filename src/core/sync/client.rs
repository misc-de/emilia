//! HTTPS-Client für die Geräte-Synchronisierung.
//!
//! Nutzt den vorhandenen `ureq`-Stack mit einer rustls-Konfiguration, die
//! ausschließlich den aus dem QR-Code gepinnten Zertifikat-Fingerprint
//! akzeptiert (siehe [`crypto::pinned_client_config`]).

use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, Result};

use crate::core::sync::protocol::{self, LibraryExport, PairRequest, PairResponse, PairingInfo};
use crate::core::sync::{crypto, ImportStats};

/// Gekoppelter Client gegen einen [`super::server::SyncServer`].
pub struct SyncClient {
    agent: ureq::Agent,
    base: String,
    device_id: String,
    device_name: String,
    session_token: Option<String>,
    /// Anzeigename der Gegenstelle (nach erfolgreicher Kopplung gesetzt).
    pub peer_name: String,
}

impl SyncClient {
    /// Baut den Client samt fingerprint-gepinnter TLS-Konfiguration.
    pub fn new(info: &PairingInfo, device_id: String, device_name: String) -> Self {
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
            session_token: None,
            peer_name: String::new(),
        }
    }

    /// Kopplungs-Handshake. Bei Erfolg wird das Sitzungstoken gespeichert.
    pub fn pair(&mut self, token: &str) -> Result<()> {
        let body = PairRequest {
            token: token.to_string(),
            device_id: self.device_id.clone(),
            device_name: self.device_name.clone(),
        };
        let resp: PairResponse = self
            .agent
            .post(&format!("{}/pair", self.base))
            .send_json(serde_json::to_value(&body)?)
            .map_err(|e| anyhow!("Kopplung fehlgeschlagen: {e}"))?
            .into_json()?;

        if resp.ok {
            self.session_token = Some(resp.session_token);
            self.peer_name = if resp.device_name.is_empty() {
                "Gerät".to_string()
            } else {
                resp.device_name
            };
            Ok(())
        } else {
            Err(anyhow!(if resp.error.is_empty() {
                "Kopplung abgelehnt".to_string()
            } else {
                resp.error
            }))
        }
    }

    fn bearer(&self) -> String {
        format!("Bearer {}", self.session_token.as_deref().unwrap_or(""))
    }

    /// Holt den Bibliotheks-Export der Gegenstelle.
    pub fn fetch_export(&self) -> Result<LibraryExport> {
        let exp: LibraryExport = self
            .agent
            .get(&format!("{}/sync/export", self.base))
            .set("Authorization", &self.bearer())
            .call()
            .map_err(|e| anyhow!("Abruf fehlgeschlagen: {e}"))?
            .into_json()?;
        Ok(exp)
    }

    /// Sendet den lokalen Export an die Gegenstelle und liefert deren Importzähler.
    pub fn push_export(&self, exp: &LibraryExport) -> Result<ImportStats> {
        let stats: ImportStats = self
            .agent
            .post(&format!("{}/sync/import", self.base))
            .set("Authorization", &self.bearer())
            .send_json(serde_json::to_value(exp)?)
            .map_err(|e| anyhow!("Senden fehlgeschlagen: {e}"))?
            .into_json()?;
        Ok(stats)
    }

    /// Lädt eine Datei (relativer Pfad) herunter und speichert sie unter `dest`.
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
            .map_err(|e| anyhow!("Download fehlgeschlagen: {e}"))?;

        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut reader = resp.into_reader();
        let mut file = std::fs::File::create(dest)?;
        let n = std::io::copy(&mut reader, &mut file)?;
        Ok(n)
    }

    /// Trennt die Verbindung sauber (Fehler werden ignoriert).
    pub fn disconnect(&self) {
        let _ = self
            .agent
            .post(&format!("{}/disconnect", self.base))
            .set("Authorization", &self.bearer())
            .call();
    }
}
