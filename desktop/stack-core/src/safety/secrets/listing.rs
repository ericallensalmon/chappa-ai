//! Which files a secret scan reads.
//!
//! The list is built here, not by the scanner, so every scanner reads the
//! same files and none of them can quietly skip what an agent would read.
//! In particular git-ignored files are included: an ignored `.env`,
//! `appsettings.Local.json` or `key.pem` is exactly what a history scan
//! misses and what an agent opens first.
//!
//! In a git repository the candidates come from `git ls-files --cached
//! --others` *without* `--exclude-standard` (tracked, untracked and ignored
//! files alike; `.git` itself is never listed). Anywhere else, or when git
//! is unavailable, a plain directory walk produces the same set. Either way
//! the directories in [`ALWAYS_SKIPPED`] are skipped at any depth, the
//! caller's excludes are applied, symlinks are reported but never followed
//! (a link can point outside the root), and each file is size-checked. The
//! result is sorted by path, so two runs over the same tree list the same
//! files in the same order and a total-size cap always drops the same ones.

use super::pattern::PatternSet;
use crate::safety::SafetyError;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

/// Largest single file scanned by default.
pub const DEFAULT_MAX_FILE_BYTES: u64 = 1024 * 1024;
/// Most bytes one scan reads by default.
pub const DEFAULT_MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;

/// Path segments never scanned: version-control internals, dependency
/// trees and build output (large, and not where a project keeps its own
/// credentials), and the app's own work directory (scan reports and
/// staging live there).
pub const ALWAYS_SKIPPED: [&str; 4] = [".git", "node_modules", "target", ".work"];

/// Size caps and how to find files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListLimits {
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
    /// The `git` program used for listing (and for checking a base commit).
    pub git: PathBuf,
    /// List through git when the root is in a repository. Off forces the
    /// directory walk.
    pub use_git: bool,
}

impl Default for ListLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            max_total_bytes: DEFAULT_MAX_TOTAL_BYTES,
            git: PathBuf::from("git"),
            use_git: true,
        }
    }
}

/// One file to scan, relative to the root, `/`-separated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListedFile {
    pub path: String,
    pub size: u64,
}

/// Why a file that would have been scanned was not. These are shown, not
/// hidden: an unscanned file is a gap the user should know about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "kebab-case")]
pub enum SkipReason {
    /// Over the per-file cap.
    TooLarge { size: u64 },
    /// Would have taken the scan past the total cap.
    OverTotal { size: u64 },
    /// A symbolic link; its target is not read.
    Symlink,
    /// A name git reported that is not valid UTF-8.
    NotUtf8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedFile {
    pub path: String,
    #[serde(flatten)]
    pub reason: SkipReason,
}

/// Where the candidates came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ListSource {
    Git,
    #[default]
    Walk,
    /// The caller named the files (incremental rescan).
    Given,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileList {
    pub files: Vec<ListedFile>,
    pub skipped: Vec<SkippedFile>,
    pub total_bytes: u64,
    /// Given paths that were outside the root (incremental rescan only).
    pub outside_root: usize,
    pub source: ListSource,
}

impl FileList {
    pub fn paths(&self) -> Vec<PathBuf> {
        self.files.iter().map(|f| PathBuf::from(&f.path)).collect()
    }
}

/// True when any segment of `rel` is one of [`ALWAYS_SKIPPED`].
pub fn always_skipped(rel: &str) -> bool {
    rel.split('/').any(|s| ALWAYS_SKIPPED.contains(&s))
}

/// List every file under `root` that a scan should read.
pub fn list_files(
    root: &Path,
    excludes: &PatternSet,
    limits: &ListLimits,
) -> Result<FileList, SafetyError> {
    if !root.is_dir() {
        return Err(SafetyError::InvalidTarget(format!(
            "{} is not a directory",
            root.display()
        )));
    }
    let mut not_utf8 = 0usize;
    let (candidates, source) = match limits
        .use_git
        .then(|| git_candidates(root, &limits.git, excludes, &mut not_utf8))
        .flatten()
    {
        Some(c) => (c, ListSource::Git),
        None => {
            let mut out = Vec::new();
            walk(root, "", excludes, &mut out)?;
            (out, ListSource::Walk)
        }
    };
    let mut list = finish(root, candidates, excludes, limits);
    list.source = source;
    for i in 0..not_utf8 {
        list.skipped.push(SkippedFile {
            path: format!("(name {} not valid UTF-8)", i + 1),
            reason: SkipReason::NotUtf8,
        });
    }
    Ok(list)
}

/// Turn caller-named paths (absolute, or relative to `root`) into a list,
/// with the same skips and caps as [`list_files`]. A path outside the root
/// is dropped and counted; a path that no longer exists is dropped silently
/// (a deleted file holds nothing to find).
pub fn select_files(
    root: &Path,
    paths: &[PathBuf],
    excludes: &PatternSet,
    limits: &ListLimits,
) -> FileList {
    let root_norm = normalize(root);
    let mut outside = 0;
    let mut candidates = Vec::new();
    for p in paths {
        let abs = if p.is_absolute() {
            normalize(p)
        } else {
            normalize(&root.join(p))
        };
        match abs.strip_prefix(&root_norm).ok().and_then(rel_string) {
            Some(rel) if !rel.is_empty() => candidates.push(rel),
            _ => outside += 1,
        }
    }
    let mut list = finish(root, candidates, excludes, limits);
    list.outside_root = outside;
    list.source = ListSource::Given;
    list
}

/// Lexical normalization: drops `.` and folds `..` without touching the
/// filesystem (the path may name a file that was just deleted).
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// A relative path as a `/`-separated string, or `None` if it leaves the
/// root or is not valid UTF-8.
fn rel_string(p: &Path) -> Option<String> {
    let mut parts = Vec::new();
    for c in p.components() {
        match c {
            Component::Normal(s) => parts.push(s.to_str()?.to_string()),
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(parts.join("/"))
}

fn git_candidates(
    root: &Path,
    git: &Path,
    excludes: &PatternSet,
    not_utf8: &mut usize,
) -> Option<Vec<String>> {
    let mut cmd = Command::new(git);
    cmd.arg("-C")
        .arg(root)
        .args(["ls-files", "-z", "--cached", "--others"]);
    // Prune the always-skipped directories from the untracked walk; tracked
    // files inside them are dropped later with everything else.
    for d in ALWAYS_SKIPPED {
        cmd.arg(format!("--exclude={d}/"));
    }
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    let mut candidates = Vec::new();
    for raw in out.stdout.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let Ok(name) = std::str::from_utf8(raw) else {
            *not_utf8 += 1;
            continue;
        };
        if let Some(dir) = name.strip_suffix('/') {
            // An untracked nested repository is listed as one directory
            // entry; its files are just as readable, so walk it.
            if !always_skipped(dir) {
                let _ = walk(&root.join(dir), dir, excludes, &mut candidates);
            }
        } else {
            candidates.push(name.to_string());
        }
    }
    Some(candidates)
}

fn walk(
    dir: &Path,
    rel: &str,
    excludes: &PatternSet,
    out: &mut Vec<String>,
) -> Result<(), SafetyError> {
    let entries = fs::read_dir(dir).map_err(|e| SafetyError::Io {
        path: dir.to_path_buf(),
        message: e.to_string(),
    })?;
    let prune_excluded = !excludes.has_negations();
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let child = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if ALWAYS_SKIPPED.contains(&name.as_str())
                || (prune_excluded && excludes.is_excluded(&child))
            {
                continue;
            }
            // A directory we cannot read is left out rather than failing
            // the whole listing.
            let _ = walk(&entry.path(), &child, excludes, out);
        } else {
            out.push(child);
        }
    }
    Ok(())
}

