//! A value the stack runner generated (`{secret:<name>}`) found where it
//! does not belong: in a commit or a tracked file of a worktree.
//!
//! Generated values live in the git-ignored work folder and reach
//! components through the instance env, so the scanner's rules let them
//! pass there (see [`crate::instance_secrets`]). Once one is in a tracked
//! file or a commit it is on its way into the repository, and no rule would
//! recognise it: 48 hex characters look like anything else. So it is looked
//! for by exact match against the values the runner holds, and reported
//! under its own rule, [`GENERATED_RULE`], with the file, the line and (for a
//! commit) the commit. Never the value, nor which secret it was: the
//! finding carries what every other finding carries.
//!
//! Only values of at least [`crate::instance_secrets::MIN_MATCH_LEN`]
//! characters are matched.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use super::process;
use crate::instance_secrets::Secrets;
use crate::safety::{Finding, Location, SafetyError, Severity};

/// The rule a generated value found in a commit or a tracked file is
/// reported under.
pub const GENERATED_RULE: &str = "generated-secret";
/// How long one `git` listing or log may take.
const GIT_TIMEOUT: Duration = Duration::from_secs(300);
/// The most `git log -p` output read for one range.
const LOG_CAP: usize = 256 * 1024 * 1024;
/// Tracked files larger than this are not read (the listing's per-file cap).
const MAX_FILE_BYTES: u64 = super::listing::DEFAULT_MAX_FILE_BYTES;

fn finding(file: &str, line: u32, commit: Option<&str>, count: u32) -> Finding {
    Finding {
        rule: GENERATED_RULE.to_string(),
        location: Location::Source {
            file: PathBuf::from(file),
            line: Some(line),
            commit: commit.map(str::to_owned),
        },
        count,
        severity: Severity::Bad,
    }
}

fn git(git: &Path, root: &Path) -> Command {
    let mut cmd = Command::new(git);
    crate::child_env::scrub(&mut cmd);
    cmd.arg("-C").arg(root);
    cmd
}

/// The lines (1-based) of `text` holding a value, with how many values
/// each holds.
fn lines_with(text: &[u8], secrets: &Secrets) -> Vec<(u32, u32)> {
    let text = String::from_utf8_lossy(text);
    let mut out = Vec::new();
    for (i, line) in text.split('\n').enumerate() {
        let n: usize = secrets.matchable().map(|v| line.matches(v).count()).sum();
        if n > 0 {
            out.push((i as u32 + 1, n as u32));
        }
    }
    out
}

/// Every tracked file of the repository at `root` (only those in `only`,
/// paths relative to `root` with `/`, when given) that holds a generated
/// value, by file and line. A folder that is not in a repository has no
/// tracked files.
pub fn find_in_tracked(
    git_program: &Path,
    root: &Path,
    only: Option<&BTreeSet<String>>,
    secrets: &Secrets,
) -> Result<Vec<Finding>, SafetyError> {
    if secrets.longest() == 0 {
        return Ok(Vec::new());
    }
    let ran = process::run(
        git(git_program, root).args(["ls-files", "-z", "--cached", "--full-name"]),
        "git ls-files",
        GIT_TIMEOUT,
        64 * 1024 * 1024,
    );
    let Ok(ran) = ran else {
        return Ok(Vec::new());
    };
    if ran.status != Some(0) {
        return Ok(Vec::new());
    }
    // `--full-name` names files from the repository's top; make them
    // relative to `root` again.
    let prefix = repo_prefix(git_program, root);
    let mut out = Vec::new();
    for raw in ran.stdout.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let Ok(full) = std::str::from_utf8(raw) else {
            continue;
        };
        let rel = match prefix.as_deref() {
            Some(p) if !p.is_empty() => match full.strip_prefix(p) {
                Some(rest) => rest.to_string(),
                None => continue,
            },
            _ => full.to_string(),
        };
        if only.is_some_and(|set| !set.contains(&rel)) {
            continue;
        }
        let path = root.join(&rel);
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
            continue;
        }
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };
        for (line, count) in lines_with(&bytes, secrets) {
            out.push(finding(&rel, line, None, count));
        }
    }
    Ok(out)
}

/// `root`'s path inside its repository (`git rev-parse --show-prefix`),
/// `/`-terminated, empty at the top.
fn repo_prefix(git_program: &Path, root: &Path) -> Option<String> {
    let ran = process::run(
        git(git_program, root).args(["rev-parse", "--show-prefix"]),
        "git rev-parse",
        Duration::from_secs(20),
        64 * 1024,
    )
    .ok()?;
    (ran.status == Some(0)).then(|| String::from_utf8_lossy(&ran.stdout).trim().to_string())
}

/// Every added line of the commits in `range` (`<base>..HEAD`, checked by
/// the caller) that holds a generated value, by file, line and commit. A
/// value committed and then deleted is still found: it is in the history
/// a push would carry.
pub fn find_in_commits(
    git_program: &Path,
    root: &Path,
    range: &str,
    secrets: &Secrets,
) -> Result<Vec<Finding>, SafetyError> {
    if secrets.longest() == 0 {
        return Ok(Vec::new());
    }
    let ran = process::run(
        git(git_program, root).args([
            "-c",
            "core.quotePath=false",
            "log",
            "-p",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            "--unified=0",
            "--format=format:%x00%H",
            "--end-of-options",
            range,
            "--",
        ]),
        "git log",
        GIT_TIMEOUT,
        LOG_CAP,
    )?;
    if ran.status != Some(0) {
        return Err(SafetyError::Scanner(format!(
            "git log of {range} in {} failed",
            root.display()
        )));
    }
    if ran.stdout_overflowed {
        return Err(SafetyError::Scanner(format!(
            "the commits of {range} are too large to read"
        )));
    }
    Ok(parse_log(&ran.stdout, secrets))
}

