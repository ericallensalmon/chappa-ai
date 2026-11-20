//! Acknowledged findings, kept so a known false positive does not come
//! back on every scan.
//!
//! Two records in the project's app-private gate storage
//! ([`crate::gate_store`]), never in the project itself: anything working
//! in the project can write there, and an acknowledgement it wrote would
//! silence a real finding.
//!
//! - [`BASELINE_RECORD`]: a JSON array in gitleaks' report shape holding,
//!   per acknowledged finding, its fingerprint and the parts it is made of
//!   (`RuleID`, `File`, `StartLine`, `Commit`) and the hash of the line the
//!   finding was on ([`line_hash`]). No value field is ever written.
//! - [`NOTES_RECORD`]: who acknowledged each fingerprint, why and when.
//!   Kept apart so the baseline stays in the scanner's shape.
//!
//! A fingerprint is gitleaks' own: `file:rule:line`, with `commit:` in
//! front for a finding from a commit scan. The baseline is applied here,
//! after any scanner ran, by fingerprint AND line hash, so it works the
//! same for gitleaks and for hooks, and a different secret written on an
//! acknowledged line is a new finding. It is deliberately not handed to
//! gitleaks as `--baseline-path`: gitleaks matches a baseline entry on its
//! description, columns, entropy and commit metadata as well, none of which
//! this record keeps, so it would suppress nothing. Like gitleaks'
//! fingerprints, an entry is tied to its line: move the secret and it is a
//! new finding.
//!
//! **Before this storage existed** the baseline was a file in the project's
//! work folder (`.work/secrets-baseline.json`). That file is never trusted:
//! until the project has a baseline record here, its fingerprints are read
//! only to say that a finding was acknowledged before
//! ([`Baseline::acked_before`]), so the person can confirm them again in
//! one step. They suppress nothing.

use crate::gate_store::{GateError, GateProject, Scope};
use crate::safety::{Finding, Location, SafetyError};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The baseline's record name in the gate storage (project scope).
pub const BASELINE_RECORD: &str = "secrets-baseline";
/// The notes' record name.
pub const NOTES_RECORD: &str = "secrets-baseline-notes";
/// The work-folder file older versions kept the baseline in.
pub const LEGACY_BASELINE_FILE: &str = "secrets-baseline.json";
/// The notes file older versions kept beside it.
pub const LEGACY_NOTES_FILE: &str = "secrets-baseline.notes.json";
/// The work folder that file was in, under the project.
pub const LEGACY_WORK_DIR: &str = ".work";
/// The largest file a line is read from for its hash.
const LINE_SOURCE_CAP: u64 = 16 * 1024 * 1024;

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

/// The hash an acknowledgement binds to: SHA-256 over the fingerprint and
/// the text of the line the finding is on (the whole file when it names no
/// line), hex. Kept only in owner-only storage, and only for lines that
/// were acknowledged.
pub fn line_hash(fingerprint: &str, line: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(b"chappa-secret-line-v1\0");
    h.update(fingerprint.as_bytes());
    h.update(b"\0");
    h.update(line);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Where the line a finding names is read from.
pub trait LineSource {
    /// The bytes of the line `f` names (without its line break), or the
    /// whole file when it names no line. `None` when it cannot be read: an
    /// acknowledgement then neither forms nor matches.
    fn line(&self, f: &Finding) -> Option<Vec<u8>>;
}

/// Lines of files named relative to `root` (or absolute), as they are on
/// disk now; a finding from a commit is read from that commit with git.
#[derive(Debug, Clone)]
pub struct TreeLines {
    pub root: PathBuf,
}

impl TreeLines {
    pub fn new(root: impl Into<PathBuf>) -> TreeLines {
        TreeLines { root: root.into() }
    }
}

impl LineSource for TreeLines {
    fn line(&self, f: &Finding) -> Option<Vec<u8>> {
        let Location::Source { file, line, commit } = &f.location else {
            return None;
        };
        let path = if file.is_absolute() {
            file.clone()
        } else {
            self.root.join(file)
        };
        let bytes = match commit {
            None => {
                if fs::metadata(&path).ok()?.len() > LINE_SOURCE_CAP {
                    return None;
                }
                fs::read(&path).ok()?
            }
            Some(c) => blob_at(&path, c)?,
        };
        nth_line(&bytes, *line)
    }
}

/// The file at `path` as commit `commit` has it, read with git from the
/// file's own folder (`<commit>:./<name>`).
fn blob_at(path: &Path, commit: &str) -> Option<Vec<u8>> {
    if !(7..=64).contains(&commit.len()) || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let dir = path.parent()?;
    let name = path.file_name()?.to_string_lossy().into_owned();
    let mut cmd = crate::git::command(crate::git::PROGRAM, dir);
    cmd.args(["show", "--no-textconv", "--end-of-options"])
        .arg(format!("{commit}:./{name}"));
    let ran = super::process::run(
        &mut cmd,
        "git show",
        Duration::from_secs(20),
        LINE_SOURCE_CAP as usize,
    )
    .ok()?;
    (ran.status == Some(0) && !ran.stdout_overflowed).then_some(ran.stdout)
}

/// Line `line` (1-based) of `bytes` without its `\n` / `\r\n`; all of
/// `bytes` for no line.
fn nth_line(bytes: &[u8], line: Option<u32>) -> Option<Vec<u8>> {
    let Some(n) = line.filter(|n| *n > 0) else {
        return Some(bytes.to_vec());
    };
    let text = bytes.split(|b| *b == b'\n').nth(n as usize - 1)?;
    Some(text.strip_suffix(b"\r").unwrap_or(text).to_vec())
}

/// One baseline entry: the fingerprint, its parts and the line's hash.
/// There is no field for a value.
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
    /// [`line_hash`] of the line when it was acknowledged. An entry
    /// without one matches nothing.
    #[serde(rename = "LineHash", default)]
    pub line_hash: String,
}

