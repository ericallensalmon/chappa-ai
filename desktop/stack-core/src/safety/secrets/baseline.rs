//! Acknowledged findings, kept locally so a known false positive does not
//! come back on every scan.
//!
//! Two files under the project's `.work/` directory (local, never
//! committed):
//!
//! - [`BASELINE_FILE`]: a JSON array in gitleaks' report shape holding,
//!   per acknowledged finding, its fingerprint and the parts it is made of
//!   (`RuleID`, `File`, `StartLine`, `Commit`) and nothing else. No value
//!   field is ever written.
//! - [`NOTES_FILE`]: who acknowledged each fingerprint, why and when. Kept
//!   apart so the baseline stays in the scanner's shape.
//!
//! A fingerprint is gitleaks' own: `file:rule:line`, with `commit:` in
//! front for a finding from a commit scan. The baseline is applied here,
//! after any scanner ran, by fingerprint, so it works the same for gitleaks
//! and for hooks. It is deliberately not handed to gitleaks as
//! `--baseline-path`: gitleaks matches a baseline entry on its description,
//! columns, entropy and commit metadata as well, none of which this file
//! keeps, so it would suppress nothing. Like gitleaks' fingerprints, an
//! entry is tied to its line: move the secret and it is a new finding.
//!
//! Both files are written atomically (temp file, then rename).

use crate::safety::{Finding, Location, SafetyError};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

pub const BASELINE_FILE: &str = "secrets-baseline.json";
pub const NOTES_FILE: &str = "secrets-baseline.notes.json";

/// The gitleaks-style fingerprint of a source finding, `None` for a data
/// finding.
pub fn fingerprint(f: &Finding) -> Option<String> {
    match &f.location {
        Location::Source { file, line, commit } => {
            let file = file.to_string_lossy().replace('\\', "/");
            let line = line.unwrap_or(0);
            Some(match commit {
                Some(c) => format!("{c}:{file}:{}:{line}", f.rule),
                None => format!("{file}:{}:{line}", f.rule),
            })
        }
        Location::Data { .. } => None,
    }
}

/// One baseline entry: the fingerprint and its parts. There is no field
/// for a value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaselineEntry {
    #[serde(rename = "RuleID")]
    pub rule_id: String,
    #[serde(rename = "File")]
    pub file: String,
    #[serde(rename = "StartLine", default)]
    pub start_line: u32,
    #[serde(rename = "Commit", default)]
    pub commit: String,
    #[serde(rename = "Fingerprint")]
    pub fingerprint: String,
}

impl BaselineEntry {
    fn from_finding(f: &Finding) -> Option<Self> {
        let fingerprint = fingerprint(f)?;
        let Location::Source { file, line, commit } = &f.location else {
            return None;
        };
        Some(Self {
            rule_id: f.rule.clone(),
            file: file.to_string_lossy().replace('\\', "/"),
            start_line: line.unwrap_or(0),
            commit: commit.clone().unwrap_or_default(),
            fingerprint,
        })
    }

    /// The fingerprint rebuilt from the parts, so a hand-edited entry is
    /// keyed by what it actually describes.
    fn derived_fingerprint(&self) -> String {
        if self.commit.is_empty() {
            format!("{}:{}:{}", self.file, self.rule_id, self.start_line)
        } else {
            format!(
                "{}:{}:{}:{}",
                self.commit, self.file, self.rule_id, self.start_line
            )
        }
    }
}

/// Who acknowledged a finding, why, and when.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AckNote {
    pub who: String,
    pub why: String,
    pub at_utc: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Baseline {
    entries: IndexMap<String, BaselineEntry>,
    notes: IndexMap<String, AckNote>,
}

impl Baseline {
    /// The baseline file under a project's `.work/` directory.
    pub fn path(work_dir: &Path) -> PathBuf {
        work_dir.join(BASELINE_FILE)
    }

    pub fn notes_path(work_dir: &Path) -> PathBuf {
        work_dir.join(NOTES_FILE)
    }