fn finish(
    root: &Path,
    mut candidates: Vec<String>,
    excludes: &PatternSet,
    limits: &ListLimits,
) -> FileList {
    for c in candidates.iter_mut() {
        if c.contains('\\') {
            *c = c.replace('\\', "/");
        }
    }
    candidates.sort();
    candidates.dedup();
    let mut list = FileList::default();
    for rel in candidates {
        if always_skipped(&rel) || excludes.is_excluded(&rel) {
            continue;
        }
        let Ok(meta) = fs::symlink_metadata(root.join(&rel)) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            list.skipped.push(SkippedFile {
                path: rel,
                reason: SkipReason::Symlink,
            });
            continue;
        }
        if !meta.is_file() {
            continue;
        }
        let size = meta.len();
        if size > limits.max_file_bytes {
            list.skipped.push(SkippedFile {
                path: rel,
                reason: SkipReason::TooLarge { size },
            });
        } else if list.total_bytes + size > limits.max_total_bytes {
            list.skipped.push(SkippedFile {
                path: rel,
                reason: SkipReason::OverTotal { size },
            });
        } else {
            list.total_bytes += size;
            list.files.push(ListedFile { path: rel, size });
        }
    }
    list
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{git, git_available, write};
    use super::*;

    fn paths(list: &FileList) -> Vec<&str> {
        list.files.iter().map(|f| f.path.as_str()).collect()
    }

    /// A tree with tracked, untracked and ignored files, plus the always
    /// skipped directories, each holding a file.
    fn tree(root: &Path) {
        write(root, ".gitignore", ".env\nlogs/\n");
        write(root, "src/main.rs", "fn main() {}\n");
        write(root, "README.md", "readme\n");
        write(root, ".env", "A=1\n");
        write(root, "logs/today.log", "log\n");
        write(root, "notes.txt", "untracked\n");
        write(root, "node_modules/pkg/index.js", "x\n");
        write(root, "target/debug/out", "x\n");
        write(root, "web/node_modules/pkg/a.js", "x\n");
        write(root, ".work/report.json", "x\n");
    }

    #[test]
    fn git_listing_includes_untracked_and_ignored_and_skips_the_rest() {
        if !git_available() {
            eprintln!("git not on PATH; skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        fs::create_dir(&root).unwrap();
        git(&root, &["init", "-q"]);
        tree(&root);
        git(&root, &["add", ".gitignore", "src/main.rs", "README.md"]);
        git(&root, &["commit", "-q", "-m", "init"]);
        // a tracked file inside an always-skipped directory is still skipped
        write(&root, "target/tracked.txt", "x\n");
        git(&root, &["add", "-f", "target/tracked.txt"]);

        let list = list_files(&root, &PatternSet::default(), &ListLimits::default()).unwrap();
        assert_eq!(list.source, ListSource::Git);
        assert_eq!(
            paths(&list),
            [
                ".env",
                ".gitignore",
                "README.md",
                "logs/today.log",
                "notes.txt",
                "src/main.rs"
            ]
        );
        assert!(list.skipped.is_empty(), "{:?}", list.skipped);

        let walked = list_files(
            &root,
            &PatternSet::default(),
            &ListLimits {
                use_git: false,
                ..ListLimits::default()
            },
        )
        .unwrap();
        assert_eq!(walked.source, ListSource::Walk);
        assert_eq!(paths(&walked), paths(&list), "walk and git agree");
    }

    #[test]
    fn nested_repository_is_walked() {
        if !git_available() {
            eprintln!("git not on PATH; skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        fs::create_dir(&root).unwrap();
        git(&root, &["init", "-q"]);
        write(&root, "a.txt", "a\n");
        let nested = root.join("vendor/lib");
        fs::create_dir_all(&nested).unwrap();
        git(&nested, &["init", "-q"]);
        write(&nested, "config/key.pem", "k\n");
        let list = list_files(&root, &PatternSet::default(), &ListLimits::default()).unwrap();
        assert_eq!(paths(&list), ["a.txt", "vendor/lib/config/key.pem"]);
    }

    #[test]
    fn walk_outside_a_repository_with_excludes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        tree(root);
        let list = list_files(
            root,
            &PatternSet::excludes(&["*.log", "src"]),
            &ListLimits::default(),
        )
        .unwrap();
        assert_eq!(
            paths(&list),
            [".env", ".gitignore", "README.md", "notes.txt"]
        );
    }

    #[test]
    fn size_caps_at_the_edges() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "a_exact.bin", &"x".repeat(10));
        write(root, "b_over.bin", &"x".repeat(11));
        write(root, "c_fits.bin", &"x".repeat(5));
        write(root, "d_total.bin", &"x".repeat(6));
        write(root, "e_small.bin", "x");
        let limits = ListLimits {
            max_file_bytes: 10,
            max_total_bytes: 16,
            use_git: false,
            ..ListLimits::default()
        };
        let list = list_files(root, &PatternSet::default(), &limits).unwrap();
        // 10 + 5 = 15; 6 more would be 21 > 16; 1 more is exactly 16
        assert_eq!(paths(&list), ["a_exact.bin", "c_fits.bin", "e_small.bin"]);
        assert_eq!(list.total_bytes, 16);
        assert_eq!(
            list.skipped,
            [
                SkippedFile {
                    path: "b_over.bin".into(),
                    reason: SkipReason::TooLarge { size: 11 }
                },
                SkippedFile {
                    path: "d_total.bin".into(),
                    reason: SkipReason::OverTotal { size: 6 }
                },
            ]
        );
    }

    #[test]
    fn selected_files_drop_outside_excluded_and_missing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        tree(&root);
        write(dir.path(), "elsewhere.txt", "x\n");
        let given = vec![
            root.join("src/main.rs"),
            PathBuf::from(".env"),
            PathBuf::from("src/../notes.txt"),
            PathBuf::from("../elsewhere.txt"),
            dir.path().join("elsewhere.txt"),
            PathBuf::from("node_modules/pkg/index.js"),
            PathBuf::from("logs/today.log"),
            PathBuf::from("deleted.txt"),
            root.join("src/main.rs"),
        ];
        let list = select_files(
            &root,
            &given,
            &PatternSet::excludes(&["logs"]),
            &ListLimits::default(),
        );
        assert_eq!(list.source, ListSource::Given);
        assert_eq!(paths(&list), [".env", "notes.txt", "src/main.rs"]);
        assert_eq!(list.outside_root, 2);
    }

    #[test]
    fn missing_root_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = list_files(
            &dir.path().join("nope"),
            &PatternSet::default(),
            &ListLimits::default(),
        )
        .unwrap_err();
        assert!(matches!(err, SafetyError::InvalidTarget(_)), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_reported_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        write(&root, "a.txt", "a\n");
        write(dir.path(), "outside/secret.txt", "s\n");
        std::os::unix::fs::symlink(dir.path().join("outside"), root.join("linkdir")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("outside/secret.txt"), root.join("link.txt"))
            .unwrap();
        let limits = ListLimits {
            use_git: false,
            ..ListLimits::default()
        };
        let list = list_files(&root, &PatternSet::default(), &limits).unwrap();
        assert_eq!(paths(&list), ["a.txt"]);
        let skipped: Vec<_> = list.skipped.iter().map(|s| s.path.as_str()).collect();
        assert_eq!(skipped, ["link.txt", "linkdir"]);
    }
}
