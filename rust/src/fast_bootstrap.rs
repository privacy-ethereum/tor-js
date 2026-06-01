//! Fast bootstrap: pre-populate directory cache from a bootstrap.zip archive.
//!
//! Parses a bootstrap.zip (from a tor-fast-bootstrap server) containing:
//! - `bootstrap/consensus-microdesc.txt`
//! - `bootstrap/authority-certs.txt`
//! - `bootstrap/microdescs.txt`
//!
//! Uses arti's own document parsers ([`MdConsensus`], [`AuthCert`])
//! for consensus and authority certs. Microdescriptors use lightweight
//! text splitting with SHA-256 (matching arti's digest computation)
//! to avoid the cost of full parsing in WASM (~3s for ~10k microdescs).
//!
//! Writes parsed documents directly to [`CachedJsStorage`] so the Tor
//! client can skip fetching directory data from the network on first boot.

use crate::storage::CachedJsStorage;
use arti_client::storage::KeyValueStore;
use digest::Digest;
use std::collections::HashMap;
use tor_checkable::{SelfSigned, Timebound};
use tor_netdoc::doc::authcert::AuthCert;
use tor_netdoc::doc::netstatus::MdConsensus;
use tracing::{info, warn};
use wasm_bindgen::JsCast;

/// Check if storage is empty and, if so, populate from the fast bootstrap callback.
pub async fn maybe_fast_bootstrap(
    storage: &CachedJsStorage,
    callback: js_sys::Function,
) -> Result<(), wasm_bindgen::JsValue> {
    // Check if we already have a consensus
    let consensus_keys = storage
        .keys("dir:consensus:")
        .map_err(|e| wasm_bindgen::JsValue::from_str(&format!("storage error: {}", e)))?;

    if !consensus_keys.is_empty() {
        info!("Fast bootstrap: storage already has consensus, skipping");
        return Ok(());
    }

    info!("Fast bootstrap: fetching bootstrap.zip...");

    // Call the JS callback: () => Promise<Uint8Array>
    let promise = callback
        .call0(&wasm_bindgen::JsValue::NULL)
        .map_err(|e| {
            wasm_bindgen::JsValue::from_str(&format!("fast bootstrap callback failed: {:?}", e))
        })?;
    let promise = js_sys::Promise::from(promise);
    let result = wasm_bindgen_futures::JsFuture::from(promise).await?;
    let zip_bytes = js_sys::Uint8Array::from(result).to_vec();

    info!(
        "Fast bootstrap: received {} bytes, parsing...",
        zip_bytes.len()
    );

    let files = parse_stored_zip(&zip_bytes)?;

    let consensus_text = files
        .get("bootstrap/consensus-microdesc.txt")
        .ok_or_else(|| {
            wasm_bindgen::JsValue::from_str(
                "fast bootstrap: missing bootstrap/consensus-microdesc.txt in zip",
            )
        })?;

    let authcert_text = files
        .get("bootstrap/authority-certs.txt")
        .map(|s| s.as_str())
        .unwrap_or("");
    let microdesc_text = files
        .get("bootstrap/microdescs.txt")
        .map(|s| s.as_str())
        .unwrap_or("");

    // Parse consensus using arti's parser to get the signed portion split and lifetime
    let (signed_str, _remainder, unchecked) = MdConsensus::parse(consensus_text)
        .map_err(|e| wasm_bindgen::JsValue::from_str(&format!("consensus parse error: {}", e)))?;

    let lifetime = unchecked.dangerously_peek().peek_lifetime();
    let valid_after_secs = system_time_to_secs(lifetime.valid_after());
    let fresh_until_secs = system_time_to_secs(lifetime.fresh_until());
    let valid_until_secs = system_time_to_secs(lifetime.valid_until());

    store_consensus(
        storage,
        consensus_text,
        signed_str,
        valid_after_secs,
        fresh_until_secs,
        valid_until_secs,
    )?;

    // Parse and store authority certificates
    store_authcerts(storage, authcert_text)?;

    // Parse and store microdescriptors
    store_microdescs(storage, microdesc_text, valid_after_secs).await?;

    info!("Fast bootstrap: done");
    Ok(())
}

// ============================================================================
// Zip parser (Stored-only)
// ============================================================================

