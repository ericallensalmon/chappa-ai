//! Content hashing for the project-file trust gate.
//!
//! The trust gate compares a hash of the CURRENT `<dir>/chappa.yml` against the
//! hash recorded in projects.json. Any edit (even a whitespace change) changes
//! the hash, so the gate re-arms and the auto-start commands are re-confirmed
//! — the personal-scale stand-in for a per-command trust review.

use std::fs;
use std::path::Path;

/// BLAKE3 hex of `bytes` — the canonical project-file content hash.
pub fn content_hash(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// Hash of the file at `path`, or `None` when it cannot be read.
pub fn file_content_hash(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    Some(content_hash(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable_and_content_sensitive() {
        let a = content_hash(b"command: py app/summarize.py\n");
        let b = content_hash(b"command: py app/summarize.py\n");
        let c = content_hash(b"command: py app/summarize.py\n ");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 64, "blake3 hex is 64 chars");
    }

    #[test]
    fn file_hash_matches_content_hash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(crate::yml::CHAPPA_YML_NAME);
        fs::write(&path, b"processes: {}").unwrap();
        assert_eq!(
            file_content_hash(&path),
            Some(content_hash(b"processes: {}"))
        );
        assert_eq!(file_content_hash(&dir.path().join("missing.yml")), None);
    }
}
