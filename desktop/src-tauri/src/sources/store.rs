//! `sources.json` in the app-config dir: everything about sources that is
//! per machine rather than per project. The `chappa.yml` says WHICH commands
//! are sources (shared, committed); this file holds what one person on one
//! machine decided about them:
//!
//! ```json
//! {
//!   "trust": {"<project path>::<command>": "<hash>", "attached:127.0.0.1:7000": "<hash>"},
//!   "attached": [{"addr": "127.0.0.1:7000", "token_file": "/home/dev/.config/x/token"}],
//!   "hidden_views": ["prs"],
//!   "prompt_policy": {"<key>": "always"},
//!   "prompts_shown": {"<key>": ["agent-here"]},
//!   "key_match": {"<key>": {"enabled": false, "pattern": "[A-Z]+-\\d+"}}
//! }
//! ```
//!
//! `key_match` is per source that owns work items: whether its item keys
//! are looked for in pull requests' branches and titles, and the pattern
//! that finds candidate keys there (absent = on, with the default pattern).
//!
//! `prompt_policy` is per source: `first_time` (the default) shows an
//! `agent` action's prompt once per action id and remembers it in
//! `prompts_shown`; `always` shows it before every run.
//!
//! Trust is keyed the way the project trust gate is keyed (per project, by
//! path, since projects have no uuid) plus the command name, or by the
//! attached address. The same missing-file / unknown-field tolerance as the
//! other config stores, and atomic owner-only writes.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const FILE_NAME: &str = "sources.json";

/// A server started outside the app that the user pointed the app at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachedSource {
    pub addr: String,
    pub token_file: PathBuf,
}

/// When an `agent` action's prompt is shown before it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptPolicy {
    /// Once per `(source, action id)`; remembered after a confirmed run.
    #[default]
    FirstTime,
    /// Before every run.
    Always,
}

impl PromptPolicy {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "first_time" => Some(Self::FirstTime),
            "always" => Some(Self::Always),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct FileShape {
    #[serde(default)]
    trust: BTreeMap<String, String>,
    #[serde(default)]
    attached: Vec<AttachedSource>,
    #[serde(default)]
    hidden_views: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    prompt_policy: BTreeMap<String, PromptPolicy>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    prompts_shown: BTreeMap<String, BTreeSet<String>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    key_match: BTreeMap<String, KeyMatch>,
}

/// Whether a source's work item keys join pull requests that mention them
/// in their branch or title, and the pattern that finds the candidates
/// (`None` = the app's default). The webview applies it; this only keeps
/// the choice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyMatch {
    pub enabled: bool,
    #[serde(default)]
    pub pattern: Option<String>,
}

impl Default for KeyMatch {
    fn default() -> Self {
        Self {
            enabled: true,
            pattern: None,
        }
    }
}

/// The longest key pattern kept: a pattern is a short regular expression,
/// not a document.
pub const KEY_PATTERN_MAX: usize = 200;

#[derive(Debug)]
pub struct SourceStore {
    path: PathBuf,
    data: FileShape,
}

impl SourceStore {
    /// Missing / unreadable / unparseable → empty, never an error (a broken
    /// file costs the trust answers, which the gate simply asks again).
    pub fn new(path: PathBuf) -> Self {
        let data = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<FileShape>(&text).ok())
            .unwrap_or_default();
        Self { path, data }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The record key of a launched source: project path + command name.
    pub fn launched_key(project_path: &Path, command: &str) -> String {
        format!("{}::{command}", project_path.display())
    }

    /// The record key of an attached source.
    pub fn attached_key(addr: &str) -> String {
        format!("attached:{addr}")
    }

    /// Whether `hash` is exactly the hash on record for `key`.
    pub fn is_trusted(&self, key: &str, hash: &str) -> bool {
        self.data.trust.get(key).is_some_and(|h| h == hash)
    }

    pub fn mark_trusted(&mut self, key: &str, hash: &str) -> Result<(), String> {
        self.data.trust.insert(key.to_owned(), hash.to_owned());
        self.save()
    }

    /// Forget a record (a detached source takes its trust with it, so
    /// attaching the same address later asks again).
    pub fn forget_trust(&mut self, key: &str) -> Result<(), String> {
        if self.data.trust.remove(key).is_some() {
            self.save()?;
        }
        Ok(())
    }

    pub fn attached(&self) -> &[AttachedSource] {
        &self.data.attached
    }

    /// Add (or re-point) the attached source at `addr`.
    pub fn add_attached(&mut self, source: AttachedSource) -> Result<(), String> {
        self.data.attached.retain(|a| a.addr != source.addr);
        self.data.attached.push(source);
        self.save()
    }

