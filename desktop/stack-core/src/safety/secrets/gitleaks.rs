//! The default secret scanner: gitleaks, run as a CLI.
//!
//! **Directory mode over a given list.** `gitleaks dir` takes one path and
//! walks it. To make it read exactly the listed files, the runner stages
//! them: each listed file is hard-linked into a fresh owner-only directory
//! under the caller's scratch folder (outside every project, see
//! [`GitleaksScanner::work_dir`]), at its relative path, and gitleaks scans
//! that directory with the staging directory as its working directory, so
//! the reported paths are the project-relative ones. A hard link copies no
//! data and leaves the original untouched; the staging directory is removed
//! when the scan ends. Where a link cannot be made (the scratch folder is
//! on another volume than the scanned tree, or the filesystem has no hard
//! links) the file is copied in instead. Before a report is trusted, every
//! staged file is checked to be what was staged: a link still the same file
//! as the original, a copy unchanged since it was made, and the original
//! not replaced meanwhile. Anything else fails the scan rather than passing
//! files nobody read. Findings for files outside the list are discarded, so
//! the scan never widens past what was listed.
//!
//! **Git mode.** `gitleaks git` with `--log-opts=… <base>..HEAD` (see
//! [`commit_log_opts`]) scans the added lines of every commit in the
//! range, so a secret committed and then deleted is still found. The git
//! gitleaks starts gets the same config overrides as every other git
//! ([`crate::git::harden_child_env`]).
//!
//! **Every run** passes `--redact`, writes a JSON report into a fresh
//! owner-only directory under the scratch folder (never the project), and uses its
//! own exit code for "leaks found" ([`LEAK_EXIT_CODE`]) so that findings are
//! a normal result and any other non-zero exit is a failure. gitleaks logs
//! some failures (an unknown revision in git mode, for one) and still exits
//! 0, so it runs at `--log-level error` and anything it writes to stderr
//! fails the scan rather than passing it as clean.
//!
//! **Nothing in the scanned tree configures the scan.** A cloned repository
//! or an agent could otherwise ship an allow-everything `.gitleaks.toml` or
//! a `.gitleaksignore` and blind it. So every run passes an explicit
//! `--config`: a trusted project config's checked text
//! ([`GitleaksScanner::config_text`], see [`super::config`]) or
//! [`DEFAULT_CONFIG`], either written into the run's scratch directory, or
//! the caller's file ([`GitleaksScanner::config`], from the user's settings),
//! which extends gitleaks' defaults with rules for credentials written
//! inline in commands and settings files. `--gitleaks-ignore-path` names an
//! empty scratch directory, and gitleaks' config variables are removed from
//! its environment ([`crate::child_env`]). gitleaks 8.30 also reads a
//! `.gitleaksignore` from the folder it scans (measured, whatever
//! `--gitleaks-ignore-path` says): a listed `.gitleaksignore` at the top of
//! the tree is therefore never staged but scanned on its own, and git mode
//! scans the repository's git folder (`git rev-parse --absolute-git-dir`)
//! from the scratch directory rather than the working tree, refusing a git
//! folder that holds a `.gitleaksignore`. Nested `.gitleaksignore` files are
//! not read by gitleaks (measured). A gitleaks baseline is passed as
//! `--baseline-path` only when the caller names one. In-file
//! `gitleaks:allow` comments are ignored (`--ignore-gitleaks-allow`).

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
/// The config every run uses unless the caller names one: gitleaks'
/// default rules plus rules for credentials written inline (env-style
/// assignments, Authorization headers, `-u user:pass`, `mysql -p…`,
/// URLs with a password), with placeholders allowed.
pub const DEFAULT_CONFIG: &str = include_str!("default-config.toml");
/// gitleaks reads this file from the folder it scans.
const IGNORE_FILE: &str = ".gitleaksignore";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitleaksScanner {
    pub program: PathBuf,
    /// The scratch folder: each run's report, config copy, ignore folder
    /// and staging go into a fresh owner-only directory here. It must be an
    /// absolute folder outside every scanned tree (the app's gate storage
    /// has one, [`crate::gate_store::GateStore::scratch_dir`]); anything
    /// that can write in a scanned tree could otherwise swap the report or
    /// drop an ignore file into it.
    pub work_dir: PathBuf,
    pub timeout: Duration,
    /// A gitleaks config file the user chose (never one from a scanned
    /// tree). `None` uses [`DEFAULT_CONFIG`].
    pub config: Option<PathBuf>,
    /// A project config's exact text, checked by the caller (its hash
    /// matched its trust, see [`super::config::choose`]). Written into the
    /// run's scratch folder layered on [`Self::config`] (or on the app's own
    /// config when that is `None`), so gitleaks reads the bytes that were
    /// checked, never the file again, and never loses the rules below.
    pub config_text: Option<String>,
    /// A baseline in gitleaks' own report format that the user keeps for
    /// gitleaks itself, named in their settings (never taken from a
    /// scanned tree), passed as `--baseline-path` when the file exists.
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
            config_text: None,
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
            crate::child_env::scrub(&mut Command::new(&self.program)).arg("version"),
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

    /// The flags every run shares. `scratch` holds the config written for
    /// this run and the empty ignore directory.
    ///
    /// `--ignore-gitleaks-allow`: a `gitleaks:allow` comment is written by
    /// whoever writes the scanned file, so it suppresses nothing here.
    fn common_args(&self, cmd: &mut Command, scratch: &Scratch, report: &Path) {
        cmd.args(["--redact", "--no-banner", "--no-color"])
            .arg("--ignore-gitleaks-allow")
            .args(["--log-level", "error"])
            .args(["--report-format", "json", "--report-path"])
            .arg(report)
            .args(["--exit-code", &LEAK_EXIT_CODE.to_string()])
            .arg("--gitleaks-ignore-path")
            .arg(&scratch.ignore_dir)
            .arg("--config")
            .arg(&scratch.config);
        if let Some(b) = self.baseline_path.as_ref().filter(|b| b.is_file()) {
            cmd.arg("--baseline-path").arg(b);
        }
        crate::child_env::scrub(cmd);
    }

    /// A scratch directory for one scan: the config file and an empty
    /// directory for `--gitleaks-ignore-path`.
    fn scratch(&self) -> Result<Scratch, SafetyError> {
        let dir = ScratchDir::new(&self.work_dir, "secret-scan")?;
        let io = |path: &Path, e: std::io::Error| SafetyError::Io {
            path: path.to_path_buf(),
            message: e.to_string(),
        };
        let ignore_dir = dir.path.join("ignore");
        fs::create_dir_all(&ignore_dir).map_err(|e| io(&ignore_dir, e))?;
        let written = |name: &str, text: &str| -> Result<PathBuf, SafetyError> {
            let c = dir.path.join(name);
            fs::write(&c, text).map_err(|e| io(&c, e))?;
            Ok(c)
        };
        let config = match (&self.config_text, &self.config) {
            // A project config is layered on its base, never in place of it:
            // the Settings config when there is one, else the app's own
            // (see `super::config::layered`).
            (Some(text), base) => {
                let base = match base {
                    Some(b) => std::path::absolute(b).unwrap_or_else(|_| b.clone()),
                    None => written("app-gitleaks.toml", DEFAULT_CONFIG)?,
                };
                let layered = super::config::layered(text, &base).map_err(|e| {
                    SafetyError::Scanner(format!("the project's gitleaks config: {e}"))
                })?;
                written("gitleaks.toml", &layered)?
            }
            (None, Some(c)) => c.clone(),
            (None, None) => written("gitleaks.toml", DEFAULT_CONFIG)?,
        };
        Ok(Scratch {
            dir,
            ignore_dir,
            config,
        })
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

/// One scan's scratch: see [`GitleaksScanner::scratch`].
struct Scratch {
    dir: ScratchDir,
    ignore_dir: PathBuf,
    config: PathBuf,
}

/// A fresh owner-only directory under the scratch folder, removed on drop.
/// The scratch folder must be absolute; it is made owner-only when this
/// has to create it.
pub(crate) struct ScratchDir {
    pub path: PathBuf,
}

impl ScratchDir {
    pub fn new(work_dir: &Path, prefix: &str) -> Result<Self, SafetyError> {
        if !work_dir.is_absolute() {
            return Err(SafetyError::Scanner(
                "no private scratch folder for the scan".into(),
            ));
        }
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
        // A scratch folder that does not exist yet is made owner-only; one
        // that does is left as it is (it may be shared, like a temp
        // folder): each run's own folder below is owner-only either way.
        if !work_dir.is_dir() {
            crate::owner_only::create_dir(work_dir).map_err(|e| SafetyError::Io {
                path: work_dir.to_path_buf(),
                message: e.to_string(),
            })?;
        }
        crate::owner_only::create_dir(&path).map_err(|e| SafetyError::Io {
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
        let scratch = self.scratch()?;
        let stage = scratch.dir.path.join("stage");
        let mut staged = Vec::new();
        let mut alone = Vec::new();
        for f in files {
            // gitleaks would read a staged top-level ignore file as its own.
            let ignore_file = f.components().count() == 1
                && f.to_string_lossy().eq_ignore_ascii_case(IGNORE_FILE);
            match (!ignore_file && self.stage_with_links)
                .then(|| stage_one(root, &stage, f))
                .flatten()
            {
                Some(s) => staged.push(s),
                None => alone.push(f),
            }
        }

        let mut hits = Vec::new();
        if !staged.is_empty() {
            let report = scratch.dir.path.join("report.json");
            let mut cmd = Command::new(&self.program);
            cmd.current_dir(&stage).arg("dir");
            self.common_args(&mut cmd, &scratch, &report);
            cmd.args(["--", "."]);
            hits.extend(self.run(cmd, &report, &stage)?);
            for s in &staged {
                s.verify(root, &stage)?;
            }
        }
        for (i, f) in alone.into_iter().enumerate() {
            let report = scratch.dir.path.join(format!("report-{i}.json"));
            let mut cmd = Command::new(&self.program);
            cmd.current_dir(root).arg("dir");
            self.common_args(&mut cmd, &scratch, &report);
            // The name is the scanned tree's: after `--`, a name that
            // starts with `-` is still a name.
            cmd.arg("--").arg(f.to_string_lossy().replace('\\', "/"));
            hits.extend(self.run(cmd, &report, root)?);
        }
        Ok(report::findings(hits))
    }

    fn scan_commits(&self, repo: &Path, range: &str) -> Result<Vec<Finding>, SafetyError> {
        let scratch = self.scratch()?;
        let report = scratch.dir.path.join("report.json");
        let git_dir = git_dir_of(repo)?;
        if git_dir.join(IGNORE_FILE).exists() {
            return Err(SafetyError::Scanner(format!(
                "{} holds a {IGNORE_FILE}, which gitleaks would honour; remove it to scan commits",
                git_dir.display()
            )));
        }
        // Scanned through the git folder, from the empty scratch directory,
        // so no ignore file in the working tree is read.
        //
        // gitleaks runs `git log -p` itself, so the git overrides go in
        // through the environment, and the log options keep the
        // repository's attributes and diff drivers from changing what the
        // patches show: `--text` because a `-diff` attribute would print
        // "Binary files differ" in place of the lines.
        let mut cmd = Command::new(&self.program);
        cmd.current_dir(&scratch.ignore_dir)
            .arg("git")
            .arg(&git_dir)
            .arg(commit_log_opts(range));
        self.common_args(&mut cmd, &scratch, &report);
        crate::git::harden_child_env(&mut cmd);
        Ok(report::findings(self.run(cmd, &report, repo)?))
    }
}

/// What a file is: the volume and file number the system knows it by, its
/// size and modification time. Two paths with the same volume and number
/// are one file (a hard link).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Ident {
    volume: u64,
    number: u64,
    len: u64,
    modified: Option<std::time::SystemTime>,
}

impl Ident {
    fn of(path: &Path) -> Option<Ident> {
        let meta = fs::symlink_metadata(path).ok()?;
        if !meta.is_file() {
            return None;
        }
        let (volume, number) = file_number(path, &meta)?;
        Some(Ident {
            volume,
            number,
            len: meta.len(),
            modified: meta.modified().ok(),
        })
    }

    fn same_file(&self, other: &Ident) -> bool {
        self.volume == other.volume && self.number == other.number
    }
}

#[cfg(unix)]
fn file_number(_path: &Path, meta: &fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some((meta.dev(), meta.ino()))
}

#[cfg(windows)]
fn file_number(path: &Path, _meta: &fs::Metadata) -> Option<(u64, u64)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };
    let file = fs::File::open(path).ok()?;
    // SAFETY: the handle is the open file's and outlives the call; the
    // out-structure is plain data.
    unsafe {
        let mut info: BY_HANDLE_FILE_INFORMATION = std::mem::zeroed();
        if GetFileInformationByHandle(file.as_raw_handle() as _, &mut info) == 0 {
            return None;
        }
        Some((
            u64::from(info.dwVolumeSerialNumber),
            (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        ))
    }
}

#[cfg(not(any(unix, windows)))]
fn file_number(_path: &Path, _meta: &fs::Metadata) -> Option<(u64, u64)> {
    None
}

/// One file put into the staging directory, and what it was then.
#[derive(Debug)]
struct Staged {
    rel: PathBuf,
    original: Ident,
    staged: Ident,
    linked: bool,
}

/// Stage `rel` (a path under `root`) into `stage`: a hard link, or a copy
/// where no link can be made. `None` when neither works; the caller then
/// scans the file on its own.
fn stage_one(root: &Path, stage: &Path, rel: &Path) -> Option<Staged> {
    let src = root.join(rel);
    let dst = stage.join(rel);
    fs::create_dir_all(dst.parent()?).ok()?;
    let original = Ident::of(&src)?;
    let linked = fs::hard_link(&src, &dst).is_ok();
    if !linked && fs::copy(&src, &dst).is_err() {
        return None;
    }
    let staged = Ident::of(&dst)?;
    let ok = if linked {
        staged.same_file(&original)
    } else {
        staged.len == original.len
    };
    ok.then(|| Staged {
        rel: rel.to_path_buf(),
        original,
        staged,
        linked,
    })
}

impl Staged {
    /// The staged file is still what was staged and the original was not
    /// replaced or changed meanwhile: a report over it speaks for the tree.
    fn verify(&self, root: &Path, stage: &Path) -> Result<(), SafetyError> {
        let now_staged = Ident::of(&stage.join(&self.rel));
        let now_original = Ident::of(&root.join(&self.rel));
        let ok = match (now_staged, now_original) {
            (Some(st), Some(or)) => {
                st == self.staged && or == self.original && (!self.linked || st.same_file(&or))
            }
            _ => false,
        };
        if ok {
            Ok(())
        } else {
            Err(SafetyError::Scanner(format!(
                "{} changed while it was scanned; scan again",
                self.rel.to_string_lossy().replace('\\', "/")
            )))
        }
    }
}

/// The `--log-opts` value of a commit scan of `range` (one revision range,
/// checked by the caller: gitleaks splits the value on spaces).
pub fn commit_log_opts(range: &str) -> String {
    let [textconv, ext_diff] = crate::git::NO_DIFF_HELPERS;
    format!("--log-opts=--text {textconv} {ext_diff} --end-of-options {range}")
}

/// The repository's git folder, absolute (a worktree's own one under the
/// main repository's `.git/worktrees/`).
fn git_dir_of(repo: &Path) -> Result<PathBuf, SafetyError> {
    let mut cmd = crate::git::command(crate::git::PROGRAM, repo);
    cmd.args(["rev-parse", "--absolute-git-dir"]);
    let ran = process::run(
        &mut cmd,
        "git rev-parse",
        Duration::from_secs(20),
        64 * 1024,
    )?;
    let text = String::from_utf8_lossy(&ran.stdout).trim().to_string();
    if ran.status != Some(0) || text.is_empty() {
        return Err(SafetyError::Scanner(format!(
            "{} is not a git repository",
            repo.display()
        )));
    }
    Ok(PathBuf::from(text))
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
