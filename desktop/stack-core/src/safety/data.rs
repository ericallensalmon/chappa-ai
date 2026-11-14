//! The data-safety rule engine: load the user's detection rules, run them
//! over what a component's `sample` hook printed, read its `marker`
//! attestation, and turn both into one verdict the enforcement acts on.
//!
//! A dev database is meant to hold scrubbed data. When one holding real
//! data is started by accident (the wrong backup restored, a scrub step that
//! silently failed), a check should say so before an agent is pointed at
//! it. The user owns extraction (the hooks know their engine and schema);
//! this module owns detection and the decision. It is pure: it never runs a
//! hook, it is handed the hook's stdout, and the only file it reads is the
//! rules file.
//!
//! ```text
//! data-safety.yml ──DataRules::load──▶ DataRules ─┐
//! sample stdout ───────────────────────────────────┼─▶ evaluate ─▶ DataVerdict ─▶ enforcement(def, verdict, ack)
//! marker stdout ───────────────────────────────────┘                                   │
//!                                              acknowledge(verdict, who, why, at) ◀────┘
//! ```
//!
//! # The rules file
//!
//! ```yaml
//! rules:
//!   - id: email-real-domain               # letters, digits, `.` `_` `-`; unique
//!     pattern: '[^@\s]+@[^@\s]+\.[a-z]{2,}'
//!     allow: ['@example\.(com|org|net)$'] # matched against the matched text
//!     threshold: 1                        # values per table.column before it is a finding
//!     validate: email-mx-shape            # luhn | iban | email-mx-shape
//!     columns: ['customers\..*']          # whole-name match on "table.column"
//!     severity: bad                       # warn | bad
//! ```
//!
//! A rule needs a `pattern`, a `validate`, or both. Unknown keys are errors
//! that name the key: a typo such as `treshold` must not quietly turn a rule
//! into a different one. Every load error names the file and, where there is
//! one, the rule.
//!
//! # How a rule matches
//!
//! For each sampled value in scope:
//! 1. **Column scope.** With `columns`, the rule applies only where some
//!    entry matches the whole `table.column` name (it is anchored at both
//!    ends, so `customers\.email` does not also select
//!    `old_customers.email_backup`). Without `columns` it applies everywhere.
//! 2. **Pattern.** Every non-empty, non-overlapping match in the value is a
//!    candidate. Without a pattern the candidate is the whole value, trimmed.
//! 3. **Allow list.** A candidate is dropped when any `allow` entry matches
//!    *the candidate text* (not the whole value), so `'@example\.com$'`
//!    anchors to the end of the matched address and `'^TEST-'` to its start.
//! 4. **Validator.** With `validate`, a surviving candidate must also pass it.
//! 5. **Count.** A value counts once for a rule when at least one candidate
//!    survives. Counts are kept per (rule, table, column); a count that
//!    reaches the rule's `threshold` becomes a [`Finding`].
//!
//! Patterns use the `regex` crate's syntax: matching takes time linear in
//! the input, which matters because both the rules and the sampled values
//! are untrusted. That syntax has no look-around and no backreferences; a
//! rule that uses them is refused at load with a message that says to use
//! `allow` instead (`'^(?!TEST-).+'` becomes `pattern: '.+'` with
//! `allow: ['^TEST-']`). Matching is case-sensitive unless the pattern says
//! `(?i)`.
//!
//! # The never-store-values rule
//!
//! Nothing this module returns holds a sampled value or any part of one.
//! [`Finding`] has no field for it, [`ScanOutcome`] and [`DataVerdict`] hold
//! counts and statuses, and errors describe a problem by position and kind
//! without quoting what was read. Table and column names come from the
//! hook, so they are length- and character-checked before they are used as
//! a finding's location. The rows themselves are borrowed for the scan and
//! never copied.

pub mod validate;

#[cfg(test)]
mod tests;

pub use validate::Validator;

use super::{
    parse_marker, parse_sample, Acknowledgement, Finding, HookOutput, Location, MarkerReport,
    Policy, SafetyError, SampleRow, Severity, Tally, SAMPLE_BYTE_CAP, SAMPLE_VALUE_CAP,
};
use crate::definition::DataSafetyDef;
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use serde_yaml::Value;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// Longest part of one value the rules look at, in bytes. A longer value is
/// cut (on a character boundary) before matching and counted in
/// [`ScanOutcome::truncated_values`], so a gap in coverage is visible rather
/// than silent.
pub const MATCH_VALUE_CAP: usize = 4 * 1024;
/// Compiled-size limit for each rule regex. Generous for any real rule; it
/// stops a pathological pattern (`\w{1000}{1000}`) from eating memory.
pub const REGEX_SIZE_LIMIT: usize = 1024 * 1024;
/// Largest rules file read.
pub const RULES_FILE_CAP: usize = 1024 * 1024;
/// Most rules one file may hold. With the sample caps this bounds a scan's
/// work at a few seconds even with every rule unscoped.
pub const MAX_RULES: usize = 128;
/// Most entries in one rule's `allow` or `columns` list.
pub const MAX_LIST_ENTRIES: usize = 64;
/// Longest table or column name accepted from a hook.
pub const MAX_NAME_LEN: usize = 128;
/// Longest rule id, in the file or from a hook.
pub const MAX_RULE_ID_LEN: usize = 64;
/// Most findings a hook may report itself.
pub const MAX_HOOK_FINDINGS: usize = 10_000;

