//! WASM-compatible runtime implementation for tor-rtcompat.
//!
//! This module provides a runtime that can run in WebAssembly environments (browsers).
//! It implements the required traits for `Runtime` with some limitations:
//!
//! - **Blocking operations**: Stubbed - will panic if called. WASM has no threads.
//! - **Networking**: Requires external transport (WebSocket/WebRTC)
//! - **TLS**: Uses rustls with rustls-rustcrypto (pure-Rust crypto for WASM)

/// A `Send` wrapper around a `!Send` future. Delegates polling through `SendWrapper`.
///
/// WASM is single-threaded, so `Send` is trivially satisfied.
/// Panics if polled from a different thread (impossible on WASM).
struct SendFut<F>(send_wrapper::SendWrapper<F>);

impl<F: Future> Future for SendFut<F> {
    type Output = F::Output;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: We don't move F after pinning. SendWrapper::deref_mut gives &mut F.
        let inner = unsafe { self.map_unchecked_mut(|s| &mut *s.0) };
        inner.poll(cx)
    }
}

// This runtime was moved here from arti's tor-rtcompat::wasm module. It depends
// only on tor-rtcompat's *public* API, so it lives in tor-js rather than in the
// fork. The traits below are re-exported at the tor_rtcompat crate root...
use tor_rtcompat::{
    Blocking, CertifiedConn, CoarseInstant, CoarseTimeProvider, NetStreamListener,
    NetStreamProvider, RealCoarseTimeProvider, SleepProvider, StreamOps, TcpConnectOptions,
    TcpListenOptions, TlsProvider, UdpProvider, UdpSocket, UnixConnectOptions, UnixListenOptions,
};
// ...while these TLS traits are only reachable via the public `tls` submodule.
use tor_rtcompat::tls::{TlsAcceptorSettings, TlsConnector};
use std::borrow::Cow;
use async_trait::async_trait;
use futures::task::{Spawn, SpawnError};
use futures::{stream, AsyncRead, AsyncWrite, Future};
use std::fmt::Debug;
use std::io::{self, Result as IoResult};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use web_time_compat::Instant;
use std::time::{SystemTime, UNIX_EPOCH};
use tor_general_addr::unix;

/// A runtime for WASM environments.
///
/// This runtime implements the traits required by `tor-rtcompat::Runtime`,
/// but with significant limitations due to WASM constraints:
///
/// - No blocking operations (will panic)
/// - No direct TCP/UDP sockets — use [`set_connect_fn`](WasmRuntime::set_connect_fn)
///   to provide a JS callback for opening socket connections
/// - No filesystem access
#[derive(Clone)]
pub struct WasmRuntime {
    /// Coarse time provider
    coarse: RealCoarseTimeProvider,
    /// Optional JS callback for connecting to relay addresses.
    /// Signature: `(addr: string) => Promise<{send, onmessage, onclose, close}>`
    connect_fn: Option<js_sys::Function>,
}

impl Debug for WasmRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmRuntime").finish()
    }
}

impl Default for WasmRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl WasmRuntime {
    /// Create a new WASM runtime.
    pub fn new() -> Self {
        Self {
            coarse: RealCoarseTimeProvider::new(),
            connect_fn: None,
        }
    }

    /// Set a JS callback for opening socket connections to relay addresses.
    ///
    /// The callback receives a target address string (e.g. `"198.51.100.1:9001"`)
    /// and must return a `Promise` that resolves to a socket object with:
    /// - `send(data: Uint8Array)` — send binary data
    /// - `onmessage: ((data: Uint8Array) => void) | null` — receive callback
    /// - `onclose: (() => void) | null` — close notification callback
    /// - `close()` — close the socket
    pub fn set_connect_fn(&mut self, f: js_sys::Function) {
        self.connect_fn = Some(f);
    }
}

// ============================================================================
// SleepProvider implementation
// ============================================================================

/// A sleep future for WASM using gloo-timers.
pub struct WasmSleepFuture {
    /// The underlying timeout future from gloo-timers
    inner: send_wrapper::SendWrapper<gloo_timers::future::TimeoutFuture>,
}

