//! Secrets the runner generates per instance: `{secret:<name>}`.
//!
//! A local database still needs a password, and a password written into
//! `chappa.yml` or a compose file is committed, shared and copied into every
//! clone. So a definition names the secret instead (`POSTGRES_PASSWORD:
//! "{secret:db}"`) and the runner makes one random value per instance and
//! name the first time the instance is resolved, keeps it under the
//! git-ignored work folder and fills it in wherever the placeholder is
//! used: the override file's environment, the instance env, hook argv.
//!
//! Each value is its own file, `<work>/instances/<instance>/secrets/<name>`,
//! holding the value and nothing else, so a person can read it (or replace
//! it with a value of their own) without a tool. The folder and each file
//! are owner-only ([`crate::owner_only`]): `0700` / `0600` on Unix, the
//! current user as the only access entry on Windows. A file that cannot be
//! restricted is removed and the resolution fails: a value is never left
//! where other users can read it. A file is written under a temporary name,
//! restricted, and linked into place, so two resolutions racing for the
//! same name agree on one value. Deleting a file makes a new value next
//! time; anything that kept the old one (a database volume initialised with
//! it) has to be reset too.
//!
//! The values stay in the instance env (components need them), and stay out
//! of everything shown: every view, job line and log the stack runner hands
//! out passes through [`Secrets::hide`], which shows each value as `••••`.
//!
//! The secret scan of the instance env puts each generated value back as
//! its placeholder before it scans ([`Secrets::mask`]): the scan looks for
//! credentials written into files, and a value the runner made and keeps
//! out of the repository is not one. Anything else in the env, such as a
//! password written literally into the definition, is scanned as it is. A
//! generated value that turns up in a commit or a tracked file of a bound
//! worktree is its own finding (`generated-secret`, see
//! [`crate::safety::secrets::generated`]): it left the work folder.

use indexmap::IndexMap;
use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::definition::StackDef;
use crate::intent::INSTANCES_DIR;
use crate::placeholders::{self, Placeholder};

/// The folder under an instance's own folder that holds its secrets.
pub const SECRETS_DIR: &str = "secrets";
/// Random bytes per generated value (written as lowercase hex).
const VALUE_BYTES: usize = 24;
const MAX_NAME_LEN: usize = 64;

/// An instance's generated secrets by name. `Debug` names them and never
/// shows a value, so a logged instance or context leaks nothing.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Secrets(IndexMap<String, String>);

impl fmt::Debug for Secrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.0.keys()).finish()
    }
}

impl Secrets {
    pub fn from_pairs<K: Into<String>, V: Into<String>>(
        pairs: impl IntoIterator<Item = (K, V)>,
    ) -> Secrets {
        Secrets(
            pairs
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        )
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// `text` with every generated value written back as its placeholder.
    /// Longer values go first, so one value inside another is never left
    /// half replaced.
    pub fn mask(&self, text: &str) -> String {
        let mut out = text.to_owned();
        for (name, value) in self.by_length() {
            if out.contains(value.as_str()) {
                out = out.replace(value.as_str(), &format!("{{secret:{name}}}"));
            }
        }
        out
    }

    /// `text` with every value long enough to match ([`MIN_MATCH_LEN`])
    /// shown as [`crate::credentials::MASK`], the way every other
    /// credential is shown: views, job output and logs carry the instance
    /// env's values, and a generated password in them is as secret as one
    /// written by hand. There is no way to reveal it from a view; the file
    /// under `.work/` is where a person reads it.
    pub fn hide(&self, text: &str) -> String {
        let mut out: Option<String> = None;
        for (_, value) in self.by_length() {
            if value.len() < MIN_MATCH_LEN {
                continue;
            }
            let cur = out.as_deref().unwrap_or(text);
            if cur.contains(value.as_str()) {
                out = Some(cur.replace(value.as_str(), crate::credentials::MASK));
            }
        }
        out.unwrap_or_else(|| text.to_owned())
    }

    /// Whether `text` holds any value long enough to match.
    pub fn found_in(&self, text: &str) -> bool {
        self.matchable().any(|v| text.contains(v))
    }

    /// The values long enough to match exactly, longest first.
    pub fn matchable(&self) -> impl Iterator<Item = &str> {
        self.by_length()
            .into_iter()
            .map(|(_, v)| v.as_str())
            .filter(|v| v.len() >= MIN_MATCH_LEN)
    }

    /// The longest matchable value's length (0 without one).
    pub fn longest(&self) -> usize {
        self.matchable().map(str::len).max().unwrap_or(0)
    }

    fn by_length(&self) -> Vec<(&String, &String)> {
        let mut by_len: Vec<(&String, &String)> =
            self.0.iter().filter(|(_, v)| !v.is_empty()).collect();
        by_len.sort_by_key(|(_, v)| std::cmp::Reverse(v.len()));
        by_len
    }
}

/// The shortest value matched against text ([`Secrets::hide`], the scan
/// for a generated value in a commit). A generated value is 48 characters;
/// a value a person wrote into the file may be shorter, and one this short
/// would match ordinary words and numbers everywhere.
pub const MIN_MATCH_LEN: usize = 8;

/// Letters, digits, `-` and `_`: a name that is also a safe file name.
pub fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Every secret name the definition uses: the stack's `env:` and each
/// component's templates and hook argv.
pub fn names(stack: &StackDef) -> BTreeSet<String> {
    let mut words: Vec<&str> = stack.env.values().map(String::as_str).collect();
    for c in stack.components.values() {
        words.extend(c.templates().into_iter().map(|(_, t)| t));
        for (_, argv) in c.hooks() {
            words.extend(argv.iter().map(String::as_str));
        }
    }
    words
        .into_iter()
        .filter_map(|w| placeholders::references(w).ok())
        .flatten()
        .filter_map(|p| match p {
            Placeholder::Secret(name) => Some(name),
            _ => None,
        })
        .collect()
}

/// The folder holding `instance`'s secrets.
pub fn dir(work_dir: &Path, instance: &str) -> PathBuf {
    work_dir
        .join(INSTANCES_DIR)
        .join(instance)
        .join(SECRETS_DIR)
}

/// The values of `names` for `instance`, generating each one that does not
/// exist yet. Nothing is written when `names` is empty.
pub fn load_or_create(
    work_dir: &Path,
    instance: &str,
    names: &BTreeSet<String>,
) -> io::Result<Secrets> {
    let dir = dir(work_dir, instance);
    let mut out = IndexMap::new();
    for name in names {
        if !is_valid_name(name) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("`{name}` is not a valid secret name"),
            ));
        }
        let path = dir.join(name);
        let value = match read(&path)? {
            Some(v) => v,
            None => create(&dir, &path)?,
        };
        out.insert(name.clone(), value);
    }
    Ok(Secrets(out))
}