const RULE_KEYS: &[&str] = &[
    "id",
    "pattern",
    "allow",
    "threshold",
    "validate",
    "columns",
    "severity",
];

// ------------------------------------------------------------------ errors

/// Why a rules file was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RulesErrorKind {
    Io(String),
    TooLarge {
        bytes: usize,
    },
    Yaml(String),
    /// The document or a rule has the wrong overall shape.
    Shape(String),
    UnknownTopLevelKey(String),
    UnknownKey(String),
    MissingId,
    InvalidId,
    DuplicateId,
    TooManyRules {
        rules: usize,
    },
    TooManyEntries {
        key: &'static str,
        entries: usize,
    },
    NeedsPatternOrValidate,
    WrongType {
        key: String,
        expected: &'static str,
    },
    /// Look-around or a backreference.
    UnsupportedSyntax {
        key: String,
    },
    BadRegex {
        key: String,
        detail: String,
    },
    RegexTooLarge {
        key: String,
    },
    UnknownValidator(String),
    UnknownSeverity(String),
    ZeroThreshold,
}

impl fmt::Display for RulesErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use RulesErrorKind::*;
        match self {
            Io(m) => write!(f, "cannot read: {m}"),
            TooLarge { bytes } => {
                write!(
                    f,
                    "file is {bytes} bytes, over the {RULES_FILE_CAP}-byte cap"
                )
            }
            Yaml(m) => write!(f, "not valid YAML: {m}"),
            Shape(m) => f.write_str(m),
            UnknownTopLevelKey(k) => write!(f, "unknown key `{k}` (expected `rules`)"),
            UnknownKey(k) => write!(
                f,
                "unknown key `{k}` (expected one of: {})",
                RULE_KEYS.join(", ")
            ),
            MissingId => f.write_str("rule has no `id`"),
            InvalidId => write!(
                f,
                "`id` must be 1-{MAX_RULE_ID_LEN} letters, digits, `.`, `_` or `-`"
            ),
            DuplicateId => f.write_str("duplicate rule id"),
            TooManyRules { rules } => write!(f, "{rules} rules, over the cap of {MAX_RULES}"),
            TooManyEntries { key, entries } => write!(
                f,
                "`{key}` has {entries} entries, over the cap of {MAX_LIST_ENTRIES}"
            ),
            NeedsPatternOrValidate => f.write_str("a rule needs `pattern`, `validate`, or both"),
            WrongType { key, expected } => write!(f, "`{key}` must be {expected}"),
            UnsupportedSyntax { key } => write!(
                f,
                "`{key}` uses look-around or a backreference, which rule patterns do not support; \
                 match broadly and use `allow` to exclude"
            ),
            BadRegex { key, detail } => write!(f, "`{key}` is not a valid pattern: {detail}"),
            RegexTooLarge { key } => write!(
                f,
                "`{key}` compiles to more than {REGEX_SIZE_LIMIT} bytes; simplify it"
            ),
            UnknownValidator(v) => write!(
                f,
                "unknown validator `{v}` (expected one of: {})",
                Validator::NAMES
            ),
            UnknownSeverity(s) => write!(f, "unknown severity `{s}` (expected warn or bad)"),
            ZeroThreshold => f.write_str("`threshold` must be at least 1"),
        }
    }
}

/// A rules-file load error, naming the file and, where known, the rule.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub struct RulesError {
    pub file: PathBuf,
    /// The rule's `id`, when it has one.
    pub rule: Option<String>,
    /// Position in `rules:`, when the error is inside a rule.
    pub index: Option<usize>,
    pub kind: RulesErrorKind,
}

impl fmt::Display for RulesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.file.display())?;
        match (&self.rule, self.index) {
            (Some(id), _) => write!(f, ": rule `{id}`")?,
            (None, Some(i)) => write!(f, ": rules[{i}]")?,
            (None, None) => {}
        }
        write!(f, ": {}", self.kind)
    }
}

// ------------------------------------------------------------------- rules

/// One compiled detection rule.
#[derive(Debug, Clone)]
pub struct DataRule {
    pub id: String,
    pattern: Option<Regex>,
    allow: Vec<Regex>,
    columns: Vec<Regex>,
    pub threshold: u32,
    pub validate: Option<Validator>,
    pub severity: Severity,
}

