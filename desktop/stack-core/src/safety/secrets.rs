//! Secret scanning: decide what to read, run a scanner over it, keep only
//! where the hits are, subtract what was acknowledged, and turn the rest
//! into a verdict the gates act on.
//!
//! A credential in a worktree (a committed key, an untracked `.env`, a
//! token in a config file) is one `cat` away from an agent's context and
//! its provider's logs, and anything copied into an image is readable by
//! whoever can pull it. The gates that call this module run before agent
//! launches, before source builds and on file changes; this module is the
//! pure runner behind them.
//!
//! ```text
//! ScanTarget ──list_target──▶ FileList ──SecretScanner──▶ Vec<Finding>
//!                                              │ (within the list, minus Baseline)
//!                                              ▼
//!                                         ScanReport ──Verdict::from_findings(policy)──▶ gate
//! ```
//!
//! | piece | where |
//! |---|---|
//! | what gets read | [`listing`] (git-listed or walked, ignored files included) |
//! | `.dockerignore` and exclude patterns | [`pattern`] |
//! | gitleaks | [`gitleaks::GitleaksScanner`] (directory mode over a staged list, git mode over a range) |
//! | any other scanner | [`hook::HookScanner`] (gitleaks JSON or SARIF on stdout) |
//! | report parsing, value stripping | `report` (internal) |
//! | acknowledgements | [`baseline::Baseline`] |
//! | a project's own gitleaks config, and which config a scan uses | [`config`] |
//! | a generated instance secret in a commit or a tracked file | [`generated`] |
//!
//! The invariant from the parent module holds throughout: a [`Finding`]
//! has a rule, a file, a line and a commit, and no value. Scanners are run
//! with redaction on, their reports are parsed into types that have no
//! field for a value, and their error output is quoted only where it
//! cannot hold one.

pub mod baseline;
pub mod config;
pub mod generated;
pub mod gitleaks;
pub mod hook;
pub mod listing;
pub mod pattern;
mod process;
mod report;

pub use baseline::{
    fingerprint, line_hash, AckNote, Baseline, BaselineEntry, LineSource, TreeLines,
};
pub use gitleaks::{find_on_path, GitleaksScanner};
pub use hook::HookScanner;
pub use listing::{FileList, ListLimits, ListSource, ListedFile, SkipReason, SkippedFile};
pub use pattern::PatternSet;
pub use report::UNNAMED_RULE;

use super::{Finding, Location, Policy, SafetyError, SecretScanner};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// What to scan. The later gates pick one per occasion: item open and agent
/// launch scan the working tree, a push-time or agent-time check adds the
/// commits ahead of base, a source build scans its context, a bring-up
/// scans the env files it passes, and the file watcher hands over changed
/// paths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ScanTarget {
    /// Every file under `root`, untracked and ignored ones included, minus
    /// the always-skipped directories and `excludes` (patterns as in
    /// [`PatternSet::excludes`]).
    WorkingTree {
        root: PathBuf,
        #[serde(default)]
        excludes: Vec<String>,
    },
    /// Added lines of every commit in `<base>..HEAD`.
    CommitsAhead { root: PathBuf, base: String },
    /// An image build context: what the build would send, every file
    /// under `root` (see [`listing::list_build_context`]). With
    /// `dockerignore`, the engine's ignore files are honoured (see
    /// [`build_ignore_files`]; none present: nothing excluded).
    /// `dockerfile` is the build's Dockerfile when it is not the context's
    /// own `Dockerfile`.
    BuildContext {
        root: PathBuf,
        #[serde(default)]
        dockerignore: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        dockerfile: Option<PathBuf>,
    },
    /// Env files handed to components, scanned as they are wherever they
    /// live: their values end up readable through the engine, so a value
    /// that matches a rule counts. Findings name each file by the path given.
    EnvFiles { paths: Vec<PathBuf> },
    /// The incremental rescan: changed paths (absolute, or relative to
    /// `root`). Paths outside the root, in an always-skipped directory or
    /// excluded are dropped; deleted ones too.
    Files {
        root: PathBuf,
        paths: Vec<PathBuf>,
        #[serde(default)]
        excludes: Vec<String>,
    },
}

impl ScanTarget {
    pub fn root(&self) -> Option<&Path> {
        match self {
            ScanTarget::WorkingTree { root, .. }
            | ScanTarget::CommitsAhead { root, .. }
            | ScanTarget::BuildContext { root, .. }
            | ScanTarget::Files { root, .. } => Some(root),
            ScanTarget::EnvFiles { .. } => None,
        }
    }
}

