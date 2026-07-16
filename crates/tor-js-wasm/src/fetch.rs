//! HTTP fetch implementation over arti-client DataStream
//!
//! This module implements HTTP/1.1 requests over Tor streams,
//! with TLS support via rustls for HTTPS.
//!
//! The fetch resolves as soon as response headers are received.
//! Body reading is deferred — chunks are read incrementally via `read_chunk()`.

use crate::error::JsTorError;
use futures::io::{AsyncReadExt, AsyncWriteExt};
use http::Method;
use std::collections::HashMap;
use tracing::{debug, info};
use url::Url;

/// Get navigator.userAgent if available (returns None in Node.js/Deno).
fn get_navigator_user_agent() -> Option<String> {
    let navigator = js_sys::Reflect::get(&js_sys::global(), &"navigator".into()).ok()?;
    if navigator.is_undefined() || navigator.is_null() {
        return None;
    }
    let ua = js_sys::Reflect::get(&navigator, &"userAgent".into()).ok()?;
    ua.as_string()
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
            Ok(0) => {
                self.done = true;
                Ok(None)
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
                        self.done = true;
                        return Ok(None);
                    }
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
                        self.done = true;
                        return Ok(None);
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
pub fn build_http_request(
    url: &Url,
    method: &Method,
    headers: &HashMap<String, String>,
    body: Option<&[u8]>,
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
        if lower == "content-length" {
            let expected = body.map(|b| b.len().to_string()).unwrap_or_default();
            if *value != expected {
                tracing::warn!("Ignoring caller's Content-Length '{}' (using '{}')", value, expected);
            }
            continue;
        }
        request.push_str(&format!("{}: {}\r\n", key, value));
    }

    // Add content-length for requests with body
    if let Some(body) = body {
        request.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }

    // End headers
    request.push_str("\r\n");

    let mut bytes = request.into_bytes();

    // Add body if present
    if let Some(body) = body {
        bytes.extend_from_slice(body);
    }

    Ok(bytes)
}

/// Write the HTTP request and read response headers.
///
/// Returns the parsed status/headers, the body framing mode, and any overflow
/// bytes read past the `\r\n\r\n` header separator. The stream is borrowed
/// mutably so the caller retains ownership for body reading.
async fn send_request_and_read_headers<S>(
    stream: &mut S,
    request_bytes: &[u8],
    method: &Method,
) -> Result<(u16, Vec<(String, String)>, BodyFraming, Vec<u8>), JsTorError>
where
    S: futures::io::AsyncRead + futures::io::AsyncWrite + Unpin,
{
    // Write the request
    stream
        .write_all(request_bytes)
        .await
        .map_err(|e| JsTorError::http_request(format!("Failed to write request: {}", e)))?;
    stream
        .flush()
        .await
        .map_err(|e| JsTorError::http_request(format!("Failed to flush request: {}", e)))?;

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
            match stream.read(&mut buf).await {
                Ok(0) => {
                    return Err(JsTorError::http_request(
                        "Connection closed before headers received",
                    ));
                }
                Ok(n) => {
                    header_buf.extend_from_slice(&buf[..n]);

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
                }
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
    body: Option<Vec<u8>>,
    is_https: bool,
    host: &str,
    tls_config: Option<std::sync::Arc<futures_rustls::rustls::ClientConfig>>,
) -> Result<FetchHeadersResult, JsTorError>
where
    S: futures::io::AsyncRead + futures::io::AsyncWrite + Unpin + 'static,
{
    let request_bytes = build_http_request(url, &method, &headers, body.as_deref())?;
    debug!("Sending {} bytes of HTTP request", request_bytes.len());

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

        let (status, resp_headers, framing, overflow) =
            send_request_and_read_headers(&mut tls_stream, &request_bytes, &method).await?;

        info!("Received response headers: status={}", status);

        let reader: BoxedReader = Box::new(tls_stream);
        Ok(FetchHeadersResult {
            status,
            headers: resp_headers,
            body_reader: BodyReader::new(reader, framing, overflow),
        })
    } else {
        let mut stream = stream;

        let (status, resp_headers, framing, overflow) =
            send_request_and_read_headers(&mut stream, &request_bytes, &method).await?;

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
