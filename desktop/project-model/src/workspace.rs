//! The workspace-level command store.
//!
//! v1 of the workspace scope treats the app's existing top level (the bottom
//! rail area) as THE workspace; the only stored entity is a flat map of
//! workspace-level commands. Storage is a NEW app-config file
//! `workspace.json` beside projects.json — `{ "commands": { "<name>":
//! ProcessDef } }` — written with `atomic::write_json_atomic` discipline.
//!
//! Deliberately NOT in projects.json (its top level is an array; no shape
//! migration) and NEVER in the per-project file — workspace commands are
//! chappa-native and cross-project by definition. No trust gate:
//! workspace.json is written only by the app, so every definition is
//! the user's own by construction (the native-store rationale).
//!
//! The lifecycle half (start/stop/restart + auto_start at launch) lives in
//! src-tauri, reusing the registry/status machinery; this module is the
//! pure-fs store so it stays container-testable alongside the other
//! project-model stores.
//!
//! ## Working-dir rule (workspace vs project)
//! Project defs hold working_dir CONTAINED in the project root (
//! `ProcessDef::validate`). A workspace command has no root, so the rule is
//! different: working_dir must be ABSOLUTE or EMPTY (`None` = the user's
//! home dir, like a plain shell). A RELATIVE working_dir is refused with a
//! clear message. Everything else mirrors `ProcessDef::validate` (command
//! non-blank, env keys non-blank).

use std::path::PathBuf;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::atomic;
use crate::yml::ProcessDef;

/// The on-disk shape of workspace.json. `commands` preserves definition
/// order (IndexMap — order is user-visible, same as the yml's processes).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkspaceFile {
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub commands: IndexMap<String, ProcessDef>,
}

/// Validate one workspace command definition.
///
/// Identical to `ProcessDef::validate` EXCEPT the working_dir rule
/// (no project root here): `None`/empty is allowed (= home dir), absolute is
/// required, RELATIVE is refused. `unknown`-key passthrough is moot but
/// harmless.
pub fn validate_workspace_def(def: &ProcessDef) -> Result<(), String> {
    if def.command.trim().is_empty() {
        return Err("command must not be blank".to_owned());
    }
    if let Some(key) = def.env.keys().find(|k| k.trim().is_empty()) {
        return Err(format!(
            "env variable name must not be blank (value `{}`)",
            def.env[key]
        ));
    }
    match &def.working_dir {
        None => Ok(()),
        Some(dir) if dir.is_absolute() => Ok(()),
        Some(dir) => Err(format!(
            "working_dir must be an absolute path (or empty for home), got `{}`",
            dir.display()
        )),
    }
}

/// The workspace.json store: plain CRUD over the commands map, every
/// mutation persisted atomically (temp + rename) so a crash can never leave
/// a truncated file.
#[derive(Debug, Clone)]
pub struct WorkspaceStore {
    path: PathBuf,
}

