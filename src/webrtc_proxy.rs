//! WebRTC data channel relay.
//!
//! Provides the same TCP relay functionality as the WebSocket proxy, but over
//! WebRTC data channels.  A single UDP socket multiplexes all peer connections.
//!
//! Signaling: browser POSTs an SDP offer to `/rtc/connect`, gets back an SDP answer.
//! Then opens data channels labeled `"ip:port"` to proxy TCP connections to Tor relays.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use str0m::change::SdpOffer;
use str0m::channel::ChannelId;
use str0m::net::{Protocol, Receive};
use str0m::{Event, IceConnectionState, Input, Output, Rtc};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::ws_proxy::{ConnectionTracker, RelayAllowlist, WsLimits, is_local};

/// A newly negotiated peer, sent from the HTTP signaling handler to the UDP loop.
pub struct NewPeer {
    pub rtc: Rtc,
    pub peer_ip: IpAddr,
}

/// How many TCP-read chunks (up to 16 KiB each) may queue per channel before
/// the TCP reader task blocks, closing the kernel receive window upstream.
const TCP_TO_DC_QUEUE: usize = 16;

/// How many data-channel messages may queue toward a slow TCP write before we
/// give up and close the channel (dropping data would corrupt the stream).
const DC_TO_TCP_QUEUE: usize = 256;

/// Bridge between one data channel and its TCP connection.
struct ChannelBridge {
    label: String,
    /// DC -> TCP: data channel bytes headed for the TCP task.
    to_tcp: mpsc::Sender<Vec<u8>>,
    /// TCP -> DC: bytes read from TCP, written to the data channel as the
    /// SCTP send buffer allows. The sender side dropping means TCP closed.
    from_tcp: mpsc::Receiver<Vec<u8>>,
    /// A chunk the SCTP send buffer didn't accept yet (write returned false).
    pending: Option<Vec<u8>>,
}

/// Per-peer state in the UDP event loop.
struct Peer {
    rtc: Rtc,
    peer_ip: IpAddr,
    /// Data channel ID -> TCP bridge state.
    channels: HashMap<ChannelId, ChannelBridge>,
    /// The `_signal` control channel, if open.
    signal_cid: Option<ChannelId>,
    created_at: Instant,
    last_activity: Instant,
}

impl Peer {
    /// Close a data channel: remove its bridge, close it in the Rtc, notify
    /// the client via the signal channel, and release the connection slot.
    fn close_channel(&mut self, cid: ChannelId, connection_tracker: &ConnectionTracker) {
        let Some(bridge) = self.channels.remove(&cid) else {
            return;
        };
        let sctp_id = self
            .rtc
            .direct_api()
            .sctp_stream_id_by_channel_id(cid)
            .unwrap_or(0);
        self.rtc.direct_api().close_data_channel(cid);
        if let Some(sig_cid) = self.signal_cid {
            if let Some(mut ch) = self.rtc.channel(sig_cid) {
                let msg = serde_json::json!({
                    "type": "closed",
                    "channel": bridge.label,
                    "sctp_id": sctp_id,
                });
                let _ = ch.write(false, msg.to_string().as_bytes());
            }
        }
        connection_tracker.release(self.peer_ip);
        debug!(
            "rtc: closed channel {:?} ({}) for peer {}",
            cid, bridge.label, self.peer_ip
        );
    }

    /// Pump queued TCP data into data channels, respecting SCTP send-buffer
    /// backpressure. Returns channels whose TCP side has finished (all data
    /// flushed) so the caller can close them.
    fn flush_bridges(&mut self) -> Vec<ChannelId> {
        let mut done: Vec<ChannelId> = Vec::new();
        let Peer {
            rtc,
            channels,
            last_activity,
            ..
        } = self;
        for (cid, bridge) in channels.iter_mut() {
            loop {
                let chunk = match bridge.pending.take() {
                    Some(c) => c,
                    None => match bridge.from_tcp.try_recv() {
                        Ok(c) => c,
                        Err(mpsc::error::TryRecvError::Empty) => break,
                        Err(mpsc::error::TryRecvError::Disconnected) => {
                            // TCP closed and every chunk has been written.
                            done.push(*cid);
                            break;
                        }
                    },
                };
                let Some(mut ch) = rtc.channel(*cid) else {
                    // Channel no longer writable; drop the bridge.
                    done.push(*cid);
                    break;
                };
                match ch.write(true, &chunk) {
                    Ok(true) => {
                        *last_activity = Instant::now();
                    }
                    Ok(false) => {
                        // SCTP send buffer full — retry after the browser acks.
                        bridge.pending = Some(chunk);
                        break;
                    }
                    Err(e) => {
                        debug!("rtc: write to channel {:?} failed: {}", cid, e);
                        done.push(*cid);
                        break;
                    }
                }
            }
        }
        done
    }
}

