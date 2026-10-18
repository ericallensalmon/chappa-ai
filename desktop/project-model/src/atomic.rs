//! One atomic-write helper family shared by every config store (review
//! — previously duplicated line-for-line in `projects` and `settings`, so a
//! durability fix would have had to land twice).
//!
//! Two flavours, deliberately separate:
//! - **Private** (`write_json_atomic`, `write_private_text_atomic`): the
//!   app's own config files. They can hold secrets (agent tool environment
//!   variables, API keys), so on Unix they are created owner-only (0600) and
//!   missing parent directories are created 0700.
//! - **Shared** (`write_text_atomic`): files that live in a user's project
//!   directory (`chappa.yml`) and are meant to be committed and read by
//!   others. These keep the mode of the file they replace, or the umask
//!   default for a new file; they are never narrowed.
//!
//! On Windows nothing changes: the per-user app-data directory is already
//! ACL'd to the user.

use std::fs;
use std::path::Path;

use serde::Serialize;

/// Serialize `value` (pretty) and replace `path` atomically with an
/// owner-only file: write a temp next to the target, then rename — a crash
/// mid-write can never leave a truncated file. Creates the parent directory
/// if missing. For app-private config files only.
pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let json = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    write_private_text_atomic(path, &json, "json.tmp")
}

/// Create `dir` (and parents) if missing and, on Unix, make it owner-only
/// (0700), tightening an existing looser directory. Plain `create_dir_all`
/// elsewhere. Call it on the app's own config/data directory, never on a
/// user-visible one.
pub fn ensure_private_dir(dir: &Path) -> Result<(), String> {
    create_dir_private(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// `create_dir_all`, with directories it creates being 0700 on Unix.
fn create_dir_private(dir: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| e.to_string())
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(dir).map_err(|e| e.to_string())
    }
}

/// Replace `path` with `text` atomically, owner-only on Unix (temp created
/// 0600, so the renamed file is 0600). `tmp_ext` names the temp file's
/// extension, as for [`write_text_atomic`].
pub fn write_private_text_atomic(path: &Path, text: &str, tmp_ext: &str) -> Result<(), String> {
    let dir = parent_of(path)?;
    create_dir_private(dir)?;
    write_via_temp(path, text, tmp_ext, true)
}

/// Replace `path` with `text` atomically (temp next to the target, then
/// rename). `tmp_ext` names the temp file's extension so two stores in one
/// directory (projects.json / settings.json) or a `chappa.yml` never collide on
/// a temp name. The project-file write-back uses this directly: the result
/// keeps the replaced file's mode (or the umask default if new), so a shared
/// `chappa.yml` never becomes owner-only.
pub fn write_text_atomic(path: &Path, text: &str, tmp_ext: &str) -> Result<(), String> {
    let dir = parent_of(path)?;
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    write_via_temp(path, text, tmp_ext, false)
}

fn parent_of(path: &Path) -> Result<&Path, String> {
    path.parent()
        .ok_or_else(|| format!("{} has no parent dir", path.display()))
}

fn write_via_temp(path: &Path, text: &str, tmp_ext: &str, private: bool) -> Result<(), String> {
    use std::io::Write;
    let tmp = path.with_extension(tmp_ext);
    // A stale temp from a crashed run may have a looser mode (open's mode
    // only applies on create) or be a planted link; start from nothing.
    let _ = fs::remove_file(&tmp);
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        if private {
            opts.mode(0o600);
        }
    }
    #[cfg(not(unix))]
    let _ = private;
    let written = (|| -> std::io::Result<()> {
        let mut file = opts.open(&tmp)?;
        file.write_all(text.as_bytes())?;
        #[cfg(unix)]
        if !private {
            // Carry the replaced file's mode over; a new file keeps the
            // umask default it was created with.
            if let Ok(meta) = fs::metadata(path) {
                file.set_permissions(meta.permissions())?;
            }
        }
        Ok(())
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return Err(e.to_string());
    }
    if let Err(e) = fs::rename(&tmp, path) {
        // A failed rename must not leave the temp file behind (
        // review): it would sit next to chappa.yml forever, and a glob like
        // `chappa.yml*` in a restart_when_changed list would keep matching it.
        let _ = fs::remove_file(&tmp);
        return Err(e.to_string());
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(p: &Path) -> u32 {
        fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("atomic-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn private_json_write_is_owner_only_even_over_a_loose_file() {
        let d = scratch("json");
        let p = d.join("agent_tools.json");
        fs::write(&p, "{}").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
        write_json_atomic(&p, &serde_json::json!({"k": "v"})).unwrap();
        assert_eq!(mode(&p), 0o600);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn private_write_ignores_a_loose_stale_temp() {
        let d = scratch("stale");
        let p = d.join("s.json");
        let tmp = p.with_extension("json.tmp");
        fs::write(&tmp, "old").unwrap();
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o666)).unwrap();
        write_json_atomic(&p, &1).unwrap();
        assert_eq!(mode(&p), 0o600);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn private_write_creates_missing_parent_as_0700() {
        let d = scratch("parent");
        let p = d.join("new").join("s.json");
        write_json_atomic(&p, &1).unwrap();
        assert_eq!(mode(p.parent().unwrap()), 0o700);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn ensure_private_dir_tightens_an_existing_directory() {
        let d = scratch("dir");
        fs::set_permissions(&d, fs::Permissions::from_mode(0o755)).unwrap();
        ensure_private_dir(&d).unwrap();
        assert_eq!(mode(&d), 0o700);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn shared_write_keeps_the_replaced_files_mode() {
        let d = scratch("shared");
        let p = d.join("chappa.yml");
        fs::write(&p, "a").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o664)).unwrap();
        write_text_atomic(&p, "b", "yml.chappa-tmp").unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "b");
        assert_eq!(mode(&p), 0o664);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn shared_write_of_a_new_file_gets_the_umask_default() {
        let d = scratch("shared-new");
        let probe = d.join("probe");
        fs::write(&probe, "").unwrap();
        let default_mode = mode(&probe);
        let p = d.join("chappa.yml");
        write_text_atomic(&p, "b", "yml.chappa-tmp").unwrap();
        assert_eq!(mode(&p), default_mode);
        let _ = fs::remove_dir_all(&d);
    }
}
