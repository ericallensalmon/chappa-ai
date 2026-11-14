//! Reading scanner reports, and dropping every value on the way in.
//!
//! Two shapes are understood: the gitleaks JSON report (an array of
//! objects) and SARIF 2.1 (`runs[].results[]`). From either, exactly four
//! things are taken per hit: the rule id, the file, the start line and the
//! commit. The report types below declare only those fields, so `Secret`,
//! `Match`, `Line`, `Fingerprint`, snippets, messages, hashes and anything a
//! scanner adds later are skipped by the parser and never reach a value we
//! own. Even the kept fields are checked: a "commit" that is not a hex id
//! and a "rule" with characters no rule id uses are dropped or replaced,
//! because a misbehaving scanner could put anything there.
//!
//! Parse errors are reported by category and position only. A JSON
//! library's own message can quote the offending text, which here may be a
//! credential.

use super::listing::FileList;
use crate::safety::{Finding, Location, SafetyError, Severity, Tally};
use serde::de::IgnoredAny;
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Rule id used when a scanner gave none, or one that does not look like
/// a rule id.
pub const UNNAMED_RULE: &str = "unnamed-rule";

/// One hit, before aggregation. Carries no value by construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Hit {
    pub rule: String,
    pub file: String,
    pub line: Option<u32>,
    pub commit: Option<String>,
}

/// A field kept only when it has the expected type; anything else is
/// skipped without being kept or quoted.
#[derive(Deserialize)]
#[serde(untagged)]
enum Lenient<T> {
    Value(T),
    Other(IgnoredAny),
}

impl<T> Lenient<T> {
    fn get(self) -> Option<T> {
        match self {
            Lenient::Value(v) => Some(v),
            Lenient::Other(_) => None,
        }
    }
}

fn lenient<T>(v: Option<Lenient<T>>) -> Option<T> {
    v.and_then(Lenient::get)
}

#[derive(Deserialize)]
struct GitleaksEntry {
    #[serde(rename = "RuleID")]
    rule_id: Option<Lenient<String>>,
    #[serde(rename = "File")]
    file: Option<Lenient<String>>,
    #[serde(rename = "StartLine")]
    start_line: Option<Lenient<u64>>,
    #[serde(rename = "Commit")]
    commit: Option<Lenient<String>>,
}

#[derive(Deserialize)]
struct Sarif {
    runs: Vec<SarifRun>,
}

#[derive(Deserialize)]
struct SarifRun {
    #[serde(default)]
    results: Option<Vec<SarifResult>>,
}

#[derive(Deserialize)]
struct SarifResult {
    #[serde(rename = "ruleId")]
    rule_id: Option<Lenient<String>>,
    rule: Option<SarifRuleRef>,
    #[serde(default)]
    locations: Option<Vec<SarifLocation>>,
    #[serde(rename = "partialFingerprints")]
    partial_fingerprints: Option<SarifPartial>,
}

#[derive(Deserialize)]
struct SarifRuleRef {
    id: Option<Lenient<String>>,
}

#[derive(Deserialize)]
struct SarifPartial {
    #[serde(rename = "commitSha")]
    commit_sha: Option<Lenient<String>>,
}

#[derive(Deserialize)]
struct SarifLocation {
    #[serde(rename = "physicalLocation")]
    physical: Option<SarifPhysical>,
}

#[derive(Deserialize)]
struct SarifPhysical {
    #[serde(rename = "artifactLocation")]
    artifact: Option<SarifArtifact>,
    region: Option<SarifRegion>,
}

#[derive(Deserialize)]
struct SarifArtifact {
    uri: Option<Lenient<String>>,
}

#[derive(Deserialize)]
struct SarifRegion {
    #[serde(rename = "startLine")]
    start_line: Option<Lenient<u64>>,
}

/// A JSON error without the text it choked on.
pub(crate) fn json_error(what: &str, e: &serde_json::Error) -> SafetyError {
    SafetyError::Scanner(format!("{what}: {}", crate::safety::json_brief(e)))
}

fn clean_rule(rule: Option<String>) -> String {
    match rule {
        Some(r)
            if !r.is_empty()
                && r.len() <= 100
                && r.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.:/".contains(c)) =>
        {
            r
        }
        _ => UNNAMED_RULE.to_string(),
    }
}