impl WasmSleepFuture {
    /// Create a new sleep future.
    fn new(duration: Duration) -> Self {
        let millis = duration.as_millis().min(u128::from(u32::MAX)) as u32;
        Self {
            inner: send_wrapper::SendWrapper::new(gloo_timers::future::TimeoutFuture::new(millis)),
        }
    }
}

impl Future for WasmSleepFuture {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: We never move the inner future after pinning.
        // Deref through SendWrapper to get the TimeoutFuture.
        let inner = unsafe { self.map_unchecked_mut(|s| &mut *s.inner) };
        inner.poll(cx)
    }
}

// WasmSleepFuture is Send because the inner future is wrapped in SendWrapper.

impl SleepProvider for WasmRuntime {
    type SleepFuture = WasmSleepFuture;

    fn sleep(&self, duration: Duration) -> Self::SleepFuture {
        WasmSleepFuture::new(duration)
    }

    fn now(&self) -> Instant {
        Instant::now()
    }

    fn wallclock(&self) -> SystemTime {
        // Use Date.now() for WASM wall-clock time
        let millis = js_sys::Date::now();
        UNIX_EPOCH + Duration::from_millis(millis as u64)
    }
}

// ============================================================================
// CoarseTimeProvider implementation
// ============================================================================

impl CoarseTimeProvider for WasmRuntime {
    fn now_coarse(&self) -> CoarseInstant {
        self.coarse.now_coarse()
    }
}

// ============================================================================
// Spawn implementation
// ============================================================================

impl Spawn for WasmRuntime {
    fn spawn_obj(&self, future: futures::task::FutureObj<'static, ()>) -> Result<(), SpawnError> {
        wasm_bindgen_futures::spawn_local(future);
        Ok(())
    }
}

// ============================================================================
// Blocking implementation (STUBBED - will panic)
// ============================================================================

impl Blocking for WasmRuntime {
    type ThreadHandle<T: Send + 'static> = StubThreadHandle<T>;

    fn spawn_blocking<F, T>(&self, _f: F) -> Self::ThreadHandle<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        panic!(
            "WasmRuntime::spawn_blocking called - blocking operations are not supported in WASM. \
             This code path should not be reached. Please report this as a bug."
        );
    }

    fn reenter_block_on<F>(&self, _future: F) -> F::Output
    where
        F: Future,
        F::Output: Send + 'static,
    {
        panic!(
            "WasmRuntime::reenter_block_on called - blocking operations are not supported in WASM. \
             This code path should not be reached. Please report this as a bug."
        );
    }
}

/// Stub thread handle that will never be created (spawn_blocking panics).
pub struct StubThreadHandle<T> {
    /// Type marker for the result type.
    _phantom: std::marker::PhantomData<T>,
}

impl<T: Send + 'static> Future for StubThreadHandle<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        // This will never be called because spawn_blocking panics
        unreachable!("StubThreadHandle should never be polled")
    }
}

// ============================================================================
// NetStreamProvider implementation (WebSocket proxy)
// ============================================================================

/// A stream backed by a JS socket object exposing WHATWG streams.
///
/// The JS socket (e.g. `ArtiSocket` from the TS wrapper) must expose a
/// `readable` (`ReadableStream<Uint8Array>`), a `writable`
/// (`WritableStream<Uint8Array>`), and a `close()` method. Data is pulled and
/// pushed one chunk at a time so backpressure propagates end to end: arti only
/// pulls from the network when it reads, and writes wait for the sink to drain.
pub struct JsProxyStream {
    /// The underlying JS socket object (e.g. ArtiSocket from the TS wrapper).
    socket: send_wrapper::SendWrapper<wasm_bindgen::JsValue>,
    /// Reader acquired from `socket.readable`.
    reader: send_wrapper::SendWrapper<web_sys::ReadableStreamDefaultReader>,
    /// Writer acquired from `socket.writable`.
    writer: send_wrapper::SendWrapper<web_sys::WritableStreamDefaultWriter>,
    /// In-flight `reader.read()` promise, held across polls.
    pending_read: Option<send_wrapper::SendWrapper<wasm_bindgen_futures::JsFuture>>,
    /// Leftover bytes from a chunk that didn't fit the caller's buffer.
    read_buf: Vec<u8>,
    /// Set once the read side has seen EOF (or errored).
    read_done: bool,
    /// In-flight `writer.write()`/`writer.close()` promise, held across polls.
    pending_write: Option<send_wrapper::SendWrapper<wasm_bindgen_futures::JsFuture>>,
    /// Set once poll_close has issued `writer.close()`.
    write_closing: bool,
    /// Set once the write side is fully closed.
    write_closed: bool,
}

