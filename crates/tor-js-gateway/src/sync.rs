//! Consensus + microdescriptor sync logic with relay-style scheduling.

use std::io::Write;
use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use base64ct::Encoding as _;
use rand::Rng;
use tor_checkable::{ExternallySigned, TimeValidityError, Timebound};
use tor_llcrypto::pk::rsa::RsaIdentity;
use tor_netdoc::doc::netstatus::{Lifetime, MdConsensus};

use arti_client::TorClient;
use tor_circmgr::DirInfo;
use tor_netdir::Timeliness;

use crate::store::{AuthCertStore, Stores};
use crate::tunnel::RelayAllowlist;

/// Fetch consensus, parse it, fetch missing microdescs, write everything to disk.
/// Returns the consensus lifetime for scheduling the next sync.
///
/// If the consensus store holds a previous consensus, a diff is requested via
/// `X-Or-Diff-From-Consensus`. The store is updated with the new consensus on success.
pub async fn sync_once(
    client: &TorClient<tor_rtcompat::PreferredRuntime>,
    output_dir: &Path,
    stores: &mut Stores,
    relay_allowlist: &RelayAllowlist,
) -> Result<Option<Lifetime>> {
    // --- Get a dedicated dir circuit for this sync cycle ---
    // As of arti 0.44 dirmgr()/circmgr() are fallible accessors.
    let dirmgr = client
        .dirmgr()
        .map_err(|e| anyhow::anyhow!("getting directory manager: {}", e))?;
    let circmgr = client
        .circmgr()
        .map_err(|e| anyhow::anyhow!("getting circuit manager: {}", e))?;
    let netdir = dirmgr
        .netdir(Timeliness::Timely)
        .map_err(|e| anyhow::anyhow!("getting network directory: {}", e))?;
    let tunnel = circmgr
        .get_or_launch_dir(DirInfo::Directory(&netdir))
        .await
        .map_err(|e| anyhow::anyhow!("getting dir circuit: {}", e))?;
    // Retire immediately so no other code reuses this circuit after we're done.
    circmgr.retire_circ(&tunnel.unique_id());
    tracing::info!("using dir circuit {}", tunnel.unique_id());

    // --- Fetch consensus (skip if still fresh) ---
    let old_digest = stores.consensus.diff_hex();
    let consensus_text = if stores.consensus.is_fresh() {
        tracing::info!("cached consensus is still fresh, skipping fetch");
        stores
            .consensus
            .text()
            .context("consensus marked fresh but no cached text")?
            .to_string()
    } else {
        let diff_hex = old_digest.clone();
        tracing::info!(
            "fetching consensus{}...",
            if diff_hex.is_some() { " (requesting diff)" } else { "" }
        );
        let consensus_bytes = match crate::dir::get(
            &tunnel,
            "/tor/status-vote/current/consensus-microdesc",
            diff_hex.as_deref(),
        )
        .await?
        {
            Some(bytes) => bytes,
            None => {
                tracing::info!("server returned 304 Not Modified, consensus unchanged");
                return Ok(None);
            }
        };
        let response_text = String::from_utf8(consensus_bytes)
            .context("consensus response is not valid UTF-8")?;
        stores.consensus.resolve_response(response_text)?
    };

    // --- Fetch authority certificates (only if coverage is incomplete) ---
    let authority_ids = AuthCertStore::trusted_authority_ids();
    let now = SystemTime::now();
    stores.certs.refresh(&now);
    if stores.certs.has_all() {
        tracing::info!(
            "authority certificates: {} cached, all authorities covered",
            stores.certs.certs().len(),
        );
    } else {
        tracing::info!("fetching authority certificates (missing coverage)...");
        let certs_bytes = crate::dir::get(&tunnel, "/tor/keys/all", None)
            .await?
            .context("unexpected 304 for /tor/keys/all")?;
        let certs_text =
            String::from_utf8(certs_bytes).context("authority certs are not valid UTF-8")?;
        stores.certs.update(certs_text, &now);
        tracing::info!(
            "authority certificates: {} trusted ({} bytes raw)",
            stores.certs.certs().len(),
            stores.certs.text().len(),
        );
    }

    // --- Parse and verify consensus (timeliness + signatures) ---
    let (_signed, _remainder, unchecked) =
        MdConsensus::parse(&consensus_text).context("parsing consensus")?;
    let unvalidated = unchecked
        .check_valid_at(&now)
        .map_err(|e: TimeValidityError| anyhow::anyhow!("consensus not timely: {}", e))?
        .set_n_authorities(authority_ids.len());

    let id_refs: Vec<&RsaIdentity> = authority_ids.iter().collect();
    if !unvalidated.authorities_are_correct(&id_refs) {
        anyhow::bail!("consensus not signed by enough recognized authorities");
    }

    let consensus = unvalidated
        .check_signature(stores.certs.certs())
        .map_err(|e| anyhow::anyhow!("consensus signature verification failed: {}", e))?;

    let lifetime = consensus.lifetime().clone();
    let num_relays = consensus.relays().len();
    tracing::info!(
        "consensus: {} relays, valid_after={}, fresh_until={}, valid_until={}",
        num_relays,
        humantime::format_rfc3339(lifetime.valid_after()),
        humantime::format_rfc3339(lifetime.fresh_until()),
        humantime::format_rfc3339(lifetime.valid_until()),
    );

    // --- Update relay allowlist for WS proxy ---
    {
        let addrs: std::collections::HashSet<std::net::SocketAddr> = consensus
            .relays()
            .iter()
            .flat_map(|rs| rs.addrs())
            .collect();
        tracing::info!("relay allowlist: {} addresses", addrs.len());
        *relay_allowlist.write().unwrap_or_else(|e| e.into_inner()) = addrs;
    }

    // --- Extract microdesc digests and diff against store ---
    let digests: Vec<_> = consensus
        .relays()
        .iter()
        .map(|rs| *rs.md_digest())
        .collect();

    stores.microdescs.retain(&digests);
    let missing = stores.microdescs.missing(&digests);
    tracing::info!(
        "microdescs: {} in consensus, {} cached, {} to fetch",
        digests.len(),
        stores.microdescs.len(),
        missing.len(),
    );

    // --- Fetch only missing microdescs in batches ---
    let batch_size = 500;
    if !missing.is_empty() {
        let total_batches = (missing.len() + batch_size - 1) / batch_size;
        for (batch_idx, batch) in missing.chunks(batch_size).enumerate() {
            tracing::info!(
                "fetching microdescs batch {}/{}...",
                batch_idx + 1,
                total_batches,
            );

            let digests_str: Vec<String> = batch
                .iter()
                .map(|d| base64ct::Base64Unpadded::encode_string(d))
                .collect();
            let path = format!("/tor/micro/d/{}", digests_str.join("-"));

            match crate::dir::get(&tunnel, &path, None).await {
                Ok(Some(bytes)) => {
                    let text = String::from_utf8(bytes)
                        .context("microdesc response is not valid UTF-8")?;
                    let added = stores.microdescs.ingest(&text);
                    tracing::debug!("batch {}: added {} microdescs", batch_idx + 1, added);
                }
                Ok(None) => {
                    tracing::warn!("microdesc batch {} returned 304", batch_idx + 1);
                }
                Err(e) => {
                    tracing::warn!("microdesc batch {} failed: {}", batch_idx + 1, e);
                }
            }
        }
    }

    let still_missing = stores.microdescs.missing(&digests);
    tracing::info!(
        "microdescs: {} cached ({} still missing)",
        stores.microdescs.len(),
        still_missing.len(),
    );

    // --- Write files atomically (write to .tmp, then rename) ---
    atomic_write(output_dir, "consensus-microdesc.txt", consensus_text.as_bytes())?;
    tracing::info!(
        "wrote consensus-microdesc ({} bytes)",
        consensus_text.len()
    );

    atomic_write(output_dir, "authority-certs.txt", stores.certs.text().as_bytes())?;
    tracing::info!("wrote authority-certs ({} bytes)", stores.certs.text().len());

    let microdescs_blob = stores.microdescs.to_concatenated();
    atomic_write(output_dir, "microdescs.txt", &microdescs_blob)?;
    tracing::info!("wrote microdescs ({} bytes)", microdescs_blob.len());

    let metadata = serde_json::json!({
        "consensus_flavor": "microdesc",
        "valid_after": humantime::format_rfc3339(lifetime.valid_after()).to_string(),
        "fresh_until": humantime::format_rfc3339(lifetime.fresh_until()).to_string(),
        "valid_until": humantime::format_rfc3339(lifetime.valid_until()).to_string(),
        "num_relays": num_relays,
        "authority_certs_bytes": stores.certs.text().len(),
        "num_microdescs_in_cache": stores.microdescs.len(),
        "num_microdescs_missing": still_missing.len(),
        "microdescs_bytes": microdescs_blob.len(),
        "synced_at": humantime::format_rfc3339(SystemTime::now()).to_string(),
    });
    atomic_write(
        output_dir,
        "metadata.json",
        serde_json::to_string_pretty(&metadata)?.as_bytes(),
    )?;

    // --- Create bootstrap archive if consensus changed or file missing ---
    let new_digest = stores.consensus.diff_hex();
    if new_digest != old_digest || !output_dir.join("bootstrap.zip").exists() {
        write_bootstrap_archive(output_dir, consensus_text.as_bytes(), stores.certs.text().as_bytes(), &microdescs_blob)?;
    } else {
        tracing::info!("consensus unchanged, skipping bootstrap archive");
    }

    Ok(Some(lifetime))
}

