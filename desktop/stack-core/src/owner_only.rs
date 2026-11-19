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
//! restriction fails. [`write_new`] creates the file empty, restricts it and
//! only then writes, so the secret never sits under the folder's access
//! list; [`is_owner_only`] tells whether a file found on disk can be
//! trusted to have stayed private.

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
/// in for it. The grant names the user by the SID of this process's own
/// token (`*S-1-5-21-…`), never by `USERNAME` / `USERDOMAIN`: those are
/// plain environment variables any parent can set, and a name is looked up
/// (a local account sharing the user's name would match first).
///
/// Unix: `0700` for a folder, `0600` for anything else.
#[cfg(windows)]
pub fn restrict(path: &Path) -> io::Result<()> {
    let principal = format!("*{}", win::current_user_sid()?);
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
/// owner-only file. The file is created EMPTY, restricted, and only then
/// written: the secret never exists under any other access list. On Unix
/// the mode is set at creation as well. On Windows the file is opened with
/// no sharing for the whole sequence, so nobody can open it while it still
/// carries the folder's access list and keep that handle to read the
/// secret later (the access list is changed on the path; it does not need
/// a handle that shares data access). When any step fails the file is
/// removed and the error returned: a secret is never left readable by
/// others.
pub fn write_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut o = fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        o.share_mode(0);
    }
    // A file that is already there is never touched (nor removed below).
    let mut f = o.open(path)?;
    let result = (|| {
        restrict(path)?;
        f.write_all(bytes)?;
        f.sync_all()
    })();
    drop(f);
    if result.is_err() {
        let _ = fs::remove_file(path);
    }
    result
}

/// Whether `path` is open to the current user alone: on Unix no group or
/// other permission bit; on Windows an access list that inherits nothing
/// and holds only allow entries for this process's own user. A file that is
/// not (made by hand, copied in, or written by an older build under a
/// folder anyone can read) may already have been read by someone else, so
/// a secret in it is replaced rather than reused.
#[cfg(unix)]
pub fn is_owner_only(path: &Path) -> io::Result<bool> {
    use std::os::unix::fs::PermissionsExt;
    Ok(fs::metadata(path)?.permissions().mode() & 0o077 == 0)
}

#[cfg(windows)]
pub fn is_owner_only(path: &Path) -> io::Result<bool> {
    win::is_owner_only(path)
}

#[cfg(not(any(unix, windows)))]
pub fn is_owner_only(_path: &Path) -> io::Result<bool> {
    Ok(true)
}

