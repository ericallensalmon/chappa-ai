//! The one way this workspace runs git.
//!
//! git runs in folders an agent can write: the project, its worktrees,
//! the item worktrees under `.work/`. A repository's own config (its
//! `.git/config`, or the config of whatever git folder a worktree's `.git`
//! file names) can make git run commands: `core.fsmonitor` on every
//! `status`, hooks on `checkout` and `worktree add`, `core.sshCommand` on a
//! fetch, textconv and external diff drivers on `diff` and `log -p`. So
//! every git this workspace starts goes through [`command`] (a
//! [`std::process::Command`]) or [`invocation`] (an [`Invocation`] for an
//! [`crate::cli::EngineCli`]), which put [`OVERRIDES`] on the command line
//! ahead of everything the repository says, keep the machine-wide config
//! out and turn terminal prompts off. A program that runs git itself (the
//! secret scanner's commit scan) gets the same overrides through the
//! environment ([`harden_child_env`]).
//!
//! What overrides cannot cover is the rest of a git folder's config
//! (filter drivers, for one). So before git runs in a worktree an agent
//! can write, [`check_worktree_link`] makes sure its `.git` file names a
//! git folder of a clone the caller knows, not one the agent made.
//!
//! Callers still pass `--end-of-options` before a revision or path that
//! came from data, and [`NO_DIFF_HELPERS`] to every `diff`, `log` and
//! `show`. Only this module may spawn git; `tests/git_spawns.rs` fails the
//! build of the test suite when a direct spawn appears anywhere else.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use crate::cli::Invocation;

/// The program name: git from `PATH`.
pub const PROGRAM: &str = "git";

/// Flags that keep a diff from running a repository-configured program:
/// textconv filters (`diff.<driver>.textconv`) and external diff drivers.
pub const NO_DIFF_HELPERS: [&str; 2] = ["--no-textconv", "--no-ext-diff"];

/// The config every git call overrides, as `(key, value)`. The hooks path
/// and the ssh command are filled in by [`overrides`].
///
/// `diff.external` is not overridden with an empty value: git takes the
/// empty string as a program name and every diff then fails. Diffs pass
/// [`NO_DIFF_HELPERS`] instead, which turns `diff.external` off too.
pub const OVERRIDES: [(&str, &str); 4] = [
    // A command git runs on every `status` and index refresh.
    ("core.fsmonitor", "false"),
    // Hooks run on checkout, worktree add, commit, merge … ; this points
    // them at a folder that cannot exist (see `no_hooks_dir`).
    ("core.hooksPath", ""),
    // A command git runs for every ssh remote: the person's own (global)
    // one, or plain `ssh`, never one a repository sets (see
    // `person_ssh_command`). `GIT_SSH_COMMAND` still wins, as in git.
    ("core.sshCommand", "ssh"),
    // `ext::` remote URLs run an arbitrary command.
    ("protocol.ext.allow", "never"),
];

/// A hooks folder that cannot exist: a child of this program's own file
/// (a path below a regular file is never a folder, on any platform), so no
/// hook is ever found and no folder has to be made and kept empty. When
/// the program's path is unknown, an unguessable name in the temp folder
/// that is never created.
fn no_hooks_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        std::env::current_exe()
            .ok()
            .filter(|p| p.is_file())
            .map(|p| p.join("no-hooks"))
            .unwrap_or_else(|| {
                let mut bytes = [0u8; 16];
                let _ = getrandom::fill(&mut bytes);
                let name: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
                std::env::temp_dir().join(format!("no-hooks-{name}"))
            })
    })
}

/// The overrides that are fixed (everything but the ssh command), for the
/// one git call that reads the person's ssh command.
fn fixed_overrides() -> Vec<(&'static str, String)> {
    OVERRIDES
        .iter()
        .map(|(k, v)| {
            let v = if *k == "core.hooksPath" {
                no_hooks_dir().display().to_string()
            } else {
                (*v).to_string()
            };
            (*k, v)
        })
        .collect()
}