fn clean_commit(commit: Option<String>) -> Option<String> {
    commit.filter(|c| (7..=64).contains(&c.len()) && c.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn clean_line(line: Option<u64>) -> Option<u32> {
    line.filter(|l| *l > 0).and_then(|l| u32::try_from(l).ok())
}

/// Decode `%XX` escapes in a URI path. Malformed escapes stay literal.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(b) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A reported path as a `/`-separated path relative to `root` when it lies
/// under it, otherwise as reported (with `/` separators).
pub(crate) fn relative_to(root: &Path, reported: &str) -> String {
    let mut p = reported.replace('\\', "/");
    if let Some(rest) = p.strip_prefix("file://") {
        let rest = rest.strip_prefix("localhost").unwrap_or(rest);
        // `file:///C:/x` names a drive path; drop the slash before it
        let rest = match rest.as_bytes() {
            [b'/', d, b':', ..] if d.is_ascii_alphabetic() => &rest[1..],
            _ => rest,
        };
        p = percent_decode(rest);
    }
    while let Some(rest) = p.strip_prefix("./") {
        p = rest.to_string();
    }
    let root_s = root.to_string_lossy().replace('\\', "/");
    let root_s = root_s.trim_end_matches('/');
    if !root_s.is_empty() {
        let lower_p = p.to_ascii_lowercase();
        let lower_root = root_s.to_ascii_lowercase();
        let same_case = p.starts_with(root_s);
        // Windows paths compare without case; elsewhere case must match.
        if same_case || (cfg!(windows) && lower_p.starts_with(&lower_root)) {
            let rest = &p[root_s.len()..];
            if let Some(rel) = rest.strip_prefix('/') {
                return rel.to_string();
            }
        }
    }
    p
}

/// Parse a gitleaks JSON report. `root` makes absolute paths relative.
pub(crate) fn parse_gitleaks(text: &str, root: &Path) -> Result<Vec<Hit>, SafetyError> {
    let entries: Option<Vec<GitleaksEntry>> =
        serde_json::from_str(text).map_err(|e| json_error("gitleaks report", &e))?;
    Ok(entries
        .unwrap_or_default()
        .into_iter()
        .map(|e| Hit {
            rule: clean_rule(lenient(e.rule_id)),
            file: lenient(e.file)
                .map(|f| relative_to(root, &f))
                .unwrap_or_default(),
            line: clean_line(lenient(e.start_line)),
            commit: clean_commit(lenient(e.commit)),
        })
        .collect())
}

/// Parse a SARIF log.
pub(crate) fn parse_sarif(text: &str, root: &Path) -> Result<Vec<Hit>, SafetyError> {
    let log: Sarif = serde_json::from_str(text).map_err(|e| json_error("SARIF report", &e))?;
    let mut hits = Vec::new();
    for run in log.runs {
        for result in run.results.unwrap_or_default() {
            let rule = clean_rule(
                lenient(result.rule_id).or_else(|| result.rule.and_then(|r| lenient(r.id))),
            );
            let commit = clean_commit(
                result
                    .partial_fingerprints
                    .and_then(|p| lenient(p.commit_sha)),
            );
            let locations = result.locations.unwrap_or_default();
            let mut placed = false;
            for loc in locations {
                let Some(phys) = loc.physical else { continue };
                let file = phys
                    .artifact
                    .and_then(|a| lenient(a.uri))
                    .map(|u| relative_to(root, &u))
                    .unwrap_or_default();
                let line = clean_line(phys.region.and_then(|r| lenient(r.start_line)));
                hits.push(Hit {
                    rule: rule.clone(),
                    file,
                    line,
                    commit: commit.clone(),
                });
                placed = true;
            }
            if !placed {
                hits.push(Hit {
                    rule,
                    file: String::new(),
                    line: None,
                    commit,
                });
            }
        }
    }
    Ok(hits)
}

/// Parse either shape: an object with `runs` is SARIF, an array (or
/// `null`, an empty gitleaks report) is gitleaks JSON.
pub(crate) fn parse_any(text: &str, root: &Path) -> Result<Vec<Hit>, SafetyError> {
    match text.trim_start().as_bytes().first() {
        Some(b'{') => parse_sarif(text, root),
        Some(b'[') | Some(b'n') => parse_gitleaks(text, root),
        Some(_) => Err(SafetyError::Scanner(
            "scanner output is neither a gitleaks JSON report nor SARIF".into(),
        )),
        None => Err(SafetyError::Scanner("scanner printed nothing".into())),
    }
}

/// Fold hits into findings: one per rule and place, counted, in a stable
/// order (file, line, commit, rule).
pub(crate) fn findings(hits: Vec<Hit>) -> Vec<Finding> {
    let mut tally = Tally::new();
    for h in hits {
        tally.record(
            &h.rule,
            Location::Source {
                file: PathBuf::from(h.file),
                line: h.line,
                commit: h.commit,
            },
        );
    }
    let mut out = tally.findings(|_| 1, |_| Severity::Bad);
    out.sort_by_key(sort_key);
    out
}

fn sort_key(f: &Finding) -> (String, u32, String, String) {
    match &f.location {
        Location::Source { file, line, commit } => (
            file.to_string_lossy().into_owned(),
            line.unwrap_or(0),
            commit.clone().unwrap_or_default(),
            f.rule.clone(),
        ),
        Location::Data { table, column } => (
            format!("{table}.{column}"),
            0,
            String::new(),
            f.rule.clone(),
        ),
    }
}

/// Keep only findings whose file was in the scanned list. A directory-mode
/// scanner that reads more than it was given (or a hook that ignores the
/// list) cannot widen the scan's scope this way.
pub(crate) fn within(findings: Vec<Finding>, list: &FileList) -> Vec<Finding> {
    findings
        .into_iter()
        .filter(|f| match &f.location {
            Location::Source { file, .. } => {
                let p = file.to_string_lossy();
                list.files.iter().any(|l| l.path == p)
            }
            Location::Data { .. } => false,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Assembled at run time so this file never holds a token-shaped string.
    fn planted() -> String {
        format!("ghp_{}", "Qz7Wx2Lp9Rt4Vm6Nk1Bh8Cd3Fs5Gj0Hy7Ue2T")
    }

    fn src(file: &str, line: Option<u32>, commit: Option<&str>) -> Location {
        Location::Source {
            file: PathBuf::from(file),
            line,
            commit: commit.map(str::to_string),
        }
    }

    #[test]
    fn gitleaks_report_keeps_four_fields_and_drops_values() {
        let v = planted();
        // an unredacted report, as a scanner that ignored --redact would print
        let text = format!(
            r#"[{{"RuleID":"github-pat","Description":"d {v}","StartLine":3,"EndLine":3,
                "StartColumn":1,"EndColumn":40,"Match":"token={v}","Secret":"{v}",
                "Line":"export TOKEN={v}","File":"config/.env","SymlinkFile":"",
                "Commit":"","Entropy":4.5,"Author":"a","Email":"a@example.test",
                "Date":"","Message":"add {v}","Tags":["{v}"],"Fingerprint":"config/.env:{v}:3"}},
               {{"RuleID":"github-pat","StartLine":3,"File":"config/.env","Secret":"{v}"}},
               {{"RuleID":"generic-api-key","StartLine":9,"File":"b.txt",
                 "Commit":"0123456789abcdef0123456789abcdef01234567"}}]"#
        );
        let found = findings(parse_any(&text, Path::new("/nowhere")).unwrap());
        assert_eq!(
            found,
            [
                Finding {
                    rule: "generic-api-key".into(),
                    location: src(
                        "b.txt",
                        Some(9),
                        Some("0123456789abcdef0123456789abcdef01234567")
                    ),
                    count: 1,
                    severity: Severity::Bad,
                },
                Finding {
                    rule: "github-pat".into(),
                    location: src("config/.env", Some(3), None),
                    count: 2,
                    severity: Severity::Bad,
                },
            ]
        );
        let debug = format!("{found:?}");
        let json = serde_json::to_string(&found).unwrap();
        for out in [&debug, &json] {
            assert!(!out.contains(&v), "value leaked: {out}");
            assert!(!out.contains("Qz7Wx2"), "partial value leaked: {out}");
        }
    }

    #[test]
    fn kept_fields_are_checked_too() {
        let v = planted();
        let text = format!(
            r#"[{{"RuleID":"{v} oops","StartLine":"{v}","File":"a","Commit":"{v}"}},
               {{"RuleID":7,"StartLine":-1,"File":"b","Commit":"abc"}},
               {{"StartLine":0}}]"#
        );
        let hits = parse_gitleaks(&text, Path::new("/r")).unwrap();
        assert_eq!(
            hits,
            [
                Hit {
                    rule: UNNAMED_RULE.into(),
                    file: "a".into(),
                    line: None,
                    commit: None
                },
                Hit {
                    rule: UNNAMED_RULE.into(),
                    file: "b".into(),
                    line: None,
                    commit: None
                },
                Hit {
                    rule: UNNAMED_RULE.into(),
                    file: "".into(),
                    line: None,
                    commit: None
                },
            ]
        );
        assert!(!format!("{hits:?}").contains(&v));
    }

    #[test]
    fn parse_errors_never_quote_the_input() {
        let v = planted();
        for text in [
            format!(r#"[{{"RuleID": "{v}""#),
            format!(r#"{{"runs": "{v}"}}"#),
            format!(r#"[{{"RuleID": {v}}}]"#),
            format!(r#"{{"runs": [{{"results": [{{"locations": "{v}"}}]}}]}}"#),
            v.clone(),
        ] {
            let err = parse_any(&text, Path::new("/r")).unwrap_err();
            let shown = format!("{err} {err:?}");
            assert!(!shown.contains(&v), "{shown}");
        }
        assert!(parse_any("  ", Path::new("/r")).is_err());
    }

    #[test]
    fn empty_reports() {
        assert!(parse_any("[]", Path::new("/r")).unwrap().is_empty());
        assert!(parse_any("null", Path::new("/r")).unwrap().is_empty());
        assert!(parse_any(r#"{"runs":[{"results":[]}]}"#, Path::new("/r"))
            .unwrap()
            .is_empty());
        assert!(parse_any(r#"{"runs":[{}]}"#, Path::new("/r"))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn sarif_results_and_locations() {
        let v = planted();
        let text = format!(
            r#"{{"version":"2.1.0","runs":[{{"tool":{{"driver":{{"name":"x"}}}},"results":[
                {{"ruleId":"aws-access-key","message":{{"text":"found {v}"}},
                  "locations":[
                    {{"physicalLocation":{{"artifactLocation":{{"uri":"src/a.rs"}},
                      "region":{{"startLine":4,"snippet":{{"text":"{v}"}}}}}}}},
                    {{"physicalLocation":{{"artifactLocation":{{"uri":"file:///r/deep/b%20c.rs"}},
                      "region":{{"startLine":7}}}}}}],
                  "partialFingerprints":{{"commitSha":"89abcdef0123456789abcdef0123456789abcdef",
                                          "secretHash":"{v}"}}}},
                {{"rule":{{"id":"from-rule-ref"}},"locations":[]}},
                {{"ruleId":"no-region","locations":[{{"physicalLocation":
                  {{"artifactLocation":{{"uri":"./c.txt"}}}}}}]}}
            ]}}]}}"#
        );
        let hits = parse_any(&text, Path::new("/r")).unwrap();
        let c = Some("89abcdef0123456789abcdef0123456789abcdef".to_string());
        assert_eq!(
            hits,
            [
                Hit {
                    rule: "aws-access-key".into(),
                    file: "src/a.rs".into(),
                    line: Some(4),
                    commit: c.clone()
                },
                Hit {
                    rule: "aws-access-key".into(),
                    file: "deep/b c.rs".into(),
                    line: Some(7),
                    commit: c
                },
                Hit {
                    rule: "from-rule-ref".into(),
                    file: "".into(),
                    line: None,
                    commit: None
                },
                Hit {
                    rule: "no-region".into(),
                    file: "c.txt".into(),
                    line: None,
                    commit: None
                },
            ]
        );
        let shown = format!("{:?}", findings(hits));
        assert!(!shown.contains(&v), "{shown}");
    }

    #[test]
    fn reported_paths_are_made_relative() {
        let table = [
            ("/r", "/r/a/b.txt", "a/b.txt"),
            ("/r", "/rr/a.txt", "/rr/a.txt"),
            ("/r/", "/r/a.txt", "a.txt"),
            ("/r", "a\\b.txt", "a/b.txt"),
            ("/r", "./a.txt", "a.txt"),
            ("/r", "file:///r/x%2Fy", "x/y"),
            ("C:/w/root", "file:///C:/w/root/a.txt", "a.txt"),
            ("C:\\w\\root", "C:\\w\\root\\a.txt", "a.txt"),
            ("/r", "%zz", "%zz"),
        ];
        for (root, reported, want) in table {
            assert_eq!(relative_to(Path::new(root), reported), want, "{reported}");
        }
        assert_eq!(percent_decode("a%"), "a%");
        assert_eq!(percent_decode("a%4"), "a%4");
        assert_eq!(percent_decode("%41"), "A");
    }
}
