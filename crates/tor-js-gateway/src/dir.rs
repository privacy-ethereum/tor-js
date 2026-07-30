//! Raw directory protocol requests over BEGINDIR streams.

use anyhow::{Context, Result, bail};
use futures::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tor_circmgr::ClientDirTunnel;

/// Fetch raw bytes from a directory cache via a BEGINDIR stream.
///
/// Opens a BEGINDIR stream on the given tunnel and sends a raw HTTP/1.0
/// GET request. The response body is decompressed automatically.
///
/// Returns `Ok(None)` on HTTP 304 Not Modified.
pub async fn get(
    tunnel: &ClientDirTunnel,
    path: &str,
    diff_from: Option<&str>,
) -> Result<Option<Vec<u8>>> {
    let mut stream = tunnel
        .begin_dir_stream()
        .await
        .map_err(|e| anyhow::anyhow!("opening BEGINDIR stream: {}", e))?;

    let diff_header = match diff_from {
        Some(hex) => format!("X-Or-Diff-From-Consensus: {}\r\n", hex),
        None => String::new(),
    };
    let request = format!(
        "GET {} HTTP/1.0\r\n\
         Accept-Encoding: deflate, identity, x-tor-lzma, x-zstd\r\n\
         {}\
         \r\n",
        path, diff_header
    );
    stream
        .write_all(request.as_bytes())
        .await
        .context("writing request")?;
    stream.flush().await.context("flushing request")?;

    // Parse HTTP/1.0 response
    let mut reader = BufReader::new(stream);
    let mut header_buf = String::new();
    loop {
        let mut line = String::new();
        let n = reader
            .read_line(&mut line)
            .await
            .context("reading header line")?;
        if n == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        header_buf.push_str(&line);
    }

    let (status, encoding) = parse_dir_response_head(&header_buf);

    if status == 304 {
        return Ok(None);
    }
    if !(200..300).contains(&status) {
        bail!("GET {} returned status {}", path, status);
    }

    let mut body = Vec::new();
    let _ = reader.read_to_end(&mut body).await;

    decompress(encoding.as_deref(), &body).await.map(Some)
}

/// Pull the status code and `Content-Encoding` out of a response head.
///
/// An unreadable status line yields `0`, which the caller treats as a failure —
/// a directory cache that answers with garbage must not be mistaken for a 200.
fn parse_dir_response_head(head: &str) -> (u16, Option<String>) {
    let status = head
        .lines()
        .next()
        .unwrap_or("")
        .split_whitespace()
        .nth(1)
        .unwrap_or("0")
        .parse()
        .unwrap_or(0);

    let encoding = head
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-encoding"))
        .map(|(_, v)| v.trim().to_string());

    (status, encoding)
}

