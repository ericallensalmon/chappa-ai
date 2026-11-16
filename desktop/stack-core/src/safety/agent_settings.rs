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
use std::process::{Command, Stdio};

/// The local settings files agent CLIs keep, relative to the folder they
/// run in. One list, so another CLI's file is one line here.
pub const AGENT_SETTINGS_FILES: &[&str] = &[".claude/settings.local.json"];

/// How exposed a settings file is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Exposure {
    /// Not ignored: the next `git add -A` commits it.
    NotIgnored,
    /// Already in the index: it is (or will be) committed.
    Tracked,
}

/// One settings file git would commit.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
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
    let mut cmd = Command::new("git");
    crate::child_env::scrub(&mut cmd);
    cmd.arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()
}

/// The agent settings files under each of `folders` (the project and the
/// folder an agent is about to work in) that git would commit. Outside a
/// repository, or without git, there is nothing to warn about.
pub fn check(project_root: &Path, folders: &[&Path]) -> Vec<AgentSettingsFile> {
    let mut out: Vec<AgentSettingsFile> = Vec::new();
    for folder in folders {
        for rel in AGENT_SETTINGS_FILES {
            let path = folder.join(rel);
            if !path.is_file() || out.iter().any(|f| f.path == path) {
                continue;
            }
            let dir = path.parent().unwrap_or(folder);
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            // 0 = tracked; 1 = not tracked; anything else = no repository.
            let tracked = match git(dir, &["ls-files", "--error-unmatch", "--", &name])
                .and_then(|s| s.code())
            {
                Some(0) => true,
                Some(1) => false,
                _ => continue,
            };
            let exposure = if tracked {
                Exposure::Tracked
            } else {
                // 0 = ignored, 1 = not ignored, 128 = not a repository.
                match git(dir, &["check-ignore", "-q", "--", &name]).and_then(|s| s.code()) {
                    Some(1) => Exposure::NotIgnored,
                    _ => continue,
                }
            };
            out.push(AgentSettingsFile {
                ignore_line: ignore_line(project_root, &path),
                path,
                exposure,
            });
        }
    }
    out
}

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
