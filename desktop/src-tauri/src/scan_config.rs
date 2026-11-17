//! The project's own gitleaks config (`chappa.yml` `secretScan.config`) as
//! part of the project's trust, and which config the app's pre-agent scan
//! and the stack runners use.
//!
//! The file sits in the tree it configures the scan of, so it is trusted
//! like a command (see stack-core's `safety::secrets::config` for the rules
//! and the order):
//!
//! - its content hash joins the project's trust hash
//!   ([`crate::stack_trust::project_trust_hash`]), so a changed file lapses
//!   trust exactly as a changed command does;
//! - the trust prompt shows it ([`ScanConfigSource::prompt_entry`]): what
//!   it skips entirely (allowlisted `paths`), any rule it fails, and its
//!   text, or what changed since it was last trusted;
//! - while the project is trusted and the file passes the rules, the gates
//!   get it pinned at its hash ([`ScanConfigSource::pinned`]) and re-check
//!   that hash before every scan; otherwise they use the Settings config or
//!   the built-in default and show a banner asking to review it.
//!
//! A Settings ▸ Security config inside an open project folder is refused
//! ([`settings_config`]): it would be a file of a scanned tree configuring
//! its own scan without the trust prompt. The project's own config is the
//! way to keep one in the tree.

use std::path::{Path, PathBuf};

use project_model::settings::Settings;
use project_model::yml::ProjectYml;
use stack_core::safety::secrets::config::{
    path_problem, read, resolve_in_project, settings_path_refusal, ConfigFacts, ProjectScanConfig,
};

/// The most lines of a config the prompt shows in full or compares.
const PROMPT_LINES: usize = 400;

/// The config a project's `chappa.yml` names, as read now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanConfigSource {
    /// As written in `chappa.yml`.
    pub rel: String,
    /// Inside the project; `Err` when the path is refused.
    pub path: Result<PathBuf, String>,
    /// Its text, content hash and what the rules say; `Err` when it cannot
    /// be read.
    pub read: Result<(String, String, Result<ConfigFacts, String>), String>,
}

impl ScanConfigSource {
    /// The config `yml` names for the project at `root`; `None` when it
    /// names none.
    pub fn of(root: &Path, yml: &ProjectYml) -> Option<ScanConfigSource> {
        let rel = yml
            .secret_scan
            .as_ref()?
            .config
            .as_deref()?
            .trim()
            .to_owned();
        let path = resolve_in_project(root, &rel);
        let read = match &path {
            Ok(p) => read(p),
            Err(e) => Err(e.clone()),
        };
        Some(ScanConfigSource { rel, path, read })
    }

    /// What goes into the trust hash: the path as written and the content
    /// hash (or a hash of why it could not be read), so editing either asks
    /// again.
    pub fn hash_part(&self) -> String {
        let content = match &self.read {
            Ok((_, hash, _)) => hash.clone(),
            Err(e) => format!(
                "unreadable:{}",
                project_model::hash::content_hash(e.as_bytes())
            ),
        };
        format!("{}\0{content}", self.rel)
    }

    /// Why the file is never used, whatever the trust: a refused path, a
    /// file that cannot be read, or one that fails the rules.
    pub fn problem(&self) -> Option<String> {
        if let Some(p) = path_problem(&self.rel) {
            return Some(p);
        }
        match &self.read {
            Err(e) => Some(e.clone()),
            Ok((_, _, Err(e))) => Some(e.clone()),
            Ok(_) => None,
        }
    }

    /// The text as read, when it could be.
    pub fn text(&self) -> Option<&str> {
        self.read.as_ref().ok().map(|(t, _, _)| t.as_str())
    }

    /// The config as the gates get it: pinned at its hash when the project
    /// is `trusted` and the file passes the rules, else named with why it
    /// is not used.
    pub fn pinned(&self, trusted: bool) -> ProjectScanConfig {
        let path = self
            .path
            .clone()
            .unwrap_or_else(|_| PathBuf::from(&self.rel));
        if let Some(p) = self.problem() {
            return ProjectScanConfig::NotUsed {
                path,
                reason: format!("is not used: {p}"),
            };
        }
        match (&self.read, trusted) {
            (Ok((_, hash, _)), true) => ProjectScanConfig::Trusted {
                path,
                hash: hash.clone(),
            },
            _ => ProjectScanConfig::NotUsed {
                path,
                reason: "is not trusted (it is new, or it changed since it was trusted)".into(),
            },
        }
    }

