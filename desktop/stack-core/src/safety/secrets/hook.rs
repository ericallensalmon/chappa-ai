//! Any other scanner, as an argv hook.
//!
//! The hook runs with the scan root as its working directory and prints a
//! gitleaks-style JSON report or a SARIF log on stdout; the shape is
//! detected from the output. Three placeholders are replaced inside argv
//! words (and also passed as environment variables, for scripts that
//! prefer them):
//!
//! | placeholder | env | value |
//! |---|---|---|
//! | `{root}` | `CHAPPA_SCAN_ROOT` | the scan root |
//! | `{list}` | `CHAPPA_SCAN_LIST` | a file listing the paths to scan, one per line, relative to the root |
//! | `{range}` | `CHAPPA_SCAN_RANGE` | the commit range (`<base>..HEAD`), commit scans only |
//!
//! The list file lives in a fresh owner-only directory under the scratch
//! folder (outside every project, as for gitleaks) and
//! is removed afterwards. Exit codes in `ok_exit_codes` (default 0 and 1,
//! since many scanners exit 1 when they find something) are a normal
//! result; anything else is a failure. The hook's stderr is never quoted in
//! an error, because a scanner that does not redact may print a match
//! there; only its size is reported.

use super::gitleaks::ScratchDir;
use super::process;
use super::report;
use crate::safety::{Finding, SafetyError, SecretScanner};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Most stdout read from a hook.
pub const HOOK_STDOUT_CAP: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookScanner {
    pub argv: Vec<String>,
    pub work_dir: PathBuf,
    pub timeout: Duration,
    pub ok_exit_codes: Vec<i32>,
}

impl HookScanner {
    pub fn new(argv: Vec<String>, work_dir: impl Into<PathBuf>) -> Self {
        Self {
            argv,
            work_dir: work_dir.into(),
            timeout: super::gitleaks::DEFAULT_TIMEOUT,
            ok_exit_codes: vec![0, 1],
        }
    }

    fn run(
        &self,
        root: &Path,
        list: Option<&Path>,
        range: Option<&str>,
    ) -> Result<Vec<Finding>, SafetyError> {
        let (program, args) = self
            .argv
            .split_first()
            .ok_or_else(|| SafetyError::Hook("scanner hook has an empty argv".into()))?;
        let root_s = root.to_string_lossy().into_owned();
        let list_s = list
            .map(|l| l.to_string_lossy().into_owned())
            .unwrap_or_default();
        let range_s = range.unwrap_or_default();
        let fill = |w: &str| {
            w.replace("{root}", &root_s)
                .replace("{list}", &list_s)
                .replace("{range}", range_s)
        };
        let mut cmd = Command::new(fill(program));
        cmd.current_dir(root)
            .args(args.iter().map(|a| fill(a)))
            .env("CHAPPA_SCAN_ROOT", &root_s)
            .env("CHAPPA_SCAN_LIST", &list_s)
            .env("CHAPPA_SCAN_RANGE", range_s);
        let ran = process::run(&mut cmd, "scanner hook", self.timeout, HOOK_STDOUT_CAP)?;
        let ok = ran
            .status
            .map(|c| self.ok_exit_codes.contains(&c))
            .unwrap_or(false);
        if !ok {
            return Err(SafetyError::Hook(format!(
                "scanner hook exited with {:?} ({} bytes on stderr, not shown)",
                ran.status,
                ran.stderr.len()
            )));
        }
        if ran.stdout_overflowed {
            return Err(SafetyError::Hook(format!(
                "scanner hook printed more than {HOOK_STDOUT_CAP} bytes"
            )));
        }
        let text = String::from_utf8(ran.stdout)
            .map_err(|_| SafetyError::Hook("scanner hook output is not UTF-8".into()))?;
        Ok(report::findings(report::parse_any(&text, root)?))
    }
}

impl SecretScanner for HookScanner {
    fn scan_files(&self, root: &Path, files: &[PathBuf]) -> Result<Vec<Finding>, SafetyError> {
        if files.is_empty() {
            return Ok(Vec::new());
        }
        let scratch = ScratchDir::new(&self.work_dir, "secret-scan")?;
        let list = scratch.path.join("files.txt");
        let mut text = String::new();
        for f in files {
            text.push_str(&f.to_string_lossy().replace('\\', "/"));
            text.push('\n');
        }
        fs::write(&list, text).map_err(|e| SafetyError::Io {
            path: list.clone(),
            message: e.to_string(),
        })?;
        self.run(root, Some(&list), None)
    }

    fn scan_commits(&self, repo: &Path, range: &str) -> Result<Vec<Finding>, SafetyError> {
        self.run(repo, None, Some(range))
    }
}
