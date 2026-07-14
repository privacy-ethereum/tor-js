mod config;
mod dir;
mod kps_server;
mod routes;
mod service;
mod store;
mod sync;
mod tunnel;

use std::collections::HashSet;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use arti_client::{TorClient, TorClientConfig};

#[derive(Parser)]
#[command(name = "tor-js-gateway")]
#[command(about = "KPS gateway for tor-js — bootstrap, TCP relay via CONNECT, worker bundles")]
struct Cli {
    /// Path to config file
    #[arg(short, long, default_value_os_t = config::config_path())]
    config: PathBuf,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the gateway server (default when no subcommand given)
    Run {
        /// Exit after the first successful sync instead of looping
        #[arg(long)]
        once: bool,

        /// Serve only from cached data: skip the Tor client and consensus sync
        /// (implies ignoring --once)
        #[arg(long)]
        no_sync: bool,
    },
    /// Create a default config file and the KPS identity key
    Init,
    /// Print the current config from disk
    ShowConfig,
    /// Print the hardcoded default config
    ShowDefaultConfig,
    /// Install and start a systemd user service
    Install,
    /// Stop and remove the systemd user service
    Uninstall,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command.unwrap_or(Command::Run { once: false, no_sync: false }) {
        Command::Init => init(&cli.config),
        Command::ShowConfig => {
            let cfg = config::Config::load(&cli.config)?;
            println!("{}", json5::to_string(&cfg)?);
            Ok(())
        }
        Command::ShowDefaultConfig => {
            println!("{}", config::Config::to_json5_with_comments());
            Ok(())
        }
        Command::Run { once, no_sync } => run(&cli.config, once, no_sync).await,
        Command::Install => service::install(&cli.config),
        Command::Uninstall => service::uninstall(),
    }
}

/// `init`: write the default config and generate the KPS identity key, so the
/// certhash (the stable part of the gateway's published address) exists before
/// the first run.
fn init(config_path: &PathBuf) -> Result<()> {
    config::Config::init(config_path)?;
    let cfg = config::Config::load(config_path)?;
    if let Some(parent) = cfg.kps_key_file.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let identity = kps::Identity::load_or_create(&cfg.kps_key_file)
        .with_context(|| format!("generating KPS key at {}", cfg.kps_key_file.display()))?;
    println!("Created KPS key at {}", cfg.kps_key_file.display());
    println!("Certhash: {}", identity.certhash);
    println!("The full dialable address(es) are printed at startup.");
    Ok(())
}

/// Check if the system has IPv6 internet connectivity by looking for a default route.
fn detect_ipv6() -> bool {
    let Ok(output) = std::process::Command::new("ip")
        .args(["-6", "route", "show", "default"])
        .output()
    else {
        return false;
    };
    let text = String::from_utf8_lossy(&output.stdout);
    text.contains("default")
}

/// Best-effort detection of the local IP used for outbound traffic in one
/// address family (a UDP `connect()` picks the route; no packets are sent).
/// Behind NAT this yields a private address — operators must then set
/// `advertised_addresses`.
fn detect_source_ip(v6: bool) -> Option<IpAddr> {
    let (bind, probe) = if v6 {
        ("[::]:0", "[2001:4860:4860::8888]:53")
    } else {
        ("0.0.0.0:0", "8.8.8.8:53")
    };
    let sock = std::net::UdpSocket::bind(bind).ok()?;
    sock.connect(probe).ok()?;
    let ip = sock.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

/// The addresses to publish: explicit config wins; otherwise derive from the
/// detected outbound IPs (v4 and, when connected, v6) with the listener's
/// port and certhash.
fn advertised_addresses(
    cfg: &config::Config,
    listener: &kps::Listener,
    has_ipv6: bool,
) -> Vec<String> {
    if !cfg.advertised_addresses.is_empty() {
        return cfg
            .advertised_addresses
            .iter()
            .map(|ip| listener.address(ip))
            .collect();
    }
    let mut out = Vec::new();
    if let Some(ip) = detect_source_ip(false) {
        out.push(listener.address(&ip.to_string()));
    }
    if has_ipv6 {
        if let Some(ip) = detect_source_ip(true) {
            out.push(listener.address(&ip.to_string()));
        }
    }
    if out.is_empty() {
        tracing::warn!("could not detect a public IP; set advertised_addresses in the config");
        out.push(listener.address(""));
    }
    out
}

/// Pre-populate the relay allowlist from a cached consensus so CONNECT works
/// immediately, before the first sync completes.
fn preload_allowlist(data_dir: &std::path::Path, allowlist: &tunnel::RelayAllowlist) {
    let consensus_path = data_dir.join("consensus-microdesc.txt");
    let Ok(text) = std::fs::read_to_string(&consensus_path) else {
        return;
    };
    let mut addrs = HashSet::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("r ") {
            // r <nickname> <identity> <date> <time> <ip> <orport> <dirport>
            let parts: Vec<&str> = rest.split_whitespace().collect();
            if parts.len() >= 6 {
                if let (Ok(ip), Ok(port)) =
                    (parts[4].parse::<std::net::IpAddr>(), parts[5].parse::<u16>())
                {
                    if port != 0 {
                        addrs.insert(std::net::SocketAddr::new(ip, port));
                    }
                }
                // DirPort
                if parts.len() >= 7 {
                    if let Ok(dport) = parts[6].parse::<u16>() {
                        if dport != 0 {
                            if let Ok(ip) = parts[4].parse::<std::net::IpAddr>() {
                                addrs.insert(std::net::SocketAddr::new(ip, dport));
                            }
                        }
                    }
                }
            }
        }
    }
    if !addrs.is_empty() {
        tracing::info!(
            "pre-populated relay allowlist with {} addresses from cached consensus",
            addrs.len()
        );
        *allowlist.write().unwrap() = addrs;
    }
}

