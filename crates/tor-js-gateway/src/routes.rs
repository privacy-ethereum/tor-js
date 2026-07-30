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

    // ---- metadata --------------------------------------------------------

    #[test]
    fn metadata_advertises_worker_bundles_only_when_enabled() {
        let addrs = vec!["1.2.3.4:12298:uEiAxk".to_string()];
        let doc: serde_json::Value = serde_json::from_str(&build_metadata(&addrs, false)).unwrap();
        assert_eq!(doc["protocol"], "kps-http/1");
        assert_eq!(doc["software"], "tor-js-gateway");
        assert_eq!(doc["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(doc["addresses"], serde_json::json!(addrs));
        assert_eq!(
            doc["capabilities"],
            serde_json::json!(["metadata", "bootstrap", "connect", "relay-random"])
        );

        let doc: serde_json::Value = serde_json::from_str(&build_metadata(&addrs, true)).unwrap();
        assert_eq!(
            doc["capabilities"],
            serde_json::json!(["metadata", "bootstrap", "connect", "relay-random", "worker-bundles"])
        );
    }

    // ---- conditional requests -------------------------------------------

    fn if_none_match(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::IF_NONE_MATCH, value.parse().unwrap());
        headers
    }

    #[test]
    fn matching_etag_is_a_304() {
        let etag = "\"abc123\"";
        let res = check_not_modified(&if_none_match(etag), etag).unwrap();
        assert_eq!(res.status(), StatusCode::NOT_MODIFIED);
        // `*` matches any existing representation (RFC 9110 §13.1.2).
        let res = check_not_modified(&if_none_match("*"), etag).unwrap();
        assert_eq!(res.status(), StatusCode::NOT_MODIFIED);
    }

    #[test]
    fn no_header_or_a_different_etag_is_not_a_304() {
        let etag = "\"abc123\"";
        assert!(check_not_modified(&HeaderMap::new(), etag).is_none());
        assert!(check_not_modified(&if_none_match("\"other\""), etag).is_none());
        // Unquoted values do not match the quoted ETag we emit.
        assert!(check_not_modified(&if_none_match("abc123"), etag).is_none());
    }

    /// The comparison is exact by choice: clients here are tor-js, which echoes
    /// back the ETag verbatim. Comma lists and weak validators therefore just
    /// miss the cache (a 200), which is safe — never a false 304.
    #[test]
    fn comma_lists_and_weak_validators_miss_rather_than_match() {
        let etag = "\"abc123\"";
        assert!(check_not_modified(&if_none_match("\"abc123\", \"other\""), etag).is_none());
        assert!(check_not_modified(&if_none_match("W/\"abc123\""), etag).is_none());
    }

    #[test]
    fn a_non_ascii_header_is_ignored_not_an_error() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::IF_NONE_MATCH,
            axum::http::HeaderValue::from_bytes(&[0xff, 0xfe]).unwrap(),
        );
        assert!(check_not_modified(&headers, "\"abc123\"").is_none());
    }

    // ---- routes ----------------------------------------------------------

    use crate::testutil::TempDir;

    fn gateway(data_dir: &TempDir, relays: &[&str], has_ipv6: bool) -> Arc<Gateway> {
        Arc::new(Gateway {
            data_dir: data_dir.path().to_path_buf(),
            relay_allowlist: Arc::new(RwLock::new(
                relays.iter().map(|r| r.parse().unwrap()).collect(),
            )),
            tracker: ConnectionTracker::new(),
            limits: TunnelLimits::default(),
            has_ipv6,
            keccak_dir: PathBuf::new(),
            keccak_enabled: false,
            verified_bundles: RwLock::new(HashSet::new()),
            metadata_json: build_metadata(&[], false),
        })
    }

    async fn body_string(res: Response) -> String {
        let bytes = axum::body::to_bytes(res.into_body(), 64 * 1024 * 1024).await.unwrap();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[tokio::test]
    async fn random_relay_needs_a_non_empty_allowlist() {
        let dir = TempDir::new("routes-relay");
        let gw = gateway(&dir, &[], true);
        let res = handle_random_relay(State(gw)).await;
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn random_relay_skips_ipv6_without_ipv6_connectivity() {
        let dir = TempDir::new("routes-relay-v6");
        let relays = ["185.220.101.1:9001", "[2606:4700::1111]:9001"];

        // No IPv6: the v6 relay is never handed out, however many draws.
        let gw = gateway(&dir, &relays, false);
        for _ in 0..50 {
            let res = handle_random_relay(State(gw.clone())).await;
            assert_eq!(res.status(), StatusCode::OK);
            assert_eq!(body_string(res).await, "185.220.101.1:9001");
        }

        // An allowlist of only v6 relays therefore has nothing to offer.
        let gw = gateway(&dir, &relays[1..], false);
        assert_eq!(
            handle_random_relay(State(gw)).await.status(),
            StatusCode::SERVICE_UNAVAILABLE
        );

        // With IPv6, both are eligible.
        let gw = gateway(&dir, &relays, true);
        let mut seen = HashSet::new();
        for _ in 0..200 {
            let res = handle_random_relay(State(gw.clone())).await;
            seen.insert(body_string(res).await);
        }
        assert_eq!(seen.len(), 2, "both relays should be drawn: {seen:?}");
    }

    #[tokio::test]
    async fn bootstrap_is_503_until_the_archive_exists() {
        let dir = TempDir::new("routes-bootstrap-missing");
        let gw = gateway(&dir, &[], true);
        let res = handle_bootstrap_zip_zst(State(gw), HeaderMap::new()).await;
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn bootstrap_serves_bytes_with_an_etag_and_decompressed_length() {
        let dir = TempDir::new("routes-bootstrap");
        std::fs::write(dir.join("bootstrap.zip.zst"), b"compressed-bytes").unwrap();
        std::fs::write(dir.join("bootstrap.zip"), vec![0u8; 4096]).unwrap();
        let gw = gateway(&dir, &[], true);

        let res = handle_bootstrap_zip_zst(State(gw.clone()), HeaderMap::new()).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/octet-stream"
        );
        assert_eq!(
            res.headers().get("x-decompressed-content-length").unwrap(),
            "4096",
            "progress hint comes from the uncompressed zip on disk"
        );
        let etag = res.headers().get(header::ETAG).unwrap().to_str().unwrap().to_string();
        assert_eq!(body_string(res).await, "compressed-bytes");

        // The ETag is derived from bootstrap.zip and cached to disk on first use.
        let cached = std::fs::read_to_string(dir.join("bootstrap.etag")).unwrap();
        assert_eq!(etag, format!("\"{cached}\""));
        assert_eq!(cached.len(), 64, "hex sha3-256");

        // Echoing it back gets a 304 with no body.
        let res = handle_bootstrap_zip_zst(State(gw.clone()), if_none_match(&etag)).await;
        assert_eq!(res.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(body_string(res).await, "");

        // A stale validator gets the bytes.
        let res = handle_bootstrap_zip_zst(State(gw), if_none_match("\"stale\"")).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(body_string(res).await, "compressed-bytes");
    }

    /// The archive can be replaced while the gateway runs; the ETag must follow
    /// it rather than pin the first value seen.
    #[tokio::test]
    async fn a_new_archive_gets_a_new_etag() {
        let dir = TempDir::new("routes-bootstrap-rotate");
        std::fs::write(dir.join("bootstrap.zip.zst"), b"v1").unwrap();
        std::fs::write(dir.join("bootstrap.zip"), b"v1-uncompressed").unwrap();
        let gw = gateway(&dir, &[], true);

        let first = handle_bootstrap_zip_zst(State(gw.clone()), HeaderMap::new()).await;
        let first_etag = first.headers().get(header::ETAG).unwrap().to_str().unwrap().to_string();

        // sync.rs rewrites both the archive and the etag file together.
        std::fs::write(dir.join("bootstrap.zip.zst"), b"v2").unwrap();
        std::fs::write(dir.join("bootstrap.zip"), b"v2-uncompressed-longer").unwrap();
        std::fs::remove_file(dir.join("bootstrap.etag")).unwrap();

        let second = handle_bootstrap_zip_zst(State(gw.clone()), HeaderMap::new()).await;
        let second_etag = second.headers().get(header::ETAG).unwrap().to_str().unwrap().to_string();
        assert_ne!(first_etag, second_etag);

        // The old validator no longer matches.
        let res = handle_bootstrap_zip_zst(State(gw), if_none_match(&first_etag)).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(body_string(res).await, "v2");
    }

    /// Missing bootstrap.zip means no progress hint, but the archive still
    /// serves — the header is optional.
    #[tokio::test]
    async fn bootstrap_without_the_uncompressed_zip_still_serves() {
        let dir = TempDir::new("routes-bootstrap-nozip");
        std::fs::write(dir.join("bootstrap.zip.zst"), b"only-compressed").unwrap();
        let gw = gateway(&dir, &[], true);
        let res = handle_bootstrap_zip_zst(State(gw), HeaderMap::new()).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert!(res.headers().get("x-decompressed-content-length").is_none());
        assert!(res.headers().get(header::ETAG).is_none(), "no source for an etag");
        assert_eq!(body_string(res).await, "only-compressed");
    }

    // ---- worker bundles --------------------------------------------------

    #[tokio::test]
    async fn worker_bundles_are_404_when_disabled() {
        let dir = TempDir::new("routes-keccak-off");
        let gw = gateway(&dir, &[], true);
        let hash = keccak256_hex(b"payload");
        let res =
            handle_worker_bundle(State(gw), Path((hash[..2].into(), hash[2..].into()))).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    fn keccak_gateway(dir: &TempDir) -> Arc<Gateway> {
        let gw = gateway(dir, &[], true);
        Arc::new(Gateway {
            data_dir: gw.data_dir.clone(),
            relay_allowlist: gw.relay_allowlist.clone(),
            tracker: gw.tracker.clone(),
            limits: gw.limits.clone(),
            has_ipv6: true,
            keccak_dir: dir.path().to_path_buf(),
            keccak_enabled: true,
            verified_bundles: RwLock::new(HashSet::new()),
            metadata_json: gw.metadata_json.clone(),
        })
    }

    #[tokio::test]
    async fn a_bundle_whose_contents_do_not_match_its_path_is_refused() {
        let dir = TempDir::new("routes-keccak");
        let gw = keccak_gateway(&dir);

        let payload = b"console.log(1)";
        let hash = keccak256_hex(payload);
        let (prefix, rest) = (hash[..2].to_string(), hash[2..].to_string());
        std::fs::create_dir_all(dir.join(&prefix)).unwrap();
        let path = dir.join(&prefix).join(&rest);

        // Honest content verifies and is cached.
        std::fs::write(&path, payload).unwrap();
        let res =
            handle_worker_bundle(State(gw.clone()), Path((prefix.clone(), rest.clone()))).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers().get(header::CACHE_CONTROL).unwrap(),
            "public, max-age=31536000, immutable"
        );
        assert_eq!(body_string(res).await, "console.log(1)");
        assert!(gw.verified_bundles.read().unwrap().contains(&hash));

        // Content swapped under a hash that has *not* been verified is refused.
        let other = TempDir::new("routes-keccak-swap");
        let gw2 = keccak_gateway(&other);
        std::fs::create_dir_all(other.join(&prefix)).unwrap();
        std::fs::write(other.join(&prefix).join(&rest), b"malicious()").unwrap();
        let res = handle_worker_bundle(State(gw2), Path((prefix.clone(), rest.clone()))).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        // Non-hex path segments are rejected before touching the filesystem,
        // which is also what keeps the join traversal-safe.
        for (p, r) in [
            ("..".to_string(), rest.clone()),
            (prefix.clone(), "../".repeat(31)),
            (prefix.clone(), "A".repeat(62)),
            (prefix.clone(), "a".repeat(61)),
        ] {
            let res = handle_worker_bundle(State(gw.clone()), Path((p.clone(), r.clone()))).await;
            assert_eq!(res.status(), StatusCode::NOT_FOUND, "{p}/{r}");
        }
    }
}