/// Create `bootstrap.zip.zst`: a store-only zip of the bootstrap files,
/// zstd-compressed (level 9).
fn write_bootstrap_archive(dir: &Path, consensus: &[u8], certs: &[u8], microdescs: &[u8]) -> Result<()> {
    use zip::write::SimpleFileOptions;
    use zip::CompressionMethod;

    // Build store-only zip in memory
    let mut zip_buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut zip_buf));
        let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);

        zip.start_file("bootstrap/consensus-microdesc.txt", opts)?;
        zip.write_all(consensus)?;

        zip.start_file("bootstrap/authority-certs.txt", opts)?;
        zip.write_all(certs)?;

        zip.start_file("bootstrap/microdescs.txt", opts)?;
        zip.write_all(microdescs)?;

        zip.finish()?;
    }

    // Zstd-compress the zip (level 9).
    let zst_buf = zstd::encode_all(&zip_buf[..], 9).context("zstd-compressing bootstrap archive")?;

    // ETag: SHA3-256 of the uncompressed zip archive
    use digest::Digest;
    let etag = hex::encode(sha3::Sha3_256::digest(&zip_buf));
    atomic_write(dir, "bootstrap.etag", etag.as_bytes())?;

    atomic_write(dir, "bootstrap.zip", &zip_buf)?;
    atomic_write(dir, "bootstrap.zip.zst", &zst_buf)?;
    tracing::info!(
        "wrote bootstrap.zip ({} bytes), .zst ({} bytes)",
        zip_buf.len(),
        zst_buf.len(),
    );
    Ok(())
}

