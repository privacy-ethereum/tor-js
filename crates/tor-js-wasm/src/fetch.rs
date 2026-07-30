//! HTTP fetch implementation over arti-client DataStream
//!
//! This module implements HTTP/1.1 requests over Tor streams,
//! with TLS support via rustls for HTTPS.
//!
//! The fetch resolves as soon as response headers are received.
//! Body reading is deferred — chunks are read incrementally via `read_chunk()`.

use crate::error::JsTorError;
use futures::io::{AsyncReadExt, AsyncWriteExt};
use futures::stream::{Stream, StreamExt};
use http::Method;
use std::collections::HashMap;
use std::pin::Pin;
use tracing::{debug, info};
use url::Url;

/// The request body source handed to {@link fetch_headers}.
pub enum RequestBody {
    /// No request body.
    None,
    /// A fully-materialized body, sent with `Content-Length`.
    Bytes(Vec<u8>),
    /// A streaming body, sent with `Transfer-Encoding: chunked`. Boxed-local
    /// (JS-backed streams are `!Send`; the fetch future runs via `spawn_local`,
    /// so `Send` is not required). Each item is one body chunk.
    Stream(Pin<Box<dyn Stream<Item = Result<Vec<u8>, JsTorError>>>>),
}

/// Get navigator.userAgent if available (returns None in Node.js/Deno).
#[cfg(not(test))]
fn get_navigator_user_agent() -> Option<String> {
    let navigator = js_sys::Reflect::get(&js_sys::global(), &"navigator".into()).ok()?;
    if navigator.is_undefined() || navigator.is_null() {
        return None;
    }
    let ua = js_sys::Reflect::get(&navigator, &"userAgent".into()).ok()?;
    ua.as_string()
}

/// Under `cargo test` there is no JS global to reach for, so this stands in for
/// the Node.js/Deno case (no `navigator`, therefore no `User-Agent`).
#[cfg(test)]
fn get_navigator_user_agent() -> Option<String> {
    None
}

/// Check if a header key exists in the map (case-insensitive).
fn has_header(headers: &HashMap<String, String>, name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    headers.keys().any(|k| k.to_ascii_lowercase() == lower)
}

/// Maximum response header size (64KB)
const MAX_HEADER_SIZE: usize = 64 * 1024;

/// Type-erased async reader for the response body stream.
pub type BoxedReader = Box<dyn futures::io::AsyncRead + Unpin>;

/// How the response body is framed.
#[derive(Debug, PartialEq, Eq)]
pub enum BodyFraming {
    /// Content-Length header present: read exactly N bytes.
    ContentLength(usize),
    /// Transfer-Encoding: chunked.
    Chunked,
    /// No framing info: read until EOF (Connection: close).
    UntilEof,
    /// No body expected (HEAD response, 204, 304, 1xx).
    None,
}

/// Result of the header phase of a fetch request.
pub struct FetchHeadersResult {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body_reader: BodyReader,
}

/// Reads the HTTP response body from a stream.
///
/// Created after headers are parsed, holds the stream and any overflow
/// bytes that were read past the header separator. Supports both
/// chunk-by-chunk reading (`read_chunk()`) and full body reading (`read_all()`).
pub struct BodyReader {
    stream: BoxedReader,
    framing: BodyFraming,
    /// Bytes already read past \r\n\r\n during header parsing.
    buffer: Vec<u8>,
    done: bool,
    total_read: usize,
    /// Chunked decoder: bytes remaining in the current HTTP chunk.
    chunk_remaining: usize,
    /// Chunked decoder: whether we're waiting for the trailing \r\n after chunk data.
    awaiting_chunk_crlf: bool,
}

impl BodyReader {
    pub fn new(stream: BoxedReader, framing: BodyFraming, overflow: Vec<u8>) -> Self {
        Self {
            stream,
            framing,
            buffer: overflow,
            done: false,
            total_read: 0,
            chunk_remaining: 0,
            awaiting_chunk_crlf: false,
        }
    }

    /// Read the next chunk of decoded body bytes. Returns `None` at EOF.
    pub async fn read_chunk(&mut self) -> Result<Option<Vec<u8>>, JsTorError> {
        if self.done {
            return Ok(None);
        }

        match &self.framing {
            BodyFraming::None => {
                self.done = true;
                Ok(None)
            }
            BodyFraming::ContentLength(len) => {
                let len = *len;
                self.read_chunk_content_length(len).await
            }
            BodyFraming::Chunked => self.read_chunk_chunked().await,
            BodyFraming::UntilEof => self.read_chunk_eof().await,
        }
    }