async fn run(config_path: &PathBuf, once: bool, no_sync: bool) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cfg = config::Config::load(config_path)?;
    std::fs::create_dir_all(&cfg.data_dir)
        .with_context(|| format!("creating data dir {:?}", cfg.data_dir))?;

    let relay_allowlist: tunnel::RelayAllowlist = Arc::new(RwLock::new(HashSet::new()));
    preload_allowlist(&cfg.data_dir, &relay_allowlist);

    let limits = tunnel::TunnelLimits {
        max_tunnels: cfg.tunnel_max,
        per_ip: cfg.tunnel_per_ip,
        per_conn: cfg.tunnel_per_ip,
        idle_timeout: Duration::from_secs(cfg.tunnel_idle_timeout),
        max_lifetime: Duration::from_secs(cfg.tunnel_max_lifetime),
    };

    // Detect IPv6 connectivity by checking for a default route.
    let has_ipv6 = detect_ipv6();
    if has_ipv6 {
        tracing::info!("IPv6 connectivity detected");
    } else {
        tracing::info!("no IPv6 connectivity — IPv6 relay targets will be rejected");
    }

    // Verify the hash-addressed object tree (empty config disables the capability).
    let worker_bundles_enabled = !cfg.keccak_dir.as_os_str().is_empty();
    let verified_bundles = if worker_bundles_enabled {
        routes::scan_keccak_dir(&cfg.keccak_dir)?
    } else {
        std::collections::HashSet::new()
    };

    // Start the KPS listener: one UDP port serving both QUIC and WebRTC.
    let listener = kps::listen(
        &format!(":{}", cfg.kps_port),
        kps::ListenOptions {
            identity: None,
            key_file: Some(cfg.kps_key_file.clone()),
        },
    )
    .await
    .with_context(|| format!("starting KPS listener on UDP port {}", cfg.kps_port))?;

    let addresses = advertised_addresses(&cfg, &listener, has_ipv6);
    tracing::info!("KPS listener on UDP port {}", listener.port());
    tracing::info!("┌─ publish this address — clients dial it directly (there is no DNS):");
    for addr in &addresses {
        tracing::info!("│    {}", addr);
    }
    tracing::info!("└─ an IP change changes the address; advertise both v4 and v6 where possible");

    let gateway = Arc::new(routes::Gateway {
        data_dir: cfg.data_dir.clone(),
        relay_allowlist: relay_allowlist.clone(),
        tracker: tunnel::ConnectionTracker::new(),
        limits,
        has_ipv6,
        keccak_dir: cfg.keccak_dir.clone(),
        verified_bundles,
        metadata_json: routes::build_metadata(&addresses, worker_bundles_enabled),
    });
    let router = routes::build_router(gateway.clone());
    tokio::spawn(kps_server::run(listener, gateway, router));

    if no_sync {
        tracing::info!("--no-sync: serving cached data only, consensus sync disabled");
        return futures::future::pending::<Result<()>>().await;
    }

    // Load stores from previous run
    let mut stores = store::Stores::load(&cfg.data_dir, &SystemTime::now())?;

    tracing::info!("bootstrapping TorClient...");
    let tor_config = TorClientConfig::default();
    let client = TorClient::create_bootstrapped(tor_config)
        .await
        .context("bootstrapping TorClient")?;
    tracing::info!("TorClient bootstrapped");

    loop {
        match sync::sync_once(&client, &cfg.data_dir, &mut stores, &relay_allowlist).await {
            Ok(Some(lifetime)) => {
                if once {
                    return Ok(());
                }
                let delay =
                    sync::relay_sync_delay(lifetime.fresh_until(), lifetime.valid_until());
                tracing::info!(
                    "next sync in {} (at ~{})",
                    humantime::format_duration(delay),
                    humantime::format_rfc3339(SystemTime::now() + delay),
                );
                tokio::time::sleep(delay).await;
            }
            Ok(None) => {
                let retry = Duration::from_secs(60);
                tracing::info!("retrying in {}", humantime::format_duration(retry));
                tokio::time::sleep(retry).await;
            }
            Err(e) => {
                tracing::error!("sync failed: {:#}", e);
                let retry = Duration::from_secs(60);
                tracing::info!("retrying in {}", humantime::format_duration(retry));
                tokio::time::sleep(retry).await;
            }
        }
    }
}
