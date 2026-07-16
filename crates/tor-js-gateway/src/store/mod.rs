//! Persistent stores for consensus, authority certs, and microdescriptors.

mod authcert;
mod consensus;
mod microdesc;

pub use authcert::AuthCertStore;
pub use consensus::ConsensusStore;
pub use microdesc::MicrodescStore;

use std::path::Path;
use std::time::SystemTime;

use anyhow::Result;

/// All stores needed for a sync cycle, loaded from a single output directory.
pub struct Stores {
    pub consensus: ConsensusStore,
    pub certs: AuthCertStore,
    pub microdescs: MicrodescStore,
}

impl Stores {
    /// Load all stores from the given output directory.
    pub fn load(dir: &Path, now: &SystemTime) -> Result<Self> {
        Ok(Self {
            consensus: ConsensusStore::load_from_file(&dir.join("consensus-microdesc.txt")),
            certs: AuthCertStore::load_from_file(&dir.join("authority-certs.txt"), now),
            microdescs: MicrodescStore::load_from_file(&dir.join("microdescs.txt"))?,
        })
    }
}
