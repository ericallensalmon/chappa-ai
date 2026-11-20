//! App-private storage for the state that decides whether a gate opens.
//!
//! Acknowledged secret-scan findings, data-safety verdicts and their
//! acknowledgements, job plans waiting for their runner, an item's workflow
//! state (approvals, human gates) and the slot ↔ item binding all decide
//! whether something is allowed. None of it may live in the project's work
//! folder: anything working in the project (an agent above all) can write
//! there, and a file it writes would then count as a person's decision. So
//! these records live here instead, in an owner-only folder under the app's
//! own config folder, outside every project:
//!
//! ```text
//! <root>/
//!   <project key>/                  one per project (sha-256 of its canonical path)
//!     project.txt                   the project's path, for a person looking in
//!     project/<name>.json           records about the whole project
//!     instances/<instance>/<name>.json
//!   scratch/                        the secret scanner's per-run scratch folders
//! ```
//!
//! Every folder and file here is owner-only ([`crate::owner_only`]); a
//! record that is not (made by hand, copied in) is refused when read, never
//! trusted. A write replaces the record atomically: the new text goes into
//! a fresh owner-only file beside it, which is then renamed over it, so a
//! reader sees the old record or the new one, never half of either.
//!
//! Build and worktree material (item folders, clones, seeds, logs,
//! generated files) stays in the work folder; only records that open gates
//! come here.
//!
//! The app and the stack runner must agree on the root. Each process
//! [`install`]s it once at startup (the app from its config folder, the
//! runner from the same folder or the `--state-dir` it was started with);
//! code that needs a record asks [`GateStore::installed`]. A process that
//! never installed one gets an error, so a gate reads as "cannot be
//! judged", never as open.

use serde::de::DeserializeOwned;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

/// The folder under the app's config folder that holds the store.
pub const DIR_NAME: &str = "gate-state";
/// The scanner's scratch space, under the root.
pub const SCRATCH_DIR: &str = "scratch";
/// The project-wide records' folder.
const PROJECT_DIR: &str = "project";
/// The per-instance records' parent folder.
const INSTANCES_DIR: &str = "instances";
/// The file naming the project a key stands for.
const PROJECT_NOTE: &str = "project.txt";
/// The longest part of a record name.
const NAME_CAP: usize = 120;

/// Why a record could not be read or written.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GateError {
    #[error("no app-private gate storage is set up in this process")]
    NotInstalled,
    #[error("`{0}` is not a usable gate record name")]
    BadName(String),
    #[error("{path}: {message}")]
    Io { path: PathBuf, message: String },
    #[error("{path} cannot be read: {message}")]
    Json { path: PathBuf, message: String },
    #[error("{0} is not private to this user, so it is not trusted")]
    NotPrivate(PathBuf),
}

impl GateError {
    fn io(path: &Path, e: io::Error) -> GateError {
        GateError::Io {
            path: path.to_path_buf(),
            message: e.to_string(),
        }
    }
}

/// Which records of a project: the project's own, or one instance's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope<'a> {
    Project,
    Instance(&'a str),
}

/// The store: a root folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateStore {
    root: PathBuf,
}

static INSTALLED: OnceLock<PathBuf> = OnceLock::new();

/// Set this process's store root. Once: a second call with another root
/// is refused (two roots would split the state), the same root again is
/// fine.
pub fn install(root: impl Into<PathBuf>) -> Result<(), String> {
    let root = root.into();
    let root = std::path::absolute(&root).unwrap_or(root);
    let set = INSTALLED.get_or_init(|| root.clone());
    if *set == root {
        Ok(())
    } else {
        Err(format!(
            "gate storage is already at {}, not {}",
            set.display(),
            root.display()
        ))
    }
}

/// The root for an app config folder.
pub fn root_in(config_dir: &Path) -> PathBuf {
    config_dir.join(DIR_NAME)
}

/// Install a fresh folder under the system temp folder as the root unless
/// one is installed already; the root in use. For tests: every test of a
/// process shares it, and records are keyed by project, so tests that use
/// their own project folders never see each other's.
#[cfg(any(test, feature = "test-support"))]
pub fn install_for_tests() -> PathBuf {
    INSTALLED
        .get_or_init(|| {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0);
            std::env::temp_dir().join(format!("chappa-gate-test-{}-{nanos}", std::process::id()))
        })
        .clone()
}

/// The records of the folder `dir` in this process's installed store.
pub fn for_dir(dir: &Path) -> Result<GateProject, GateError> {
    Ok(GateStore::installed()?.project(dir))
}