impl DataRule {
    /// Whether the rule applies to `table.column`.
    pub fn in_scope(&self, table: &str, column: &str) -> bool {
        if self.columns.is_empty() {
            return true;
        }
        let name = format!("{table}.{column}");
        self.columns.iter().any(|c| c.is_match(&name))
    }

    /// Whether `value` holds at least one match that survives the allow list
    /// and the validator. See the module docs for the order.
    pub fn matches(&self, value: &str) -> bool {
        match &self.pattern {
            Some(re) => re
                .find_iter(value)
                .any(|m| !m.as_str().is_empty() && self.accepts(m.as_str())),
            None => {
                let v = value.trim();
                !v.is_empty() && self.accepts(v)
            }
        }
    }

    fn accepts(&self, candidate: &str) -> bool {
        !self.allow.iter().any(|a| a.is_match(candidate))
            && self.validate.is_none_or(|v| v.check(candidate))
    }
}

/// A loaded rules file.
#[derive(Debug, Clone)]
pub struct DataRules {
    pub file: PathBuf,
    /// blake3 of the file's bytes, hex. Recorded on the verdict so a change
    /// to the rules is visible in the instance status, and so an
    /// acknowledgement given under one set of rules does not carry over to
    /// another.
    pub hash: String,
    pub rules: Vec<DataRule>,
}

/// What a scan saw, without anything it saw.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanOutcome {
    pub findings: Vec<Finding>,
    /// Sampled values looked at.
    pub scanned_values: usize,
    /// Values longer than [`MATCH_VALUE_CAP`] whose tail was not looked at.
    pub truncated_values: usize,
}

impl DataRules {
    /// Read and parse the rules file at `path`.
    pub fn load(path: &Path) -> Result<Self, RulesError> {
        let err = |kind| RulesError {
            file: path.to_path_buf(),
            rule: None,
            index: None,
            kind,
        };
        let meta = std::fs::metadata(path).map_err(|e| err(RulesErrorKind::Io(e.to_string())))?;
        if meta.len() > RULES_FILE_CAP as u64 {
            return Err(err(RulesErrorKind::TooLarge {
                bytes: meta.len() as usize,
            }));
        }
        let text =
            std::fs::read_to_string(path).map_err(|e| err(RulesErrorKind::Io(e.to_string())))?;
        Self::parse(&text, path)
    }

    /// Parse rules text; `file` is only used to name it in errors.
    pub fn parse(text: &str, file: impl Into<PathBuf>) -> Result<Self, RulesError> {
        let file = file.into();
        let top_err = |kind| RulesError {
            file: file.clone(),
            rule: None,
            index: None,
            kind,
        };
        if text.len() > RULES_FILE_CAP {
            return Err(top_err(RulesErrorKind::TooLarge { bytes: text.len() }));
        }
        let hash = blake3::hash(text.as_bytes()).to_hex().to_string();
        let doc: Value =
            serde_yaml::from_str(text).map_err(|e| top_err(RulesErrorKind::Yaml(e.to_string())))?;
        let Value::Mapping(top) = doc else {
            return Err(top_err(RulesErrorKind::Shape(
                "expected a mapping with a `rules:` list".into(),
            )));
        };
        let mut list = None;
        for (k, v) in &top {
            match k.as_str() {
                Some("rules") => list = Some(v),
                Some(other) => {
                    return Err(top_err(RulesErrorKind::UnknownTopLevelKey(other.into())))
                }
                None => {
                    return Err(top_err(RulesErrorKind::Shape(
                        "top-level keys must be strings".into(),
                    )))
                }
            }
        }
        let items: &[Value] = match list {
            None => {
                return Err(top_err(RulesErrorKind::Shape(
                    "missing `rules:` (an empty file still needs `rules:`)".into(),
                )))
            }
            // `rules:` with every entry commented out
            Some(Value::Null) => &[],
            Some(Value::Sequence(s)) => s,
            Some(_) => {
                return Err(top_err(RulesErrorKind::Shape(
                    "`rules` must be a list".into(),
                )))
            }
        };
        if items.len() > MAX_RULES {
            return Err(top_err(RulesErrorKind::TooManyRules { rules: items.len() }));
        }
        let mut rules: Vec<DataRule> = Vec::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            let rule = parse_rule(item, index, &file)?;
            if rules.iter().any(|r| r.id == rule.id) {
                return Err(RulesError {
                    file,
                    rule: Some(rule.id),
                    index: Some(index),
                    kind: RulesErrorKind::DuplicateId,
                });
            }
            rules.push(rule);
        }
        Ok(DataRules { file, hash, rules })
    }

    pub fn rule(&self, id: &str) -> Option<&DataRule> {
        self.rules.iter().find(|r| r.id == id)
    }

    /// Run every rule over `rows`. Input over [`SAMPLE_VALUE_CAP`] values or
    /// [`SAMPLE_BYTE_CAP`] bytes is refused whole, never partly scanned, and
    /// so is a row whose table or column name could not be shown safely.
    pub fn scan(&self, rows: &[SampleRow]) -> Result<ScanOutcome, SafetyError> {
        check_caps(rows)?;
        for (i, row) in rows.iter().enumerate() {
            check_name("table", &row.table)
                .map_err(|m| SafetyError::Hook(format!("row {i}: {m}")))?;
            check_name("column", &row.column)
                .map_err(|m| SafetyError::Hook(format!("row {i}: {m}")))?;
        }
        let mut truncated_values = 0;
        let clipped: Vec<Vec<&str>> = rows
            .iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(|v| {
                        let c = clip(v);
                        if c.len() < v.len() {
                            truncated_values += 1;
                        }
                        c
                    })
                    .collect()
            })
            .collect();
        let mut tally = Tally::new();
        for rule in &self.rules {
            for (row, values) in rows.iter().zip(&clipped) {
                if !rule.in_scope(&row.table, &row.column) {
                    continue;
                }
                let location = Location::Data {
                    table: row.table.clone(),
                    column: row.column.clone(),
                };
                for value in values {
                    if rule.matches(value) {
                        tally.record(&rule.id, location.clone());
                    }
                }
            }
        }
        let findings = tally.findings(
            |id| self.rule(id).map_or(1, |r| r.threshold),
            |id| self.rule(id).map_or(Severity::Bad, |r| r.severity),
        );
        Ok(ScanOutcome {
            findings,
            scanned_values: clipped.iter().map(Vec::len).sum(),
            truncated_values,
        })
    }
}