impl Debug for JsProxyStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JsProxyStream").finish()
    }
}

impl JsProxyStream {
    /// Wrap a JS socket that exposes `readable`, `writable`, and `close()`.
    fn wrap(socket: wasm_bindgen::JsValue) -> IoResult<Self> {
        use wasm_bindgen::JsCast;

        let readable: web_sys::ReadableStream = js_sys::Reflect::get(&socket, &"readable".into())
            .map_err(|e| io::Error::other(format!("socket has no readable: {:?}", e)))?
            .dyn_into()
            .map_err(|_| io::Error::other("socket.readable is not a ReadableStream"))?;
        let reader: web_sys::ReadableStreamDefaultReader = readable
            .get_reader()
            .unchecked_into();

        let writable: web_sys::WritableStream = js_sys::Reflect::get(&socket, &"writable".into())
            .map_err(|e| io::Error::other(format!("socket has no writable: {:?}", e)))?
            .dyn_into()
            .map_err(|_| io::Error::other("socket.writable is not a WritableStream"))?;
        let writer = writable
            .get_writer()
            .map_err(|e| io::Error::other(format!("failed to acquire writer: {:?}", e)))?;

        Ok(Self {
            socket: send_wrapper::SendWrapper::new(socket),
            reader: send_wrapper::SendWrapper::new(reader),
            writer: send_wrapper::SendWrapper::new(writer),
            pending_read: None,
            read_buf: Vec::new(),
            read_done: false,
            pending_write: None,
            write_closing: false,
            write_closed: false,
        })
    }

    /// Call `socket.close()`.
    fn js_close(&self) -> IoResult<()> {
        use wasm_bindgen::JsCast;
        let close_fn = js_sys::Reflect::get(&self.socket, &"close".into())
            .map_err(|e| io::Error::other(format!("socket has no close: {:?}", e)))?;
        let close_fn: js_sys::Function = close_fn.dyn_into()
            .map_err(|_| io::Error::other("socket.close is not a function"))?;
        close_fn.call0(&self.socket)
            .map_err(|e| io::Error::other(format!("socket.close failed: {:?}", e)))?;
        Ok(())
    }
}

impl Drop for JsProxyStream {
    fn drop(&mut self) {
        // Cancel the reader to unblock any pending read, then close the socket.
        //
        // reader.cancel() returns a Promise that REJECTS when the stream was
        // already errored — which is the normal teardown order here: the KPS
        // connection is usually closed (erroring its open streams, per KPS SPEC
        // §9.2) before arti drops these stream objects. Dropping that rejected
        // promise uncaught surfaces as an unhandledRejection and crashes Node,
        // so drive it to completion and swallow the result.
        wasm_bindgen_futures::spawn_local({
            let cancel = self.reader.cancel();
            async move { let _ = wasm_bindgen_futures::JsFuture::from(cancel).await; }
        });
        let _ = self.js_close();
    }
}

// JsProxyStream is Send/Sync because JS types are wrapped in SendWrapper.

impl AsyncRead for JsProxyStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<IoResult<usize>> {
        use wasm_bindgen::JsCast;
        let this = self.get_mut();

        // Serve leftover bytes from a previous oversized chunk first.
        if !this.read_buf.is_empty() {
            let len = buf.len().min(this.read_buf.len());
            buf[..len].copy_from_slice(&this.read_buf[..len]);
            this.read_buf.drain(..len);
            return Poll::Ready(Ok(len));
        }
        if this.read_done {
            return Poll::Ready(Ok(0)); // EOF
        }

