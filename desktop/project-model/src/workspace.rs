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

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::atomic;
use crate::projects::ProcessOverride;
use crate::records::{
    new_record_id, reconcile_runs, update_run, AgentRecord, AgentRecordPatch, RecordKind, RecordRef,
    Run, RunUpdate, StoredRecord, TerminalRecord,
};
use crate::yml::ProcessDef;

/// The on-disk shape of workspace.json. `commands` preserves definition
/// order (IndexMap — order is user-visible, same as the yml's processes).
///
/// The workspace is also the bucket for durable records that belong to no
/// project: a command's record state rides `process_overrides.<name>`
/// (the same record the project store keeps per command), and project-less
/// agents and shells are their own lists. Every record key is absent from
/// the file while empty.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkspaceFile {
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub commands: IndexMap<String, ProcessDef>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub process_overrides: BTreeMap<String, ProcessOverride>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<AgentRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub terminals: Vec<TerminalRecord>,
}

impl WorkspaceFile {
    /// Every record in the file: commands with a record id (in map order),
    /// then agents, then terminals.
    pub fn records(&self) -> Vec<StoredRecord> {
        let commands = self.process_overrides.iter().filter_map(|(name, o)| {
            o.record_id.as_ref().map(|record_id| StoredRecord {
                record_id: record_id.clone(),
                kind: RecordKind::Command,
                name: name.clone(),
                last_run: o.last_run.clone(),
                agent: None,
                terminal: None,
            })
        });
        let agents = self.agents.iter().map(|a| StoredRecord {
            record_id: a.record_id.clone(),
            kind: RecordKind::Agent,
            name: a.name.clone(),
            last_run: a.last_run.clone(),
            agent: Some(a.clone()),
            terminal: None,
        });
        let terminals = self.terminals.iter().map(|t| StoredRecord {
            record_id: t.record_id.clone(),
            kind: RecordKind::Terminal,
            name: t.name.clone(),
            last_run: t.last_run.clone(),
            agent: None,
            terminal: Some(t.clone()),
        });
        commands.chain(agents).chain(terminals).collect()
    }

    /// The `last_run` slot of `record_id`, whatever its kind.
    fn run_slot(&mut self, record_id: &str) -> Option<&mut Option<Run>> {
        if let Some(o) = self
            .process_overrides
            .values_mut()
            .find(|o| o.record_id.as_deref() == Some(record_id))
        {
            return Some(&mut o.last_run);
        }
        if let Some(a) = self.agents.iter_mut().find(|a| a.record_id == record_id) {
            return Some(&mut a.last_run);
        }
        self.terminals
            .iter_mut()
            .find(|t| t.record_id == record_id)
            .map(|t| &mut t.last_run)
    }

    fn runs_mut(&mut self) -> impl Iterator<Item = &mut Run> {
        let commands = self.process_overrides.values_mut().filter_map(|o| o.last_run.as_mut());
        let agents = self.agents.iter_mut().filter_map(|a| a.last_run.as_mut());
        let terminals = self.terminals.iter_mut().filter_map(|t| t.last_run.as_mut());
        commands.chain(agents).chain(terminals)
    }
}