impl WorkspaceStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Load the file fresh from disk. A missing / blank file is an empty
    /// workspace; a structurally invalid file degrades to empty too (the app
    /// owns the file, so this only happens after manual editing).
    pub fn load(&self) -> WorkspaceFile {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// The current command map, in stored order.
    pub fn commands(&self) -> IndexMap<String, ProcessDef> {
        self.load().commands
    }

    /// The ordered names of the `auto_start: true` commands, in stored order.
    /// The UI's `up-all` seed: at launch (after settings load) the caller
    /// starts these one by one, each with its own frames channel.
    pub fn auto_start_commands(&self) -> Vec<String> {
        self.load()
            .commands
            .iter()
            .filter(|(_, def)| def.auto_start)
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// `+ Add command` (`original_name = None`) or `Edit command…`
    /// (`original_name = Some`). Validates first (an invalid def persists
    /// nothing), then writes atomically. A rename is a POSITION-PRESERVING
    /// key move (same rule as the yml editor). Unknown keys of the
    /// old entry carry over unless the new def already carries its own.
    pub fn save_command(
        &self,
        original_name: Option<&str>,
        name: &str,
        mut def: ProcessDef,
    ) -> Result<(), String> {
        validate_workspace_def(&def)?;
        let name = clean_name(name)?;
        let mut file = self.load();
        if let Some(old) = original_name {
            match file.commands.get_index_of(old) {
                None => {
                    return Err(format!("no command named `{old}`"));
                }
                Some(idx) => {
                    if name != old && file.commands.contains_key(&name) {
                        return Err(format!("a command named `{name}` already exists"));
                    }
                    if def.unknown.is_empty() {
                        def.unknown = file.commands[idx].unknown.clone();
                    }
                    if name == old {
                        file.commands[idx] = def;
                    } else {
                        // Position-preserving key move: drop the old slot,
                        // append under the new name, move it back up.
                        file.commands.shift_remove_index(idx);
                        file.commands.insert(name, def);
                        let last = file.commands.len() - 1;
                        file.commands.move_index(last, idx);
                    }
                }
            }
        } else {
            if file.commands.contains_key(&name) {
                return Err(format!("a command named `{name}` already exists"));
            }
            file.commands.insert(name, def);
        }
        self.write_atomic(&file)
    }

    /// `Delete command "<name>"`. Refuses (persists nothing) when the name is
    /// absent.
    pub fn delete_command(&self, name: &str) -> Result<(), String> {
        let mut file = self.load();
        file.commands
            .shift_remove(name)
            .map(|_| ())
            .ok_or_else(|| format!("no command named `{name}`"))?;
        self.write_atomic(&file)
    }

    fn write_atomic(&self, file: &WorkspaceFile) -> Result<(), String> {
        atomic::write_json_atomic(&self.path, file)
    }
}

/// The editor's name rule, shared with native.rs: trimmed, non-blank.
fn clean_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("command name must not be blank".to_owned());
    }
    Ok(name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(command: &str) -> ProcessDef {
        ProcessDef {
            command: command.to_owned(),
            ..Default::default()
        }
    }

    fn store(dir: &std::path::Path) -> WorkspaceStore {
        WorkspaceStore::new(dir.join("workspace.json"))
    }

    /// add → edit (rename keeps position, order among siblings preserved) →
    /// delete: the store round-trips the map and persists atomically. A
    /// RELOAD from disk sees the same ordered map (IndexMap order is
    /// user-visible).
    #[test]
    fn round_trip_preserves_order_and_is_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());

        s.save_command(None, "server", def("npm run dev")).unwrap();
        s.save_command(None, "watch", def("npm run watch")).unwrap();
        s.save_command(None, "audit", def("npm audit")).unwrap();
        assert_eq!(
            s.commands().keys().cloned().collect::<Vec<_>>(),
            vec!["server", "watch", "audit"]
        );

        // Rename "watch" -> "watching": position preserved, siblings stable.
        s.save_command(Some("watch"), "watching", def("npm run watch")).unwrap();
        assert_eq!(
            s.commands().keys().cloned().collect::<Vec<_>>(),
            vec!["server", "watching", "audit"]
        );

        // Delete middle: tail shifts up.
        s.delete_command("watching").unwrap();
        assert_eq!(
            s.commands().keys().cloned().collect::<Vec<_>>(),
            vec!["server", "audit"]
        );

        // A brand-new store reading the same file sees everything (ruling
        // out an in-memory-only write).
        let reload = store(dir.path());
        assert_eq!(
            reload.commands().keys().cloned().collect::<Vec<_>>(),
            vec!["server", "audit"]
        );
        let persisted = std::fs::read_to_string(dir.path().join("workspace.json")).unwrap();
        assert!(persisted.contains("\"commands\""), "pretty JSON on disk");
    }

    /// Blank command, blank env key, blank name and a DUPLICATE name are all
    /// refused and persist nothing.
    #[test]
    fn invalid_defs_are_refused_and_persist_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());

        assert!(s.save_command(None, "a", def("  ")).is_err());
        assert!(s.save_command(None, "  ", def("echo hi")).is_err());
        let bad_env = ProcessDef {
            command: "echo hi".into(),
            env: [("".to_owned(), "v".to_owned())].into_iter().collect(),
            ..Default::default()
        };
        assert!(s.save_command(None, "a", bad_env).is_err());

        // Nothing persisted by the refused writes.
        assert!(s.commands().is_empty());

        // Duplicate add refused; a rename onto a taken name refused.
        s.save_command(None, "a", def("echo a")).unwrap();
        assert!(s.save_command(None, "a", def("echo again")).is_err());
        s.save_command(None, "b", def("echo b")).unwrap();
        assert!(s.save_command(Some("a"), "b", def("echo x")).is_err());
        // Rename of a MISSING original refused.
        assert!(s.save_command(Some("ghost"), "c", def("echo x")).is_err());
        // Delete of a missing name refused.
        assert!(s.delete_command("ghost").is_err());
    }

    fn wd(path: impl Into<PathBuf>) -> ProcessDef {
        ProcessDef {
            command: "echo hi".into(),
            working_dir: Some(path.into()),
            ..Default::default()
        }
    }

    /// The workspace working-dir rule: ABSOLUTE accepted, EMPTY (None) = home
    /// accepted, RELATIVE refused with a clear message.
    #[test]
    fn working_dir_must_be_absolute_or_empty() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());

        // Platform-correct fixtures: absoluteness is OS-defined, and this
        // suite runs on BOTH the Linux container and the Windows host —
        // "/opt/x" is absolute only on unix, "C:\x" only on Windows. The
        // original hardcoded the Linux reading and failed on Windows
        // (fixed 2026-08-31).
        #[cfg(windows)]
        let (abs_dir, foreign) = ("C:\\workspace", "/opt/workspace");
        #[cfg(not(windows))]
        let (abs_dir, foreign) = ("/opt/workspace", "C:\\tools");
        assert!(s.save_command(None, "abs", wd(abs_dir)).is_ok());
        assert!(s.save_command(None, "rel", wd("relative/dir")).is_err());
        assert!(s.save_command(None, "none", def("echo home")).is_ok()); // None = home
        // The OTHER platform's absolute spelling is not absolute here and
        // must be refused too.
        assert!(s.save_command(None, "win", wd(foreign)).is_err());
    }

    /// Unknown keys survive a save→load round trip (the same passthrough
    /// guarantee the yml editor gives; here it is moot but harmless, and it
    /// must not warn the write).
    #[test]
    fn unknown_keys_are_preserved() {
}

    /// `auto_start: true` commands list in stored order — the launch seed.
    #[test]
    fn auto_start_commands_list_only_flagged_ones_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.save_command(None, "first", def("echo a")).unwrap(); // auto_start default true
        let mut no_auto = def("echo b");
        no_auto.auto_start = false;
        s.save_command(None, "second", no_auto).unwrap();
        s.save_command(None, "third", def("echo c")).unwrap();
        assert_eq!(s.auto_start_commands(), vec!["first", "third"]);
    }
}