impl GateStore {
    pub fn new(root: impl Into<PathBuf>) -> GateStore {
        GateStore { root: root.into() }
    }

    /// The store this process installed.
    pub fn installed() -> Result<GateStore, GateError> {
        INSTALLED
            .get()
            .map(|r| GateStore::new(r.clone()))
            .ok_or(GateError::NotInstalled)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The records of the project in `project_dir`, keyed by its canonical
    /// path (its absolute path when it cannot be resolved), so every
    /// spelling of one folder finds the same records.
    pub fn project(&self, project_dir: &Path) -> GateProject {
        let canonical = canonical(project_dir);
        let key = project_key(&canonical);
        GateProject {
            root: self.root.clone(),
            dir: self.root.join(key),
            project: canonical,
        }
    }

    /// An owner-only folder for scanner scratch, outside every project.
    pub fn scratch_dir(&self) -> Result<PathBuf, GateError> {
        ensure_private(&self.root)?;
        let dir = self.root.join(SCRATCH_DIR);
        ensure_private(&dir)?;
        Ok(dir)
    }
}

/// The records of one project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateProject {
    root: PathBuf,
    dir: PathBuf,
    project: PathBuf,
}

impl GateProject {
    /// The project's folder as the key was made from.
    pub fn project_dir(&self) -> &Path {
        &self.project
    }

    /// The folder holding this project's records.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn scope_dir(&self, scope: Scope<'_>) -> PathBuf {
        match scope {
            Scope::Project => self.dir.join(PROJECT_DIR),
            Scope::Instance(i) => self.dir.join(INSTANCES_DIR).join(encode_key(i)),
        }
    }

    /// The file a record lives in. `name` is one or two parts joined by
    /// `/` (a collection and a record), each a safe file name.
    pub fn path(&self, scope: Scope<'_>, name: &str) -> Result<PathBuf, GateError> {
        check_name(name)?;
        let mut path = self.scope_dir(scope);
        for part in name.split('/') {
            path.push(part);
        }
        path.set_extension("json");
        Ok(path)
    }

    /// Read a record; `None` when there is none. A record that is not
    /// owner-only, or does not parse, is an error: the caller treats the
    /// gate as one that cannot be judged.
    pub fn read<T: DeserializeOwned>(
        &self,
        scope: Scope<'_>,
        name: &str,
    ) -> Result<Option<T>, GateError> {
        let path = self.path(scope, name)?;
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(GateError::io(&path, e)),
        };
        match crate::owner_only::is_owner_only(&path) {
            Ok(true) => {}
            Ok(false) => return Err(GateError::NotPrivate(path)),
            Err(e) => return Err(GateError::io(&path, e)),
        }
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| GateError::Json {
                path: path.clone(),
                message: crate::safety::json_brief(&e),
            })
    }

    /// Whether a record exists (readable or not).
    pub fn exists(&self, scope: Scope<'_>, name: &str) -> bool {
        self.path(scope, name).is_ok_and(|p| p.is_file())
    }

    /// Write a record, replacing any before it atomically. The folders on
    /// the way are made owner-only.
    pub fn write<T: Serialize>(
        &self,
        scope: Scope<'_>,
        name: &str,
        value: &T,
    ) -> Result<(), GateError> {
        let path = self.path(scope, name)?;
        let mut text = serde_json::to_string_pretty(value).map_err(|e| GateError::Json {
            path: path.clone(),
            message: e.to_string(),
        })?;
        text.push('\n');
        self.ensure_dirs(&path)?;
        replace_private(&path, text.as_bytes())
    }

    /// Remove a record; whether it was there.
    pub fn remove(&self, scope: Scope<'_>, name: &str) -> Result<bool, GateError> {
        let path = self.path(scope, name)?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(GateError::io(&path, e)),
        }
    }

    /// The record names in a collection (`<collection>/<name>`), sorted.
    pub fn list(&self, scope: Scope<'_>, collection: &str) -> Result<Vec<String>, GateError> {
        check_part(collection)?;
        let dir = self.scope_dir(scope).join(collection);
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(GateError::io(&dir, e)),
        };
        let mut names: Vec<String> = entries
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let stem = name.strip_suffix(".json")?;
                check_part(stem).ok()?;
                Some(stem.to_string())
            })
            .collect();
        names.sort();
        Ok(names)
    }

    /// Make every folder from the root down to `path`'s owner-only, and
    /// leave the project's path beside its records.
    fn ensure_dirs(&self, path: &Path) -> Result<(), GateError> {
        ensure_private(&self.root)?;
        let fresh = !self.dir.is_dir();
        ensure_private(&self.dir)?;
        if fresh {
            let note = self.dir.join(PROJECT_NOTE);
            let text = format!("{}\n", self.project.display());
            match crate::owner_only::write_new(&note, text.as_bytes()) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(GateError::io(&note, e)),
            }
        }
        let parent = path.parent().expect("a record path has a folder");
        let rel = parent.strip_prefix(&self.dir).unwrap_or(Path::new(""));
        let mut dir = self.dir.clone();
        for part in rel.components() {
            dir.push(part);
            ensure_private(&dir)?;
        }
        Ok(())
    }
}