    /// The trust prompt's entry: what the file is for, any rule it fails,
    /// the files it skips entirely, then its text, or the lines that
    /// changed since `last_trusted` (the text when the project was last
    /// trusted).
    pub fn prompt_entry(&self, last_trusted: Option<&str>) -> String {
        let mut out = vec![format!(
            "secretScan.config: {} (a gitleaks config: every secret scan of this project uses it, and it can let findings through)",
            self.rel
        )];
        if let Some(p) = self.problem() {
            out.push(format!(
                "    ✗ it will not be used: {p}; scans use the Settings config or the built-in rules"
            ));
        }
        if let Ok((_, _, Ok(facts))) = &self.read {
            out.extend(facts.prompt_lines().into_iter().map(|l| format!("    {l}")));
        }
        if let Some(text) = self.text() {
            match last_trusted.filter(|old| *old != text) {
                Some(old) => {
                    out.push("    changed since it was last trusted:".into());
                    out.extend(line_diff(old, text).into_iter().map(|l| format!("    {l}")));
                }
                None => {
                    out.push("    contents:".into());
                    let lines: Vec<&str> = text.lines().collect();
                    for l in lines.iter().take(PROMPT_LINES) {
                        out.push(format!("    | {l}"));
                    }
                    if lines.len() > PROMPT_LINES {
                        out.push(format!("    | … {} more lines", lines.len() - PROMPT_LINES));
                    }
                }
            }
        }
        out.join("\n")
    }
}