        loop {
            // Pull exactly one chunk on demand — no chunk is requested from the
            // network until arti calls poll_read, which is what carries KPS
            // backpressure back to the sender.
            if this.pending_read.is_none() {
                let promise = this.reader.read();
                this.pending_read =
                    Some(send_wrapper::SendWrapper::new(wasm_bindgen_futures::JsFuture::from(promise)));
            }

            let poll = {
                let fut = this.pending_read.as_mut().unwrap();
                Pin::new(&mut **fut).poll(cx)
            };
            match poll {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Ok(result)) => {
                    this.pending_read = None;
                    let done = js_sys::Reflect::get(&result, &"done".into())
                        .ok()
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    if done {
                        this.read_done = true;
                        return Poll::Ready(Ok(0)); // server FIN → EOF
                    }
                    let value = match js_sys::Reflect::get(&result, &"value".into()) {
                        Ok(v) => v,
                        Err(e) => {
                            return Poll::Ready(Err(io::Error::other(format!(
                                "read result has no value: {:?}",
                                e
                            ))))
                        }
                    };
                    let arr: js_sys::Uint8Array = match value.dyn_into() {
                        Ok(a) => a,
                        Err(_) => {
                            return Poll::Ready(Err(io::Error::other(
                                "read value is not a Uint8Array",
                            )))
                        }
                    };
                    let data = arr.to_vec();
                    if data.is_empty() {
                        // Empty chunk — request the next one.
                        continue;
                    }
                    let len = buf.len().min(data.len());
                    buf[..len].copy_from_slice(&data[..len]);
                    if len < data.len() {
                        this.read_buf.extend_from_slice(&data[len..]);
                    }
                    return Poll::Ready(Ok(len));
                }
                Poll::Ready(Err(e)) => {
                    this.pending_read = None;
                    this.read_done = true;
                    return Poll::Ready(Err(io::Error::other(format!("read failed: {:?}", e))));
                }
            }
        }
    }
}

impl JsProxyStream {
    /// Poll the in-flight write/close promise, if any. Returns `Pending` while
    /// it is outstanding — this is what backpressures the writer against a slow
    /// sink. Clears it and returns `Ready(Ok)` once it resolves.
    fn poll_pending_write(&mut self, cx: &mut Context<'_>) -> Poll<IoResult<()>> {
        if let Some(fut) = self.pending_write.as_mut() {
            match Pin::new(&mut **fut).poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Ok(_)) => self.pending_write = None,
                Poll::Ready(Err(e)) => {
                    self.pending_write = None;
                    return Poll::Ready(Err(io::Error::other(format!("write failed: {:?}", e))));
                }
            }
        }
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for JsProxyStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<IoResult<usize>> {
        let this = self.get_mut();
        // Wait for the previous write to be accepted before sending more; this
        // reflects the sink's backpressure back to arti.
        match this.poll_pending_write(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            Poll::Ready(Ok(())) => {}
        }
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let arr = js_sys::Uint8Array::from(buf);
        let promise = this.writer.write_with_chunk(arr.as_ref());
        this.pending_write =
            Some(send_wrapper::SendWrapper::new(wasm_bindgen_futures::JsFuture::from(promise)));
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<IoResult<()>> {
        self.get_mut().poll_pending_write(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<IoResult<()>> {
        let this = self.get_mut();
        loop {
            match this.poll_pending_write(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Ready(Ok(())) => {}
            }
            if this.write_closed {
                return Poll::Ready(Ok(()));
            }
            if !this.write_closing {
                // Issue writer.close() (sends the FIN); loop to await it.
                let promise = this.writer.close();
                this.pending_write =
                    Some(send_wrapper::SendWrapper::new(wasm_bindgen_futures::JsFuture::from(promise)));
                this.write_closing = true;
                continue;
            }
            // write_closing set and no pending promise ⇒ close resolved.
            this.write_closed = true;
            return Poll::Ready(Ok(()));
        }
    }
}

impl StreamOps for JsProxyStream {
    fn new_handle(&self) -> Box<dyn StreamOps + Send + Unpin> {
        Box::new(NoOpHandle)
    }
}

/// A no-op [`StreamOps`] handle.
///
/// tor-rtcompat ships `NoOpStreamOpsHandle` for this, but it's `#[non_exhaustive]`
/// and so can't be constructed from outside that crate. Since `StreamOps` has
/// all-default methods, a local unit type serves the same purpose.
#[derive(Clone, Copy, Debug)]
struct NoOpHandle;

impl StreamOps for NoOpHandle {}

/// A stub listener that never accepts connections.
/// WASM does not support listening on sockets.
#[non_exhaustive]
pub struct StubListener;

impl NetStreamListener<SocketAddr> for StubListener {
    type Stream = JsProxyStream;
    type Incoming = stream::Empty<IoResult<(Self::Stream, SocketAddr)>>;

    fn incoming(self) -> Self::Incoming {
        stream::empty()
    }

    fn local_addr(&self) -> IoResult<SocketAddr> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "StubListener has no local address",
        ))
    }
}

