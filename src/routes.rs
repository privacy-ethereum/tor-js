//! The gateway's HTTP routes, served per-stream over KPS (PROTOCOL.md §5).
//!
//! axum stays as the routing library: a `Router` is a tower `Service`, called
//! once per KPS stream by `kps_server` without any TCP listener behind it.

use std::collections::HashMap;
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use bytes::Bytes;

use crate::tunnel::{ConnectionTracker, RelayAllowlist, TunnelLimits};

/// Shared server state: everything the routes and the CONNECT handler need.
pub struct Gateway {
    pub data_dir: PathBuf,
    pub relay_allowlist: RelayAllowlist,
    pub tracker: ConnectionTracker,
    pub limits: TunnelLimits,
    pub has_ipv6: bool,
    /// keccak256-hex → verified bundle bytes, loaded at startup.
    pub bundles: HashMap<String, Bytes>,
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

/// Loads worker bundles from `dir`: files named `<64-hex>.js` whose
/// keccak256(bytes) equals the filename. Mismatches are refused and logged
/// loudly; other files are ignored with a warning.
pub fn load_worker_bundles(dir: &FsPath) -> Result<HashMap<String, Bytes>> {
    let mut bundles = HashMap::new();
    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("reading worker_bundles_dir {}", dir.display()))?;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(hash) = name.strip_suffix(".js").filter(|h| is_keccak_hex(h)) else {
            tracing::warn!(
                "worker bundles: ignoring {} (not named <64-lowercase-hex>.js)",
                name
            );
            continue;
        };
        let bytes = std::fs::read(entry.path())
            .with_context(|| format!("reading {}", entry.path().display()))?;
        let actual = keccak256_hex(&bytes);
        if actual != hash {
            tracing::error!(
                "worker bundles: REFUSING {} — keccak256 of contents is {}, filename says {}",
                entry.path().display(),
                actual,
                hash,
            );
            continue;
        }
        bundles.insert(hash.to_string(), Bytes::from(bytes));
    }
    tracing::info!("worker bundles: serving {} verified bundle(s) from {}", bundles.len(), dir.display());
    Ok(bundles)
}

fn is_keccak_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
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
        .route("/bootstrap.zip.br", get(handle_bootstrap_zip_br))
        .route("/worker/{file}", get(handle_worker_bundle))
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

/// GET /worker/{keccak-hex}.js — immutable, hash-verified worker bundles.
async fn handle_worker_bundle(
    State(gw): State<Arc<Gateway>>,
    Path(file): Path<String>,
) -> Response {
    let Some(hash) = file.strip_suffix(".js").filter(|h| is_keccak_hex(h)) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match gw.bundles.get(hash) {
        Some(bytes) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "text/javascript"),
                (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
            ],
            bytes.clone(),
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
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
async fn read_etag(dir: &FsPath) -> Option<String> {
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

/// GET /bootstrap.zip.br — the brotli-compressed bootstrap archive, served as
/// raw bytes (there is no transparent decompression over KPS streams; clients
/// decompress themselves). Includes `X-Decompressed-Content-Length` with the
/// uncompressed zip size for download progress. Supports ETag/304.
async fn handle_bootstrap_zip_br(
    State(gw): State<Arc<Gateway>>,
    headers: HeaderMap,
) -> Response {
    let etag = read_etag(&gw.data_dir).await;
    if let Some(ref etag) = etag {
        if let Some(not_modified) = check_not_modified(&headers, etag) {
            return not_modified;
        }
    }
    let data = match tokio::fs::read(gw.data_dir.join("bootstrap.zip.br")).await {
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
    fn keccak_hex_validation() {
        assert!(is_keccak_hex(&"a".repeat(64)));
        assert!(is_keccak_hex(&"0123456789abcdef".repeat(4)));
        assert!(!is_keccak_hex(&"A".repeat(64))); // uppercase rejected
        assert!(!is_keccak_hex(&"a".repeat(63)));
        assert!(!is_keccak_hex(&"a".repeat(65)));
        assert!(!is_keccak_hex(&"g".repeat(64)));
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