fn parse_stored_zip(data: &[u8]) -> Result<HashMap<String, String>, wasm_bindgen::JsValue> {
    let mut files = HashMap::new();
    let mut offset = 0;

    while offset + 30 <= data.len() {
        let sig = u32::from_le_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]);
        if sig != 0x04034b50 {
            break;
        }

        let method = u16::from_le_bytes([data[offset + 8], data[offset + 9]]);
        if method != 0 {
            return Err(wasm_bindgen::JsValue::from_str(&format!(
                "unsupported zip compression method {}, expected Stored (0)",
                method
            )));
        }

        let compressed_size = u32::from_le_bytes([
            data[offset + 18],
            data[offset + 19],
            data[offset + 20],
            data[offset + 21],
        ]) as usize;
        let name_len = u16::from_le_bytes([data[offset + 26], data[offset + 27]]) as usize;
        let extra_len = u16::from_le_bytes([data[offset + 28], data[offset + 29]]) as usize;

        let name_start = offset + 30;
        let name_end = name_start + name_len;
        let data_start = name_end + extra_len;
        let data_end = data_start + compressed_size;

        if data_end > data.len() {
            return Err(wasm_bindgen::JsValue::from_str("zip file truncated"));
        }

        let name = std::str::from_utf8(&data[name_start..name_end])
            .map_err(|_| wasm_bindgen::JsValue::from_str("invalid utf8 in zip filename"))?;
        let content = std::str::from_utf8(&data[data_start..data_end])
            .map_err(|_| wasm_bindgen::JsValue::from_str("invalid utf8 in zip content"))?;

        files.insert(name.to_string(), content.to_string());
        offset = data_end;
    }

    Ok(files)
}

// ============================================================================
// Helpers
// ============================================================================