impl NetStreamListener<unix::SocketAddr> for StubListener {
    type Stream = JsProxyStream;
    type Incoming = stream::Empty<IoResult<(Self::Stream, unix::SocketAddr)>>;

    fn incoming(self) -> Self::Incoming {
        stream::empty()
    }

    fn local_addr(&self) -> IoResult<unix::SocketAddr> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "StubListener has no local address",
        ))
    }
}

#[async_trait]
impl NetStreamProvider<SocketAddr> for WasmRuntime {
    type Stream = JsProxyStream;
    type Listener = StubListener;
    type ConnectOptions = TcpConnectOptions;
    type ListenOptions = TcpListenOptions;

    // Socket options can't be applied to JS-proxied connections, so they are
    // ignored here, as with listen() below.
    async fn connect(
        &self,
        addr: &SocketAddr,
        _options: &Self::ConnectOptions,
    ) -> IoResult<Self::Stream> {
        let connect_fn = self.connect_fn.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "WasmRuntime: no connect function configured. \
                 Call set_connect_fn() to enable connections.",
            )
        })?;

        let addr_str = format!("{}", addr);
        tracing::debug!("WasmRuntime: connecting to {}", addr_str);

        // Call JS: connect_fn(addr) -> Promise<socket>
        let promise = connect_fn
            .call1(&wasm_bindgen::JsValue::NULL, &wasm_bindgen::JsValue::from_str(&addr_str))
            .map_err(|e| io::Error::other(format!("connect_fn call failed: {:?}", e)))?;

        let promise = js_sys::Promise::from(promise);
        let socket = SendFut(send_wrapper::SendWrapper::new(
            wasm_bindgen_futures::JsFuture::from(promise),
        ))
        .await
        .map_err(|e| io::Error::other(format!("connect failed: {:?}", e)))?;

        JsProxyStream::wrap(socket)
    }

    async fn listen(
        &self,
        _addr: &SocketAddr,
        _options: &Self::ListenOptions,
    ) -> IoResult<Self::Listener> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "WasmRuntime does not support listening on TCP sockets",
        ))
    }
}

#[async_trait]
impl NetStreamProvider<unix::SocketAddr> for WasmRuntime {
    type Stream = JsProxyStream;
    type Listener = StubListener;
    type ConnectOptions = UnixConnectOptions;
    type ListenOptions = UnixListenOptions;

    async fn connect(
        &self,
        _addr: &unix::SocketAddr,
        _options: &Self::ConnectOptions,
    ) -> IoResult<Self::Stream> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "WasmRuntime does not support Unix sockets",
        ))
    }

    async fn listen(
        &self,
        _addr: &unix::SocketAddr,
        _options: &Self::ListenOptions,
    ) -> IoResult<Self::Listener> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "WasmRuntime does not support Unix sockets",
        ))
    }
}

// ============================================================================
// TlsProvider implementation using rustls (with rustls-rustcrypto for WASM)
// ============================================================================

/// TLS connector for WASM using rustls with a pure-Rust crypto provider.
///
/// Configured for Tor's requirements:
/// - Skips certificate verification (Tor validates via CERTS cells instead)
/// - Verifies TLS handshake signatures (proves key possession)
pub struct WasmTlsConnector {
    /// The underlying TLS connector.
    connector: futures_rustls::TlsConnector,
}