/// `core.sshCommand` from the person's own config only (`--global`: the
/// global and XDG files), read once per process. A person who routes ssh
/// through a key or a proxy keeps that; only a repository's or a
/// worktree's value is overridden.
fn person_ssh_command() -> &'static str {
    static SSH: OnceLock<String> = OnceLock::new();
    SSH.get_or_init(|| read_global_ssh_command(None).unwrap_or_else(|| "ssh".to_string()))
}

/// `git config --global --get core.sshCommand`, with the fixed overrides
/// and run from the temp folder (no repository's config is read for a
/// `--global` question, and none is near). `global_config` names the file
/// in place of the person's (tests). `None` when unset or unreadable.
fn read_global_ssh_command(global_config: Option<&Path>) -> Option<String> {
    let mut cmd = Command::new(PROGRAM);
    crate::child_env::scrub(&mut cmd);
    for (k, v) in fixed_overrides() {
        if k != "core.sshCommand" {
            cmd.arg("-c").arg(format!("{k}={v}"));
        }
    }
    cmd.args(["config", "--global", "--get", "core.sshCommand"])
        .current_dir(std::env::temp_dir())
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    for (k, v) in ENV {
        cmd.env(k, v);
    }
    if let Some(file) = global_config {
        cmd.env("GIT_CONFIG_GLOBAL", file);
    }
    let out = cmd.output().ok()?;
    let value = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (out.status.success() && !value.is_empty()).then_some(value)
}

/// [`OVERRIDES`] with the hooks path and the person's ssh command filled in.
pub fn overrides() -> Vec<(&'static str, String)> {
    fixed_overrides()
        .into_iter()
        .map(|(k, v)| {
            if k == "core.sshCommand" {
                (k, person_ssh_command().to_string())
            } else {
                (k, v)
            }
        })
        .collect()
}

/// The arguments that go between `git` and its subcommand: one `-c` per
/// override, then `--no-optional-locks` (a read-only question never takes
/// the index lock a running `git` in the same worktree may want).
pub fn global_args() -> Vec<String> {
    let mut out = Vec::new();
    for (k, v) in overrides() {
        out.push("-c".to_string());
        out.push(format!("{k}={v}"));
    }
    out.push("--no-optional-locks".to_string());
    out
}

/// The environment every git call gets: no machine-wide config (a file
/// outside the person's own), and a missing credential fails instead of
/// waiting at a prompt nobody sees.
pub const ENV: [(&str, &str); 2] = [("GIT_CONFIG_NOSYSTEM", "1"), ("GIT_TERMINAL_PROMPT", "0")];

/// `git <global args> -C <dir>` as a [`Command`] from `program` (normally
/// [`PROGRAM`]), with [`ENV`] set and the variables no child may inherit
/// removed. Append the subcommand and its arguments.
pub fn command(program: impl AsRef<OsStr>, dir: &Path) -> Command {
    let mut cmd = Command::new(program);
    crate::child_env::scrub(&mut cmd);
    cmd.args(global_args()).arg("-C").arg(dir);
    for (k, v) in ENV {
        cmd.env(k, v);
    }
    cmd
}

/// `git <global args> -C <dir> <args>` as an [`Invocation`], with [`ENV`].
pub fn invocation(dir: &Path, args: &[&str]) -> Invocation {
    let mut argv = vec![PROGRAM.to_string()];
    argv.extend(global_args());
    argv.push("-C".to_string());
    argv.push(dir.display().to_string());
    argv.extend(args.iter().map(|a| a.to_string()));
    ENV.iter()
        .fold(Invocation::from_argv(&argv), |inv, (k, v)| inv.env(k, v))
}

