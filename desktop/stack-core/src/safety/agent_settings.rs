//! Agent CLIs' local settings files, which can end up in a commit.
//!
//! When a person answers "don't ask again", an agent CLI stores the approved
//! shell command verbatim in a local settings file, secrets included
//! (`PGPASSWORD=…`, `curl -u user:pass`, an Authorization header). Only a
//! per-user global ignore rule keeps that file out of git, so in a fresh
//! clone, or a folder that later becomes a repository, it can be committed.
//! [`check`] finds such files that git would pick up; the warning it feeds
//! never blocks a launch (a secret inside the file still goes through the
//! normal scan and its policy).

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;

/// The local settings files agent CLIs keep, relative to the folder they
/// run in. One list, so another CLI's file is one line here.
pub const AGENT_SETTINGS_FILES: &[&str] = &[".claude/settings.local.json"];

/// How exposed a settings file is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Exposure {
    /// Not ignored: the next `git add -A` commits it.
    NotIgnored,
    /// Already in the index: it is (or will be) committed.
    Tracked,
}

/// One settings file git would commit.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSettingsFile {
    /// The file, absolute.
    pub path: PathBuf,
    /// The line `.gitignore` at `project_root` needs to ignore it
    /// (`/` + the path relative to the project, forward slashes), when the
    /// file is inside the project.
    pub ignore_line: Option<String>,
    pub exposure: Exposure,
}

impl AgentSettingsFile {
    /// What the warning says.
    pub fn message(&self) -> String {
        let file = self.path.display();
        match self.exposure {
            Exposure::NotIgnored => format!(
                "{file} is not git-ignored; it stores approved commands verbatim and can be committed"
            ),
            Exposure::Tracked => format!(
                "{file} is committed; remove it from the index (git rm --cached) and rotate any secret it holds"
            ),
        }
    }
}

fn git(dir: &Path, args: &[&str]) -> Option<std::process::ExitStatus> {
    crate::git::command(crate::git::PROGRAM, dir)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()
}

/// The agent settings files under each of `folders` (the project, the
/// folder an agent works in, the stack's worktrees) that git would commit,
/// each file once however many folders name it. Outside a repository, or
/// without git, there is nothing to warn about. Cheap: a file that is not
/// there costs a stat, one that is costs two git calls.
pub fn check(project_root: &Path, folders: &[&Path]) -> Vec<AgentSettingsFile> {
    let mut out: Vec<AgentSettingsFile> = Vec::new();
    for folder in folders {
        for path in candidates(folder) {
            if out.iter().any(|f| f.path == path) {
                continue;
            }
            if let Some(f) = check_file(project_root, &path) {
                out.push(f);
            }
        }
    }
    out
}

/// The settings files an agent CLI would keep in `folder`, as [`plain`]
/// paths (so one file named through two spellings is one file).
pub fn candidates(folder: &Path) -> Vec<PathBuf> {
    let folder = plain(folder);
    AGENT_SETTINGS_FILES
        .iter()
        .map(|rel| {
            let mut p = folder.clone();
            p.extend(rel.split('/'));
            p
        })
        .collect()
}

/// `path` resolved (links, `..`, letter case on Windows) when it exists,
/// without the `\\?\` prefix Windows adds, else as given. Paths in warnings
/// are compared in this form.
pub fn plain(path: &Path) -> PathBuf {
    match fs::canonicalize(path) {
        Ok(c) => {
            let text = c.to_string_lossy();
            match text.strip_prefix(r"\\?\") {
                Some(rest) if !rest.starts_with("UNC") => PathBuf::from(rest),
                _ => c,
            }
        }
        Err(_) => path.to_path_buf(),
    }
}

/// One settings file, when git would commit it.
pub fn check_file(project_root: &Path, path: &Path) -> Option<AgentSettingsFile> {
    if !path.is_file() {
        return None;
    }
    let dir = path.parent()?;
    let name = path.file_name()?.to_string_lossy().into_owned();
    // 0 = tracked; 1 = not tracked; anything else = no repository.
    let tracked =
        match git(dir, &["ls-files", "--error-unmatch", "--", &name]).and_then(|s| s.code()) {
            Some(0) => true,
            Some(1) => false,
            _ => return None,
        };
    let exposure = if tracked {
        Exposure::Tracked
    } else {
        // 0 = ignored, 1 = not ignored, 128 = not a repository.
        match git(dir, &["check-ignore", "-q", "--", &name]).and_then(|s| s.code()) {
            Some(1) => Exposure::NotIgnored,
            _ => return None,
        }
    };
    Some(AgentSettingsFile {
        ignore_line: ignore_line(project_root, path),
        path: path.to_path_buf(),
        exposure,
    })
}

