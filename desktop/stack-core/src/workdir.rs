//! The work folder (`<project>/.work/` by default) as git sees it.
//!
//! The work folder holds item workspaces, job logs and other build
//! material (the records that decide gates live in the app's own storage,
//! see [`crate::gate_store`]). None of it belongs in a commit, and
//! a project's own `.gitignore` may not say so. So whoever creates the work
//! folder also writes `.work/.gitignore` holding `*`, which makes git ignore
//! the folder and everything in it whatever the project's ignore rules say.
//! An existing `.gitignore` there is the user's and is never overwritten.
//!
//! A scan that crashed can leave its scratch directory behind (hard links
//! to the files it read, a scratch env file) in the scratch folder, or in
//! the work folder where older versions kept it. [`sweep_stale_scratch`]
//! removes those once they are old enough that no scan can still be using
//! them.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// The ignore file written into a new work folder.
pub const IGNORE_FILE: &str = ".gitignore";
/// What it holds: ignore everything in this folder, itself included.
pub const IGNORE_ALL: &str = "*\n";
/// The name every secret-scan scratch entry starts with.
pub const SCRATCH_PREFIX: &str = "secret-scan-";
/// How old a scratch entry must be before a sweep removes it. A scan has a
/// time limit far below this.
pub const STALE_AFTER: Duration = Duration::from_secs(60 * 60);

/// Create `dir` (and its parents) if needed and make sure git ignores it:
/// write `dir/.gitignore` with `*` unless one is already there.
pub fn ensure(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let file = dir.join(IGNORE_FILE);
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&file)
    {
        Ok(mut f) => io::Write::write_all(&mut f, IGNORE_ALL.as_bytes()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

/// Remove the secret-scan scratch entries (directories and files) directly
/// under `dir` last modified more than `older_than` ago. The ones removed.
pub fn sweep_stale_scratch(dir: &Path, older_than: Duration) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let now = SystemTime::now();
    let mut removed = Vec::new();
    for entry in entries.flatten() {
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with(SCRATCH_PREFIX)
        {
            continue;
        }
        let Ok(meta) = fs::symlink_metadata(entry.path()) else {
            continue;
        };
        let age = meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .unwrap_or_default();
        if age < older_than {
            continue;
        }
        let path = entry.path();
        let gone = if meta.is_dir() {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
        if gone.is_ok() {
            removed.push(path);
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_work_folder_ignores_itself_and_an_existing_ignore_file_is_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join(".work");
        ensure(&work).unwrap();
        assert_eq!(fs::read_to_string(work.join(IGNORE_FILE)).unwrap(), "*\n");
        fs::write(work.join(IGNORE_FILE), "mine\n").unwrap();
        ensure(&work).unwrap();
        assert_eq!(
            fs::read_to_string(work.join(IGNORE_FILE)).unwrap(),
            "mine\n"
        );
    }

    #[test]
    fn stale_scratch_is_swept_and_fresh_scratch_and_other_entries_are_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path();
        fs::create_dir_all(work.join("secret-scan-1-0-0/stage")).unwrap();
        fs::write(work.join("secret-scan-1-0-0/stage/a.txt"), "x").unwrap();
        fs::write(work.join("secret-scan-env-1-0.env"), "x").unwrap();
        fs::create_dir_all(work.join("instances")).unwrap();
        // Nothing is an hour old yet: kept.
        assert!(sweep_stale_scratch(work, STALE_AFTER).is_empty());
        assert!(work.join("secret-scan-1-0-0").is_dir());
        // Everything counts as stale with a zero age limit, but only scratch goes.
        let removed = sweep_stale_scratch(work, Duration::ZERO);
        assert_eq!(removed.len(), 2, "{removed:?}");
        assert!(!work.join("secret-scan-1-0-0").exists());
        assert!(!work.join("secret-scan-env-1-0.env").exists());
        assert!(work.join("instances").is_dir());
        assert!(sweep_stale_scratch(&work.join("missing"), Duration::ZERO).is_empty());
    }
}
