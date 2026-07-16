//! The gateway's HTTP routes, served per-stream over KPS (PROTOCOL.md §5).
//!
//! axum stays as the routing library: a `Router` is a tower `Service`, called
//! once per KPS stream by `kps_server` without any TCP listener behind it.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

use crate::tunnel::{ConnectionTracker, RelayAllowlist, TunnelLimits};

/// Shared server state: everything the routes and the CONNECT handler need.
pub struct Gateway {
    pub data_dir: PathBuf,
    pub relay_allowlist: RelayAllowlist,
    pub tracker: ConnectionTracker,
    pub limits: TunnelLimits,
    pub has_ipv6: bool,
    /// Root of the hash-addressed object tree (`<keccak_dir>/<hh>/<rest>`);
    /// empty when the capability is disabled.
    pub keccak_dir: PathBuf,
    /// Whether the worker-bundles capability is enabled (keccak_dir is set).
    pub keccak_enabled: bool,
    /// Hashes verified on a prior request. Content is hash-addressed and
    /// immutable, so a hash that verified once stays valid; caching it lets
    /// repeat requests skip re-hashing. Populated lazily — nothing needs to
    /// exist at startup.
    pub verified_bundles: RwLock<HashSet<String>>,
    /// Precomputed `/metadata.json` document.
    pub metadata_json: String,
}

/// Builds the metadata document served at `/metadata.json` (PROTOCOL.md §5).
pub fn build_metadata(addresses: &[String], worker_bundles: bool) -> String {
    let mut capabilities = vec!["metadata", "bootstrap", "connect", "relay-random"];
    if worker_bundles {
        capabilities.push("worker-bundles");
    }
    serde_json::json!({
        "protocol": "kps-http/1",
        "software": "tor-js-gateway",
        "version": env!("CARGO_PKG_VERSION"),
        "capabilities": capabilities,
        "addresses": addresses,
    })
    .to_string()
}

fn is_lower_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn keccak256_hex(bytes: &[u8]) -> String {
    use digest::Digest;
    hex::encode(sha3::Keccak256::digest(bytes))
}

/// Builds the router. Unknown paths → `404`; known path, wrong method → `405`
/// (axum's defaults, matching PROTOCOL.md §3.5).
pub fn build_router(gateway: Arc<Gateway>) -> Router {
    Router::new()
        .route("/metadata.json", get(handle_metadata))
        .route("/bootstrap.zip.zst", get(handle_bootstrap_zip_zst))
        .route("/keccak/{prefix}/{rest}", get(handle_worker_bundle))
        .route("/relay/random", get(handle_random_relay))
        .with_state(gateway)
}

/// GET /metadata.json — capability discovery.
async fn handle_metadata(State(gw): State<Arc<Gateway>>) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        gw.metadata_json.clone(),
    )
        .into_response()
}

/// GET /keccak/{hash[0..2]}/{hash[2..]} — immutable, hash-addressed worker
/// bundles served from disk at the same sharded path.
///
/// Verification is lazy: the file is read and its keccak256 checked against
/// the path on request, so objects can be added after startup without a
/// restart. A hash that verifies is cached (content is immutable), and the
/// hex validation on the path segments also keeps the join traversal-safe.
async fn handle_worker_bundle(
    State(gw): State<Arc<Gateway>>,
    Path((prefix, rest)): Path<(String, String)>,
) -> Response {
    if !gw.keccak_enabled || !is_lower_hex(&prefix, 2) || !is_lower_hex(&rest, 62) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let hash = format!("{prefix}{rest}");
    let path = gw.keccak_dir.join(&prefix).join(&rest);

    let bytes = match tokio::fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };

    // Verify unless a previous request already did (immutable content).
    let cached = gw.verified_bundles.read().unwrap_or_else(|e| e.into_inner()).contains(&hash);
    if !cached {
        let actual = keccak256_hex(&bytes);
        if actual != hash {
            tracing::error!(
                "keccak dir: REFUSING {} — keccak256 of contents is {}, path says {}",
                path.display(),
                actual,
                hash,
            );
            return StatusCode::NOT_FOUND.into_response();
        }
        gw.verified_bundles.write().unwrap_or_else(|e| e.into_inner()).insert(hash);
    }

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/javascript"),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        bytes,
    )
        .into_response()
}

