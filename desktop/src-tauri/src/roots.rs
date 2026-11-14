//! Where an agent or terminal may be started: the cwd roots.
//!
//! A folder a source names (a work item's `folder`, an `agent` or
//! `terminal` action's `cwd`, a job's `run.cwd`) or a caller passes
//! (`spawn_agent`'s `working_dir`) is accepted only when it resolves inside
//! one of:
//!
//! - an open project's directory;
//! - a git worktree of an open project (`git worktree list --porcelain`,
//!   argv only; work-item folders are usually sibling worktrees, so this is
//!   the common case);
//! - an extra root a TRUSTED source asked for in its manifest (the user
//!   approved it with the source's trust hash).
//!
//! Containment is decided on resolved paths: symlinks are followed on both
//! sides, a `..` anywhere in the given path is refused outright, and on
//! Windows the comparison ignores case (the filesystem does). The verdict
//! carries its reason as data, so a disabled button can say why.

use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::Serialize;

/// What kind of root a path matched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RootKind {
    Project { id: u32, name: String },
    Worktree { project_id: u32 },
    Source { source_id: String },
}

/// One allowed root, kept both as given (for messages) and resolved (for
/// the comparison).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root {
    pub path: PathBuf,
    resolved: PathBuf,
    pub kind: RootKind,
}

impl Root {
    pub fn new(path: impl Into<PathBuf>, kind: RootKind) -> Self {
        let path = path.into();
        let resolved = resolve_best_effort(&path);
        Self { path, resolved, kind }
    }
}

/// The answer to "may this folder be a working directory?".
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum RootsVerdict {
    /// `cwd` is the resolved folder to spawn in; `root` the root it is under.
    Allowed { cwd: PathBuf, root: PathBuf, kind: RootKind },
    /// Shown on the disabled button and in the refusal.
    Denied { reason: String },
}

impl RootsVerdict {
    pub fn allowed(&self) -> Option<&Path> {
        match self {
            RootsVerdict::Allowed { cwd, .. } => Some(cwd),
            RootsVerdict::Denied { .. } => None,
        }
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            RootsVerdict::Allowed { .. } => None,
            RootsVerdict::Denied { reason } => Some(reason),
        }
    }

    /// `Ok(cwd)` or `Err(reason)`.
    pub fn into_result(self) -> Result<PathBuf, String> {
        match self {
            RootsVerdict::Allowed { cwd, .. } => Ok(cwd),
            RootsVerdict::Denied { reason } => Err(reason),
        }
    }
}

/// Whether paths compare case-insensitively on this platform.
pub const CASE_INSENSITIVE: bool = cfg!(windows);

/// Decide `cwd` against `roots`. The text is what a source or caller gave:
/// it must be absolute, free of `..`, exist as a folder, and resolve inside
/// a root.
pub fn check_cwd(roots: &[Root], cwd: &str) -> RootsVerdict {
    let text = cwd.trim();
    if text.is_empty() {
        return RootsVerdict::Denied { reason: "no working directory was given".into() };
    }
    let given = Path::new(text);
    if !given.is_absolute() {
        return RootsVerdict::Denied { reason: format!("`{text}` is a relative path; the working directory must be an absolute folder") };
    }
    if given.components().any(|c| c == Component::ParentDir) {
        return RootsVerdict::Denied { reason: format!("`{text}` climbs out of its folder with `..`") };
    }
    if roots.is_empty() {
        return RootsVerdict::Denied { reason: format!("`{text}` cannot be used: no project is open and no trusted source names a folder") };
    }
    if !given.is_dir() {
        return RootsVerdict::Denied { reason: format!("`{text}` does not exist or is not a folder") };
    }
    let resolved = resolve_best_effort(given);
    for root in roots {
        if same_or_inside(&root.resolved, &resolved, CASE_INSENSITIVE) {
            return RootsVerdict::Allowed { cwd: resolved, root: root.path.clone(), kind: root.kind.clone() };
        }
    }
    let shown = display(&resolved);
    let note = if shown != text && !same_or_inside(Path::new(text), &resolved, CASE_INSENSITIVE) {
        format!(" (it resolves to `{shown}`)")
    } else {
        String::new()
    };
    RootsVerdict::Denied {
        reason: format!(
            "`{text}` is not inside an open project, one of its git worktrees, or a folder a trusted source may use{note}; open the project that holds it first"
        ),
    }
}

/// `child` is `root` or below it, compared component by component on
/// resolved paths. `case_insensitive` makes `C:\Code` and `c:\code` the
/// same folder, as Windows does; a parameter rather than a `cfg` so the
/// rule is testable on every platform.
pub fn same_or_inside(root: &Path, child: &Path, case_insensitive: bool) -> bool {
    let root: Vec<String> = root.components().map(|c| component_key(c, case_insensitive)).collect();
    let child: Vec<String> = child.components().map(|c| component_key(c, case_insensitive)).collect();
    child.len() >= root.len() && root.iter().zip(child.iter()).all(|(a, b)| a == b)
}

