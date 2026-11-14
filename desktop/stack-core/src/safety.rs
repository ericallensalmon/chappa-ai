//! Data safety and secret scanning: the shared vocabulary, and the one
//! invariant everything else in this area depends on.
//!
//! A dev database should hold scrubbed data, and a worktree should hold no
//! credentials. Nothing enforces either unless something checks. The check
//! is split: the user owns *extraction* (how to sample their database, which
//! scanner to run), this crate owns *detection, policy and enforcement*.
//!
//! **A finding never carries the value it matched.** [`Finding`] has no
//! field that could hold one, so no code path can store, log or show it by
//! accident. Matches are counted ([`Tally`]) and, where a shape must be
//! displayed, masked ([`mask`]). A raw sample lives in memory only while the
//! rules run and is dropped afterwards.
//!
//! This module holds the shared types, the hook stdout shapes, the masking
//! and tallying helpers, and the two traits the data rule engine and the
//! secret scanners implement. The secret-scan runner (file listing,
//! gitleaks and hook scanners, baseline, policy verdicts) is [`secrets`].
//! The data rule engine (patterns, allow lists, thresholds, `luhn` /
//! `iban` / `email-mx-shape` validators) behind [`DataSampler`] comes later.
//!
//! # Hook stdout shapes
//!
//! `marker` (attestation that the scrub ran; missing means unscrubbed,
//! whatever the sample says):
//! ```json
//! { "scrubbed": true, "version": "2026-03", "at": "2026-03-01T10:00:00Z" }
//! ```
//!
//! `sample` (rows for the rule engine; capped at [`SAMPLE_VALUE_CAP`] values
//! and [`SAMPLE_BYTE_CAP`] bytes, larger output is refused):
//! ```json
//! [ { "table": "customers", "column": "email", "values": ["a@example.com"] } ]
//! ```
//!
//! Either hook may instead print findings it computed itself:
//! ```json
//! { "findings": [ { "rule": "email-real-domain", "table": "customers",
//!                   "column": "email", "count": 12, "severity": "bad" } ] }
//! ```
//!
//! A secret scanner prints gitleaks-style JSON or SARIF. Only the rule id,
//! file, start line and commit are read from it; `Secret`, `Match`, `Line`
//! and every other field are ignored even when present, and gitleaks always
//! runs with `--redact` (see [`secrets`]).

pub mod secrets;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};

/// Most values one `sample` run may return.
pub const SAMPLE_VALUE_CAP: usize = 10_000;
/// Most bytes one `sample` run may print.
pub const SAMPLE_BYTE_CAP: usize = 5 * 1024 * 1024;
/// Longest masked shape shown for any value.
pub const MASK_MAX_LEN: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Warn,
    Bad,
}

/// What to do when a check finds something, or an attestation is missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Policy {
    /// Needs-you item, alert on the component, banner on the bound work item.
    Warn,
    /// Warn, and no new agent may be launched into anything bound to the
    /// instance until acknowledged. Running agents are not killed.
    BlockAgents,
    /// Block agents and stop the component (its volume is kept for inspection).
    Stop,
    /// The instance never becomes ready; `up` fails.
    BlockUp,
    /// A source build does not start (secret scanning only).
    BlockBuild,
}

impl Policy {
    pub fn as_str(self) -> &'static str {
        match self {
            Policy::Warn => "warn",
            Policy::BlockAgents => "block-agents",
            Policy::Stop => "stop",
            Policy::BlockUp => "block-up",
            Policy::BlockBuild => "block-build",
        }
    }

    /// Policies a data-safety `onFinding` may name.
    pub fn allowed_for_data(self) -> bool {
        matches!(
            self,
            Policy::Warn | Policy::BlockAgents | Policy::Stop | Policy::BlockUp
        )
    }

    /// Policies a secret-scan `onFinding` may name.
    pub fn allowed_for_secrets(self) -> bool {
        matches!(
            self,
            Policy::Warn | Policy::BlockAgents | Policy::BlockBuild
        )
    }

    /// Whether agents are blocked under this policy (every policy above
    /// `warn` includes the agent block).
    pub fn blocks_agents(self) -> bool {
        !matches!(self, Policy::Warn)
    }
}