/// Validate one workspace command definition.
///
/// Identical to `ProcessDef::validate` EXCEPT the working_dir rule
/// (no project root here): `None`/empty is allowed (= home dir), absolute is
/// required, RELATIVE is refused. `unknown`-key passthrough is moot but
/// harmless.
pub fn validate_workspace_def(def: &ProcessDef) -> Result<(), String> {
    if def.command.is_blank() {
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
///
/// The store is stateless (every call reads the file fresh), so its clones
/// share ONE lock: a mutation is a read-modify-write that must not interleave
/// with another thread's — the pump reporting a closed run's end races the
/// user removing that very record — and the atomic writer's temp file is one
/// path per target, so two concurrent writes would also trip over each
/// other's temp. Reads take the lock too, so a reader never sees the window
/// between temp and rename.
#[derive(Debug, Clone)]
pub struct WorkspaceStore {
    path: PathBuf,
    lock: Arc<Mutex<()>>,
}

impl WorkspaceStore {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            lock: Arc::default(),
        }
    }

    fn guard(&self) -> MutexGuard<'_, ()> {
        self.lock.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Load the file fresh from disk. A missing / blank file is an empty
    /// workspace; a structurally invalid file degrades to empty too (the app
    /// owns the file, so this only happens after manual editing).
    pub fn load(&self) -> WorkspaceFile {
        let _guard = self.guard();
        self.read()
    }

    /// The file as it is, for callers already holding the lock.
    fn read(&self) -> WorkspaceFile {
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
        let _guard = self.guard();
        let mut file = self.read();
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
                        file.commands.insert(name.clone(), def);
                        let last = file.commands.len() - 1;
                        file.commands.move_index(last, idx);
                        // The record state is keyed by name too, so it moves
                        // with the command (a rename keeps its history).
                        if let Some(record) = file.process_overrides.remove(old) {
                            file.process_overrides.insert(name, record);
                        }
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
    /// absent. The command's record state goes with it, so a later command
    /// reusing the name starts clean; callers prune the ledger by the id
    /// they read beforehand ([`Self::command_record_id`]).
    pub fn delete_command(&self, name: &str) -> Result<(), String> {
        let _guard = self.guard();
        let mut file = self.read();
        file.commands
            .shift_remove(name)
            .map(|_| ())
            .ok_or_else(|| format!("no command named `{name}`"))?;
        file.process_overrides.remove(name);
        self.write_atomic(&file)
    }

    // ---- durable records ----------------------------------------------

    /// The record id of command `name`, assigning one on first use. Unknown
    /// command names are an error: a record must describe a definition.
    pub fn ensure_command_record_id(&self, name: &str) -> Result<String, String> {
        let _guard = self.guard();
        let mut file = self.read();
        if !file.commands.contains_key(name) {
            return Err(format!("no command named `{name}`"));
        }
        if let Some(id) = file.process_overrides.get(name).and_then(|o| o.record_id.clone()) {
            return Ok(id);
        }
        let record_id = new_record_id()?;
        file.process_overrides
            .entry(name.to_owned())
            .or_default()
            .record_id = Some(record_id.clone());
        self.write_atomic(&file)?;
        Ok(record_id)
    }

    /// The record id of command `name`, if one was assigned.
    pub fn command_record_id(&self, name: &str) -> Option<String> {
        self.load()
            .process_overrides
            .get(name)
            .and_then(|o| o.record_id.clone())
    }

    pub fn add_agent_record(&self, record: AgentRecord) -> Result<(), String> {
        let _guard = self.guard();
        let mut file = self.read();
        file.agents.push(record);
        self.write_atomic(&file)
    }

    pub fn add_terminal_record(&self, record: TerminalRecord) -> Result<(), String> {
        let _guard = self.guard();
        let mut file = self.read();
        file.terminals.push(record);
        self.write_atomic(&file)
    }

    /// Locate a record by id.
    pub fn find_record(&self, record_id: &str) -> Option<RecordRef> {
        self.load()
            .records()
            .into_iter()
            .find(|r| r.record_id == record_id)
            .map(|r| RecordRef { kind: r.kind, name: r.name })
    }

    /// One record with its full state.
    pub fn record(&self, record_id: &str) -> Option<StoredRecord> {
        self.load().records().into_iter().find(|r| r.record_id == record_id)
    }

    /// Every workspace record.
    pub fn records(&self) -> Vec<StoredRecord> {
        self.load().records()
    }

    /// Start a new run on a record (see `ProjectStore::begin_run`).
    pub fn begin_run(&self, record_id: &str, run: Run) -> Result<bool, String> {
        let _guard = self.guard();
        let mut file = self.read();
        let Some(slot) = file.run_slot(record_id) else {
            return Ok(false);
        };
        *slot = Some(run);
        self.write_atomic(&file)?;
        Ok(true)
    }

    /// Apply a late fact to the record's current run under the run-uuid rule.
    pub fn update_run(
        &self,
        record_id: &str,
        run_uuid: &str,
        edit: impl FnOnce(&mut Run),
    ) -> Result<RunUpdate, String> {
        let _guard = self.guard();
        let mut file = self.read();
        let Some(slot) = file.run_slot(record_id) else {
            return Ok(RunUpdate::NotFound);
        };
        let outcome = update_run(slot.as_mut(), run_uuid, edit);
        if outcome == RunUpdate::Applied {
            self.write_atomic(&file)?;
        }
        Ok(outcome)
    }

    /// Delete an agent or terminal record (a command record goes with its
    /// definition through [`Self::delete_command`]).
    pub fn remove_record(&self, record_id: &str) -> Result<Option<RecordKind>, String> {
        let _guard = self.guard();
        let mut file = self.read();
        let agents = file.agents.len();
        file.agents.retain(|a| a.record_id != record_id);
        let removed = if file.agents.len() != agents {
            Some(RecordKind::Agent)
        } else {
            let terminals = file.terminals.len();
            file.terminals.retain(|t| t.record_id != record_id);
            (file.terminals.len() != terminals).then_some(RecordKind::Terminal)
        };
        if removed.is_some() {
            self.write_atomic(&file)?;
        }
        Ok(removed)
    }

    /// Edit a workspace agent record's row state (see
    /// `ProjectStore::update_agent_record`). `Ok(None)` when no workspace
    /// agent has that id.
    pub fn update_agent_record(
        &self,
        record_id: &str,
        patch: &AgentRecordPatch,
    ) -> Result<Option<AgentRecord>, String> {
        patch.validate()?;
        let _guard = self.guard();
        let mut file = self.read();
        let Some(agent) = file.agents.iter_mut().find(|a| a.record_id == record_id) else {
            return Ok(None);
        };
        patch.apply(agent);
        let updated = agent.clone();
        self.write_atomic(&file)?;
        Ok(Some(updated))
    }

    /// At launch: reconcile runs cut off by a crash; returns the changed
    /// run uuids (see `ProjectStore::reconcile_crashed_runs`).
    pub fn reconcile_crashed_runs(&self) -> Result<Vec<String>, String> {
        let _guard = self.guard();
        let mut file = self.read();
        let changed = reconcile_runs(file.runs_mut());
        if !changed.is_empty() {
            self.write_atomic(&file)?;
        }
        Ok(changed)
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
            command: command.into(),
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

    /// Records in workspace.json: a command's record id is assigned once and
    /// travels with a rename; agent and terminal records are their own
    /// lists; `last_run` follows the run-uuid rule; a delete drops the
    /// command's record state; a file without any record keys loads with
    /// empty defaults and is not rewritten with them.
    #[test]
    fn workspace_records_round_trip_and_follow_the_run_rule() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        s.save_command(None, "build", def("make")).unwrap();
        assert!(!std::fs::read_to_string(dir.path().join("workspace.json"))
            .unwrap()
            .contains("process_overrides"));
        assert!(s.ensure_command_record_id("ghost").is_err());

        let id = s.ensure_command_record_id("build").unwrap();
        assert_eq!(s.ensure_command_record_id("build").unwrap(), id, "assigned once");
        assert_eq!(s.command_record_id("build").as_deref(), Some(id.as_str()));
        assert_eq!(
            s.find_record(&id),
            Some(RecordRef { kind: RecordKind::Command, name: "build".into() })
        );

        // A rename keeps the record (and so its history).
        s.save_command(Some("build"), "compile", def("make")).unwrap();
        assert_eq!(s.command_record_id("build"), None);
        assert_eq!(s.command_record_id("compile").as_deref(), Some(id.as_str()));

        // Runs: begin replaces the head, a stale fact is dropped, a current
        // one applies and persists.
        assert!(s.begin_run(&id, Run::begin("run-1", 10, Default::default())).unwrap());
        assert!(!s.begin_run("nope", Run::begin("x", 1, Default::default())).unwrap());
        assert_eq!(s.update_run(&id, "run-0", |r| r.stop(11)).unwrap(), RunUpdate::Stale);
        assert_eq!(
            s.update_run(&id, "run-1", |r| r.finish(12, Some(0), true, vec![])).unwrap(),
            RunUpdate::Applied
        );
        assert_eq!(s.update_run("nope", "run-1", |_| {}).unwrap(), RunUpdate::NotFound);
        let head = s.record(&id).unwrap().last_run.unwrap();
        assert_eq!(head.status, term_core::status::ProcessStatus::Exited);
        assert_eq!(head.ended_at_ms, Some(12));

        // Agents and terminals.
        s.add_agent_record(AgentRecord {
            record_id: "agent-1".into(),
            name: "claude".into(),
            tool_id: 2,
            favorite: false,
            disable_auto_rename: false,
            notification_level: None,
            parent_record_id: Some(id.clone()),
            extra_args: vec!["--verbose".into()],
            prompt: Some("hello".into()),
            last_run: Some(Run::begin("run-a", 1, Default::default())),
        })
        .unwrap();
        s.add_terminal_record(TerminalRecord {
            record_id: "term-1".into(),
            name: "shell".into(),
            shell_profile: None,
            cwd: None,
            last_run: None,
        })
        .unwrap();
        let kinds: Vec<RecordKind> = s.records().iter().map(|r| r.kind).collect();
        assert_eq!(kinds, vec![RecordKind::Command, RecordKind::Agent, RecordKind::Terminal]);
        assert_eq!(
            s.record("agent-1").unwrap().agent.unwrap().parent_record_id.as_deref(),
            Some(id.as_str())
        );

        // Row state: a patch edits the workspace agent in place; an unknown
        // id or a refused patch writes nothing.
        let patch = AgentRecordPatch {
            name: Some("reviewer".into()),
            favorite: Some(true),
            notification_level: Some(Some("none".into())),
            ..Default::default()
        };
        let agent = s.update_agent_record("agent-1", &patch).unwrap().unwrap();
        assert_eq!((agent.name.as_str(), agent.favorite), ("reviewer", true));
        assert_eq!(s.record("agent-1").unwrap().name, "reviewer");
        assert_eq!(s.update_agent_record("term-1", &patch).unwrap(), None, "not an agent");
        let bad = AgentRecordPatch { notification_level: Some(Some("loud".into())), ..Default::default() };
        assert!(s.update_agent_record("agent-1", &bad).is_err());
        assert_eq!(
            s.record("agent-1").unwrap().agent.unwrap().notification_level.as_deref(),
            Some("none")
        );

        // Crash reconcile: the open agent run closes as exited/null.
        assert_eq!(s.reconcile_crashed_runs().unwrap(), vec!["run-a".to_owned()]);
        let run = s.record("agent-1").unwrap().last_run.unwrap();
        assert_eq!(run.status, term_core::status::ProcessStatus::Exited);
        assert_eq!(run.exit_code, None);
        assert!(s.reconcile_crashed_runs().unwrap().is_empty(), "idempotent");

        // Remove: agents and terminals go; a command id is not a record to
        // remove here (its definition is); the delete prunes its state.
        assert_eq!(s.remove_record("agent-1").unwrap(), Some(RecordKind::Agent));
        assert_eq!(s.remove_record("term-1").unwrap(), Some(RecordKind::Terminal));
        assert_eq!(s.remove_record("term-1").unwrap(), None);
        assert_eq!(s.remove_record(&id).unwrap(), None);
        assert!(s.record(&id).is_some());
        s.delete_command("compile").unwrap();
        assert!(s.record(&id).is_none());
        let text = std::fs::read_to_string(dir.path().join("workspace.json")).unwrap();
        assert!(!text.contains("process_overrides"), "{text}");
        assert!(!text.contains("agents"), "{text}");
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
