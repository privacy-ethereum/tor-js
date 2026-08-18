//! Configuration file handling.
//!
//! Config lives at `~/.config/tor-js-gateway/config.json5`.
//! Data lives at `~/.local/share/tor-js-gateway/`.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const APP_NAME: &str = "tor-js-gateway";

/// Resolved paths for config and data directories.
pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join(APP_NAME)
        .join("config.json5")
}

pub fn default_data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("~/.local/share"))
        .join(APP_NAME)
}

fn default_key_file() -> PathBuf {
    default_data_dir().join("kps.key")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Directory for cached consensus data and bootstrap archives
    pub data_dir: PathBuf,

    /// UDP port the KPS listener binds (serves both QUIC and WebRTC)
    pub kps_port: u16,

    /// Path to the persistent KPS identity key (PEM; created by `init`)
    pub kps_key_file: PathBuf,

    /// GitHub repository mirrored into the hash-addressed object store, as
    /// `owner/repo`. Empty disables the worker-bundles capability; there is
    /// deliberately no default, so no operator mirrors a repository they did
    /// not name
    pub keccak_repo: String,

    /// Branch of `keccak_repo` to mirror. Required whenever `keccak_repo` is
    /// set — never defaulted, for the same reason
    pub keccak_branch: String,

    /// Seconds between automatic mirror polls
    pub keccak_poll_interval: u64,

    /// Minimum seconds between client-triggered syncs (`POST /keccak/sync`);
    /// a trigger inside the window is refused
    pub keccak_manual_sync_min_interval: u64,

    /// IP addresses to advertise in metadata.json (the UDP port and certhash
    /// are appended automatically); empty auto-detects
    pub advertised_addresses: Vec<String>,

    /// Max concurrent CONNECT tunnels
    pub tunnel_max: usize,

    /// Max concurrent CONNECT tunnels per client IP (also the per-KPS-connection cap)
    pub tunnel_per_ip: usize,

    /// Tunnel idle timeout in seconds
    pub tunnel_idle_timeout: u64,

    /// Tunnel max lifetime in seconds
    pub tunnel_max_lifetime: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            kps_port: 12298,
            kps_key_file: default_key_file(),
            keccak_repo: String::new(),
            keccak_branch: String::new(),
            keccak_poll_interval: 86_400,
            keccak_manual_sync_min_interval: 1_800,
            advertised_addresses: Vec::new(),
            tunnel_max: 8192,
            tunnel_per_ip: 16,
            tunnel_idle_timeout: 300,
            tunnel_max_lifetime: 3600,
        }
    }
}

/// Where the hash-addressed objects come from, once the config has been
/// validated. There is no "a directory the operator fills by hand" variant:
/// the mirror owns `<data_dir>/keccak` and prunes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeccakSource {
    /// `keccak_repo`/`keccak_branch` unset — the worker-bundles capability is
    /// not advertised and `/keccak/*` answers 404.
    Disabled,
    Mirror { repo: String, branch: String },
}

/// `owner/repo`: exactly one slash, both halves non-empty, and only the
/// characters GitHub actually allows in each. Rejecting the rest here keeps
/// anything surprising out of the URL paths built from it.
fn valid_repo(repo: &str) -> bool {
    let Some((owner, name)) = repo.split_once('/') else {
        return false;
    };
    let ok = |s: &str, extra: &str| {
        !s.is_empty()
            && s.len() <= 100
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || extra.as_bytes().contains(&b))
    };
    ok(owner, "-_") && ok(name, "-_.")
}

/// A branch name that is safe to splice into a URL path. Deliberately stricter
/// than git's own rules (which permit `/`, `?`, `#`, `%`, …): those would let a
/// branch name reach past the endpoint it is interpolated into.
fn valid_branch(branch: &str) -> bool {
    !branch.is_empty()
        && branch.len() <= 255
        && !branch.starts_with(['-', '.'])
        && !branch.ends_with('.')
        && branch
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./".contains(&b))
        && !branch.contains("..")
        && !branch.contains("//")
}

