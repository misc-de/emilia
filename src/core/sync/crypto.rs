//! TLS-Identität, Zertifikat-Fingerprint-Pinning und Token-Erzeugung.
//!
//! Sicherheitsmodell (wie in DrivePulse): selbstsigniertes EC-Zertifikat pro
//! Sitzung, dessen SubjectPublicKeyInfo-Fingerprint im QR-Code steht. Der Client
//! verifiziert ausschließlich diesen Fingerprint (Pinning) statt einer Zertifikat-
//! kette – das schützt im LAN gegen Man-in-the-Middle ohne PKI.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use sha2::{Digest, Sha256};

/// Frisch erzeugte TLS-Identität des Servers (nur für die Dauer einer Sitzung).
pub struct ServerIdentity {
    pub cert_pem: String,
    pub key_pem: String,
    /// `base64url(SHA256(SubjectPublicKeyInfo))` – wandert in den QR-Code.
    pub fingerprint: String,
}

/// Erzeugt ein selbstsigniertes EC-Zertifikat (P-256) samt Schlüssel und
/// berechnet den SPKI-Fingerprint.
pub fn generate_identity() -> Result<ServerIdentity> {
    let key = rcgen::generate_simple_self_signed(vec!["emilia".to_string()])
        .map_err(|e| anyhow!("Zertifikat-Erzeugung fehlgeschlagen: {e}"))?;
    let fingerprint = spki_fingerprint(key.cert.der().as_ref())?;
    Ok(ServerIdentity {
        cert_pem: key.cert.pem(),
        key_pem: key.signing_key.serialize_pem(),
        fingerprint,
    })
}

/// SHA256 über die SubjectPublicKeyInfo eines DER-Zertifikats, base64url ohne
/// Padding. Server- und clientseitig identisch, damit die Fingerprints exakt
/// übereinstimmen.
pub fn spki_fingerprint(cert_der: &[u8]) -> Result<String> {
    use x509_parser::prelude::*;
    let (_, cert) =
        X509Certificate::from_der(cert_der).map_err(|e| anyhow!("Zertifikat nicht lesbar: {e}"))?;
    let spki = cert.public_key().raw;
    Ok(URL_SAFE_NO_PAD.encode(Sha256::digest(spki)))
}

/// Zufälliges base64url-Token mit `n` Byte Entropie.
pub fn generate_token(n: usize) -> String {
    let mut buf = vec![0u8; n];
    getrandom::getrandom(&mut buf).expect("Systemzufall nicht verfügbar");
    URL_SAFE_NO_PAD.encode(buf)
}

/// Zufällige Hex-ID (für die persistente Geräte-ID in den Einstellungen).
pub fn random_hex(n: usize) -> String {
    let mut buf = vec![0u8; n];
    getrandom::getrandom(&mut buf).expect("Systemzufall nicht verfügbar");
    let mut s = String::with_capacity(n * 2);
    for b in buf {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Konstantzeitiger Vergleich (gegen Timing-Angriffe auf Token).
pub fn constant_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// rustls-Client-Konfiguration, die genau den gepinnten Fingerprint akzeptiert
/// (für den `ureq`-Client).
pub fn pinned_client_config(fingerprint: String) -> Arc<rustls::ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(FingerprintVerifier {
        fingerprint,
        provider: provider.clone(),
    });
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("rustls-Protokollversionen")
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    Arc::new(config)
}

/// Verifier, der die Zertifikatkette ignoriert und nur den SPKI-Fingerprint prüft.
#[derive(Debug)]
struct FingerprintVerifier {
    fingerprint: String,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl rustls::client::danger::ServerCertVerifier for FingerprintVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        match spki_fingerprint(end_entity.as_ref()) {
            Ok(fp) if constant_eq(&fp, &self.fingerprint) => {
                Ok(rustls::client::danger::ServerCertVerified::assertion())
            }
            _ => Err(rustls::Error::General(
                "Zertifikat-Fingerprint stimmt nicht überein".into(),
            )),
        }
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
            &self.provider.signature_verification_algorithms,
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
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_stable_and_url_safe() {
        let id = generate_identity().expect("identity");
        // base64url ohne Padding: keine '+', '/', '='.
        assert!(!id.fingerprint.contains(['+', '/', '=']));
        // Deterministisch bezogen auf dasselbe Zertifikat.
        let again = spki_fingerprint(
            rcgen::generate_simple_self_signed(vec!["emilia".to_string()])
                .unwrap()
                .cert
                .der()
                .as_ref(),
        )
        .unwrap();
        assert_ne!(id.fingerprint, again, "verschiedene Zertifikate → verschiedene Fingerprints");
    }

    #[test]
    fn constant_eq_works() {
        assert!(constant_eq("abc", "abc"));
        assert!(!constant_eq("abc", "abd"));
        assert!(!constant_eq("abc", "abcd"));
    }
}