/// GET /relay/random — return a random relay address from the allowlist.
async fn handle_random_relay(State(gw): State<Arc<Gateway>>) -> Response {
    use rand::Rng;
    let allowlist = gw.relay_allowlist.read().unwrap_or_else(|e| e.into_inner());
    let candidates: Vec<_> = if gw.has_ipv6 {
        allowlist.iter().collect()
    } else {
        allowlist.iter().filter(|a| a.is_ipv4()).collect()
    };
    if candidates.is_empty() {
        return (StatusCode::SERVICE_UNAVAILABLE, "no relays available").into_response();
    }
    let idx = rand::rng().random_range(0..candidates.len());
    let addr = candidates[idx];
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain")],
        addr.to_string(),
    )
        .into_response()
}

/// Read the cached ETag from disk, or compute it from bootstrap.zip if missing.
async fn read_etag(dir: &std::path::Path) -> Option<String> {
    if let Ok(s) = tokio::fs::read_to_string(dir.join("bootstrap.etag")).await {
        return Some(format!("\"{}\"", s.trim()));
    }
    // Fallback: hash the zip on disk and write the etag file for next time.
    let data = tokio::fs::read(dir.join("bootstrap.zip")).await.ok()?;
    use digest::Digest;
    let hash = hex::encode(sha3::Sha3_256::digest(&data));
    let _ = tokio::fs::write(dir.join("bootstrap.etag"), hash.as_bytes()).await;
    Some(format!("\"{}\"", hash))
}

/// Check If-None-Match against the current ETag. Returns Some(304) if matched.
fn check_not_modified(headers: &HeaderMap, etag: &str) -> Option<Response> {
    let if_none_match = headers.get(header::IF_NONE_MATCH)?.to_str().ok()?;
    if if_none_match == etag || if_none_match == "*" {
        Some(StatusCode::NOT_MODIFIED.into_response())
    } else {
        None
    }
}

/// GET /bootstrap.zip.zst — the zstd-compressed bootstrap archive, served as
/// raw bytes (there is no transparent decompression over KPS streams; clients
/// decompress themselves). Includes `X-Decompressed-Content-Length` with the
/// uncompressed zip size for download progress. Supports ETag/304.
async fn handle_bootstrap_zip_zst(
    State(gw): State<Arc<Gateway>>,
    headers: HeaderMap,
) -> Response {
    let etag = read_etag(&gw.data_dir).await;
    if let Some(ref etag) = etag {
        if let Some(not_modified) = check_not_modified(&headers, etag) {
            return not_modified;
        }
    }
    let data = match tokio::fs::read(gw.data_dir.join("bootstrap.zip.zst")).await {
        Ok(d) => d,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    // Decompressed size from the uncompressed zip on disk.
    let decompressed_len = tokio::fs::metadata(gw.data_dir.join("bootstrap.zip"))
        .await
        .map(|m| m.len().to_string())
        .unwrap_or_default();
    let mut res = (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/octet-stream")],
        data,
    )
        .into_response();
    if !decompressed_len.is_empty() {
        res.headers_mut().insert(
            "x-decompressed-content-length",
            decompressed_len.parse().unwrap(),
        );
    }
    if let Some(etag) = etag {
        res.headers_mut().insert(header::ETAG, etag.parse().unwrap());
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lower_hex_validation() {
        assert!(is_lower_hex(&"a".repeat(62), 62));
        assert!(is_lower_hex("0f", 2));
        assert!(!is_lower_hex("0F", 2)); // uppercase rejected
        assert!(!is_lower_hex(&"a".repeat(61), 62));
        assert!(!is_lower_hex(&"a".repeat(63), 62));
        assert!(!is_lower_hex(&"g".repeat(62), 62));
    }

    #[test]
    fn keccak256_is_keccak_not_sha3() {
        // keccak256("") — the Ethereum constant, distinct from sha3-256("").
        assert_eq!(
            keccak256_hex(b""),
            "c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470"
        );
    }
}