impl WasmTlsConnector {
    /// Create a new WASM TLS connector.
    ///
    /// This connector skips certificate verification since Tor uses its own
    /// certificate validation via CERTS cells in the Tor protocol.
    pub fn new() -> Self {
        use futures_rustls::rustls;

        let provider = rustls_rustcrypto::provider();
        let algorithms = provider.signature_verification_algorithms;
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(provider))
            .with_safe_default_protocol_versions()
            .expect("default protocol versions should be supported")
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(TorCertVerifier(algorithms)))
            .with_no_client_auth();

        Self {
            connector: futures_rustls::TlsConnector::from(Arc::new(config)),
        }
    }
}

impl Default for WasmTlsConnector {
    fn default() -> Self {
        Self::new()
    }
}

/// A certificate verifier that skips WebPKI validation.
///
/// Tor relays use self-signed certificates; authentication happens via CERTS
/// cells in the Tor protocol. We still verify TLS handshake signatures to
/// prove the server possesses the key in its certificate.
#[derive(Debug)]
pub struct TorCertVerifier(futures_rustls::rustls::crypto::WebPkiSupportedAlgorithms);

impl futures_rustls::rustls::client::danger::ServerCertVerifier for TorCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls_pki_types::CertificateDer,
        _intermediates: &[rustls_pki_types::CertificateDer],
        _server_name: &rustls_pki_types::ServerName,
        _ocsp_response: &[u8],
        _now: rustls_pki_types::UnixTime,
    ) -> Result<futures_rustls::rustls::client::danger::ServerCertVerified, futures_rustls::rustls::Error> {
        // Skip cert validation — Tor validates via CERTS cells
        Ok(futures_rustls::rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls_pki_types::CertificateDer,
        dss: &futures_rustls::rustls::DigitallySignedStruct,
    ) -> Result<futures_rustls::rustls::client::danger::HandshakeSignatureValid, futures_rustls::rustls::Error> {
        futures_rustls::rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls_pki_types::CertificateDer,
        dss: &futures_rustls::rustls::DigitallySignedStruct,
    ) -> Result<futures_rustls::rustls::client::danger::HandshakeSignatureValid, futures_rustls::rustls::Error> {
        futures_rustls::rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0)
    }

    fn supported_verify_schemes(&self) -> Vec<futures_rustls::rustls::SignatureScheme> {
        self.0.supported_schemes()
    }

    fn root_hint_subjects(&self) -> Option<&[futures_rustls::rustls::DistinguishedName]> {
        None
    }
}

#[async_trait]
impl<S> TlsConnector<S> for WasmTlsConnector
where
    S: AsyncRead + AsyncWrite + StreamOps + Unpin + Send + 'static,
{
    type Conn = WasmTlsStream<S>;

    async fn negotiate_unvalidated(
        &self,
        stream: S,
        sni_hostname: &str,
    ) -> IoResult<Self::Conn> {
        let name: rustls_pki_types::ServerName<'_> = sni_hostname
            .try_into()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let conn = self.connector.connect(name.to_owned(), stream).await?;
        Ok(WasmTlsStream(conn))
    }
}

/// An uninhabitable TLS type for server-side TLS, which WASM does not support.
///
/// This wraps `void::Void` so it can never be constructed. All trait methods
/// use `void::unreachable()` to prove at compile time that the code paths are
/// impossible. This is the same pattern as `UnimplementedTls` in
/// `impls/unimpl_tls.rs`, duplicated here because that module is not compiled
/// for WASM targets.
#[derive(Clone, Debug)]
pub struct WasmUnimplementedTls(void::Void);

#[async_trait]
impl<S: Send + 'static> TlsConnector<S> for WasmUnimplementedTls {
    type Conn = WasmUnimplementedTls;

    async fn negotiate_unvalidated(&self, _stream: S, _sni_hostname: &str) -> IoResult<Self::Conn> {
        void::unreachable(self.0)
    }
}

impl CertifiedConn for WasmUnimplementedTls {
    fn export_keying_material(
        &self,
        _len: usize,
        _label: &[u8],
        _context: Option<&[u8]>,
    ) -> IoResult<Vec<u8>> {
        void::unreachable(self.0)
    }

    fn peer_certificate(&self) -> IoResult<Option<Cow<'_, [u8]>>> {
        void::unreachable(self.0)
    }

    fn own_certificate(&self) -> IoResult<Option<Cow<'_, [u8]>>> {
        void::unreachable(self.0)
    }
}