/// The outcome of one scan.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanReport {
    /// Findings to act on: inside the scanned list, not in the baseline.
    pub findings: Vec<Finding>,
    /// Findings the baseline covered.
    pub baselined: usize,
    /// Files handed to the scanner (0 for a commit scan).
    pub scanned_files: usize,
    /// Files that would have been read but were not (size caps, symlinks).
    pub skipped: Vec<SkippedFile>,
    /// Given paths outside the root (incremental rescans).
    pub outside_root: usize,
    /// Build contexts: the `.git` folders the build would send, which are
    /// not read (see [`FileList::git_dirs`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub git_dirs: Vec<String>,
}

/// The list of files `target` would scan. Exposed so a gate can show it
/// ("scanning 412 files, 2 skipped as too large") before or after a scan.
/// A commit scan has no list and returns an empty one.
pub fn list_target(target: &ScanTarget, limits: &ListLimits) -> Result<FileList, SafetyError> {
    match target {
        ScanTarget::WorkingTree { root, excludes } => {
            listing::list_files(root, &PatternSet::excludes(excludes), limits)
        }
        ScanTarget::BuildContext {
            root,
            dockerignore,
            dockerfile,
        } => {
            let mut sets = Vec::new();
            if *dockerignore {
                for file in build_ignore_files(root, dockerfile.as_deref()) {
                    match fs::read_to_string(&file) {
                        Ok(text) => sets.push(PatternSet::dockerignore(&text)),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => {
                            return Err(SafetyError::Io {
                                path: file,
                                message: e.to_string(),
                            })
                        }
                    }
                }
            }
            listing::list_build_context(root, &sets, limits)
        }
        ScanTarget::Files {
            root,
            paths,
            excludes,
        } => Ok(listing::select_files(
            root,
            paths,
            &PatternSet::excludes(excludes),
            limits,
        )),
        ScanTarget::EnvFiles { .. } | ScanTarget::CommitsAhead { .. } => Ok(FileList::default()),
    }
}

/// The ignore files an engine may read for a build of the context `root`
/// with `dockerfile` (`None`: the context's own `Dockerfile` or
/// `Containerfile`). Which one wins depends on the engine and its builder:
/// BuildKit reads `<Dockerfile>.dockerignore` before the context's
/// `.dockerignore`, the classic builder only the latter, podman prefers
/// `.containerignore`. The listing leaves a file out only when every one
/// of these that exists leaves it out, so whichever the engine reads, what
/// it sends is scanned; an ignore file the engine does not read can only
/// make the scan read more.
pub fn build_ignore_files(root: &Path, dockerfile: Option<&Path>) -> Vec<PathBuf> {
    let files: Vec<PathBuf> = match dockerfile {
        Some(d) => vec![d.to_path_buf()],
        None => vec![root.join("Dockerfile"), root.join("Containerfile")],
    };
    let mut out = Vec::new();
    for f in files {
        for ext in [".dockerignore", ".containerignore"] {
            let mut name = f.clone().into_os_string();
            name.push(ext);
            out.push(PathBuf::from(name));
        }
    }
    out.push(root.join(".containerignore"));
    out.push(root.join(".dockerignore"));
    out
}

/// Run `scanner` over `target` and subtract `baseline` (each line read
/// from the target's root as it is now).
pub fn scan(
    scanner: &dyn SecretScanner,
    target: &ScanTarget,
    limits: &ListLimits,
    baseline: &Baseline,
) -> Result<ScanReport, SafetyError> {
    let mut report = ScanReport::default();
    let found = match target {
        ScanTarget::CommitsAhead { root, base } => {
            verify_base(root, base, &limits.git)?;
            scanner.scan_commits(root, &format!("{base}..HEAD"))?
        }
        ScanTarget::EnvFiles { paths } => scan_env_files(scanner, paths, limits, &mut report)?,
        _ => {
            let root = target.root().expect("every listed target has a root");
            let list = list_target(target, limits)?;
            report.scanned_files = list.files.len();
            report.skipped = list.skipped.clone();
            report.outside_root = list.outside_root;
            report.git_dirs = list.git_dirs.clone();
            if list.files.is_empty() {
                Vec::new()
            } else {
                report::within(scanner.scan_files(root, &list.paths())?, &list)
            }
        }
    };
    let lines = TreeLines::new(target.root().unwrap_or(Path::new("")));
    let (findings, baselined) = baseline.apply(found, &lines);
    report.findings = findings;
    report.baselined = baselined;
    Ok(report)
}

