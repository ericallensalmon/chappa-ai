//! Files and folders only the current user can read: the app's control
//! token and the secrets the stack runner generates per instance.
//!
//! On Unix that is the mode bits, set when the file or folder is created
//! (`0600` / `0700`), so it is never readable by anyone else, not even for
//! an instant. On Windows a new file takes the access list its folder passes
//! down (often every user of the machine can read a project folder), so
//! [`restrict`] rewrites it: the current user's full-control grant becomes
//! the only entry. A caller that cannot restrict a secret's file must not
//! keep the file (fail closed): [`write_new`] removes what it wrote when the
//! restriction fails.

use std::fs;
use std::io::{self, Write};
use std::path::Path;

/// Leave the current user as the only one who can open `path` (a file or a
/// folder).
///
/// Windows: `icacls` ships with every Windows; shelling out keeps the crate
/// free of the Win32 security API. Two steps, because a new file can carry
/// explicit entries that `/inheritance:r` keeps: where the folder passes
/// nothing down, Windows writes the creating token's default access list
/// onto the file (SYSTEM and Administrators for an elevated account).
/// `/reset` drops every explicit entry, leaving only what the folder passes
/// down; `/inheritance:r` then strips that, and the grant is all that is
/// left. A folder's grant is for the folder itself and passes nothing down:
/// each file in it is restricted on its own.
///
/// `icacls` is run by its full System32 path, never resolved by name, so a
/// same-named binary next to the program or earlier on `PATH` can't stand
/// in for it. The grant names `DOMAIN\user` when `USERDOMAIN` is set: a bare
/// name is looked up on the local machine first, so on a domain-joined
/// machine a local account sharing the user's name would otherwise get the
/// grant.
///
/// Unix: `0700` for a folder, `0600` for anything else.
#[cfg(windows)]
pub fn restrict(path: &Path) -> io::Result<()> {
    let user = std::env::var("USERNAME")
        .map_err(|_| io::Error::other("USERNAME not set; cannot restrict the access list"))?;
    let principal = match std::env::var("USERDOMAIN") {
        Ok(domain) if !domain.is_empty() => format!("{domain}\\{user}"),
        _ => user,
    };
    let icacls = icacls_path()?;
    let run = |args: &[std::ffi::OsString]| -> io::Result<()> {
        let mut cmd = std::process::Command::new(&icacls);
        crate::child_env::scrub(&mut cmd);
        let output = cmd.arg(path).args(args).output()?;
        if output.status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "icacls failed on {}: {}",
                path.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            )))
        }
    };
    run(&["/reset".into()])?;
    run(&[
        "/inheritance:r".into(),
        "/grant:r".into(),
        format!("{principal}:F").into(),
    ])
}

#[cfg(unix)]
pub fn restrict(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if fs::metadata(path)?.is_dir() {
        0o700
    } else {
        0o600
    };
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(not(any(unix, windows)))]
pub fn restrict(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// `%SystemRoot%\System32\icacls.exe` (`windir` as the fallback). With
/// neither set there is no trustworthy location, so this fails rather than
/// falling back to a `PATH` lookup.
#[cfg(windows)]
fn icacls_path() -> io::Result<std::path::PathBuf> {
    let root = std::env::var_os("SystemRoot")
        .or_else(|| std::env::var_os("windir"))
        .filter(|r| !r.is_empty())
        .ok_or_else(|| io::Error::other("SystemRoot not set; cannot locate icacls"))?;
    Ok(std::path::PathBuf::from(root)
        .join("System32")
        .join("icacls.exe"))
}

/// Create `dir` (and its parents) as an owner-only folder. One that already
/// exists is restricted again, so a folder made before it was restricted
/// (or by hand) ends up owner-only too.
pub fn create_dir(dir: &Path) -> io::Result<()> {
    if let Some(parent) = dir.parent() {
        fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        match fs::DirBuilder::new().mode(0o700).create(dir) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            other => other?,
        }
    }
    #[cfg(not(unix))]
    fs::create_dir_all(dir)?;
    restrict(dir)
}

/// Write `bytes` to the new file `path` (it must not exist) as an
/// owner-only file. On Unix the mode is set at creation; elsewhere the file
/// is restricted right after it is written and before anything else can
/// rely on it. When that fails the file is removed and the error returned:
/// a secret is never left readable by others.
pub fn write_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut o = fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    // A file that is already there is never touched (nor removed below).
    let mut f = o.open(path)?;
    let result = (|| {
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        restrict(path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(path);
    }
    result
}

/// What `icacls` lists for `path`: one line per access entry.
#[cfg(all(test, windows))]
pub(crate) fn access_entries(path: &Path) -> (Vec<String>, String) {
    let out = std::process::Command::new("icacls")
        .arg(path)
        .output()
        .expect("icacls");
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let entries = text
        .lines()
        .filter(|l| l.contains(":(") || l.contains(":F") || l.contains("(F)"))
        .map(str::to_owned)
        .collect();
    (entries, text)
}

/// The strict check: exactly one entry, not inherited, the current user's.
#[cfg(all(test, windows))]
pub(crate) fn assert_owner_only(path: &Path) {
    let (entries, text) = access_entries(path);
    assert!(
        !text.contains("(I)"),
        "inherited entries must be stripped: {text}"
    );
    assert_eq!(entries.len(), 1, "exactly one entry (the owner): {text}");
    let user = std::env::var("USERNAME").unwrap();
    assert!(
        entries[0].to_lowercase().contains(&user.to_lowercase()),
        "the single entry must be the current user: {text}"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_file_and_folder_are_owner_only_and_a_file_is_never_overwritten() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("a").join("private");
        create_dir(&dir).unwrap();
        create_dir(&dir).unwrap();
        let file = dir.join("value");
        write_new(&file, b"x").unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"x");
        let again = write_new(&file, b"y").unwrap_err();
        assert_eq!(again.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&file).unwrap(), b"x", "never overwritten");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&dir), 0o700);
            assert_eq!(mode(&file), 0o600);
        }
        #[cfg(windows)]
        {
            assert_owner_only(&dir);
            assert_owner_only(&file);
        }
    }

    #[cfg(windows)]
    #[test]
    fn explicit_entries_a_file_was_given_are_dropped_too() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("t");
        fs::write(&file, "x").unwrap();
        // The entries an elevated creator leaves behind as explicit ones.
        let out = std::process::Command::new("icacls")
            .arg(&file)
            .args(["/grant", "*S-1-5-18:F", "/grant", "*S-1-5-32-544:F"])
            .output()
            .unwrap();
        assert!(out.status.success());
        restrict(&file).unwrap();
        assert_owner_only(&file);
    }
}
