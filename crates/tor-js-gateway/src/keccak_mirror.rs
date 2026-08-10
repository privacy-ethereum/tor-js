//! Mirroring the hash-addressed object tree from a branch of a GitHub
//! repository (PROTOCOL.md §5, `worker-bundles`).
//!
//! The branch *is* the publishing interface: every file in it must be named
//! for its own content — `<hh>/<rest>`, the 64 lowercase hex chars of
//! `keccak256(bytes)` split after the first byte — and the gateway reproduces
//! that tree under `<data_dir>/keccak`. Publishing a bundle is a `git push`;
//! unpublishing is deleting the file. Anything else in the branch is ignored,
//! so a README or a licence in there costs nothing.
//!
//! The mirror **owns** its directory: it adds what the branch gained and
//! deletes what the branch dropped, which is why the path is derived from
//! `data_dir` rather than configured. Objects are hashed before they are put
//! in place, so a file that exists is a file whose bytes match its name; the
//! route re-checks anyway, as defence against later corruption.
//!
//! Two things drive a sync: a poll (roughly daily) and a client trigger
//! (`POST /keccak/sync`). Triggers are throttled — the endpoint is
//! unauthenticated, so the throttle is what bounds how often anyone can make
//! this gateway talk to GitHub — and only one sync ever runs at a time.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio::time::Instant;
use tracing::{debug, error, info, warn};

use crate::routes::{is_lower_hex, keccak256_hex};

/// Per-object ceiling. Matches the anon-rpc harness's `MAX_BUNDLE_BYTES`, so
/// the mirror never stores an object no client would accept.
const MAX_OBJECT_BYTES: u64 = 64 * 1024 * 1024;

/// Objects fetched at once on a cold start. Enough to hide per-request latency
/// without making the gateway look like a stampede to the origin.
const OBJECT_CONCURRENCY: usize = 4;

/// Ceiling on tree entries, well above any plausible branch. Guards against
/// paging a pathological repository into memory before the `truncated` check.
const MAX_TREE_ENTRIES: usize = 100_000;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(180);

const DEFAULT_API_BASE: &str = "https://api.github.com";
const DEFAULT_RAW_BASE: &str = "https://raw.githubusercontent.com";

/// What a completed sync did.
#[derive(Debug, Clone, Serialize)]
pub struct SyncOutcome {
    /// Head commit of the mirrored branch.
    pub commit: String,
    pub added: usize,
    pub removed: usize,
    /// Objects the mirror is serving afterwards.
    pub objects: usize,
    /// Paths in the branch that are not hash-addressed objects, and objects
    /// refused for their own reasons (oversized, or contents that don't hash
    /// to their name). Never fatal — one bad file must not stop the mirror.
    pub ignored: usize,
    /// Nothing changed: the branch and the disk already agreed.
    pub unchanged: bool,
}

/// Observable state, served by `GET /keccak/sync`. Timestamps are RFC 3339
/// strings rather than epoch numbers because the only consumer is a human
/// checking whether the mirror is healthy.
#[derive(Debug, Clone, Serialize, Default)]
pub struct Status {
    pub repo: String,
    pub branch: String,
    pub commit: Option<String>,
    pub objects: usize,
    pub last_attempt: Option<String>,
    pub last_success: Option<String>,
    pub last_error: Option<String>,
}

/// Why a trigger did not run a sync. Each maps to one status code at the route.
#[derive(Debug)]
pub enum TriggerError {
    /// Syncing is switched off for this run (`--no-mirror`).
    Disabled,
    /// A manual sync ran too recently.
    Throttled { retry_after: Duration },
    /// A sync (poll or trigger) is already running.
    Busy,
    /// The sync ran and failed.
    Failed(anyhow::Error),
}

/// On-disk memo of the last successful sync. Purely an optimisation and a
/// diagnostic: everything in it is re-derived from the branch and the disk on
/// the next sync, so losing or corrupting it costs one redundant reconcile.
#[derive(Debug, Serialize, Deserialize)]
struct State {
    repo: String,
    branch: String,
    commit: String,
    objects: usize,
    synced_at: String,
}

