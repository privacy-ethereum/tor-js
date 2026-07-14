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
use std::net::{IpAddr, SocketAddr};
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
pub(crate) fn is_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                // ::ffff:127.0.0.1 etc.
                || v6.to_ipv4_mapped().is_some_and(|v4| {
                    v4.is_loopback() || v4.is_private() || v4.is_link_local()
                })
        }
    }
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

    // Authority-form target: `<ip>:<port>`, IPv6 bracketed. Names are not
    // permitted (targets are relay IPs from the consensus).
    let target: SocketAddr = match req.uri().authority().and_then(|a| a.as_str().parse().ok()) {
        Some(addr) => addr,
        None => {
            warn!("CONNECT: bad target '{}'", req.uri());
            return text_response(StatusCode::BAD_REQUEST, "invalid target address");
        }
    };

    if target.is_ipv6() && !gw.has_ipv6 {
        warn!("CONNECT: rejected IPv6 target {} (no IPv6 connectivity)", target);
        return text_response(
            StatusCode::BAD_REQUEST,
            "IPv6 targets are not supported on this server",
        );
    }

    if is_local(target.ip()) && !allow_local_targets() {
        warn!("CONNECT: rejected local target {}", target);
        return text_response(
            StatusCode::FORBIDDEN,
            "connections to local addresses are forbidden",
        );
    }

    let allowed = gw
        .relay_allowlist
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&target);
    if !allowed {
        warn!("CONNECT: rejected non-relay target {}", target);
        return text_response(StatusCode::FORBIDDEN, "target is not an advertised Tor relay");
    }

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
