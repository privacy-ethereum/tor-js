//! The KPS listener and the per-stream KPS-HTTP/1 server (PROTOCOL.md §3).
//!
//! Each KPS stream carries exactly one HTTP/1.1 exchange under the strict
//! profile: no keep-alive, no chunked encoding, bodies delimited by FIN.
//! hyper's http1 server drives each stream directly (`kps::Stream` is
//! `AsyncRead + AsyncWrite`); `CONNECT` upgrades into a TCP tunnel, everything
//! else is routed through the axum `Router`.
//!
//! Profile enforcement on top of hyper where hyper is lenient:
//! - requests bearing `Transfer-Encoding` → `400` (§3.4)
//! - `Host` required on origin-form requests (§3.2)
//! - header block capped at 16 KiB (§3.6, via `max_buf_size`)
//! - 30 s from stream open to complete request headers (§3.6)
//! - keep-alive disabled: a second request on a stream is a protocol error
//! - responses always carry `Content-Length` (full bodies) — never chunked

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Method, Request, Response, StatusCode};
use axum::Router;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use tower::ServiceExt;
use tracing::{debug, warn};

use crate::routes::Gateway;

/// Max request header block (request line included), PROTOCOL.md §3.6.
const HEADER_CAP: usize = 16 * 1024;

/// Time from stream open to complete request header block, PROTOCOL.md §3.6.
const HEADER_TIMEOUT: Duration = Duration::from_secs(30);

/// Per-KPS-connection context shared by every stream on that connection.
#[derive(Clone)]
pub struct ConnCtx {
    pub gateway: Arc<Gateway>,
    pub router: Router,
    /// The client's UDP endpoint, canonicalized (v4-mapped v6 → v4) so per-IP
    /// limits key consistently across transports.
    pub remote: SocketAddr,
    /// Live tunnel count on this connection (per-connection limit).
    pub conn_tunnels: Arc<AtomicUsize>,
}

/// Accept loop: connections → streams → one exchange per stream.
/// Runs forever; call via `tokio::spawn`.
pub async fn run(listener: kps::Listener, gateway: Arc<Gateway>, router: Router) {
    loop {
        match listener.accept().await {
            Ok(conn) => {
                let gateway = gateway.clone();
                let router = router.clone();
                tokio::spawn(handle_conn(conn, gateway, router));
            }
            Err(e) => {
                warn!("kps listener closed: {}", e);
                return;
            }
        }
    }
}

async fn handle_conn(conn: Box<dyn kps::Conn>, gateway: Arc<Gateway>, router: Router) {
    let raw = conn.remote_addr();
    let remote = SocketAddr::new(raw.ip().to_canonical(), raw.port());
    debug!("kps connection from {}", remote);
    let conn: Arc<dyn kps::Conn> = Arc::from(conn);
    let ctx = ConnCtx {
        gateway,
        router,
        remote,
        conn_tunnels: Arc::new(AtomicUsize::new(0)),
    };
    loop {
        match conn.accept_stream().await {
            Ok(stream) => {
                let ctx = ctx.clone();
                tokio::spawn(serve_stream(stream, ctx));
            }
            Err(e) => {
                debug!("kps connection from {} ended: {}", remote, e);
                return;
            }
        }
    }
}

/// Serves one KPS-HTTP/1 exchange on one stream.
///
/// `half_close(true)`: the client's FIN after its request (the EOF that
/// delimits the request body, §3.2) must not tear down the exchange.
/// On error the stream is dropped without a graceful FIN, which the
/// transports surface as an abortive close — the §3.4 "reset, don't recover"
/// behavior.
async fn serve_stream(stream: Box<dyn kps::Stream>, ctx: ConnCtx) {
    let remote = ctx.remote;
    let io = TokioIo::new(stream);
    let service = service_fn(move |req| {
        let ctx = ctx.clone();
        async move { Ok::<_, Infallible>(handle_request(req, ctx).await) }
    });
    let result = http1::Builder::new()
        .timer(TokioTimer::new())
        .keep_alive(false)
        .half_close(true)
        .max_buf_size(HEADER_CAP)
        .header_read_timeout(HEADER_TIMEOUT)
        .serve_connection(io, service)
        .with_upgrades()
        .await;
    if let Err(e) = result {
        debug!("stream from {}: {}", remote, e);
    }
}

fn text_response(status: StatusCode, msg: &str) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from(msg.to_string()))
        .unwrap()
}

/// Reconstructed size of the request head (request line + header fields).
/// hyper's `max_buf_size` is a soft cap — a head that arrives in one read can
/// exceed it — so §3.6 is also enforced here, where we can still answer `431`.
fn header_block_size<B>(req: &Request<B>) -> usize {
    let request_line =
        req.method().as_str().len() + req.uri().to_string().len() + " HTTP/1.1\r\n".len() + 1;
    req.headers()
        .iter()
        .fold(request_line, |n, (name, value)| n + name.as_str().len() + value.len() + 4)
}

