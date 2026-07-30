//! CONNECT-over-KPS TCP tunneling (PROTOCOL.md §4).
//!
//! A `CONNECT <ip>:<port>` exchange on a KPS stream turns the stream into a
//! raw byte pipe to the target TCP address. This replaces the WebSocket and
//! WebRTC relays: the KPS stream *is* the pipe, no framing layer.
//!
//! Only targets present in the current consensus relay allowlist are
//! permitted. Local/private IPs are also rejected as a defence-in-depth
//! measure.
//!
//! Tunnels are subject to configurable limits: max total, per client IP,
//! per KPS connection, idle timeout, and max lifetime.
//!
//! Lifecycle mapping (PROTOCOL.md §4): client FIN → TCP FIN to the target and
//! target FIN → stream FIN; an abort on either side (reset, error, timeout)
//! tears the other side down abortively (stream reset / TCP RST).

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, Response, StatusCode};
use hyper::body::Incoming;
use hyper::upgrade::Upgraded;
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, info, warn};

/// Shared set of relay `SocketAddr`s from the current consensus.
pub type RelayAllowlist = Arc<RwLock<HashSet<SocketAddr>>>;

/// TCP dial timeout for CONNECT targets (→ `504` on expiry).
const DIAL_TIMEOUT: Duration = Duration::from_secs(10);

/// Configuration for tunnel limits.
#[derive(Clone, Debug)]
pub struct TunnelLimits {
    pub max_tunnels: usize,
    pub per_ip: usize,
    /// Cap per KPS connection (one client can hold several connections, each
    /// capped here; the per-IP cap bounds the total).
    pub per_conn: usize,
    pub idle_timeout: Duration,
    pub max_lifetime: Duration,
}

impl Default for TunnelLimits {
    fn default() -> Self {
        Self {
            max_tunnels: 8192,
            per_ip: 16,
            per_conn: 16,
            idle_timeout: Duration::from_secs(300),
            max_lifetime: Duration::from_secs(3600),
        }
    }
}

/// Tracks active tunnels globally and per client IP.
#[derive(Clone)]
pub struct ConnectionTracker {
    total: Arc<AtomicUsize>,
    per_ip: Arc<Mutex<HashMap<IpAddr, usize>>>,
}