impl FromStr for DataRules {
    type Err = RulesError;

    /// Parse rules text that has no file behind it (named `<inline>`).
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text, "<inline>")
    }
}

fn parse_rule(item: &Value, index: usize, file: &Path) -> Result<DataRule, RulesError> {
    let mut rule_id: Option<String> = None;
    let err = |rule_id: &Option<String>, kind| RulesError {
        file: file.to_path_buf(),
        rule: rule_id.clone(),
        index: Some(index),
        kind,
    };
    let Value::Mapping(map) = item else {
        return Err(err(
            &rule_id,
            RulesErrorKind::Shape("each rule must be a mapping".into()),
        ));
    };
    // The id first, so every later error can name the rule.
    match map.get("id") {
        None => return Err(err(&rule_id, RulesErrorKind::MissingId)),
        Some(Value::String(s)) if is_valid_rule_id(s) => rule_id = Some(s.clone()),
        Some(_) => return Err(err(&rule_id, RulesErrorKind::InvalidId)),
    }
    for k in map.keys() {
        match k.as_str() {
            Some(k) if RULE_KEYS.contains(&k) => {}
            Some(k) => return Err(err(&rule_id, RulesErrorKind::UnknownKey(k.into()))),
            None => {
                return Err(err(
                    &rule_id,
                    RulesErrorKind::Shape("rule keys must be strings".into()),
                ))
            }
        }
    }
    let wrong = |key: &str, expected| RulesErrorKind::WrongType {
        key: key.into(),
        expected,
    };
    let string = |key: &str| -> Result<Option<&str>, RulesError> {
        match map.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.as_str())),
            Some(_) => Err(err(&rule_id, wrong(key, "a string"))),
        }
    };
    let list = |key: &'static str| -> Result<Vec<&str>, RulesError> {
        match map.get(key) {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::Sequence(items)) => {
                if items.len() > MAX_LIST_ENTRIES {
                    return Err(err(
                        &rule_id,
                        RulesErrorKind::TooManyEntries {
                            key,
                            entries: items.len(),
                        },
                    ));
                }
                items
                    .iter()
                    .map(|v| {
                        v.as_str()
                            .ok_or_else(|| err(&rule_id, wrong(key, "a list of strings")))
                    })
                    .collect()
            }
            Some(_) => Err(err(&rule_id, wrong(key, "a list of strings"))),
        }
    };

    let compile = |key: String, source: &str| -> Result<Regex, RulesError> {
        compile_rule_regex(&key, source).map_err(|kind| err(&rule_id, kind))
    };

    let pattern = string("pattern")?
        .map(|p| compile("pattern".into(), p))
        .transpose()?;
    let allow = list("allow")?
        .into_iter()
        .enumerate()
        .map(|(i, p)| compile(format!("allow[{i}]"), p))
        .collect::<Result<Vec<_>, _>>()?;
    let columns = list("columns")?
        .into_iter()
        .enumerate()
        .map(|(i, p)| compile(format!("columns[{i}]"), &format!("^(?:{p})$")))
        .collect::<Result<Vec<_>, _>>()?;
    let validate = match string("validate")? {
        None => None,
        Some(name) => Some(
            Validator::from_name(name)
                .ok_or_else(|| err(&rule_id, RulesErrorKind::UnknownValidator(name.into())))?,
        ),
    };
    let severity = match string("severity")? {
        None | Some("bad") => Severity::Bad,
        Some("warn") => Severity::Warn,
        Some(other) => return Err(err(&rule_id, RulesErrorKind::UnknownSeverity(other.into()))),
    };
    let threshold = match map.get("threshold") {
        None | Some(Value::Null) => 1,
        Some(Value::Number(n)) => match n.as_u64() {
            Some(0) => return Err(err(&rule_id, RulesErrorKind::ZeroThreshold)),
            Some(t) if t <= u64::from(u32::MAX) => t as u32,
            _ => return Err(err(&rule_id, wrong("threshold", "a positive whole number"))),
        },
        Some(_) => return Err(err(&rule_id, wrong("threshold", "a positive whole number"))),
    };
    if pattern.is_none() && validate.is_none() {
        return Err(err(&rule_id, RulesErrorKind::NeedsPatternOrValidate));
    }
    Ok(DataRule {
        id: rule_id.unwrap_or_default(),
        pattern,
        allow,
        columns,
        threshold,
        validate,
        severity,
    })
}