/// The folders agents work in for the stacks a folder defines: every work
/// item's worktrees and every review workspace, under each stack's work
/// folder. A folder that defines no stack (or whose definition does not
/// load) has none.
pub fn stack_worktrees(stack_project: &Path) -> Vec<PathBuf> {
    let Ok(Some((_, doc))) = crate::definition::StacksDocument::load_from_project(stack_project)
    else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = Vec::new();
    let mut push = |p: PathBuf| {
        if p.is_dir() && !out.contains(&p) {
            out.push(p);
        }
    };
    for stack in doc.stacks.values() {
        let work = if stack.work_dir.is_absolute() {
            stack.work_dir.clone()
        } else {
            stack_project.join(&stack.work_dir)
        };
        for (dir, rec) in crate::workspace::list_items(&work) {
            for repo in rec.repos.keys() {
                if let Some(wt) = rec.worktree(&dir, repo) {
                    push(wt);
                }
            }
        }
        if let Ok(entries) = fs::read_dir(work.join(REVIEWS_DIR)) {
            let mut reviews: Vec<PathBuf> = entries
                .filter_map(Result::ok)
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .map(|e| e.path())
                .collect();
            reviews.sort();
            for r in reviews {
                push(r);
            }
        }
    }
    out
}

/// Where a stack keeps its review workspaces, under its work folder.
pub const REVIEWS_DIR: &str = "reviews";

/// `/` + `path` relative to `project_root`, when it is inside it.
fn ignore_line(project_root: &Path, path: &Path) -> Option<String> {
    let canon = |p: &Path| fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let rel = canon(path)
        .strip_prefix(canon(project_root))
        .ok()?
        .to_path_buf();
    Some(format!("/{}", rel.to_string_lossy().replace('\\', "/")))
}