/// The value in `path`, `None` when there is no such file. An empty file is
/// an error: it would make an empty password.
fn read(path: &Path) -> io::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => {
            let value = text.trim();
            if value.is_empty() {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "{} is empty; delete it to have a new value generated",
                        path.display()
                    ),
                ))
            } else {
                Ok(Some(value.to_owned()))
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Write a fresh value under a temporary name and link it into place. When
/// another resolution linked its own first, that one is the value.
fn create(dir: &Path, path: &Path) -> io::Result<String> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    make_private_dir(dir)?;
    let value = generate()?;
    let tmp = dir.join(format!(
        ".tmp-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    write_private(&tmp, &value)?;
    let linked = fs::hard_link(&tmp, path);
    let _ = fs::remove_file(&tmp);
    match linked {
        Ok(()) => Ok(value),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => read(path)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} vanished", path.display()),
            )
        }),
        Err(e) => Err(e),
    }
}

fn generate() -> io::Result<String> {
    let mut bytes = [0u8; VALUE_BYTES];
    getrandom::fill(&mut bytes).map_err(|e| io::Error::other(e.to_string()))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// The secrets folder, owner-only ([`crate::owner_only`]): `0700` on Unix,
/// the current user as its only access entry on Windows.
fn make_private_dir(dir: &Path) -> io::Result<()> {
    crate::owner_only::create_dir(dir)
        .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", dir.display())))
}

/// A value's file, owner-only before it is linked into place: a value
/// whose file cannot be restricted is not created at all.
fn write_private(path: &Path, value: &str) -> io::Result<()> {
    crate::owner_only::write_new(path, value.as_bytes())
        .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))
}

