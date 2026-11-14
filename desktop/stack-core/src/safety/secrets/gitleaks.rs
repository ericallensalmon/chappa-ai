//! The default secret scanner: gitleaks, run as a CLI.
//!
//! **Directory mode over a given list.** `gitleaks dir` takes one path and
//! walks it. To make it read exactly the listed files, the runner stages
//! them: each listed file is hard-linked into a fresh directory under the
//! caller's work directory, at its relative path, and gitleaks scans that
//! directory with the staging directory as its working directory, so the
//! reported paths are the project-relative ones. A hard link copies no
//! data and leaves the original untouched; the staging directory is removed
//! when the scan ends. Where a link cannot be made (the work directory is
//! on another volume, or the filesystem has no hard links) that file is
//! scanned on its own instead, `gitleaks dir <relative path>` run from the
//! root, which reports the same relative path. Findings for files outside
//! the list are discarded, so the scan never widens past what was listed.
//!
//! **Git mode.** `gitleaks git --log-opts=<base>..HEAD` from the repository
//! root scans the added lines of every commit in the range, so a secret
//! committed and then deleted is still found.
//!
//! **Every run** passes `--redact`, writes a JSON report into a fresh
//! directory under the work directory (never the project), and uses its
//! own exit code for "leaks found" ([`LEAK_EXIT_CODE`]) so that findings are
//! a normal result and any other non-zero exit is a failure. gitleaks logs
//! some failures (an unknown revision in git mode, for one) and still exits
//! 0, so it runs at `--log-level error` and anything it writes to stderr
//! fails the scan rather than passing it as clean. `--config` is passed when
//! one is configured or the root holds a `.gitleaks.toml`, the root's
//! `.gitleaksignore` is honoured (`--gitleaks-ignore-path <root>`), and a
//! configured gitleaks baseline is passed as `--baseline-path`.

use super::process;
use super::report;
use crate::safety::{Finding, SafetyError, SecretScanner};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Exit code gitleaks is told to use when it finds something. Anything
/// other than this and 0 is a failure.
pub const LEAK_EXIT_CODE: i32 = 42;
/// Default deadline for one gitleaks run.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);
/// Largest report read back.
const REPORT_CAP: u64 = 64 * 1024 * 1024;
/// Most stderr quoted in an error.
const STDERR_QUOTE: usize = 800;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitleaksScanner {
    pub program: PathBuf,
    /// Reports and staging directories go here. It must not be inside a
    /// scanned tree unless it is under `.work`, which listing always skips.
    pub work_dir: PathBuf,
    pub timeout: Duration,
    /// A gitleaks config file. `None` uses the root's `.gitleaks.toml` when
    /// present, otherwise gitleaks' built-in rules.
    pub config: Option<PathBuf>,
    /// A baseline in gitleaks' own report format (one the user keeps for
    /// gitleaks itself), passed as `--baseline-path` when the file exists.
    pub baseline_path: Option<PathBuf>,
    /// Stage the list with hard links and scan it in one run (the default).
    /// Off scans each file in its own run: slower, but needs no links.
    pub stage_with_links: bool,
}

impl GitleaksScanner {
    pub fn new(program: impl Into<PathBuf>, work_dir: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            work_dir: work_dir.into(),
            timeout: DEFAULT_TIMEOUT,
            config: None,
            baseline_path: None,
            stage_with_links: true,
        }
    }

    /// The scanner at `configured` if that file exists, otherwise gitleaks
    /// from `PATH`. `None` means it is not installed, which the caller shows
    /// as a check failure with an install hint.
    pub fn detect(configured: Option<&Path>, work_dir: impl Into<PathBuf>) -> Option<Self> {
        let program = match configured {
            Some(p) => p.is_file().then(|| p.to_path_buf())?,
            None => find_on_path(
                "gitleaks",
                std::env::var_os("PATH").as_deref(),
                std::env::var_os("PATHEXT").as_deref(),
            )?,
        };
        Some(Self::new(program, work_dir))
    }

    /// `gitleaks version`, trimmed.
    pub fn version(&self) -> Result<String, SafetyError> {
        let ran = process::run(
            Command::new(&self.program).arg("version"),
            "gitleaks version",
            Duration::from_secs(20),
            4096,
        )?;
        if ran.status != Some(0) {
            return Err(SafetyError::Scanner(format!(
                "gitleaks version exited with {:?}",
                ran.status
            )));
        }
        Ok(String::from_utf8_lossy(&ran.stdout).trim().to_string())
    }

    fn common_args(&self, cmd: &mut Command, root: &Path, report: &Path) {
        cmd.args(["--redact", "--no-banner", "--no-color"])
            .args(["--log-level", "error"])
            .args(["--report-format", "json", "--report-path"])
            .arg(report)
            .args(["--exit-code", &LEAK_EXIT_CODE.to_string()])
            .arg("--gitleaks-ignore-path")
            .arg(root);
        let config = self.config.clone().or_else(|| {
            let c = root.join(".gitleaks.toml");
            c.is_file().then_some(c)
        });
        if let Some(c) = config {
            cmd.arg("--config").arg(c);
        }
        if let Some(b) = self.baseline_path.as_ref().filter(|b| b.is_file()) {
            cmd.arg("--baseline-path").arg(b);
        }
    }

    /// Run one gitleaks command and parse its report. `report_root` makes
    /// absolute paths in the report relative.
    fn run(
        &self,
        mut cmd: Command,
        report: &Path,
        report_root: &Path,
    ) -> Result<Vec<report::Hit>, SafetyError> {
        let ran = process::run(&mut cmd, "gitleaks", self.timeout, 1024 * 1024)?;
        let ok = matches!(ran.status, Some(0)) || ran.status == Some(LEAK_EXIT_CODE);
        if !ok || !ran.stderr.is_empty() {
            let mut quoted: String = ran.stderr.chars().take(STDERR_QUOTE).collect();
            if quoted.len() < ran.stderr.len() {
                quoted.push('…');
            }
            return Err(SafetyError::Scanner(format!(
                "gitleaks failed (exit {:?}): {quoted}",
                ran.status
            )));
        }
        let size = fs::metadata(report)
            .map_err(|e| SafetyError::Scanner(format!("gitleaks wrote no report: {e}")))?
            .len();
        if size > REPORT_CAP {
            return Err(SafetyError::Scanner(format!(
                "gitleaks report too large ({size} bytes)"
            )));
        }
        let text = fs::read_to_string(report)
            .map_err(|e| SafetyError::Scanner(format!("reading gitleaks report: {e}")))?;
        report::parse_gitleaks(&text, report_root)
    }
}

