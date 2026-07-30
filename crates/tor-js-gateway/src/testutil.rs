//! Test-only helpers shared across the unit tests.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A fresh empty directory that removes itself when the returned guard drops.
///
/// The counter keeps concurrently running tests (cargo runs them on a thread
/// pool) from colliding without needing a `tempfile` dependency.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "tor-js-gw-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::remove_dir_all(&path).ok();
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    pub fn path(&self) -> &std::path::Path {
        &self.0
    }

    pub fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
    }
}