/// Append `line` to `<project_root>/.gitignore`, creating the file when
/// needed; a line already there is not added again. Whether it was added.
pub fn add_to_gitignore(project_root: &Path, line: &str) -> io::Result<bool> {
    let line = line.trim();
    if line.is_empty() || line.contains(['\n', '\r']) || !line.starts_with('/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a path line",
        ));
    }
    let file = project_root.join(".gitignore");
    let existing = match fs::read_to_string(&file) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    if existing.lines().any(|l| l.trim() == line) {
        return Ok(false);
    }
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&file)?;
    if !existing.is_empty() && !existing.ends_with('\n') {
        f.write_all(b"\n")?;
    }
    f.write_all(format!("{line}\n").as_bytes())?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::safety::secrets::test_support::{git as run_git, git_available, write};

    #[test]
    fn exposure_follows_git_and_nothing_is_said_outside_a_repository() {
        if !git_available() {
            eprintln!("git not on PATH; skipping");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        // A per-user global ignore rule would hide the file from git here;
        // this test is about the repository alone.
        std::env::set_var("GIT_CONFIG_GLOBAL", tmp.path().join("no-global-config"));
        std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
        std::env::set_var("XDG_CONFIG_HOME", tmp.path().join("no-xdg"));
        // No repository: nothing to commit it into.
        let plain = tmp.path().join("plain");
        write(&plain, ".claude/settings.local.json", "{}\n");
        assert!(check(&plain, &[&plain]).is_empty());

        let repo = tmp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "-q"]);
        // No file: nothing.
        assert!(check(&repo, &[&repo]).is_empty());
        write(&repo, ".claude/settings.local.json", "{}\n");
        let found = check(&repo, &[&repo]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].exposure, Exposure::NotIgnored);
        assert_eq!(
            found[0].ignore_line.as_deref(),
            Some("/.claude/settings.local.json")
        );
        assert!(found[0].message().contains("is not git-ignored"));
        // The action, twice: one line.
        assert!(add_to_gitignore(&repo, "/.claude/settings.local.json").unwrap());
        assert!(!add_to_gitignore(&repo, "/.claude/settings.local.json").unwrap());
        let text = fs::read_to_string(repo.join(".gitignore")).unwrap();
        assert_eq!(text.matches("settings.local.json").count(), 1, "{text}");
        assert!(check(&repo, &[&repo]).is_empty(), "ignored now");
        // Tracked anyway (added before the rule): bad, whatever the ignore says.
        run_git(&repo, &["add", "-f", ".claude/settings.local.json"]);
        let found = check(&repo, &[&repo]);
        assert_eq!(found[0].exposure, Exposure::Tracked);
        assert!(found[0].message().contains("is committed"));
    }

    #[test]
    fn a_file_named_through_two_folders_is_warned_about_once() {
        if !git_available() {
            eprintln!("git not on PATH; skipping");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("GIT_CONFIG_GLOBAL", tmp.path().join("no-global-config"));
        std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
        std::env::set_var("XDG_CONFIG_HOME", tmp.path().join("no-xdg"));
        let repo = tmp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "-q"]);
        write(&repo, ".claude/settings.local.json", "{}\n");
        // The same folder, spelled with a detour.
        let detour = repo.join("sub").join("..");
        fs::create_dir_all(repo.join("sub")).unwrap();
        let found = check(&repo, &[&repo, &detour, &repo]);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(
            found[0].path,
            plain(&repo.join(".claude/settings.local.json"))
        );
    }

    #[test]
    fn the_stack_worktrees_are_every_items_and_every_review_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path();
        // No definition: nothing.
        assert!(stack_worktrees(project).is_empty());
        fs::write(
            project.join("chappa.yml"),
            "stacks:\n  app:\n    components: {}\n",
        )
        .unwrap();
        let work = project.join(".work");
        let item_dir = crate::workspace::ItemRecord::dir(&work, "ABC-1");
        let mut rec = crate::workspace::ItemRecord::new(
            crate::workspace::ItemRef {
                source: "tracker".into(),
                key: "ABC-1".into(),
            },
            "ABC-1",
            "2026-01-01T00:00:00Z",
        );
        for repo in ["api", "web"] {
            rec.repos.insert(
                repo.into(),
                crate::workspace::ItemRepo {
                    branch: format!("abc-1-{repo}"),
                    path: PathBuf::from(format!("src/{repo}-1a2b3c4d")),
                    c8: "1a2b3c4d".into(),
                    base: "main".into(),
                    created_utc: "2026-01-01T00:00:00Z".into(),
                },
            );
            fs::create_dir_all(item_dir.join(format!("src/{repo}-1a2b3c4d"))).unwrap();
        }
        rec.save(&item_dir).unwrap();
        fs::create_dir_all(work.join(REVIEWS_DIR).join("github-app-7")).unwrap();
        // A record without its folder on disk names nothing.
        let mut gone = rec.clone();
        gone.key = "ABC-2".into();
        gone.repos.shift_remove("web");
        gone.save(&crate::workspace::ItemRecord::dir(&work, "ABC-2"))
            .unwrap();
        let found = stack_worktrees(project);
        assert_eq!(
            found,
            vec![
                item_dir.join("src/api-1a2b3c4d"),
                item_dir.join("src/web-1a2b3c4d"),
                work.join(REVIEWS_DIR).join("github-app-7"),
            ]
        );
    }

    #[test]
    fn appending_keeps_what_is_there_and_refuses_odd_lines() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(".gitignore"), "target/").unwrap();
        assert!(add_to_gitignore(tmp.path(), "/a/b.json").unwrap());
        assert_eq!(
            fs::read_to_string(tmp.path().join(".gitignore")).unwrap(),
            "target/\n/a/b.json\n"
        );
        assert!(add_to_gitignore(tmp.path(), "/x\n/y").is_err());
        assert!(add_to_gitignore(tmp.path(), "relative").is_err());
        let fresh = tempfile::tempdir().unwrap();
        assert!(add_to_gitignore(fresh.path(), "/c.json").unwrap());
        assert_eq!(
            fs::read_to_string(fresh.path().join(".gitignore")).unwrap(),
            "/c.json\n"
        );
    }
}
