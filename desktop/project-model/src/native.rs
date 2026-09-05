//! The native process store: the
//! store owns the process definitions and `chappa.yml` is the project file —
//! always, with no toggle.
//!
//! ## The model
//!
//! - The store owns process definitions: each projects.json row carries
//!   `processes: {<name>: ProcessDef}`. Unknown keys are carried through
//!   verbatim, so a key this version does not model survives a
//!   read→mutate→backwrite round trip with the same guarantee as the
//!   in-place rewrite.
//! - `chappa.yml` in the project root is the project file, always. It is
//!   read, watched and backwritten with the full pipeline (round-trip
//!   passthrough, one-time `.chappa-bak`, atomic tmp+rename, concurrent-change
//!   guard). The store is canonical BETWEEN reads; every mutation backwrites
//!   the file, and an external edit is an import ([`import`]) that replaces
//!   the store wholesale — last import wins, no merge.
//! - A project with no commands has no file; the first added command creates
//!   it ([`persist`] materializes `chappa.yml` when absent).
//!
//! ## Migration (first launch after 57)
//!
//! Every stored project materializes `chappa.yml` from its store definitions,
//! so nothing is lost and nothing needs a user action. The definitions are
//! already in the store (put them there), so migration reads no other
//! file; a project with no definitions writes nothing. Migration is
//! observable as a no-op in behavior: same commands, same statuses.

use std::path::{Path, PathBuf};

use indexmap::IndexMap;

use crate::projects::ProjectStore;
use crate::yml::{CHAPPA_YML_NAME, ProcessDef, ProjectYml};

fn no_project(id: u32) -> String {
    format!("no such project: {id}")
}

/// A process-draft is only committed/backwritten when every definition is
/// valid against the project root (command non-blank, env keys non-blank,
/// working-dir containment revalidated) — the same validation the write-back runs
/// before it lets the editor store a definition.
fn validate_defs(root: &Path, defs: &IndexMap<String, ProcessDef>) -> Result<(), String> {
    for (name, def) in defs {
        def.validate(root).map_err(|e| format!("{name}: {e}"))?;
    }
    Ok(())
}

/// Write the store's defs to `chappa.yml`:
/// - file already present → the guarded write-back ([`writeback::apply`]:
///   fresh load keeps top-level unknown keys, one-time backup, concurrent
///   change guard — a refused write refuses the WHOLE operation, so the store
///   is untouched);
/// - file absent → the first definition materializes it directly (there is no
///   prior content to preserve or race with).
///
/// Returns the defs as written.
fn persist(
    store: &mut ProjectStore,
    id: u32,
    defs: IndexMap<String, ProcessDef>,
) -> Result<IndexMap<String, ProcessDef>, String> {
    let root = store
        .get(id)
        .map(|p| p.path.clone())
        .ok_or_else(|| no_project(id))?;
    if root.join(CHAPPA_YML_NAME).exists() {
        writeback_apply(store, id, defs)
    } else {
        let name = store.get(id).map(|p| p.name.clone());
        let yml = ProjectYml {
            name,
            processes: defs.clone(),
            ..ProjectYml::default()
        };
        yml.write(&root)
            .map_err(|e| format!("cannot write {}: {e}", root.join(CHAPPA_YML_NAME).display()))?;
        Ok(defs)
    }
}

