//! `chappa.yml` write-back: the guarded
//! mutation path, the one-time per-file backup, and the reload diff.
//!
//! HIGH-CARE: this rewrites the project file. The rules:
//!
//! 1. Round-trip through the model with the unknown-key passthrough
//!    (`yml.rs`) — keys chappa-ai does not model survive byte-equivalent in
//!    value and relative order.
//! 2. Comments are lost (serde_yaml). Before chappa-ai's FIRST write to a given
//!    project file the file is copied to `chappa.yml.chappa-bak` next to it —
//!    once per file, tracked in projects.json (`yml_backed_up`), never
//!    refreshed: the backup is the pre-chappa original with its comments, and
//!    a later copy would overwrite exactly the thing worth keeping.
//! 3. Atomic write (temp + rename), then reload through the normal path. The
//!    reload DIFFS old/new definitions ([`diff_processes`]) so the yml watcher
//!    only restarts processes whose spawn inputs changed.
//! 4. Never write while a load error is outstanding: `apply` loads the file
//!    FRESH and refuses when that load fails — a stale in-memory model must
//!    never clobber a file the user is mid-edit on in another editor.
//! 5. Concurrent-change guard (added 2026-08-31): the load hashes
//!    the bytes it parsed;
//!    immediately before the tmp+rename the file is re-hashed, and a
//!    mismatch REFUSES the write ([`CHANGED_ON_DISK`]) — the caller reloads
//!    through the path and surfaces the message. Never
//!    last-writer-wins a file another writer touches.

use std::fs;
use std::path::Path;

use indexmap::IndexMap;

use crate::projects::ProjectStore;
use crate::yml::{CHAPPA_YML_NAME, ProcessDef, ProjectYml};

/// The backup file name, next to the project file.
pub const BACKUP_NAME: &str = "chappa.yml.chappa-bak";

/// The concurrent-change refusal, verbatim what the UI surfaces (the modal
/// keeps its values, so re-apply is one click). Callers match on this to
/// fire the reload.
pub const CHANGED_ON_DISK: &str = "chappa.yml changed on disk — re-apply your edit";

/// Run one mutation against project `id`'s project file: fresh load (a parse
/// failure refuses the write with its reason), `edit`, one-time backup,
/// re-hash guard, atomic write. Returns the model as written. An `edit` that
/// changes nothing still writes (the caller decided; this keeps the path
/// uniform), but a failed `edit` writes nothing and makes no backup.
pub fn apply<F>(store: &mut ProjectStore, id: u32, edit: F) -> Result<ProjectYml, String>
where
    F: FnOnce(&Path, &mut ProjectYml) -> Result<(), String>,
{
    let root = store
        .get(id)
        .map(|p| p.path.clone())
        .ok_or_else(|| format!("no such project: {id}"))?;
    let (mut yml, loaded_hash) = ProjectYml::load_result_with_hash(&root)
        .map_err(|e| format!("refusing to write {CHAPPA_YML_NAME} while it fails to load: {e}"))?;
    edit(&root, &mut yml)?;
    ensure_backup(store, id, &root)?;
    // Concurrent-change guard (rule 5): another writer can touch this file,
    // and a rename would silently discard whatever landed between our load
    // and now. The re-hash sits immediately before the write — the window
    // cannot be closed entirely without file locks, but it shrinks to the
    // rename itself.
    if crate::hash::file_content_hash(&root.join(CHAPPA_YML_NAME)).as_deref()
        != Some(loaded_hash.as_str())
    {
        return Err(CHANGED_ON_DISK.to_owned());
    }
    yml.write(&root).map_err(|e| {
        format!(
            "cannot write {}: {e}",
            root.join(CHAPPA_YML_NAME).display()
        )
    })?;
    Ok(yml)
}

