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
//! - **A source that is gone** takes its record with it: one detached in
//!   Settings, or one whose project was removed from the app. A source
//!   that merely failed to start or crashed keeps it (fail closed).
//! - **A launched source no longer declared** in its project's
//!   `chappa.yml` (`undeclared`) keeps its record, still enforced and
//!   marked as removed, until the project's new content is trusted: only
//!   then does the record go. Anything that can write the file could
//!   otherwise lift a block by deleting a `source:` key.
//! - **Stack sources are remembered** (`stackSources`): once a source's
//!   trusted manifest says it is a stack (`stack:…` serving Environment),
//!   that is kept, so on a later launch it blocks its folder from the first
//!   instant, before its manifest arrives, until its first answer. Trusting
//!   a changed manifest keeps the flag only if it is still a stack.
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

/// A source whose trusted manifest said it is a stack: what its block
/// before the manifest arrives is named after.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RememberedStack {
    pub manifest_id: String,
    pub source_label: String,
}

/// A launched source whose `source:` command its project's `chappa.yml`
/// stopped declaring, waiting for that content to be trusted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Undeclared {
    pub project: PathBuf,
    pub command: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct FileShape {
    #[serde(default)]
    verdicts: BTreeMap<String, SavedVerdict>,
    #[serde(default, rename = "stackSources", skip_serializing_if = "BTreeMap::is_empty")]
    stack_sources: BTreeMap<String, RememberedStack>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    undeclared: BTreeMap<String, Undeclared>,
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

    /// Whether `source_id` is remembered as a stack source.
    pub fn stack(&self, source_id: &str) -> Option<&RememberedStack> {
        self.data.stack_sources.get(source_id)
    }

    /// Record what the trusted manifest of `source_id` says: `Some` = a
    /// stack source, `None` = not one (the flag goes). Written only on a
    /// change.
    pub fn set_stack(&mut self, source_id: &str, stack: Option<RememberedStack>) -> Result<(), String> {
        let changed = match stack {
            Some(stack) => self.data.stack_sources.insert(source_id.to_owned(), stack.clone()).as_ref() != Some(&stack),
            None => self.data.stack_sources.remove(source_id).is_some(),
        };
        if changed {
            self.save()?;
        }
        Ok(())
    }

    /// Whether `source_id` is waiting for its project's new content to be
    /// trusted before its record goes.
    pub fn undeclared(&self, source_id: &str) -> Option<&Undeclared> {
        self.data.undeclared.get(source_id)
    }

    /// The sources of the project at `project` waiting that way.
    pub fn undeclared_in(&self, project: &Path) -> Vec<(String, Undeclared)> {
        self.data
            .undeclared
            .iter()
            .filter(|(_, u)| u.project == project)
            .map(|(id, u)| (id.clone(), u.clone()))
            .collect()
    }

    /// Mark `source_id` as no longer declared (`Some`), or as declared again
    /// (`None`). Written only on a change.
    pub fn set_undeclared(&mut self, source_id: &str, undeclared: Option<Undeclared>) -> Result<(), String> {
        let changed = match undeclared {
            Some(u) => self.data.undeclared.insert(source_id.to_owned(), u.clone()).as_ref() != Some(&u),
            None => self.data.undeclared.remove(source_id).is_some(),
        };
        if changed {
            self.save()?;
        }
        Ok(())
    }

    /// Whether anything is kept for `source_id`.
    pub fn holds(&self, source_id: &str) -> bool {
        self.data.verdicts.contains_key(source_id) || self.data.stack_sources.contains_key(source_id)
    }

    /// Drop everything kept for a source that is gone: its verdict, whether
    /// it was a stack, and whether it waited to be dropped.
    pub fn remove(&mut self, source_id: &str) -> Result<(), String> {
        let verdict = self.data.verdicts.remove(source_id).is_some();
        let stack = self.data.stack_sources.remove(source_id).is_some();
        let undeclared = self.data.undeclared.remove(source_id).is_some();
        if verdict || stack || undeclared {
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
    fn a_remembered_stack_source_is_kept_cleared_and_removed_with_its_verdict() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let mut store = VerdictStore::load(path.clone());
        store.put("s1", verdict("clean", false, 1)).unwrap();
        let without_flags = std::fs::read_to_string(&path).unwrap();
        assert!(!without_flags.contains("stackSources"), "no empty map on disk: {without_flags}");
        let stack = RememberedStack { manifest_id: "stack:app".into(), source_label: "Stack app".into() };
        store.set_stack("s1", Some(stack.clone())).unwrap();
        store.set_stack("s2", Some(stack.clone())).unwrap();
        let reopened = VerdictStore::load(path.clone());
        assert_eq!(reopened.stack("s1"), Some(&stack));
        assert_eq!(reopened.stack("s2"), Some(&stack));
        // The same flag again does not rewrite the file.
        let before = std::fs::read_to_string(&path).unwrap();
        store.set_stack("s1", Some(stack.clone())).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        // Not a stack any more: cleared, the verdict stays.
        store.set_stack("s2", None).unwrap();
        assert!(VerdictStore::load(path.clone()).stack("s2").is_none());
        // Removed: verdict and flag both go.
        store.remove("s1").unwrap();
        let reopened = VerdictStore::load(path.clone());
        assert!(reopened.get("s1").is_none());
        assert!(reopened.stack("s1").is_none());
        // A file written before flags existed reads as having none.
        std::fs::write(&path, without_flags).unwrap();
        assert!(VerdictStore::load(path).stack("s1").is_none());
    }

    #[test]
    fn an_undeclared_source_is_marked_listed_per_project_and_removed_with_its_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let mut store = VerdictStore::load(path.clone());
        store.put("s1", verdict("not-clean", true, 1)).unwrap();
        assert!(store.holds("s1") && !store.holds("s2"));
        let mark = |project: &str, command: &str| Undeclared { project: PathBuf::from(project), command: command.into() };
        store.set_undeclared("s1", Some(mark("/srv/a", "stack"))).unwrap();
        store.set_undeclared("s3", Some(mark("/srv/b", "stack"))).unwrap();
        let reopened = VerdictStore::load(path.clone());
        assert_eq!(reopened.undeclared("s1"), Some(&mark("/srv/a", "stack")));
        assert_eq!(reopened.undeclared_in(Path::new("/srv/a")), vec![("s1".to_owned(), mark("/srv/a", "stack"))]);
        assert!(reopened.get("s1").is_some(), "the verdict stays while it waits");
        store.set_undeclared("s3", None).unwrap();
        assert!(VerdictStore::load(path.clone()).undeclared("s3").is_none());
        store.remove("s1").unwrap();
        let reopened = VerdictStore::load(path);
        assert!(reopened.undeclared("s1").is_none() && reopened.get("s1").is_none());
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