impl Config {
    /// Where `/keccak/*` objects come from, or an error when the two mirror
    /// fields disagree about whether the capability is wanted.
    ///
    /// Both unset is a valid, silent "off". Exactly one set is always a
    /// mistake, and is refused rather than half-honored — the alternative is a
    /// gateway that mirrors a defaulted branch nobody chose.
    pub fn keccak_source(&self) -> Result<KeccakSource> {
        let repo = self.keccak_repo.trim();
        let branch = self.keccak_branch.trim();
        match (repo.is_empty(), branch.is_empty()) {
            (true, true) => return Ok(KeccakSource::Disabled),
            (true, false) => anyhow::bail!(
                "keccak_branch is set to {branch:?} but keccak_repo is empty — \
                 set both (e.g. \"ethereum/tor-js\" + \"keccak\") to serve \
                 worker bundles, or clear both to disable the capability"
            ),
            (false, true) => anyhow::bail!(
                "keccak_repo is set to {repo:?} but keccak_branch is empty — \
                 there is no default branch on purpose; name the branch to mirror"
            ),
            (false, false) => {}
        }
        if !valid_repo(repo) {
            anyhow::bail!("keccak_repo {repo:?} is not a GitHub \"owner/repo\"");
        }
        if !valid_branch(branch) {
            anyhow::bail!("keccak_branch {branch:?} is not a usable branch name");
        }
        Ok(KeccakSource::Mirror {
            repo: repo.to_string(),
            branch: branch.to_string(),
        })
    }

    /// Root of the mirrored object tree: `<data_dir>/keccak/<hh>/<rest>`, the
    /// same sharded layout the `/keccak/` route exposes. Not configurable —
    /// the mirror deletes files here, so it must be a directory it owns.
    pub fn keccak_dir(&self) -> PathBuf {
        self.data_dir.join("keccak")
    }

    /// The tunnel limits this config asks for. `tunnel_per_ip` deliberately
    /// caps both the per-IP and the per-KPS-connection budget, so one client
    /// cannot multiply its allowance by opening more connections.
    pub fn tunnel_limits(&self) -> crate::tunnel::TunnelLimits {
        crate::tunnel::TunnelLimits {
            max_tunnels: self.tunnel_max,
            per_ip: self.tunnel_per_ip,
            per_conn: self.tunnel_per_ip,
            idle_timeout: std::time::Duration::from_secs(self.tunnel_idle_timeout),
            max_lifetime: std::time::Duration::from_secs(self.tunnel_max_lifetime),
        }
    }