impl BaselineEntry {
    fn from_finding(f: &Finding, line_hash: String) -> Option<Self> {
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
            line_hash,
        })
    }

    /// The fingerprint rebuilt from the parts, so an entry is keyed by what
    /// it actually describes.
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
    /// Fingerprints the work-folder file of an older version listed. They
    /// suppress nothing; see the module docs.
    legacy: BTreeSet<String>,
}

fn gate_err(e: GateError) -> SafetyError {
    let path = match &e {
        GateError::Io { path, .. } | GateError::Json { path, .. } | GateError::NotPrivate(path) => {
            path.clone()
        }
        GateError::NotInstalled | GateError::BadName(_) => PathBuf::new(),
    };
    SafetyError::Io {
        path,
        message: e.to_string(),
    }
}

impl Baseline {
    /// Load the project's baseline from the gate storage; none is an empty
    /// one. While there is none, the fingerprints an older version left in
    /// the project's work folder are read as acknowledged before.
    pub fn load(gate: &GateProject) -> Result<Self, SafetyError> {
        let mut b = Baseline::default();
        match gate
            .read::<Vec<BaselineEntry>>(Scope::Project, BASELINE_RECORD)
            .map_err(gate_err)?
        {
            Some(list) => {
                for mut e in list {
                    e.fingerprint = e.derived_fingerprint();
                    b.entries.insert(e.fingerprint.clone(), e);
                }
                if let Some(notes) = gate
                    .read::<IndexMap<String, AckNote>>(Scope::Project, NOTES_RECORD)
                    .map_err(gate_err)?
                {
                    b.notes = notes
                        .into_iter()
                        .filter(|(k, _)| b.entries.contains_key(k))
                        .collect();
                }
            }
            None => b.legacy = legacy_fingerprints(gate.project_dir()),
        }
        Ok(b)
    }

    /// Write both records. Once written, the work-folder file of an older
    /// version is no longer read at all.
    pub fn save(&self, gate: &GateProject) -> Result<(), SafetyError> {
        let entries: Vec<&BaselineEntry> = self.entries.values().collect();
        gate.write(Scope::Project, BASELINE_RECORD, &entries)
            .map_err(gate_err)?;
        gate.write(Scope::Project, NOTES_RECORD, &self.notes)
            .map_err(gate_err)
    }