    /// Load both files; a missing file is an empty baseline.
    pub fn load(work_dir: &Path) -> Result<Self, SafetyError> {
        let mut b = Baseline::default();
        let path = Self::path(work_dir);
        if let Some(text) = read_optional(&path)? {
            let list: Vec<BaselineEntry> = serde_json::from_str(&text).map_err(|e| {
                super::report::json_error(&format!("baseline {}", path.display()), &e)
            })?;
            for mut e in list {
                e.fingerprint = e.derived_fingerprint();
                b.entries.insert(e.fingerprint.clone(), e);
            }
        }
        let notes_path = Self::notes_path(work_dir);
        if let Some(text) = read_optional(&notes_path)? {
            let notes: IndexMap<String, AckNote> = serde_json::from_str(&text).map_err(|e| {
                super::report::json_error(&format!("baseline notes {}", notes_path.display()), &e)
            })?;
            b.notes = notes
                .into_iter()
                .filter(|(k, _)| b.entries.contains_key(k))
                .collect();
        }
        Ok(b)
    }

    /// Write both files atomically, creating the directory if needed.
    pub fn save(&self, work_dir: &Path) -> Result<(), SafetyError> {
        // The baseline names files and lines of acknowledged secrets: the
        // folder it lives in must never be committed.
        crate::workdir::ensure(work_dir).map_err(|e| SafetyError::Io {
            path: work_dir.to_path_buf(),
            message: e.to_string(),
        })?;
        let entries: Vec<&BaselineEntry> = self.entries.values().collect();
        write_atomic(&Self::path(work_dir), &entries)?;
        write_atomic(&Self::notes_path(work_dir), &self.notes)
    }

    /// Record `findings` as acknowledged. Returns how many were new; an
    /// already-known one keeps its entry but takes the newer note.
    pub fn acknowledge(
        &mut self,
        findings: &[Finding],
        who: &str,
        why: &str,
        at_utc: &str,
    ) -> usize {
        let mut added = 0;
        for f in findings {
            let Some(entry) = BaselineEntry::from_finding(f) else {
                continue;
            };
            let key = entry.fingerprint.clone();
            if self.entries.insert(key.clone(), entry).is_none() {
                added += 1;
            }
            self.notes.insert(
                key,
                AckNote {
                    who: who.to_string(),
                    why: why.to_string(),
                    at_utc: at_utc.to_string(),
                },
            );
        }
        added
    }

    /// Withdraw an acknowledgement. Returns whether it existed.
    pub fn remove(&mut self, fingerprint: &str) -> bool {
        self.notes.shift_remove(fingerprint);
        self.entries.shift_remove(fingerprint).is_some()
    }

    pub fn contains(&self, f: &Finding) -> bool {
        fingerprint(f).is_some_and(|k| self.entries.contains_key(&k))
    }

    pub fn note(&self, fingerprint: &str) -> Option<&AckNote> {
        self.notes.get(fingerprint)
    }

    pub fn entries(&self) -> impl Iterator<Item = &BaselineEntry> {
        self.entries.values()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Split scan results into those still to act on and the number the
    /// baseline covers.
    pub fn apply(&self, findings: Vec<Finding>) -> (Vec<Finding>, usize) {
        let before = findings.len();
        let fresh: Vec<Finding> = findings.into_iter().filter(|f| !self.contains(f)).collect();
        let covered = before - fresh.len();
        (fresh, covered)
    }
}

fn read_optional(path: &Path) -> Result<Option<String>, SafetyError> {
    match fs::read_to_string(path) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(SafetyError::Io {
            path: path.to_path_buf(),
            message: e.to_string(),
        }),
    }
}