/// The findings in `git log -p --unified=0 --format=%x00%H` output.
fn parse_log(log: &[u8], secrets: &Secrets) -> Vec<Finding> {
    let text = String::from_utf8_lossy(log);
    let mut out: Vec<Finding> = Vec::new();
    let mut commit: Option<String> = None;
    let mut file: Option<String> = None;
    let mut next_line: u32 = 0;
    for line in text.split('\n') {
        if let Some(sha) = line.strip_prefix('\0') {
            commit = Some(sha.trim().to_string());
            file = None;
            continue;
        }
        if line.starts_with("diff --git ") {
            file = None;
            continue;
        }
        if let Some(path) = line.strip_prefix("+++ ") {
            file = path.strip_prefix("b/").map(str::to_owned);
            continue;
        }
        if line.starts_with("--- ") {
            continue;
        }
        if let Some(hunk) = line.strip_prefix("@@ ") {
            next_line = hunk_start(hunk).unwrap_or(0);
            continue;
        }
        if let Some(added) = line.strip_prefix('+') {
            if let Some(f) = &file {
                let n: usize = secrets.matchable().map(|v| added.matches(v).count()).sum();
                if n > 0 {
                    out.push(finding(f, next_line, commit.as_deref(), n as u32));
                }
            }
            next_line = next_line.saturating_add(1);
        } else if line.starts_with(' ') {
            next_line = next_line.saturating_add(1);
        }
    }
    out
}

/// The new side's first line from a hunk header's `-a,b +c,d @@`.
fn hunk_start(hunk: &str) -> Option<u32> {
    let plus = hunk.split_whitespace().find(|w| w.starts_with('+'))?;
    plus[1..].split(',').next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{git, git_available, write};
    use super::*;

    fn value(seed: &str) -> String {
        seed.repeat(48 / seed.len())
    }

    #[test]
    fn a_generated_value_in_a_tracked_file_or_a_commit_is_found_by_place_never_by_value() {
        if !git_available() {
            eprintln!("skipped: git is not on PATH");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        let v = value("9f3c");
        let secrets = Secrets::from_pairs([("app-0/db", v.as_str()), ("app-0/tiny", "abc")]);
        git(&repo, &["init", "-q"]);
        write(&repo, "README", "hello\n");
        write(&repo, ".gitignore", ".env\n");
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "start"]);
        git(&repo, &["checkout", "-q", "-b", "item"]);
        // Committed, then deleted again: still in the range.
        write(
            &repo,
            "cfg/db.txt",
            &format!("a\nurl=postgres://app:{v}@db/app\n"),
        );
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "add"]);
        fs::remove_file(repo.join("cfg/db.txt")).unwrap();
        git(&repo, &["commit", "-q", "-am", "remove"]);
        // Tracked and edited now; an ignored file holding it is not tracked.
        write(&repo, "src/app.txt", &format!("x\ny\npass {v} and {v}\n"));
        git(&repo, &["add", "src/app.txt"]);
        write(&repo, ".env", &format!("DB={v}\n"));
        // A short value matches nothing.
        write(&repo, "README", "hello abc\n");

        let tracked = find_in_tracked(Path::new("git"), &repo, None, &secrets).unwrap();
        let places: Vec<String> = tracked.iter().map(|f| f.location.to_string()).collect();
        assert_eq!(places, ["src/app.txt:3"], "{tracked:?}");
        assert_eq!(tracked[0].count, 2);
        assert_eq!(tracked[0].rule, GENERATED_RULE);
        // From a subfolder, files are named from that folder.
        let sub = find_in_tracked(Path::new("git"), &repo.join("src"), None, &secrets).unwrap();
        assert_eq!(sub[0].location.to_string(), "app.txt:3");
        let only: BTreeSet<String> = ["README".to_string()].into();
        assert!(
            find_in_tracked(Path::new("git"), &repo, Some(&only), &secrets)
                .unwrap()
                .is_empty()
        );

        let commits = find_in_commits(Path::new("git"), &repo, "main..HEAD", &secrets).unwrap();
        assert_eq!(commits.len(), 1, "{commits:?}");
        match &commits[0].location {
            Location::Source { file, line, commit } => {
                assert_eq!(file, Path::new("cfg/db.txt"));
                assert_eq!(*line, Some(2));
                assert_eq!(commit.as_ref().map(String::len), Some(40));
            }
            other => panic!("{other:?}"),
        }
        let shown = format!("{tracked:?}{commits:?}");
        assert!(
            !shown.contains(&v) && !shown.contains("app-0/db"),
            "{shown}"
        );
        // Outside a repository: nothing tracked.
        let bare = tmp.path().join("bare");
        fs::create_dir_all(&bare).unwrap();
        write(&bare, "x.txt", &v);
        assert!(find_in_tracked(Path::new("git"), &bare, None, &secrets)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn hunk_headers_give_the_new_line() {
        assert_eq!(hunk_start("-0,0 +1,2 @@"), Some(1));
        assert_eq!(hunk_start("-3 +7 @@ fn x()"), Some(7));
        let v = value("ab12");
        let s = Secrets::from_pairs([("i/x", v.as_str())]);
        let log = format!(
            "\0{sha}\ndiff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +4,2 @@\n-old\n+keep\n+has {v}\n",
            sha = "c".repeat(40)
        );
        let f = parse_log(log.as_bytes(), &s);
        assert_eq!(f.len(), 1);
        assert_eq!(
            f[0].location.to_string(),
            format!("a.txt:5 @{}", "c".repeat(8))
        );
    }
}