    /// Record `findings` as acknowledged, each bound to its line as
    /// `lines` reads it now. A finding whose line cannot be read is not
    /// recorded. Returns how many were new; an already-known one takes the
    /// newer line hash and note.
    pub fn acknowledge(
        &mut self,
        findings: &[Finding],
        lines: &dyn LineSource,
        who: &str,
        why: &str,
        at_utc: &str,
    ) -> usize {
        let mut added = 0;
        for f in findings {
            let Some(fp) = fingerprint(f) else {
                continue;
            };
            let Some(line) = lines.line(f) else {
                continue;
            };
            let Some(entry) = BaselineEntry::from_finding(f, line_hash(&fp, &line)) else {
                continue;
            };
            if self.entries.insert(fp.clone(), entry).is_none() {
                added += 1;
            }
            self.notes.insert(
                fp,
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

    /// Whether an acknowledgement covers `f`: its fingerprint is listed and
    /// the line it is on now hashes to what was acknowledged.
    pub fn contains(&self, f: &Finding, lines: &dyn LineSource) -> bool {
        let Some(fp) = fingerprint(f) else {
            return false;
        };
        let Some(entry) = self.entries.get(&fp) else {
            return false;
        };
        if entry.line_hash.is_empty() {
            return false;
        }
        lines
            .line(f)
            .is_some_and(|line| line_hash(&fp, &line) == entry.line_hash)
    }

    /// Whether an older version's work-folder baseline listed `f`: shown so
    /// the person can confirm it again, never suppressing it.
    pub fn acked_before(&self, f: &Finding) -> bool {
        fingerprint(f).is_some_and(|k| self.legacy.contains(&k))
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
    pub fn apply(&self, findings: Vec<Finding>, lines: &dyn LineSource) -> (Vec<Finding>, usize) {
        let before = findings.len();
        let fresh: Vec<Finding> = findings
            .into_iter()
            .filter(|f| !self.contains(f, lines))
            .collect();
        let covered = before - fresh.len();
        (fresh, covered)
    }
}

/// The fingerprints in an older version's
/// `<project>/.work/secrets-baseline.json`. Unreadable or absent: none.
fn legacy_fingerprints(project: &Path) -> BTreeSet<String> {
    #[derive(Deserialize)]
    struct Old {
        #[serde(rename = "RuleID")]
        rule_id: String,
        #[serde(rename = "File")]
        file: String,
        #[serde(rename = "StartLine", default)]
        start_line: u32,
        #[serde(rename = "Commit", default)]
        commit: String,
    }
    let path = project.join(LEGACY_WORK_DIR).join(LEGACY_BASELINE_FILE);
    let Ok(text) = fs::read_to_string(path) else {
        return BTreeSet::new();
    };
    let Ok(list) = serde_json::from_str::<Vec<Old>>(&text) else {
        return BTreeSet::new();
    };
    list.into_iter()
        .map(|o| {
            BaselineEntry {
                rule_id: o.rule_id,
                file: o.file,
                start_line: o.start_line,
                commit: o.commit,
                fingerprint: String::new(),
                line_hash: String::new(),
            }
            .derived_fingerprint()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gate_store::GateStore;
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

    /// A project folder and its gate records, both in a temp folder.
    struct Fixture {
        _tmp: tempfile::TempDir,
        project: PathBuf,
        gate: GateProject,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("proj");
        fs::create_dir_all(&project).unwrap();
        let gate = GateStore::new(tmp.path().join("gate")).project(&project);
        Fixture {
            _tmp: tmp,
            project,
            gate,
        }
    }

    fn write(root: &Path, rel: &str, text: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
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
        let fx = fixture();
        let lines = TreeLines::new(&fx.project);
        write(&fx.project, ".env", "TOKEN=one\nTOKEN=two\n");
        write(&fx.project, "cfg.txt", "a\nkey = x\n");
        assert!(
            Baseline::load(&fx.gate).unwrap().is_empty(),
            "missing is empty"
        );

        let a = finding("github-pat", ".env", Some(1), None);
        let b = finding("generic-api-key", "cfg.txt", Some(2), None);
        let c = finding("github-pat", ".env", Some(2), None);
        let mut base = Baseline::default();
        assert_eq!(
            base.acknowledge(
                &[a.clone(), b.clone()],
                &lines,
                "me",
                "test fixture",
                "2026-01-01T00:00:00Z"
            ),
            2
        );
        assert_eq!(
            base.acknowledge(
                std::slice::from_ref(&a),
                &lines,
                "you",
                "still a fixture",
                "2026-01-02T00:00:00Z"
            ),
            0
        );
        base.save(&fx.gate).unwrap();

        let loaded = Baseline::load(&fx.gate).unwrap();
        assert_eq!(loaded, base);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded.note(".env:github-pat:1").unwrap().who, "you");
        let (fresh, covered) = loaded.apply(vec![a.clone(), b.clone(), c.clone()], &lines);
        assert_eq!(fresh, [c]);
        assert_eq!(covered, 2);

        // The record is in gitleaks' report shape plus the line hash, and
        // nothing that holds a value.
        let raw: serde_json::Value = fx
            .gate
            .read(Scope::Project, BASELINE_RECORD)
            .unwrap()
            .unwrap();
        let first = raw.as_array().unwrap()[0].as_object().unwrap();
        let mut keys: Vec<&str> = first.keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "Commit",
                "File",
                "Fingerprint",
                "LineHash",
                "RuleID",
                "StartLine"
            ]
        );
        assert_eq!(first["Fingerprint"], ".env:github-pat:1");
        assert!(!raw.to_string().contains("one"));

        let mut again = loaded;
        assert!(again.remove(".env:github-pat:1"));
        assert!(!again.remove(".env:github-pat:1"));
        assert!(again.note(".env:github-pat:1").is_none());
        assert!(!again.contains(&a, &lines));
        assert!(again.contains(&b, &lines));
        // Nothing was written into the project.
        assert!(!fx.project.join(".work").exists());
    }

    /// The acknowledgement is of the line as it was: another secret written
    /// on the same line (same file, rule and line number) is a new finding.
    #[test]
    fn a_different_secret_on_an_acknowledged_line_is_a_new_finding() {
        let fx = fixture();
        let lines = TreeLines::new(&fx.project);
        write(&fx.project, ".env", "TOKEN=first-value\n");
        let f = finding("github-pat", ".env", Some(1), None);
        let mut base = Baseline::default();
        base.acknowledge(std::slice::from_ref(&f), &lines, "me", "ok", "t");
        base.save(&fx.gate).unwrap();
        let base = Baseline::load(&fx.gate).unwrap();
        assert!(base.contains(&f, &lines));
        write(&fx.project, ".env", "TOKEN=other-value\n");
        assert!(!base.contains(&f, &lines), "a new secret is a new finding");
        let (fresh, covered) = base.apply(vec![f.clone()], &lines);
        assert_eq!((fresh.len(), covered), (1, 0));
        write(&fx.project, ".env", "TOKEN=first-value\r\n");
        assert!(base.contains(&f, &lines), "only the line break changed");
    }

    /// An entry written into the record by hand (no line hash, or one that
    /// does not match) covers nothing.
    #[test]
    fn an_entry_without_the_acknowledged_line_hash_suppresses_nothing() {
        let fx = fixture();
        let lines = TreeLines::new(&fx.project);
        write(&fx.project, "f", "a\nb\nc\nkey\n");
        fx.gate
            .write(
                Scope::Project,
                BASELINE_RECORD,
                &serde_json::json!([
                    {"RuleID":"r","File":"f","StartLine":4,"Fingerprint":"something else"},
                    {"RuleID":"r","File":"f","StartLine":3,"Fingerprint":"f:r:3","LineHash":"00"}
                ]),
            )
            .unwrap();
        let b = Baseline::load(&fx.gate).unwrap();
        assert_eq!(b.len(), 2, "keyed by their parts");
        assert!(!b.contains(&finding("r", "f", Some(4), None), &lines));
        assert!(!b.contains(&finding("r", "f", Some(3), None), &lines));
    }

    /// The work-folder baseline an older version kept (or one an agent
    /// planted) suppresses nothing; its entries are only shown as
    /// acknowledged before, until the first acknowledgement here.
    #[test]
    fn a_hand_written_work_folder_baseline_suppresses_nothing() {
        let fx = fixture();
        let lines = TreeLines::new(&fx.project);
        write(&fx.project, ".env", "TOKEN=x\n");
        write(
            &fx.project,
            ".work/secrets-baseline.json",
            r#"[{"RuleID":"github-pat","File":".env","StartLine":1,"Fingerprint":".env:github-pat:1"}]"#,
        );
        let f = finding("github-pat", ".env", Some(1), None);
        let other = finding("github-pat", "other", Some(1), None);
        let b = Baseline::load(&fx.gate).unwrap();
        let (fresh, covered) = b.apply(vec![f.clone()], &lines);
        assert_eq!((fresh.len(), covered), (1, 0), "nothing suppressed");
        assert!(b.acked_before(&f));
        assert!(!b.acked_before(&other));

        // One confirm: the acknowledgement made here suppresses it, and the
        // old file is not read any more.
        let mut b = b;
        b.acknowledge(std::slice::from_ref(&f), &lines, "human:me", "again", "t");
        b.save(&fx.gate).unwrap();
        let b = Baseline::load(&fx.gate).unwrap();
        assert!(b.contains(&f, &lines));
        assert!(!b.acked_before(&f));
    }

    #[test]
    fn a_record_that_does_not_parse_is_an_error_naming_it() {
        let fx = fixture();
        fx.gate
            .write(
                Scope::Project,
                BASELINE_RECORD,
                &serde_json::json!({"x": 1}),
            )
            .unwrap();
        let err = Baseline::load(&fx.gate).unwrap_err().to_string();
        assert!(err.contains(BASELINE_RECORD), "{err}");
    }

    #[test]
    fn lines_are_read_one_based_and_a_missing_line_is_none() {
        assert_eq!(nth_line(b"a\r\nb\nc", Some(2)), Some(b"b".to_vec()));
        assert_eq!(nth_line(b"a\nb", Some(1)), Some(b"a".to_vec()));
        assert_eq!(nth_line(b"a\nb", Some(9)), None);
        assert_eq!(nth_line(b"a\nb", None), Some(b"a\nb".to_vec()));
    }
}
