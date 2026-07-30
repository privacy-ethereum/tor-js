//! Fast bootstrap: pre-populate directory cache from a bootstrap.zip archive.
//!
//! The callback may return the archive either as raw zip bytes or
//! zstd-compressed (`bootstrap.zip.zst`, as served by tor-js-gateway);
//! compression is detected by the zstd magic and decompressed with ruzstd.
//!
//! Parses a bootstrap.zip (from a tor-js-gateway server) containing:
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

/// Zstd frame magic number (little-endian 0xFD2FB528).
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

/// Decompress the archive if it is zstd-compressed (tor-js-gateway serves
/// `/bootstrap.zip.zst` as raw zstd bytes over KPS — there is no transparent
/// content-encoding on KPS streams). Raw zip bytes pass through unchanged.
fn maybe_decompress_zstd(bytes: Vec<u8>) -> Result<Vec<u8>, String> {
    if bytes.len() < 4 || bytes[..4] != ZSTD_MAGIC {
        return Ok(bytes);
    }
    let cursor = std::io::Cursor::new(bytes);
    let mut decoder = ruzstd::decoding::StreamingDecoder::new(cursor)
        .map_err(|e| format!("fast bootstrap: zstd decode error: {}", e))?;
    let mut decompressed = Vec::new();
    std::io::Read::read_to_end(&mut decoder, &mut decompressed)
        .map_err(|e| format!("fast bootstrap: zstd decompress error: {}", e))?;
    info!(
        "Fast bootstrap: decompressed zstd archive to {} bytes",
        decompressed.len()
    );
    Ok(decompressed)
}

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
    let zip_bytes =
        maybe_decompress_zstd(js_sys::Uint8Array::from(result).to_vec()).map_err(js_err)?;

    info!(
        "Fast bootstrap: received {} bytes, parsing...",
        zip_bytes.len()
    );

    let files = parse_stored_zip(&zip_bytes)
        .map_err(|e| js_err(format_args!("fast bootstrap: {e}")))?;

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

/// Why a bootstrap archive could not be read.
///
/// A plain error type rather than `JsValue`: these bytes come off the wire from
/// the gateway, so the parser is worth testing, and `JsValue` cannot be
/// constructed outside a JS host.
#[derive(Debug, PartialEq, Eq)]
pub enum ZipError {
    /// Compression method other than Stored (0).
    UnsupportedMethod(u16),
    /// A member's declared size runs past the end of the archive.
    Truncated,
    /// A member name is not valid UTF-8.
    InvalidName,
    /// A member's contents are not valid UTF-8.
    InvalidContent,
}

impl std::fmt::Display for ZipError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedMethod(m) => {
                write!(f, "unsupported zip compression method {m}, expected Stored (0)")
            }
            Self::Truncated => write!(f, "zip file truncated"),
            Self::InvalidName => write!(f, "invalid utf8 in zip filename"),
            Self::InvalidContent => write!(f, "invalid utf8 in zip content"),
        }
    }
}