    /// Serialize to pretty JSON5 with comments.
    pub fn to_json5_with_comments() -> String {
        let cfg = Self::default();
        format!(
            r#"{{
  // Directory for cached consensus data and bootstrap archives
  "data_dir": {},

  // UDP port for the KPS listener — both QUIC and WebRTC ride this one port
  "kps_port": {},

  // Persistent KPS identity key (PEM). Generated by `init`; the certhash in
  // the gateway's published address is derived from it, so keep it stable.
  "kps_key_file": {},

  // ---- Worker bundles (/keccak/{{hash[0..2]}}/{{hash[2..]}}) ----
  //
  // Objects are mirrored from a branch of a GitHub repository into
  // <data_dir>/keccak, which the gateway owns: it adds objects the branch
  // gained and deletes ones the branch dropped, so do not put files there by
  // hand. Every file in the branch must be named for its own content —
  // <hh>/<rest>, the 64 lowercase hex chars of keccak256(bytes) split after
  // the first byte, no extension. Anything else in the tree is ignored.
  //
  // BOTH of these must be set to serve worker bundles, and neither has a
  // default: leaving them empty disables the capability rather than quietly
  // mirroring somebody else's repository. Setting only one is an error.
  //
  //   "keccak_repo": "ethereum/tor-js",
  //   "keccak_branch": "keccak",
  "keccak_repo": "",
  "keccak_branch": "",

  // Seconds between automatic mirror polls (86400 = once a day)
  "keccak_poll_interval": {},

  // Minimum seconds between client-triggered syncs (POST /keccak/sync).
  // A trigger inside the window is refused with 429 + Retry-After; the
  // automatic poll is unaffected by it.
  "keccak_manual_sync_min_interval": {},

  // IP addresses to advertise in metadata.json (the UDP port and certhash are
  // appended automatically). Empty: auto-detect from the default route.
  // Operators behind NAT must set this to their public IP(s).
  "advertised_addresses": [],

  // Max concurrent CONNECT tunnels
  "tunnel_max": {},

  // Max concurrent CONNECT tunnels per client IP (also caps tunnels per KPS connection)
  "tunnel_per_ip": {},

  // Tunnel idle timeout in seconds
  "tunnel_idle_timeout": {},

  // Tunnel max lifetime in seconds
  "tunnel_max_lifetime": {},
}}"#,
            serde_json::to_string(&cfg.data_dir).unwrap(),
            cfg.kps_port,
            serde_json::to_string(&cfg.kps_key_file).unwrap(),
            cfg.keccak_poll_interval,
            cfg.keccak_manual_sync_min_interval,
            cfg.tunnel_max,
            cfg.tunnel_per_ip,
            cfg.tunnel_idle_timeout,
            cfg.tunnel_max_lifetime,
        )
    }

    /// Load config from the given path.
    pub fn load(path: &PathBuf) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config at {}\n\nRun `tor-js-gateway init` to create a default config.", path.display()))?;
        let cfg: Config = json5::from_str(&text)
            .map_err(|e| match text.contains("keccak_dir") {
                // `deny_unknown_fields` catches the old key, but "unknown field
                // keccak_dir" does not tell an operator what to do about it.
                true => anyhow::anyhow!(
                    "`keccak_dir` is gone: objects are now mirrored from a GitHub branch \
                     into <data_dir>/keccak.\nRemove the field and set `keccak_repo` \
                     (\"owner/repo\") and `keccak_branch` instead — see \
                     `tor-js-gateway show-default-config`.\n\n({e})"
                ),
                false => anyhow::anyhow!(e),
            })
            .with_context(|| format!("parsing {}", path.display()))?;
        // Fail at load, not at first use: a gateway that starts and only then
        // reveals it cannot serve bundles is worse than one that refuses to.
        cfg.keccak_source()
            .with_context(|| format!("in {}", path.display()))?;
        Ok(cfg)
    }

    /// Create the default config file. Errors if it already exists.
    pub fn init(path: &PathBuf) -> Result<()> {
        if path.exists() {
            anyhow::bail!("config already exists at {}", path.display());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let content = Self::to_json5_with_comments();
        std::fs::write(&path, &content)
            .with_context(|| format!("writing {}", path.display()))?;
        println!("Created config at {}", path.display());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    fn as_value(cfg: &Config) -> serde_json::Value {
        serde_json::to_value(cfg).unwrap()
    }

    /// The commented template is hand-written, so it can drift from the struct.
    /// `deny_unknown_fields` turns either direction of drift into a parse error:
    /// a field added to the struct but not the template is "missing field", and
    /// one removed from the struct but left in the template is "unknown field".
    #[test]
    fn the_commented_template_parses_back_to_the_defaults() {
        let text = Config::to_json5_with_comments();
        let parsed: Config = json5::from_str(&text)
            .unwrap_or_else(|e| panic!("template does not match Config: {e}\n\n{text}"));
        assert_eq!(as_value(&parsed), as_value(&Config::default()));
    }

    #[test]
    fn json5_comments_and_trailing_commas_are_accepted() {
        // Both appear in the template, so the parser has to tolerate them.
        let text = r#"{
  // a comment
  "data_dir": "/tmp/d",
  "kps_port": 1234,
  "kps_key_file": "/tmp/k",
  "keccak_repo": "",
  "keccak_branch": "",
  "keccak_poll_interval": 86400,
  "keccak_manual_sync_min_interval": 1800,
  "advertised_addresses": ["1.2.3.4"],
  "tunnel_max": 10,
  "tunnel_per_ip": 2,
  "tunnel_idle_timeout": 30,
  "tunnel_max_lifetime": 60, /* trailing comma next */
}"#;
        let cfg: Config = json5::from_str(text).unwrap();
        assert_eq!(cfg.kps_port, 1234);
        assert_eq!(cfg.advertised_addresses, vec!["1.2.3.4".to_string()]);
    }

    #[test]
    fn an_unknown_field_is_rejected_rather_than_ignored() {
        // A typo in a config key must not silently fall back to the default.
        let text = Config::to_json5_with_comments()
            .replace("\"kps_port\"", "\"kps_prot\"");
        let err = json5::from_str::<Config>(&text).unwrap_err();
        assert!(err.to_string().contains("kps_prot"), "unexpected error: {err}");
    }

    #[test]
    fn tunnel_limits_cap_per_connection_with_the_per_ip_budget() {
        let cfg = Config {
            tunnel_max: 100,
            tunnel_per_ip: 7,
            tunnel_idle_timeout: 11,
            tunnel_max_lifetime: 22,
            ..Config::default()
        };
        let limits = cfg.tunnel_limits();
        assert_eq!(limits.max_tunnels, 100);
        assert_eq!(limits.per_ip, 7);
        assert_eq!(
            limits.per_conn, 7,
            "otherwise a client lifts its own cap by opening more connections"
        );
        assert_eq!(limits.idle_timeout, std::time::Duration::from_secs(11));
        assert_eq!(limits.max_lifetime, std::time::Duration::from_secs(22));
    }

    /// The defaults the struct declares and the ones the template ships must be
    /// the same numbers an operator sees; this pins them so a change is visible.
    #[test]
    fn default_limits_match_the_tunnel_defaults() {
        let from_config = Config::default().tunnel_limits();
        let from_tunnel = crate::tunnel::TunnelLimits::default();
        assert_eq!(from_config.max_tunnels, from_tunnel.max_tunnels);
        assert_eq!(from_config.per_ip, from_tunnel.per_ip);
        assert_eq!(from_config.per_conn, from_tunnel.per_conn);
        assert_eq!(from_config.idle_timeout, from_tunnel.idle_timeout);
        assert_eq!(from_config.max_lifetime, from_tunnel.max_lifetime);
    }

    #[test]
    fn init_writes_a_loadable_config_and_refuses_to_clobber_it() {
        let dir = TempDir::new("config");
        let path = dir.join("nested").join("config.json5");

        Config::init(&path).unwrap();
        assert!(path.exists(), "init creates missing parent directories");
        let loaded = Config::load(&path).unwrap();
        assert_eq!(as_value(&loaded), as_value(&Config::default()));

        let err = Config::init(&path).unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
    }

    // ---- keccak source resolution ---------------------------------------

    fn source_of(repo: &str, branch: &str) -> Result<KeccakSource> {
        Config {
            keccak_repo: repo.to_string(),
            keccak_branch: branch.to_string(),
            ..Config::default()
        }
        .keccak_source()
    }

    #[test]
    fn both_fields_empty_disables_the_capability() {
        assert_eq!(source_of("", "").unwrap(), KeccakSource::Disabled);
        // Whitespace is not a configuration value.
        assert_eq!(source_of("  ", "\t").unwrap(), KeccakSource::Disabled);
    }

    #[test]
    fn both_fields_set_resolves_to_a_mirror() {
        assert_eq!(
            source_of("ethereum/tor-js", "keccak").unwrap(),
            KeccakSource::Mirror {
                repo: "ethereum/tor-js".into(),
                branch: "keccak".into(),
            }
        );
    }

    /// Half-configured is always a mistake. Defaulting the other half is the
    /// specific failure mode to avoid: it would mirror a repository or branch
    /// the operator never named.
    #[test]
    fn exactly_one_field_set_is_an_error_naming_the_missing_one() {
        let err = source_of("ethereum/tor-js", "").unwrap_err().to_string();
        assert!(err.contains("keccak_branch"), "{err}");
        assert!(err.contains("no default"), "{err}");

        let err = source_of("", "keccak").unwrap_err().to_string();
        assert!(err.contains("keccak_repo"), "{err}");
    }

    #[test]
    fn a_malformed_repo_is_rejected() {
        for repo in [
            "tor-js",                       // no owner
            "ethereum/tor-js/x",    // too many segments
            "/tor-js",                      // empty owner
            "ethereum/",            // empty name
            "privacy ethereum/tor-js",      // space
            "ethereum/../../etc",   // traversal
            "https://github.com/a/b",       // a URL, not owner/repo
        ] {
            assert!(source_of(repo, "keccak").is_err(), "accepted repo {repo:?}");
        }
    }

    /// Branch names are spliced into URL paths, so the rules here are tighter
    /// than git's: git would allow every one of these.
    #[test]
    fn a_branch_name_that_could_escape_its_url_path_is_rejected() {
        for branch in [
            "..",
            "a/../../b",
            "keccak?ref=main",
            "keccak#frag",
            "keccak%2F",
            "-keccak",
            ".keccak",
            "keccak.",
            "a//b",
            "with space",
        ] {
            assert!(
                source_of("ethereum/tor-js", branch).is_err(),
                "accepted branch {branch:?}",
            );
        }
        // Slashes inside a name are normal and stay allowed.
        assert!(source_of("ethereum/tor-js", "feature/keccak").is_ok());
    }

    #[test]
    fn load_rejects_a_half_configured_mirror() {
        let dir = TempDir::new("config-half");
        let path = dir.join("config.json5");
        let text = Config::to_json5_with_comments()
            .replace(r#""keccak_repo": """#, r#""keccak_repo": "ethereum/tor-js""#);
        std::fs::write(&path, text).unwrap();
        let msg = format!("{:#}", Config::load(&path).unwrap_err());
        assert!(msg.contains("keccak_branch"), "{msg}");
    }

    /// The old key names a directory the operator filled by hand; the mirror
    /// now owns that directory. Say so, rather than "unknown field".
    #[test]
    fn load_explains_the_retired_keccak_dir_field() {
        let dir = TempDir::new("config-legacy");
        let path = dir.join("config.json5");
        let text = Config::to_json5_with_comments()
            .replace(r#""keccak_repo": """#, r#""keccak_dir": "/srv/bundles""#);
        std::fs::write(&path, text).unwrap();
        let msg = format!("{:#}", Config::load(&path).unwrap_err());
        assert!(msg.contains("keccak_repo"), "{msg}");
        assert!(msg.contains("mirrored"), "{msg}");
    }

    #[test]
    fn the_object_dir_lives_under_the_data_dir() {
        let cfg = Config {
            data_dir: PathBuf::from("/var/lib/gw"),
            ..Config::default()
        };
        assert_eq!(cfg.keccak_dir(), PathBuf::from("/var/lib/gw/keccak"));
    }

    #[test]
    fn load_errors_point_at_init_when_the_file_is_missing() {
        let dir = TempDir::new("config-missing");
        let err = Config::load(&dir.join("config.json5")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("tor-js-gateway init"), "unhelpful error: {msg}");
    }

    #[test]
    fn load_reports_the_path_on_a_parse_error() {
        let dir = TempDir::new("config-bad");
        let path = dir.join("config.json5");
        std::fs::write(&path, "{ this is not json5").unwrap();
        let err = Config::load(&path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("config.json5"), "unhelpful error: {msg}");
    }
}