pub struct Mirror {
    dir: PathBuf,
    state_path: PathBuf,
    repo: String,
    branch: String,
    api_base: String,
    raw_base: String,
    http: reqwest::Client,
    token: Option<String>,
    manual_min_interval: Duration,
    /// Cleared by `--no-mirror`, which promises the branch is not contacted at
    /// all — so it has to stop client triggers too, not just the poll.
    syncs_enabled: std::sync::atomic::AtomicBool,
    /// Held for the duration of a sync: one at a time, so two syncs can never
    /// reconcile the same directory against different trees.
    sync_lock: Mutex<()>,
    /// When a *manual* sync was last attempted. Attempted, not succeeded: a
    /// failing upstream must not become a way to keep hitting it.
    last_manual: Mutex<Option<Instant>>,
    status: RwLock<Status>,
}

impl Mirror {
    pub fn new(
        dir: PathBuf,
        state_path: PathBuf,
        repo: String,
        branch: String,
        manual_min_interval: Duration,
    ) -> Result<Arc<Self>> {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating object dir {}", dir.display()))?;

        // GitHub rejects requests without a User-Agent, and rate-limits
        // unauthenticated ones to 60/hour per IP. A daily poll plus the
        // throttled trigger fits inside that, but an operator sharing an
        // address with other traffic can lift it with a token.
        let token = std::env::var("GITHUB_TOKEN").ok().filter(|t| !t.is_empty());
        let http = reqwest::Client::builder()
            .user_agent(concat!("tor-js-gateway/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("building the HTTP client for the object mirror")?;

        let status = Status {
            repo: repo.clone(),
            branch: branch.clone(),
            ..Status::default()
        };

        Ok(Arc::new(Self {
            dir,
            state_path,
            repo,
            branch,
            // Test hooks: the integration suite points these at a local server
            // so the mirror can be exercised without reaching GitHub.
            api_base: env_base("TOR_JS_GATEWAY_GITHUB_API", DEFAULT_API_BASE),
            raw_base: env_base("TOR_JS_GATEWAY_GITHUB_RAW", DEFAULT_RAW_BASE),
            http,
            token,
            manual_min_interval,
            syncs_enabled: std::sync::atomic::AtomicBool::new(true),
            sync_lock: Mutex::new(()),
            last_manual: Mutex::new(None),
            status: RwLock::new(status),
        }))
    }

    /// Stop contacting the branch: no poll, and client triggers are refused.
    /// Whatever is on disk keeps being served.
    pub fn disable_syncs(&self) {
        self.syncs_enabled
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn status(&self) -> Status {
        self.status.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Adopt whatever a previous run left on disk, so a restart serves
    /// immediately instead of waiting for the first sync to finish.
    pub fn adopt_existing(&self) {
        let scan = match scan_objects(&self.dir) {
            Ok(scan) => scan,
            Err(e) => {
                warn!("object mirror: cannot read {}: {:#}", self.dir.display(), e);
                return;
            }
        };
        let state = self.read_state();
        let mut status = self.status.write().unwrap_or_else(|e| e.into_inner());
        status.objects = scan.objects.len();
        status.commit = state.as_ref().map(|s| s.commit.clone());
        status.last_success = state.as_ref().map(|s| s.synced_at.clone());
        drop(status);
        match state {
            Some(s) => info!(
                "object mirror: {} object(s) on disk from {}@{} ({}), serving while we re-check",
                scan.objects.len(),
                s.repo,
                short(&s.commit),
                s.synced_at,
            ),
            None => info!(
                "object mirror: {} object(s) on disk, no recorded sync",
                scan.objects.len()
            ),
        }
    }

    /// Poll forever: sync at startup, then at roughly `interval`.
    ///
    /// The interval is jittered so a fleet of gateways started by the same
    /// deployment doesn't converge on the same minute of the day, and a
    /// failure retries sooner than a whole cycle later.
    pub async fn poll_loop(self: Arc<Self>, interval: Duration) {
        let retry = Duration::from_secs(300).min(interval);
        loop {
            let delay = match self.sync().await {
                Ok(outcome) => {
                    if outcome.unchanged {
                        debug!(
                            "object mirror: up to date at {} ({} object(s))",
                            short(&outcome.commit),
                            outcome.objects
                        );
                    } else {
                        info!(
                            "object mirror: {} → +{} -{}, now {} object(s){}",
                            short(&outcome.commit),
                            outcome.added,
                            outcome.removed,
                            outcome.objects,
                            match outcome.ignored {
                                0 => String::new(),
                                n => format!(" ({n} path(s) ignored)"),
                            },
                        );
                    }
                    jitter(interval)
                }
                Err(e) => {
                    error!("object mirror: sync failed: {:#}", e);
                    jitter(retry)
                }
            };
            // Whole seconds: humantime otherwise prints the jitter's nanoseconds.
            debug!(
                "object mirror: next poll in {}",
                humantime::format_duration(Duration::from_secs(delay.as_secs()))
            );
            tokio::time::sleep(delay).await;
        }
    }

    /// A client-triggered sync: refused inside the throttle window, refused
    /// while another sync is running, otherwise run to completion so the caller
    /// learns whether the objects it wants are now here.
    pub async fn trigger(&self) -> Result<SyncOutcome, TriggerError> {
        if !self.syncs_enabled.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(TriggerError::Disabled);
        }
        // Take the throttle decision and stamp it under one lock, so two
        // simultaneous triggers cannot both pass the check.
        {
            let mut last = self.last_manual.lock().await;
            if let Some(at) = *last {
                let elapsed = at.elapsed();
                if elapsed < self.manual_min_interval {
                    return Err(TriggerError::Throttled {
                        retry_after: self.manual_min_interval - elapsed,
                    });
                }
            }
            *last = Some(Instant::now());
        }

        // Don't queue behind a running sync: the caller would wait on work that
        // started before its request and may already have missed the object it
        // is asking about.
        let Ok(guard) = self.sync_lock.try_lock() else {
            return Err(TriggerError::Busy);
        };
        info!("object mirror: sync triggered by a client");
        self.sync_locked(guard).await.map_err(TriggerError::Failed)
    }

    /// Reconcile the object directory with the branch.
    pub async fn sync(&self) -> Result<SyncOutcome> {
        let guard = self.sync_lock.lock().await;
        self.sync_locked(guard).await
    }

    async fn sync_locked(&self, _guard: tokio::sync::MutexGuard<'_, ()>) -> Result<SyncOutcome> {
        self.status
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .last_attempt = Some(now_rfc3339());

        let result = self.reconcile().await;
        let mut status = self.status.write().unwrap_or_else(|e| e.into_inner());
        match &result {
            Ok(outcome) => {
                status.commit = Some(outcome.commit.clone());
                status.objects = outcome.objects;
                status.last_success = Some(now_rfc3339());
                status.last_error = None;
            }
            Err(e) => status.last_error = Some(format!("{e:#}")),
        }
        result
    }

    async fn reconcile(&self) -> Result<SyncOutcome> {
        let commit = self.head_commit().await?;
        let tree = self.tree(&commit).await?;

        // Everything the branch says should exist, and everything that does.
        let mut wanted: BTreeSet<String> = BTreeSet::new();
        let mut ignored = tree.ignored;
        for (hash, size) in tree.objects {
            if size > MAX_OBJECT_BYTES {
                warn!(
                    "object mirror: skipping {} — {} bytes exceeds the {}-byte object cap",
                    short(&hash),
                    size,
                    MAX_OBJECT_BYTES
                );
                ignored += 1;
                continue;
            }
            wanted.insert(hash);
        }

        let scan = scan_objects(&self.dir)?;
        let missing: Vec<String> = wanted.difference(&scan.objects).cloned().collect();
        let stale: Vec<String> = scan.objects.difference(&wanted).cloned().collect();

        // Fetch first, prune second: if fetching fails we keep serving what we
        // have rather than ending up with neither the old nor the new object.
        let fetched = self.fetch_objects(&commit, &missing).await;
        let added = fetched.iter().filter(|r| r.is_ok()).count();
        let failed = fetched.len() - added;
        for err in fetched.into_iter().filter_map(Result::err) {
            error!("object mirror: {:#}", err);
        }
        ignored += failed;

        let mut removed = 0;
        for hash in &stale {
            let path = object_path(&self.dir, hash);
            match tokio::fs::remove_file(&path).await {
                Ok(()) => {
                    info!("object mirror: dropped {} (no longer in the branch)", short(hash));
                    removed += 1;
                }
                Err(e) => warn!("object mirror: cannot remove {}: {}", path.display(), e),
            }
        }
        // Leftovers from an interrupted sync, and anything an operator put here
        // by hand. The directory belongs to the mirror.
        for path in &scan.junk {
            let removed = match path.is_dir() {
                true => tokio::fs::remove_dir_all(path).await,
                false => tokio::fs::remove_file(path).await,
            };
            match removed {
                Ok(()) => debug!("object mirror: removed stray {}", path.display()),
                Err(e) => debug!("object mirror: cannot remove stray {}: {}", path.display(), e),
            }
        }
        prune_empty_shards(&self.dir).await;

        let objects = scan_objects(&self.dir)?.objects.len();
        let outcome = SyncOutcome {
            commit,
            added,
            removed,
            objects,
            ignored,
            unchanged: added == 0 && removed == 0,
        };
        self.write_state(&outcome);
        Ok(outcome)
    }

    /// Fetch the missing objects, verifying each before it lands. A failure is
    /// per-object: the rest of the sync still applies.
    async fn fetch_objects(&self, commit: &str, missing: &[String]) -> Vec<Result<()>> {
        // Owned hashes, not borrows: a closure yielding a future that borrows
        // its argument is not general enough over lifetimes for the combinator.
        stream::iter(missing.to_vec())
            .map(|hash| async move {
                self.fetch_object(commit, &hash)
                    .await
                    .with_context(|| format!("fetching object {}", short(&hash)))
            })
            .buffer_unordered(OBJECT_CONCURRENCY)
            .collect()
            .await
    }

    async fn fetch_object(&self, commit: &str, hash: &str) -> Result<()> {
        let url = format!(
            "{}/{}/{}/{}/{}",
            self.raw_base,
            self.repo,
            commit,
            &hash[..2],
            &hash[2..]
        );
        let res = self.http.get(&url).send().await.context("requesting")?;
        let status = res.status();
        if !status.is_success() {
            anyhow::bail!("HTTP {} from {}", status.as_u16(), url);
        }
        // Cheap pre-check on the advertised length; the real bound is the read
        // below, since Content-Length is not a promise.
        if let Some(len) = res.content_length()
            && len > MAX_OBJECT_BYTES
        {
            anyhow::bail!("{} bytes exceeds the {}-byte object cap", len, MAX_OBJECT_BYTES);
        }
        let bytes = read_capped(res, MAX_OBJECT_BYTES).await?;

        // The name is the hash: this is the check that makes serving these
        // bytes safe, and it happens before they are anywhere a client can
        // reach. A mismatch means the branch is lying about its own content.
        let actual = keccak256_hex(&bytes);
        if actual != hash {
            anyhow::bail!("keccak256 of the contents is {actual}, but the branch filed it as {hash}");
        }

        let final_path = object_path(&self.dir, hash);
        let shard = final_path.parent().expect("object paths have a shard dir");
        tokio::fs::create_dir_all(shard)
            .await
            .with_context(|| format!("creating {}", shard.display()))?;
        // Write-then-rename: a reader either sees no file or sees all of it,
        // never a half-written object that would fail its own hash check.
        let tmp = shard.join(format!(".{}.tmp", &hash[2..]));
        tokio::fs::write(&tmp, &bytes)
            .await
            .with_context(|| format!("writing {}", tmp.display()))?;
        tokio::fs::rename(&tmp, &final_path)
            .await
            .with_context(|| format!("renaming into {}", final_path.display()))?;
        info!("object mirror: added {} ({} bytes)", short(hash), bytes.len());
        Ok(())
    }

    async fn head_commit(&self) -> Result<String> {
        #[derive(Deserialize)]
        struct Ref {
            object: RefObject,
        }
        #[derive(Deserialize)]
        struct RefObject {
            sha: String,
        }

        let url = format!("{}/repos/{}/git/ref/heads/{}", self.api_base, self.repo, self.branch);
        let res = self.get_api(&url).await?;
        let sha = res.json::<Ref>().await.context("parsing the ref document")?.object.sha;
        if !is_lower_hex(&sha, 40) {
            anyhow::bail!("branch head {sha:?} is not a commit sha");
        }
        Ok(sha)
    }

    async fn tree(&self, commit: &str) -> Result<Tree> {
        #[derive(Deserialize)]
        struct TreeDoc {
            tree: Vec<Entry>,
            #[serde(default)]
            truncated: bool,
        }
        #[derive(Deserialize)]
        struct Entry {
            path: String,
            #[serde(rename = "type")]
            kind: String,
            #[serde(default)]
            size: u64,
        }

        let url = format!(
            "{}/repos/{}/git/trees/{}?recursive=1",
            self.api_base, self.repo, commit
        );
        let doc = self
            .get_api(&url)
            .await?
            .json::<TreeDoc>()
            .await
            .context("parsing the tree document")?;

        // A truncated listing is not a smaller branch. Pruning against one
        // would delete objects that are still published, so refuse the whole
        // sync and keep serving what we have.
        if doc.truncated {
            anyhow::bail!(
                "the tree listing for {}@{} is truncated — the branch has too many entries \
                 to mirror safely",
                self.repo,
                self.branch
            );
        }
        if doc.tree.len() > MAX_TREE_ENTRIES {
            anyhow::bail!("the tree has {} entries, more than this mirror will scan", doc.tree.len());
        }

        let mut objects = Vec::new();
        let mut ignored = 0;
        for entry in doc.tree {
            match parse_object_path(&entry.path) {
                Some(hash) if entry.kind == "blob" => objects.push((hash, entry.size)),
                _ => ignored += 1,
            }
        }
        Ok(Tree { objects, ignored })
    }

    async fn get_api(&self, url: &str) -> Result<reqwest::Response> {
        let mut req = self
            .http
            .get(url)
            .header("accept", "application/vnd.github+json")
            .header("x-github-api-version", "2022-11-28");
        if let Some(token) = &self.token {
            req = req.bearer_auth(token);
        }
        let res = req.send().await.with_context(|| format!("requesting {url}"))?;
        let status = res.status();
        if status.is_success() {
            return Ok(res);
        }
        // Rate limiting is the failure an operator is most likely to hit, and
        // the least self-evident from a bare status code.
        let remaining = header(&res, "x-ratelimit-remaining");
        if (status.as_u16() == 403 || status.as_u16() == 429) && remaining.as_deref() == Some("0") {
            anyhow::bail!(
                "HTTP {} from {url}: GitHub rate limit exhausted{}{}",
                status.as_u16(),
                header(&res, "x-ratelimit-reset")
                    .map(|r| format!(", resets at {r} (epoch seconds)"))
                    .unwrap_or_default(),
                match self.token.is_some() {
                    true => "",
                    false => "; set GITHUB_TOKEN to raise the limit",
                },
            );
        }
        if status.as_u16() == 404 {
            anyhow::bail!(
                "HTTP 404 from {url}: no branch {:?} in {} (or the repository is private)",
                self.branch,
                self.repo
            );
        }
        anyhow::bail!("HTTP {} from {url}", status.as_u16());
    }

    fn read_state(&self) -> Option<State> {
        let text = std::fs::read_to_string(&self.state_path).ok()?;
        let state: State = serde_json::from_str(&text).ok()?;
        // A state file describing a different source says nothing about this
        // one; the reconcile will rebuild it.
        (state.repo == self.repo && state.branch == self.branch).then_some(state)
    }

    fn write_state(&self, outcome: &SyncOutcome) {
        let state = State {
            repo: self.repo.clone(),
            branch: self.branch.clone(),
            commit: outcome.commit.clone(),
            objects: outcome.objects,
            synced_at: now_rfc3339(),
        };
        match serde_json::to_vec_pretty(&state) {
            Ok(bytes) => {
                if let Err(e) = std::fs::write(&self.state_path, bytes) {
                    // Cosmetic: everything in it is re-derived next sync.
                    debug!("object mirror: cannot write {}: {}", self.state_path.display(), e);
                }
            }
            Err(e) => debug!("object mirror: cannot serialize state: {e}"),
        }
    }
}

struct Tree {
    objects: Vec<(String, u64)>,
    ignored: usize,
}

struct Scan {
    objects: BTreeSet<String>,
    /// Paths under the object dir that are not objects — stale `.tmp` files
    /// from an interrupted sync, or anything added by hand.
    junk: Vec<PathBuf>,
}

fn env_base(var: &str, default: &str) -> String {
    std::env::var(var)
        .ok()
        .filter(|v| !v.is_empty())
        .map(|v| v.trim_end_matches('/').to_string())
        .unwrap_or_else(|| default.to_string())
}

/// `<hh>/<rest>` → the 64-char hash, for paths that are exactly that.
pub fn parse_object_path(path: &str) -> Option<String> {
    let (prefix, rest) = path.split_once('/')?;
    (is_lower_hex(prefix, 2) && is_lower_hex(rest, 62)).then(|| format!("{prefix}{rest}"))
}

fn object_path(dir: &Path, hash: &str) -> PathBuf {
    dir.join(&hash[..2]).join(&hash[2..])
}

/// The objects present under `dir`, plus everything there that isn't one.
fn scan_objects(dir: &Path) -> Result<Scan> {
    let mut objects = BTreeSet::new();
    let mut junk = Vec::new();
    let shards = match std::fs::read_dir(dir) {
        Ok(shards) => shards,
        // Nothing mirrored yet is not an error.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Scan { objects, junk }),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    for shard in shards.flatten() {
        let name = shard.file_name().to_string_lossy().into_owned();
        let is_shard = is_lower_hex(&name, 2) && shard.path().is_dir();
        if !is_shard {
            junk.push(shard.path());
            continue;
        }
        let entries = match std::fs::read_dir(shard.path()) {
            Ok(entries) => entries,
            Err(e) => {
                warn!("object mirror: cannot read {}: {}", shard.path().display(), e);
                continue;
            }
        };
        for entry in entries.flatten() {
            let rest = entry.file_name().to_string_lossy().into_owned();
            if is_lower_hex(&rest, 62) && entry.path().is_file() {
                objects.insert(format!("{name}{rest}"));
            } else {
                junk.push(entry.path());
            }
        }
    }
    Ok(Scan { objects, junk })
}

/// Remove shard directories left empty by pruning, so the tree doesn't
/// accumulate 256 empty dirs over a repository's life.
async fn prune_empty_shards(dir: &Path) {
    let Ok(shards) = std::fs::read_dir(dir) else {
        return;
    };
    for shard in shards.flatten() {
        let path = shard.path();
        if !path.is_dir() {
            continue;
        }
        if std::fs::read_dir(&path).ok().and_then(|mut d| d.next()).is_none() {
            let _ = tokio::fs::remove_dir(&path).await;
        }
    }
}

/// Read a response body, refusing to buffer more than `max` bytes. Streaming
/// rather than `bytes()` so a lying `Content-Length` can't make us allocate
/// the whole thing first.
async fn read_capped(res: reqwest::Response, max: u64) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(res.content_length().unwrap_or(0).min(max) as usize);
    let mut stream = res.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading the body")?;
        if out.len() as u64 + chunk.len() as u64 > max {
            anyhow::bail!("body exceeds the {max}-byte object cap");
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// ±10% so a fleet started together doesn't poll in lockstep.
fn jitter(base: Duration) -> Duration {
    use rand::Rng;
    let span = base.as_secs_f64() * 0.1;
    let offset = rand::rng().random_range(-span..=span);
    Duration::from_secs_f64((base.as_secs_f64() + offset).max(1.0))
}

fn header(res: &reqwest::Response, name: &str) -> Option<String> {
    res.headers().get(name)?.to_str().ok().map(str::to_string)
}

fn now_rfc3339() -> String {
    humantime::format_rfc3339_seconds(SystemTime::now()).to_string()
}

/// Hashes are 64 hex chars; logs are more readable with the first 12.
fn short(hash: &str) -> &str {
    &hash[..hash.len().min(12)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    #[test]
    fn only_exactly_shaped_paths_are_objects() {
        let h = "a".repeat(62);
        assert_eq!(parse_object_path(&format!("ab/{h}")), Some(format!("ab{h}")));
        // Everything a real branch also contains, or might.
        assert_eq!(parse_object_path("README.md"), None);
        assert_eq!(parse_object_path(&format!("ab/{h}.js")), None); // extension
        assert_eq!(parse_object_path(&format!("AB/{h}")), None); // uppercase shard
        assert_eq!(parse_object_path(&format!("ab/{}", "a".repeat(61))), None); // short
        assert_eq!(parse_object_path(&format!("ab/{}", "a".repeat(63))), None); // long
        assert_eq!(parse_object_path(&"a".repeat(64)), None); // unsharded
        assert_eq!(parse_object_path(&format!("x/ab/{h}")), None); // nested
        assert_eq!(parse_object_path(&format!("ab/cd/{}", "a".repeat(60))), None);
    }

    #[test]
    fn scanning_separates_objects_from_everything_else() {
        let dir = TempDir::new("mirror-scan");
        let root = dir.path();
        let good = format!("{}{}", "ab", "1".repeat(62));
        std::fs::create_dir_all(root.join("ab")).unwrap();
        std::fs::write(object_path(root, &good), b"x").unwrap();
        // A stale temp file from an interrupted sync, a hand-placed file, and a
        // directory whose name is not a shard.
        std::fs::write(root.join("ab").join(".stale.tmp"), b"x").unwrap();
        std::fs::write(root.join("notes.txt"), b"x").unwrap();
        std::fs::create_dir_all(root.join("zz")).unwrap();
        // A well-named shard that happens to be empty is not junk.
        std::fs::create_dir_all(root.join("cd")).unwrap();

        let scan = scan_objects(root).unwrap();
        assert_eq!(scan.objects, BTreeSet::from([good]));
        assert_eq!(scan.junk.len(), 3, "junk: {:?}", scan.junk);
        assert!(scan.junk.iter().any(|p| p.ends_with(".stale.tmp")));
        assert!(scan.junk.iter().any(|p| p.ends_with("notes.txt")));
        assert!(scan.junk.iter().any(|p| p.ends_with("zz")), "non-hex dir is junk");
        assert!(!scan.junk.iter().any(|p| p.ends_with("cd")));
    }

    #[test]
    fn scanning_a_missing_directory_is_empty_not_an_error() {
        let dir = TempDir::new("mirror-absent");
        let scan = scan_objects(&dir.join("never-created")).unwrap();
        assert!(scan.objects.is_empty());
        assert!(scan.junk.is_empty());
    }

    #[tokio::test]
    async fn empty_shard_dirs_are_pruned_but_populated_ones_are_kept() {
        let dir = TempDir::new("mirror-prune");
        let root = dir.path();
        std::fs::create_dir_all(root.join("ab")).unwrap();
        std::fs::create_dir_all(root.join("cd")).unwrap();
        std::fs::write(root.join("cd").join("1".repeat(62)), b"x").unwrap();

        prune_empty_shards(root).await;
        assert!(!root.join("ab").exists(), "empty shard should be gone");
        assert!(root.join("cd").exists(), "populated shard must stay");
    }

    #[test]
    fn jitter_stays_within_ten_percent() {
        let base = Duration::from_secs(86_400);
        for _ in 0..200 {
            let d = jitter(base);
            assert!(
                d >= Duration::from_secs_f64(base.as_secs_f64() * 0.9)
                    && d <= Duration::from_secs_f64(base.as_secs_f64() * 1.1),
                "{d:?} outside ±10% of {base:?}"
            );
        }
    }

    #[test]
    fn jitter_never_returns_zero_for_a_tiny_interval() {
        // Guards the sleep in poll_loop against becoming a busy loop.
        assert!(jitter(Duration::from_secs(1)) >= Duration::from_secs(1));
    }

    #[test]
    fn env_bases_lose_their_trailing_slash_and_ignore_empties() {
        assert_eq!(env_base("TJG_NOT_SET_AT_ALL", "https://d"), "https://d");
        // SAFETY: single-threaded test, no other thread reads the environment.
        unsafe {
            std::env::set_var("TJG_TEST_BASE", "http://127.0.0.1:9/");
            std::env::set_var("TJG_TEST_EMPTY", "");
        }
        assert_eq!(env_base("TJG_TEST_BASE", "https://d"), "http://127.0.0.1:9");
        assert_eq!(env_base("TJG_TEST_EMPTY", "https://d"), "https://d");
        unsafe {
            std::env::remove_var("TJG_TEST_BASE");
            std::env::remove_var("TJG_TEST_EMPTY");
        }
    }

    fn mirror(dir: &TempDir, manual_min_interval: Duration) -> Arc<Mirror> {
        Mirror::new(
            dir.join("keccak"),
            dir.join("keccak.json"),
            "owner/repo".into(),
            "keccak".into(),
            manual_min_interval,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn a_second_trigger_inside_the_window_is_throttled() {
        let dir = TempDir::new("mirror-throttle");
        // api_base points at the real default, but the throttle is decided
        // before any request — the first trigger fails on the network, the
        // second must be refused without trying.
        let m = mirror(&dir, Duration::from_secs(1800));
        match m.trigger().await {
            Err(TriggerError::Failed(_)) | Ok(_) => {}
            Err(e) => panic!("first trigger should have run, got {e:?}"),
        }
        match m.trigger().await {
            Err(TriggerError::Throttled { retry_after }) => {
                assert!(retry_after <= Duration::from_secs(1800));
                assert!(retry_after > Duration::from_secs(1700), "{retry_after:?}");
            }
            other => panic!("expected Throttled, got {other:?}"),
        }
    }

    /// The window is stamped on *attempt*: a trigger that fails upstream still
    /// consumes it, so a broken origin can't be hammered through this endpoint.
    #[tokio::test]
    async fn a_failed_trigger_still_consumes_the_window() {
        let dir = TempDir::new("mirror-throttle-fail");
        let m = mirror(&dir, Duration::from_secs(600));
        let first = m.trigger().await;
        assert!(
            matches!(first, Err(TriggerError::Failed(_))) || first.is_ok(),
            "unexpected: {first:?}"
        );
        assert!(matches!(m.trigger().await, Err(TriggerError::Throttled { .. })));
    }

    #[tokio::test]
    async fn a_zero_window_lets_every_trigger_through() {
        let dir = TempDir::new("mirror-nothrottle");
        let m = mirror(&dir, Duration::ZERO);
        for _ in 0..2 {
            assert!(
                !matches!(m.trigger().await, Err(TriggerError::Throttled { .. })),
                "a zero window must not throttle"
            );
        }
    }

    #[tokio::test]
    async fn a_trigger_is_refused_while_a_sync_holds_the_lock() {
        let dir = TempDir::new("mirror-busy");
        let m = mirror(&dir, Duration::ZERO);
        let held = m.sync_lock.lock().await;
        assert!(matches!(m.trigger().await, Err(TriggerError::Busy)));
        drop(held);
    }

    #[tokio::test]
    async fn adopting_a_directory_reports_what_is_there() {
        let dir = TempDir::new("mirror-adopt");
        let m = mirror(&dir, Duration::ZERO);
        let hash = format!("ab{}", "2".repeat(62));
        std::fs::create_dir_all(dir.join("keccak").join("ab")).unwrap();
        std::fs::write(object_path(&dir.join("keccak"), &hash), b"x").unwrap();

        m.adopt_existing();
        let status = m.status();
        assert_eq!(status.objects, 1);
        assert_eq!(status.repo, "owner/repo");
        assert_eq!(status.commit, None, "no state file yet");
    }

    #[test]
    fn state_from_a_different_source_is_ignored() {
        let dir = TempDir::new("mirror-state");
        let m = mirror(&dir, Duration::ZERO);
        let write = |repo: &str, branch: &str| {
            let state = State {
                repo: repo.into(),
                branch: branch.into(),
                commit: "c".repeat(40),
                objects: 3,
                synced_at: now_rfc3339(),
            };
            std::fs::write(dir.join("keccak.json"), serde_json::to_vec(&state).unwrap()).unwrap();
        };

        write("owner/repo", "keccak");
        assert!(m.read_state().is_some(), "matching source should be adopted");
        write("someone/else", "keccak");
        assert!(m.read_state().is_none(), "a different repo must not be adopted");
        write("owner/repo", "main");
        assert!(m.read_state().is_none(), "a different branch must not be adopted");
    }

    #[test]
    fn short_hashes_are_truncated_without_panicking_on_stubs() {
        assert_eq!(short(&"a".repeat(64)), "a".repeat(12));
        assert_eq!(short("abc"), "abc");
        assert_eq!(short(""), "");
    }
}
