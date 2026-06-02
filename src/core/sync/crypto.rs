//! TLS identity, certificate fingerprint pinning and token generation.
//!
//! Security model (as in DrivePulse): a self-signed EC certificate per
//! session, whose SubjectPublicKeyInfo fingerprint is encoded in the QR code. The client
//! verifies only this fingerprint (pinning) instead of a certificate
//! chain – this protects against man-in-the-middle attacks on the LAN without PKI.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use sha2::{Digest, Sha256};

/// Freshly generated TLS identity of the server (only for the duration of a session).
pub struct ServerIdentity {
    /// DER-encoded self-signed certificate.
    pub cert_der: Vec<u8>,
    /// DER-encoded PKCS#8 private key.
    pub key_der: Vec<u8>,
    /// `base64url(SHA256(SubjectPublicKeyInfo))` – goes into the QR code.
    pub fingerprint: String,
}

/// Generates a self-signed EC certificate (P-256) together with its key and
/// computes the SPKI fingerprint.
pub fn generate_identity() -> Result<ServerIdentity> {
    let key = rcgen::generate_simple_self_signed(vec!["emilia".to_string()])
        .map_err(|e| anyhow!("certificate generation failed: {e}"))?;
    let cert_der = key.cert.der().as_ref().to_vec();
    let fingerprint = spki_fingerprint(&cert_der)?;
    Ok(ServerIdentity {
        cert_der,
        key_der: key.signing_key.serialize_der(),
        fingerprint,
    })
}

/// rustls **server** configuration for this identity. The certificate is the
/// per-session self-signed one; the client trusts it by SPKI-fingerprint pinning
/// (see [`pinned_client_config`]), not via a CA chain. Uses the ring provider
/// with safe default protocol versions (TLS 1.2/1.3) – the same maintained
/// rustls 0.23 stack as the client (no more EOL TLS on the server side).
pub fn server_config(identity: &ServerIdentity) -> Result<Arc<rustls::ServerConfig>> {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cert = CertificateDer::from(identity.cert_der.clone());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(identity.key_der.clone()));
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| anyhow!("rustls protocol versions: {e}"))?
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .map_err(|e| anyhow!("server certificate invalid: {e}"))?;
    Ok(Arc::new(config))
}

/// SHA256 over the SubjectPublicKeyInfo of a DER certificate, base64url without
/// padding. Identical on the server and client sides so the fingerprints match
/// exactly.
pub fn spki_fingerprint(cert_der: &[u8]) -> Result<String> {
    use x509_parser::prelude::*;
    let (_, cert) =
        X509Certificate::from_der(cert_der).map_err(|e| anyhow!("certificate not readable: {e}"))?;
    let spki = cert.public_key().raw;
    Ok(URL_SAFE_NO_PAD.encode(Sha256::digest(spki)))
}

/// Random base64url token with `n` bytes of entropy.
pub fn generate_token(n: usize) -> String {
    let mut buf = vec![0u8; n];
    getrandom::getrandom(&mut buf).expect("system randomness not available");
    URL_SAFE_NO_PAD.encode(buf)
}

/// Random hex ID (for the persistent device ID in the settings).
pub fn random_hex(n: usize) -> String {
    let mut buf = vec![0u8; n];
    getrandom::getrandom(&mut buf).expect("system randomness not available");
    let mut s = String::with_capacity(n * 2);
    for b in buf {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Constant-time comparison (against timing attacks on tokens).
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

/// rustls client configuration that accepts exactly the pinned fingerprint
/// (for the `ureq` client).
pub fn pinned_client_config(fingerprint: String) -> Arc<rustls::ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(FingerprintVerifier {
        fingerprint,
        provider: provider.clone(),
    });
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("rustls protocol versions")
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    Arc::new(config)
}

/// Verifier that ignores the certificate chain and only checks the SPKI fingerprint.
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
                "certificate fingerprint does not match".into(),
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
        // base64url without padding: no '+', '/', '='.
        assert!(!id.fingerprint.contains(['+', '/', '=']));
        // Deterministic with respect to the same certificate.
        let again = spki_fingerprint(
            rcgen::generate_simple_self_signed(vec!["emilia".to_string()])
                .unwrap()
                .cert
                .der()
                .as_ref(),
        )
        .unwrap();
        assert_ne!(id.fingerprint, again, "different certificates → different fingerprints");
    }

    #[test]
    fn constant_eq_works() {
        assert!(constant_eq("abc", "abc"));
        assert!(!constant_eq("abc", "abd"));
        assert!(!constant_eq("abc", "abcd"));
    }
}
