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
    NetStreamProvider, RealCoarseTimeProvider, SleepProvider, StreamOps, TlsProvider, UdpProvider,
    UdpSocket,
};
// ...while these TLS traits are only reachable via the public `tls` submodule.
use tor_rtcompat::tls::{TlsAcceptorSettings, TlsConnector};
use std::borrow::Cow;
use async_trait::async_trait;
use futures::task::{Spawn, SpawnError};
use futures::{stream, AsyncRead, AsyncWrite, Future, StreamExt};
use wasm_bindgen::JsCast;
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

/// A stream backed by a JS socket object (WebSocket, WebRTC data channel, etc.).
///
/// Owned JS closures kept alive for the socket's lifetime.
type JsClosures = send_wrapper::SendWrapper<Vec<wasm_bindgen::closure::Closure<dyn FnMut(wasm_bindgen::JsValue)>>>;

/// When a [`WasmRuntime`] has a connect function configured via
/// [`WasmRuntime::set_connect_fn`], calls to `connect(addr)` invoke the JS
/// callback and wrap the returned socket object as this stream.
///
/// The JS socket must implement: `send(Uint8Array)`, `onmessage` setter,
/// `onclose` setter, and `close()`.
#[allow(dead_code)] // Fields held to keep JS callbacks alive
pub struct JsProxyStream {
    /// The underlying JS socket object (e.g. ArtiSocket from the TS wrapper).
    socket: send_wrapper::SendWrapper<wasm_bindgen::JsValue>,
    /// Channel receiving incoming data chunks from the JS onmessage callback.
    rx: futures_channel::mpsc::UnboundedReceiver<IoResult<Vec<u8>>>,
    /// Leftover bytes from a previous read that didn't fit the caller's buffer.
    buffer: Vec<u8>,
    /// Prevent the JS closures (onmessage, onclose) from being garbage collected.
    _closures: JsClosures,
}

impl Debug for JsProxyStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JsProxyStream").finish()
    }
}

impl JsProxyStream {
    /// Wrap a JS socket object that has `send`, `onmessage`, `onclose`, `close`.
    fn wrap(socket: wasm_bindgen::JsValue) -> IoResult<Self> {
        use wasm_bindgen::prelude::*;
        use wasm_bindgen::JsCast;

        let (tx, rx) = futures_channel::mpsc::unbounded();

        // Set onmessage — receives Uint8Array chunks
        let tx_msg = tx.clone();
        let on_message = Closure::wrap(Box::new(move |data: wasm_bindgen::JsValue| {
            if let Ok(arr) = data.dyn_into::<js_sys::Uint8Array>() {
                let _ = tx_msg.unbounded_send(Ok(arr.to_vec()));
            }
        }) as Box<dyn FnMut(wasm_bindgen::JsValue)>);
        js_sys::Reflect::set(&socket, &"onmessage".into(), on_message.as_ref())
            .map_err(|e| io::Error::other(format!("failed to set onmessage: {:?}", e)))?;

        // Set onclose — signals EOF on the read channel
        let tx_close = tx;
        let on_close = Closure::wrap(Box::new(move |_: wasm_bindgen::JsValue| {
            tx_close.close_channel();
        }) as Box<dyn FnMut(wasm_bindgen::JsValue)>);
        js_sys::Reflect::set(&socket, &"onclose".into(), on_close.as_ref())
            .map_err(|e| io::Error::other(format!("failed to set onclose: {:?}", e)))?;

        Ok(Self {
            socket: send_wrapper::SendWrapper::new(socket),
            rx,
            buffer: Vec::new(),
            _closures: send_wrapper::SendWrapper::new(vec![on_message, on_close]),
        })
    }

    /// Call `socket.send(data)`.
    fn js_send(&self, data: &[u8]) -> IoResult<()> {
        let send_fn = js_sys::Reflect::get(&self.socket, &"send".into())
            .map_err(|e| io::Error::other(format!("socket has no send: {:?}", e)))?;
        let send_fn: js_sys::Function = send_fn.dyn_into()
            .map_err(|_| io::Error::other("socket.send is not a function"))?;
        let arr = js_sys::Uint8Array::from(data);
        send_fn.call1(&self.socket, &arr)
            .map_err(|e| io::Error::other(format!("socket.send failed: {:?}", e)))?;
        Ok(())
    }

    /// Call `socket.close()`.
    fn js_close(&self) -> IoResult<()> {
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
        // Clear callbacks before dropping closures to prevent use-after-free
        let _ = js_sys::Reflect::set(&self.socket, &"onmessage".into(), &wasm_bindgen::JsValue::NULL);
        let _ = js_sys::Reflect::set(&self.socket, &"onclose".into(), &wasm_bindgen::JsValue::NULL);
        let _ = self.js_close();
    }
}

// JsProxyStream is Send/Sync because JS types are wrapped in SendWrapper.

impl AsyncRead for JsProxyStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<IoResult<usize>> {
        // Drain internal buffer first
        if !self.buffer.is_empty() {
            let len = buf.len().min(self.buffer.len());
            buf[..len].copy_from_slice(&self.buffer[..len]);
            self.buffer.drain(..len);
            return Poll::Ready(Ok(len));
        }

        match self.rx.poll_next_unpin(cx) {
            Poll::Ready(Some(Ok(data))) => {
                if data.is_empty() {
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }
                let len = buf.len().min(data.len());
                buf[..len].copy_from_slice(&data[..len]);
                if len < data.len() {
                    self.buffer.extend_from_slice(&data[len..]);
                }
                Poll::Ready(Ok(len))
            }
            Poll::Ready(Some(Err(e))) => Poll::Ready(Err(e)),
            Poll::Ready(None) => Poll::Ready(Ok(0)), // EOF
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for JsProxyStream {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<IoResult<usize>> {
        self.js_send(buf).map(|_| buf.len()).into()
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<IoResult<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<IoResult<()>> {
        self.js_close().into()
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

    async fn connect(&self, addr: &SocketAddr) -> IoResult<Self::Stream> {
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

    async fn listen(&self, _addr: &SocketAddr) -> IoResult<Self::Listener> {
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

    async fn connect(&self, _addr: &unix::SocketAddr) -> IoResult<Self::Stream> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "WasmRuntime does not support Unix sockets",
        ))
    }

    async fn listen(&self, _addr: &unix::SocketAddr) -> IoResult<Self::Listener> {
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
