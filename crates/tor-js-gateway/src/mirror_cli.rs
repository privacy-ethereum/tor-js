//! The `sync` subcommand: drive a gateway's `worker-bundles-sync` capability
//! (PROTOCOL.md §5.1) from a terminal.
//!
//! This is a *client*, not a second copy of the mirror. It dials a gateway over
//! KPS and runs one exchange against `/keccak/sync`, which means:
//!
//! - it works against a remote gateway as well as the local one, so a publisher
//!   can nudge the gateways that serve their objects;
//! - the running gateway stays the only process that writes the object
//!   directory, so there is no second writer to race with;
//! - it exercises exactly the path documented for everyone else, rather than a
//!   privileged side door.
//!
//! Native clients dial QUIC (browsers dial WebRTC against the same address).

use std::path::PathBuf;

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::config::{Config, KeccakSource};

/// Cap on a response we are going to print. The bodies here are small JSON
/// documents; anything larger means we are not talking to what we think.
const MAX_RESPONSE: usize = 256 * 1024;

/// `tor-js-gateway sync [ADDRESS] [--status]`.
pub async fn run(config_path: &PathBuf, address: Option<String>, status_only: bool) -> Result<()> {
    let address = match address {
        Some(address) => address,
        None => local_address(config_path)?,
    };

    let method = if status_only { "GET" } else { "POST" };
    // §3.2: Host is required, and the certhash is the name in KPS.
    let certhash = address.rsplit(':').next().unwrap_or_default();
    let request = format!("{method} /keccak/sync HTTP/1.1\r\nHost: {certhash}\r\n\r\n");

    if !status_only {
        eprintln!("asking {address} to sync its worker-bundle objects…");
    }
    let (status, body) = exchange(&address, &request).await?;

    // Pretty-print when it is the JSON we expect, fall back to raw otherwise —
    // an unexpected body is exactly when you want to see it verbatim.
    match serde_json::from_str::<serde_json::Value>(&body) {
        Ok(value) => println!("{}", serde_json::to_string_pretty(&value)?),
        Err(_) if body.is_empty() => {}
        Err(_) => println!("{body}"),
    }

    match status {
        200 => Ok(()),
        // Give the common refusals a line an operator can act on, and a
        // non-zero exit so a script can too.
        404 => anyhow::bail!(
            "HTTP 404 — that gateway does not serve worker bundles \
             (keccak_repo/keccak_branch unset)"
        ),
        409 => anyhow::bail!("HTTP 409 — a sync is already running there; try again shortly"),
        429 => anyhow::bail!(
            "HTTP 429 — a client-triggered sync ran there too recently; \
             the poll will pick the objects up regardless"
        ),
        503 => anyhow::bail!("HTTP 503 — syncing is disabled on that gateway (--no-mirror)"),
        502 => anyhow::bail!("HTTP 502 — the sync ran and failed; see the error above"),
        other => anyhow::bail!("HTTP {other}"),
    }
}

/// The local gateway's own address, for the common case of running this on the
/// host that serves. Loopback rather than the advertised IP: the listener binds
/// every interface, and this way it works behind NAT and before DNS-less
/// publication is set up.
fn local_address(config_path: &PathBuf) -> Result<String> {
    let cfg = Config::load(config_path)?;
    if cfg.keccak_source()? == KeccakSource::Disabled {
        anyhow::bail!(
            "this gateway is not configured to serve worker bundles: \
             set keccak_repo and keccak_branch in {}",
            config_path.display()
        );
    }
    // Read the key; never create one. A missing key means the gateway has not
    // been initialised, which is worth saying rather than papering over.
    let pem = std::fs::read_to_string(&cfg.kps_key_file).with_context(|| {
        format!(
            "reading the KPS identity at {}\n\nRun `tor-js-gateway init` first, \
             or pass the gateway's address explicitly.",
            cfg.kps_key_file.display()
        )
    })?;
    let identity = kps::Identity::from_pem(&pem)
        .with_context(|| format!("parsing {}", cfg.kps_key_file.display()))?;
    Ok(format!("127.0.0.1:{}:{}", cfg.kps_port, identity.certhash))
}

/// One KPS-HTTP/1 exchange (PROTOCOL.md §3): write the request, half-close,
/// read to EOF. Returns the status code and the body.
async fn exchange(address: &str, request: &str) -> Result<(u16, String)> {
    let conn = kps::dial(address)
        .await
        .with_context(|| format!("dialing {address}"))?;
    let mut stream = conn.open_stream().await.context("opening a KPS stream")?;

    stream
        .write_all(request.as_bytes())
        .await
        .context("writing the request")?;
    // §3.2: the request is terminated by our FIN, not by a Content-Length.
    stream.close_write().await.context("half-closing the stream")?;

    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = stream.read(&mut buf).await.context("reading the response")?;
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&buf[..n]);
        if raw.len() > MAX_RESPONSE {
            anyhow::bail!("response exceeds {MAX_RESPONSE} bytes");
        }
    }
    let _ = conn.close().await;

    parse_response(&raw)
}

fn parse_response(raw: &[u8]) -> Result<(u16, String)> {
    let sep = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .context("no header terminator in the response")?;
    let head = String::from_utf8_lossy(&raw[..sep]);
    let status_line = head.lines().next().unwrap_or_default();
    let status: u16 = status_line
        .split(' ')
        .nth(1)
        .and_then(|c| c.parse().ok())
        .with_context(|| format!("malformed status line: {status_line:?}"))?;
    Ok((status, String::from_utf8_lossy(&raw[sep + 4..]).into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_response_splits_into_status_and_body() {
        let raw = b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\r\n{\"added\":1}";
        assert_eq!(parse_response(raw).unwrap(), (200, "{\"added\":1}".to_string()));
    }

    #[test]
    fn a_bodyless_response_still_parses() {
        let raw = b"HTTP/1.1 404 Not Found\r\n\r\n";
        assert_eq!(parse_response(raw).unwrap(), (404, String::new()));
    }

    #[test]
    fn a_response_without_a_header_terminator_is_an_error() {
        assert!(parse_response(b"HTTP/1.1 200 OK\r\n").is_err());
        assert!(parse_response(b"").is_err());
    }

    #[test]
    fn a_malformed_status_line_is_an_error() {
        assert!(parse_response(b"garbage\r\n\r\n").is_err());
        assert!(parse_response(b"HTTP/1.1 nope OK\r\n\r\n").is_err());
    }

    #[test]
    fn the_local_address_is_refused_when_bundles_are_not_configured() {
        let dir = crate::testutil::TempDir::new("mirror-cli");
        let path = dir.join("config.json5");
        std::fs::write(&path, Config::to_json5_with_comments()).unwrap();
        let err = local_address(&path).unwrap_err().to_string();
        assert!(err.contains("keccak_repo"), "{err}");
    }
}