impl ConnectionTracker {
    pub fn new() -> Self {
        Self {
            total: Arc::new(AtomicUsize::new(0)),
            per_ip: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Try to acquire a tunnel slot; `conn_count` is the owning KPS
    /// connection's tunnel counter. Returns a guard that releases the slot on
    /// drop, or `None` if a limit would be exceeded.
    fn acquire(
        &self,
        ip: IpAddr,
        conn_count: &Arc<AtomicUsize>,
        limits: &TunnelLimits,
    ) -> Option<TunnelGuard> {
        if self.total.load(Ordering::Relaxed) >= limits.max_tunnels {
            return None;
        }
        if conn_count.load(Ordering::Relaxed) >= limits.per_conn {
            return None;
        }

        {
            let mut map = self.per_ip.lock().unwrap_or_else(|e| e.into_inner());
            let count = map.entry(ip).or_insert(0);
            if *count >= limits.per_ip {
                return None;
            }
            *count += 1;
        }
        self.total.fetch_add(1, Ordering::Relaxed);
        conn_count.fetch_add(1, Ordering::Relaxed);
        Some(TunnelGuard { tracker: self.clone(), ip, conn_count: conn_count.clone() })
    }

    fn release(&self, ip: IpAddr) {
        self.total.fetch_sub(1, Ordering::Relaxed);
        let mut map = self.per_ip.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(count) = map.get_mut(&ip) {
            *count -= 1;
            if *count == 0 {
                map.remove(&ip);
            }
        }
    }
}

/// Releases a tunnel slot (global, per-IP, per-connection) on drop.
pub struct TunnelGuard {
    tracker: ConnectionTracker,
    ip: IpAddr,
    conn_count: Arc<AtomicUsize>,
}

impl Drop for TunnelGuard {
    fn drop(&mut self) {
        self.tracker.release(self.ip);
        self.conn_count.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Returns true if the IP is non-routable (loopback, private, link-local, etc.).
///
/// Both families delegate to one predicate per family, and the v6 arm runs the
/// v4 predicate on embedded v4 addresses, so the two arms cannot drift apart.
pub(crate) fn is_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_local_v4(v4),
        IpAddr::V6(v6) => is_local_v6(v6),
    }
}

fn is_local_v4(v4: Ipv4Addr) -> bool {
    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_broadcast()
        || v4.is_unspecified()
        // Never a relay, and 224.0.0.0/24 reaches the local segment.
        || v4.is_multicast()
}

fn is_local_v6(v6: Ipv6Addr) -> bool {
    // `to_ipv4` covers both v4-mapped (`::ffff:a.b.c.d`) and the deprecated
    // v4-compatible (`::a.b.c.d`) forms, either of which some stacks will
    // route to the embedded v4 address.
    if v6.to_ipv4().is_some_and(is_local_v4) {
        return true;
    }
    v6.is_loopback()
        || v6.is_unspecified()
        // fe80::/10
        || v6.is_unicast_link_local()
        // fc00::/7
        || v6.is_unique_local()
        || v6.is_multicast()
}

/// Why a `CONNECT` target was refused: the status and body to reply with.
type Refusal = (StatusCode, &'static str);

/// Validates a `CONNECT` target against the authority form, this server's IPv6
/// connectivity, the local-address ban and the relay allowlist.
///
/// Split out of [`handle_connect`] so the ladder is testable without a live
/// gateway; the tunnel limits stay behind, since acquiring a slot has to
/// produce a guard rather than a verdict.
fn validate_target(
    authority: Option<&str>,
    has_ipv6: bool,
    allow_local: bool,
    allowlist: &HashSet<SocketAddr>,
) -> Result<SocketAddr, Refusal> {
    // Authority-form target: `<ip>:<port>`, IPv6 bracketed. Names are not
    // permitted (targets are relay IPs from the consensus).
    let target: SocketAddr = authority
        .and_then(|a| a.parse().ok())
        .ok_or((StatusCode::BAD_REQUEST, "invalid target address"))?;

    if target.is_ipv6() && !has_ipv6 {
        return Err((
            StatusCode::BAD_REQUEST,
            "IPv6 targets are not supported on this server",
        ));
    }

    if is_local(target.ip()) && !allow_local {
        return Err((
            StatusCode::FORBIDDEN,
            "connections to local addresses are forbidden",
        ));
    }

    if !allowlist.contains(&target) {
        return Err((
            StatusCode::FORBIDDEN,
            "target is not an advertised Tor relay",
        ));
    }

    Ok(target)
}

/// Test-only escape hatch: lets integration tests tunnel to targets on
/// loopback (they still must be present in the relay allowlist).
fn allow_local_targets() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("TOR_JS_GATEWAY_ALLOW_LOCAL_TARGETS")
            .map(|v| !v.is_empty() && v != "0")
            .unwrap_or(false)
    })
}

fn text_response(status: StatusCode, msg: &str) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .body(Body::from(msg.to_string()))
        .unwrap()
}

/// Handles a `CONNECT <ip>:<port>` request on a KPS stream: validate the
/// target, dial it, reply `200`, then relay bytes between the upgraded stream
/// and the TCP socket until both directions close.
pub async fn handle_connect(req: Request<Incoming>, ctx: &crate::kps_server::ConnCtx) -> Response<Body> {
    debug_assert_eq!(req.method(), Method::CONNECT);
    let gw = &ctx.gateway;
    let peer_ip = ctx.remote.ip();

    let target = {
        let allowlist = gw.relay_allowlist.read().unwrap_or_else(|e| e.into_inner());
        validate_target(
            req.uri().authority().map(|a| a.as_str()),
            gw.has_ipv6,
            allow_local_targets(),
            &allowlist,
        )
    };
    let target = match target {
        Ok(target) => target,
        Err((status, msg)) => {
            warn!("CONNECT: rejected '{}': {} ({})", req.uri(), msg, status);
            return text_response(status, msg);
        }
    };

    let Some(guard) = gw.tracker.acquire(peer_ip, &ctx.conn_tunnels, &gw.limits) else {
        warn!("CONNECT: tunnel limit reached for {}", peer_ip);
        return text_response(StatusCode::TOO_MANY_REQUESTS, "tunnel limit reached");
    };

    let tcp = match tokio::time::timeout(DIAL_TIMEOUT, TcpStream::connect(target)).await {
        Ok(Ok(tcp)) => tcp,
        Ok(Err(e)) => {
            warn!("CONNECT: dial {} failed: {}", target, e);
            return text_response(StatusCode::BAD_GATEWAY, "failed to reach target");
        }
        Err(_) => {
            warn!("CONNECT: dial {} timed out", target);
            return text_response(StatusCode::GATEWAY_TIMEOUT, "timed out reaching target");
        }
    };
    info!("CONNECT: tunnel to {} for {}", target, peer_ip);

    let idle_timeout = gw.limits.idle_timeout;
    let max_lifetime = gw.limits.max_lifetime;
    tokio::spawn(async move {
        let _guard = guard;
        match hyper::upgrade::on(req).await {
            Ok(upgraded) => relay(upgraded, tcp, target, idle_timeout, max_lifetime).await,
            Err(e) => debug!("CONNECT: upgrade to {} failed: {}", target, e),
        }
    });

    // 200 with an empty body and no Content-Length/Transfer-Encoding; hyper
    // treats 2xx-to-CONNECT as headers-only and emits no framing headers.
    Response::builder().status(StatusCode::OK).body(Body::empty()).unwrap()
}