/// Per-request dispatch: profile checks, then CONNECT or the router.
async fn handle_request(req: Request<Incoming>, ctx: ConnCtx) -> Response<Body> {
    // §3.6: header block capped at 16 KiB; overflow → 431, then close.
    if header_block_size(&req) > HEADER_CAP {
        return text_response(
            StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
            "header block exceeds 16 KiB",
        );
    }

    // §3.4: Transfer-Encoding is forbidden in any message, any value.
    if req.headers().contains_key(header::TRANSFER_ENCODING) {
        return text_response(StatusCode::BAD_REQUEST, "Transfer-Encoding is not permitted");
    }

    if req.method() == Method::CONNECT {
        return crate::tunnel::handle_connect(req, &ctx).await;
    }

    // §3.2: origin-form requests must carry Host. (Trust never derives from
    // it; this gateway doesn't virtual-host, so the value is ignored.)
    if !req.headers().contains_key(header::HOST) {
        return text_response(StatusCode::BAD_REQUEST, "missing Host header");
    }

    // §3.5: unknown method → 501 (axum would answer 405).
    match *req.method() {
        Method::GET
        | Method::HEAD
        | Method::POST
        | Method::PUT
        | Method::DELETE
        | Method::OPTIONS
        | Method::PATCH
        | Method::TRACE => {}
        _ => return text_response(StatusCode::NOT_IMPLEMENTED, "unknown method"),
    }

    match ctx.router.clone().oneshot(req.map(Body::new)).await {
        Ok(res) => res,
        Err(never) => match never {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(method: &str, uri: &str, headers: &[(&str, &str)]) -> Request<()> {
        let mut builder = Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(()).unwrap()
    }

    /// This arithmetic, not hyper, is what makes the §3.6 cap answerable with a
    /// `431`: `max_buf_size` is a soft cap that a single large read can exceed.
    #[test]
    fn header_block_size_reconstructs_the_wire_bytes() {
        // "GET / HTTP/1.1\r\n" is 16 bytes; no field lines.
        assert_eq!(header_block_size(&req("GET", "/", &[])), 16);
        // Each field line is `name: value\r\n`.
        assert_eq!(
            header_block_size(&req("GET", "/", &[("host", "x")])),
            16 + "host: x\r\n".len()
        );
        // The URI counts in full, including the query.
        assert_eq!(
            header_block_size(&req("GET", "/relay/random?a=1", &[])),
            16 + "/relay/random?a=1".len() - 1
        );
        // CONNECT carries an authority-form target.
        assert_eq!(
            header_block_size(&req("CONNECT", "1.2.3.4:9001", &[])),
            "CONNECT 1.2.3.4:9001 HTTP/1.1\r\n".len()
        );
    }

    /// The count omits the blank line that terminates the block, so it reads two
    /// bytes low. That is deliberate slack in a limit, never an overcount that
    /// could reject a legal head.
    #[test]
    fn header_block_size_never_overcounts() {
        let head = "GET /x HTTP/1.1\r\nhost: example.com\r\naccept: */*\r\n\r\n";
        let counted = header_block_size(&req(
            "GET",
            "/x",
            &[("host", "example.com"), ("accept", "*/*")],
        ));
        assert_eq!(counted, head.len() - 2);
        assert!(counted <= head.len());
    }

    #[test]
    fn header_block_size_straddling_the_cap() {
        // One header value sized so the whole block lands exactly on the cap.
        let fixed = header_block_size(&req("GET", "/", &[("x", "")]));
        let at_cap = "v".repeat(HEADER_CAP - fixed);
        assert_eq!(header_block_size(&req("GET", "/", &[("x", &at_cap)])), HEADER_CAP);
        assert!(
            header_block_size(&req("GET", "/", &[("x", &at_cap)])) <= HEADER_CAP,
            "exactly at the cap is allowed"
        );

        let over = "v".repeat(HEADER_CAP - fixed + 1);
        assert!(header_block_size(&req("GET", "/", &[("x", &over)])) > HEADER_CAP);
    }

    /// Many small headers must add up the same way one big one does — the cap
    /// has to bound the whole block, not any single field.
    #[test]
    fn header_block_size_accumulates_across_fields() {
        let headers: Vec<(String, String)> =
            (0..600).map(|i| (format!("x-pad-{i:03}"), "v".repeat(24))).collect();
        let borrowed: Vec<(&str, &str)> =
            headers.iter().map(|(n, v)| (n.as_str(), v.as_str())).collect();
        assert!(header_block_size(&req("GET", "/", &borrowed)) > HEADER_CAP);
    }
}