/// The Win32 calls behind the Windows side: the process token's user SID
/// and the file's access list as the system holds it.
#[cfg(windows)]
mod win {
    use std::io;
    use std::path::Path;
    use std::sync::OnceLock;

    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, ERROR_SUCCESS, HANDLE};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, GetNamedSecurityInfoW, SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::{
        AclSizeInformation, GetAce, GetAclInformation, GetSecurityDescriptorControl,
        GetTokenInformation, TokenUser, ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, ACL_SIZE_INFORMATION,
        DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED, TOKEN_QUERY,
        TOKEN_USER,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    /// `ACCESS_ALLOWED_ACE_TYPE`.
    const ACCESS_ALLOWED: u8 = 0;

    /// The string form of a SID (`S-1-5-21-…`).
    ///
    /// # Safety
    /// `sid` must point at a valid SID.
    unsafe fn sid_string(sid: PSID) -> io::Result<String> {
        let mut wide: *mut u16 = std::ptr::null_mut();
        if ConvertSidToStringSidW(sid, &mut wide) == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut len = 0;
        while *wide.add(len) != 0 {
            len += 1;
        }
        let text = String::from_utf16_lossy(std::slice::from_raw_parts(wide, len));
        LocalFree(wide.cast());
        Ok(text)
    }

    /// The user SID of this process's token, read once.
    pub fn current_user_sid() -> io::Result<String> {
        static SID: OnceLock<String> = OnceLock::new();
        if let Some(sid) = SID.get() {
            return Ok(sid.clone());
        }
        let sid = read_token_user()?;
        Ok(SID.get_or_init(|| sid).clone())
    }

    fn read_token_user() -> io::Result<String> {
        // SAFETY: plain Win32 calls on this process's own token; the buffer
        // is sized by the first call and aligned for TOKEN_USER (u64s).
        unsafe {
            let mut token: HANDLE = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut needed = 0u32;
            GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed);
            let mut buf = vec![0u64; (needed as usize).div_ceil(8).max(1)];
            let ok = GetTokenInformation(
                token,
                TokenUser,
                buf.as_mut_ptr().cast(),
                (buf.len() * 8) as u32,
                &mut needed,
            );
            let err = io::Error::last_os_error();
            CloseHandle(token);
            if ok == 0 {
                return Err(err);
            }
            let user = &*(buf.as_ptr() as *const TOKEN_USER);
            sid_string(user.User.Sid)
        }
    }

    pub fn is_owner_only(path: &Path) -> io::Result<bool> {
        use std::os::windows::ffi::OsStrExt;
        let me = current_user_sid()?;
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: the out-pointers are valid; the descriptor the call
        // allocates is freed with LocalFree once read, and the DACL and ACE
        // pointers into it are only used before that.
        unsafe {
            let mut dacl: *mut ACL = std::ptr::null_mut();
            let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            let rc = GetNamedSecurityInfoW(
                wide.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut dacl,
                std::ptr::null_mut(),
                &mut sd,
            );
            if rc != ERROR_SUCCESS {
                return Err(io::Error::from_raw_os_error(rc as i32));
            }
            let verdict = (|| -> io::Result<bool> {
                // No DACL at all grants everyone everything.
                if dacl.is_null() {
                    return Ok(false);
                }
                let mut control = 0u16;
                let mut revision = 0u32;
                if GetSecurityDescriptorControl(sd, &mut control, &mut revision) == 0 {
                    return Err(io::Error::last_os_error());
                }
                if control & SE_DACL_PROTECTED == 0 {
                    return Ok(false);
                }
                let mut info = ACL_SIZE_INFORMATION::default();
                if GetAclInformation(
                    dacl,
                    (&mut info as *mut ACL_SIZE_INFORMATION).cast(),
                    std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                    AclSizeInformation,
                ) == 0
                {
                    return Err(io::Error::last_os_error());
                }
                if info.AceCount == 0 {
                    return Ok(false);
                }
                for i in 0..info.AceCount {
                    let mut ace: *mut core::ffi::c_void = std::ptr::null_mut();
                    if GetAce(dacl, i, &mut ace) == 0 {
                        return Err(io::Error::last_os_error());
                    }
                    let header = &*(ace as *const ACE_HEADER);
                    if header.AceType != ACCESS_ALLOWED {
                        return Ok(false);
                    }
                    let allowed = ace as *const ACCESS_ALLOWED_ACE;
                    let sid = std::ptr::addr_of!((*allowed).SidStart) as PSID;
                    if sid_string(sid)? != me {
                        return Ok(false);
                    }
                }
                Ok(true)
            })();
            LocalFree(sd);
            verdict
        }
    }
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
    assert!(is_owner_only(path).unwrap(), "{}", path.display());
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

    /// A file left with the folder's access list (or wider mode bits) is
    /// not owner-only; once restricted it is.
    #[test]
    fn a_plain_file_is_not_owner_only_until_restricted() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("plain");
        fs::write(&file, "x").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        }
        assert!(!is_owner_only(&file).unwrap());
        restrict(&file).unwrap();
        assert!(is_owner_only(&file).unwrap());
    }

    /// The new file is restricted before a byte of the secret is in it, and
    /// on Windows nobody else can open it while that happens.
    #[cfg(windows)]
    #[test]
    fn a_new_file_cannot_be_opened_by_anyone_else_while_it_is_written() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("secret");
        let mut o = fs::OpenOptions::new();
        o.write(true).create_new(true);
        {
            use std::os::windows::fs::OpenOptionsExt;
            o.share_mode(0);
        }
        let held = o.open(&file).unwrap();
        let err = fs::File::open(&file).unwrap_err();
        assert_eq!(err.raw_os_error(), Some(32), "a sharing violation: {err}");
        restrict(&file).expect("the access list changes on a file opened unshared");
        drop(held);
        assert_owner_only(&file);
        fs::remove_file(&file).unwrap();
        write_new(&file, b"s").unwrap();
        assert_owner_only(&file);
        assert_eq!(fs::read(&file).unwrap(), b"s");
    }

    #[cfg(windows)]
    #[test]
    fn the_grant_names_the_process_token_user_by_sid() {
        let sid = win::current_user_sid().unwrap();
        // Compared against an independent source rather than a prefix:
        // local and domain accounts are S-1-5-21-…, directory-joined
        // cloud accounts are S-1-12-1-…. The full path keeps a `whoami`
        // from another toolset on PATH out of it.
        let system_root = std::env::var_os("SystemRoot").expect("SystemRoot is set");
        let out = std::process::Command::new(
            std::path::Path::new(&system_root)
                .join("System32")
                .join("whoami.exe"),
        )
        .args(["/user", "/fo", "csv", "/nh"])
        .output()
        .unwrap();
        assert!(out.status.success());
        let line = String::from_utf8_lossy(&out.stdout);
        let expected = line
            .trim()
            .rsplit(',')
            .next()
            .unwrap()
            .trim_matches('"')
            .to_string();
        assert_eq!(sid, expected);
        // The environment cannot redirect the grant.
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("t");
        fs::write(&file, "x").unwrap();
        restrict(&file).unwrap();
        assert!(is_owner_only(&file).unwrap());
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