/// `dir` exists and is owner-only: made so when it is not.
fn ensure_private(dir: &Path) -> Result<(), GateError> {
    if dir.is_dir() && crate::owner_only::is_owner_only(dir).unwrap_or(false) {
        return Ok(());
    }
    crate::owner_only::create_dir(dir).map_err(|e| GateError::io(dir, e))
}

/// Write `bytes` into a fresh owner-only file beside `path` and rename it
/// over `path`.
fn replace_private(path: &Path, bytes: &[u8]) -> Result<(), GateError> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!(
        ".{name}.{}-{}-{nanos}.tmp",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    crate::owner_only::write_new(&tmp, bytes).map_err(|e| GateError::io(&tmp, e))?;
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        GateError::io(path, e)
    })
}

/// `dir` with its deepest existing ancestor resolved and the rest appended
/// as spelled. Resolving only a folder that exists would key a folder one
/// way before it is created and another way after (`/var` resolves to
/// `/private/var` on macOS; Windows resolves short names and adds its
/// `\\?\` prefix), and a record written before the work folder exists
/// must still be found once it does.
fn canonical(dir: &Path) -> PathBuf {
    use std::path::Component;
    let abs = std::path::absolute(dir).unwrap_or_else(|_| dir.to_path_buf());
    let mut base = abs.as_path();
    let mut tail = Vec::new();
    loop {
        if let Ok(mut out) = fs::canonicalize(base) {
            for part in tail.iter().rev() {
                match part {
                    Component::ParentDir => {
                        out.pop();
                    }
                    Component::Normal(name) => out.push(name),
                    _ => {}
                }
            }
            return out;
        }
        let (Some(parent), Some(last)) = (base.parent(), base.components().next_back()) else {
            return abs;
        };
        tail.push(last);
        base = parent;
    }
}

