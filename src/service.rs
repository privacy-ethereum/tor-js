//! Systemd user service management.
//!
//! `install` writes a unit file, reloads systemd, and enables+starts the service.
//! `uninstall` stops, disables, removes the unit file, and reloads.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

const SERVICE_NAME: &str = "tor-js-gateway";

fn unit_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("systemd/user")
}

fn unit_path() -> PathBuf {
    unit_dir().join(format!("{}.service", SERVICE_NAME))
}

fn generate_unit(binary: &Path, config: &Path) -> String {
    format!(
        r#"[Unit]
Description=tor-js-gateway — KPS gateway for tor-js browser clients
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart={binary} --config {config} run
Restart=on-failure
RestartSec=10

[Install]
WantedBy=default.target
"#,
        binary = binary.display(),
        config = config.display(),
    )
}

fn systemctl(args: &[&str]) -> Result<()> {
    let output = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .context("running systemctl")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("systemctl --user {} failed: {}", args.join(" "), stderr.trim());
    }
    Ok(())
}

pub fn install(config_path: &PathBuf) -> Result<()> {
    // Resolve the absolute path to the running binary
    let binary = std::env::current_exe().context("resolving binary path")?;
    let config = std::fs::canonicalize(config_path)
        .with_context(|| format!("resolving config path {:?}", config_path))?;

    // Ensure config exists
    if !config.exists() {
        anyhow::bail!(
            "config not found at {}\nRun `tor-js-gateway init` first.",
            config.display()
        );
    }

    // Write unit file
    let dir = unit_dir();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;

    let path = unit_path();
    let unit = generate_unit(&binary, &config);
    std::fs::write(&path, &unit)
        .with_context(|| format!("writing {}", path.display()))?;
    println!("Wrote {}", path.display());

    // Reload, enable, start
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", SERVICE_NAME])?;
    systemctl(&["start", SERVICE_NAME])?;

    println!("Service {} installed and started.", SERVICE_NAME);
    println!();
    println!("  systemctl --user status {}", SERVICE_NAME);
    println!("  journalctl --user -u {} -f", SERVICE_NAME);
    println!("  systemctl --user restart {}", SERVICE_NAME);

    Ok(())
}

pub fn uninstall() -> Result<()> {
    let path = unit_path();

    // Stop and disable (ignore errors — service may not be running)
    let _ = systemctl(&["stop", SERVICE_NAME]);
    let _ = systemctl(&["disable", SERVICE_NAME]);

    // Remove unit file
    if path.exists() {
        std::fs::remove_file(&path)
            .with_context(|| format!("removing {}", path.display()))?;
        println!("Removed {}", path.display());
    } else {
        println!("No unit file at {}", path.display());
    }

    systemctl(&["daemon-reload"])?;

    println!("Service {} uninstalled.", SERVICE_NAME);
    Ok(())
}