fn compile_rule_regex(key: &str, source: &str) -> Result<Regex, RulesErrorKind> {
    RegexBuilder::new(source)
        .size_limit(REGEX_SIZE_LIMIT)
        .dfa_size_limit(REGEX_SIZE_LIMIT)
        .build()
        .map_err(|e| match e {
            regex::Error::CompiledTooBig(_) => RulesErrorKind::RegexTooLarge { key: key.into() },
            regex::Error::Syntax(detail) => {
                if uses_lookaround_or_backref(source) {
                    RulesErrorKind::UnsupportedSyntax { key: key.into() }
                } else {
                    RulesErrorKind::BadRegex {
                        key: key.into(),
                        detail,
                    }
                }
            }
            other => RulesErrorKind::BadRegex {
                key: key.into(),
                detail: other.to_string(),
            },
        })
}

/// Whether a pattern that failed to compile used syntax a backtracking
/// engine has and this one deliberately lacks, so the error can say what
/// to do instead of only quoting the parser.
fn uses_lookaround_or_backref(source: &str) -> bool {
    let bytes = source.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                if let Some(next) = bytes.get(i + 1) {
                    if (next.is_ascii_digit() && *next != b'0') || *next == b'k' {
                        return true;
                    }
                }
                i += 2;
            }
            b'(' => {
                let rest = &source[i..];
                if ["(?=", "(?!", "(?<=", "(?<!"]
                    .iter()
                    .any(|p| rest.starts_with(p))
                {
                    return true;
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    false
}

fn is_valid_rule_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_RULE_ID_LEN
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// A table or column name that may be shown and stored. The hook chooses
/// these, so they are bounded and kept free of control characters; the
/// message never repeats the name.
fn check_name(what: &str, name: &str) -> Result<(), String> {
    if name.trim().is_empty() {
        Err(format!("{what} name is empty"))
    } else if name.chars().count() > MAX_NAME_LEN {
        Err(format!(
            "{what} name is longer than {MAX_NAME_LEN} characters"
        ))
    } else if name.chars().any(char::is_control) {
        Err(format!("{what} name contains control characters"))
    } else {
        Ok(())
    }
}

fn check_caps(rows: &[SampleRow]) -> Result<(), SafetyError> {
    let values: usize = rows.iter().map(|r| r.values.len()).sum();
    if values > SAMPLE_VALUE_CAP {
        return Err(SafetyError::TooManyValues { values });
    }
    let bytes: usize = rows
        .iter()
        .map(|r| r.table.len() + r.column.len() + r.values.iter().map(String::len).sum::<usize>())
        .sum();
    if bytes > SAMPLE_BYTE_CAP {
        return Err(SafetyError::TooManyBytes { bytes });
    }
    Ok(())
}

/// The first [`MATCH_VALUE_CAP`] bytes of `value`, cut on a character
/// boundary.
fn clip(value: &str) -> &str {
    if value.len() <= MATCH_VALUE_CAP {
        return value;
    }
    let mut end = MATCH_VALUE_CAP;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

/// Check findings a hook computed itself before they are stored: a data
/// location (a source location from a data hook is a hook bug), a rule id
/// and names that pass the same checks as the engine's own, and at most
/// [`MAX_HOOK_FINDINGS`]. Zero-count entries are dropped (they found
/// nothing); repeated (rule, table, column) entries are merged, counts
/// added and the worse severity kept, so each place appears once.
pub fn accept_hook_findings(findings: Vec<Finding>) -> Result<Vec<Finding>, SafetyError> {
    if findings.len() > MAX_HOOK_FINDINGS {
        return Err(SafetyError::Hook(format!(
            "{} findings, over the cap of {MAX_HOOK_FINDINGS}",
            findings.len()
        )));
    }
    let mut out: Vec<Finding> = Vec::new();
    for (i, f) in findings.into_iter().enumerate() {
        let bad = |m: String| SafetyError::Hook(format!("finding {i}: {m}"));
        if !is_valid_rule_id(&f.rule) {
            return Err(bad(format!(
                "rule id must be 1-{MAX_RULE_ID_LEN} letters, digits, `.`, `_` or `-`"
            )));
        }
        match &f.location {
            Location::Data { table, column } => {
                check_name("table", table).map_err(bad)?;
                check_name("column", column).map_err(bad)?;
            }
            Location::Source { .. } => {
                return Err(bad("a data check must report a table and column".into()))
            }
        }
        if f.count == 0 {
            continue;
        }
        if let Some(prev) = out
            .iter_mut()
            .find(|p| p.rule == f.rule && p.location == f.location)
        {
            prev.count = prev.count.saturating_add(f.count);
            prev.severity = prev.severity.max(f.severity);
        } else {
            out.push(f);
        }
    }
    Ok(out)
}

// ----------------------------------------------------------------- verdict

/// What a hook produced, as the runner saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookRun<'a> {
    /// The component declares no such hook.
    NotDefined,
    /// The hook could not be run, exited non-zero or timed out. Why is the
    /// runner's to log; it is not kept here because hook error output may
    /// quote data.
    Failed,
    /// It ran; this is its stdout.
    Stdout(&'a str),
}

/// Why the data counts as unscrubbed according to its marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UnscrubbedReason {
    /// The hook printed nothing or failed: no proof the scrub ran.
    Missing,
    /// The hook said `scrubbed: false`.
    NotScrubbed,
    /// The hook printed something that is not a marker.
    Unparsable,
}

/// The marker's part of a verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum MarkerVerdict {
    NotDefined,
    Scrubbed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at: Option<String>,
    },
    /// Treated as unscrubbed whatever the sample says.
    Unscrubbed {
        reason: UnscrubbedReason,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at: Option<String>,
    },
    /// The marker hook printed `{findings: [...]}`: the user wrote the whole
    /// check there. Its findings are in the verdict; it attests nothing.
    HookFindings,
}