/// HTTP handler for `POST /rtc/connect` — SDP offer/answer signaling.
pub async fn handle_rtc_connect(
    State(state): State<crate::server::AppState>,
    req: axum::extract::ConnectInfo<SocketAddr>,
    body: String,
) -> Response {
    let peer_ip = req.0.ip();

    let webrtc = match &state.webrtc_tx {
        Some(tx) => tx,
        None => {
            return (StatusCode::NOT_FOUND, "WebRTC not enabled").into_response();
        }
    };

    // Check connection limits before doing any work.
    if !state.connection_tracker.acquire(peer_ip, &state.ws_limits) {
        warn!("rtc: connection limit reached for {}", peer_ip);
        return (StatusCode::SERVICE_UNAVAILABLE, "connection limit reached").into_response();
    }

    // Browser sends {"type":"offer","sdp":"v=0\r\n..."} — extract the raw SDP string.
    #[derive(Deserialize)]
    struct BrowserOffer {
        sdp: String,
    }
    let browser_offer: BrowserOffer = match serde_json::from_str(&body) {
        Ok(o) => o,
        Err(e) => {
            state.connection_tracker.release(peer_ip);
            warn!("rtc: bad SDP offer from {}: {}", peer_ip, e);
            return (StatusCode::BAD_REQUEST, "invalid SDP offer").into_response();
        }
    };
    let offer = match SdpOffer::from_sdp_string(&browser_offer.sdp) {
        Ok(o) => o,
        Err(e) => {
            state.connection_tracker.release(peer_ip);
            warn!("rtc: bad SDP from {}: {}", peer_ip, e);
            return (StatusCode::BAD_REQUEST, "invalid SDP").into_response();
        }
    };

    let mut rtc = Rtc::new(Instant::now());

    // Add host ICE candidates for all network interfaces so the browser
    // can reach us via whichever path works (loopback, LAN, public, tunneled).
    if let Some(local_addr) = state.webrtc_local_addr {
        let port = local_addr.port();
        if local_addr.ip().is_unspecified() {
            // Bound to 0.0.0.0 — advertise all interface IPs.
            // Always include the peer's IP (the address they used to reach us).
            let mut added = std::collections::HashSet::new();
            for ip in gather_local_ips() {
                if added.insert(ip) {
                    if let Ok(c) = str0m::Candidate::host(SocketAddr::new(ip, port), "udp") {
                        rtc.add_local_candidate(c);
                    }
                }
            }
            // Also add the peer's connecting IP in case it's not in our interface list
            // (e.g. port-forwarded).
            if added.insert(peer_ip) {
                if let Ok(c) = str0m::Candidate::host(SocketAddr::new(peer_ip, port), "udp") {
                    rtc.add_local_candidate(c);
                }
            }
        } else {
            if let Ok(c) = str0m::Candidate::host(local_addr, "udp") {
                rtc.add_local_candidate(c);
            }
        }
    }

    let answer = match rtc.sdp_api().accept_offer(offer) {
        Ok(answer) => answer,
        Err(e) => {
            state.connection_tracker.release(peer_ip);
            warn!("rtc: failed to accept offer from {}: {}", peer_ip, e);
            return (StatusCode::BAD_REQUEST, "failed to process SDP offer").into_response();
        }
    };

    // Browser expects {"type":"answer","sdp":"v=0\r\n..."} format.
    let answer_json = serde_json::json!({
        "type": "answer",
        "sdp": answer.to_sdp_string(),
    })
    .to_string();

    // Hand off the Rtc instance to the UDP event loop.
    if webrtc.send(NewPeer { rtc, peer_ip }).await.is_err() {
        state.connection_tracker.release(peer_ip);
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    (
        StatusCode::OK,
        [("content-type", "application/json")],
        answer_json,
    )
        .into_response()
}

/// Run the UDP event loop that drives all WebRTC peers.
pub async fn run_udp_loop(
    udp: UdpSocket,
    local_addr: SocketAddr,
    mut new_peers_rx: mpsc::Receiver<NewPeer>,
    relay_allowlist: RelayAllowlist,
    connection_tracker: ConnectionTracker,
    ws_limits: WsLimits,
    has_ipv6: bool,
) {
    let mut peers: Vec<Peer> = Vec::new();
    let mut buf = vec![0u8; 65536];

    // Cache local interface IPs — gathering shells out to `ip` and must not
    // run per packet. Refreshed periodically in case addresses change.
    let mut local_ips = gather_local_ips();
    let mut local_ips_refreshed = Instant::now();
    const LOCAL_IPS_TTL: Duration = Duration::from_secs(60);

    info!("WebRTC UDP loop listening on {}", local_addr);

    loop {
        // Accept new peers.
        while let Ok(new_peer) = new_peers_rx.try_recv() {
            let now = Instant::now();
            peers.push(Peer {
                rtc: new_peer.rtc,
                peer_ip: new_peer.peer_ip,
                channels: HashMap::new(),
                signal_cid: None,
                created_at: now,
                last_activity: now,
            });
            debug!("rtc: new peer from {}, total={}", new_peer.peer_ip, peers.len());
        }

        // Pump TCP -> data channel bytes, respecting SCTP backpressure.
        // Chunks the send buffer rejects stay queued; the bounded per-channel
        // queue blocks the TCP reader, pushing backpressure to the relay.
        for peer in peers.iter_mut() {
            for cid in peer.flush_bridges() {
                peer.close_channel(cid, &connection_tracker);
            }
        }

        // Poll all peers for output.
        let mut earliest_timeout = Instant::now() + Duration::from_millis(100);

        for peer in peers.iter_mut() {
            loop {
                match peer.rtc.poll_output() {
                    Ok(Output::Transmit(t)) => {
                        let _ = udp.send_to(&t.contents, t.destination).await;
                    }
                    Ok(Output::Event(event)) => {
                        handle_peer_event(
                            peer,
                            event,
                            &relay_allowlist,
                            &connection_tracker,
                            &ws_limits,
                            has_ipv6,
                        );
                    }
                    Ok(Output::Timeout(t)) => {
                        if t < earliest_timeout {
                            earliest_timeout = t;
                        }
                        break;
                    }
                    Err(e) => {
                        debug!("rtc: poll error for {}: {}", peer.peer_ip, e);
                        break;
                    }
                }
            }
        }

        // Enforce idle timeout and max lifetime.
        let now = Instant::now();
        peers.retain_mut(|peer| {
            let idle = now.duration_since(peer.last_activity) > ws_limits.idle_timeout;
            let expired = now.duration_since(peer.created_at) > ws_limits.max_lifetime;
            if idle || expired {
                if idle {
                    debug!("rtc: idle timeout for {}", peer.peer_ip);
                } else {
                    debug!("rtc: max lifetime for {}", peer.peer_ip);
                }
                let n = peer.channels.len();
                peer.channels.clear();
                for _ in 0..n {
                    connection_tracker.release(peer.peer_ip);
                }
                // Release the initial connection slot from signaling.
                connection_tracker.release(peer.peer_ip);
                return false;
            }
            true
        });

        // Remove disconnected peers.
        peers.retain_mut(|peer| {
            if !peer.rtc.is_alive() {
                debug!("rtc: peer {} disconnected", peer.peer_ip);
                let n = peer.channels.len();
                peer.channels.clear();
                for _ in 0..n {
                    connection_tracker.release(peer.peer_ip);
                }
                connection_tracker.release(peer.peer_ip);
                return false;
            }
            true
        });

        // Wait for UDP packet or timeout.
        let wait = earliest_timeout.saturating_duration_since(Instant::now());
        let wait = wait.max(Duration::from_millis(1));

        tokio::select! {
            result = udp.recv_from(&mut buf) => {
                match result {
                    Ok((n, source)) => {
                        let port = local_addr.port();

                        // When bound to 0.0.0.0, we don't know which local IP the
                        // packet was addressed to. Try each candidate IP we registered
                        // until a peer accepts.
                        let candidate_ips: Vec<IpAddr> = if local_addr.ip().is_unspecified() {
                            if local_ips_refreshed.elapsed() > LOCAL_IPS_TTL {
                                local_ips = gather_local_ips();
                                local_ips_refreshed = Instant::now();
                            }
                            local_ips.clone()
                        } else {
                            vec![local_addr.ip()]
                        };

                        let mut handled = false;
                        for dest_ip in &candidate_ips {
                            let dest = SocketAddr::new(*dest_ip, port);
                            let Ok(receive) = Receive::new(
                                Protocol::Udp,
                                source,
                                dest,
                                &buf[..n],
                            ) else {
                                continue;
                            };
                            let input = Input::Receive(Instant::now(), receive);

                            if let Some(peer) = peers.iter_mut().find(|p| p.rtc.accepts(&input)) {
                                if let Err(e) = peer.rtc.handle_input(input) {
                                    debug!("rtc: input error for {}: {}", peer.peer_ip, e);
                                }
                                peer.last_activity = Instant::now();
                                handled = true;
                                break;
                            }
                        }
                        if !handled {
                            debug!("rtc: no peer accepted packet from {}", source);
                        }
                    }
                    Err(e) => {
                        warn!("rtc: UDP recv error: {}", e);
                    }
                }
            }
            _ = tokio::time::sleep(wait) => {
                // Drive timeouts for all peers.
                let now = Instant::now();
                for peer in peers.iter_mut() {
                    let _ = peer.rtc.handle_input(Input::Timeout(now));
                }
            }
        }
    }
}

/// Handle an event from a peer's Rtc instance.
fn handle_peer_event(
    peer: &mut Peer,
    event: Event,
    relay_allowlist: &RelayAllowlist,
    connection_tracker: &ConnectionTracker,
    ws_limits: &WsLimits,
    has_ipv6: bool,
) {
    /// Send a JSON message on the signal channel.
    fn signal_send(peer: &mut Peer, msg: &serde_json::Value) {
        if let Some(cid) = peer.signal_cid {
            if let Some(mut ch) = peer.rtc.channel(cid) {
                let _ = ch.write(false, msg.to_string().as_bytes());
            }
        }
    }

    match event {
        Event::ChannelOpen(cid, label) => {
            // --- Signal channel ---
            if label == "_signal" {
                info!("rtc: signal channel open from {}", peer.peer_ip);
                peer.signal_cid = Some(cid);
                signal_send(peer, &serde_json::json!({
                    "type": "hello",
                    "server": "tor-js-gateway",
                    "ipv6": has_ipv6,
                }));
                return;
            }

            // --- Init channel (ignored) ---
            if label == "_init" {
                peer.rtc.direct_api().close_data_channel(cid);
                return;
            }

            let sctp_id = peer.rtc.direct_api()
                .sctp_stream_id_by_channel_id(cid)
                .unwrap_or(0);
            info!("rtc: channel open {:?} sctp={} label='{}' from {}", cid, sctp_id, label, peer.peer_ip);

            // Parse label as target address.
            let addr: SocketAddr = match label.parse() {
                Ok(a) => a,
                Err(_) => {
                    warn!("rtc: bad channel label '{}'", label);
                    signal_send(peer, &serde_json::json!({
                        "type": "rejected",
                        "channel": label,
                        "sctp_id": sctp_id,
                        "reason": "invalid target address",
                    }));
                    peer.rtc.direct_api().close_data_channel(cid);
                    return;
                }
            };

            // Security checks (same as WS proxy).
            let rejection = if addr.is_ipv6() && !has_ipv6 {
                Some("IPv6 not supported on this server")
            } else if is_local(addr.ip()) {
                Some("local addresses forbidden")
            } else if !relay_allowlist
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&addr)
            {
                Some("not an advertised relay")
            } else if !connection_tracker.acquire(peer.peer_ip, ws_limits) {
                Some("connection limit reached")
            } else {
                None
            };

            if let Some(reason) = rejection {
                warn!("rtc: rejected {} — {}", addr, reason);
                signal_send(peer, &serde_json::json!({
                    "type": "rejected",
                    "channel": label,
                    "sctp_id": sctp_id,
                    "reason": reason,
                }));
                peer.rtc.direct_api().close_data_channel(cid);
                return;
            }

            // Spawn TCP bridge task.
            let (to_tcp_tx, to_tcp_rx) = mpsc::channel::<Vec<u8>>(DC_TO_TCP_QUEUE);
            let (from_tcp_tx, from_tcp_rx) = mpsc::channel::<Vec<u8>>(TCP_TO_DC_QUEUE);
            tokio::spawn(tcp_bridge_task(addr, to_tcp_rx, from_tcp_tx));

            peer.channels.insert(
                cid,
                ChannelBridge {
                    label: label.to_string(),
                    to_tcp: to_tcp_tx,
                    from_tcp: from_tcp_rx,
                    pending: None,
                },
            );
            peer.last_activity = Instant::now();
        }
        Event::ChannelData(data) => {
            peer.last_activity = Instant::now();

            // Handle signal channel messages.
            if peer.signal_cid == Some(data.id) {
                if let Ok(text) = std::str::from_utf8(&data.data) {
                    if let Ok(msg) = serde_json::from_str::<serde_json::Value>(text) {
                        match msg.get("type").and_then(|t| t.as_str()) {
                            Some("ping") => {
                                signal_send(peer, &serde_json::json!({
                                    "type": "pong",
                                    "ts": msg.get("ts"),
                                }));
                            }
                            _ => {}
                        }
                    }
                }
                return;
            }

            if let Some(bridge) = peer.channels.get(&data.id) {
                match bridge.to_tcp.try_send(data.data.to_vec()) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        // Dropping bytes would corrupt the TCP stream; fail loudly.
                        warn!(
                            "rtc: DC->TCP queue overflow on channel {:?}, closing",
                            data.id
                        );
                        peer.close_channel(data.id, connection_tracker);
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        // TCP task exited; flush_bridges will close the channel
                        // once any remaining TCP->DC data has been written.
                    }
                }
            }
        }
        Event::ChannelClose(cid) => {
            if peer.signal_cid == Some(cid) {
                peer.signal_cid = None;
            } else if peer.channels.remove(&cid).is_some() { // bridge dropped, TCP task exits
                connection_tracker.release(peer.peer_ip);
                debug!("rtc: channel {:?} closed by remote", cid);
            }
        }
        Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
            debug!("rtc: ICE disconnected for {}", peer.peer_ip);
        }
        Event::Connected => {
            info!("rtc: peer {} connected", peer.peer_ip);
        }
        _ => {}
    }
}