impl AsyncRead for WasmUnimplementedTls {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &mut [u8],
    ) -> Poll<IoResult<usize>> {
        void::unreachable(self.0)
    }
}

impl AsyncWrite for WasmUnimplementedTls {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buf: &[u8],
    ) -> Poll<IoResult<usize>> {
        void::unreachable(self.0)
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<IoResult<()>> {
        void::unreachable(self.0)
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<IoResult<()>> {
        void::unreachable(self.0)
    }
}

impl StreamOps for WasmUnimplementedTls {}

impl<S> TlsProvider<S> for WasmRuntime
where
    S: AsyncRead + AsyncWrite + StreamOps + Unpin + Send + 'static,
{
    type Connector = WasmTlsConnector;
    type TlsStream = WasmTlsStream<S>;
    type Acceptor = WasmUnimplementedTls;
    type TlsServerStream = WasmUnimplementedTls;

    fn tls_connector(&self) -> Self::Connector {
        WasmTlsConnector::new()
    }

    fn tls_acceptor(&self, _settings: TlsAcceptorSettings) -> IoResult<Self::Acceptor> {
        // tor-rtcompat's TlsServerUnsupported error type can't be constructed
        // from outside that crate, so return an equivalent io::Error directly.
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "WasmRuntime does not support running as a TLS server",
        ))
    }

    fn supports_keying_material_export(&self) -> bool {
        true
    }
}

/// A newtype wrapper around `futures_rustls`'s client TLS stream.
///
/// arti needs the relay TLS stream to implement tor-rtcompat's [`StreamOps`]
/// and [`CertifiedConn`]. Both those traits and
/// `futures_rustls::client::TlsStream` are foreign to this crate, so the orphan
/// rule forbids implementing them on the stream directly — that's only legal
/// inside tor-rtcompat. Wrapping it in this local type lets us provide the
/// impls here. (In arti the equivalent native impls live in impls/rustls.rs.)
pub struct WasmTlsStream<S>(futures_rustls::client::TlsStream<S>);

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for WasmTlsStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<IoResult<usize>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for WasmTlsStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<IoResult<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<IoResult<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<IoResult<()>> {
        Pin::new(&mut self.0).poll_close(cx)
    }
}

impl<S> StreamOps for WasmTlsStream<S> where S: AsyncRead + AsyncWrite + Unpin {}

impl<S> CertifiedConn for WasmTlsStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn peer_certificate(&self) -> IoResult<Option<Cow<'_, [u8]>>> {
        let (_, session) = self.0.get_ref();
        Ok(session
            .peer_certificates()
            .and_then(|certs| certs.first().map(|c| Cow::from(c.as_ref()))))
    }

    fn own_certificate(&self) -> IoResult<Option<Cow<'_, [u8]>>> {
        Ok(None)
    }

    fn export_keying_material(
        &self,
        len: usize,
        label: &[u8],
        context: Option<&[u8]>,
    ) -> IoResult<Vec<u8>> {
        let (_, session) = self.0.get_ref();
        session
            .export_keying_material(Vec::with_capacity(len), label, context)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

// ============================================================================
// UdpProvider implementation (STUBBED)
// ============================================================================

/// A stub UDP socket that always returns errors.
#[non_exhaustive]
pub struct StubUdpSocket;

#[async_trait]
impl UdpSocket for StubUdpSocket {
    async fn recv(&self, _buf: &mut [u8]) -> IoResult<(usize, SocketAddr)> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "WasmRuntime does not support UDP sockets",
        ))
    }

    async fn send(&self, _buf: &[u8], _target: &SocketAddr) -> IoResult<usize> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "WasmRuntime does not support UDP sockets",
        ))
    }

    fn local_addr(&self) -> IoResult<SocketAddr> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "StubUdpSocket has no local address",
        ))
    }
}

#[async_trait]
impl UdpProvider for WasmRuntime {
    type UdpSocket = StubUdpSocket;

    async fn bind(&self, _addr: &SocketAddr) -> IoResult<Self::UdpSocket> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "WasmRuntime does not support UDP sockets",
        ))
    }
}
