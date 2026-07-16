//! Authority certificate store: cached certs with validation and freshness.

use std::path::Path;
use std::time::SystemTime;

use tor_checkable::{SelfSigned, Timebound};
use tor_llcrypto::pk::rsa::RsaIdentity;
use tor_netdoc::doc::authcert::AuthCert;

/// Cached authority certificates (raw text + parsed certs).
/// Re-fetch is only needed when not all trusted authorities have a valid cert.
pub struct AuthCertStore {
    text: String,
    certs: Vec<AuthCert>,
}

impl AuthCertStore {
    /// The default trusted directory authority identity fingerprints
    /// from Arti's compiled-in configuration.
    pub fn trusted_authority_ids() -> Vec<RsaIdentity> {
        tor_dircommon::authority::AuthorityContacts::builder()
            .build()
            .expect("default authority config")
            .v3idents()
            .clone()
    }

    /// Create an empty store.
    pub fn new() -> Self {
        Self {
            text: String::new(),
            certs: Vec::new(),
        }
    }

    /// Load authority certs from a file on disk, parse and validate them.
    /// Returns an empty store if the file doesn't exist.
    pub fn load_from_file(path: &Path, now: &SystemTime) -> Self {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::info!("no existing authority-certs file, starting fresh");
                return Self::new();
            }
            Err(e) => {
                tracing::warn!("failed to read authority certs: {}", e);
                return Self::new();
            }
        };
        let certs = parse_and_validate_certs(&text, now);
        tracing::info!(
            "loaded {} valid authority certs from cache ({} bytes)",
            certs.len(),
            text.len(),
        );
        Self { text, certs }
    }

    /// Check whether we have a valid cert for every trusted authority.
    pub fn has_all(&self) -> bool {
        let ids = Self::trusted_authority_ids();
        ids.iter()
            .all(|id| self.certs.iter().any(|c| c.id_fingerprint() == id))
    }

    /// Re-validate cached certs against current time, dropping any that expired.
    pub fn refresh(&mut self, now: &SystemTime) {
        let before = self.certs.len();
        self.certs = parse_and_validate_certs(&self.text, now);
        let dropped = before.saturating_sub(self.certs.len());
        if dropped > 0 {
            tracing::info!("dropped {} expired authority certs", dropped);
        }
    }

    /// Replace the store contents with a freshly fetched response.
    pub fn update(&mut self, text: String, now: &SystemTime) {
        self.certs = parse_and_validate_certs(&text, now);
        self.text = text;
    }

    /// The parsed, validated certs (for consensus signature verification).
    pub fn certs(&self) -> &[AuthCert] {
        &self.certs
    }

    /// The raw concatenated cert text (for writing to disk).
    pub fn text(&self) -> &str {
        &self.text
    }
}

/// Parse authority certs from text, keeping only valid certs from trusted authorities.
fn parse_and_validate_certs(text: &str, now: &SystemTime) -> Vec<AuthCert> {
    let trusted_ids = AuthCertStore::trusted_authority_ids();
    let iter = match AuthCert::parse_multiple(text) {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!("failed to parse authority certs: {}", e);
            return Vec::new();
        }
    };
    let mut certs = Vec::new();
    for item in iter {
        match item {
            Ok(unchecked) => match unchecked.check_signature() {
                Ok(timebound) => match timebound.check_valid_at(now) {
                    Ok(cert) => {
                        if trusted_ids.contains(cert.id_fingerprint()) {
                            certs.push(cert);
                        }
                    }
                    Err(_) => {}
                },
                Err(_) => {}
            },
            Err(_) => {}
        }
    }
    certs
}