impl fmt::Display for Policy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where a finding is. Data findings name a table and column; source
/// findings a file, line and (for commit scans) the commit.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Location {
    Data {
        table: String,
        column: String,
    },
    Source {
        file: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        line: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        commit: Option<String>,
    },
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Location::Data { table, column } => write!(f, "{table}.{column}"),
            Location::Source { file, line, commit } => {
                write!(f, "{}", file.display())?;
                if let Some(l) = line {
                    write!(f, ":{l}")?;
                }
                if let Some(c) = commit {
                    write!(f, " @{}", &c[..c.len().min(8)])?;
                }
                Ok(())
            }
        }
    }
}

/// One detection result. There is deliberately no field for the matched
/// text: add one and the no-value guarantee is gone for every consumer.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Finding {
    pub rule: String,
    #[serde(flatten)]
    pub location: Location,
    pub count: u32,
    pub severity: Severity,
}

impl Finding {
    /// Stable identity for acknowledgements and baselines: rule plus place.
    pub fn key(&self) -> String {
        format!("{}@{}", self.rule, self.location)
    }
}

/// A recorded "I know, carry on" for a set of findings. Lasts until the
/// next scan produces a finding outside `covers`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Acknowledgement {
    pub who: String,
    pub why: String,
    pub at_utc: String,
    /// [`Finding::key`] of each acknowledged finding.
    pub covers: Vec<String>,
}

impl Acknowledgement {
    /// True when every finding is covered, so the block lifts.
    pub fn clears(&self, findings: &[Finding]) -> bool {
        findings.iter().all(|f| self.covers.contains(&f.key()))
    }
}

/// Replace every character of `value` by `*`, keeping at most
/// [`MASK_MAX_LEN`] of them and marking a longer value with `…`. The result
/// says "something this long was here" and nothing else: no character class,
/// no prefix, no suffix survives.
pub fn mask(value: &str) -> String {
    let n = value.chars().count();
    if n == 0 {
        return String::new();
    }
    if n > MASK_MAX_LEN {
        format!("{}…", "*".repeat(MASK_MAX_LEN))
    } else {
        "*".repeat(n)
    }
}

/// Counts matches per rule and location without touching the values. The
/// rule engine feeds it one call per match; findings come out aggregated.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tally {
    counts: IndexMap<(String, Location), u32>,
}

impl Tally {
    pub fn new() -> Self {
        Self::default()
    }

    /// One more match of `rule` at `location`.
    pub fn record(&mut self, rule: &str, location: Location) {
        *self.counts.entry((rule.to_string(), location)).or_insert(0) += 1;
    }

    pub fn count(&self, rule: &str, location: &Location) -> u32 {
        self.counts
            .get(&(rule.to_string(), location.clone()))
            .copied()
            .unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.counts.is_empty()
    }

    /// Findings for every (rule, location) whose count reached the rule's
    /// threshold. `threshold` and `severity` are looked up per rule.
    pub fn findings(
        &self,
        threshold: impl Fn(&str) -> u32,
        severity: impl Fn(&str) -> Severity,
    ) -> Vec<Finding> {
        self.counts
            .iter()
            .filter(|((rule, _), count)| **count >= threshold(rule).max(1))
            .map(|((rule, location), count)| Finding {
                rule: rule.clone(),
                location: location.clone(),
                count: *count,
                severity: severity(rule),
            })
            .collect()
    }
}

// ------------------------------------------------------------ hook shapes

/// `marker` hook output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkerReport {
    pub scrubbed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
}

/// One `sample` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SampleRow {
    pub table: String,
    pub column: String,
    pub values: Vec<String>,
}

/// What a `sample` (or `marker`) hook printed: rows for the engine, or
/// findings the hook computed itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum HookOutput {
    Findings { findings: Vec<Finding> },
    Rows(Vec<SampleRow>),
}

#[derive(Debug, thiserror::Error)]
pub enum SafetyError {
    /// Holds the error's category and position only: a JSON parser's own
    /// message can quote the offending text, which here may be a value.
    #[error("hook output is not valid JSON ({0})")]
    Json(String),
    #[error("sample too large: {values} values (cap {SAMPLE_VALUE_CAP})")]
    TooManyValues { values: usize },
    #[error("sample too large: {bytes} bytes (cap {SAMPLE_BYTE_CAP})")]
    TooManyBytes { bytes: usize },
    #[error("hook failed: {0}")]
    Hook(String),
    #[error("scanner failed: {0}")]
    Scanner(String),
    #[error("{what} timed out after {seconds}s")]
    Timeout { what: String, seconds: u64 },
    #[error("cannot scan: {0}")]
    InvalidTarget(String),
    #[error("{}: {message}", path.display())]
    Io { path: PathBuf, message: String },
}

