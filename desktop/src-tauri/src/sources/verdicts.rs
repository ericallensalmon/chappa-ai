//! `data-safety.json` in the app-config dir: each source's last data-safety
//! verdict, kept so a restart does not open a window in which agents may
//! start next to data a stack said may be real.
//!
//! ```json
//! {
//!   "verdicts": {
//!     "<source registry id>": {
//!       "block": { "instance": "app/0", "state": "not-clean", "blocksAgents": true, … },
//!       "sourceLabel": "Stack app", "manifestId": "stack:app",
//!       "fallback": "/srv/example/stack", "savedMs": 1794657600000
//!     }
//!   }
//! }
//! ```
//!
//! - **Every answer is kept, clean or not.** A fresh Environment answer
//!   replaces the record (`block: null` when the answer carried none); the
//!   file is rewritten only when what it says changed, not on every poll.
//! - **At startup, before a source answers**, its record stands in for the
//!   answer: a saved verdict that blocks is enforced and reads as the last
//!   known one; a saved one that does not block allows.
//! - **A source removed in Settings** takes its record with it.
//!
//! The keys are the opaque registry ids (a hash of the trust key), which
//! are the same on every run. A missing, unreadable or garbled file is an
//! empty store: what it held comes back with the next answers, and a stack
//! source with no record blocks its folder until it answers anyway.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::data_safety::WireBlock;

pub const FILE_NAME: &str = "data-safety.json";

/// One source's last verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedVerdict {
    /// The block as last sent; `None` when the last answer carried none.
    pub block: Option<WireBlock>,
    /// The source's label when it answered (shown before it is back).
    pub source_label: String,
    pub manifest_id: String,
    /// The folder the block covers when it names neither items nor a
    /// project: the folder of the project that launched the source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<PathBuf>,
    pub saved_ms: u64,
}

impl SavedVerdict {
    /// The same verdict, saved at another time.
    fn same_as(&self, other: &SavedVerdict) -> bool {
        self.block == other.block
            && self.source_label == other.source_label
            && self.manifest_id == other.manifest_id
            && self.fallback == other.fallback
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct FileShape {
    #[serde(default)]
    verdicts: BTreeMap<String, SavedVerdict>,
}

#[derive(Debug)]
pub struct VerdictStore {
    path: PathBuf,
    data: FileShape,
}

impl VerdictStore {
    /// Missing / unreadable / unparseable → empty (logged when it existed).
    pub fn load(path: PathBuf) -> Self {
        let data = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str::<FileShape>(&text).unwrap_or_else(|e| {
                log::warn!("[data-safety] {} cannot be read ({e}); starting with no saved verdicts", path.display());
                FileShape::default()
            }),
            Err(_) => FileShape::default(),
        };
        Self { path, data }
    }

    /// The store beside another config file (`sources.json`).
    pub fn beside(config_file: &Path) -> Self {
        Self::load(config_file.with_file_name(FILE_NAME))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn get(&self, source_id: &str) -> Option<&SavedVerdict> {
        self.data.verdicts.get(source_id)
    }

    /// Every saved verdict: `(registry id, verdict)`.
    pub fn all(&self) -> impl Iterator<Item = (&String, &SavedVerdict)> {
        self.data.verdicts.iter()
    }

    /// Replace the record of `source_id`. Written (atomically, owner-only)
    /// only when what it says changed.
    pub fn put(&mut self, source_id: &str, verdict: SavedVerdict) -> Result<(), String> {
        if self.data.verdicts.get(source_id).is_some_and(|v| v.same_as(&verdict)) {
            return Ok(());
        }
        self.data.verdicts.insert(source_id.to_owned(), verdict);
        self.save()
    }

    /// Drop the record of a source that is gone from Settings.
    pub fn remove(&mut self, source_id: &str) -> Result<(), String> {
        if self.data.verdicts.remove(source_id).is_some() {
            self.save()?;
        }
        Ok(())
    }

    fn save(&self) -> Result<(), String> {
        project_model::atomic::write_json_atomic(&self.path, &self.data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data_safety::parse_block;
    use serde_json::json;

    fn verdict(state: &str, blocks: bool, at: u64) -> SavedVerdict {
        SavedVerdict {
            block: parse_block(&json!({ "dataSafety": {
                "instance": "app/0", "state": state, "blocksAgents": blocks,
                "findings": [{ "rule": "card-number", "count": 2 }],
                "reasons": [{ "component": "db", "text": "db: 2 findings (card-number ×2)" }],
                "bound": { "source": "tracker", "key": "ABC-1", "folders": [if cfg!(windows) { "C:\\srv\\example\\abc-1" } else { "/srv/example/abc-1" }] },
                "project": if cfg!(windows) { "C:\\srv\\example" } else { "/srv/example" },
                "ackActionId": "data-safety-ack",
            }})),
            source_label: "Stack app".into(),
            manifest_id: "stack:app".into(),
            fallback: Some(PathBuf::from("/srv/example")),
            saved_ms: at,
        }
    }

    #[test]
    fn verdicts_are_saved_replaced_and_removed_and_survive_a_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let mut store = VerdictStore::load(path.clone());
        assert!(store.get("s1").is_none());
        store.put("s1", verdict("not-clean", true, 1)).unwrap();
        store.put("s2", SavedVerdict { block: None, ..verdict("clean", false, 1) }).unwrap();
        let reopened = VerdictStore::load(path.clone());
        assert_eq!(reopened.get("s1"), Some(&verdict("not-clean", true, 1)), "every field round-trips");
        assert_eq!(reopened.get("s2").unwrap().block, None, "an answer without a block is kept as such");
        assert_eq!(reopened.all().count(), 2);

        // The same verdict at a later time does not rewrite the file.
        let before = std::fs::read_to_string(&path).unwrap();
        store.put("s1", verdict("not-clean", true, 99)).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        // A changed one replaces it.
        store.put("s1", verdict("acknowledged", false, 100)).unwrap();
        let reopened = VerdictStore::load(path.clone());
        let b = reopened.get("s1").unwrap().block.as_ref().unwrap();
        assert_eq!((b.state.as_str(), b.blocks_agents), ("acknowledged", false));

        store.remove("s1").unwrap();
        store.remove("s1").unwrap();
        let reopened = VerdictStore::load(path);
        assert!(reopened.get("s1").is_none());
        assert!(reopened.get("s2").is_some());
    }

    #[test]
    fn a_broken_or_missing_file_is_an_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        assert_eq!(VerdictStore::load(path.clone()).all().count(), 0);
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(VerdictStore::load(path.clone()).all().count(), 0);
        assert_eq!(VerdictStore::beside(&dir.path().join("sources.json")).path(), path);
    }
}