/// Copy the project file → `chappa.yml.chappa-bak` unless that file already
/// exists. The guard is the FILE (`create_new`: an existing backup is never
/// overwritten, whatever projects.json says — the flag can lag the disk when
/// a project is removed and re-added, or when two chappa-ai instances share a
/// config); the per-project `yml_backed_up` flag is only an optimisation that
/// skips the filesystem probe. Recorded BEFORE the write so a write failure
/// after a successful copy still leaves the flag true — the backup exists,
/// and that is what the flag asserts.
fn ensure_backup(store: &mut ProjectStore, id: u32, root: &Path) -> Result<(), String> {
    if store.is_yml_backed_up(id) {
        return Ok(());
    }
    let src = root.join(CHAPPA_YML_NAME);
    let dst = root.join(BACKUP_NAME);
    match fs::OpenOptions::new().write(true).create_new(true).open(&dst) {
        Ok(mut out) => {
            let mut input = fs::File::open(&src)
                .map_err(|e| format!("cannot back up {}: {e}", src.display()))?;
            if let Err(e) = std::io::copy(&mut input, &mut out) {
                // A half-written backup is worse than none: remove it so the
                // next attempt starts over.
                drop(out);
                let _ = fs::remove_file(&dst);
                return Err(format!("cannot back up {}: {e}", src.display()));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // Someone (an earlier install, a re-added project) already made
            // the backup: keep it — it is the pre-chappa original.
        }
        Err(e) => return Err(format!("cannot back up {}: {e}", src.display())),
    }
    store.mark_yml_backed_up(id)
}

/// What a reload has to do, computed from the OLD runtime definitions and the
/// NEW file. Names in `changed` differ in a spawn input
/// ([`ProcessDef::runtime_eq`]); a process whose only change is policy
/// (`auto_start`, `auto_restart`) or unknown keys lands in `policy_only` so the
/// caller updates its stored def WITHOUT a restart.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProcessDiff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<String>,
    pub policy_only: Vec<String>,
}