/// Each env file is scanned from its own directory, then its findings are
/// renamed to the path as given, so they read (and fingerprint) the same
/// way however the scanner reported them.
fn scan_env_files(
    scanner: &dyn SecretScanner,
    paths: &[PathBuf],
    limits: &ListLimits,
    report: &mut ScanReport,
) -> Result<Vec<Finding>, SafetyError> {
    let mut out = Vec::new();
    for p in paths {
        let (Some(dir), Some(name)) = (p.parent(), p.file_name()) else {
            continue;
        };
        let dir = if dir.as_os_str().is_empty() {
            Path::new(".")
        } else {
            dir
        };
        let shown = p.to_string_lossy().replace('\\', "/");
        let Ok(meta) = fs::metadata(p) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        if meta.len() > limits.max_file_bytes {
            report.skipped.push(SkippedFile {
                path: shown,
                reason: SkipReason::TooLarge { size: meta.len() },
            });
            continue;
        }
        report.scanned_files += 1;
        let name = PathBuf::from(name);
        let name_s = name.to_string_lossy().into_owned();
        for mut f in scanner.scan_files(dir, std::slice::from_ref(&name))? {
            if let Location::Source { file, .. } = &mut f.location {
                if file.to_string_lossy() != name_s {
                    continue;
                }
                *file = PathBuf::from(&shown);
            }
            out.push(f);
        }
    }
    Ok(out)
}

/// A base for `--log-opts` must be one revision, not options: gitleaks
/// splits the value into `git log` arguments, so a base with a space or a
/// leading `-` could smuggle options in. It must also resolve, because
/// gitleaks reports an unknown revision as a clean scan.
fn verify_base(root: &Path, base: &str, git: &Path) -> Result<(), SafetyError> {
    let shape_ok = !base.is_empty()
        && !base.starts_with('-')
        && !base.contains("..")
        && base
            .chars()
            .all(|c| !c.is_whitespace() && !c.is_control() && c != '\'' && c != '"');
    if !shape_ok {
        return Err(SafetyError::InvalidTarget(format!(
            "`{base}` is not a usable base revision"
        )));
    }
    let out = crate::git::command(git, root)
        .args(["rev-parse", "--verify", "--quiet", "--end-of-options"])
        .arg(format!("{base}^{{commit}}"))
        .output()
        .map_err(|e| SafetyError::Scanner(format!("could not run git: {e}")))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(SafetyError::InvalidTarget(format!(
            "base `{base}` is not a commit in {}",
            root.display()
        )))
    }
}

/// What a gate does with a scan's findings, from `secrets.onFinding`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", content = "findings", rename_all = "kebab-case")]
pub enum Verdict {
    Clean,
    /// Show it (needs-you item, banner); block nothing.
    Warn(Vec<Finding>),
    /// Warn, and refuse new agent launches until acknowledged or fixed.
    BlockAgents(Vec<Finding>),
    /// Block agents, and refuse source builds too.
    BlockBuild(Vec<Finding>),
}

impl Verdict {
    /// Apply `policy` to `findings`. No findings is always [`Verdict::Clean`].
    /// The data-only policies (`stop`, `block-up`) are rejected when a
    /// definition is validated; should one reach here it is read as the
    /// default, `block-agents`.
    pub fn from_findings(policy: Policy, findings: Vec<Finding>) -> Self {
        if findings.is_empty() {
            return Verdict::Clean;
        }
        match policy {
            Policy::Warn => Verdict::Warn(findings),
            Policy::BlockBuild => Verdict::BlockBuild(findings),
            Policy::BlockAgents | Policy::Stop | Policy::BlockUp => Verdict::BlockAgents(findings),
        }
    }

    pub fn findings(&self) -> &[Finding] {
        match self {
            Verdict::Clean => &[],
            Verdict::Warn(f) | Verdict::BlockAgents(f) | Verdict::BlockBuild(f) => f,
        }
    }

    pub fn is_clean(&self) -> bool {
        matches!(self, Verdict::Clean)
    }

    pub fn blocks_agents(&self) -> bool {
        matches!(self, Verdict::BlockAgents(_) | Verdict::BlockBuild(_))
    }

    pub fn blocks_build(&self) -> bool {
        matches!(self, Verdict::BlockBuild(_))
    }
}

/// The policy a stack declares, or the default when it declares none.
pub fn policy_for(def: Option<&crate::definition::SecretsDef>) -> Policy {
    def.map(|d| d.on_finding).unwrap_or(Policy::BlockAgents)
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    pub fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Run git in `repo` without machine config, with a fixed identity.
    pub fn git(repo: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                repo.join("..").join("no-global-config"),
            )
            .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
            .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
            .arg("-C")
            .arg(repo)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.test",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.autocrlf=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    pub fn write(root: &Path, rel: &str, text: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    /// A token of the shape gitleaks' `github-pat` rule detects, assembled
    /// at run time so no source file holds one. Not a real credential.
    pub fn planted_token() -> String {
        let filler = "aB3dE5fG7hJ9kL1mN2pQ4rS6tU8vW0xYz1C3";
        format!("ghp_{filler}")
    }
}

#[cfg(test)]
mod tests;