fn parse_stored_zip(data: &[u8]) -> Result<HashMap<String, String>, ZipError> {
    let mut files = HashMap::new();
    let mut offset = 0usize;

    while offset.saturating_add(30) <= data.len() {
        let sig = u32::from_le_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]);
        if sig != 0x04034b50 {
            // Central directory (or trailing junk): the members are all read.
            break;
        }

        let method = u16::from_le_bytes([data[offset + 8], data[offset + 9]]);
        if method != 0 {
            return Err(ZipError::UnsupportedMethod(method));
        }

        let compressed_size = u32::from_le_bytes([
            data[offset + 18],
            data[offset + 19],
            data[offset + 20],
            data[offset + 21],
        ]);
        let name_len = u16::from_le_bytes([data[offset + 26], data[offset + 27]]);
        let extra_len = u16::from_le_bytes([data[offset + 28], data[offset + 29]]);

        // Offsets are accumulated in u64 because every term is attacker-supplied
        // and `usize` is 32 bits on wasm32: computing this in `usize` lets a
        // crafted `compressed_size` near `u32::MAX` wrap past the bounds check
        // below and then panic on the backwards slice. u64 also keeps the check
        // — and its test — behaving identically on every target.
        let name_start = offset as u64 + 30;
        let name_end = name_start + name_len as u64;
        let data_start = name_end + extra_len as u64;
        let data_end = data_start + compressed_size as u64;

        if data_end > data.len() as u64 {
            return Err(ZipError::Truncated);
        }

        // Proven in range, so narrowing cannot lose information.
        let (name_start, name_end) = (name_start as usize, name_end as usize);
        let (data_start, data_end) = (data_start as usize, data_end as usize);

        let name = std::str::from_utf8(&data[name_start..name_end])
            .map_err(|_| ZipError::InvalidName)?;
        let content = std::str::from_utf8(&data[data_start..data_end])
            .map_err(|_| ZipError::InvalidContent)?;

        files.insert(name.to_string(), content.to_string());
        // Each member advances at least past its own 30-byte header, so the
        // loop always makes progress.
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

#[cfg(test)]
mod zip_tests {
    use super::*;

    /// Build one Stored local-file-header record, exactly as the gateway's
    /// `write_bootstrap_archive` emits them.
    fn member(name: &str, content: &[u8]) -> Vec<u8> {
        member_with(name, content, 0, &[])
    }