impl MarkerVerdict {
    pub fn is_unscrubbed(&self) -> bool {
        matches!(self, MarkerVerdict::Unscrubbed { .. })
    }

    /// The scrub version an attestation named, for the instance status.
    pub fn version(&self) -> Option<&str> {
        match self {
            MarkerVerdict::Scrubbed { version, .. } | MarkerVerdict::Unscrubbed { version, .. } => {
                version.as_deref()
            }
            _ => None,
        }
    }
}

/// Longest marker `version` / `at` kept. They are labels, not data; a
/// longer one is cut so a hook cannot park a value in the status.
const MARKER_FIELD_CAP: usize = 64;

fn marker_field(s: Option<String>) -> Option<String> {
    s.map(|s| {
        s.chars()
            .filter(|c| !c.is_control())
            .take(MARKER_FIELD_CAP)
            .collect()
    })
}

/// Read a `marker` hook's result. Missing, failed, `scrubbed: false` and
/// unparsable all mean unscrubbed. A `{findings: [...]}` object is the
/// user's own check and is returned for the verdict to merge.
pub fn evaluate_marker(run: HookRun<'_>) -> (MarkerVerdict, Result<Vec<Finding>, SafetyError>) {
    let unscrubbed = |reason| MarkerVerdict::Unscrubbed {
        reason,
        version: None,
        at: None,
    };
    let stdout = match run {
        HookRun::NotDefined => return (MarkerVerdict::NotDefined, Ok(Vec::new())),
        HookRun::Failed => return (unscrubbed(UnscrubbedReason::Missing), Ok(Vec::new())),
        HookRun::Stdout(s) if s.trim().is_empty() => {
            return (unscrubbed(UnscrubbedReason::Missing), Ok(Vec::new()))
        }
        HookRun::Stdout(s) => s,
    };
    match parse_marker(stdout) {
        Ok(MarkerReport {
            scrubbed,
            version,
            at,
        }) => {
            let (version, at) = (marker_field(version), marker_field(at));
            let v = if scrubbed {
                MarkerVerdict::Scrubbed { version, at }
            } else {
                MarkerVerdict::Unscrubbed {
                    reason: UnscrubbedReason::NotScrubbed,
                    version,
                    at,
                }
            };
            (v, Ok(Vec::new()))
        }
        Err(_) => {
            #[derive(Deserialize)]
            struct Findings {
                findings: Vec<Finding>,
            }
            if stdout.len() <= SAMPLE_BYTE_CAP {
                if let Ok(Findings { findings }) = serde_json::from_str::<Findings>(stdout) {
                    return (MarkerVerdict::HookFindings, accept_hook_findings(findings));
                }
            }
            (unscrubbed(UnscrubbedReason::Unparsable), Ok(Vec::new()))
        }
    }
}