fn write_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), SafetyError> {
    let io = |e: std::io::Error| SafetyError::Io {
        path: path.to_path_buf(),
        message: e.to_string(),
    };
    let mut text = serde_json::to_string_pretty(value).map_err(|e| SafetyError::Io {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;
    text.push('\n');
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, text.as_bytes()).map_err(io)?;
    fs::rename(&tmp, path).map_err(io)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::safety::Severity;

    fn finding(rule: &str, file: &str, line: Option<u32>, commit: Option<&str>) -> Finding {
        Finding {
            rule: rule.into(),
            location: Location::Source {
                file: PathBuf::from(file),
                line,
                commit: commit.map(str::to_string),
            },
            count: 1,
            severity: Severity::Bad,
        }
    }

    #[test]
    fn fingerprints_match_the_gitleaks_shape() {
        assert_eq!(
            fingerprint(&finding("github-pat", ".env", Some(1), None)).unwrap(),
            ".env:github-pat:1"
        );
        assert_eq!(
            fingerprint(&finding("github-pat", "a/b.txt", Some(3), Some("f6bb19e1"))).unwrap(),
            "f6bb19e1:a/b.txt:github-pat:3"
        );
        assert_eq!(
            fingerprint(&finding("r", "x", None, None)).unwrap(),
            "x:r:0"
        );
        let data = Finding {
            rule: "r".into(),
            location: Location::Data {
                table: "t".into(),
                column: "c".into(),
            },
            count: 1,
            severity: Severity::Bad,
        };
        assert_eq!(fingerprint(&data), None);
    }

    #[test]
    fn round_trip_and_apply() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join(".work");
        assert!(
            Baseline::load(&work).unwrap().is_empty(),
            "missing is empty"
        );

        let a = finding("github-pat", ".env", Some(1), None);
        let b = finding(
            "generic-api-key",
            "cfg.txt",
            Some(2),
            Some("0123456789abcdef"),
        );
        let c = finding("github-pat", ".env", Some(2), None);
        let mut base = Baseline::default();
        assert_eq!(
            base.acknowledge(
                &[a.clone(), b.clone()],
                "me",
                "test fixture",
                "2026-01-01T00:00:00Z"
            ),
            2
        );
        assert_eq!(
            base.acknowledge(
                std::slice::from_ref(&a),
                "you",
                "still a fixture",
                "2026-01-02T00:00:00Z"
            ),
            0
        );
        base.save(&work).unwrap();
        assert!(!work.join("secrets-baseline.json.tmp").exists());
        assert!(!work.join("secrets-baseline.notes.json.tmp").exists());

        let loaded = Baseline::load(&work).unwrap();
        assert_eq!(loaded, base);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded.note(".env:github-pat:1").unwrap().who, "you");
        let (fresh, covered) = loaded.apply(vec![a.clone(), b.clone(), c.clone()]);
        assert_eq!(fresh, [c]);
        assert_eq!(covered, 2);

        // the baseline file is in gitleaks' report shape, and nothing else
        let raw: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(Baseline::path(&work)).unwrap()).unwrap();
        let first = raw.as_array().unwrap()[0].as_object().unwrap();
        let mut keys: Vec<&str> = first.keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(
            keys,
            ["Commit", "File", "Fingerprint", "RuleID", "StartLine"]
        );
        assert_eq!(first["Fingerprint"], ".env:github-pat:1");

        let mut again = loaded;
        assert!(again.remove(".env:github-pat:1"));
        assert!(!again.remove(".env:github-pat:1"));
        assert!(again.note(".env:github-pat:1").is_none());
        assert!(!again.contains(&a));
        assert!(again.contains(&b));
    }

    #[test]
    fn hand_edited_entries_are_keyed_by_their_parts_and_orphan_notes_dropped() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            Baseline::path(dir.path()),
            r#"[{"RuleID":"r","File":"f","StartLine":4,"Fingerprint":"something else"}]"#,
        )
        .unwrap();
        fs::write(
            Baseline::notes_path(dir.path()),
            r#"{"f:r:4":{"who":"a","why":"b","atUtc":"c"},"gone:r:1":{"who":"a","why":"b","atUtc":"c"}}"#,
        )
        .unwrap();
        let b = Baseline::load(dir.path()).unwrap();
        assert!(b.contains(&finding("r", "f", Some(4), None)));
        assert!(b.note("f:r:4").is_some());
        assert!(b.note("gone:r:1").is_none());
    }

    #[test]
    fn corrupt_baseline_is_an_error_naming_the_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(Baseline::path(dir.path()), "[{").unwrap();
        let err = Baseline::load(dir.path()).unwrap_err().to_string();
        assert!(err.contains(BASELINE_FILE), "{err}");
    }
}