    pub fn remove_attached(&mut self, addr: &str) -> Result<bool, String> {
        let before = self.data.attached.len();
        self.data.attached.retain(|a| a.addr != addr);
        if self.data.attached.len() == before {
            return Ok(false);
        }
        self.save()?;
        Ok(true)
    }

    /// The views the user switched off (they keep their data and trust).
    pub fn hidden_views(&self) -> &BTreeSet<String> {
        &self.data.hidden_views
    }

    pub fn set_view_hidden(&mut self, view: &str, hidden: bool) -> Result<(), String> {
        let changed = if hidden {
            self.data.hidden_views.insert(view.to_owned())
        } else {
            self.data.hidden_views.remove(view)
        };
        if changed {
            self.save()?;
        }
        Ok(())
    }

    /// How a source's agent prompts are shown (default: first time only).
    pub fn prompt_policy(&self, key: &str) -> PromptPolicy {
        self.data
            .prompt_policy
            .get(key)
            .copied()
            .unwrap_or_default()
    }

    pub fn set_prompt_policy(&mut self, key: &str, policy: PromptPolicy) -> Result<(), String> {
        if policy == PromptPolicy::default() {
            self.data.prompt_policy.remove(key);
        } else {
            self.data.prompt_policy.insert(key.to_owned(), policy);
        }
        self.save()
    }

    /// Whether the prompt of `action_id` was shown (and accepted) before.
    pub fn prompt_shown(&self, key: &str, action_id: &str) -> bool {
        self.data
            .prompts_shown
            .get(key)
            .is_some_and(|set| set.contains(action_id))
    }

