use std::process::Command;

fn main() {
    // Version from the npm package.json at the repo root (one level up from
    // this crate, which lives in rust/).
    let pkg_json = std::fs::read_to_string("../package.json").unwrap_or_default();
    let version = pkg_json
        .lines()
        .find_map(|line| {
            let line = line.trim();
            if line.starts_with("\"version\"") {
                // Extract value from: "version": "0.3.0",
                line.split('"').nth(3)
            } else {
                None
            }
        })
        .unwrap_or("unknown");
    // Verify Cargo.toml version matches package.json
    let cargo_version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    assert_eq!(
        cargo_version, version,
        "Version mismatch: Cargo.toml has {cargo_version} but ../package.json has {version}"
    );

    // Build profile
    let profile = std::env::var("PROFILE").unwrap_or_default();
    let dev = if profile != "release" { " (dev)" } else { "" };

    println!("cargo:rustc-env=TOR_JS_VERSION={version}{dev}");

    // Git short SHA
    let sha = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".into());

    // Dirty check
    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    let git_info = if dirty {
        let ts = Command::new("date")
            .args(["-u", "+%Y-%m-%dT%H:%M:%S.%3NZ"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "unknown".into());
        format!("{sha} dirty {ts}")
    } else {
        sha
    };
    println!("cargo:rustc-env=TOR_JS_GIT_INFO={git_info}");

    // Always rerun: git dirty status and timestamps are volatile and can't be
    // tracked by specific files. Pointing at a non-existent path forces Cargo
    // to always rerun (it sees the dep as perpetually missing/changed).
    // Note: Just means we rebuild this build.rs - not a full rebuild.
    println!("cargo:rerun-if-changed=.force-rebuild");
}
