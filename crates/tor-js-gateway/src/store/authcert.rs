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


#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    /// Ten real certificates fetched from a directory authority
    /// (`/tor/keys/all`), kept verbatim so signature verification is exercised
    /// for real: the nine current trusted authorities plus one from a retired
    /// authority that Arti no longer trusts.
    ///
    /// Every assertion below fixes `now` at a literal instant rather than using
    /// the clock, so the fixture cannot rot into a failing test as the certs age.
    const CERTS: &str = include_str!("../../testdata/authority-certs.txt");

    /// Inside every fixture cert's validity window (published ≤ t ≤ expires) for
    /// the nine trusted ones.
    fn all_valid() -> SystemTime {
        at("2026-08-01T00:00:00Z")
    }

    fn at(rfc3339: &str) -> SystemTime {
        humantime::parse_rfc3339(rfc3339).unwrap()
    }

    fn hex_id(cert: &AuthCert) -> String {
        hex::encode_upper(cert.id_fingerprint().as_bytes())
    }

    #[test]
    fn the_fixture_holds_ten_parseable_certs() {
        let count = AuthCert::parse_multiple(CERTS).unwrap().count();
        assert_eq!(count, 10, "fixture changed shape");
    }

    #[test]
    fn only_trusted_authorities_survive_validation() {
        let certs = parse_and_validate_certs(CERTS, &all_valid());
        assert_eq!(
            certs.len(),
            9,
            "expected the nine trusted authority certs, got {:?}",
            certs.iter().map(hex_id).collect::<Vec<_>>()
        );

        let trusted = AuthCertStore::trusted_authority_ids();
        for cert in &certs {
            assert!(trusted.contains(cert.id_fingerprint()), "{} not trusted", hex_id(cert));
        }
        // The retired authority's cert is in the file but never accepted.
        assert!(
            !certs.iter().any(|c| hex_id(c) == "D586D18309DED4CD6D57C18FDB97EFA96D330566"),
            "a cert from an untrusted authority was accepted"
        );
    }

    /// The untrusted cert's own validity window is 2022-11 → 2023-11, so at an
    /// instant inside it the only thing that can exclude it is the trusted-id
    /// check — which is what this pins.
    #[test]
    fn a_valid_cert_from_an_untrusted_authority_is_still_dropped() {
        let certs = parse_and_validate_certs(CERTS, &at("2023-01-01T00:00:00Z"));
        assert!(
            certs.is_empty(),
            "accepted {:?} at an instant when only the untrusted cert was in window",
            certs.iter().map(hex_id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn expired_certs_are_dropped() {
        // One trusted cert's window closes on 2026-08-17; the rest run later.
        let before = parse_and_validate_certs(CERTS, &all_valid()).len();
        let after = parse_and_validate_certs(CERTS, &at("2026-09-01T00:00:00Z")).len();
        assert_eq!(after, before - 1, "the cert expiring 2026-08-17 should be gone");

        // Far enough out, nothing is valid.
        assert!(parse_and_validate_certs(CERTS, &at("2030-01-01T00:00:00Z")).is_empty());
    }

    #[test]
    fn certs_are_dropped_before_they_are_published() {
        // Earlier than every fixture cert's publication date.
        assert!(parse_and_validate_certs(CERTS, &at("2000-01-01T00:00:00Z")).is_empty());
    }

    /// A cert whose body has been altered must fail signature verification, even
    /// though its fingerprint still names a trusted authority.
    #[test]
    fn a_tampered_cert_fails_its_signature_check() {
        let good = parse_and_validate_certs(CERTS, &all_valid()).len();

        // Corrupt one base64 character inside the first cert's key material.
        let marker = "dir-signing-key\n-----BEGIN RSA PUBLIC KEY-----\n";
        let at_key = CERTS.find(marker).unwrap() + marker.len();
        let victim = &CERTS[at_key..at_key + 1];
        let replacement = if victim == "A" { "B" } else { "A" };
        let tampered = format!("{}{replacement}{}", &CERTS[..at_key], &CERTS[at_key + 1..]);

        let certs = parse_and_validate_certs(&tampered, &all_valid());
        assert!(
            certs.len() < good,
            "tampering with the signing key did not invalidate any cert"
        );
    }

    #[test]
    fn unparseable_text_yields_no_certs_rather_than_panicking() {
        for text in ["", "not a certificate", "dir-key-certificate-version 3\n"] {
            assert!(parse_and_validate_certs(text, &all_valid()).is_empty(), "{text:?}");
        }
    }

    // ---- store behaviour -------------------------------------------------

    #[test]
    fn an_empty_store_has_nothing_and_covers_nobody() {
        let store = AuthCertStore::new();
        assert!(store.certs().is_empty());
        assert_eq!(store.text(), "");
        assert!(!store.has_all());
    }

    #[test]
    fn a_full_set_of_trusted_certs_needs_no_refetch() {
        let mut store = AuthCertStore::new();
        store.update(CERTS.to_string(), &all_valid());
        assert!(
            store.has_all(),
            "every trusted authority should be covered by the fixture — if Arti's \
             authority list changed, refresh testdata/authority-certs.txt"
        );
        assert_eq!(store.certs().len(), 9);
        assert_eq!(store.text(), CERTS, "the raw text is kept verbatim for writing to disk");
    }

    #[test]
    fn a_missing_authority_forces_a_refetch() {
        // Drop the first certificate from the bundle.
        let second = CERTS[1..].find("dir-key-certificate-version").unwrap() + 1;
        let mut store = AuthCertStore::new();
        store.update(CERTS[second..].to_string(), &all_valid());
        assert_eq!(store.certs().len(), 8);
        assert!(!store.has_all(), "has_all must not tolerate a gap in coverage");
    }

    /// `refresh` re-validates the cached text against a new clock, which is how
    /// a long-running gateway notices its certs aged out.
    #[test]
    fn refresh_revalidates_against_the_new_time() {
        let mut store = AuthCertStore::new();
        store.update(CERTS.to_string(), &all_valid());
        assert_eq!(store.certs().len(), 9);

        store.refresh(&at("2026-09-01T00:00:00Z"));
        assert_eq!(store.certs().len(), 8);
        assert!(!store.has_all());

        store.refresh(&at("2030-01-01T00:00:00Z"));
        assert!(store.certs().is_empty());

        // The text is untouched, so an earlier clock recovers the same certs.
        assert_eq!(store.text(), CERTS);
        store.refresh(&all_valid());
        assert_eq!(store.certs().len(), 9);
    }

    #[test]
    fn update_keeps_the_text_even_when_nothing_validates() {
        let mut store = AuthCertStore::new();
        store.update("garbage".to_string(), &all_valid());
        assert!(store.certs().is_empty());
        assert_eq!(store.text(), "garbage");
        assert!(!store.has_all());
    }

    #[test]
    fn load_from_file_matches_update_and_tolerates_a_missing_file() {
        let dir = TempDir::new("authcert");

        let empty = AuthCertStore::load_from_file(&dir.join("absent"), &all_valid());
        assert!(empty.certs().is_empty());
        assert_eq!(empty.text(), "");

        let path = dir.join("authority-certs.txt");
        std::fs::write(&path, CERTS).unwrap();
        let loaded = AuthCertStore::load_from_file(&path, &all_valid());
        assert_eq!(loaded.certs().len(), 9);
        assert!(loaded.has_all());
        assert_eq!(loaded.text(), CERTS);

        // Loading at an instant when they have all expired keeps the text but
        // no certs, so the next sync refetches.
        let stale = AuthCertStore::load_from_file(&path, &at("2030-01-01T00:00:00Z"));
        assert!(stale.certs().is_empty());
        assert!(!stale.has_all());
        assert_eq!(stale.text(), CERTS);
    }

    #[test]
    fn load_from_file_survives_a_corrupt_cache() {
        let dir = TempDir::new("authcert-corrupt");
        let path = dir.join("authority-certs.txt");
        std::fs::write(&path, "not certificates at all\n").unwrap();
        let store = AuthCertStore::load_from_file(&path, &all_valid());
        assert!(store.certs().is_empty());
        assert!(!store.has_all());
    }

    #[test]
    fn the_trusted_authority_list_is_populated() {
        // A silently empty list would make `has_all` vacuously true and skip
        // certificate fetching entirely.
        let ids = AuthCertStore::trusted_authority_ids();
        assert!(ids.len() >= 5, "suspiciously few authorities: {}", ids.len());
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "duplicate authority ids");
    }
}
