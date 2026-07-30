//! Consensus store: cached text, SHA3-256 digest, and freshness tracking.

use std::path::Path;
use std::time::SystemTime;

use anyhow::{Context, Result};
use digest::Digest;
use tor_netdoc::doc::netstatus::MdConsensus;

/// Parse a consensus timestamp line like `valid-after YYYY-MM-DD HH:MM:SS`.
fn parse_timestamp(text: &str, prefix: &str) -> Option<SystemTime> {
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix(prefix) {
            let rfc3339 = format!("{}Z", rest.trim().replace(' ', "T"));
            return humantime::parse_rfc3339(&rfc3339).ok();
        }
    }
    None
}

/// Cached consensus text and SHA3-256 digest of its signed portion.
/// Used to request diffs on subsequent fetches.
pub struct ConsensusStore {
    state: Option<ConsensusState>,
}

struct ConsensusState {
    text: String,
    sha3_of_signed: [u8; 32],
    valid_after: SystemTime,
    fresh_until: SystemTime,
}

impl ConsensusStore {
    /// Create an empty store (no previous consensus).
    pub fn new() -> Self {
        Self { state: None }
    }

    /// Load a previous consensus from disk and compute its signed-portion SHA3-256.
    /// Returns an empty store if the file doesn't exist or can't be parsed.
    pub fn load_from_file(path: &Path) -> Self {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::info!("no existing consensus file, starting fresh");
                return Self::new();
            }
            Err(e) => {
                tracing::warn!("failed to read previous consensus: {}", e);
                return Self::new();
            }
        };
        let valid_after = match parse_timestamp(&text, "valid-after ") {
            Some(t) => t,
            None => {
                tracing::warn!("no valid-after in cached consensus");
                return Self::new();
            }
        };
        let fresh_until = match parse_timestamp(&text, "fresh-until ") {
            Some(t) => t,
            None => {
                tracing::warn!("no fresh-until in cached consensus");
                return Self::new();
            }
        };
        match MdConsensus::parse(&text) {
            Ok((signed, _remainder, _unchecked)) => {
                let sha3: [u8; 32] = sha3::Sha3_256::digest(signed.as_bytes()).into();
                tracing::info!(
                    "loaded previous consensus ({} bytes, valid_after={}, fresh_until={}, sha3={})",
                    text.len(),
                    humantime::format_rfc3339(valid_after),
                    humantime::format_rfc3339(fresh_until),
                    hex::encode(sha3),
                );
                Self {
                    state: Some(ConsensusState {
                        text,
                        sha3_of_signed: sha3,
                        valid_after,
                        fresh_until,
                    }),
                }
            }
            Err(e) => {
                tracing::warn!("failed to parse previous consensus: {}", e);
                Self::new()
            }
        }
    }

    /// Hex-encoded SHA3-256 of the signed portion, for the
    /// `X-Or-Diff-From-Consensus` request header.
    pub fn diff_hex(&self) -> Option<String> {
        self.state.as_ref().map(|s| hex::encode(s.sha3_of_signed))
    }

    /// The previous consensus text (needed to apply diffs).
    pub fn text(&self) -> Option<&str> {
        self.state.as_ref().map(|s| s.text.as_str())
    }

    /// Whether the cached consensus is still fresh (i.e. `now < fresh_until`).
    pub fn is_fresh(&self) -> bool {
        self.state
            .as_ref()
            .map(|s| SystemTime::now() < s.fresh_until)
            .unwrap_or(false)
    }

    /// Resolve a consensus response: apply diff if needed, then update store.
    /// Returns the full consensus text ready for parsing.
    /// Errors if the response contains an older consensus than what we have.
    pub fn resolve_response(&mut self, response: String) -> Result<String> {
        let consensus_text = if tor_consdiff::looks_like_diff(&response) {
            let old_text = self
                .text()
                .ok_or_else(|| anyhow::anyhow!("got diff but no previous consensus"))?;
            tracing::info!(
                "applying consensus diff ({} bytes diff, {} bytes old)",
                response.len(),
                old_text.len(),
            );
            let result = tor_consdiff::apply_diff(old_text, &response, None)
                .context("applying consensus diff")?;
            result
                .check_digest()
                .context("consensus diff digest mismatch")?;
            result.to_string()
        } else {
            tracing::info!("got full consensus ({} bytes)", response.len());
            response
        };

        // Extract timestamps from text and compute SHA3-256 of signed portion
        let new_valid_after = parse_timestamp(&consensus_text, "valid-after ")
            .context("no valid-after in consensus response")?;
        let new_fresh_until = parse_timestamp(&consensus_text, "fresh-until ")
            .context("no fresh-until in consensus response")?;

        // Reject if older than what we already have
        if let Some(ref state) = self.state {
            if new_valid_after < state.valid_after {
                anyhow::bail!(
                    "ignoring older consensus (valid_after={}, have={})",
                    humantime::format_rfc3339(new_valid_after),
                    humantime::format_rfc3339(state.valid_after),
                );
            }
        }

        let (signed, _remainder, _unchecked) =
            MdConsensus::parse(&consensus_text).context("parsing consensus for digest")?;
        let sha3: [u8; 32] = sha3::Sha3_256::digest(signed.as_bytes()).into();
        self.state = Some(ConsensusState {
            text: consensus_text.clone(),
            sha3_of_signed: sha3,
            valid_after: new_valid_after,
            fresh_until: new_fresh_until,
        });

        Ok(consensus_text)
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    /// The smallest document `MdConsensus::parse` accepts, parameterised by the
    /// timestamps the store reads. Signatures are not checked by `parse` (that
    /// is what the returned `unchecked` value is for), so placeholders suffice.
    fn consensus_text(valid_after: &str, fresh_until: &str) -> String {
        format!(
            "\
network-status-version 3 microdesc
vote-status consensus
consensus-method 34
valid-after {valid_after}
fresh-until {fresh_until}
valid-until 2100-01-01 00:00:00
voting-delay 300 300
known-flags Fast Guard Running Stable Valid
params CircuitPriorityHalflifeMsec=30000
dir-source aaa {f40} 1.2.3.4 1.2.3.4 80 9001
contact nobody
vote-digest {f40}
r Unnamed {f27} 2026-07-30 00:00:00 5.6.7.8 9001 0
m {d43}
s Fast Guard Running Stable Valid
pr Link=1-5 LinkAuth=3 Relay=1-2
w Bandwidth=1000
directory-footer
bandwidth-weights Wbd=0
directory-signature sha256 {f40} {g40}
-----BEGIN SIGNATURE-----
AAAA
-----END SIGNATURE-----
",
            f40 = "A".repeat(40),
            g40 = "B".repeat(40),
            f27 = "A".repeat(27),
            d43 = "A".repeat(43),
        )
    }

    fn secs(t: SystemTime) -> u64 {
        t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs()
    }

    #[test]
    fn timestamps_are_read_from_their_own_lines() {
        let text = consensus_text("2026-07-30 00:00:00", "2026-07-30 01:00:00");
        let va = parse_timestamp(&text, "valid-after ").unwrap();
        let fu = parse_timestamp(&text, "fresh-until ").unwrap();
        assert_eq!(fu.duration_since(va).unwrap(), std::time::Duration::from_secs(3600));
        assert_eq!(secs(va), 1785369600);
    }

    #[test]
    fn missing_or_malformed_timestamps_are_none() {
        assert_eq!(parse_timestamp("vote-status consensus\n", "valid-after "), None);
        assert_eq!(parse_timestamp("valid-after not-a-date\n", "valid-after "), None);
        assert_eq!(parse_timestamp("valid-after 2026-13-45 99:99:99\n", "valid-after "), None);
        // The prefix must start the line, so a mention elsewhere is not picked up.
        assert_eq!(
            parse_timestamp("contact see valid-after 2026-07-30 00:00:00\n", "valid-after "),
            None
        );
        // Trailing junk is rejected rather than silently truncated: spaces are
        // rewritten to `T` wholesale, so extra tokens corrupt the timestamp.
        assert_eq!(
            parse_timestamp("valid-after 2026-07-30 00:00:00 extra\n", "valid-after "),
            None
        );
    }

    #[test]
    fn full_consensus_populates_the_store() {
        let mut store = ConsensusStore::new();
        assert!(store.diff_hex().is_none());
        assert!(store.text().is_none());
        assert!(!store.is_fresh(), "an empty store is never fresh");

        let text = consensus_text("2026-07-30 00:00:00", "2099-01-01 00:00:00");
        let out = store.resolve_response(text.clone()).unwrap();
        assert_eq!(out, text);
        assert_eq!(store.text(), Some(text.as_str()));
        assert_eq!(store.diff_hex().unwrap().len(), 64, "hex sha3-256");
        assert!(store.is_fresh(), "fresh-until is in the future");
    }

    #[test]
    fn a_stale_consensus_is_not_fresh() {
        let mut store = ConsensusStore::new();
        store
            .resolve_response(consensus_text("2020-01-01 00:00:00", "2020-01-01 01:00:00"))
            .unwrap();
        assert!(!store.is_fresh());
    }

    #[test]
    fn a_diff_without_a_previous_consensus_is_an_error() {
        let mut store = ConsensusStore::new();
        let diff = "network-status-diff-version 1\nhash A B\n";
        let err = store.resolve_response(diff.to_string()).unwrap_err();
        assert!(
            err.to_string().contains("no previous consensus"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn a_diff_whose_digest_does_not_match_is_rejected() {
        let mut store = ConsensusStore::new();
        store
            .resolve_response(consensus_text("2026-07-30 00:00:00", "2099-01-01 00:00:00"))
            .unwrap();

        // Applies cleanly to the stored text but claims the wrong result digest.
        let diff = format!(
            "network-status-diff-version 1\nhash {} {}\n2c\nvote-status consensus\n.\n",
            "0".repeat(64),
            "1".repeat(64)
        );
        let err = store.resolve_response(diff).unwrap_err();
        let chain = format!("{err:#}");
        assert!(chain.contains("digest"), "unexpected error: {chain}");
    }

    /// The integrity-critical branch: a directory server (or a gateway serving a
    /// replayed snapshot) must not be able to walk the cached consensus
    /// backwards.
    #[test]
    fn an_older_consensus_is_rejected_and_leaves_the_store_untouched() {
        let mut store = ConsensusStore::new();
        let newer = consensus_text("2026-07-30 12:00:00", "2099-01-01 00:00:00");
        store.resolve_response(newer.clone()).unwrap();
        let digest_before = store.diff_hex().unwrap();

        let older = consensus_text("2026-07-30 00:00:00", "2099-01-01 00:00:00");
        let err = store.resolve_response(older).unwrap_err();
        assert!(
            err.to_string().contains("older consensus"),
            "unexpected error: {err}"
        );

        assert_eq!(store.text(), Some(newer.as_str()), "rollback must not overwrite");
        assert_eq!(store.diff_hex().unwrap(), digest_before);
    }

    #[test]
    fn an_equally_recent_consensus_is_accepted() {
        // Only strictly older is rejected; a re-fetch of the same period is a
        // legitimate refresh (its signatures may have grown).
        let mut store = ConsensusStore::new();
        let first = consensus_text("2026-07-30 00:00:00", "2026-07-30 01:00:00");
        store.resolve_response(first).unwrap();
        let same = consensus_text("2026-07-30 00:00:00", "2026-07-30 02:00:00");
        assert!(store.resolve_response(same.clone()).is_ok());
        assert_eq!(store.text(), Some(same.as_str()), "the refresh is stored");
    }

    #[test]
    fn a_newer_consensus_replaces_the_previous_one() {
        let mut store = ConsensusStore::new();
        store
            .resolve_response(consensus_text("2026-07-30 00:00:00", "2026-07-30 01:00:00"))
            .unwrap();
        let before = store.diff_hex().unwrap();
        let newer = consensus_text("2026-07-30 12:00:00", "2026-07-30 13:00:00");
        store.resolve_response(newer.clone()).unwrap();
        assert_eq!(store.text(), Some(newer.as_str()));
        assert_ne!(store.diff_hex().unwrap(), before, "digest tracks the new text");
    }

    #[test]
    fn responses_missing_required_fields_are_errors() {
        let full = consensus_text("2026-07-30 00:00:00", "2026-07-30 01:00:00");

        let no_valid_after: String =
            full.lines().filter(|l| !l.starts_with("valid-after ")).collect::<Vec<_>>().join("\n");
        let err = ConsensusStore::new().resolve_response(no_valid_after).unwrap_err();
        assert!(err.to_string().contains("valid-after"), "{err}");

        let no_fresh_until: String =
            full.lines().filter(|l| !l.starts_with("fresh-until ")).collect::<Vec<_>>().join("\n");
        let err = ConsensusStore::new().resolve_response(no_fresh_until).unwrap_err();
        assert!(err.to_string().contains("fresh-until"), "{err}");

        // Timestamps present but the document is not a consensus.
        let junk = "valid-after 2026-07-30 00:00:00\nfresh-until 2026-07-30 01:00:00\n";
        let err = ConsensusStore::new().resolve_response(junk.to_string()).unwrap_err();
        assert!(err.to_string().contains("parsing consensus"), "{err}");
    }

    fn tempdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tor-js-gw-consensus-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn load_from_file_falls_back_to_empty_on_anything_unusable() {
        let dir = tempdir();

        let missing = dir.join("does-not-exist");
        assert!(ConsensusStore::load_from_file(&missing).text().is_none());

        let garbage = dir.join("garbage");
        std::fs::write(&garbage, "not a consensus at all\n").unwrap();
        assert!(ConsensusStore::load_from_file(&garbage).text().is_none());

        // Parses as text and has timestamps, but is not a consensus document.
        let half = dir.join("half");
        std::fs::write(&half, "valid-after 2026-07-30 00:00:00\nfresh-until 2026-07-30 01:00:00\n")
            .unwrap();
        assert!(ConsensusStore::load_from_file(&half).text().is_none());

        // A real one loads, and agrees with what resolve_response would compute.
        let good = dir.join("good");
        let text = consensus_text("2026-07-30 00:00:00", "2099-01-01 00:00:00");
        std::fs::write(&good, &text).unwrap();
        let loaded = ConsensusStore::load_from_file(&good);
        assert_eq!(loaded.text(), Some(text.as_str()));
        assert!(loaded.is_fresh());

        let mut fresh = ConsensusStore::new();
        fresh.resolve_response(text).unwrap();
        assert_eq!(loaded.diff_hex(), fresh.diff_hex());

        std::fs::remove_dir_all(&dir).ok();
    }
}