/// Every value generated for any instance under `work_dir`, without making
/// any. Unreadable or empty files are left out (a resolution reports them).
/// Named `<instance>/<name>`, so two instances' values stay apart.
pub fn existing(work_dir: &Path) -> Secrets {
    let mut out = IndexMap::new();
    let Ok(instances) = fs::read_dir(work_dir.join(INSTANCES_DIR)) else {
        return Secrets(out);
    };
    let mut instances: Vec<_> = instances.flatten().collect();
    instances.sort_by_key(|e| e.file_name());
    for inst in instances {
        let Ok(files) = fs::read_dir(inst.path().join(SECRETS_DIR)) else {
            continue;
        };
        let mut files: Vec<_> = files.flatten().collect();
        files.sort_by_key(|e| e.file_name());
        for f in files {
            let name = f.file_name().to_string_lossy().into_owned();
            if !is_valid_name(&name) {
                continue;
            }
            if let Ok(Some(v)) = read(&f.path()) {
                out.insert(format!("{}/{name}", inst.file_name().to_string_lossy()), v);
            }
        }
    }
    Secrets(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn a_value_is_generated_once_kept_and_reused() {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path();
        let first = load_or_create(work, "app-0", &set(&["db", "api-key"])).unwrap();
        let db = first.get("db").unwrap().to_owned();
        assert_eq!(db.len(), VALUE_BYTES * 2);
        assert!(db.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(Some(db.as_str()), first.get("api-key"));
        assert_eq!(
            fs::read_to_string(dir(work, "app-0").join("db")).unwrap(),
            db
        );
        let again = load_or_create(work, "app-0", &set(&["db"])).unwrap();
        assert_eq!(again.get("db"), Some(db.as_str()));
        let other_slot = load_or_create(work, "app-1", &set(&["db"])).unwrap();
        assert_ne!(other_slot.get("db"), Some(db.as_str()), "one per instance");
        let leftovers: Vec<_> = fs::read_dir(dir(work, "app-0"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn nothing_is_written_without_names_and_a_person_may_set_a_value() {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path();
        assert!(load_or_create(work, "app-0", &BTreeSet::new())
            .unwrap()
            .is_empty());
        assert!(!work.join(INSTANCES_DIR).exists());
        fs::create_dir_all(dir(work, "app-0")).unwrap();
        fs::write(dir(work, "app-0").join("db"), "chosen-by-hand\n").unwrap();
        let s = load_or_create(work, "app-0", &set(&["db"])).unwrap();
        assert_eq!(s.get("db"), Some("chosen-by-hand"));
        fs::write(dir(work, "app-0").join("db"), "\n").unwrap();
        let err = load_or_create(work, "app-0", &set(&["db"])).unwrap_err();
        assert!(err.to_string().contains("is empty"), "{err}");
        assert!(load_or_create(work, "app-0", &set(&["../x"])).is_err());
    }

    #[test]
    fn racing_resolutions_agree_on_one_value() {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().to_path_buf();
        let values: Vec<String> = (0..8)
            .map(|_| {
                let work = work.clone();
                std::thread::spawn(move || {
                    load_or_create(&work, "app-0", &set(&["db"]))
                        .unwrap()
                        .get("db")
                        .unwrap()
                        .to_owned()
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect();
        assert!(values.windows(2).all(|w| w[0] == w[1]), "{values:?}");
    }

    #[cfg(unix)]
    #[test]
    fn the_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        load_or_create(tmp.path(), "app-0", &set(&["db"])).unwrap();
        let d = dir(tmp.path(), "app-0");
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&d), 0o700);
        assert_eq!(mode(&d.join("db")), 0o600);
    }

    /// The folder and each value file have exactly one access entry, the
    /// current user's, not inherited.
    #[cfg(windows)]
    #[test]
    fn the_folder_and_files_have_the_owner_as_their_only_access_entry() {
        let tmp = tempfile::tempdir().unwrap();
        load_or_create(tmp.path(), "app-0", &set(&["db", "api-key"])).unwrap();
        let d = dir(tmp.path(), "app-0");
        crate::owner_only::assert_owner_only(&d);
        crate::owner_only::assert_owner_only(&d.join("db"));
        crate::owner_only::assert_owner_only(&d.join("api-key"));
    }

    #[test]
    fn hide_shows_each_value_as_the_mask_and_existing_reads_every_instance() {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path();
        let a = load_or_create(work, "app-0", &set(&["db"])).unwrap();
        let b = load_or_create(work, "app-1", &set(&["db"])).unwrap();
        fs::write(dir(work, "app-1").join("short"), "abc").unwrap();
        let all = existing(work);
        let (va, vb) = (a.get("db").unwrap(), b.get("db").unwrap());
        assert_eq!(all.get("app-0/db"), Some(va));
        assert_eq!(all.get("app-1/db"), Some(vb));
        let line = format!("DATABASE_URL=postgres://app:{va}@db/app other={vb} abc");
        let shown = all.hide(&line);
        assert_eq!(
            shown, "DATABASE_URL=postgres://app:••••@db/app other=•••• abc",
            "a value too short to match is left alone"
        );
        assert!(all.found_in(&line) && !all.found_in("abc"));
        assert!(existing(&work.join("nothing-here")).is_empty());
    }

    #[test]
    fn mask_puts_each_value_back_as_its_placeholder_and_debug_shows_names_only() {
        let long = "ab".repeat(12);
        let short = "ab".repeat(6);
        let s = Secrets::from_pairs([("short", short.as_str()), ("long", long.as_str())]);
        let url = format!("postgres://app:{long}@localhost:5432/app?x={short}");
        assert_eq!(
            s.mask(&url),
            "postgres://app:{secret:long}@localhost:5432/app?x={secret:short}"
        );
        assert_eq!(s.mask("nothing here"), "nothing here");
        let shown = format!("{s:?}");
        assert!(
            shown.contains("short") && !shown.contains(&short),
            "{shown}"
        );
    }

    #[test]
    fn names_come_from_env_templates_and_hooks() {
        let doc = r#"
stacks:
  app:
    env:
      SHARED: "{secret:shared}"
    components:
      db:
        driver: compose
        file: core.compose.yml
        service: db
        env:
          POSTGRES_PASSWORD: "{secret:db}"
        outputs:
          url: "postgres://app:{secret:db}@{hostname}:{port:db}/app"
        dataSafety:
          marker: [tool, --password, "{secret:hook}"]
"#;
        let doc = crate::definition::StacksDocument::parse(doc).unwrap();
        let stack = &doc.stacks["app"];
        assert_eq!(names(stack), set(&["db", "hook", "shared"]));
    }
}