/// A JSON error by category and position, without the text it choked on.
pub(crate) fn json_brief(e: &serde_json::Error) -> String {
    let kind = match e.classify() {
        serde_json::error::Category::Io => "read error",
        serde_json::error::Category::Syntax => "syntax error",
        serde_json::error::Category::Data => "unexpected shape",
        serde_json::error::Category::Eof => "truncated",
    };
    format!("{kind} at line {} column {}", e.line(), e.column())
}

impl From<serde_json::Error> for SafetyError {
    fn from(e: serde_json::Error) -> Self {
        SafetyError::Json(json_brief(&e))
    }
}

/// Parse `marker` stdout.
pub fn parse_marker(stdout: &str) -> Result<MarkerReport, SafetyError> {
    Ok(serde_json::from_str(stdout)?)
}

/// Parse `sample` stdout, refusing output over either cap before any value
/// is kept.
pub fn parse_sample(stdout: &str) -> Result<HookOutput, SafetyError> {
    if stdout.len() > SAMPLE_BYTE_CAP {
        return Err(SafetyError::TooManyBytes {
            bytes: stdout.len(),
        });
    }
    let out: HookOutput = serde_json::from_str(stdout)?;
    if let HookOutput::Rows(rows) = &out {
        let values: usize = rows.iter().map(|r| r.values.len()).sum();
        if values > SAMPLE_VALUE_CAP {
            return Err(SafetyError::TooManyValues { values });
        }
    }
    Ok(out)
}

/// Runs a component's data-safety hooks. Implemented over the engine CLIs
/// elsewhere; the rule engine consumes the result.
pub trait DataSampler {
    /// `None` when the component declares no `marker` hook.
    fn marker(&self, component: &str) -> Result<Option<MarkerReport>, SafetyError>;
    /// `None` when the component declares no `sample` hook.
    fn sample(&self, component: &str) -> Result<Option<HookOutput>, SafetyError>;
}

/// Scans source for credentials. The default implementation (elsewhere)
/// wraps gitleaks; any scanner printing gitleaks-style JSON or SARIF fits.
pub trait SecretScanner {
    /// Scan `files` (paths under `root`, untracked and ignored ones
    /// included, which is what agents actually read).
    fn scan_files(&self, root: &Path, files: &[PathBuf]) -> Result<Vec<Finding>, SafetyError>;
    /// Scan the commits in `range` (`<base>..HEAD`), so a secret committed
    /// and then deleted is still caught before a push.
    fn scan_commits(&self, repo: &Path, range: &str) -> Result<Vec<Finding>, SafetyError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(table: &str, column: &str) -> Location {
        Location::Data {
            table: table.into(),
            column: column.into(),
        }
    }

    #[test]
    fn mask_keeps_only_length_capped() {
        assert_eq!(mask(""), "");
        assert_eq!(mask("a"), "*");
        assert_eq!(mask("12345678"), "********");
        assert_eq!(mask("123456789"), "********…");
        assert_eq!(mask("john.doe@example.com"), "********…");
        assert_eq!(mask("héllo"), "*****");
        for v in ["4111 1111 1111 1111", "secret-token-xyz", "ünïcödé"] {
            let m = mask(v);
            assert!(m.chars().all(|c| c == '*' || c == '…'), "{m}");
            assert!(!m.contains(|c: char| c.is_alphanumeric()));
        }
    }