/// The folder name a project's records live under.
pub fn project_key(canonical: &Path) -> String {
    let digest = Sha256::digest(canonical.to_string_lossy().as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("p-{}", &hex[..32])
}

/// `key` as one safe file name: itself when it is made of letters, digits,
/// `.`, `_` and `-` (and does not start with `.`), else `x` and its bytes
/// in hex. Two different keys never share a name.
pub fn encode_key(key: &str) -> String {
    let safe = !key.is_empty()
        && key.len() <= NAME_CAP
        && !key.starts_with('.')
        && !key.starts_with('x')
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
    if safe {
        key.to_string()
    } else {
        let hex: String = key.bytes().map(|b| format!("{b:02x}")).collect();
        let mut out = format!("x{hex}");
        if out.len() > NAME_CAP {
            let digest = Sha256::digest(key.as_bytes());
            out = format!(
                "xh{}",
                digest
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            );
        }
        out
    }
}

fn check_part(part: &str) -> Result<(), GateError> {
    let ok = !part.is_empty()
        && part.len() <= NAME_CAP
        && !part.starts_with('.')
        && part
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
    if ok {
        Ok(())
    } else {
        Err(GateError::BadName(part.to_string()))
    }
}

fn check_name(name: &str) -> Result<(), GateError> {
    let parts: Vec<&str> = name.split('/').collect();
    if parts.len() > 2 {
        return Err(GateError::BadName(name.to_string()));
    }
    for p in parts {
        check_part(p).map_err(|_| GateError::BadName(name.to_string()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    #[test]
    fn records_round_trip_per_project_and_instance_and_are_replaced_whole() {
        let tmp = tempfile::tempdir().unwrap();
        let store = GateStore::new(tmp.path().join("gate"));
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        let pa = store.project(&a);
        let pb = store.project(&b);
        assert_ne!(pa.dir(), pb.dir());
        // Another spelling of the same folder finds the same records.
        assert_eq!(store.project(&a.join(".").join("..").join("a")), pa);

        assert_eq!(pa.read::<Value>(Scope::Project, "r").unwrap(), None);
        pa.write(Scope::Project, "r", &json!({"v": 1})).unwrap();
        pa.write(Scope::Instance("s-0"), "r", &json!({"v": 2}))
            .unwrap();
        pa.write(Scope::Project, "r", &json!({"v": 3})).unwrap();
        assert_eq!(
            pa.read::<Value>(Scope::Project, "r").unwrap(),
            Some(json!({"v": 3}))
        );
        assert_eq!(
            pa.read::<Value>(Scope::Instance("s-0"), "r").unwrap(),
            Some(json!({"v": 2}))
        );
        assert_eq!(pb.read::<Value>(Scope::Project, "r").unwrap(), None);
        // No temporary file is left behind.
        let left: Vec<_> = fs::read_dir(pa.dir().join(PROJECT_DIR))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, ["r.json"]);
        assert!(fs::read_to_string(pa.dir().join(PROJECT_NOTE))
            .unwrap()
            .contains('a'));
        assert!(pa.remove(Scope::Project, "r").unwrap());
        assert!(!pa.remove(Scope::Project, "r").unwrap());
    }

    #[test]
    fn a_folder_keeps_its_key_from_before_it_exists_to_after() {
        let tmp = tempfile::tempdir().unwrap();
        let store = GateStore::new(tmp.path().join("gate"));
        let real = tmp.path().join("real");
        fs::create_dir_all(&real).unwrap();
        // Reached through a link, the way a system temp folder can be.
        #[cfg(unix)]
        let parent = {
            let link = tmp.path().join("link");
            std::os::unix::fs::symlink(&real, &link).unwrap();
            link
        };
        #[cfg(not(unix))]
        let parent = real.clone();
        let work = parent.join(".work");
        let before = store.project(&work);
        assert_eq!(store.project(&work.join("x").join("..")), before);
        fs::create_dir_all(&work).unwrap();
        let after = store.project(&work);
        assert_eq!(before, after);
        assert_eq!(after, store.project(&real.join(".work")));
    }

    #[test]
    fn collections_list_their_records() {
        let tmp = tempfile::tempdir().unwrap();
        let p = GateStore::new(tmp.path()).project(tmp.path());
        assert!(p.list(Scope::Project, "jobs").unwrap().is_empty());
        p.write(Scope::Project, "jobs/j2", &1).unwrap();
        p.write(Scope::Project, "jobs/j1", &2).unwrap();
        assert_eq!(p.list(Scope::Project, "jobs").unwrap(), ["j1", "j2"]);
    }

    #[test]
    fn names_that_could_leave_their_folder_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let p = GateStore::new(tmp.path()).project(tmp.path());
        for bad in [
            "", "..", "../x", "a/../b", "a/b/c", ".hidden", "a\\b", "a:b",
        ] {
            assert!(
                matches!(p.write(Scope::Project, bad, &1), Err(GateError::BadName(_))),
                "{bad}"
            );
        }
        // Keys that are not safe file names are encoded, never refused.
        let enc = encode_key("review:a/../b");
        assert!(check_part(&enc).is_ok(), "{enc}");
        assert_ne!(encode_key("x41"), encode_key("A"));
        assert_eq!(encode_key("ABC-1"), "ABC-1");
        p.write(Scope::Instance("../../x"), "r", &1).unwrap();
        assert!(p.dir().join(INSTANCES_DIR).read_dir().unwrap().count() == 1);
    }

    #[test]
    fn the_folders_and_records_are_owner_only() {
        let tmp = tempfile::tempdir().unwrap();
        let store = GateStore::new(tmp.path().join("gate"));
        let p = store.project(tmp.path());
        p.write(Scope::Instance("s-0"), "r", &1).unwrap();
        let file = p.path(Scope::Instance("s-0"), "r").unwrap();
        for path in [
            store.root().to_path_buf(),
            p.dir().to_path_buf(),
            file.parent().unwrap().to_path_buf(),
            file.clone(),
        ] {
            assert!(
                crate::owner_only::is_owner_only(&path).unwrap(),
                "{}",
                path.display()
            );
        }
        assert!(crate::owner_only::is_owner_only(&store.scratch_dir().unwrap()).unwrap());
    }

    /// A record someone else could have written is not trusted.
    #[test]
    fn a_record_that_is_not_owner_only_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let p = GateStore::new(tmp.path().join("gate")).project(tmp.path());
        p.write(Scope::Project, "r", &1).unwrap();
        let file = p.path(Scope::Project, "r").unwrap();
        fs::remove_file(&file).unwrap();
        fs::write(&file, "1").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        }
        assert!(matches!(
            p.read::<i32>(Scope::Project, "r"),
            Err(GateError::NotPrivate(_))
        ));
    }
}