/// For a program that runs git itself: [`ENV`] plus [`OVERRIDES`] passed
/// the way git reads config from the environment (`GIT_CONFIG_COUNT`,
/// `GIT_CONFIG_KEY_<n>`, `GIT_CONFIG_VALUE_<n>`), which, like `-c`, wins
/// over every config file.
pub fn harden_child_env(cmd: &mut Command) -> &mut Command {
    for (k, v) in ENV {
        cmd.env(k, v);
    }
    let list = overrides();
    cmd.env("GIT_CONFIG_COUNT", list.len().to_string());
    for (i, (k, v)) in list.iter().enumerate() {
        cmd.env(format!("GIT_CONFIG_KEY_{i}"), k);
        cmd.env(format!("GIT_CONFIG_VALUE_{i}"), v);
    }
    cmd
}

/// Whether `s` has the shape of a commit id: 7 to 64 hex digits (an
/// abbreviated or full SHA-1 or SHA-256 id). Anything else that names a
/// commit (a ref, `HEAD~2`, an option) is refused where the value came
/// from data.
pub fn is_commit_shaped(s: &str) -> bool {
    (7..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The shared git folder of the repository whose working tree is `repo`:
/// `repo/.git` when it is a folder, else the folder its `.git` file names
/// (through `commondir` when that is itself a worktree's). Read from the
/// files, so it needs no git; `None` when `repo` has no git folder.
pub fn common_dir(repo: &Path) -> Option<PathBuf> {
    let dot = repo.join(".git");
    let meta = fs::symlink_metadata(&dot).ok()?;
    let dir = if meta.is_dir() {
        dot
    } else if meta.is_file() {
        gitdir_of(&dot, repo)?
    } else {
        return None;
    };
    match fs::read_to_string(dir.join("commondir")) {
        Ok(text) => {
            let rel = text.trim();
            (!rel.is_empty()).then(|| dir.join(rel))
        }
        Err(_) => Some(dir),
    }
}

/// The folder a `gitdir: <path>` file names, relative paths against `base`.
fn gitdir_of(file: &Path, base: &Path) -> Option<PathBuf> {
    let text = fs::read_to_string(file).ok()?;
    let path = text.lines().next()?.strip_prefix("gitdir:")?.trim();
    if path.is_empty() {
        return None;
    }
    let path = Path::new(path);
    Some(if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    })
}

/// Check that `worktree` is a linked worktree of one of the repositories
/// whose shared git folders are `common_dirs`: its `.git` is a plain file
/// (not a folder or a link), it names `<common>/worktrees/<name>`, and
/// that folder's own `gitdir` file names this worktree back.
///
/// A worktree's `.git` file is writable by whatever works in the
/// worktree. Pointed at a git folder of its own making, every git call
/// there would read that folder's config, so a worktree that fails this
/// check is reported as an error and git is not run in it.
pub fn check_worktree_link(worktree: &Path, common_dirs: &[PathBuf]) -> Result<(), String> {
    let shown = worktree.display();
    let bad = |why: &str| format!("{shown} is not a worktree of a known clone: {why}");
    let dot = worktree.join(".git");
    let meta = fs::symlink_metadata(&dot).map_err(|_| bad("it has no .git file"))?;
    if !meta.is_file() {
        return Err(bad(".git is not a plain file"));
    }
    let named = gitdir_of(&dot, worktree).ok_or_else(|| bad(".git does not name a git folder"))?;
    let named =
        fs::canonicalize(&named).map_err(|_| bad("the git folder its .git names is missing"))?;
    let parent_ok = named.parent().is_some_and(|parent| {
        common_dirs
            .iter()
            .any(|c| fs::canonicalize(c.join("worktrees")).is_ok_and(|w| w.as_path() == parent))
    });
    if !parent_ok {
        return Err(bad(
            "its .git names a git folder outside the clone's worktrees",
        ));
    }
    // `<git folder>/gitdir` holds the worktree's `.git` path itself, no
    // `gitdir:` prefix.
    let back = fs::read_to_string(named.join("gitdir"))
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .map(|t| named.join(t))
        .and_then(|p| fs::canonicalize(p).ok())
        .ok_or_else(|| bad("the clone does not list it"))?;
    let here = fs::canonicalize(&dot).map_err(|_| bad("its .git cannot be resolved"))?;
    if back != here {
        return Err(bad("the clone lists that git folder for another worktree"));
    }
    Ok(())
}

/// The folders a caller runs git in as they are (a project and its
/// clones), and the clones a worktree anywhere else must belong to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KnownRepos {
    /// The project and the clones, resolved.
    trusted: Vec<PathBuf>,
    /// The shared git folder of each clone.
    commons: Vec<PathBuf>,
}