    /// Read the entire remaining body by calling `read_chunk()` in a loop.
    #[allow(dead_code)]
    pub async fn read_all(&mut self) -> Result<Vec<u8>, JsTorError> {
        let mut body = Vec::new();
        while let Some(chunk) = self.read_chunk().await? {
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }

    /// Read the next chunk for Content-Length framing.
    async fn read_chunk_content_length(&mut self, total_len: usize) -> Result<Option<Vec<u8>>, JsTorError> {
        let remaining = total_len.saturating_sub(self.total_read);
        if remaining == 0 {
            self.done = true;
            return Ok(None);
        }

        // Drain overflow buffer first
        if !self.buffer.is_empty() {
            let take = std::cmp::min(self.buffer.len(), remaining);
            let chunk: Vec<u8> = self.buffer.drain(..take).collect();
            self.total_read += chunk.len();
            if self.total_read >= total_len {
                self.done = true;
            }
            return Ok(Some(chunk));
        }

        let read_size = std::cmp::min(8192, remaining);
        let mut buf = vec![0u8; read_size];
        match self.stream.read(&mut buf).await {
            // EOF with bytes still owed: the body is short. Reporting this as a
            // clean end would hand the caller a silently truncated response.
            Ok(0) => {
                self.done = true;
                Err(JsTorError::http_request(format!(
                    "Response body truncated: got {} of {} bytes",
                    self.total_read, total_len
                )))
            }
            Ok(n) => {
                let take = std::cmp::min(n, remaining);
                buf.truncate(take);
                self.total_read += take;
                if self.total_read >= total_len {
                    self.done = true;
                }
                Ok(Some(buf))
            }
            Err(e) => {
                self.done = true;
                Err(JsTorError::http_request(format!("Failed to read body: {}", e)))
            }
        }
    }

    /// Read the next chunk for EOF-terminated framing.
    async fn read_chunk_eof(&mut self) -> Result<Option<Vec<u8>>, JsTorError> {
        // Drain overflow buffer first
        if !self.buffer.is_empty() {
            let chunk = std::mem::take(&mut self.buffer);
            // Counted so a later read error is treated as a close, matching the
            // case where the same bytes arrived from the stream itself.
            self.total_read += chunk.len();
            return Ok(Some(chunk));
        }

        let mut buf = [0u8; 8192];
        match self.stream.read(&mut buf).await {
            Ok(0) => {
                self.done = true;
                Ok(None)
            }
            Ok(n) => {
                self.total_read += n;
                Ok(Some(buf[..n].to_vec()))
            }
            Err(e) => {
                self.done = true;
                if self.total_read > 0 {
                    // Had some data, treat error as EOF
                    debug!("Read ended with error (may be normal close): {}", e);
                    Ok(None)
                } else {
                    Err(JsTorError::http_request(format!("Failed to read body: {}", e)))
                }
            }
        }
    }

    /// Read the next chunk for chunked transfer-encoding.
    ///
    /// Uses persistent state fields (`chunk_remaining`, `awaiting_chunk_crlf`)
    /// to decode HTTP chunks incrementally across calls.
    async fn read_chunk_chunked(&mut self) -> Result<Option<Vec<u8>>, JsTorError> {
        loop {
            // Step 1: If we have data remaining in the current HTTP chunk, return it
            if self.chunk_remaining > 0 {
                let available = if !self.buffer.is_empty() {
                    let take = std::cmp::min(self.buffer.len(), self.chunk_remaining);
                    let chunk: Vec<u8> = self.buffer.drain(..take).collect();
                    self.chunk_remaining -= chunk.len();
        
                    if self.chunk_remaining == 0 {
                        self.awaiting_chunk_crlf = true;
                    }
                    chunk
                } else {
                    let read_size = std::cmp::min(8192, self.chunk_remaining);
                    let mut buf = vec![0u8; read_size];
                    let n = self.fill_buf(&mut buf).await?;
                    if n == 0 {
                        self.done = true;
                        return Err(JsTorError::http_request(format!(
                            "Connection closed with {} bytes remaining in chunk",
                            self.chunk_remaining
                        )));
                    }
                    buf.truncate(n);
                    self.chunk_remaining -= n;

                    if self.chunk_remaining == 0 {
                        self.awaiting_chunk_crlf = true;
                    }
                    buf
                };

                return Ok(Some(available));
            }

            // Step 2: Consume trailing \r\n after chunk data
            if self.awaiting_chunk_crlf {
                // Ensure we have at least 2 bytes in buffer
                while self.buffer.len() < 2 {
                    let n = self.fill_buffer_from_stream().await?;
                    if n == 0 {
                        // The terminating chunk never arrived, so the body is
                        // incomplete however many bytes we handed over already.
                        self.done = true;
                        return Err(JsTorError::http_request(
                            "Connection closed before the CRLF following a chunk",
                        ));
                    }
                }
                if &self.buffer[..2] != b"\r\n" {
                    self.done = true;
                    return Err(JsTorError::http_request(
                        "Malformed chunked body: expected CRLF after chunk data",
                    ));
                }
                self.buffer.drain(..2);
                self.awaiting_chunk_crlf = false;
            }

            // Step 3: Read chunk size line
            loop {
                if let Some(pos) = find_crlf(&self.buffer) {
                    let size_str = std::str::from_utf8(&self.buffer[..pos])
                        .map_err(|_| JsTorError::http_request("Chunk size not UTF-8"))?;
                    let size_str = size_str.split(';').next().unwrap_or("").trim();

                    if size_str.is_empty() {
                        self.buffer.drain(..pos + 2);
                        continue;
                    }

                    let size = usize::from_str_radix(size_str, 16).map_err(|e| {
                        JsTorError::http_request(format!("Invalid chunk size '{}': {}", size_str, e))
                    })?;

                    self.buffer.drain(..pos + 2);

                    if size == 0 {
                        // Terminal chunk
                        self.done = true;
                        return Ok(None);
                    }

                    self.chunk_remaining = size;
                    break; // Go back to step 1 to read chunk data
                } else {
                    // Need more data for the size line
                    let n = self.fill_buffer_from_stream().await?;
                    if n == 0 {
                        // A well-formed body ends at the `0` chunk, which
                        // returns above; reaching EOF here means it was cut off.
                        self.done = true;
                        return Err(JsTorError::http_request(
                            "Connection closed before the terminating chunk",
                        ));
                    }
                }
            }
        }
    }

    /// Read from stream into provided buffer, returning bytes read.
    async fn fill_buf(&mut self, buf: &mut [u8]) -> Result<usize, JsTorError> {
        self.stream.read(buf).await.map_err(|e| {
            JsTorError::http_request(format!("Failed to read response: {}", e))
        })
    }

    /// Read from stream and append to self.buffer.
    async fn fill_buffer_from_stream(&mut self) -> Result<usize, JsTorError> {
        let mut buf = [0u8; 8192];
        let n = self.fill_buf(&mut buf).await?;
        if n > 0 {
            self.buffer.extend_from_slice(&buf[..n]);
        }
        Ok(n)
    }
}

/// Build an HTTP/1.1 request as raw bytes
pub fn build_request_head(
    url: &Url,
    method: &Method,
    headers: &HashMap<String, String>,
    body: &RequestBody,
) -> Result<Vec<u8>, JsTorError> {
    let host = url.host_str().unwrap_or("localhost");
    let path = if url.path().is_empty() {
        "/"
    } else {
        url.path()
    };

    let query = url.query().map(|q| format!("?{}", q)).unwrap_or_default();

    let mut request = format!(
        "{} {}{} HTTP/1.1\r\nHost: {}\r\n",
        method.as_str(),
        path,
        query,
        host
    );

    // Add default headers if not present (case-insensitive checks)
    if !has_header(headers, "User-Agent") {
        // Forward the browser's User-Agent to blend in with normal traffic.
        // In Node.js/Deno, navigator.userAgent is undefined — send no UA.
        if let Some(ua) = get_navigator_user_agent() {
            request.push_str(&format!("User-Agent: {}\r\n", ua));
        }
    }
    if !has_header(headers, "Accept") {
        request.push_str("Accept: */*\r\n");
    }
    if !has_header(headers, "Connection") {
        request.push_str("Connection: close\r\n");
    }

    // Add custom headers (reject any with CR/LF to prevent header injection).
    // Skip Host and Content-Length — we set these ourselves.
    for (key, value) in headers {
        if key.contains('\r') || key.contains('\n') || value.contains('\r') || value.contains('\n')
        {
            return Err(JsTorError::new(
                "INVALID_HEADER",
                "validation",
                format!("Header contains invalid CR/LF characters: {}", key),
                false,
            ));
        }
        let lower = key.to_ascii_lowercase();
        if lower == "host" {
            if value != host {
                tracing::warn!("Ignoring caller's Host header '{}' (using '{}')", value, host);
            }
            continue;
        }
        if lower == "content-length" || lower == "transfer-encoding" {
            // We set framing (Content-Length or chunked) ourselves from the body.
            continue;
        }
        request.push_str(&format!("{}: {}\r\n", key, value));
    }

    // Framing: Content-Length for a known body, chunked for a stream.
    match body {
        RequestBody::Bytes(b) => request.push_str(&format!("Content-Length: {}\r\n", b.len())),
        RequestBody::Stream(_) => request.push_str("Transfer-Encoding: chunked\r\n"),
        RequestBody::None => {}
    }

    // End headers; the body (if any) is written separately by write_request.
    request.push_str("\r\n");
    Ok(request.into_bytes())
}

/// Write the request head, then the body — raw bytes for `Bytes`, or
/// `Transfer-Encoding: chunked` frames for `Stream` — then flush. Streaming a
/// chunk at a time keeps backpressure on the Tor stream, so a large upload is
/// never fully buffered in memory.
async fn write_request<S>(stream: &mut S, head: &[u8], body: RequestBody) -> Result<(), JsTorError>
where
    S: futures::io::AsyncWrite + Unpin,
{
    stream
        .write_all(head)
        .await
        .map_err(|e| JsTorError::http_request(format!("Failed to write request head: {}", e)))?;

    match body {
        RequestBody::None => {}
        RequestBody::Bytes(b) => {
            stream
                .write_all(&b)
                .await
                .map_err(|e| JsTorError::http_request(format!("Failed to write request body: {}", e)))?;
        }
        RequestBody::Stream(mut s) => {
            while let Some(chunk) = s.next().await {
                let chunk = chunk?;
                if chunk.is_empty() {
                    continue; // a zero-length chunk would prematurely terminate the body
                }
                stream
                    .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
                    .await
                    .map_err(|e| JsTorError::http_request(format!("Failed to write chunk size: {}", e)))?;
                stream
                    .write_all(&chunk)
                    .await
                    .map_err(|e| JsTorError::http_request(format!("Failed to write chunk: {}", e)))?;
                stream
                    .write_all(b"\r\n")
                    .await
                    .map_err(|e| JsTorError::http_request(format!("Failed to write chunk CRLF: {}", e)))?;
            }
            stream
                .write_all(b"0\r\n\r\n")
                .await
                .map_err(|e| JsTorError::http_request(format!("Failed to write final chunk: {}", e)))?;
        }
    }

    stream
        .flush()
        .await
        .map_err(|e| JsTorError::http_request(format!("Failed to flush request: {}", e)))?;
    Ok(())
}

/// Write the HTTP request and read response headers.
///
/// Returns the parsed status/headers, the body framing mode, and any overflow
/// bytes read past the `\r\n\r\n` header separator. The stream is borrowed
/// mutably so the caller retains ownership for body reading.
async fn read_response_headers<S>(
    stream: &mut S,
    method: &Method,
) -> Result<(u16, Vec<(String, String)>, BodyFraming, Vec<u8>), JsTorError>
where
    S: futures::io::AsyncRead + Unpin,
{
    // Read response headers, skipping 1xx interim responses (except 101).
    let mut header_buf = Vec::new();
    let status: u16;
    let mut headers: Vec<(String, String)> = Vec::new();
    let overflow: Vec<u8>;

    loop {
        // Read until we find \r\n\r\n (header/body separator)
        let mut buf = [0u8; 8192];
        let header_end;

        loop {
            // Check what we already hold before reading. After a 1xx the
            // carried-over bytes can already contain the whole final response,
            // and reading first would block for data the server has no reason
            // to send.
            if let Some(pos) = find_subsequence(&header_buf, b"\r\n\r\n") {
                header_end = pos;
                break;
            }

            if header_buf.len() > MAX_HEADER_SIZE {
                return Err(JsTorError::http_request(format!(
                    "Response headers exceed {}KB limit",
                    MAX_HEADER_SIZE / 1024
                )));
            }

            match stream.read(&mut buf).await {
                Ok(0) => {
                    return Err(JsTorError::http_request(
                        "Connection closed before headers received",
                    ));
                }
                Ok(n) => header_buf.extend_from_slice(&buf[..n]),
                Err(e) => {
                    return Err(JsTorError::http_request(format!(
                        "Failed to read response headers: {}",
                        e
                    )));
                }
            }
        }

        // Split headers from overflow body bytes
        let header_bytes = &header_buf[..header_end];
        let remaining = header_buf[header_end + 4..].to_vec();

        // Parse headers
        let header_str = std::str::from_utf8(header_bytes)
            .map_err(|e| JsTorError::http_request(format!("Invalid HTTP headers: {}", e)))?;

        let mut lines = header_str.lines();

        // Parse status line: "HTTP/1.1 200 OK"
        let status_line = lines
            .next()
            .ok_or_else(|| JsTorError::http_request("Invalid HTTP response: no status line"))?;

        let parts: Vec<&str> = status_line.splitn(3, ' ').collect();
        if parts.len() < 2 {
            return Err(JsTorError::http_request("Invalid HTTP status line"));
        }

        let parsed_status: u16 = parts[1]
            .parse()
            .map_err(|e| JsTorError::http_request(format!("Invalid status code: {}", e)))?;

        // Handle 1xx interim responses
        if (100..200).contains(&parsed_status) {
            if parsed_status == 101 {
                return Err(JsTorError::http_request(
                    "101 Switching Protocols not supported",
                ));
            }
            // Discard 1xx headers and re-read the next response.
            // Any overflow bytes are the start of the next response.
            header_buf = remaining;
            continue;
        }

        // Final response
        status = parsed_status;
        headers.clear();
        for line in lines {
            if let Some((key, value)) = line.split_once(':') {
                headers.push((key.trim().to_lowercase(), value.trim().to_string()));
            }
        }
        overflow = remaining;
        break;
    }

    // Determine body framing
    let framing = if *method == Method::HEAD
        || status == 204
        || status == 304
    {
        BodyFraming::None
    } else if headers
        .iter()
        .any(|(k, v)| k == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked"))
    {
        debug!("Body framing: chunked");
        BodyFraming::Chunked
    } else if let Some((_, cl)) = headers.iter().find(|(k, _)| k == "content-length") {
        let len: usize = cl
            .parse()
            .map_err(|e| JsTorError::http_request(format!("Invalid content-length: {}", e)))?;
        debug!("Body framing: content-length {}", len);
        BodyFraming::ContentLength(len)
    } else {
        debug!("Body framing: read until EOF");
        BodyFraming::UntilEof
    };

    debug!(
        "Parsed response headers: status={}, headers={}, overflow_bytes={}",
        status,
        headers.len(),
        overflow.len()
    );

    Ok((status, headers, framing, overflow))
}

/// Perform an HTTP fetch over a Tor stream, resolving as soon as headers arrive.
///
/// The returned `FetchHeadersResult` contains parsed headers and a `BodyReader`
/// that can be used to read the body asynchronously.
#[expect(clippy::too_many_arguments, reason = "HTTP fetch requires many parameters")]
pub async fn fetch_headers<S>(
    stream: S,
    url: &Url,
    method: Method,
    headers: HashMap<String, String>,
    body: RequestBody,
    is_https: bool,
    host: &str,
    tls_config: Option<std::sync::Arc<futures_rustls::rustls::ClientConfig>>,
) -> Result<FetchHeadersResult, JsTorError>
where
    S: futures::io::AsyncRead + futures::io::AsyncWrite + Unpin + 'static,
{
    let head = build_request_head(url, &method, &headers, &body)?;
    debug!("Sending {}-byte request head", head.len());

    if is_https {
        let tls_config = tls_config.ok_or_else(|| {
            JsTorError::tls("HTTPS requested but no TLS config provided")
        })?;

        let connector = futures_rustls::TlsConnector::from(tls_config);
        let server_name = rustls_pki_types::ServerName::try_from(host.to_string())
            .map_err(|e| JsTorError::tls(format!("Invalid server name '{}': {}", host, e)))?;

        let mut tls_stream = connector
            .connect(server_name, stream)
            .await
            .map_err(|e| {
                JsTorError::tls(format!("TLS handshake failed with {}: {}", host, e))
            })?;
        info!("TLS connection established with {}", host);

        write_request(&mut tls_stream, &head, body).await?;
        let (status, resp_headers, framing, overflow) =
            read_response_headers(&mut tls_stream, &method).await?;

        info!("Received response headers: status={}", status);

        let reader: BoxedReader = Box::new(tls_stream);
        Ok(FetchHeadersResult {
            status,
            headers: resp_headers,
            body_reader: BodyReader::new(reader, framing, overflow),
        })
    } else {
        let mut stream = stream;

        write_request(&mut stream, &head, body).await?;
        let (status, resp_headers, framing, overflow) =
            read_response_headers(&mut stream, &method).await?;

        info!("Received response headers: status={}", status);

        let reader: BoxedReader = Box::new(stream);
        Ok(FetchHeadersResult {
            status,
            headers: resp_headers,
            body_reader: BodyReader::new(reader, framing, overflow),
        })
    }
}

/// Find the position of a subsequence in a byte slice
fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Find the position of \r\n in a byte slice
fn find_crlf(data: &[u8]) -> Option<usize> {
    find_subsequence(data, b"\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::executor::block_on;
    use std::collections::VecDeque;
    use std::task::{Context, Poll};

    /// A reader that hands out exactly the pieces it was scripted with, one per
    /// `poll_read`, so tests can put boundaries wherever the codec is likely to
    /// get them wrong. After the script runs out it reports EOF, or an error if
    /// `fail_at_end` is set.
    struct ScriptedReader {
        pieces: VecDeque<Vec<u8>>,
        fail_at_end: bool,
    }

    impl ScriptedReader {
        fn new<I, B>(pieces: I) -> Self
        where
            I: IntoIterator<Item = B>,
            B: AsRef<[u8]>,
        {
            Self {
                pieces: pieces.into_iter().map(|p| p.as_ref().to_vec()).collect(),
                fail_at_end: false,
            }
        }

        fn failing<I, B>(pieces: I) -> Self
        where
            I: IntoIterator<Item = B>,
            B: AsRef<[u8]>,
        {
            let mut r = Self::new(pieces);
            r.fail_at_end = true;
            r
        }
    }

    impl futures::io::AsyncRead for ScriptedReader {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut [u8],
        ) -> Poll<std::io::Result<usize>> {
            if buf.is_empty() {
                return Poll::Ready(Ok(0));
            }
            if self.pieces.is_empty() {
                return if self.fail_at_end {
                    Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::ConnectionReset,
                        "peer reset",
                    )))
                } else {
                    Poll::Ready(Ok(0))
                };
            }
            let piece = self.pieces.front_mut().expect("non-empty");
            let n = std::cmp::min(buf.len(), piece.len());
            buf[..n].copy_from_slice(&piece[..n]);
            piece.drain(..n);
            if piece.is_empty() {
                self.pieces.pop_front();
            }
            Poll::Ready(Ok(n))
        }
    }

    fn reader<I, B>(pieces: I) -> BoxedReader
    where
        I: IntoIterator<Item = B>,
        B: AsRef<[u8]>,
    {
        Box::new(ScriptedReader::new(pieces))
    }

    fn body_of(mut r: BodyReader) -> Result<Vec<u8>, JsTorError> {
        block_on(r.read_all())
    }

    // =====================================================================
    // Content-Length framing
    // =====================================================================

    #[test]
    fn a_content_length_body_arrives_whole() {
        let r = BodyReader::new(reader(["hello world"]), BodyFraming::ContentLength(11), vec![]);
        assert_eq!(body_of(r).unwrap(), b"hello world");
    }

    #[test]
    fn a_content_length_body_already_in_the_overflow_buffer_needs_no_read() {
        // The head and the whole body arrived in one packet.
        let r = BodyReader::new(
            reader(Vec::<Vec<u8>>::new()),
            BodyFraming::ContentLength(5),
            b"hello".to_vec(),
        );
        assert_eq!(body_of(r).unwrap(), b"hello");
    }

    #[test]
    fn a_content_length_body_split_across_reads_is_reassembled() {
        let r = BodyReader::new(
            reader(["he", "llo wo", "rld"]),
            BodyFraming::ContentLength(11),
            vec![],
        );
        assert_eq!(body_of(r).unwrap(), b"hello world");
    }

    #[test]
    fn overflow_plus_stream_bytes_combine() {
        let r = BodyReader::new(reader([" world"]), BodyFraming::ContentLength(11), b"hello".to_vec());
        assert_eq!(body_of(r).unwrap(), b"hello world");
    }

    #[test]
    fn bytes_past_the_content_length_are_ignored() {
        // Keep-alive is off, so trailing bytes are never a second response, but
        // they must not leak into this body either.
        let r = BodyReader::new(reader(["hello world!!!!"]), BodyFraming::ContentLength(11), vec![]);
        assert_eq!(body_of(r).unwrap(), b"hello world");

        let r = BodyReader::new(
            reader(Vec::<Vec<u8>>::new()),
            BodyFraming::ContentLength(5),
            b"hello, and more".to_vec(),
        );
        assert_eq!(body_of(r).unwrap(), b"hello");
    }

    #[test]
    fn a_zero_length_body_completes_immediately() {
        let mut r = BodyReader::new(reader(Vec::<Vec<u8>>::new()), BodyFraming::ContentLength(0), vec![]);
        assert_eq!(block_on(r.read_chunk()).unwrap(), None);
    }

    /// The bug this pins: EOF with bytes still owed used to be reported as a
    /// clean end, so a caller received a short body and no indication of it.
    #[test]
    fn a_short_content_length_body_is_an_error_not_a_clean_end() {
        let r = BodyReader::new(reader(["hel"]), BodyFraming::ContentLength(11), vec![]);
        let err = body_of(r).unwrap_err();
        assert!(err.message.contains("truncated"), "{}", err.message);
        assert!(err.message.contains("3 of 11"), "{}", err.message);
    }

    #[test]
    fn a_body_truncated_after_several_reads_is_still_an_error() {
        let r = BodyReader::new(
            reader(["hello", " wor"]),
            BodyFraming::ContentLength(11),
            vec![],
        );
        let err = body_of(r).unwrap_err();
        assert!(err.message.contains("9 of 11"), "{}", err.message);
    }

    #[test]
    fn a_body_that_never_starts_is_an_error() {
        let r = BodyReader::new(reader(Vec::<Vec<u8>>::new()), BodyFraming::ContentLength(11), vec![]);
        let err = body_of(r).unwrap_err();
        assert!(err.message.contains("0 of 11"), "{}", err.message);
    }

    #[test]
    fn a_read_error_during_a_content_length_body_is_reported() {
        let mut r = BodyReader::new(
            Box::new(ScriptedReader::failing(["hel"])),
            BodyFraming::ContentLength(11),
            vec![],
        );
        assert_eq!(block_on(r.read_chunk()).unwrap(), Some(b"hel".to_vec()));
        let err = block_on(r.read_chunk()).unwrap_err();
        assert!(err.message.contains("Failed to read body"), "{}", err.message);
    }

    // =====================================================================
    // Chunked framing
    // =====================================================================

    fn chunked(r: BodyReader) -> Result<Vec<u8>, JsTorError> {
        body_of(r)
    }

    fn chunked_reader<I, B>(pieces: I) -> BodyReader
    where
        I: IntoIterator<Item = B>,
        B: AsRef<[u8]>,
    {
        BodyReader::new(reader(pieces), BodyFraming::Chunked, vec![])
    }

    #[test]
    fn a_chunked_body_is_decoded() {
        let r = chunked_reader(["5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n"]);
        assert_eq!(chunked(r).unwrap(), b"hello world");
    }

    #[test]
    fn an_empty_chunked_body_is_just_the_terminator() {
        let r = chunked_reader(["0\r\n\r\n"]);
        assert_eq!(chunked(r).unwrap(), b"");
    }

    #[test]
    fn a_size_line_split_across_reads_is_reassembled() {
        // The boundary falls inside the size digits, and again inside its CRLF.
        // `0b` also covers a size written with a leading zero.
        let r = chunked_reader(["0", "b\r", "\nhello world\r\n0\r\n\r\n"]);
        assert_eq!(chunked(r).unwrap(), b"hello world");
    }

    #[test]
    fn chunk_data_split_across_reads_is_reassembled() {
        let r = chunked_reader(["b\r\nhel", "lo wo", "rld", "\r\n0\r\n\r\n"]);
        assert_eq!(chunked(r).unwrap(), b"hello world");
    }

    #[test]
    fn a_chunk_boundary_split_mid_crlf_is_handled() {
        // The CRLF that ends the chunk data is itself split.
        let r = chunked_reader(["5\r\nhello\r", "\n0\r\n\r\n"]);
        assert_eq!(chunked(r).unwrap(), b"hello");
    }

    #[test]
    fn overflow_bytes_holding_the_start_of_the_body_are_used() {
        // The header read went past \r\n\r\n into the first chunk.
        let r = BodyReader::new(
            reader(["o world\r\n0\r\n\r\n"]),
            BodyFraming::Chunked,
            b"b\r\nhell".to_vec(),
        );
        assert_eq!(chunked(r).unwrap(), b"hello world");
    }

    #[test]
    fn overflow_containing_the_whole_body_needs_no_read() {
        let r = BodyReader::new(
            reader(Vec::<Vec<u8>>::new()),
            BodyFraming::Chunked,
            b"5\r\nhello\r\n0\r\n\r\n".to_vec(),
        );
        assert_eq!(chunked(r).unwrap(), b"hello");
    }

    #[test]
    fn chunk_extensions_are_ignored() {
        let r = chunked_reader(["5;name=value\r\nhello\r\n0;last\r\n\r\n"]);
        assert_eq!(chunked(r).unwrap(), b"hello");
    }

    #[test]
    fn chunk_sizes_are_read_as_hex_in_either_case() {
        let payload = "x".repeat(0x1f);
        let r = chunked_reader([format!("1F\r\n{payload}\r\n0\r\n\r\n")]);
        assert_eq!(chunked(r).unwrap().len(), 0x1f);

        let r = chunked_reader([format!("1f\r\n{payload}\r\n0\r\n\r\n")]);
        assert_eq!(chunked(r).unwrap().len(), 0x1f);
    }

    #[test]
    fn a_chunk_larger_than_the_read_buffer_is_assembled() {
        // 8192 is the internal read size, so this spans several reads.
        let payload = "y".repeat(20_000);
        let r = chunked_reader([format!("{:x}\r\n{payload}\r\n0\r\n\r\n", payload.len())]);
        assert_eq!(chunked(r).unwrap().len(), 20_000);
    }

    #[test]
    fn many_small_chunks_are_all_delivered() {
        let mut wire = String::new();
        for i in 0..100 {
            wire.push_str(&format!("1\r\n{}\r\n", (b'a' + (i % 26) as u8) as char));
        }
        wire.push_str("0\r\n\r\n");
        let r = chunked_reader([wire]);
        assert_eq!(chunked(r).unwrap().len(), 100);
    }

    #[test]
    fn a_blank_line_before_a_size_line_is_skipped() {
        // Some servers emit a stray CRLF between chunks.
        let r = chunked_reader(["5\r\nhello\r\n\r\n0\r\n\r\n"]);
        assert_eq!(chunked(r).unwrap(), b"hello");
    }

    #[test]
    fn a_non_hex_chunk_size_is_an_error() {
        let r = chunked_reader(["zz\r\nhello\r\n0\r\n\r\n"]);
        let err = chunked(r).unwrap_err();
        assert!(err.message.contains("Invalid chunk size"), "{}", err.message);
    }

    #[test]
    fn a_chunk_size_line_that_is_not_utf8_is_an_error() {
        let r = chunked_reader([b"\xff\xfe\r\nhello\r\n0\r\n\r\n".to_vec()]);
        let err = chunked(r).unwrap_err();
        assert!(err.message.contains("UTF-8"), "{}", err.message);
    }

    #[test]
    fn eof_partway_through_a_chunk_is_an_error() {
        let r = chunked_reader(["b\r\nhello"]);
        let err = chunked(r).unwrap_err();
        assert!(
            err.message.contains("bytes remaining in chunk"),
            "{}",
            err.message
        );
    }

    /// Used to return a clean end: the chunk's bytes had all been handed over,
    /// so the truncation was invisible even though the terminator never came.
    #[test]
    fn eof_while_awaiting_the_crlf_after_a_chunk_is_an_error() {
        let r = chunked_reader(["5\r\nhello"]);
        let err = chunked(r).unwrap_err();
        assert!(err.message.contains("CRLF following a chunk"), "{}", err.message);

        // Also when only one of the two bytes arrived.
        let r = chunked_reader(["5\r\nhello\r"]);
        assert!(chunked(r).is_err());
    }

    /// Likewise: a body cut off between chunks looked complete.
    #[test]
    fn eof_before_the_terminating_chunk_is_an_error() {
        let r = chunked_reader(["5\r\nhello\r\n"]);
        let err = chunked(r).unwrap_err();
        assert!(
            err.message.contains("before the terminating chunk"),
            "{}",
            err.message
        );
    }

    #[test]
    fn a_chunk_not_followed_by_crlf_is_an_error() {
        // `XX` where the trailing CRLF belongs: the framing is broken, and
        // blindly dropping two bytes would silently corrupt the next size line.
        let r = chunked_reader(["5\r\nhelloXX0\r\n\r\n"]);
        let err = chunked(r).unwrap_err();
        assert!(err.message.contains("expected CRLF"), "{}", err.message);
    }

    #[test]
    fn trailers_after_the_terminating_chunk_do_not_break_the_body() {
        let r = chunked_reader(["5\r\nhello\r\n0\r\nX-Trailer: v\r\n\r\n"]);
        assert_eq!(chunked(r).unwrap(), b"hello");
    }

    #[test]
    fn reading_past_the_end_keeps_returning_none() {
        let mut r = chunked_reader(["5\r\nhello\r\n0\r\n\r\n"]);
        assert_eq!(block_on(r.read_chunk()).unwrap(), Some(b"hello".to_vec()));
        assert_eq!(block_on(r.read_chunk()).unwrap(), None);
        assert_eq!(block_on(r.read_chunk()).unwrap(), None);
    }

    // =====================================================================
    // EOF framing
    // =====================================================================

    #[test]
    fn an_until_eof_body_reads_to_the_close() {
        let r = BodyReader::new(reader(["hello", " ", "world"]), BodyFraming::UntilEof, vec![]);
        assert_eq!(body_of(r).unwrap(), b"hello world");
    }

    #[test]
    fn an_until_eof_body_includes_the_overflow() {
        let r = BodyReader::new(reader([" world"]), BodyFraming::UntilEof, b"hello".to_vec());
        assert_eq!(body_of(r).unwrap(), b"hello world");
    }

    /// With no length to check against, a reset after data is indistinguishable
    /// from a close, so the bytes received are returned.
    #[test]
    fn an_until_eof_body_treats_a_late_reset_as_the_end() {
        let r = BodyReader::new(
            Box::new(ScriptedReader::failing(["hello"])),
            BodyFraming::UntilEof,
            vec![],
        );
        assert_eq!(body_of(r).unwrap(), b"hello");
    }

    #[test]
    fn an_until_eof_body_that_fails_before_any_data_is_an_error() {
        let r = BodyReader::new(
            Box::new(ScriptedReader::failing(Vec::<Vec<u8>>::new())),
            BodyFraming::UntilEof,
            vec![],
        );
        assert!(body_of(r).is_err());
    }

    #[test]
    fn no_body_framing_yields_nothing() {
        let mut r = BodyReader::new(reader(["ignored"]), BodyFraming::None, b"ignored".to_vec());
        assert_eq!(block_on(r.read_chunk()).unwrap(), None);
    }

    // =====================================================================
    // build_request_head
    // =====================================================================

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    fn head(u: &str, method: Method, headers: &[(&str, &str)], body: RequestBody) -> String {
        let map: HashMap<String, String> =
            headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let bytes = build_request_head(&url(u), &method, &map, &body).unwrap();
        String::from_utf8(bytes).unwrap()
    }

    fn lines_of(head: &str) -> Vec<String> {
        head.split("\r\n").map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_minimal_get_has_a_request_line_host_and_defaults() {
        let h = head("https://example.com/", Method::GET, &[], RequestBody::None);
        let lines = lines_of(&h);
        assert_eq!(lines[0], "GET / HTTP/1.1");
        assert_eq!(lines[1], "Host: example.com");
        assert!(h.contains("Accept: */*\r\n"));
        assert!(h.contains("Connection: close\r\n"));
        assert!(h.ends_with("\r\n\r\n"), "head must end with a blank line");
        // No navigator off-browser, so no User-Agent is invented.
        assert!(!h.to_ascii_lowercase().contains("user-agent"));
        // No body, no framing headers.
        assert!(!h.to_ascii_lowercase().contains("content-length"));
        assert!(!h.to_ascii_lowercase().contains("transfer-encoding"));
    }

    #[test]
    fn the_query_string_is_carried_into_the_request_line() {
        let h = head("https://example.com/a/b?x=1&y=2", Method::GET, &[], RequestBody::None);
        assert_eq!(lines_of(&h)[0], "GET /a/b?x=1&y=2 HTTP/1.1");
    }

    #[test]
    fn a_port_and_userinfo_do_not_reach_the_host_header() {
        // Host carries the hostname only; the port is implied by the connection.
        let h = head("https://example.com:8443/x", Method::GET, &[], RequestBody::None);
        assert_eq!(lines_of(&h)[1], "Host: example.com");
    }

    #[test]
    fn caller_headers_are_passed_through() {
        let h = head(
            "https://example.com/",
            Method::POST,
            &[("X-Custom", "value"), ("Authorization", "Bearer t")],
            RequestBody::None,
        );
        assert!(h.contains("X-Custom: value\r\n"));
        assert!(h.contains("Authorization: Bearer t\r\n"));
        assert_eq!(lines_of(&h)[0], "POST / HTTP/1.1");
    }

    #[test]
    fn caller_defaults_are_not_duplicated() {
        let h = head(
            "https://example.com/",
            Method::GET,
            &[("accept", "application/json"), ("CONNECTION", "keep-alive")],
            RequestBody::None,
        );
        assert!(h.contains("accept: application/json\r\n"));
        assert!(!h.contains("Accept: */*\r\n"), "default Accept should not be added too");
        assert!(h.contains("CONNECTION: keep-alive\r\n"));
        assert!(!h.contains("Connection: close\r\n"));
    }

    #[test]
    fn a_caller_supplied_user_agent_is_kept() {
        let h = head(
            "https://example.com/",
            Method::GET,
            &[("User-Agent", "my-app/1.0")],
            RequestBody::None,
        );
        assert!(h.contains("User-Agent: my-app/1.0\r\n"));
        assert_eq!(h.to_ascii_lowercase().matches("user-agent").count(), 1);
    }

    /// The gateway does not virtual-host and the connection already fixes the
    /// origin, so a caller-supplied Host is dropped rather than sent alongside.
    #[test]
    fn a_caller_supplied_host_never_reaches_the_wire() {
        let h = head(
            "https://example.com/",
            Method::GET,
            &[("Host", "evil.example")],
            RequestBody::None,
        );
        assert_eq!(h.matches("Host: ").count(), 1);
        assert!(h.contains("Host: example.com\r\n"));
        assert!(!h.contains("evil.example"));
    }

    /// Framing is derived from the body, so a caller cannot contradict it.
    #[test]
    fn caller_supplied_framing_headers_are_stripped() {
        let h = head(
            "https://example.com/",
            Method::POST,
            &[("Content-Length", "999"), ("Transfer-Encoding", "chunked")],
            RequestBody::Bytes(b"hello".to_vec()),
        );
        assert!(h.contains("Content-Length: 5\r\n"));
        assert!(!h.contains("999"));
        assert_eq!(h.to_ascii_lowercase().matches("content-length").count(), 1);
        assert!(!h.to_ascii_lowercase().contains("transfer-encoding"));
    }

    #[test]
    fn a_known_length_body_is_framed_with_content_length() {
        let h = head(
            "https://example.com/",
            Method::PUT,
            &[],
            RequestBody::Bytes(vec![0u8; 1234]),
        );
        assert!(h.contains("Content-Length: 1234\r\n"));
        assert!(!h.to_ascii_lowercase().contains("transfer-encoding"));
    }

    #[test]
    fn an_empty_bytes_body_still_declares_a_zero_length() {
        let h = head("https://example.com/", Method::POST, &[], RequestBody::Bytes(vec![]));
        assert!(h.contains("Content-Length: 0\r\n"));
    }

    #[test]
    fn a_stream_body_is_framed_chunked() {
        let body = RequestBody::Stream(Box::pin(futures::stream::empty()));
        let h = head("https://example.com/", Method::POST, &[], body);
        assert!(h.contains("Transfer-Encoding: chunked\r\n"));
        assert!(
            !h.to_ascii_lowercase().contains("content-length"),
            "a stream has no known length"
        );
    }

    #[test]
    fn header_injection_through_a_value_is_refused() {
        let map: HashMap<String, String> = [(
            "X-Evil".to_string(),
            "v\r\nX-Injected: yes".to_string(),
        )]
        .into();
        let err = build_request_head(
            &url("https://example.com/"),
            &Method::GET,
            &map,
            &RequestBody::None,
        )
        .unwrap_err();
        assert_eq!(err.code, "INVALID_HEADER");
    }

    #[test]
    fn header_injection_through_a_name_is_refused() {
        for name in ["X\r\nEvil", "X\nEvil", "X\rEvil"] {
            let map: HashMap<String, String> = [(name.to_string(), "v".to_string())].into();
            let err = build_request_head(
                &url("https://example.com/"),
                &Method::GET,
                &map,
                &RequestBody::None,
            )
            .unwrap_err();
            assert_eq!(err.code, "INVALID_HEADER", "{name:?}");
        }
    }

    /// A lone CR or LF is enough to split a message for some parsers, so the
    /// check must not look only for the pair.
    #[test]
    fn a_bare_newline_in_a_value_is_refused() {
        for value in ["a\nb", "a\rb", "a\r\n b"] {
            let map: HashMap<String, String> = [("X".to_string(), value.to_string())].into();
            assert!(
                build_request_head(
                    &url("https://example.com/"),
                    &Method::GET,
                    &map,
                    &RequestBody::None
                )
                .is_err(),
                "{value:?}"
            );
        }
    }

    /// Rejection has to happen before the header is skipped, or injection via a
    /// dropped header name would slip through.
    #[test]
    fn injection_through_a_skipped_header_is_still_refused() {
        for name in ["Host", "Content-Length", "Transfer-Encoding"] {
            let map: HashMap<String, String> =
                [(name.to_string(), "x\r\nX-Injected: yes".to_string())].into();
            let err = build_request_head(
                &url("https://example.com/"),
                &Method::GET,
                &map,
                &RequestBody::None,
            )
            .unwrap_err();
            assert_eq!(err.code, "INVALID_HEADER", "{name}");
        }
    }

    // =====================================================================
    // write_request
    // =====================================================================

    fn write(body: RequestBody) -> Result<Vec<u8>, JsTorError> {
        let mut out = futures::io::Cursor::new(Vec::new());
        block_on(write_request(&mut out, b"HEAD\r\n\r\n", body))?;
        Ok(out.into_inner())
    }

    fn stream_body(chunks: Vec<&[u8]>) -> RequestBody {
        let items: Vec<Result<Vec<u8>, JsTorError>> =
            chunks.into_iter().map(|c| Ok(c.to_vec())).collect();
        RequestBody::Stream(Box::pin(futures::stream::iter(items)))
    }

    #[test]
    fn a_bodyless_request_writes_only_the_head() {
        assert_eq!(write(RequestBody::None).unwrap(), b"HEAD\r\n\r\n");
    }

    #[test]
    fn a_bytes_body_is_written_raw_after_the_head() {
        assert_eq!(write(RequestBody::Bytes(b"payload".to_vec())).unwrap(), b"HEAD\r\n\r\npayload");
    }

    #[test]
    fn a_stream_body_is_written_as_chunks_with_a_terminator() {
        let out = write(stream_body(vec![b"hello", b" world"])).unwrap();
        assert_eq!(out, b"HEAD\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n");
    }

    #[test]
    fn chunk_sizes_are_written_in_hex() {
        let out = write(stream_body(vec![&[b'x'; 255]])).unwrap();
        let text = String::from_utf8_lossy(&out).to_string();
        assert!(text.contains("\r\n\r\nff\r\n"), "size should be hex: {}", &text[..24]);
    }

    /// A producer yielding an empty buffer must not terminate the body — `0\r\n`
    /// is the end-of-body marker.
    #[test]
    fn empty_stream_chunks_are_skipped_not_written() {
        let out = write(stream_body(vec![b"", b"a", b"", b"b", b""])).unwrap();
        assert_eq!(out, b"HEAD\r\n\r\n1\r\na\r\n1\r\nb\r\n0\r\n\r\n");
    }

    #[test]
    fn an_empty_stream_writes_just_the_terminator() {
        let out = write(stream_body(vec![])).unwrap();
        assert_eq!(out, b"HEAD\r\n\r\n0\r\n\r\n");
    }

    #[test]
    fn a_stream_error_aborts_the_request() {
        let items: Vec<Result<Vec<u8>, JsTorError>> = vec![
            Ok(b"partial".to_vec()),
            Err(JsTorError::http_request("producer failed")),
        ];
        let body = RequestBody::Stream(Box::pin(futures::stream::iter(items)));
        let err = write(body).unwrap_err();
        assert!(err.message.contains("producer failed"), "{}", err.message);
    }

    /// The framing the head declares has to match what the body writer emits.
    #[test]
    fn the_declared_framing_matches_the_bytes_written() {
        let h = head("https://example.com/", Method::POST, &[], RequestBody::Bytes(b"hello".to_vec()));
        assert!(h.contains("Content-Length: 5\r\n"));
        let written = write(RequestBody::Bytes(b"hello".to_vec())).unwrap();
        assert_eq!(&written[b"HEAD\r\n\r\n".len()..], b"hello");

        let h = head(
            "https://example.com/",
            Method::POST,
            &[],
            RequestBody::Stream(Box::pin(futures::stream::empty())),
        );
        assert!(h.contains("Transfer-Encoding: chunked\r\n"));
        let written = write(stream_body(vec![b"hello"])).unwrap();
        assert!(written.ends_with(b"0\r\n\r\n"));
    }

    /// A chunked request body written by `write_request` must decode back through
    /// the same codec that reads chunked responses.
    #[test]
    fn a_written_chunked_body_decodes_back() {
        let written = write(stream_body(vec![b"hello", b" ", b"world"])).unwrap();
        let wire = written[b"HEAD\r\n\r\n".len()..].to_vec();
        let r = BodyReader::new(reader([wire]), BodyFraming::Chunked, vec![]);
        assert_eq!(chunked(r).unwrap(), b"hello world");
    }

    // =====================================================================
    // read_response_headers
    // =====================================================================

    fn read_head<I, B>(pieces: I, method: Method) -> Result<(u16, Vec<(String, String)>, BodyFraming, Vec<u8>), JsTorError>
    where
        I: IntoIterator<Item = B>,
        B: AsRef<[u8]>,
    {
        let mut r = ScriptedReader::new(pieces);
        block_on(read_response_headers(&mut r, &method))
    }

    #[test]
    fn a_simple_response_head_is_parsed() {
        let (status, headers, framing, overflow) = read_head(
            ["HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5\r\n\r\nhello"],
            Method::GET,
        )
        .unwrap();
        assert_eq!(status, 200);
        assert_eq!(framing, BodyFraming::ContentLength(5));
        assert_eq!(overflow, b"hello");
        // Names are lowercased for lookup; values keep their case.
        assert!(headers.contains(&("content-type".into(), "text/plain".into())));
    }

    #[test]
    fn a_head_split_across_reads_is_reassembled() {
        let (status, _, framing, overflow) = read_head(
            ["HTTP/1.1 20", "0 OK\r\nContent-Len", "gth: 2\r\n", "\r\nhi"],
            Method::GET,
        )
        .unwrap();
        assert_eq!(status, 200);
        assert_eq!(framing, BodyFraming::ContentLength(2));
        assert_eq!(overflow, b"hi");
    }

    #[test]
    fn a_separator_split_across_reads_is_found() {
        let (status, _, _, overflow) =
            read_head(["HTTP/1.1 204 No Content\r\n\r", "\nnext"], Method::GET).unwrap();
        assert_eq!(status, 204);
        assert_eq!(overflow, b"next");
    }

    #[test]
    fn a_status_line_without_a_reason_phrase_is_accepted() {
        let (status, _, _, _) =
            read_head(["HTTP/1.1 200\r\nContent-Length: 0\r\n\r\n"], Method::GET).unwrap();
        assert_eq!(status, 200);
    }

    /// The interim response is discarded and the final one used. Both arrive in
    /// one read here, which used to leave the parser waiting for bytes the
    /// server had already finished sending.
    #[test]
    fn an_interim_1xx_response_is_skipped() {
        let (status, _, framing, overflow) = read_head(
            ["HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\nok"],
            Method::POST,
        )
        .unwrap();
        assert_eq!(status, 201);
        assert_eq!(framing, BodyFraming::ContentLength(2));
        assert_eq!(overflow, b"ok");
    }

    #[test]
    fn several_interim_responses_are_skipped() {
        let (status, _, _, _) = read_head(
            [
                "HTTP/1.1 100 Continue\r\n\r\n",
                "HTTP/1.1 103 Early Hints\r\nLink: </s.css>\r\n\r\n",
                "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n",
            ],
            Method::POST,
        )
        .unwrap();
        assert_eq!(status, 200);
    }

    #[test]
    fn interim_headers_do_not_leak_into_the_final_response() {
        let (_, headers, _, _) = read_head(
            [
                "HTTP/1.1 103 Early Hints\r\nX-Interim: yes\r\n\r\nHTTP/1.1 200 OK\r\nX-Final: yes\r\nContent-Length: 0\r\n\r\n",
            ],
            Method::GET,
        )
        .unwrap();
        assert!(headers.iter().any(|(k, _)| k == "x-final"));
        assert!(!headers.iter().any(|(k, _)| k == "x-interim"));
    }

    #[test]
    fn an_upgrade_is_refused() {
        let err = read_head(
            ["HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\r\n"],
            Method::GET,
        )
        .unwrap_err();
        assert!(err.message.contains("101"), "{}", err.message);
    }

    #[test]
    fn a_head_response_never_has_a_body() {
        let (_, _, framing, _) = read_head(
            ["HTTP/1.1 200 OK\r\nContent-Length: 1234\r\n\r\n"],
            Method::HEAD,
        )
        .unwrap();
        assert_eq!(framing, BodyFraming::None, "Content-Length describes the GET body");
    }

    #[test]
    fn bodyless_statuses_have_no_body() {
        for status in [204u16, 304] {
            let (_, _, framing, _) = read_head(
                [format!("HTTP/1.1 {status} X\r\nContent-Length: 99\r\n\r\n")],
                Method::GET,
            )
            .unwrap();
            assert_eq!(framing, BodyFraming::None, "status {status}");
        }
    }

    /// RFC 9112 §6.1: when both are present, chunked wins and Content-Length is
    /// ignored — the alternative is a request-smuggling primitive.
    #[test]
    fn chunked_takes_precedence_over_content_length() {
        let (_, _, framing, _) = read_head(
            ["HTTP/1.1 200 OK\r\nContent-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\n"],
            Method::GET,
        )
        .unwrap();
        assert_eq!(framing, BodyFraming::Chunked);
    }

    #[test]
    fn transfer_encoding_is_matched_case_insensitively_and_in_a_list() {
        for value in ["chunked", "Chunked", "CHUNKED", "gzip, chunked"] {
            let (_, _, framing, _) = read_head(
                [format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: {value}\r\n\r\n")],
                Method::GET,
            )
            .unwrap();
            assert_eq!(framing, BodyFraming::Chunked, "{value}");
        }
    }

    #[test]
    fn a_response_without_framing_reads_until_eof() {
        let (_, _, framing, _) =
            read_head(["HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\nbody"], Method::GET)
                .unwrap();
        assert_eq!(framing, BodyFraming::UntilEof);
    }

    #[test]
    fn a_malformed_content_length_is_an_error() {
        for value in ["abc", "-1", "5 5", ""] {
            let err = read_head(
                [format!("HTTP/1.1 200 OK\r\nContent-Length: {value}\r\n\r\n")],
                Method::GET,
            )
            .unwrap_err();
            assert!(err.message.contains("content-length"), "{value:?}: {}", err.message);
        }
    }

    #[test]
    fn a_malformed_status_line_is_an_error() {
        for line in ["HTTP/1.1", "garbage", "HTTP/1.1 abc OK", ""] {
            assert!(
                read_head([format!("{line}\r\nX: y\r\n\r\n")], Method::GET).is_err(),
                "{line:?} should not parse"
            );
        }
    }

    #[test]
    fn a_closed_connection_before_the_head_is_an_error() {
        let err = read_head(["HTTP/1.1 200 OK\r\nContent-"], Method::GET).unwrap_err();
        assert!(err.message.contains("closed before headers"), "{}", err.message);

        let err = read_head(Vec::<Vec<u8>>::new(), Method::GET).unwrap_err();
        assert!(err.message.contains("closed before headers"), "{}", err.message);
    }

    #[test]
    fn a_read_error_before_the_head_is_reported() {
        let mut r = ScriptedReader::failing(["HTTP/1.1 200 OK\r\n"]);
        let err = block_on(read_response_headers(&mut r, &Method::GET)).unwrap_err();
        assert!(err.message.contains("Failed to read response headers"), "{}", err.message);
    }

    #[test]
    fn an_oversized_head_is_refused() {
        // Well past the 64 KB cap, in pieces so the cumulative check is what
        // catches it.
        let filler = format!("X-Pad: {}\r\n", "v".repeat(1000));
        let mut pieces = vec!["HTTP/1.1 200 OK\r\n".to_string()];
        for _ in 0..80 {
            pieces.push(filler.clone());
        }
        let err = read_head(pieces, Method::GET).unwrap_err();
        assert!(err.message.contains("exceed"), "{}", err.message);
    }

    #[test]
    fn a_head_just_under_the_cap_is_accepted() {
        let filler = format!("X-Pad: {}\r\n", "v".repeat(1000));
        let mut head = "HTTP/1.1 200 OK\r\n".to_string();
        for _ in 0..50 {
            head.push_str(&filler);
        }
        head.push_str("Content-Length: 0\r\n\r\n");
        assert!(head.len() < MAX_HEADER_SIZE, "fixture should be under the cap");
        let (status, _, _, _) = read_head([head], Method::GET).unwrap();
        assert_eq!(status, 200);
    }

    #[test]
    fn header_lines_without_a_colon_are_dropped() {
        let (status, headers, _, _) = read_head(
            ["HTTP/1.1 200 OK\r\ngarbage line\r\nX-Good: 1\r\nContent-Length: 0\r\n\r\n"],
            Method::GET,
        )
        .unwrap();
        assert_eq!(status, 200);
        assert!(headers.iter().any(|(k, _)| k == "x-good"));
        assert!(headers.iter().all(|(k, _)| k != "garbage line"));
    }

    #[test]
    fn a_repeated_header_keeps_both_values() {
        let (_, headers, _, _) = read_head(
            ["HTTP/1.1 200 OK\r\nSet-Cookie: a=1\r\nSet-Cookie: b=2\r\nContent-Length: 0\r\n\r\n"],
            Method::GET,
        )
        .unwrap();
        let cookies: Vec<_> = headers.iter().filter(|(k, _)| k == "set-cookie").collect();
        assert_eq!(cookies.len(), 2);
    }

    // =====================================================================
    // head + body together
    // =====================================================================

    /// The overflow handed to the BodyReader must line up exactly with where the
    /// head parser stopped, whatever the read boundaries were.
    #[test]
    fn a_whole_exchange_decodes_across_arbitrary_read_boundaries() {
        let wire = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
                    5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        for split in 1..wire.len() {
            let pieces = vec![wire[..split].to_string(), wire[split..].to_string()];
            let mut r = ScriptedReader::new(pieces);
            let (status, _, framing, overflow) =
                block_on(read_response_headers(&mut r, &Method::GET)).unwrap();
            assert_eq!(status, 200, "split at {split}");
            assert_eq!(framing, BodyFraming::Chunked, "split at {split}");
            let body = BodyReader::new(Box::new(r), framing, overflow);
            assert_eq!(body_of(body).unwrap(), b"hello world", "split at {split}");
        }
    }

    #[test]
    fn a_content_length_exchange_decodes_across_arbitrary_read_boundaries() {
        let wire = "HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\nhello world";
        for split in 1..wire.len() {
            let pieces = vec![wire[..split].to_string(), wire[split..].to_string()];
            let mut r = ScriptedReader::new(pieces);
            let (_, _, framing, overflow) =
                block_on(read_response_headers(&mut r, &Method::GET)).unwrap();
            let body = BodyReader::new(Box::new(r), framing, overflow);
            assert_eq!(body_of(body).unwrap(), b"hello world", "split at {split}");
        }
    }
}