/// Compute the relay-style sync delay.
///
/// Per dir-spec §5.3 (download-ns-from-auth):
///
///   "The cache downloads a new consensus document at a randomly chosen
///    time in the first half-interval after its current consensus stops
///    being fresh."
///
/// The "interval" is `valid_until - fresh_until`.  With typical values
/// (fresh_until = valid_after + 1h, valid_until = valid_after + 3h) the
/// interval is 2h, so the first half-interval is 1h.  We pick a random
/// instant in `[fresh_until, fresh_until + interval/2]` and return the
/// duration from now until that instant.
///
/// Ref: https://spec.torproject.org/dir-spec/directory-cache-operation.html#download-ns-from-auth
pub fn relay_sync_delay(fresh_until: SystemTime, valid_until: SystemTime) -> Duration {
    let interval = valid_until
        .duration_since(fresh_until)
        .unwrap_or(Duration::from_secs(3600));
    let half_interval = interval / 2;
    let offset = rand::rng().random_range(Duration::ZERO..=half_interval);
    let target = fresh_until + offset;
    target
        .duration_since(SystemTime::now())
        .unwrap_or(Duration::ZERO)
}

/// Write `data` to `dir/name` atomically via a `.tmp` intermediate.
fn atomic_write(dir: &Path, name: &str, data: &[u8]) -> Result<()> {
    let tmp = dir.join(format!("{}.tmp", name));
    let dst = dir.join(name);
    std::fs::write(&tmp, data).with_context(|| format!("writing {:?}", tmp))?;
    std::fs::rename(&tmp, &dst).with_context(|| format!("renaming to {:?}", dst))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    // ---- atomic_write ----------------------------------------------------

    #[test]
    fn atomic_write_replaces_contents_and_leaves_no_temp_file() {
        let dir = TempDir::new("sync-atomic");

        atomic_write(dir.path(), "f.txt", b"first").unwrap();
        assert_eq!(std::fs::read(dir.join("f.txt")).unwrap(), b"first");

        atomic_write(dir.path(), "f.txt", b"second").unwrap();
        assert_eq!(std::fs::read(dir.join("f.txt")).unwrap(), b"second");

        assert!(!dir.join("f.txt.tmp").exists(), "the intermediate is renamed away");
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["f.txt".to_string()]);
    }

    #[test]
    fn atomic_write_reports_the_path_it_could_not_write() {
        let dir = TempDir::new("sync-atomic-missing");
        let err = atomic_write(&dir.join("no-such-subdir"), "f.txt", b"x").unwrap_err();
        assert!(format!("{err:#}").contains("f.txt.tmp"), "{err:#}");
    }

    // ---- relay_sync_delay ------------------------------------------------

    fn sample_delays(fresh_in: i64, lifetime_secs: u64, n: usize) -> Vec<Duration> {
        let now = SystemTime::now();
        let fresh_until = if fresh_in >= 0 {
            now + Duration::from_secs(fresh_in as u64)
        } else {
            now - Duration::from_secs((-fresh_in) as u64)
        };
        let valid_until = fresh_until + Duration::from_secs(lifetime_secs);
        (0..n).map(|_| relay_sync_delay(fresh_until, valid_until)).collect()
    }

    /// dir-spec §5.3: refetch at a random instant in the first half of
    /// `[fresh_until, valid_until]`.
    #[test]
    fn sync_delay_lands_in_the_first_half_interval() {
        // fresh_until 1h out, interval 2h → target in [+1h, +2h].
        let delays = sample_delays(3600, 7200, 200);
        for d in &delays {
            assert!(
                *d >= Duration::from_secs(3595) && *d <= Duration::from_secs(7200),
                "delay {d:?} outside the first half-interval"
            );
        }
        let spread = delays.iter().max().unwrap().as_secs() - delays.iter().min().unwrap().as_secs();
        assert!(spread > 600, "delays should be randomised, spread was {spread}s");
    }

    /// An already-stale consensus must not produce a long wait: the refetch
    /// window opened in the past, so only the random offset remains.
    #[test]
    fn a_stale_consensus_refetches_within_the_remaining_window() {
        let delays = sample_delays(-3600, 7200, 200);
        for d in &delays {
            assert!(*d <= Duration::from_secs(3600), "delay {d:?} too long for a stale consensus");
        }
        assert!(
            delays.iter().any(|d| *d == Duration::ZERO),
            "some draws should be due immediately"
        );
    }

    #[test]
    fn a_degenerate_lifetime_falls_back_to_an_hour_interval() {
        // valid_until == fresh_until: duration_since fails, so the documented
        // 1h fallback applies and the offset is at most half of it.
        let now = SystemTime::now();
        let fresh_until = now + Duration::from_secs(60);
        for _ in 0..200 {
            let d = relay_sync_delay(fresh_until, fresh_until);
            assert!(d <= Duration::from_secs(60 + 1800), "delay {d:?} exceeds the fallback");
        }
        // valid_until *before* fresh_until takes the same path rather than panicking.
        let d = relay_sync_delay(fresh_until, now);
        assert!(d <= Duration::from_secs(60 + 1800), "{d:?}");
    }

    #[test]
    fn sync_delay_never_returns_a_negative_or_panics_at_the_extremes() {
        let now = SystemTime::now();
        let far_past = now - Duration::from_secs(86_400 * 365);
        let far_future = now + Duration::from_secs(86_400 * 365);
        assert!(relay_sync_delay(far_past, far_past + Duration::from_secs(1)) == Duration::ZERO);
        assert!(relay_sync_delay(far_future, far_future) > Duration::ZERO);
        // A huge interval is halved, not overflowed.
        let d = relay_sync_delay(now, far_future);
        assert!(d <= Duration::from_secs(86_400 * 365 / 2 + 1), "{d:?}");
    }

    // ---- bootstrap archive ----------------------------------------------

    /// Mirrors the assumptions of the *consumer*,
    /// `parse_stored_zip` in `crates/tor-js-wasm/src/fast_bootstrap.rs`: walk
    /// local file headers from offset 0, require Stored (method 0), and read
    /// each member's bytes from the sizes in its own local header. The two
    /// crates cannot share code (one is a wasm cdylib built against the arti
    /// fork), so this test pins the wire format the client relies on.
    fn parse_like_the_client(data: &[u8]) -> Vec<(String, Vec<u8>)> {
        let mut files = Vec::new();
        let mut offset = 0usize;
        while offset + 30 <= data.len() {
            let sig = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
            if sig != 0x04034b50 {
                break; // central directory reached
            }
            let method = u16::from_le_bytes(data[offset + 8..offset + 10].try_into().unwrap());
            assert_eq!(method, 0, "the client only implements Stored");
            let compressed_size =
                u32::from_le_bytes(data[offset + 18..offset + 22].try_into().unwrap()) as usize;
            let name_len =
                u16::from_le_bytes(data[offset + 26..offset + 28].try_into().unwrap()) as usize;
            let extra_len =
                u16::from_le_bytes(data[offset + 28..offset + 30].try_into().unwrap()) as usize;

            let name_start = offset + 30;
            let name_end = name_start + name_len;
            let data_start = name_end + extra_len;
            let data_end = data_start + compressed_size;
            assert!(data_end <= data.len(), "member extends past the archive");

            let name = std::str::from_utf8(&data[name_start..name_end])
                .expect("member names must be UTF-8")
                .to_string();
            files.push((name, data[data_start..data_end].to_vec()));
            offset = data_end;
        }
        files
    }

    #[test]
    fn the_bootstrap_archive_is_readable_by_the_clients_parser() {
        let dir = TempDir::new("sync-archive");
        let consensus = b"network-status-version 3 microdesc\n".repeat(10);
        let certs = b"dir-key-certificate-version 3\n".repeat(5);
        let microdescs = b"onion-key\n".repeat(20);

        write_bootstrap_archive(dir.path(), &consensus, &certs, &microdescs).unwrap();

        let zip_bytes = std::fs::read(dir.join("bootstrap.zip")).unwrap();
        let members = parse_like_the_client(&zip_bytes);
        assert_eq!(
            members.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            vec![
                "bootstrap/consensus-microdesc.txt",
                "bootstrap/authority-certs.txt",
                "bootstrap/microdescs.txt",
            ],
            "names and order the client expects"
        );
        assert_eq!(members[0].1, consensus);
        assert_eq!(members[1].1, certs);
        assert_eq!(members[2].1, microdescs);
    }

    #[test]
    fn the_compressed_archive_decompresses_to_the_plain_one() {
        let dir = TempDir::new("sync-archive-zst");
        write_bootstrap_archive(dir.path(), b"c", b"a", b"m").unwrap();

        let zip_bytes = std::fs::read(dir.join("bootstrap.zip")).unwrap();
        let zst_bytes = std::fs::read(dir.join("bootstrap.zip.zst")).unwrap();
        assert_eq!(zstd::decode_all(&zst_bytes[..]).unwrap(), zip_bytes);
    }

    /// The ETag is over the *uncompressed* zip, which is what
    /// `read_etag`'s fallback recomputes from `bootstrap.zip`.
    #[test]
    fn the_etag_covers_the_uncompressed_archive() {
        use digest::Digest;
        let dir = TempDir::new("sync-archive-etag");
        write_bootstrap_archive(dir.path(), b"c", b"a", b"m").unwrap();

        let zip_bytes = std::fs::read(dir.join("bootstrap.zip")).unwrap();
        let etag = std::fs::read_to_string(dir.join("bootstrap.etag")).unwrap();
        assert_eq!(etag, hex::encode(sha3::Sha3_256::digest(&zip_bytes)));
    }

    #[test]
    fn different_inputs_get_different_etags() {
        let dir = TempDir::new("sync-archive-etag2");
        write_bootstrap_archive(dir.path(), b"c1", b"a", b"m").unwrap();
        let first = std::fs::read_to_string(dir.join("bootstrap.etag")).unwrap();
        write_bootstrap_archive(dir.path(), b"c2", b"a", b"m").unwrap();
        let second = std::fs::read_to_string(dir.join("bootstrap.etag")).unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn empty_members_are_still_valid_archive_entries() {
        // A gateway that has not yet fetched certs writes an empty member
        // rather than omitting it; the client indexes members by name.
        let dir = TempDir::new("sync-archive-empty");
        write_bootstrap_archive(dir.path(), b"", b"", b"").unwrap();
        let zip_bytes = std::fs::read(dir.join("bootstrap.zip")).unwrap();
        let members = parse_like_the_client(&zip_bytes);
        assert_eq!(members.len(), 3);
        assert!(members.iter().all(|(_, bytes)| bytes.is_empty()));
    }
}
