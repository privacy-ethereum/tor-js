//! Microdescriptor store: in-memory cache keyed by SHA-256 digest.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use tor_netdoc::doc::microdesc::{MdDigest, MicrodescReader};
use tor_netdoc::AllowAnnotations;

/// In-memory store of microdescriptors keyed by their SHA-256 digest.
pub struct MicrodescStore {
    /// Map from digest to raw microdescriptor text.
    entries: HashMap<MdDigest, String>,
}

impl MicrodescStore {
    /// Create an empty store.
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    /// Load cached microdescs from a file on disk (the concatenated microdescs file).
    /// Parses individual microdescs and indexes them by digest.
    /// Errors in individual microdescs are logged and skipped.
    pub fn load_from_file(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::info!("no existing microdescs file, starting with empty store");
                return Ok(Self::new());
            }
            Err(e) => return Err(e).context("reading microdescs file"),
        };

        let mut store = Self::new();
        let reader = MicrodescReader::new(&text, &AllowAnnotations::AnnotationsNotAllowed)
            .context("parsing microdescs file")?;
        for item in reader {
            match item {
                Ok(annotated) => {
                    let digest = *annotated.md().digest();
                    if let Some(raw) = annotated.within(&text) {
                        store.entries.insert(digest, raw.to_string());
                    }
                }
                Err(e) => {
                    tracing::warn!("skipping unparseable microdesc: {}", e);
                }
            }
        }
        tracing::info!("loaded {} microdescs from store", store.entries.len());
        Ok(store)
    }

    /// Return digests from `wanted` that are not in the store.
    pub fn missing(&self, wanted: &[MdDigest]) -> Vec<MdDigest> {
        wanted
            .iter()
            .filter(|d| !self.entries.contains_key(*d))
            .copied()
            .collect()
    }

    /// Ingest a concatenated microdesc response, adding new entries to the store.
    /// Returns the number of new microdescs added.
    pub fn ingest(&mut self, text: &str) -> usize {
        let reader = match MicrodescReader::new(text, &AllowAnnotations::AnnotationsNotAllowed) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("failed to parse microdesc response: {}", e);
                return 0;
            }
        };
        let mut added = 0;
        for item in reader {
            match item {
                Ok(annotated) => {
                    let digest = *annotated.md().digest();
                    if let Some(raw) = annotated.within(text) {
                        self.entries.entry(digest).or_insert_with(|| {
                            added += 1;
                            raw.to_string()
                        });
                    }
                }
                Err(e) => {
                    tracing::warn!("skipping unparseable microdesc in response: {}", e);
                }
            }
        }
        added
    }

    /// Retain only entries whose digest is in `wanted`, dropping the rest.
    pub fn retain(&mut self, wanted: &[MdDigest]) {
        let before = self.entries.len();
        let wanted_set: HashSet<&MdDigest> = wanted.iter().collect();
        self.entries.retain(|k, _| wanted_set.contains(k));
        let dropped = before - self.entries.len();
        if dropped > 0 {
            tracing::info!("dropped {} stale microdescs from store", dropped);
        }
    }

    /// Number of stored entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Serialize all stored microdescs as a concatenated blob.
    pub fn to_concatenated(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for text in self.entries.values() {
            out.extend_from_slice(text.as_bytes());
        }
        out
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use base64ct::Encoding as _;

    /// The smallest microdescriptor `MicrodescReader` accepts. `seed` varies the
    /// ntor key, which is what makes each one's digest distinct.
    fn md(seed: u8) -> String {
        let key = base64ct::Base64Unpadded::encode_string(&[seed; 32]);
        let id = base64ct::Base64Unpadded::encode_string(&[seed ^ 0xff; 32]);
        format!("onion-key\nntor-onion-key {key}\nid ed25519 {id}\n")
    }

    fn digest_of(text: &str) -> MdDigest {
        let reader =
            MicrodescReader::new(text, &AllowAnnotations::AnnotationsNotAllowed).unwrap();
        let annotated = reader.into_iter().next().unwrap().unwrap();
        *annotated.md().digest()
    }

    fn digests(seeds: &[u8]) -> Vec<MdDigest> {
        seeds.iter().map(|s| digest_of(&md(*s))).collect()
    }

    fn concat(seeds: &[u8]) -> String {
        seeds.iter().map(|s| md(*s)).collect()
    }

    #[test]
    fn ingest_counts_only_new_entries() {
        let mut store = MicrodescStore::new();
        assert_eq!(store.len(), 0);

        assert_eq!(store.ingest(&concat(&[1, 2, 3])), 3);
        assert_eq!(store.len(), 3);

        // A batch that overlaps what we already hold adds only the difference.
        assert_eq!(store.ingest(&concat(&[2, 3, 4])), 1);
        assert_eq!(store.len(), 4);

        // A repeat of the whole thing adds nothing.
        assert_eq!(store.ingest(&concat(&[1, 2, 3, 4])), 0);
        assert_eq!(store.len(), 4);
    }

    /// A duplicate inside one response must be counted once — the counter and
    /// the map have to agree, or the "added" figure in the logs misleads.
    #[test]
    fn a_duplicate_within_one_response_is_counted_once() {
        let mut store = MicrodescStore::new();
        assert_eq!(store.ingest(&concat(&[7, 7, 7])), 1);
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn unparseable_input_is_skipped_not_fatal() {
        let mut store = MicrodescStore::new();
        assert_eq!(store.ingest("this is not a microdesc\n"), 0);
        assert_eq!(store.len(), 0);

        // A bad entry between good ones does not cost us the good ones.
        let mixed = format!("{}onion-key\nntor-onion-key !!!\n{}", md(1), md(2));
        let added = store.ingest(&mixed);
        assert!(added >= 1, "valid entries around the bad one should survive");
        assert_eq!(store.len(), added);
    }

    #[test]
    fn missing_reports_exactly_what_is_absent() {
        let mut store = MicrodescStore::new();
        store.ingest(&concat(&[1, 2]));

        let wanted = digests(&[1, 2, 3, 4]);
        assert_eq!(store.missing(&wanted), digests(&[3, 4]));
        assert!(store.missing(&digests(&[1, 2])).is_empty());
        assert!(store.missing(&[]).is_empty());

        // An empty store wants everything.
        assert_eq!(MicrodescStore::new().missing(&wanted).len(), 4);
    }

    #[test]
    fn retain_drops_everything_outside_the_consensus() {
        let mut store = MicrodescStore::new();
        store.ingest(&concat(&[1, 2, 3, 4]));

        store.retain(&digests(&[2, 4]));
        assert_eq!(store.len(), 2);
        assert!(store.missing(&digests(&[2, 4])).is_empty());
        assert_eq!(store.missing(&digests(&[1, 3])).len(), 2);

        // Retaining against an empty consensus empties the store.
        store.retain(&[]);
        assert_eq!(store.len(), 0);
    }

    /// `retain` must not require the digest to be present: a consensus listing
    /// relays we have not fetched yet is the normal case.
    #[test]
    fn retain_tolerates_digests_the_store_does_not_have() {
        let mut store = MicrodescStore::new();
        store.ingest(&concat(&[1]));
        store.retain(&digests(&[1, 99]));
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn to_concatenated_round_trips_through_ingest() {
        let mut store = MicrodescStore::new();
        store.ingest(&concat(&[1, 2, 3]));
        let blob = store.to_concatenated();

        let mut restored = MicrodescStore::new();
        assert_eq!(restored.ingest(&String::from_utf8(blob).unwrap()), 3);
        assert_eq!(restored.len(), 3);
        assert!(restored.missing(&digests(&[1, 2, 3])).is_empty());
    }

    #[test]
    fn an_empty_store_serialises_to_nothing() {
        assert!(MicrodescStore::new().to_concatenated().is_empty());
    }

    #[test]
    fn load_from_file_round_trips_and_tolerates_a_missing_file() {
        let dir = crate::testutil::TempDir::new("microdesc");

        let store = MicrodescStore::load_from_file(&dir.join("absent")).unwrap();
        assert_eq!(store.len(), 0);

        let mut original = MicrodescStore::new();
        original.ingest(&concat(&[1, 2, 3]));
        let path = dir.join("microdescs.txt");
        std::fs::write(&path, original.to_concatenated()).unwrap();

        let loaded = MicrodescStore::load_from_file(&path).unwrap();
        assert_eq!(loaded.len(), 3);
        assert!(loaded.missing(&digests(&[1, 2, 3])).is_empty());
    }

    /// A corrupt cache file must degrade to "fetch everything", never abort
    /// startup.
    #[test]
    fn load_from_file_survives_a_corrupt_cache() {
        let dir = crate::testutil::TempDir::new("microdesc-corrupt");
        let path = dir.join("microdescs.txt");
        std::fs::write(&path, "garbage that is not a microdesc\n").unwrap();
        let store = MicrodescStore::load_from_file(&path).unwrap();
        assert_eq!(store.len(), 0);

        // Truncated mid-entry: the good prefix is kept, the partial tail dropped.
        let blob = concat(&[1, 2]);
        std::fs::write(&path, &blob[..blob.len() - 20]).unwrap();
        let store = MicrodescStore::load_from_file(&path).unwrap();
        assert_eq!(store.len(), 1);
        assert!(store.missing(&digests(&[1])).is_empty());
    }
}
