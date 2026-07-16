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