/// A fresh directory under the work directory, removed on drop.
pub(crate) struct ScratchDir {
    pub path: PathBuf,
}

impl ScratchDir {
    pub fn new(work_dir: &Path, prefix: &str) -> Result<Self, SafetyError> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let name = format!(
            "{prefix}-{}-{}-{nanos}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let path = work_dir.join(name);
        fs::create_dir_all(&path).map_err(|e| SafetyError::Io {
            path: path.clone(),
            message: e.to_string(),
        })?;
        Ok(Self { path })
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

impl SecretScanner for GitleaksScanner {
    fn scan_files(&self, root: &Path, files: &[PathBuf]) -> Result<Vec<Finding>, SafetyError> {
        if files.is_empty() {
            return Ok(Vec::new());
        }
        let scratch = ScratchDir::new(&self.work_dir, "secret-scan")?;
        let stage = scratch.path.join("stage");
        let mut staged = 0usize;
        let mut alone = Vec::new();
        for f in files {
            let dst = stage.join(f);
            let linked = self.stage_with_links
                && dst
                    .parent()
                    .map(|p| fs::create_dir_all(p).is_ok())
                    .unwrap_or(false)
                && fs::hard_link(root.join(f), &dst).is_ok();
            if linked {
                staged += 1;
            } else {
                alone.push(f);
            }
        }

        let mut hits = Vec::new();
        if staged > 0 {
            let report = scratch.path.join("report.json");
            let mut cmd = Command::new(&self.program);
            cmd.current_dir(&stage).args(["dir", "."]);
            self.common_args(&mut cmd, root, &report);
            hits.extend(self.run(cmd, &report, &stage)?);
        }
        for (i, f) in alone.into_iter().enumerate() {
            let report = scratch.path.join(format!("report-{i}.json"));
            let mut cmd = Command::new(&self.program);
            cmd.current_dir(root)
                .arg("dir")
                .arg(f.to_string_lossy().replace('\\', "/"));
            self.common_args(&mut cmd, root, &report);
            hits.extend(self.run(cmd, &report, root)?);
        }
        Ok(report::findings(hits))
    }

    fn scan_commits(&self, repo: &Path, range: &str) -> Result<Vec<Finding>, SafetyError> {
        let scratch = ScratchDir::new(&self.work_dir, "secret-scan")?;
        let report = scratch.path.join("report.json");
        let mut cmd = Command::new(&self.program);
        cmd.current_dir(repo)
            .args(["git", "."])
            .arg(format!("--log-opts={range}"));
        self.common_args(&mut cmd, repo, &report);
        Ok(report::findings(self.run(cmd, &report, repo)?))
    }
}

/// Find `name` on a `PATH`-style list. On Windows each `PATHEXT` extension
/// is tried too (`.EXE`, `.CMD`, …), as the shell would.
pub fn find_on_path(name: &str, path: Option<&OsStr>, pathext: Option<&OsStr>) -> Option<PathBuf> {
    let path = path?;
    let exts: Vec<OsString> = if cfg!(windows) {
        let raw = pathext
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".to_string());
        raw.split(';')
            .filter(|e| !e.is_empty())
            .map(OsString::from)
            .collect()
    } else {
        Vec::new()
    };
    for dir in std::env::split_paths(path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let plain = dir.join(name);
        if is_runnable(&plain) {
            return Some(plain);
        }
        for ext in &exts {
            let mut file = OsString::from(name);
            file.push(ext);
            let candidate = dir.join(file);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(unix)]
fn is_runnable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_runnable(p: &Path) -> bool {
    // Windows runs nothing without an extension; the PATHEXT pass finds it.
    let _ = p;
    false
}