    fn member_with(name: &str, content: &[u8], method: u16, extra: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&0x04034b50u32.to_le_bytes()); // signature
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0u16.to_le_bytes()); // flags
        out.extend_from_slice(&method.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // mod time
        out.extend_from_slice(&0u16.to_le_bytes()); // mod date
        out.extend_from_slice(&0u32.to_le_bytes()); // crc32
        out.extend_from_slice(&(content.len() as u32).to_le_bytes()); // compressed size
        out.extend_from_slice(&(content.len() as u32).to_le_bytes()); // uncompressed size
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&(extra.len() as u16).to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(extra);
        out.extend_from_slice(content);
        out
    }

    /// A central-directory record, which the parser must stop at rather than
    /// try to read as another member.
    fn central_directory(name: &str) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&0x02014b50u32.to_le_bytes());
        out.extend_from_slice(&[0u8; 24]);
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&[0u8; 16]);
        out.extend_from_slice(name.as_bytes());
        out
    }

    fn bootstrap_archive() -> Vec<u8> {
        let mut zip = Vec::new();
        zip.extend(member("bootstrap/consensus-microdesc.txt", b"consensus text"));
        zip.extend(member("bootstrap/authority-certs.txt", b"cert text"));
        zip.extend(member("bootstrap/microdescs.txt", b"microdesc text"));
        zip.extend(central_directory("bootstrap/consensus-microdesc.txt"));
        zip
    }

    #[test]
    fn a_gateway_archive_yields_its_three_members() {
        let files = parse_stored_zip(&bootstrap_archive()).unwrap();
        assert_eq!(files.len(), 3);
        assert_eq!(files["bootstrap/consensus-microdesc.txt"], "consensus text");
        assert_eq!(files["bootstrap/authority-certs.txt"], "cert text");
        assert_eq!(files["bootstrap/microdescs.txt"], "microdesc text");
    }

    #[test]
    fn parsing_stops_at_the_central_directory() {
        // Trailing bytes after the central directory must not be read as members.
        let mut zip = bootstrap_archive();
        zip.extend_from_slice(&[0xff; 64]);
        assert_eq!(parse_stored_zip(&zip).unwrap().len(), 3);
    }

    #[test]
    fn an_extra_field_is_skipped() {
        let zip = member_with("f.txt", b"payload", 0, &[1, 2, 3, 4, 5]);
        let files = parse_stored_zip(&zip).unwrap();
        assert_eq!(files["f.txt"], "payload");
    }

    #[test]
    fn empty_members_are_read_as_empty_strings() {
        let mut zip = Vec::new();
        zip.extend(member("a.txt", b""));
        zip.extend(member("b.txt", b"x"));
        let files = parse_stored_zip(&zip).unwrap();
        assert_eq!(files["a.txt"], "");
        assert_eq!(files["b.txt"], "x");
    }

    #[test]
    fn compressed_members_are_refused_rather_than_mis_read() {
        // Deflate (8) is the zip default, so this is the likely mistake.
        let zip = member_with("f.txt", b"payload", 8, &[]);
        assert_eq!(parse_stored_zip(&zip), Err(ZipError::UnsupportedMethod(8)));
    }

    #[test]
    fn nothing_before_a_local_header_is_read() {
        assert!(parse_stored_zip(b"").unwrap().is_empty());
        assert!(parse_stored_zip(b"too short").unwrap().is_empty());
        // 30+ bytes but no signature: not a zip, and not an error either.
        assert!(parse_stored_zip(&[0u8; 128]).unwrap().is_empty());
    }

    #[test]
    fn a_member_running_past_the_end_is_truncated() {
        let zip = member("f.txt", b"payload");
        for cut in 1..=8 {
            assert_eq!(
                parse_stored_zip(&zip[..zip.len() - cut]),
                Err(ZipError::Truncated),
                "cutting {cut} bytes should read as truncated"
            );
        }
    }

    /// A gateway can hand back anything. With the offsets computed in `usize`,
    /// this header wrapped on wasm32 (32-bit `usize`), passed the truncation
    /// check, and then panicked on a backwards slice — a bootstrap-time crash
    /// triggerable by whoever serves the archive.
    #[test]
    fn a_size_near_u32_max_is_truncated_not_a_panic() {
        let mut zip = member("f.txt", b"payload");
        // Overwrite the compressed size with a value that wraps 32-bit usize
        // once the header and name offsets are added.
        let huge = u32::MAX.to_le_bytes();
        zip[18..22].copy_from_slice(&huge);
        assert_eq!(parse_stored_zip(&zip), Err(ZipError::Truncated));

        // The same for values that wrap only after the name and extra fields.
        for size in [u32::MAX - 30, u32::MAX - 35, u32::MAX - 1] {
            let mut zip = member("f.txt", b"payload");
            zip[18..22].copy_from_slice(&size.to_le_bytes());
            assert_eq!(parse_stored_zip(&zip), Err(ZipError::Truncated), "size {size}");
        }
    }

    /// Documents *why* the parser widens to u64. On a 64-bit host these terms
    /// cannot wrap, so the truncation tests above would pass either way — this
    /// is the arithmetic that used to defeat the bounds check on wasm32.
    #[test]
    fn the_offset_arithmetic_would_wrap_in_32_bits() {
        let data_start: u32 = 30 + 5; // header + a short name, no extra field
        let compressed_size: u32 = u32::MAX;
        assert!(
            data_start.checked_add(compressed_size).is_none(),
            "32-bit `usize` wraps here, yielding data_end < data_start"
        );
        assert_eq!(data_start.wrapping_add(compressed_size), 34);
        // Widened, the terms stay ordered, so `data_end > len` still holds.
        assert!(data_start as u64 + compressed_size as u64 > u32::MAX as u64);
    }

    #[test]
    fn an_oversized_name_length_is_truncated_not_a_panic() {
        let mut zip = member("f.txt", b"payload");
        zip[26..28].copy_from_slice(&u16::MAX.to_le_bytes());
        assert_eq!(parse_stored_zip(&zip), Err(ZipError::Truncated));
    }

    #[test]
    fn an_oversized_extra_length_is_truncated_not_a_panic() {
        let mut zip = member("f.txt", b"payload");
        zip[28..30].copy_from_slice(&u16::MAX.to_le_bytes());
        assert_eq!(parse_stored_zip(&zip), Err(ZipError::Truncated));
    }

    #[test]
    fn non_utf8_names_and_contents_are_rejected() {
        // Name bytes are spliced in directly so they can be invalid UTF-8.
        let mut zip = member("ab.txt", b"payload");
        zip[30] = 0xff;
        assert_eq!(parse_stored_zip(&zip), Err(ZipError::InvalidName));

        let zip = member("f.txt", &[0xff, 0xfe]);
        assert_eq!(parse_stored_zip(&zip), Err(ZipError::InvalidContent));
    }

    #[test]
    fn a_repeated_name_keeps_the_last_member() {
        let mut zip = Vec::new();
        zip.extend(member("f.txt", b"first"));
        zip.extend(member("f.txt", b"second"));
        let files = parse_stored_zip(&zip).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files["f.txt"], "second");
    }

    /// Every member advances the cursor past at least its own header, so no
    /// crafted archive can spin the loop.
    #[test]
    fn zero_length_members_still_terminate() {
        let mut zip = Vec::new();
        for _ in 0..50 {
            zip.extend(member("", b""));
        }
        let files = parse_stored_zip(&zip).unwrap();
        assert_eq!(files.len(), 1, "all 50 share the empty name");
    }

    // ---- zstd wrapper ----------------------------------------------------

    #[test]
    fn raw_zip_bytes_pass_through_undisturbed() {
        let zip = bootstrap_archive();
        assert_eq!(maybe_decompress_zstd(zip.clone()).unwrap(), zip);
    }

    #[test]
    fn short_inputs_are_not_mistaken_for_zstd_frames() {
        for len in 0..4 {
            let bytes = ZSTD_MAGIC[..len].to_vec();
            assert_eq!(maybe_decompress_zstd(bytes.clone()).unwrap(), bytes);
        }
    }

    #[test]
    fn a_zstd_frame_is_decompressed_and_then_parses() {
        let zip = bootstrap_archive();
        let compressed = zstd_encode(&zip);
        assert_eq!(&compressed[..4], &ZSTD_MAGIC, "fixture is not a zstd frame");

        let decompressed = maybe_decompress_zstd(compressed).unwrap();
        assert_eq!(decompressed, zip);
        assert_eq!(parse_stored_zip(&decompressed).unwrap().len(), 3);
    }

    #[test]
    fn a_corrupt_zstd_frame_is_an_error_not_a_panic() {
        let mut compressed = zstd_encode(&bootstrap_archive());
        let tail = compressed.len() - 8;
        compressed.truncate(tail);
        assert!(maybe_decompress_zstd(compressed).is_err());

        // Magic bytes with nothing behind them.
        assert!(maybe_decompress_zstd(ZSTD_MAGIC.to_vec()).is_err());
    }

    /// Minimal zstd frame writer: a single raw (uncompressed) block, which
    /// ruzstd decodes like any other frame. Avoids adding an encoder dependency
    /// to the wasm crate just for tests.
    fn zstd_encode(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&ZSTD_MAGIC);
        // Frame_Header_Descriptor: no content size, not single-segment, no
        // checksum, no dictionary — so a Window_Descriptor follows and no
        // Frame_Content_Size field does.
        out.push(0x00);
        // Window_Descriptor: exponent 7 → windowLog 17 → a 128 KiB window,
        // which is the largest a raw block can be.
        out.push(0x38);
        // Raw blocks are capped at 128 KiB each.
        let mut chunks = data.chunks(1 << 17).peekable();
        if data.is_empty() {
            out.extend_from_slice(&[0x01, 0x00, 0x00]);
        }
        while let Some(chunk) = chunks.next() {
            let last = chunks.peek().is_none() as u32;
            // block_header = last_block | (block_type << 1) | (size << 3),
            // block_type 0 = Raw.
            let header = last | (0 << 1) | ((chunk.len() as u32) << 3);
            out.extend_from_slice(&header.to_le_bytes()[..3]);
            out.extend_from_slice(chunk);
        }
        out
    }
}