/// The sample's part of a verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum SampleStatus {
    NotDefined,
    /// Rows were scanned with the rules.
    Scanned,
    /// The hook reported findings itself.
    HookFindings,
    /// Rows arrived but no rules file is configured, so nothing could be
    /// detected. Not clean: the check the user declared did not happen.
    NoRules,
    /// The output was refused: over a cap, not JSON of a known shape, a bad
    /// name or a bad hook finding. `reason` is the error's text, which never
    /// quotes what was read.
    Refused {
        reason: String,
    },
    /// The hook could not be run.
    Failed,
}

impl SampleStatus {
    fn is_ok(&self) -> bool {
        matches!(
            self,
            SampleStatus::NotDefined | SampleStatus::Scanned | SampleStatus::HookFindings
        )
    }

    fn key(&self) -> &'static str {
        match self {
            SampleStatus::NotDefined => "not-defined",
            SampleStatus::Scanned => "scanned",
            SampleStatus::HookFindings => "hook-findings",
            SampleStatus::NoRules => "no-rules",
            SampleStatus::Refused { .. } => "refused",
            SampleStatus::Failed => "failed",
        }
    }
}

/// One data-safety check of one component, as later phases store it on the
/// instance and on each saved state. Counts and statuses only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataVerdict {
    /// No findings, the marker (if any) attests a scrub, and the sample (if
    /// any) was actually checked.
    pub clean: bool,
    pub findings: Vec<Finding>,
    pub marker: MarkerVerdict,
    pub sample: SampleStatus,
    /// [`DataRules::hash`] of the rules the sample was checked with.
    pub rules_hash: Option<String>,
    pub scanned_values: usize,
    pub truncated_values: usize,
}

impl DataVerdict {
    /// What an acknowledgement of this verdict is tied to: every finding's
    /// [`Finding::key`] with its count (`rule@table.column#count`), a
    /// `marker:` / `sample:` entry for a marker or sample problem, and the
    /// rules hash. Sorted, no repeats. Empty when clean.
    ///
    /// Counts are part of the key because a different count is different
    /// data: a restore of real data can add matches to a column that held a
    /// few known fixtures, and it can just as well replace them with fewer
    /// matches that are all real. Neither direction is safe to wave through
    /// on an earlier decision.
    pub fn ack_keys(&self) -> Vec<String> {
        if self.clean {
            return Vec::new();
        }
        let mut keys: Vec<String> = self
            .findings
            .iter()
            .map(|f| format!("{}#{}", f.key(), f.count))
            .collect();
        if let MarkerVerdict::Unscrubbed { reason, .. } = &self.marker {
            let r = match reason {
                UnscrubbedReason::Missing => "missing",
                UnscrubbedReason::NotScrubbed => "not-scrubbed",
                UnscrubbedReason::Unparsable => "unparsable",
            };
            keys.push(format!("marker:{r}"));
        }
        if !self.sample.is_ok() {
            keys.push(format!("sample:{}", self.sample.key()));
        }
        keys.push(format!(
            "rules:{}",
            self.rules_hash.as_deref().unwrap_or("none")
        ));
        keys.sort();
        keys.dedup();
        keys
    }
}

/// Combine a component's marker and sample results into one verdict.
/// `None` when the component declares neither hook: that is "unscanned",
/// which has its own policy (see [`enforcement`]). `rules` is the loaded
/// rules file, if the stack names one.
pub fn evaluate(
    rules: Option<&DataRules>,
    marker: HookRun<'_>,
    sample: HookRun<'_>,
) -> Option<DataVerdict> {
    if marker == HookRun::NotDefined && sample == HookRun::NotDefined {
        return None;
    }
    let mut findings = Vec::new();
    let (mut marker_verdict, marker_findings) = evaluate_marker(marker);
    match marker_findings {
        Ok(f) => findings.extend(f),
        // A findings object that fails validation proves nothing either way.
        Err(_) => {
            marker_verdict = MarkerVerdict::Unscrubbed {
                reason: UnscrubbedReason::Unparsable,
                version: None,
                at: None,
            }
        }
    }
    let (mut scanned_values, mut truncated_values) = (0, 0);
    let sample_status = match sample {
        HookRun::NotDefined => SampleStatus::NotDefined,
        HookRun::Failed => SampleStatus::Failed,
        HookRun::Stdout(stdout) => match parse_sample(stdout) {
            Err(e) => SampleStatus::Refused {
                reason: e.to_string(),
            },
            Ok(HookOutput::Findings { findings: hook }) => match accept_hook_findings(hook) {
                Ok(f) => {
                    findings.extend(f);
                    SampleStatus::HookFindings
                }
                Err(e) => SampleStatus::Refused {
                    reason: e.to_string(),
                },
            },
            Ok(HookOutput::Rows(rows)) => match rules {
                None => SampleStatus::NoRules,
                Some(rules) => match rules.scan(&rows) {
                    Ok(outcome) => {
                        scanned_values = outcome.scanned_values;
                        truncated_values = outcome.truncated_values;
                        findings.extend(outcome.findings);
                        SampleStatus::Scanned
                    }
                    Err(e) => SampleStatus::Refused {
                        reason: e.to_string(),
                    },
                },
            },
        },
    };
    let clean = findings.is_empty() && !marker_verdict.is_unscrubbed() && sample_status.is_ok();
    Some(DataVerdict {
        clean,
        findings,
        marker: marker_verdict,
        sample: sample_status,
        rules_hash: rules.map(|r| r.hash.clone()),
        scanned_values,
        truncated_values,
    })
}