fn system_time_to_secs(t: std::time::SystemTime) -> u64 {
    t.duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn js_err(msg: impl std::fmt::Display) -> wasm_bindgen::JsValue {
    wasm_bindgen::JsValue::from_str(&msg.to_string())
}

fn storage_set(
    storage: &CachedJsStorage,
    key: &str,
    value: &str,
) -> Result<(), wasm_bindgen::JsValue> {
    storage
        .set(key, value)
        .map_err(|e| js_err(format_args!("storage error: {}", e)))
}

// ============================================================================
// Consensus storage
// ============================================================================

fn store_consensus(
    storage: &CachedJsStorage,
    consensus_text: &str,
    signed_str: &str,
    valid_after_secs: u64,
    fresh_until_secs: u64,
    valid_until_secs: u64,
) -> Result<(), wasm_bindgen::JsValue> {
    let sha3_of_whole =
        hex::encode(tor_llcrypto::d::Sha3_256::digest(consensus_text.as_bytes()));
    let sha3_of_signed =
        hex::encode(tor_llcrypto::d::Sha3_256::digest(signed_str.as_bytes()));

    let key = format!("dir:consensus:microdesc:{}", sha3_of_whole);
    let value = serde_json::json!({
        "valid_after_secs": valid_after_secs,
        "fresh_until_secs": fresh_until_secs,
        "valid_until_secs": valid_until_secs,
        "sha3_of_signed_hex": sha3_of_signed,
        "sha3_of_whole_hex": sha3_of_whole,
        "pending": false,
        "content": consensus_text,
    });

    storage_set(storage, &key, &value.to_string())?;
    info!(
        "Fast bootstrap: stored consensus (valid-after {})",
        valid_after_secs
    );
    Ok(())
}

// ============================================================================
// Authority certificate storage
// ============================================================================

fn store_authcerts(
    storage: &CachedJsStorage,
    authcert_text: &str,
) -> Result<(), wasm_bindgen::JsValue> {
    if authcert_text.is_empty() {
        return Ok(());
    }

    let certs_iter = AuthCert::parse_multiple(authcert_text)
        .map_err(|e| js_err(format_args!("authcert parse error: {}", e)))?;

    let mut entries = Vec::new();
    for cert_result in certs_iter {
        match cert_result {
            Ok(unchecked) => {
                // Extract text from the unchecked cert (within() is on UncheckedAuthCert)
                let cert_text = unchecked
                    .within(authcert_text)
                    .unwrap_or("");

                // Skip signature and time checks for metadata extraction only.
                // Arti re-verifies everything when loading from storage (see state.rs add_from_cache impls).
                let cert = unchecked
                    .dangerously_assume_wellsigned()
                    .dangerously_assume_timely();
                let ids = cert.key_ids();
                let id_hex = hex::encode(ids.id_fingerprint.as_bytes());
                let sk_hex = hex::encode(ids.sk_fingerprint.as_bytes());
                let published_secs = system_time_to_secs(cert.published());
                let expires_secs = system_time_to_secs(cert.expires());

                let key = format!("dir:authcert:{}:{}", id_hex, sk_hex);
                let value = serde_json::json!({
                    "id_fingerprint_hex": id_hex,
                    "sk_fingerprint_hex": sk_hex,
                    "published_secs": published_secs,
                    "expires_secs": expires_secs,
                    "content": cert_text,
                });
                entries.push((key, value.to_string()));
            }
            Err(e) => {
                warn!("Fast bootstrap: skipping malformed authcert: {}", e);
            }
        }
    }

    let count = entries.len();
    storage
        .set_many(entries)
        .map_err(|e| js_err(format_args!("storage error: {}", e)))?;
    info!("Fast bootstrap: stored {} authority certs", count);
    Ok(())
}

// ============================================================================
// Microdescriptor storage
// ============================================================================

/// Store microdescriptors using lightweight text splitting instead of arti's
/// full parser. Each microdesc starts with "onion-key\n" and the storage key
/// is SHA-256 of the text from that boundary to the next (matching how arti
/// computes `Microdesc::sha256`).
///
/// Uses `crypto.subtle.digest` for SHA-256 (hardware-accelerated) instead of
/// the pure-Rust `sha2` crate which is ~100x slower in WASM.
async fn store_microdescs(
    storage: &CachedJsStorage,
    microdesc_text: &str,
    listed_at_secs: u64,
) -> Result<(), wasm_bindgen::JsValue> {
    if microdesc_text.is_empty() {
        return Ok(());
    }

    // Pre-format the fixed suffix since listed_at_secs is the same for all.
    let listed_suffix = format!(",\"listed_at_secs\":{}}}", listed_at_secs);

    // Split on "onion-key\n" boundaries. Each microdesc starts with this marker.
    let marker = "onion-key\n";

    // Find all occurrences of "onion-key\n" as microdesc boundaries.
    let mut positions: Vec<usize> = Vec::new();
    let mut search_from = 0;
    while let Some(pos) = microdesc_text[search_from..].find(marker) {
        positions.push(search_from + pos);
        search_from = search_from + pos + marker.len();
    }

    // Compute text boundaries
    let mut slices: Vec<&str> = Vec::with_capacity(positions.len());
    for (i, &start) in positions.iter().enumerate() {
        let end = if i + 1 < positions.len() {
            let next_start = positions[i + 1];
            let slice = &microdesc_text[start..next_start];
            start + slice.trim_end().len() + 1
        } else {
            let slice = &microdesc_text[start..];
            start + slice.trim_end().len() + 1
        };
        let end = end.min(microdesc_text.len());
        slices.push(&microdesc_text[start..end]);
    }

    // Batch SHA-256 via crypto.subtle.digest (hardware-accelerated).
    // Works in both Window and Worker contexts.
    let crypto: web_sys::Crypto = js_sys::Reflect::get(&js_sys::global(), &"crypto".into())
        .map_err(|_| js_err("crypto not available"))?
        .dyn_into()
        .map_err(|_| js_err("crypto is not a Crypto object"))?;
    let subtle = crypto.subtle();

    let digest_promises = js_sys::Array::new_with_length(slices.len() as u32);
    for (i, slice) in slices.iter().enumerate() {
        let buf = js_sys::Uint8Array::from(slice.as_bytes());
        let promise = subtle.digest_with_str_and_buffer_source("SHA-256", &buf)?;
        digest_promises.set(i as u32, promise.into());
    }

    let all_digests = wasm_bindgen_futures::JsFuture::from(
        js_sys::Promise::all(&digest_promises),
    )
    .await?;
    let results = js_sys::Array::from(&all_digests);

    // Build entries with hex-encoded digests
    let mut entries = Vec::with_capacity(slices.len());
    for (idx, md_text) in slices.iter().enumerate() {
        let array_buf = results.get(idx as u32);
        let digest_bytes = js_sys::Uint8Array::new(&array_buf);
        let mut digest = [0u8; 32];
        digest_bytes.copy_to(&mut digest);
        let digest_hex = hex::encode(digest);

        let key = format!("dir:microdesc:{}", digest_hex);
        // Build JSON directly with newline escaping.
        let mut value = String::with_capacity(md_text.len() + md_text.len() / 20 + 60);
        value.push_str("{\"content\":\"");
        for b in md_text.bytes() {
            match b {
                b'\n' => value.push_str("\\n"),
                b'\r' => value.push_str("\\r"),
                b'"' => value.push_str("\\\""),
                b'\\' => value.push_str("\\\\"),
                _ => value.push(b as char),
            }
        }
        value.push('"');
        value.push_str(&listed_suffix);
        entries.push((key, value));
    }

    let count = entries.len();
    storage
        .set_many(entries)
        .map_err(|e| js_err(format_args!("storage error: {}", e)))?;
    info!("Fast bootstrap: stored {} microdescriptors", count);
    Ok(())
}