/// The lines that differ between `old` and `new`: `- ` removed, `+ ` added,
/// in order, by a longest-common-subsequence match of whole lines. Files
/// too long to compare are shown as their new text, said so.
pub fn line_diff(old: &str, new: &str) -> Vec<String> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    if a.len() > PROMPT_LINES || b.len() > PROMPT_LINES {
        let mut out = vec!["(too long to compare line by line; the new text:)".to_owned()];
        out.extend(b.iter().take(PROMPT_LINES).map(|l| format!("+ {l}")));
        return out;
    }
    // lcs[i][j]: the common lines of a[i..] and b[j..].
    let mut lcs = vec![vec![0u16; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::new();
    while i < a.len() || j < b.len() {
        if i < a.len() && j < b.len() && a[i] == b[j] {
            i += 1;
            j += 1;
        } else if i < a.len() && (j == b.len() || lcs[i + 1][j] >= lcs[i][j + 1]) {
            out.push(format!("- {}", a[i]));
            i += 1;
        } else {
            out.push(format!("+ {}", b[j]));
            j += 1;
        }
    }
    out
}

/// The Settings ▸ Security config the scans may use: the one set, unless it
/// lies inside one of the open project folders `roots` (refused: logged,
/// and treated as not set, so the built-in default applies).
pub fn settings_config<'a>(
    settings: &Settings,
    roots: impl IntoIterator<Item = &'a Path>,
) -> Option<PathBuf> {
    let set = settings.secret_scan_config.trim();
    if set.is_empty() {
        return None;
    }
    let path = PathBuf::from(set);
    match settings_path_refusal(&path, roots) {
        Some(why) => {
            log::warn!("[secret-scan] Settings ▸ Security: {why}; it is not used");
            None
        }
        None => Some(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = "[extend]\nuseDefault = true\ndisabledRules = [\"generic-api-key\"]\n\n[allowlist]\npaths = ['''^samples/''']\n";

    fn project(config: Option<&str>, yml_extra: &str) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("p");
        std::fs::create_dir_all(root.join("tools")).unwrap();
        if let Some(c) = config {
            std::fs::write(root.join("tools").join("gitleaks.toml"), c).unwrap();
        }
        std::fs::write(
            root.join("chappa.yml"),
            format!("processes:\n  web:\n    command: npm run dev\n{yml_extra}"),
        )
        .unwrap();
        (tmp, root)
    }

    fn source(root: &Path) -> Option<ScanConfigSource> {
        ScanConfigSource::of(root, &ProjectYml::load_result(root).unwrap())
    }

    #[test]
    fn the_prompt_shows_the_file_what_it_skips_and_then_what_changed() {
        let (_t, root) = project(Some(CONFIG), "secretScan:\n  config: tools/gitleaks.toml\n");
        let s = source(&root).unwrap();
        assert_eq!(s.problem(), None);
        let first = s.prompt_entry(None);
        assert!(
            first.contains("secretScan.config: tools/gitleaks.toml"),
            "{first}"
        );
        assert!(
            first.contains("turns off these rules: generic-api-key"),
            "{first}"
        );
        assert!(
            first.contains("skips these files entirely: ^samples/"),
            "{first}"
        );
        assert!(first.contains("| useDefault = true"), "{first}");
        // Edited: the same entry, with the changed lines only.
        std::fs::write(
            root.join("tools").join("gitleaks.toml"),
            CONFIG.replace("^samples/", "^.*"),
        )
        .unwrap();
        let s = source(&root).unwrap();
        let again = s.prompt_entry(Some(CONFIG));
        assert!(
            again.contains("changed since it was last trusted:"),
            "{again}"
        );
        assert!(again.contains("- paths = ['''^samples/''']"), "{again}");
        assert!(again.contains("+ paths = ['''^.*''']"), "{again}");
        assert!(again.contains("skips these files entirely: ^.*"), "{again}");
        assert!(!again.contains("| useDefault"), "{again}");
    }

    #[test]
    fn a_file_that_breaks_a_rule_is_shown_as_an_error_and_never_pinned() {
        for (config, says) in [
            ("[allowlist]\npaths = ['''.*''']\n", "useDefault = true"),
            (
                "[extend]\nuseDefault = true\npath = \"more.toml\"\n",
                "`[extend] path` is refused",
            ),
        ] {
            let (_t, root) = project(Some(config), "secretScan:\n  config: tools/gitleaks.toml\n");
            let s = source(&root).unwrap();
            let entry = s.prompt_entry(None);
            assert!(entry.contains("✗ it will not be used"), "{entry}");
            assert!(entry.contains(says), "{entry}");
            assert!(
                matches!(s.pinned(true), ProjectScanConfig::NotUsed { reason, .. } if reason.contains(says))
            );
        }
        for bad in ["../outside.toml", "/etc/gitleaks.toml"] {
            let (_t, root) = project(Some(CONFIG), &format!("secretScan:\n  config: {bad}\n"));
            let s = source(&root).unwrap();
            assert!(s.problem().is_some(), "{bad}");
            assert!(matches!(s.pinned(true), ProjectScanConfig::NotUsed { .. }));
        }
        let (_t, root) = project(None, "");
        assert_eq!(source(&root), None);
    }

    #[test]
    fn a_trusted_valid_file_is_pinned_at_its_hash_and_an_untrusted_one_is_not() {
        let (_t, root) = project(Some(CONFIG), "secretScan:\n  config: tools/gitleaks.toml\n");
        let s = source(&root).unwrap();
        let want = stack_core::safety::secrets::config::content_hash(CONFIG.as_bytes());
        match s.pinned(true) {
            ProjectScanConfig::Trusted { path, hash } => {
                assert_eq!(hash, want);
                assert!(path.ends_with(Path::new("tools").join("gitleaks.toml")));
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            s.pinned(false),
            ProjectScanConfig::NotUsed { reason, .. } if reason.contains("not trusted")
        ));
    }

    #[test]
    fn a_settings_config_inside_an_open_project_is_not_used() {
        let (_t, root) = project(Some(CONFIG), "");
        let mut settings = Settings {
            secret_scan_config: root
                .join("tools")
                .join("gitleaks.toml")
                .display()
                .to_string(),
            ..Settings::default()
        };
        assert_eq!(settings_config(&settings, [root.as_path()]), None);
        assert_eq!(
            settings_config(&settings, std::iter::empty()),
            Some(root.join("tools").join("gitleaks.toml"))
        );
        settings.secret_scan_config = "  ".into();
        assert_eq!(settings_config(&settings, [root.as_path()]), None);
    }

    #[test]
    fn the_line_diff_names_removed_and_added_lines_in_order() {
        assert_eq!(
            line_diff("a\nb\nc\n", "a\nx\nc\nd\n"),
            ["- b", "+ x", "+ d"].map(String::from).to_vec()
        );
        assert!(line_diff("same\n", "same\n").is_empty());
    }
}