// ------------------------------------------------------------------ policy

/// Why [`enforcement`] decided what it did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EnforcementCause {
    /// The last check was clean.
    Clean,
    /// Not clean, but an acknowledgement covers exactly this verdict.
    Acknowledged,
    /// Findings, an unscrubbed marker, or a sample that was not checked.
    NotClean,
    /// The component declares no check.
    Unscanned,
}

/// What to enforce for one component.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataEnforcement {
    /// `None`: nothing to do. Otherwise always a policy
    /// [`Policy::allowed_for_data`] accepts.
    pub policy: Option<Policy>,
    pub cause: EnforcementCause,
}

/// The `onFinding` policy in force. A policy data safety does not accept
/// (`block-build`) is refused when the definition is validated; should one
/// reach here it is read as the default, `block-agents`.
pub fn on_finding_policy(def: Option<&DataSafetyDef>) -> Policy {
    match def.map(|d| d.on_finding) {
        Some(p) if p.allowed_for_data() => p,
        _ => Policy::BlockAgents,
    }
}

/// The `unscanned` policy in force: `warn` or `block-agents`. Anything
/// stronger is refused at validation and read as `block-agents` here, the
/// strongest one allowed, since the user asked for more than a warning.
pub fn unscanned_policy(def: Option<&DataSafetyDef>) -> Policy {
    match def.map(|d| d.unscanned) {
        None | Some(Policy::Warn) => Policy::Warn,
        Some(_) => Policy::BlockAgents,
    }
}

/// Decide what to enforce for a component from its stack's `dataSafety`
/// settings, its last verdict (`None`: it declares no check) and the
/// acknowledgement on record, if any. An acknowledgement lifts a block only
/// when it covers this exact verdict ([`ack_clears`]); it never applies to
/// an unscanned component, which has nothing to acknowledge.
pub fn enforcement(
    def: Option<&DataSafetyDef>,
    verdict: Option<&DataVerdict>,
    ack: Option<&Acknowledgement>,
) -> DataEnforcement {
    match verdict {
        None => DataEnforcement {
            policy: Some(unscanned_policy(def)),
            cause: EnforcementCause::Unscanned,
        },
        Some(v) if v.clean => DataEnforcement {
            policy: None,
            cause: EnforcementCause::Clean,
        },
        Some(v) if ack.is_some_and(|a| ack_clears(a, v)) => DataEnforcement {
            policy: None,
            cause: EnforcementCause::Acknowledged,
        },
        Some(_) => DataEnforcement {
            policy: Some(on_finding_policy(def)),
            cause: EnforcementCause::NotClean,
        },
    }
}

/// Record an acknowledgement of `verdict` (the UI confirms first). It covers
/// the verdict's [`DataVerdict::ack_keys`], so it is tied to these findings
/// at these exact counts, this marker state and these rules.
pub fn acknowledge(verdict: &DataVerdict, who: &str, why: &str, at_utc: &str) -> Acknowledgement {
    Acknowledgement {
        who: who.into(),
        why: why.into(),
        at_utc: at_utc.into(),
        covers: verdict.ack_keys(),
    }
}

/// Whether `ack` was given for exactly this verdict, so it lifts the block.
/// The covered keys must equal the verdict's [`DataVerdict::ack_keys`]: a
/// key the ack lacks (a new finding, a changed count, a marker or rules
/// change) and a key the verdict lacks (a vanished finding) both mean the
/// data is not what was looked at. A clean verdict has nothing to clear.
pub fn ack_clears(ack: &Acknowledgement, verdict: &DataVerdict) -> bool {
    if verdict.clean {
        return true;
    }
    let mut covers = ack.covers.clone();
    covers.sort();
    covers.dedup();
    covers == verdict.ack_keys()
}

/// Whether an acknowledgement stays on record after a rescan produced
/// `next`. It holds only while the rescan reports exactly what was
/// acknowledged: the same findings at the same counts, the same marker and
/// sample state, the same rules. Any count change, up or down, ends it, as
/// does a finding added or gone, a marker change or a rules change; a
/// clean rescan ends it too (a later problem needs a fresh look). A
/// periodic check that finds the data unchanged keeps it, so a decision
/// already made is not re-raised.
pub fn ack_survives(ack: &Acknowledgement, next: &DataVerdict) -> bool {
    !next.clean && ack_clears(ack, next)
}