/// Migration (first launch after 57): materialize `chappa.yml` for every
/// stored project from its store definitions, so the new project file exists
/// and nothing is lost. Rules:
///
/// - A row never touched by the native store (`processes: None`) adopts the
///   project file's defs (a directory that carries `chappa.yml` is
///   authoritative); a missing file is a bare project — empty defs, no error.
///   A row with store defs never re-reads the file: its defs are already in
///   the store (migration ran first), and imports are user-requested.
/// - A project with definitions materializes `chappa.yml` if (and only if) it
///   does not exist yet — so the migration is idempotent across launches and
///   never clobbers an external edit.
/// - A project with no definitions writes no file.
/// - Any existing `chappa.yml` is left byte-identical, un-watched.
///
/// Returns `(id, reason)` for projects whose lasting `chappa.yml` is present
/// but unparsable — the caller surfaces it as the load error.
pub fn migrate(store: &mut ProjectStore) -> Result<Vec<(u32, String)>, String> {
    let rows: Vec<(u32, PathBuf)> = store
        .list()
        .iter()
        .map(|p| (p.id, p.path.clone()))
        .collect();
    let mut load_errors = Vec::new();
    for (id, root) in rows {
        if store.get(id).is_some_and(|p| p.processes.is_none()) {
            match ProjectYml::load_result(&root) {
                Ok(yml) => store.set_processes(id, yml.processes)?,
                Err(e) => {
                    store.set_processes(id, IndexMap::new())?;
                    if root.join(CHAPPA_YML_NAME).exists() {
                        load_errors.push((id, e));
                    }
                }
            }
        }
        if !store.process_defs(id).is_empty() && !root.join(CHAPPA_YML_NAME).exists() {
            let defs = store.process_defs(id);
            persist(store, id, defs)?;
        }
    }
    Ok(load_errors)
}

/// Import an external project-file edit into the store (the yml watcher's data
/// side; the trust gate that guards imported auto-start/command changes lives
/// in the caller with the hash this returns). Last import wins — no merge —
/// so the store is canonical BETWEEN imports.
pub fn import(store: &mut ProjectStore, id: u32) -> Result<(ProjectYml, String), String> {
    let root = store
        .get(id)
        .map(|p| p.path.clone())
        .ok_or_else(|| no_project(id))?;
    let (yml, hash) = ProjectYml::load_result_with_hash(&root)?;
    store.set_processes(id, yml.processes.clone())?;
    Ok((yml, hash))
}


/// Run one mutation against project `id`'s process definitions. The mutation
/// drafts from the store's defs, then backwrites the WHOLE store map to
/// `chappa.yml` ([`persist`]): fresh load preserves top-level unknown keys, a
/// concurrent disk change refuses the whole mutation with the store untouched,
/// and a definition-less project gains its file on the first added command.
///
/// Returns the defs as committed.
pub fn mutate(
    store: &mut ProjectStore,
    id: u32,
    edit: impl FnOnce(&mut IndexMap<String, ProcessDef>) -> Result<(), String>,
) -> Result<IndexMap<String, ProcessDef>, String> {
    let root = store
        .get(id)
        .map(|p| p.path.clone())
        .ok_or_else(|| no_project(id))?;
    let mut draft = store.process_defs(id);
    edit(&mut draft)?;
    validate_defs(&root, &draft)?;
    let written = persist(store, id, draft)?;
    store.set_processes(id, written)?;
    Ok(store.process_defs(id))
}

// ---- def-map editing --------------------------------------------------------
//
// The editor commands (src-tauri save/delete/duplicate) run their closure
// against the STORE's def map. These mirror the
// `ProjectYml::{add,edit,remove,duplicate}_process` operations on the RAW map —
// same name rules, same position guarantees, same ` copy` suffix walk. Per-def
// validation is NOT done here: `mutate` runs [`validate_defs`] over the whole
// committed map, so an invalid def refuses the whole mutation with the store
// untouched.

/// The name rule shared by every def-map edit: trimmed and non-blank (a blank
/// yml key would be an unclickable row). Mirrors yml's private `valid_name`.
pub fn clean_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("command name must not be blank".to_owned());
    }
    Ok(name.to_owned())
}

/// `+ Add command`: append `def` under `name` (blank name refused, duplicate
/// refused). Mirrors [`ProjectYml::add_process`].
pub fn add_process(
    defs: &mut IndexMap<String, ProcessDef>,
    name: &str,
    def: ProcessDef,
) -> Result<(), String> {
    let name = clean_name(name)?;
    if defs.contains_key(&name) {
        return Err(format!("a command named `{name}` already exists"));
    }
    defs.insert(name, def);
    Ok(())
}