/// Bridge between a data channel and a TCP connection to a Tor relay.
///
/// Dropping `to_dc` is how the event loop learns the TCP side is done (it
/// closes the data channel once all queued data has been flushed).
async fn tcp_bridge_task(
    target: SocketAddr,
    mut from_dc: mpsc::Receiver<Vec<u8>>,
    to_dc: mpsc::Sender<Vec<u8>>,
) {
    let tcp = match tokio::time::timeout(
        Duration::from_secs(10),
        TcpStream::connect(target),
    )
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            debug!("rtc: TCP connect to {} failed: {}", target, e);
            return;
        }
        Err(_) => {
            debug!("rtc: TCP connect to {} timed out", target);
            return;
        }
    };
    info!("rtc: TCP connected to {}", target);

    let (mut tcp_read, mut tcp_write) = tcp.into_split();

    // DC -> TCP
    let dc_to_tcp = async {
        while let Some(data) = from_dc.recv().await {
            if tcp_write.write_all(&data).await.is_err() {
                break;
            }
        }
        let _ = tcp_write.shutdown().await;
    };

    // TCP -> DC. The bounded channel applies backpressure: when the event
    // loop can't write to the data channel (SCTP send buffer full), this
    // send blocks, we stop reading, and the kernel closes the TCP window.
    let tcp_to_dc = async {
        let mut buf = vec![0u8; 16384];
        loop {
            match tcp_read.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if to_dc.send(buf[..n].to_vec()).await.is_err() {
                        break;
                    }
                }
            }
        }
    };

    tokio::select! {
        _ = dc_to_tcp => {}
        _ = tcp_to_dc => {}
    }

    debug!("rtc: TCP bridge to {} done", target);
}