impl KnownRepos {
    pub fn new(project: &Path, clones: &[PathBuf]) -> Self {
        let mut trusted: Vec<PathBuf> = clones.iter().map(|c| resolved(c)).collect();
        trusted.push(resolved(project));
        Self {
            trusted,
            commons: clones.iter().filter_map(|c| common_dir(c)).collect(),
        }
    }

    /// Whether git may run in `dir`: the git folder it would find there is
    /// the project's, a clone's, or a worktree's of a clone. Walking up
    /// from `dir` as git does, the first folder that is the project or a
    /// clone answers yes, and the first one holding a `.git` must be a
    /// worktree of a clone ([`check_worktree_link`]); a folder with no
    /// `.git` anywhere above it has no repository for git to read.
    pub fn check(&self, dir: &Path) -> Result<(), String> {
        let start = resolved(dir);
        let mut at: Option<&Path> = Some(&start);
        while let Some(level) = at {
            if self.trusted.iter().any(|t| t == level) {
                return Ok(());
            }
            if fs::symlink_metadata(level.join(".git")).is_ok() {
                return check_worktree_link(level, &self.commons);
            }
            at = level.parent();
        }
        Ok(())
    }
}

/// `p` with links and `..` resolved when it exists, else as given.
fn resolved(p: &Path) -> PathBuf {
    fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn commit_shapes() {
        assert!(is_commit_shaped("0123abc"));
        assert!(is_commit_shaped(&"a".repeat(40)));
        assert!(is_commit_shaped(&"F".repeat(64)));
        for bad in [
            "",
            "012345",
            "--all",
            "HEAD",
            "0123abc~1",
            &"a".repeat(65),
            "0123abg",
        ] {
            assert!(!is_commit_shaped(bad), "{bad}");
        }
    }

    #[test]
    fn every_override_is_on_the_command_line_before_the_subcommand() {
        let inv = invocation(Path::new("/r"), &["status"]);
        let argv = inv.argv();
        let at = |s: &str| argv.iter().position(|a| a == s).unwrap();
        for (k, _) in OVERRIDES {
            let i = argv
                .iter()
                .position(|a| a.starts_with(&format!("{k}=")))
                .unwrap();
            assert_eq!(argv[i - 1], "-c");
            assert!(i < at("status"));
        }
        assert!(at("--no-optional-locks") < at("-C"));
        assert_eq!(&argv[argv.len() - 3..], ["-C", "/r", "status"]);
        assert_eq!(
            inv.env.get("GIT_CONFIG_NOSYSTEM").map(String::as_str),
            Some("1")
        );
        assert_eq!(
            inv.env.get("GIT_TERMINAL_PROMPT").map(String::as_str),
            Some("0")
        );
        let hooks = argv
            .iter()
            .find_map(|a| a.strip_prefix("core.hooksPath="))
            .unwrap();
        assert!(!Path::new(hooks).exists(), "{hooks}");
    }

    /// git for setting a repository up, outside the helper on purpose:
    /// these tests compare a plain git with the hardened one.
    fn setup_git(dir: &Path, args: &[&str]) -> std::process::Output {
        let out = Command::new(PROGRAM)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", dir.join("no-global-config"))
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.test"])
            .args([
                "-c",
                "commit.gpgsign=false",
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
        out
    }

    fn git_available() -> bool {
        let ok = Command::new(PROGRAM)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success());
        if !ok {
            eprintln!("git not on PATH; skipping");
        }
        ok
    }

    /// A repository with one commit at `dir`.
    fn repo(dir: &Path) {
        fs::create_dir_all(dir).unwrap();
        setup_git(dir, &["init", "-q"]);
        fs::write(dir.join("a.txt"), "a\n").unwrap();
        setup_git(dir, &["add", "a.txt"]);
        setup_git(dir, &["commit", "-q", "-m", "init"]);
    }

    /// Set `core.fsmonitor` in `repo`'s own config to a command that leaves
    /// `marker` behind.
    pub(crate) fn fsmonitor_trap(repo: &Path, marker: &Path) {
        let shown = marker.display().to_string().replace('\\', "/");
        setup_git(
            repo,
            &[
                "config",
                "core.fsmonitor",
                &format!("echo x > '{shown}'; false"),
            ],
        );
    }

    #[test]
    fn a_repository_fsmonitor_runs_under_plain_git_and_never_under_the_helper() {
        if !git_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path().join("r");
        repo(&r);
        let marker = tmp.path().join("marker");
        fsmonitor_trap(&r, &marker);
        fs::write(r.join("a.txt"), "changed\n").unwrap();

        // The control: the trap works.
        setup_git(&r, &["status", "--porcelain"]);
        assert!(marker.exists(), "plain git runs the repository's fsmonitor");
        fs::remove_file(&marker).unwrap();

        let out = command(PROGRAM, &r)
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        assert!(out.status.success());
        assert!(String::from_utf8_lossy(&out.stdout).contains("a.txt"));
        assert!(!marker.exists(), "the helper's git ran the fsmonitor");
    }

    #[test]
    fn the_ssh_command_is_the_persons_own_never_the_repositorys() {
        if !git_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path().join("r");
        repo(&r);
        setup_git(
            &r,
            &["config", "core.sshCommand", "echo from-the-repository"],
        );

        // The person's global config, in a file of its own.
        let global = tmp.path().join("global-config");
        assert_eq!(read_global_ssh_command(Some(&global)), None, "missing file");
        fs::write(&global, "[core]\n\tsshCommand = ssh -i ~/.ssh/work_key\n").unwrap();
        assert_eq!(
            read_global_ssh_command(Some(&global)).as_deref(),
            Some("ssh -i ~/.ssh/work_key")
        );
        fs::write(&global, "[user]\n\tname = t\n").unwrap();
        assert_eq!(read_global_ssh_command(Some(&global)), None, "unset");

        // What git in the repository uses is what the helper passes, not
        // the repository's own value.
        let seen = command(PROGRAM, &r)
            .args(["config", "--get", "core.sshCommand"])
            .output()
            .unwrap();
        let seen = String::from_utf8_lossy(&seen.stdout).trim().to_string();
        assert_eq!(seen, person_ssh_command());
        assert_ne!(seen, "echo from-the-repository");
    }

    #[test]
    fn a_repository_hook_never_runs_under_the_helper() {
        if !git_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path().join("r");
        repo(&r);
        let marker = tmp.path().join("marker");
        let hooks = tmp.path().join("hooks");
        fs::create_dir_all(&hooks).unwrap();
        let hook = hooks.join("post-checkout");
        let shown = marker.display().to_string().replace('\\', "/");
        fs::write(&hook, format!("#!/bin/sh\necho x > '{shown}'\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let hooks_text = hooks.display().to_string().replace('\\', "/");
        setup_git(&r, &["config", "core.hooksPath", &hooks_text]);

        setup_git(&r, &["checkout", "-q", "-b", "one"]);
        assert!(marker.exists(), "plain git runs the repository's hook");
        fs::remove_file(&marker).unwrap();

        let out = command(PROGRAM, &r)
            .args(["checkout", "-q", "-b", "two"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!marker.exists(), "the helper's git ran a hook");
    }

    #[test]
    fn a_worktree_must_name_its_own_git_folder_in_a_known_clone() {
        if !git_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let clone = tmp.path().join("clone");
        repo(&clone);
        let wt = tmp.path().join(".work").join("ITEM").join("src").join("r");
        let other = tmp.path().join(".work").join("OTHER").join("src").join("r");
        for (w, b) in [(&wt, "item"), (&other, "other")] {
            let text = w.display().to_string();
            setup_git(&clone, &["worktree", "add", "-q", "-b", b, &text]);
        }
        let commons = vec![common_dir(&clone).unwrap()];
        assert!(commons[0].ends_with(".git"));
        assert_eq!(
            common_dir(&wt).map(|p| fs::canonicalize(p).unwrap()),
            Some(fs::canonicalize(&commons[0]).unwrap()),
            "a worktree's shared folder is its clone's"
        );
        check_worktree_link(&wt, &commons).unwrap();
        check_worktree_link(&other, &commons).unwrap();

        let unknown = check_worktree_link(&wt, &[tmp.path().join("elsewhere/.git")]);
        assert!(unknown.is_err(), "a clone the caller does not know");

        let saved = fs::read_to_string(wt.join(".git")).unwrap();
        // A git folder of the worktree's own making.
        let fake = tmp.path().join(".work").join("fake");
        repo(&fake);
        fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", fake.join(".git").display()),
        )
        .unwrap();
        let err = check_worktree_link(&wt, &commons).unwrap_err();
        assert!(err.contains("outside the clone's worktrees"), "{err}");

        // Another worktree's git folder: the clone lists it for that one.
        let theirs = fs::read_to_string(other.join(".git")).unwrap();
        fs::write(wt.join(".git"), theirs).unwrap();
        let err = check_worktree_link(&wt, &commons).unwrap_err();
        assert!(err.contains("another worktree"), "{err}");

        fs::write(wt.join(".git"), &saved).unwrap();
        check_worktree_link(&wt, &commons).unwrap();

        // A .git folder in place of the file.
        fs::remove_file(wt.join(".git")).unwrap();
        fs::create_dir(wt.join(".git")).unwrap();
        let err = check_worktree_link(&wt, &commons).unwrap_err();
        assert!(err.contains("not a plain file"), "{err}");
    }

    #[test]
    fn known_repos_judge_the_git_folder_git_would_find() {
        if !git_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        repo(&project);
        let clone = tmp.path().join("clones").join("api");
        repo(&clone);
        let wt = project.join(".work").join("ITEM").join("src").join("api");
        let text = wt.display().to_string();
        setup_git(&clone, &["worktree", "add", "-q", "-b", "item", &text]);
        let known = KnownRepos::new(&project, std::slice::from_ref(&clone));

        known.check(&project).unwrap();
        known.check(&clone).unwrap();
        known.check(&wt).unwrap();
        fs::create_dir_all(wt.join("src")).unwrap();
        known.check(&wt.join("src")).unwrap();
        // A folder under the project with no git folder of its own: git
        // finds the project's.
        let plain = project.join(".work").join("OTHER").join("src").join("api");
        fs::create_dir_all(&plain).unwrap();
        known.check(&plain).unwrap();
        // A git folder planted between it and the project.
        repo(&project.join(".work").join("OTHER"));
        assert!(known.check(&plain).is_err());
        // Somewhere else entirely, with a repository of its own.
        let elsewhere = tmp.path().join("elsewhere");
        repo(&elsewhere);
        assert!(known.check(&elsewhere).is_err());
        // The worktree pointed at a git folder of its own making.
        fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", elsewhere.join(".git").display()),
        )
        .unwrap();
        assert!(known.check(&wt).is_err());
        assert!(known.check(&wt.join("src")).is_err());
    }
}