/// Why a tunnel ended abortively.
enum Abort {
    /// No bytes in one direction for the idle timeout.
    Idle,
    /// Max lifetime reached.
    Lifetime,
    /// KPS stream error (peer reset or connection loss).
    Kps(std::io::Error),
    /// TCP error from the target side.
    Tcp(std::io::Error),
}

/// Relays bytes both ways between the upgraded KPS stream and the TCP socket,
/// applying the §4 lifecycle mapping.
async fn relay(
    upgraded: Upgraded,
    mut tcp: TcpStream,
    target: SocketAddr,
    idle_timeout: Duration,
    max_lifetime: Duration,
) {
    // Recover the KPS stream from hyper so aborts can reset it. `read_buf`
    // holds tunnel bytes the client sent before hyper handed the stream over;
    // they must reach the target first.
    let parts = match upgraded.downcast::<TokioIo<Box<dyn kps::Stream>>>() {
        Ok(parts) => parts,
        Err(_) => {
            warn!("CONNECT: upgraded IO was not a KPS stream");
            return;
        }
    };
    let mut stream = parts.io.into_inner();

    let result = relay_inner(
        &mut stream,
        &mut tcp,
        parts.read_buf,
        idle_timeout,
        max_lifetime,
    )
    .await;

    match result {
        Ok(()) => {
            debug!("tunnel to {} done", target);
            let _ = stream.close().await;
        }
        Err(abort) => {
            let (code, why) = match abort {
                Abort::Idle => (kps::ErrorCode::Timeout, "idle timeout".to_string()),
                Abort::Lifetime => (kps::ErrorCode::Timeout, "max lifetime reached".to_string()),
                Abort::Kps(e) => (kps::ErrorCode::Reset, format!("stream error: {e}")),
                Abort::Tcp(e) => (kps::ErrorCode::NetworkError, format!("tcp error: {e}")),
            };
            debug!("tunnel to {} aborted: {}", target, why);
            // Abort ↔ abortive close, both directions (§4): reset the KPS
            // stream and RST the TCP side (SO_LINGER 0 turns close into RST).
            // tokio deprecated set_linger because a *nonzero* linger blocks
            // the thread on drop; linger 0 closes immediately.
            let _ = stream.close_with_error(code).await;
            #[allow(deprecated)]
            let _ = tcp.set_linger(Some(Duration::ZERO));
        }
    }
}