/// Gather all non-unspecified IP addresses from local network interfaces.
fn gather_local_ips() -> Vec<IpAddr> {
    let mut ips = Vec::new();

    // Always include loopback.
    ips.push(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));

    // Use getifaddrs via a UDP connect trick to discover interface IPs.
    // Connect to a remote address (doesn't actually send anything) to find
    // the default outgoing IP.
    if let Ok(sock) = std::net::UdpSocket::bind("0.0.0.0:0") {
        // Try a public IP to get the default route IP.
        if sock.connect("8.8.8.8:80").is_ok() {
            if let Ok(addr) = sock.local_addr() {
                if !addr.ip().is_unspecified() && !addr.ip().is_loopback() {
                    ips.push(addr.ip());
                }
            }
        }
    }

    // Parse /proc/net/if_inet6 and /proc/net/fib_trie for more addresses.
    // Simpler: read from ip command output.
    if let Ok(output) = std::process::Command::new("ip")
        .args(["-o", "addr", "show"])
        .output()
    {
        if let Ok(text) = String::from_utf8(output.stdout) {
            for line in text.lines() {
                // Format: "2: eth0    inet 192.168.1.5/24 ..."
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 4 && (parts[2] == "inet" || parts[2] == "inet6") {
                    if let Some(addr_str) = parts[3].split('/').next() {
                        if let Ok(ip) = addr_str.parse::<IpAddr>() {
                            if !ip.is_unspecified() && !ips.contains(&ip) {
                                ips.push(ip);
                            }
                        }
                    }
                }
            }
        }
    }

    ips
}