    pub fn prompts_shown(&self, key: &str) -> Vec<String> {
        self.data
            .prompts_shown
            .get(key)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn mark_prompt_shown(&mut self, key: &str, action_id: &str) -> Result<(), String> {
        if self
            .data
            .prompts_shown
            .entry(key.to_owned())
            .or_default()
            .insert(action_id.to_owned())
        {
            self.save()?;
        }
        Ok(())
    }

    /// How a source's item keys join pull requests (default: on, default pattern).
    pub fn key_match(&self, key: &str) -> KeyMatch {
        self.data.key_match.get(key).cloned().unwrap_or_default()
    }

    /// Store the key-match choice. A blank pattern means the default; a
    /// pattern with control characters or longer than [`KEY_PATTERN_MAX`]
    /// is refused.
    pub fn set_key_match(
        &mut self,
        key: &str,
        enabled: bool,
        pattern: Option<&str>,
    ) -> Result<(), String> {
        let pattern = pattern.map(str::trim).filter(|p| !p.is_empty());
        if let Some(p) = pattern {
            if p.chars().count() > KEY_PATTERN_MAX {
                return Err(format!(
                    "a key pattern is at most {KEY_PATTERN_MAX} characters"
                ));
            }
            if p.chars().any(char::is_control) {
                return Err("a key pattern cannot hold control characters".into());
            }
        }
        let choice = KeyMatch {
            enabled,
            pattern: pattern.map(str::to_owned),
        };
        if choice == KeyMatch::default() {
            self.data.key_match.remove(key);
        } else {
            self.data.key_match.insert(key.to_owned(), choice);
        }
        self.save()
    }

    /// A changed trust hash means the source may be a different program:
    /// what it showed before no longer counts as shown.
    pub fn forget_prompts(&mut self, key: &str) -> Result<(), String> {
        if self.data.prompts_shown.remove(key).is_some() {
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

    fn temp_store() -> (tempfile::TempDir, SourceStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = SourceStore::new(dir.path().join(FILE_NAME));
        (dir, store)
    }

    #[test]
    fn trust_is_per_key_and_per_exact_hash_and_survives_a_reload() {
        let (dir, mut store) = temp_store();
        let key = SourceStore::launched_key(Path::new("/code/app"), "feed");
        assert_eq!(key, "/code/app::feed");
        assert!(!store.is_trusted(&key, "h1"));
        store.mark_trusted(&key, "h1").unwrap();
        assert!(store.is_trusted(&key, "h1"));
        assert!(
            !store.is_trusted(&key, "h2"),
            "a changed hash re-arms the gate"
        );
        assert!(!store.is_trusted(
            &SourceStore::launched_key(Path::new("/code/app"), "other"),
            "h1"
        ));
        let reopened = SourceStore::new(dir.path().join(FILE_NAME));
        assert!(reopened.is_trusted(&key, "h1"));
        let mut reopened = reopened;
        reopened.forget_trust(&key).unwrap();
        assert!(!SourceStore::new(dir.path().join(FILE_NAME)).is_trusted(&key, "h1"));
    }

    #[test]
    fn attached_sources_and_hidden_views_round_trip() {
        let (dir, mut store) = temp_store();
        store
            .add_attached(AttachedSource {
                addr: "127.0.0.1:7000".into(),
                token_file: "/tmp/t".into(),
            })
            .unwrap();
        // Re-adding the same address re-points it rather than duplicating.
        store
            .add_attached(AttachedSource {
                addr: "127.0.0.1:7000".into(),
                token_file: "/tmp/t2".into(),
            })
            .unwrap();
        store.set_view_hidden("prs", true).unwrap();
        store.set_view_hidden("prs", true).unwrap();
        store.set_view_hidden("work", true).unwrap();
        store.set_view_hidden("work", false).unwrap();
        let reopened = SourceStore::new(dir.path().join(FILE_NAME));
        assert_eq!(reopened.attached().len(), 1);
        assert_eq!(reopened.attached()[0].token_file, PathBuf::from("/tmp/t2"));
        assert_eq!(
            reopened.hidden_views().iter().cloned().collect::<Vec<_>>(),
            ["prs"]
        );
        let mut reopened = reopened;
        assert!(reopened.remove_attached("127.0.0.1:7000").unwrap());
        assert!(!reopened.remove_attached("127.0.0.1:7000").unwrap());
        assert!(SourceStore::new(dir.path().join(FILE_NAME))
            .attached()
            .is_empty());
    }

    #[test]
    fn prompt_policy_and_shown_prompts_round_trip() {
        let (dir, mut store) = temp_store();
        assert_eq!(store.prompt_policy("k"), PromptPolicy::FirstTime);
        assert!(!store.prompt_shown("k", "a"));
        store.mark_prompt_shown("k", "a").unwrap();
        store.mark_prompt_shown("k", "a").unwrap();
        store.set_prompt_policy("k", PromptPolicy::Always).unwrap();
        let reopened = SourceStore::new(dir.path().join(FILE_NAME));
        assert_eq!(reopened.prompt_policy("k"), PromptPolicy::Always);
        assert!(reopened.prompt_shown("k", "a"));
        assert!(!reopened.prompt_shown("k", "b"));
        assert_eq!(reopened.prompts_shown("k"), ["a"]);
        let mut reopened = reopened;
        reopened
            .set_prompt_policy("k", PromptPolicy::FirstTime)
            .unwrap();
        reopened.forget_prompts("k").unwrap();
        let text = std::fs::read_to_string(dir.path().join(FILE_NAME)).unwrap();
        assert!(
            !text.contains("prompt_policy") && !text.contains("prompts_shown"),
            "defaults leave no keys: {text}"
        );
        assert_eq!(PromptPolicy::parse("always"), Some(PromptPolicy::Always));
        assert_eq!(PromptPolicy::parse("sometimes"), None);
    }

    #[test]
    fn key_match_choices_round_trip_and_defaults_leave_no_key() {
        let (dir, mut store) = temp_store();
        assert_eq!(
            store.key_match("k"),
            KeyMatch {
                enabled: true,
                pattern: None
            }
        );
        store
            .set_key_match("k", false, Some("  [A-Z]+-\\d+ "))
            .unwrap();
        let reopened = SourceStore::new(dir.path().join(FILE_NAME));
        assert_eq!(
            reopened.key_match("k"),
            KeyMatch {
                enabled: false,
                pattern: Some("[A-Z]+-\\d+".into())
            }
        );
        assert_eq!(reopened.key_match("other"), KeyMatch::default());
        let mut reopened = reopened;
        assert!(reopened
            .set_key_match("k", true, Some(&"x".repeat(KEY_PATTERN_MAX + 1)))
            .unwrap_err()
            .contains("at most"));
        assert!(reopened
            .set_key_match(
                "k",
                true,
                Some(
                    "a
b"
                )
            )
            .unwrap_err()
            .contains("control"));
        reopened.set_key_match("k", true, Some("  ")).unwrap();
        let text = std::fs::read_to_string(dir.path().join(FILE_NAME)).unwrap();
        assert!(
            !text.contains("key_match"),
            "on with the default pattern is the default: {text}"
        );
    }

    #[test]
    fn a_broken_or_missing_file_is_an_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        assert!(SourceStore::new(path.clone()).attached().is_empty());
        std::fs::write(&path, "{ not json").unwrap();
        let store = SourceStore::new(path.clone());
        assert!(store.hidden_views().is_empty());
        // Unknown fields load (a newer build's file) and known ones are kept.
        std::fs::write(&path, r#"{"trust": {"k": "h"}, "future": 1}"#).unwrap();
        assert!(SourceStore::new(path).is_trusted("k", "h"));
    }
}