async fn relay_inner(
    stream: &mut Box<dyn kps::Stream>,
    tcp: &mut TcpStream,
    leftover: bytes::Bytes,
    idle_timeout: Duration,
    max_lifetime: Duration,
) -> Result<(), Abort> {
    if !leftover.is_empty() {
        tcp.write_all(&leftover).await.map_err(Abort::Tcp)?;
    }

    let deadline = tokio::time::Instant::now() + max_lifetime;
    let (mut kps_read, mut kps_write) = tokio::io::split(&mut *stream);
    let (mut tcp_read, mut tcp_write) = tcp.split();

    // KPS → TCP: client FIN becomes TCP FIN.
    let kps_to_tcp = async {
        let mut buf = vec![0u8; 16384];
        loop {
            let n = tokio::time::timeout(idle_timeout, kps_read.read(&mut buf))
                .await
                .map_err(|_| Abort::Idle)?
                .map_err(Abort::Kps)?;
            if n == 0 {
                let _ = tcp_write.shutdown().await;
                return Ok(());
            }
            tcp_write.write_all(&buf[..n]).await.map_err(Abort::Tcp)?;
        }
    };

    // TCP → KPS: target FIN becomes stream FIN (close_write).
    let tcp_to_kps = async {
        let mut buf = vec![0u8; 16384];
        loop {
            let n = tokio::time::timeout(idle_timeout, tcp_read.read(&mut buf))
                .await
                .map_err(|_| Abort::Idle)?
                .map_err(Abort::Tcp)?;
            if n == 0 {
                let _ = kps_write.shutdown().await;
                return Ok(());
            }
            kps_write.write_all(&buf[..n]).await.map_err(Abort::Kps)?;
        }
    };

    tokio::select! {
        r = async { tokio::try_join!(kps_to_tcp, tcp_to_kps) } => r.map(|_| ()),
        _ = tokio::time::sleep_until(deadline) => Err(Abort::Lifetime),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    /// Every address a `CONNECT` must never reach, by family and notation.
    #[test]
    fn local_addresses_are_rejected() {
        let local = [
            // IPv4
            "127.0.0.1",
            "127.255.255.254",
            "10.0.0.1",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254", // cloud metadata service
            "255.255.255.255",
            "0.0.0.0",
            "224.0.0.1",
            // IPv6
            "::1",
            "::",
            "fe80::1",
            "febf:ffff::1", // top of fe80::/10
            "fc00::1",
            "fd00::1",
            "fdff:ffff::1", // top of fc00::/7
            "ff02::1",
            // IPv4-mapped: the embedded address decides.
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "::ffff:192.168.0.1",
            "::ffff:169.254.169.254",
            "::ffff:255.255.255.255",
            "::ffff:0.0.0.0",
            // Deprecated IPv4-compatible form.
            "::127.0.0.1",
            "::10.0.0.1",
        ];
        for addr in local {
            assert!(is_local(ip(addr)), "{addr} should be rejected as local");
        }
    }

    #[test]
    fn public_addresses_are_allowed() {
        let public = [
            "1.1.1.1",
            "8.8.8.8",
            "185.220.101.1",   // a real Tor relay range
            "172.32.0.1",      // just outside 172.16.0.0/12
            "169.253.0.1",     // just outside 169.254.0.0/16
            "223.255.255.255", // just below 224.0.0.0/4
            "2606:4700:4700::1111",
            "2a01:4f8::1",
            "::ffff:1.1.1.1",
            "fe00::1", // just below fe80::/10
            "fb00::1", // just below fc00::/7
        ];
        for addr in public {
            assert!(!is_local(ip(addr)), "{addr} should be allowed");
        }
    }

    fn allowlist(addrs: &[&str]) -> HashSet<SocketAddr> {
        addrs.iter().map(|a| a.parse().unwrap()).collect()
    }

    #[test]
    fn malformed_authority_is_a_400() {
        let list = allowlist(&["1.2.3.4:443"]);
        // Names are not permitted, and neither is a missing port.
        for authority in [None, Some(""), Some("example.com:443"), Some("1.2.3.4"), Some("::1")] {
            let err = validate_target(authority, true, false, &list).unwrap_err();
            assert_eq!(err, (StatusCode::BAD_REQUEST, "invalid target address"), "{authority:?}");
        }
    }

    #[test]
    fn ipv6_target_needs_ipv6_connectivity() {
        let list = allowlist(&["[2606:4700::1111]:443"]);
        let err = validate_target(Some("[2606:4700::1111]:443"), false, false, &list).unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(validate_target(Some("[2606:4700::1111]:443"), true, false, &list).is_ok());
    }

    #[test]
    fn local_target_is_a_403_even_when_allowlisted() {
        // The local ban is defence in depth: it must hold even if a poisoned
        // consensus put a local address in the allowlist.
        let list = allowlist(&["127.0.0.1:9001"]);
        let err = validate_target(Some("127.0.0.1:9001"), true, false, &list).unwrap_err();
        assert_eq!(
            err,
            (StatusCode::FORBIDDEN, "connections to local addresses are forbidden")
        );
        // The escape hatch lifts only that check, not the allowlist.
        assert!(validate_target(Some("127.0.0.1:9001"), true, true, &list).is_ok());
        assert_eq!(
            validate_target(Some("127.0.0.1:9002"), true, true, &list).unwrap_err(),
            (StatusCode::FORBIDDEN, "target is not an advertised Tor relay")
        );
    }

    #[test]
    fn non_relay_target_is_a_403() {
        let list = allowlist(&["185.220.101.1:9001"]);
        assert_eq!(
            validate_target(Some("8.8.8.8:53"), true, false, &list).unwrap_err(),
            (StatusCode::FORBIDDEN, "target is not an advertised Tor relay")
        );
        // Same IP, wrong port: the allowlist is by socket address, not host.
        assert_eq!(
            validate_target(Some("185.220.101.1:9002"), true, false, &list).unwrap_err(),
            (StatusCode::FORBIDDEN, "target is not an advertised Tor relay")
        );
        assert_eq!(
            validate_target(Some("185.220.101.1:9001"), true, false, &list).unwrap(),
            "185.220.101.1:9001".parse::<SocketAddr>().unwrap()
        );
    }

    /// The ladder's order is observable through which message comes back, and
    /// the cheap checks must run before the lock-taking one.
    #[test]
    fn earlier_rungs_win() {
        let empty = allowlist(&[]);
        // Unparseable beats everything.
        assert_eq!(
            validate_target(Some("nonsense"), false, false, &empty).unwrap_err().0,
            StatusCode::BAD_REQUEST
        );
        // A local IPv6 target with no IPv6 connectivity reports the family, not
        // the locality.
        assert_eq!(
            validate_target(Some("[::1]:9001"), false, false, &empty).unwrap_err(),
            (
                StatusCode::BAD_REQUEST,
                "IPv6 targets are not supported on this server"
            )
        );
        // Locality beats the allowlist.
        assert_eq!(
            validate_target(Some("10.0.0.1:9001"), true, false, &empty).unwrap_err().0,
            StatusCode::FORBIDDEN
        );
    }

    fn limits(max_tunnels: usize, per_ip: usize, per_conn: usize) -> TunnelLimits {
        TunnelLimits { max_tunnels, per_ip, per_conn, ..TunnelLimits::default() }
    }

    #[test]
    fn per_ip_cap_is_enforced_and_released() {
        let tracker = ConnectionTracker::new();
        let limits = limits(100, 2, 100);
        let conn = Arc::new(AtomicUsize::new(0));
        let a = ip("1.1.1.1");

        let g1 = tracker.acquire(a, &conn, &limits).expect("first slot");
        let g2 = tracker.acquire(a, &conn, &limits).expect("second slot");
        assert!(tracker.acquire(a, &conn, &limits).is_none(), "third exceeds per_ip");

        // A different client IP has its own budget.
        let other = tracker.acquire(ip("2.2.2.2"), &conn, &limits).expect("other ip");

        drop(g1);
        assert!(tracker.acquire(a, &conn, &limits).is_some(), "slot freed on drop");
        drop((g2, other));
    }

    #[test]
    fn per_conn_cap_is_enforced_per_connection() {
        let tracker = ConnectionTracker::new();
        let limits = limits(100, 100, 1);
        let conn_a = Arc::new(AtomicUsize::new(0));
        let conn_b = Arc::new(AtomicUsize::new(0));
        let client = ip("1.1.1.1");

        let g = tracker.acquire(client, &conn_a, &limits).expect("first on conn a");
        assert!(
            tracker.acquire(client, &conn_a, &limits).is_none(),
            "second on conn a exceeds per_conn"
        );
        // Same client, second KPS connection: capped separately.
        let other = tracker.acquire(client, &conn_b, &limits).expect("first on conn b");
        assert_eq!(conn_a.load(Ordering::Relaxed), 1);
        assert_eq!(conn_b.load(Ordering::Relaxed), 1);

        drop(g);
        assert_eq!(conn_a.load(Ordering::Relaxed), 0, "guard decrements its own connection");
        assert_eq!(conn_b.load(Ordering::Relaxed), 1, "and only its own");
        drop(other);
    }

    #[test]
    fn global_cap_is_enforced_across_ips() {
        let tracker = ConnectionTracker::new();
        let limits = limits(2, 100, 100);
        let conn = Arc::new(AtomicUsize::new(0));

        let g1 = tracker.acquire(ip("1.1.1.1"), &conn, &limits).unwrap();
        let g2 = tracker.acquire(ip("2.2.2.2"), &conn, &limits).unwrap();
        assert!(
            tracker.acquire(ip("3.3.3.3"), &conn, &limits).is_none(),
            "third exceeds max_tunnels regardless of client"
        );
        drop(g1);
        assert!(tracker.acquire(ip("3.3.3.3"), &conn, &limits).is_some());
        drop(g2);
    }

    /// Per-IP counters must not accumulate empty entries, or a gateway serving
    /// many short-lived clients leaks the map.
    #[test]
    fn per_ip_entries_are_removed_at_zero() {
        let tracker = ConnectionTracker::new();
        let limits = TunnelLimits::default();
        let conn = Arc::new(AtomicUsize::new(0));

        let g = tracker.acquire(ip("1.1.1.1"), &conn, &limits).unwrap();
        assert_eq!(tracker.per_ip.lock().unwrap().len(), 1);
        drop(g);
        assert!(tracker.per_ip.lock().unwrap().is_empty());
        assert_eq!(tracker.total.load(Ordering::Relaxed), 0);
    }
}