/// `Edit command…`: replace `old_name` with `def`, optionally renaming to
/// `new_name`. A rename is a KEY MOVE (the entry keeps its position); unknown
/// keys of the old entry are carried over unless `def` already carries its
/// own. Mirrors [`ProjectYml::edit_process`].
pub fn edit_process(
    defs: &mut IndexMap<String, ProcessDef>,
    old_name: &str,
    new_name: &str,
    mut def: ProcessDef,
) -> Result<(), String> {
    let new_name = clean_name(new_name)?;
    let idx = defs
        .get_index_of(old_name)
        .ok_or_else(|| format!("no command named `{old_name}`"))?;
    if new_name != old_name && defs.contains_key(&new_name) {
        return Err(format!("a command named `{new_name}` already exists"));
    }
    if def.unknown.is_empty() {
        def.unknown = defs[idx].unknown.clone();
    }
    if new_name == old_name {
        defs[idx] = def;
    } else {
        // Position-preserving key move: drop the old slot (shifting the tail
        // down one), append under the new name, move it back up.
        defs.shift_remove_index(idx);
        defs.insert(new_name, def);
        let last = defs.len() - 1;
        defs.move_index(last, idx);
    }
    Ok(())
}

/// `Delete command "<name>"`. The tail shifts up (order preserved).
/// Mirrors [`ProjectYml::remove_process`].
pub fn remove_process(defs: &mut IndexMap<String, ProcessDef>, name: &str) -> Result<(), String> {
    defs.shift_remove(name)
        .map(|_| ())
        .ok_or_else(|| format!("no command named `{name}`"))
}

/// `Duplicate to ▸`: append a copy of `def` under `name`, or — when that name
/// is taken in THIS map — under `<name> copy`, `<name> copy 2`, … Mirrors
/// [`ProjectYml::duplicate_process`]. `def` keeps its unknown keys; validation
/// happens in `mutate`.
pub fn duplicate_process(
    defs: &mut IndexMap<String, ProcessDef>,
    name: &str,
    def: ProcessDef,
) -> Result<String, String> {
    let base = clean_name(name)?;
    let mut candidate = base.clone();
    let mut n = 1;
    while defs.contains_key(&candidate) {
        n += 1;
        candidate = if n == 2 {
            format!("{base} copy")
        } else {
            format!("{base} copy {}", n - 1)
        };
    }
    defs.insert(candidate.clone(), def);
    Ok(candidate)
}