fn component_key(c: Component<'_>, case_insensitive: bool) -> String {
    let text = c.as_os_str().to_string_lossy();
    if case_insensitive {
        text.to_lowercase()
    } else {
        text.into_owned()
    }
}

/// Canonicalize the longest existing prefix of `path` and re-append the
/// rest, so a folder that does not exist yet still resolves through a
/// symlinked parent, and so a root and a child are normalised the same way
/// (on Windows canonicalize adds the `\\?\` prefix to both).
pub fn resolve_best_effort(path: &Path) -> PathBuf {
    if let Ok(p) = std::fs::canonicalize(path) {
        return p;
    }
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = path.to_path_buf();
    loop {
        match cur.parent() {
            Some(parent) => {
                if let Some(name) = cur.file_name() {
                    rest.push(name.to_owned());
                }
                match std::fs::canonicalize(parent) {
                    Ok(mut base) => {
                        for part in rest.iter().rev() {
                            base.push(part);
                        }
                        return base;
                    }
                    Err(_) => cur = parent.to_path_buf(),
                }
            }
            None => return path.to_path_buf(),
        }
    }
}

/// A path for people: the Windows verbatim prefix stripped.
pub fn display(path: &Path) -> String {
    let text = path.display().to_string();
    match text.strip_prefix(r"\\?\UNC\") {
        Some(unc) => format!(r"\\{unc}"),
        None => text.strip_prefix(r"\\?\").unwrap_or(&text).to_owned(),
    }
}

/// How long one `git worktree list` may take before the refresh is given
/// up (the cached list stays).
pub const GIT_TIMEOUT: Duration = Duration::from_secs(5);

/// The worktree paths in `git worktree list --porcelain` output: every
/// `worktree <path>` line, the main worktree included, in order.
pub fn parse_worktree_list(porcelain: &str) -> Vec<PathBuf> {
    porcelain
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// `git -C <root> worktree list --porcelain`, argv only, on its own
/// thread with a deadline. `None` when git is missing, the folder is not a
/// repository, or the call did not finish in time.
pub fn git_worktrees(root: &Path) -> Option<Vec<PathBuf>> {
    let root = root.to_path_buf();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let out = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["worktree", "list", "--porcelain"])
            .env("GIT_TERMINAL_PROMPT", "0")
            .output();
        let _ = tx.send(out);
    });
    let out = rx.recv_timeout(GIT_TIMEOUT).ok()?.ok()?;
    if !out.status.success() {
        return None;
    }
    Some(parse_worktree_list(&String::from_utf8_lossy(&out.stdout)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(path: &Path) -> Root {
        Root::new(path, RootKind::Project { id: 1, name: "app".into() })
    }

    #[test]
    fn porcelain_lists_every_worktree_in_order() {
        let text = "worktree C:/code/app\nHEAD 0123\nbranch refs/heads/main\n\nworktree C:/code/app-wt/feature\nHEAD 4567\nbranch refs/heads/feature\n\nworktree /srv/detached\nHEAD 89ab\ndetached\n";
        assert_eq!(
            parse_worktree_list(text),
            vec![PathBuf::from("C:/code/app"), PathBuf::from("C:/code/app-wt/feature"), PathBuf::from("/srv/detached")]
        );
        assert!(parse_worktree_list("").is_empty());
        assert!(parse_worktree_list("fatal: not a git repository").is_empty());
    }

    #[test]
    fn containment_is_component_wise_and_optionally_case_insensitive() {
        let root = Path::new("/code/app");
        assert!(same_or_inside(root, Path::new("/code/app"), false));
        assert!(same_or_inside(root, Path::new("/code/app/src/x"), false));
        assert!(!same_or_inside(root, Path::new("/code/app-other"), false), "a prefix of the NAME is not inside");
        assert!(!same_or_inside(root, Path::new("/code"), false));
        assert!(!same_or_inside(root, Path::new("/code/App"), false));
        assert!(same_or_inside(root, Path::new("/code/App/SRC"), true));
    }

    #[cfg(windows)]
    #[test]
    fn windows_paths_compare_without_case_and_across_separators() {
        let root = Path::new(r"C:\Code\App");
        assert!(same_or_inside(root, Path::new(r"c:\code\app\src"), true));
        assert!(same_or_inside(root, Path::new("C:/Code/App/src"), true));
        assert!(!same_or_inside(root, Path::new(r"D:\Code\App"), true));
        assert!(!same_or_inside(root, Path::new(r"C:\Code\Application"), true));
        assert_eq!(display(Path::new(r"\\?\C:\Code\App")), r"C:\Code\App");
        assert_eq!(display(Path::new(r"\\?\UNC\server\share\x")), r"\\server\share\x");
    }

    #[test]
    fn check_cwd_accepts_the_root_and_folders_below_it_and_names_the_kind() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("app");
        std::fs::create_dir_all(root.join("src")).unwrap();
        let roots = [project(&root)];
        let text = root.join("src").display().to_string();
        match check_cwd(&roots, &text) {
            RootsVerdict::Allowed { cwd, root: r, kind } => {
                assert_eq!(cwd, resolve_best_effort(&root.join("src")));
                assert_eq!(r, root);
                assert_eq!(kind, RootKind::Project { id: 1, name: "app".into() });
            }
            other => panic!("{other:?}"),
        }
        assert!(check_cwd(&roots, &root.display().to_string()).allowed().is_some(), "the root itself");
        if CASE_INSENSITIVE {
            let upper = text.to_uppercase();
            assert!(check_cwd(&roots, &upper).allowed().is_some(), "{upper}");
        }
    }

    #[test]
    fn check_cwd_refuses_relative_dotdot_missing_and_outside_with_reasons() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("app");
        let sibling = dir.path().join("other");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        let roots = [project(&root)];
        let reason = |text: &str| check_cwd(&roots, text).reason().map(str::to_owned).unwrap_or_else(|| panic!("{text} allowed"));
        assert!(reason("").contains("no working directory"));
        assert!(reason("src").contains("relative"));
        let climb = root.join("src").join("..").join("..").join("other");
        assert!(reason(&climb.display().to_string()).contains("`..`"));
        assert!(reason(&sibling.display().to_string()).contains("not inside an open project"));
        assert!(reason(&root.join("missing").display().to_string()).contains("does not exist"));
        // A file is not a folder.
        std::fs::write(root.join("file.txt"), "x").unwrap();
        assert!(reason(&root.join("file.txt").display().to_string()).contains("not a folder"));
        // No roots at all is its own message.
        assert!(check_cwd(&[], &root.display().to_string()).reason().unwrap().contains("no project is open"));
    }

    #[test]
    fn a_source_root_and_a_worktree_root_count_too() {
        let dir = tempfile::tempdir().unwrap();
        let wt = dir.path().join("app-wt").join("feature");
        let extra = dir.path().join("srv");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::create_dir_all(&extra).unwrap();
        let roots = [
            Root::new(&wt, RootKind::Worktree { project_id: 1 }),
            Root::new(&extra, RootKind::Source { source_id: "abc".into() }),
        ];
        assert!(matches!(check_cwd(&roots, &wt.display().to_string()), RootsVerdict::Allowed { kind: RootKind::Worktree { project_id: 1 }, .. }));
        assert!(matches!(
            check_cwd(&roots, &extra.join("x").display().to_string()),
            RootsVerdict::Denied { .. }
        ), "a folder that does not exist yet is refused even under a root");
        std::fs::create_dir_all(extra.join("x")).unwrap();
        assert!(matches!(check_cwd(&roots, &extra.join("x").display().to_string()), RootsVerdict::Allowed { kind: RootKind::Source { .. }, .. }));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_that_escapes_the_root_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("app");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        let roots = [project(&root)];
        let verdict = check_cwd(&roots, &root.join("link").display().to_string());
        assert!(verdict.reason().unwrap().contains("resolves to"), "{verdict:?}");
    }

    #[test]
    fn best_effort_resolution_keeps_the_missing_tail() {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let missing = dir.path().join("a").join("b");
        assert_eq!(resolve_best_effort(&missing), base.join("a").join("b"));
    }

    #[test]
    fn worktrees_of_a_real_repository_include_the_main_one() {
        let git = Command::new("git").arg("--version").output().map(|o| o.status.success()).unwrap_or(false);
        if !git {
            eprintln!("skipping: git is not on PATH");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["-c", "user.name=t", "-c", "user.email=t@example.test", "-c", "commit.gpgsign=false", "-c", "init.defaultBranch=main"])
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        run(&["init", "-q"]);
        std::fs::write(repo.join("f"), "x").unwrap();
        run(&["add", "f"]);
        run(&["commit", "-q", "-m", "one"]);
        let wt = dir.path().join("wt");
        run(&["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "feature"]);
        let list = git_worktrees(&repo).expect("a repository lists its worktrees");
        assert_eq!(list.len(), 2, "{list:?}");
        let resolved: Vec<PathBuf> = list.iter().map(|p| resolve_best_effort(p)).collect();
        assert!(resolved.contains(&resolve_best_effort(&repo)), "{resolved:?}");
        assert!(resolved.contains(&resolve_best_effort(&wt)), "{resolved:?}");
        assert!(git_worktrees(dir.path()).is_none(), "not a repository");
    }
}