async fn decompress(encoding: Option<&str>, data: &[u8]) -> Result<Vec<u8>> {
    use async_compression::futures::bufread::*;

    let mut out = Vec::new();
    match encoding {
        None | Some("identity") => {
            out = data.to_vec();
        }
        Some("deflate") => {
            let mut decoder = ZlibDecoder::new(data);
            decoder
                .read_to_end(&mut out)
                .await
                .context("deflate decode")?;
        }
        Some("x-tor-lzma") => {
            let mut decoder = XzDecoder::new(data);
            decoder.read_to_end(&mut out).await.context("xz decode")?;
        }
        Some("x-zstd") => {
            let mut decoder = ZstdDecoder::new(data);
            decoder
                .read_to_end(&mut out)
                .await
                .context("zstd decode")?;
        }
        Some(other) => bail!("unsupported encoding: {}", other),
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(lines: &[&str]) -> String {
        lines.iter().map(|l| format!("{l}\r\n")).collect()
    }

    #[test]
    fn status_and_encoding_are_read_from_the_head() {
        let (status, encoding) = parse_dir_response_head(&head(&[
            "HTTP/1.0 200 OK",
            "Date: Wed, 30 Jul 2026 00:00:00 GMT",
            "Content-Encoding: x-zstd",
            "Content-Type: text/plain",
        ]));
        assert_eq!(status, 200);
        assert_eq!(encoding.as_deref(), Some("x-zstd"));
    }

    #[test]
    fn a_304_is_recognised() {
        let (status, _) = parse_dir_response_head(&head(&["HTTP/1.0 304 Not Modified"]));
        assert_eq!(status, 304);
    }

    /// Header names are case-insensitive on the wire, and directory caches do
    /// vary the casing.
    #[test]
    fn the_encoding_header_name_is_matched_case_insensitively() {
        for name in ["Content-Encoding", "content-encoding", "CONTENT-ENCODING"] {
            let (_, encoding) =
                parse_dir_response_head(&head(&["HTTP/1.0 200 OK", &format!("{name}: deflate")]));
            assert_eq!(encoding.as_deref(), Some("deflate"), "{name}");
        }
    }

    #[test]
    fn a_missing_encoding_header_is_none() {
        let (status, encoding) =
            parse_dir_response_head(&head(&["HTTP/1.0 200 OK", "Content-Type: text/plain"]));
        assert_eq!(status, 200);
        assert_eq!(encoding, None);
    }

    /// The status line itself is skipped when looking for headers, so an HTTP
    /// version string containing a colon cannot be read as a field.
    #[test]
    fn the_status_line_is_not_treated_as_a_header() {
        let (_, encoding) = parse_dir_response_head(&head(&["content-encoding: 200 OK"]));
        assert_eq!(encoding, None);
    }

    #[test]
    fn garbage_status_lines_become_zero_not_success() {
        for line in ["", "not http at all", "HTTP/1.0", "HTTP/1.0 abc OK", "HTTP/1.0 -1 X"] {
            let (status, _) = parse_dir_response_head(&head(&[line]));
            assert_eq!(status, 0, "{line:?}");
            assert!(!(200..300).contains(&status), "{line:?} must not read as success");
            assert_ne!(status, 304, "{line:?} must not read as not-modified");
        }
    }

    #[test]
    fn an_empty_head_is_a_failure() {
        let (status, encoding) = parse_dir_response_head("");
        assert_eq!(status, 0);
        assert_eq!(encoding, None);
    }

    // ---- decompress ------------------------------------------------------

    async fn zlib(data: &[u8]) -> Vec<u8> {
        use async_compression::futures::write::ZlibEncoder;
        use futures::io::AsyncWriteExt as _;
        let mut encoder = ZlibEncoder::new(Vec::new());
        encoder.write_all(data).await.unwrap();
        encoder.close().await.unwrap();
        encoder.into_inner()
    }

    async fn xz(data: &[u8]) -> Vec<u8> {
        use async_compression::futures::write::XzEncoder;
        use futures::io::AsyncWriteExt as _;
        let mut encoder = XzEncoder::new(Vec::new());
        encoder.write_all(data).await.unwrap();
        encoder.close().await.unwrap();
        encoder.into_inner()
    }

    async fn zstd_enc(data: &[u8]) -> Vec<u8> {
        use async_compression::futures::write::ZstdEncoder;
        use futures::io::AsyncWriteExt as _;
        let mut encoder = ZstdEncoder::new(Vec::new());
        encoder.write_all(data).await.unwrap();
        encoder.close().await.unwrap();
        encoder.into_inner()
    }

    /// Every codec named in the `Accept-Encoding` we send must round-trip, or a
    /// directory cache picking one of them breaks sync.
    #[tokio::test]
    async fn every_advertised_codec_round_trips() {
        let payload = b"network-status-version 3 microdesc\n".repeat(200);

        assert_eq!(decompress(None, &payload).await.unwrap(), payload);
        assert_eq!(decompress(Some("identity"), &payload).await.unwrap(), payload);
        assert_eq!(
            decompress(Some("deflate"), &zlib(&payload).await).await.unwrap(),
            payload
        );
        assert_eq!(
            decompress(Some("x-tor-lzma"), &xz(&payload).await).await.unwrap(),
            payload
        );
        assert_eq!(
            decompress(Some("x-zstd"), &zstd_enc(&payload).await).await.unwrap(),
            payload
        );
    }

    #[tokio::test]
    async fn an_unadvertised_encoding_is_an_error_not_raw_bytes() {
        // Returning the compressed bytes as-is would corrupt the consensus.
        let err = decompress(Some("gzip"), b"\x1f\x8b").await.unwrap_err();
        assert!(err.to_string().contains("gzip"), "{err}");
        let err = decompress(Some("br"), b"x").await.unwrap_err();
        assert!(err.to_string().contains("br"), "{err}");
    }

    #[tokio::test]
    async fn corrupt_compressed_bodies_are_errors() {
        for encoding in ["deflate", "x-tor-lzma", "x-zstd"] {
            let err = decompress(Some(encoding), b"not really compressed").await;
            assert!(err.is_err(), "{encoding} accepted garbage");
        }
    }

    #[tokio::test]
    async fn an_empty_identity_body_is_empty_not_an_error() {
        assert_eq!(decompress(None, b"").await.unwrap(), Vec::<u8>::new());
    }
}