    #[test]
    fn tally_counts_without_values() {
        let mut t = Tally::new();
        t.record("email", data("customers", "email"));
        t.record("email", data("customers", "email"));
        t.record("phone", data("customers", "phone"));
        assert_eq!(t.count("email", &data("customers", "email")), 2);
        assert_eq!(t.count("phone", &data("customers", "phone")), 1);
        assert_eq!(t.count("ssn", &data("customers", "ssn")), 0);
        let f = t.findings(|r| if r == "phone" { 5 } else { 1 }, |_| Severity::Bad);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].rule, "email");
        assert_eq!(f[0].count, 2);
        assert_eq!(f[0].key(), "email@customers.email");
        // a zero threshold behaves like one
        assert_eq!(t.findings(|_| 0, |_| Severity::Warn).len(), 2);
        let json = serde_json::to_string(&f[0]).unwrap();
        assert!(!json.contains("value"), "{json}");
    }

    #[test]
    fn finding_serializes_flat_for_both_locations() {
        let d = Finding {
            rule: "email-real-domain".into(),
            location: data("customers", "email"),
            count: 12,
            severity: Severity::Bad,
        };
        let json = serde_json::to_value(&d).unwrap();
        assert_eq!(json["table"], "customers");
        assert_eq!(json["column"], "email");
        assert_eq!(json["severity"], "bad");
        assert_eq!(serde_json::from_value::<Finding>(json).unwrap(), d);

        let s = Finding {
            rule: "aws-access-key".into(),
            location: Location::Source {
                file: PathBuf::from("src/config.rs"),
                line: Some(42),
                commit: Some("0123456789abcdef".into()),
            },
            count: 1,
            severity: Severity::Bad,
        };
        let json = serde_json::to_value(&s).unwrap();
        assert_eq!(json["file"], "src/config.rs");
        assert_eq!(json["line"], 42);
        assert_eq!(serde_json::from_value::<Finding>(json).unwrap(), s);
        assert_eq!(s.key(), "aws-access-key@src/config.rs:42 @01234567");
    }

    #[test]
    fn hook_shapes_parse() {
        let m = parse_marker(r#"{"scrubbed": true, "version": "3"}"#).unwrap();
        assert_eq!(
            m,
            MarkerReport {
                scrubbed: true,
                version: Some("3".into()),
                at: None
            }
        );
        assert!(parse_marker("not json").is_err());

        let rows = parse_sample(r#"[{"table":"t","column":"c","values":["x","y"]}]"#).unwrap();
        assert_eq!(
            rows,
            HookOutput::Rows(vec![SampleRow {
                table: "t".into(),
                column: "c".into(),
                values: vec!["x".into(), "y".into()]
            }])
        );

        let findings = parse_sample(
            r#"{"findings":[{"rule":"r","table":"t","column":"c","count":1,"severity":"warn"}]}"#,
        )
        .unwrap();
        match findings {
            HookOutput::Findings { findings } => assert_eq!(findings[0].rule, "r"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn sample_caps_are_enforced_at_the_edge() {
        let values: Vec<String> = vec!["v".to_string(); SAMPLE_VALUE_CAP];
        let row = SampleRow {
            table: "t".into(),
            column: "c".into(),
            values,
        };
        let ok = serde_json::to_string(&vec![row.clone()]).unwrap();
        assert!(parse_sample(&ok).is_ok());
        let mut over = row;
        over.values.push("one more".into());
        let text = serde_json::to_string(&vec![over]).unwrap();
        assert!(
            matches!(parse_sample(&text), Err(SafetyError::TooManyValues { values }) if values == SAMPLE_VALUE_CAP + 1)
        );

        let big = format!("[{}]", " ".repeat(SAMPLE_BYTE_CAP));
        assert!(matches!(
            parse_sample(&big),
            Err(SafetyError::TooManyBytes { .. })
        ));
    }

    #[test]
    fn json_errors_do_not_quote_values() {
        let err = parse_sample(r#"[{"table":"t","column":"c","values":[123456789]}]"#).unwrap_err();
        let shown = format!("{err} {err:?}");
        assert!(!shown.contains("123456789"), "{shown}");
        assert!(shown.contains("unexpected shape"), "{shown}");
        let err = parse_marker(r#"{"scrubbed": "4111111111111111"}"#).unwrap_err();
        assert!(!format!("{err} {err:?}").contains("4111"), "{err:?}");
    }

    #[test]
    fn policy_sets() {
        assert!(Policy::BlockUp.allowed_for_data());
        assert!(!Policy::BlockBuild.allowed_for_data());
        assert!(Policy::BlockBuild.allowed_for_secrets());
        assert!(!Policy::Stop.allowed_for_secrets());
        assert!(!Policy::Warn.blocks_agents());
        assert!(Policy::BlockAgents.blocks_agents());
        assert_eq!(
            serde_yaml::from_str::<Policy>("block-agents").unwrap(),
            Policy::BlockAgents
        );
        assert_eq!(Policy::BlockUp.to_string(), "block-up");
    }

    #[test]
    fn acknowledgement_clears_only_covered_findings() {
        let f1 = Finding {
            rule: "a".into(),
            location: data("t", "c"),
            count: 1,
            severity: Severity::Warn,
        };
        let f2 = Finding {
            rule: "b".into(),
            location: data("t", "d"),
            count: 1,
            severity: Severity::Warn,
        };
        let ack = Acknowledgement {
            who: "me".into(),
            why: "fixture data".into(),
            at_utc: "2026-01-01T00:00:00Z".into(),
            covers: vec![f1.key()],
        };
        assert!(ack.clears(std::slice::from_ref(&f1)));
        assert!(!ack.clears(&[f1, f2]));
        assert!(ack.clears(&[]));
    }
}