/// Diff by NAME (a rename reads as remove + add — the runtime cannot know a
/// moved key is "the same" process).
pub fn diff_processes(
    old: &IndexMap<String, ProcessDef>,
    new: &IndexMap<String, ProcessDef>,
) -> ProcessDiff {
    let mut diff = ProcessDiff::default();
    for (name, def) in new {
        match old.get(name) {
            None => diff.added.push(name.clone()),
            Some(prev) if !prev.runtime_eq(def) => diff.changed.push(name.clone()),
            Some(prev) if prev != def => diff.policy_only.push(name.clone()),
            Some(_) => {}
        }
    }
    for name in old.keys() {
        if !new.contains_key(name) {
            diff.removed.push(name.clone());
        }
    }
    diff
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A fixture with unknown keys at BOTH levels, an exotic-but-valid value
    /// set (multi-line string, nested map, list of maps, numbers, an env whose
    /// keys are deliberately NOT alphabetical), and three processes.
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

    fn project(text: &str) -> (tempfile::TempDir, ProjectStore, u32) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join(CHAPPA_YML_NAME), text).unwrap();
        let mut store = ProjectStore::new(dir.path().join("projects.json"));
        let id = store.add(root, "fixture".into(), None).unwrap().id;
        (dir, store, id)
    }

    fn unknown_subtrees(yml: &ProjectYml) -> String {
        // Re-serialize ONLY the unknown subtrees (top-level + per process, in
        // order) so the comparison is byte-level on exactly what the
        // passthrough promises.
        let mut out = serde_yaml::to_string(&yml.unknown).unwrap();
        for (name, p) in &yml.processes {
            out.push_str(&format!("--- {name}\n"));
            out.push_str(&serde_yaml::to_string(&p.unknown).unwrap());
        }
        out
    }

    #[test]
    fn round_trip_keeps_unknown_keys_and_order_byte_identical_across_a_mutation() {
        let (_dir, mut store, id) = project(FIXTURE);
        let before = ProjectYml::parse(FIXTURE).unwrap();
        let unknown_before = unknown_subtrees(&before);

        // Mutate ONE process (beta's command) through the guarded path.
        let written = apply(&mut store, id, |root, yml| {
            let mut def = yml.processes["beta"].clone();
            def.command = "py b2.py".into();
            yml.edit_process(root, "beta", "beta", def)
        })
        .unwrap();
        assert_eq!(written.processes["beta"].command, "py b2.py");

        // Reload from DISK and compare the re-serialized unknown subtrees.
        let root = store.get(id).unwrap().path.clone();
        let after = ProjectYml::load_result(&root).unwrap();
        assert_eq!(unknown_subtrees(&after), unknown_before);
        // Top-level unknown keys keep their relative order…
        assert_eq!(
            after.unknown.keys().collect::<Vec<_>>(),
            vec!["theme", "plugins", "notes"]
        );
        // …the multi-line scalar is verbatim…
        assert_eq!(
            after.unknown["notes"].as_str(),
            Some("keep me\nverbatim\n")
        );
        // …process order is untouched, and env order is the FILE order, not
        // alphabetical.
        assert_eq!(
            after.processes.keys().collect::<Vec<_>>(),
            vec!["alpha", "beta", "gamma"]
        );
        assert_eq!(
            after.processes["alpha"].env.keys().collect::<Vec<_>>(),
            vec!["ZED", "ALPHA", "MID"]
        );
        // Known fields of the untouched processes are unchanged.
        assert!(!after.processes["alpha"].auto_start);
        assert_eq!(after.processes["beta"].working_dir, Some(PathBuf::from("sub")));
        assert_eq!(after.processes["beta"].restart_when_changed, vec!["**/*.py"]);
    }

    #[test]
    fn rename_keeps_the_position_and_carries_unknown_keys() {
        let (_dir, mut store, id) = project(FIXTURE);
        let written = apply(&mut store, id, |root, yml| {
            let def = yml.processes["beta"].clone();
            yml.edit_process(root, "beta", "beta renamed", def)
        })
        .unwrap();
        assert_eq!(
            written.processes.keys().collect::<Vec<_>>(),
            vec!["alpha", "beta renamed", "gamma"]
        );
        let root = store.get(id).unwrap().path.clone();
        let reloaded = ProjectYml::load_result(&root).unwrap();
        assert_eq!(
            reloaded.processes.keys().collect::<Vec<_>>(),
            vec!["alpha", "beta renamed", "gamma"]
        );
        assert_eq!(
            reloaded.processes["beta renamed"].unknown["tags"],
            serde_yaml::from_str::<serde_yaml::Value>("[x, y]").unwrap()
        );
        // Renaming onto an existing name is refused, and nothing is written.
        let before = fs::read_to_string(root.join(CHAPPA_YML_NAME)).unwrap();
        let err = apply(&mut store, id, |root, yml| {
            let def = yml.processes["alpha"].clone();
            yml.edit_process(root, "alpha", "gamma", def)
        })
        .unwrap_err();
        assert!(err.contains("already exists"), "{err}");
        assert_eq!(fs::read_to_string(root.join(CHAPPA_YML_NAME)).unwrap(), before);
    }

    #[test]
    fn add_remove_and_validation() {
        let (_dir, mut store, id) = project(FIXTURE);
        // Add appends at the end.
        let written = apply(&mut store, id, |root, yml| {
            yml.add_process(
                root,
                "  delta  ",
                ProcessDef {
                    command: "py d.py".into(),
                    ..Default::default()
                },
            )
        })
        .unwrap();
        assert_eq!(written.processes.keys().last().map(String::as_str), Some("delta"));
        // Duplicate name, blank name, blank command, escaping working_dir:
        // every one refused.
        for (name, def) in [
            ("alpha", ProcessDef { command: "x".into(), ..Default::default() }),
            ("   ", ProcessDef { command: "x".into(), ..Default::default() }),
            ("ok", ProcessDef { command: "  ".into(), ..Default::default() }),
            (
                "ok",
                ProcessDef {
                    command: "x".into(),
                    working_dir: Some("../outside".into()),
                    ..Default::default()
                },
            ),
        ] {
            assert!(
                apply(&mut store, id, |root, yml| yml.add_process(root, name, def)).is_err(),
                "{name} must be refused"
            );
        }
        // Remove shifts the tail up.
        let written = apply(&mut store, id, |_, yml| yml.remove_process("alpha").map(|_| ()))
            .unwrap();
        assert_eq!(
            written.processes.keys().collect::<Vec<_>>(),
            vec!["beta", "gamma", "delta"]
        );
    }

    #[test]
    fn duplicate_to_writes_only_the_target_file() {
        let (_dir, mut store, source) = project(FIXTURE);
        let target_dir = tempfile::tempdir().unwrap();
        fs::write(
            target_dir.path().join(CHAPPA_YML_NAME),
            "processes:\n  alpha:\n    command: py already.py\n",
        )
        .unwrap();
        let target = store
            .add(target_dir.path().to_path_buf(), "target".into(), None)
            .unwrap()
            .id;
        let source_root = store.get(source).unwrap().path.clone();
        let source_before = fs::read_to_string(source_root.join(CHAPPA_YML_NAME)).unwrap();

        let defs: Vec<(String, ProcessDef)> = ProjectYml::load_result(&source_root)
            .unwrap()
            .processes
            .into_iter()
            .collect();
        let written = apply(&mut store, target, |root, yml| {
            for (name, def) in defs.clone() {
                yml.duplicate_process(root, &name, def)?;
            }
            Ok(())
        })
        .unwrap();
        // The clash with the target's own `alpha` gets the ` copy` suffix;
        // the rest land verbatim, with their unknown keys.
        assert_eq!(
            written.processes.keys().collect::<Vec<_>>(),
            vec!["alpha", "alpha copy", "beta", "gamma"]
        );
        assert_eq!(written.processes["alpha"].command, "py already.py");
        assert_eq!(written.processes["alpha copy"].unknown["priority"], serde_yaml::Value::from(7));
        // Source untouched, byte for byte; no backup appeared beside it.
        assert_eq!(
            fs::read_to_string(source_root.join(CHAPPA_YML_NAME)).unwrap(),
            source_before
        );
        assert!(!source_root.join(BACKUP_NAME).exists());
        assert!(target_dir.path().join(BACKUP_NAME).exists());

        // A second same-project duplicate walks the suffix sequence.
        let written = apply(&mut store, target, |root, yml| {
            let def = yml.processes["alpha"].clone();
            yml.duplicate_process(root, "alpha", def).map(|_| ())
        })
        .unwrap();
        assert!(written.processes.contains_key("alpha copy 2"));
    }

    #[test]
    fn backup_is_created_exactly_once_per_file_with_the_original_bytes() {
        let (_dir, mut store, id) = project(FIXTURE);
        let root = store.get(id).unwrap().path.clone();
        let bak = root.join(BACKUP_NAME);
        assert!(!bak.exists());

        apply(&mut store, id, |_, yml| yml.remove_process("gamma").map(|_| ())).unwrap();
        assert_eq!(fs::read_to_string(&bak).unwrap(), FIXTURE, "bak = pre-chappa original");
        let first_mtime = fs::metadata(&bak).unwrap().modified().unwrap();
        assert!(store.is_yml_backed_up(id));

        // Second write: the backup is NOT refreshed (it would lose the
        // comments the first copy preserved).
        std::thread::sleep(std::time::Duration::from_millis(30));
        apply(&mut store, id, |_, yml| yml.remove_process("beta").map(|_| ())).unwrap();
        assert_eq!(fs::read_to_string(&bak).unwrap(), FIXTURE);
        assert_eq!(fs::metadata(&bak).unwrap().modified().unwrap(), first_mtime);

        // The flag survives a store reopen.
        let reopened = ProjectStore::new(store.path().to_path_buf());
        assert!(reopened.is_yml_backed_up(id));
        assert!(!reopened.is_yml_backed_up(99));
    }

    /// The backup guard is the FILE, not the projects.json
    /// flag. A `.chappa-bak` that already exists (flag unset: the project was
    /// removed and re-added, or another install made it) is never overwritten.
    #[test]
    fn an_existing_backup_file_is_never_overwritten_even_when_the_flag_is_unset() {
        let (_dir, mut store, id) = project(FIXTURE);
        let root = store.get(id).unwrap().path.clone();
        let bak = root.join(BACKUP_NAME);
        fs::write(&bak, "# the original, with comments
").unwrap();
        assert!(!store.is_yml_backed_up(id), "flag lags the disk");

        apply(&mut store, id, |_, yml| yml.remove_process("gamma").map(|_| ())).unwrap();
        assert_eq!(
            fs::read_to_string(&bak).unwrap(),
            "# the original, with comments
",
            "an existing backup must survive"
        );
        // The write itself went through, and the flag caught up.
        assert!(!fs::read_to_string(root.join(CHAPPA_YML_NAME)).unwrap().contains("gamma"));
        assert!(store.is_yml_backed_up(id));
    }

    #[test]
    fn a_load_error_refuses_the_write_and_makes_no_backup() {
        let (_dir, mut store, id) = project("processes:\n  x:\n    auto_start: true\n");
        let root = store.get(id).unwrap().path.clone();
        let before = fs::read_to_string(root.join(CHAPPA_YML_NAME)).unwrap();
        let err = apply(&mut store, id, |root, yml| {
            yml.add_process(root, "y", ProcessDef { command: "z".into(), ..Default::default() })
        })
        .unwrap_err();
        assert!(err.contains("refusing to write"), "{err}");
        assert_eq!(fs::read_to_string(root.join(CHAPPA_YML_NAME)).unwrap(), before);
        assert!(!root.join(BACKUP_NAME).exists());
        assert!(!store.is_yml_backed_up(id));
        // Unknown project: also an error, nothing touched.
        assert!(apply(&mut store, 99, |_, _| Ok(())).is_err());
    }

    /// The watcher guard: a mutation that only touches process B must not
    /// restart process A. Tempdir + the fake-clock debounce.
    #[test]
    fn reload_diff_restarts_only_the_changed_process() {
        let (_dir, mut store, id) = project(FIXTURE);
        let root = store.get(id).unwrap().path.clone();
        let old = ProjectYml::load_result(&root).unwrap().processes;

        // Something edits B's command and flips A's auto_start (policy only).
        apply(&mut store, id, |root, yml| {
            let mut b = yml.processes["beta"].clone();
            b.command = "py b2.py".into();
            yml.edit_process(root, "beta", "beta", b)?;
            let mut a = yml.processes["alpha"].clone();
            a.auto_start = true;
            yml.edit_process(root, "alpha", "alpha", a)
        })
        .unwrap();

        // (The watcher-side burst coalescing is covered by
        // `watcher::tests::debounce_coalesces_a_burst_into_one_restart`.)
        let new = ProjectYml::load_result(&root).unwrap().processes;
        let diff = diff_processes(&old, &new);
        assert_eq!(diff.changed, vec!["beta"]);
        assert_eq!(diff.policy_only, vec!["alpha"]);
        assert!(diff.added.is_empty() && diff.removed.is_empty());
        // A no-op reload (our own write echoed back) restarts nothing.
        assert_eq!(diff_processes(&new, &new), ProcessDiff::default());
        // Add/remove/rename regimes.
        let mut renamed = new.clone();
        let def = renamed.shift_remove("gamma").unwrap();
        renamed.insert("delta".into(), def);
        let diff = diff_processes(&new, &renamed);
        assert_eq!(diff.added, vec!["delta"]);
        assert_eq!(diff.removed, vec!["gamma"]);
        assert!(diff.changed.is_empty());
    }

    /// Concurrent-change guard (added 2026-08-31): mutate the
    /// file on disk between load and save → write refused, the file on disk
    /// keeps the external change byte-identical. (The reload the callers fire
    /// on this refusal lives in src-tauri, on top of the same watcher path
    /// that reloads any external edit.)
    #[test]
    fn a_concurrent_disk_change_between_load_and_save_refuses_the_write() {
        let (_dir, mut store, id) = project(FIXTURE);
        let root = store.get(id).unwrap().path.clone();
        let external = "processes:\n  ext-added:\n    command: py s.py\n";

        // The edit closure runs between apply's fresh load and its write —
        // exactly where an external write can land.
        let err = apply(&mut store, id, |root, yml| {
            fs::write(root.join(CHAPPA_YML_NAME), external).unwrap();
            yml.remove_process("gamma").map(|_| ())
        })
        .unwrap_err();
        assert_eq!(err, CHANGED_ON_DISK);
        // The external change survives byte-identical — never last-writer-wins.
        assert_eq!(fs::read_to_string(root.join(CHAPPA_YML_NAME)).unwrap(), external);

        // The next apply loads the external content fresh and goes through.
        let written =
            apply(&mut store, id, |_, yml| yml.remove_process("ext-added").map(|_| ()))
                .unwrap();
        assert!(written.processes.is_empty());
    }

    /// The chappa-side toggles live in projects.json and must never leak into
    /// the yml, whatever state they are in when a write happens.
    #[test]
    fn favorites_and_renaming_toggles_never_appear_in_yml_output() {
        let (_dir, mut store, id) = project(FIXTURE);
        store.set_process_favorite(id, "alpha", true).unwrap();
        store.set_process_auto_rename_disabled(id, "beta", true).unwrap();
        store
            .set_notification_level(id, Some("gamma"), Some("none".into()))
            .unwrap();
        apply(&mut store, id, |root, yml| {
            let def = yml.processes["alpha"].clone();
            yml.edit_process(root, "alpha", "alpha", def)
        })
        .unwrap();
        let root = store.get(id).unwrap().path.clone();
        let text = fs::read_to_string(root.join(CHAPPA_YML_NAME)).unwrap();
        for needle in ["favorite", "auto_rename", "notification", "lesser_used"] {
            assert!(!text.contains(needle), "`{needle}` leaked into the project file:\n{text}");
        }
        // …while projects.json carries all three.
        let json = fs::read_to_string(store.path()).unwrap();
        assert!(json.contains("\"favorite\": true"), "{json}");
        assert!(json.contains("\"disable_auto_rename\": true"), "{json}");
        assert!(json.contains("\"notification_level\": \"none\""), "{json}");
    }
}