/// The write-back, closed over the store map we want on disk. `apply`
/// derives the project root from the store itself, so this is the single
/// path `persist` uses when the project file already exists.
fn writeback_apply(
    store: &mut ProjectStore,
    id: u32,
    defs: IndexMap<String, ProcessDef>,
) -> Result<IndexMap<String, ProcessDef>, String> {
    let written = crate::writeback::apply(store, id, |_, yml| {
        yml.processes = defs.clone();
        Ok(())
    })?;
    Ok(written.processes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// The fixture, verbatim (see `writeback.rs::tests`): unknown keys
    /// at both levels, exotic-but-valid values, env deliberately not
    /// alphabetical, three processes.
    const FIXTURE: &str = r#"name: fixture
icon_initials: FX
theme: dark
plugins:
  - name: lint
    on: [save, open]
  - name: fmt
notes: |
  keep me
  verbatim
processes:
  alpha:
    command: py a.py
    auto_start: false
    env:
      ZED: "1"
      ALPHA: two
      MID: "3.5"
    priority: 7
    hooks:
      before: echo hi
      after: [a, b]
  beta:
    command: py b.py
    working_dir: sub
    restart_when_changed: ["**/*.py"]
    tags: [x, y]
  gamma:
    command: py c.py
"#;

    fn project_with_chappa(text: &str) -> (tempfile::TempDir, ProjectStore, u32) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join(CHAPPA_YML_NAME), text).unwrap();
        let mut store = ProjectStore::new(dir.path().join("projects.json"));
        let id = store.add(root, "fixture".into(), None).unwrap().id;
        store
            .set_processes(id, ProjectYml::from_str(text).unwrap().processes)
            .unwrap();
        (dir, store, id)
    }

    fn unknown_subtrees(yml: &ProjectYml) -> String {
        let mut out = serde_yaml::to_string(&yml.unknown).unwrap();
        for (name, p) in &yml.processes {
            out.push_str(&format!("--- {name}\n"));
            out.push_str(&serde_yaml::to_string(&p.unknown).unwrap());
        }
        out
    }

    /// Store round trip: defs + order + unknown keys survive
    /// import→mutate→backwrite through the project file with the same
    /// byte-level guarantees as the in-place tests.
    #[test]
    fn store_round_trip_preserves_defs_order_and_unknown_keys() {
        let (dir, mut store, id) = project_with_chappa(FIXTURE);
        let root = store.get(id).unwrap().path.clone();
        let unknown_before = unknown_subtrees(&ProjectYml::from_str(FIXTURE).unwrap());

        // Mutate ONE process through the store; the others' unknown keys and
        // the process order must survive the backwrite byte-for-byte.
        let committed = mutate(&mut store, id, |defs| {
            let mut def = defs["beta"].clone();
            def.command = "py b2.py".into();
            defs.insert("beta".into(), def);
            Ok(())
        })
        .unwrap();
        assert_eq!(committed["beta"].command, "py b2.py");
        assert_eq!(
            committed.keys().collect::<Vec<_>>(),
            vec!["alpha", "beta", "gamma"]
        );
        assert!(committed["alpha"].unknown.contains_key("priority"));

        // Reload from DISK: unknown subtrees byte-identical, order intact.
        let after = ProjectYml::load_result(&root).unwrap();
        assert_eq!(unknown_subtrees(&after), unknown_before);
        assert_eq!(
            after.processes.keys().collect::<Vec<_>>(),
            vec!["alpha", "beta", "gamma"]
        );

        // The store is canonical BETWEEN imports: reopening sees the same defs.
        let reopened = ProjectStore::new(dir.path().join("projects.json"));
        assert_eq!(reopened.process_defs(id)["beta"].command, "py b2.py");
        assert!(reopened.process_defs(id)["alpha"]
            .unknown
            .contains_key("priority"));
    }

    /// "A project with no commands has no file; the first added command
    /// creates it." A bare directory gains chappa.yml on its first mutation.
    #[test]
    fn first_command_creates_the_file_and_watches_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("bare");
        fs::create_dir_all(&root).unwrap();
        let mut store = ProjectStore::new(dir.path().join("projects.json"));
        let id = store.add(root.clone(), "bare".into(), None).unwrap().id;
        assert!(!root.join(CHAPPA_YML_NAME).exists());

        // No defs yet → no file.
        assert!(store.process_defs(id).is_empty());
        assert!(!root.join(CHAPPA_YML_NAME).exists());

        // First command materializes the file; subsequent edits go through the
        // guarded write-back.
        mutate(&mut store, id, |defs| {
            add_process(
                defs,
                "app",
                ProcessDef {
                    command: "py app.py".into(),
                    ..ProcessDef::default()
                },
            )
        })
        .unwrap();
        let on_disk = ProjectYml::load_result(&root).expect("first command created chappa.yml");
        assert_eq!(on_disk.processes["app"].command, "py app.py");
        assert_eq!(store.process_defs(id), on_disk.processes);
    }

    /// Refused backwrite leaves store AND disk untouched: when the project
    /// file cannot be loaded (the "never write while a load error is
    /// outstanding"), the whole mutation refuses and the store defs do not
    /// move.
    #[test]
    fn refused_backwrite_leaves_store_and_disk_untouched() {
        let (_dir, mut store, id) = project_with_chappa(FIXTURE);
        let root = store.get(id).unwrap().path.clone();
        // Break chappa.yml AFTER data was committed (mutations were allowed
        // before, then the file corrupts on disk).
        let broken = "processes:\n  x:\n    auto_start: true\n"; // missing command
        fs::write(root.join(CHAPPA_YML_NAME), broken).unwrap();
        let before_defs = store.process_defs(id);

        let err = mutate(&mut store, id, |defs| {
            defs.insert(
                "new".into(),
                ProcessDef {
                    command: "py new.py".into(),
                    ..ProcessDef::default()
                },
            );
            Ok(())
        })
        .unwrap_err();
        assert!(
            err.contains("refusing to write"),
            "the load error must refuse the mutation: {err}"
        );
        // Store untouched, disk untouched.
        assert_eq!(store.process_defs(id), before_defs);
        assert_eq!(fs::read_to_string(root.join(CHAPPA_YML_NAME)).unwrap(), broken);

        // A validation failure (blank command) also refuses with nothing
        // persisted.
        let before = store.process_defs(id);
        assert!(mutate(&mut store, id, |defs| {
            defs.insert(
                "bad".into(),
                ProcessDef {
                    command: "  ".into(),
                    ..ProcessDef::default()
                },
            );
            Ok(())
        })
        .is_err());
        assert_eq!(store.process_defs(id), before);
    }

    /// An external project-file edit imports through the replace path and the
    /// store is canonical BETWEEN imports (last import wins, no merge).
    #[test]
    fn external_edit_imports_and_becomes_canonical() {
        let (_dir, mut store, id) = project_with_chappa(FIXTURE);
        let root = store.get(id).unwrap().path.clone();
        let external = "processes:\n  added:\n    command: py a.py\n";
        fs::write(root.join(CHAPPA_YML_NAME), external).unwrap();

        // The watcher fires: import replaces the store wholesale.
        let (yml, _hash) = import(&mut store, id).unwrap();
        assert_eq!(yml.processes.keys().collect::<Vec<_>>(), vec!["added"]);
        assert_eq!(store.process_defs(id).keys().collect::<Vec<_>>(), vec!["added"]);

        // A store edit no longer competes with an earlier import.
        let committed = mutate(&mut store, id, |defs| {
            defs.insert(
                "mine".into(),
                ProcessDef {
                    command: "py mine.py".into(),
                    ..ProcessDef::default()
                },
            );
            Ok(())
        })
        .unwrap();
        assert_eq!(committed.len(), 2);
        let on_disk = ProjectYml::load_result(&root).unwrap();
        assert_eq!(
            on_disk.processes.keys().collect::<Vec<_>>(),
            vec!["added", "mine"]
        );
    }

    // ---- migration ----------------------------------------------------------

    /// Migration materializes chappa.yml per project with definitions, writes
    /// none for an empty project, and writes to NO other file in the project
    /// directory. The bystander here is a yml of another name carrying the
    /// same shape — the closest thing to a file migration could plausibly
    /// mistake for its own.
    #[test]
    fn migration_materializes_per_project_and_writes_no_other_file() {
        let dir = tempfile::tempdir().unwrap();
        let with_defs = dir.path().join("with-defs");
        fs::create_dir_all(&with_defs).unwrap();
        let bystander_before = FIXTURE.as_bytes();
        fs::write(with_defs.join("processes.yml"), FIXTURE).unwrap();

        let empty = dir.path().join("empty");
        fs::create_dir_all(&empty).unwrap();

        let mut store = ProjectStore::new(dir.path().join("projects.json"));
        let a = store.add(with_defs.clone(), "with-defs".into(), None).unwrap().id;
        store
            .set_processes(a, ProjectYml::from_str(FIXTURE).unwrap().processes)
            .unwrap();
        let b = store.add(empty.clone(), "empty".into(), None).unwrap().id;
        store.set_processes(b, IndexMap::new()).unwrap();

        let errors = migrate(&mut store).unwrap();
        assert!(errors.is_empty(), "{errors:?}");

        // A project with defs gained chappa.yml, carrying the same defs.
        assert!(with_defs.join(CHAPPA_YML_NAME).exists());
        let materialized = ProjectYml::load_result(&with_defs).unwrap();
        assert_eq!(materialized.processes, ProjectYml::from_str(FIXTURE).unwrap().processes);
        // An empty project wrote no file.
        assert!(!empty.join(CHAPPA_YML_NAME).exists());
        // The bystander is byte-identical — migration writes its own file only.
        assert_eq!(fs::read(with_defs.join("processes.yml")).unwrap(), bystander_before);
    }

    /// Migration is idempotent: a second migrate never rewrites an existing
    /// chappa.yml (an external edit would be clobbered otherwise) and never
    /// touches migrated rows.
    #[test]
    fn a_second_migrate_never_touches_migrated_rows_or_files() {
        let (_dir, mut store, id) = {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("proj");
            fs::create_dir_all(&root).unwrap();
            fs::write(
                dir.path().join("projects.json"),
                format!(
                    r#"[{{"id":1,"name":"fixture","path":{}}}]"#,
                    serde_json::to_string(&root).unwrap()
                ),
            )
            .unwrap();
            let mut store = ProjectStore::new(dir.path().join("projects.json"));
            store
                .set_processes(
                    1,
                    IndexMap::from([(
                        "app".into(),
                        ProcessDef {
                            command: "py app.py".into(),
                            ..ProcessDef::default()
                        },
                    )]),
                )
                .unwrap();
            (dir, store, 1)
        };
        let errors = migrate(&mut store).unwrap();
        assert!(errors.is_empty());
        let root = store.get(id).unwrap().path.clone();
        let first = fs::read_to_string(root.join(CHAPPA_YML_NAME)).unwrap();

        // Second launch: the file is NOT rewritten (an external edit would
        // otherwise be lost).
        migrate(&mut store).unwrap();
        assert_eq!(fs::read_to_string(root.join(CHAPPA_YML_NAME)).unwrap(), first);
        assert_eq!(store.process_defs(id).keys().collect::<Vec<_>>(), vec!["app"]);
    }

    /// An un-touched row (`processes: None`) adopts an existing chappa.yml's
    /// defs during migration — a project directory that carries its file is
    /// authoritative. A missing file is a bare project, not an error.
    #[test]
    fn untouched_row_adopts_the_project_file_defs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(CHAPPA_YML_NAME), FIXTURE).unwrap();
        fs::write(
            dir.path().join("projects.json"),
            format!(
                r#"[{{"id":1,"name":"fixture","path":{}}}]"#,
                serde_json::to_string(&root).unwrap()
            ),
        )
        .unwrap();
        let mut store = ProjectStore::new(dir.path().join("projects.json"));
        assert_eq!(store.process_defs(1).len(), 0);

        let errors = migrate(&mut store).unwrap();
        assert!(errors.is_empty());
        assert_eq!(
            store.process_defs(1).keys().collect::<Vec<_>>(),
            vec!["alpha", "beta", "gamma"]
        );
        // The file already existed → not rewritten.
        assert_eq!(
            fs::read(root.join(CHAPPA_YML_NAME)).unwrap(),
            FIXTURE.as_bytes()
        );
    }

    /// An un-touched row whose lasting chappa.yml is present but unparsable
    /// migrates with empty defs and reports the load error — the same "never
    /// write while a load error is outstanding" contract, surfaced on open.
    #[test]
    fn untouched_row_with_a_broken_chappa_yml_reports_and_keeps_empty_defs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join(CHAPPA_YML_NAME),
            "processes:\n  x:\n    auto_start: true\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("projects.json"),
            format!(
                r#"[{{"id":1,"name":"broken","path":{}}}]"#,
                serde_json::to_string(&root).unwrap()
            ),
        )
        .unwrap();
        let mut store = ProjectStore::new(dir.path().join("projects.json"));
        let errors = migrate(&mut store).unwrap();
        assert_eq!(errors.len(), 1);
        assert!(store.process_defs(1).is_empty());
    }

    /// The trust gate guards the lasting project file: a hash the
    /// user has not approved arms the gate; approval clears it; an external
    /// edit re-arms it.
    #[test]
    fn trust_gate_arms_on_unapproved_hash_and_clears_on_approval() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join(CHAPPA_YML_NAME),
            "processes:\n  a:\n    command: echo hi\n",
        )
        .unwrap();
        let mut store = ProjectStore::new(dir.path().join("projects.json"));
        let id = store.add(root.clone(), "proj".into(), None).unwrap().id;
        let hash = crate::hash::file_content_hash(&root.join(CHAPPA_YML_NAME)).unwrap();
        assert!(!store.is_trusted(id, &hash), "unapproved hash arms the gate");
        store.mark_trusted(id, hash.clone()).unwrap();
        assert!(store.is_trusted(id, &hash), "approval clears the gate");
        // An edit outside the app changes the hash → the gate re-arms.
        fs::write(
            root.join(CHAPPA_YML_NAME),
            "processes:\n  a:\n    command: echo changed\n",
        )
        .unwrap();
        let edited = crate::hash::file_content_hash(&root.join(CHAPPA_YML_NAME)).unwrap();
        assert_ne!(edited, hash);
        assert!(!store.is_trusted(id, &edited), "an edit re-arms the gate");
    }

    // ---- def-map editing ops ------------------------------------------------

    fn def(command: &str) -> ProcessDef {
        ProcessDef {
            command: command.to_owned(),
            unknown: IndexMap::new(),
            ..ProcessDef::default()
        }
    }

    /// The raw-map ops mirror the ProjectYml semantics exactly.
    #[test]
    fn raw_map_ops_mirror_projectyml_editing_semantics() {
        let mut defs = IndexMap::from([("alpha".to_owned(), def("py a.py"))]);
        add_process(&mut defs, "beta", def("py b.py")).unwrap();
        assert_eq!(defs.keys().collect::<Vec<_>>(), vec!["alpha", "beta"]);
        assert!(add_process(&mut defs, "alpha", def("x")).is_err());
        assert!(add_process(&mut defs, "  ", def("x")).is_err());

        // A position-preserving rename carries unknown keys from the old entry.
        let mut rich = IndexMap::new();
        rich.insert(
            "one".to_owned(),
            ProcessDef {
                command: "py 1.py".into(),
                unknown: IndexMap::from([("priority".to_owned(), serde_yaml::Value::from(7))]),
                ..ProcessDef::default()
            },
        );
        rich.insert("two".to_owned(), def("py 2.py"));
        edit_process(&mut rich, "one", "one renamed", def("py 1b.py")).unwrap();
        assert_eq!(rich.keys().collect::<Vec<_>>(), vec!["one renamed", "two"]);
        assert_eq!(
            rich["one renamed"].unknown["priority"],
            serde_yaml::Value::from(7),
            "unknown keys carry across the rename"
        );
        assert!(edit_process(&mut rich, "one renamed", "two", def("x")).is_err());

        assert_eq!(
            duplicate_process(&mut rich, "two", def("py 2.py")).unwrap(),
            "two copy"
        );
        assert_eq!(
            duplicate_process(&mut rich, "two", def("py 2.py")).unwrap(),
            "two copy 2"
        );
        assert!(remove_process(&mut rich, "two").is_ok());
        assert!(remove_process(&mut rich, "two").is_err());
        assert!(!rich.contains_key("two"));
    }

    /// The ops commit through `mutate` with validation refusal and the file
    /// backs the store.
    #[test]
    fn raw_map_ops_commit_through_mutate_creating_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("bare");
        fs::create_dir_all(&root).unwrap();
        let mut store = ProjectStore::new(dir.path().join("projects.json"));
        let id = store.add(root.clone(), "bare".into(), None).unwrap().id;

        mutate(&mut store, id, |defs| add_process(defs, "app", def("py app.py"))).unwrap();
        assert_eq!(store.process_defs(id)["app"].command, "py app.py");
        assert!(root.join(CHAPPA_YML_NAME).exists(), "first command created it");

        mutate(&mut store, id, |defs| {
            edit_process(defs, "app", "server", def("py server.py"))
        })
        .unwrap();
        assert_eq!(store.process_defs(id).keys().collect::<Vec<_>>(), vec!["server"]);

        mutate(&mut store, id, |defs| {
            duplicate_process(defs, "server", def("py server.py")).map(|_| ())
        })
        .unwrap();
        assert!(store.process_defs(id).contains_key("server copy"));

        let before = store.process_defs(id);
        let file_before = fs::read(root.join(CHAPPA_YML_NAME)).unwrap();
        assert!(mutate(&mut store, id, |defs| add_process(defs, "bad", def("  "))).is_err());
        assert_eq!(store.process_defs(id), before);
        // A refused mutation leaves both the store AND the file untouched.
        assert_eq!(fs::read(root.join(CHAPPA_YML_NAME)).unwrap(), file_before);
    }
}

