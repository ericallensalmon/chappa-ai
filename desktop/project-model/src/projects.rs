//! Stored-projects list (`projects.json`) with the trust-gate hash.
//!
//! The app-config-dir JSON is `[{id, name, path, icon}]` — the stored-project
//! list. One
//! non-schema extension: each entry carries `trusted_hash`, the content-hash
//! of the per-project file the user last confirmed via the Run/Skip trust gate.
//! When the file's hash differs from the recorded one the gate re-arms (the
//! per-command trust review, personal scale).
//!
//! The config path is INJECTED so tests use a tempdir and the app supplies its
//! real app-config dir at startup.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::records::{
    new_record_id, reconcile_runs, update_run, AgentRecord, AgentRecordPatch, RecordKind, RecordRef,
    Run, RunUpdate, StoredRecord, TerminalRecord,
};
use crate::yml::ProcessDef;

/// The notification-level vocabulary. On the wire and on disk these
/// are the literals `"all" | "important" | "none"` — lowercase; the UI labels
/// (All/Important/None) are presentation only.
///
/// EVIDENCE NOTE: only the literal `"all"` (the shipped default) is confirmed by
/// observation. Lowercase `"important"` / `"none"` is INFERENCE from the UI
/// labels plus the `"all"` casing — nothing external pins the other two.
pub const NOTIFICATION_LEVELS: [&str; 3] = ["all", "important", "none"];

/// `None` (unset) is always valid: it means "inherit", "level: None
/// CLEARS (removes the override / resets the project field to None = 'all')".
pub(crate) fn validate_level(level: Option<&str>) -> Result<(), String> {
    match level {
        None => Ok(()),
        Some(l) if NOTIFICATION_LEVELS.contains(&l) => Ok(()),
        Some(l) => Err(format!(
            "invalid notification level: `{l}` (expected all|important|none)"
        )),
    }
}

/// One stored project row. Serialized exactly as `{id, name, path, icon}` plus
/// the internal `trusted_hash` and the notification fields.
///
/// The struct has NO `rename_all` — projects.json stays snake_case, so the
/// new keys are literally `notification_level` and `process_overrides`. Both
/// are `skip_serializing_if = "Option::is_none"`, so a projects.json written
/// by an earlier build round-trips byte-identical while untouched ("existing files without the new fields load unchanged").
// `PartialEq` only: a row now embeds `ProcessDef`, whose `unknown`
// passthrough is `serde_yaml::Value` and can carry floats — so `Eq` is gone,
// exactly as `ProcessDef` itself is `PartialEq` only. `Eq` is unused for
// `Project` (store lookups are by `u32` id), so this is a no-op for callers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub id: u32,
    pub name: String,
    pub path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trusted_hash: Option<String>,
    /// Project default. `None` = 'all' (the schema default); resolution
    /// itself — "per-process override → project default → 'all'" — is the
    /// UI's job, this store only holds the RAW value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notification_level: Option<String>,
    /// The rail badge's overrides: up to three letters, and a palette colour
    /// key (see [`ProjectStore::COLORS`]). Absent = derived from the name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initials: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Per-process chappa-side state, `process name -> override record`
    /// (the level; the favorite + renaming toggle). Absent
    /// entirely when no process has anything set (an entry is dropped when it
    /// empties, the map when the last entry goes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_overrides: Option<BTreeMap<String, ProcessOverride>>,
    /// `true` once chappa-ai has copied this project's chappa.yml to
    /// `chappa.yml.chappa-bak` — the one-time pre-first-write backup. Absent
    /// (= false) until then, so untouched rows stay byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yml_backed_up: Option<bool>,
    /// The native process definitions THE STORE owns, by process
    /// name. `None` (absent) until a project is migrated or mutated; after
    /// that `Some(map)` carries the defs (an EMPTY `Some({})` is a valid
    /// project that has surrendered all its processes). Order is
    /// user-visible and preserved through round trips — the same guarantee
    /// the project file gives — so `IndexMap`, not a `BTreeMap`. The
    /// unknown-key passthrough rides each [`ProcessDef`], so unmodelled
    /// keys survive an import→mutate→backwrite round trip onto and off the
    /// disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub processes: Option<IndexMap<String, ProcessDef>>,
    /// The project's agent records (durable rows; see `records.rs`), in
    /// creation order. Absent from the file while empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<AgentRecord>,
    /// The project's terminal (shell) records, likewise.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub terminals: Vec<TerminalRecord>,
}

/// Everything chappa-ai stores about one project process OUTSIDE the yml
/// ("chappa-side only … NOT written to yml"). On disk under
/// `process_overrides.<name>`:
///
/// ```json
/// {"notification_level": "none", "favorite": true, "disable_auto_rename": true,
///  "record_id": "…", "last_run": {…}}
/// ```
///
/// Absent fields are their defaults, and a record with nothing set is pruned.
/// LEGACY: earlier builds stored a bare level string (`"server": "none"`); that shape
/// still deserializes (into `notification_level`) and is rewritten as the
/// record on the next save.
///
/// A command IS a durable record, so its record state lives here too:
/// `record_id` (assigned the first time the command is opened or started,
/// then stable for the command's life — it moves with a rename and goes with
/// a delete) and `last_run`, the head of its run ledger.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(from = "ProcessOverrideRepr")]
pub struct ProcessOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notification_level: Option<String>,
    /// `Add to favorites`: sorts first within the COMMANDS section.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub favorite: bool,
    /// `Disable automatic renaming`: OSC title events stop renaming the row.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disable_auto_rename: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run: Option<Run>,
}

impl ProcessOverride {
    /// True when every field is at its default — the record carries nothing
    /// and is dropped from the map.
    fn is_empty(&self) -> bool {
        *self == ProcessOverride::default()
    }
}

/// The two on-disk shapes a `process_overrides` value can take.
#[derive(Deserialize)]
#[serde(untagged)]
enum ProcessOverrideRepr {
    Legacy(String),
    Record {
        #[serde(default)]
        notification_level: Option<String>,
        #[serde(default)]
        favorite: bool,
        #[serde(default)]
        disable_auto_rename: bool,
        #[serde(default)]
        record_id: Option<String>,
        #[serde(default)]
        last_run: Option<Run>,
    },
}

impl From<ProcessOverrideRepr> for ProcessOverride {
    fn from(repr: ProcessOverrideRepr) -> Self {
        match repr {
            ProcessOverrideRepr::Legacy(level) => ProcessOverride {
                notification_level: Some(level),
                ..Default::default()
            },
            ProcessOverrideRepr::Record {
                notification_level,
                favorite,
                disable_auto_rename,
                record_id,
                last_run,
            } => ProcessOverride {
                notification_level,
                favorite,
                disable_auto_rename,
                record_id,
                last_run,
            },
        }
    }
}

impl Project {
    /// The whole override record for `process`, if any field is set.
    pub fn process_override(&self, process: &str) -> Option<&ProcessOverride> {
        self.process_overrides.as_ref().and_then(|m| m.get(process))
    }

    /// The RAW stored override for `process` (no resolution — the UI resolves
    /// "per-process override → project default → 'all'").
    pub fn process_notification_level(&self, process: &str) -> Option<&str> {
        self.process_override(process)
            .and_then(|o| o.notification_level.as_deref())
    }

    /// Wire DTO shape: exactly `{id, name, path, icon}` (trust state stays
    /// server-side).
    pub fn dto(&self) -> ProjectDto<'_> {
        ProjectDto {
            id: self.id,
            name: &self.name,
            path: &self.path,
            icon: self.icon.as_deref(),
        }
    }
}

/// Wire DTO shape: exactly `{id, name, path, icon}` (trust state stays
/// server-side).
///
/// Deliberately does NOT extend this DTO: the notification fields ride
/// the src-tauri DTOs (`ProjectInfoDto` / `ProjectProcessDto`) instead, so this
/// stays the four-key shape `dto_shape_excludes_trust` pins.
#[derive(Debug, Clone, Serialize)]
pub struct ProjectDto<'a> {
    pub id: u32,
    pub name: &'a str,
    pub path: &'a Path,
    pub icon: Option<&'a str>,
}

/// JSON store over an injected path. Loads lazily from disk on construction
/// and persists (atomically) on every mutation.
#[derive(Debug, Clone)]
pub struct ProjectStore {
    path: PathBuf,
    projects: Vec<Project>,
    next_id: u32,
}

impl ProjectStore {
    /// `path` is the projects.json location (injected for tests). A missing or
    /// unreadable file is an empty store, not an error.
    pub fn new(path: PathBuf) -> Self {
        let projects = fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Vec<Project>>(&text).ok())
            .unwrap_or_default();
        let next_id = projects.iter().map(|p| p.id).max().map_or(1, |m| m + 1);
        Self {
            path,
            projects,
            next_id,
        }
    }

    /// Reload from disk (discards in-memory changes). Used when the store may
    /// have changed out from under us.
    pub fn reload(&mut self) {
        *self = Self::new(self.path.clone());
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn list(&self) -> &[Project] {
        &self.projects
    }

    pub fn get(&self, id: u32) -> Option<&Project> {
        self.projects.iter().find(|p| p.id == id)
    }

    /// Add a project. Idempotent by canonical path: re-adding the same
    /// directory returns the existing row.
    pub fn add(
        &mut self,
        path: PathBuf,
        name: String,
        icon: Option<String>,
    ) -> Result<Project, String> {
        if let Some(existing) = self.find_by_path(&path) {
            return Ok(existing.clone());
        }
        let id = self.next_id;
        self.next_id += 1;
        let project = Project {
            id,
            name,
            path,
            icon,
            trusted_hash: None,
            notification_level: None,
            initials: None,
            color: None,
            process_overrides: None,
            yml_backed_up: None,
            processes: None,
            agents: Vec::new(),
            terminals: Vec::new(),
        };
        self.projects.push(project.clone());
        self.save()?;
        Ok(project)
    }

    /// Remove by id. Never touches the project directory.
    pub fn remove(&mut self, id: u32) -> Result<(), String> {
        let before = self.projects.len();
        self.projects.retain(|p| p.id != id);
        if self.projects.len() == before {
            return Err(format!("no such project: {id}"));
        }
        self.save()
    }

    /// Record that the user confirmed the current chappa.yml hash for `id`.
    pub fn mark_trusted(&mut self, id: u32, hash: String) -> Result<(), String> {
        let project = self
            .projects
            .iter_mut()
            .find(|p| p.id == id)
            .ok_or_else(|| format!("no such project: {id}"))?;
        project.trusted_hash = Some(hash);
        self.save()
    }

    /// Rename a stored project — the setter behind the switcher's
    /// per-row ✎. Mirrors [`mark_trusted`](Self::mark_trusted): an unknown id
    /// is an `Err`.
    ///
    /// The name is validated BEFORE the lookup and BEFORE any mutation, the
    /// same discipline `set_notification_level` uses: a rejected rename must
    /// leave the store — and the file — untouched. Blank-after-trim is the
    /// only rejection (a project row with an invisible name is unclickable);
    /// the stored value is the TRIMMED string, so " chappa-ai " and "chappa-ai" are the
    /// same rename.
    pub fn rename(&mut self, id: u32, name: String) -> Result<(), String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("project name must not be blank".to_owned());
        }
        let project = self
            .projects
            .iter_mut()
            .find(|p| p.id == id)
            .ok_or_else(|| format!("no such project: {id}"))?;
        project.name = name.to_owned();
        self.save()
    }

    /// The palette keys a project colour may take. The UI paints them; the
    /// store only checks that a stored key is one of these.
    pub const COLORS: [&'static str; 8] = [
        "gray", "blue", "green", "red", "purple", "teal", "orange", "pink",
    ];

    /// Set the badge identity. Blank initials or no colour CLEAR that
    /// override (the badge derives from the name again); initials are at
    /// most three characters and a colour must be a palette key. Validated
    /// before anything mutates, so a rejection leaves the file untouched.
    pub fn set_identity(
        &mut self,
        id: u32,
        initials: Option<String>,
        color: Option<String>,
    ) -> Result<(), String> {
        let initials = initials
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty());
        if let Some(i) = &initials {
            if i.chars().count() > 3 {
                return Err("initials are at most three characters".to_owned());
            }
        }
        let color = color
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty());
        if let Some(c) = &color {
            if !Self::COLORS.contains(&c.as_str()) {
                return Err(format!("unknown project colour: {c}"));
            }
        }
        let project = self.project_mut(id)?;
        project.initials = initials;
        project.color = color;
        self.save()
    }

    /// Set (or clear) a notification level — the persistence half.
    ///
    /// `process = None` targets the PROJECT default; `Some(name)` targets that
    /// process's override. `level = None` CLEARS (removes the override /
    /// resets the project field to `None` = 'all'); `Some(s)` must be one of
    /// [`NOTIFICATION_LEVELS`] or this is an `Err` with nothing persisted.
    ///
    /// Clearing the LAST override drops the whole `process_overrides` key from
    /// the JSON, so a file that never used the feature stays byte-identical to
    /// its pre-form.
    pub fn set_notification_level(
        &mut self,
        id: u32,
        process: Option<&str>,
        level: Option<String>,
    ) -> Result<(), String> {
        // Validate BEFORE mutating: a rejected level must leave the store —
        // and the file — untouched.
        validate_level(level.as_deref())?;
        let project = self
            .projects
            .iter_mut()
            .find(|p| p.id == id)
            .ok_or_else(|| format!("no such project: {id}"))?;
        match process {
            Some(name) => Self::edit_override(project, name, |o| o.notification_level = level),
            None => project.notification_level = level,
        }
        self.save()
    }

    /// `Add to favorites` / its removal. chappa-side only: this
    /// never touches chappa.yml.
    pub fn set_process_favorite(
        &mut self,
        id: u32,
        process: &str,
        favorite: bool,
    ) -> Result<(), String> {
        let project = self.project_mut(id)?;
        Self::edit_override(project, process, |o| o.favorite = favorite);
        self.save()
    }

    /// `Disable automatic renaming`: when set, OSC title events stop
    /// renaming the rail entry. chappa-side only.
    pub fn set_process_auto_rename_disabled(
        &mut self,
        id: u32,
        process: &str,
        disabled: bool,
    ) -> Result<(), String> {
        let project = self.project_mut(id)?;
        Self::edit_override(project, process, |o| o.disable_auto_rename = disabled);
        self.save()
    }

    /// A project-file process rename is a key move, so the
    /// name-keyed override record (favorite, renaming toggle, notification
    /// level) moves with it. A record already under `new` (stale — the name was
    /// reused)
    /// is replaced. No record under `old` = nothing to do, no save.
    pub fn rename_process_override(&mut self, id: u32, old: &str, new: &str) -> Result<(), String> {
        if old == new {
            return Ok(());
        }
        let project = self.project_mut(id)?;
        let Some(map) = project.process_overrides.as_mut() else {
            return Ok(());
        };
        let Some(record) = map.remove(old) else {
            return Ok(());
        };
        map.insert(new.to_owned(), record);
        self.save()
    }

    /// A deleted command's override record goes with it,
    /// so a later command reusing the name starts clean. Prunes the map like
    /// [`Self::edit_override`].
    pub fn remove_process_override(&mut self, id: u32, process: &str) -> Result<(), String> {
        let project = self.project_mut(id)?;
        let Some(map) = project.process_overrides.as_mut() else {
            return Ok(());
        };
        if map.remove(process).is_none() {
            return Ok(());
        }
        if map.is_empty() {
            project.process_overrides = None;
        }
        self.save()
    }

    /// Has the one-time `chappa.yml.chappa-bak` been made for `id`?
    pub fn is_yml_backed_up(&self, id: u32) -> bool {
        self.get(id).and_then(|p| p.yml_backed_up).unwrap_or(false)
    }

    /// Record that the backup exists (never cleared: the backup is the
    /// pre-chappa original, and a refresh would destroy exactly that).
    pub fn mark_yml_backed_up(&mut self, id: u32) -> Result<(), String> {
        self.project_mut(id)?.yml_backed_up = Some(true);
        self.save()
    }

    // ---- the native process store --------------------------------

    /// The store's process definitions for `id`, owned (empty when
    /// the project has never been touched by the native store). The store is
    /// canonical BETWEEN imports, so callers build a mutation draft from this
    /// and commit it via [`Self::set_processes`].
    pub fn process_defs(&self, id: u32) -> IndexMap<String, ProcessDef> {
        self.get(id)
            .and_then(|p| p.processes.clone())
            .unwrap_or_default()
    }

    /// Replace the store's process definitions wholesale and
    /// persist. `Some(map)` even for an empty map — the project HAS been
    /// touched by the native store.
    pub fn set_processes(
        &mut self,
        id: u32,
        processes: IndexMap<String, ProcessDef>,
    ) -> Result<(), String> {
        self.project_mut(id)?.processes = Some(processes);
        self.save()
    }

    fn project_mut(&mut self, id: u32) -> Result<&mut Project, String> {
        self.projects
            .iter_mut()
            .find(|p| p.id == id)
            .ok_or_else(|| format!("no such project: {id}"))
    }

    /// Apply `edit` to `process`'s override record, creating it on demand and
    /// pruning it (and the map) when it ends up carrying nothing — so a
    /// projects.json that never used any override stays byte-identical.
    fn edit_override(
        project: &mut Project,
        process: &str,
        edit: impl FnOnce(&mut ProcessOverride),
    ) {
        let map = project.process_overrides.get_or_insert_with(BTreeMap::new);
        let record = map.entry(process.to_owned()).or_default();
        edit(record);
        if record.is_empty() {
            map.remove(process);
        }
        if map.is_empty() {
            project.process_overrides = None;
        }
    }

    // ---- durable records ----------------------------------------------
    //
    // A command's record state rides its override (above); agent and
    // terminal records are their own lists on the project row. Every method
    // here addresses a record by its id, whatever its kind, so the services
    // above never care which list a record sits in.

    /// The record id of command `process` in project `id`, assigning one on
    /// first use (and persisting it). A command gets its id when it is first
    /// opened or started, never earlier, so a file whose commands were never
    /// touched by this build stays byte-identical.
    pub fn ensure_command_record_id(&mut self, id: u32, process: &str) -> Result<String, String> {
        if let Some(existing) = self.command_record_id(id, process) {
            return Ok(existing);
        }
        let record_id = new_record_id()?;
        let project = self.project_mut(id)?;
        let assigned = record_id.clone();
        Self::edit_override(project, process, |o| o.record_id = Some(assigned));
        self.save()?;
        Ok(record_id)
    }

    /// Assign record ids to every listed command that lacks one, in ONE
    /// save (a project open touches every command at once). Nothing is
    /// written when every command already has its id.
    pub fn ensure_command_record_ids(&mut self, id: u32, processes: &[String]) -> Result<(), String> {
        let missing: Vec<&String> = processes
            .iter()
            .filter(|name| self.command_record_id(id, name).is_none())
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        let ids: Vec<String> = missing.iter().map(|_| new_record_id()).collect::<Result<_, _>>()?;
        let project = self.project_mut(id)?;
        for (name, record_id) in missing.into_iter().zip(ids) {
            Self::edit_override(project, name, |o| o.record_id = Some(record_id));
        }
        self.save()
    }

    /// The record id of command `process`, if one was assigned.
    pub fn command_record_id(&self, id: u32, process: &str) -> Option<String> {
        self.get(id)
            .and_then(|p| p.process_override(process))
            .and_then(|o| o.record_id.clone())
    }

    /// Store a new agent record under project `id`.
    pub fn add_agent_record(&mut self, id: u32, record: AgentRecord) -> Result<(), String> {
        self.project_mut(id)?.agents.push(record);
        self.save()
    }

    /// Store a new terminal record under project `id`.
    pub fn add_terminal_record(&mut self, id: u32, record: TerminalRecord) -> Result<(), String> {
        self.project_mut(id)?.terminals.push(record);
        self.save()
    }

    /// Locate a record by id: the owning project and what kind of record
    /// it is (a command is named by its process key).
    pub fn find_record(&self, record_id: &str) -> Option<(u32, RecordRef)> {
        self.projects.iter().find_map(|p| {
            if let Some((name, _)) = p
                .process_overrides
                .as_ref()
                .and_then(|m| m.iter().find(|(_, o)| o.record_id.as_deref() == Some(record_id)))
            {
                return Some((p.id, RecordRef { kind: RecordKind::Command, name: name.clone() }));
            }
            if let Some(a) = p.agents.iter().find(|a| a.record_id == record_id) {
                return Some((p.id, RecordRef { kind: RecordKind::Agent, name: a.name.clone() }));
            }
            p.terminals
                .iter()
                .find(|t| t.record_id == record_id)
                .map(|t| (p.id, RecordRef { kind: RecordKind::Terminal, name: t.name.clone() }))
        })
    }

    /// One record with its full state, and the project that owns it.
    pub fn record(&self, record_id: &str) -> Option<(u32, StoredRecord)> {
        self.projects
            .iter()
            .find_map(|p| records_of(p).into_iter().find(|r| r.record_id == record_id).map(|r| (p.id, r)))
    }

    /// Every record of project `id`: commands with a record id first (in
    /// override-map order), then agents, then terminals.
    pub fn records_of(&self, id: u32) -> Vec<StoredRecord> {
        self.get(id).map(records_of).unwrap_or_default()
    }

    /// Every record in the store, tagged with its project.
    pub fn all_records(&self) -> Vec<(u32, StoredRecord)> {
        self.projects
            .iter()
            .flat_map(|p| records_of(p).into_iter().map(move |r| (p.id, r)))
            .collect()
    }

    /// Start a new run on a record: `last_run` becomes `run` whatever it held
    /// before (the ledger keeps the previous run). `Ok(false)` when no record
    /// has that id.
    pub fn begin_run(&mut self, record_id: &str, run: Run) -> Result<bool, String> {
        let Some(slot) = self.run_slot(record_id) else {
            return Ok(false);
        };
        *slot = Some(run);
        self.save()?;
        Ok(true)
    }

    /// Apply a late fact to the record's CURRENT run, under the run-uuid
    /// rule (`records::update_run`). Saves only when something applied.
    pub fn update_run(
        &mut self,
        record_id: &str,
        run_uuid: &str,
        edit: impl FnOnce(&mut Run),
    ) -> Result<RunUpdate, String> {
        let Some(slot) = self.run_slot(record_id) else {
            return Ok(RunUpdate::NotFound);
        };
        let outcome = update_run(slot.as_mut(), run_uuid, edit);
        if outcome == RunUpdate::Applied {
            self.save()?;
        }
        Ok(outcome)
    }

    /// Delete an agent or terminal record. A command record is its
    /// definition: deleting the command (`remove_process_override` on the
    /// way) is how a command record goes, so a command id answers `None`
    /// here and nothing changes.
    pub fn remove_record(&mut self, record_id: &str) -> Result<Option<(u32, RecordKind)>, String> {
        let removed = self.projects.iter_mut().find_map(|p| {
            let agents = p.agents.len();
            p.agents.retain(|a| a.record_id != record_id);
            if p.agents.len() != agents {
                return Some((p.id, RecordKind::Agent));
            }
            let terminals = p.terminals.len();
            p.terminals.retain(|t| t.record_id != record_id);
            (p.terminals.len() != terminals).then_some((p.id, RecordKind::Terminal))
        });
        if removed.is_some() {
            self.save()?;
        }
        Ok(removed)
    }

    /// Edit an agent record's row state (name, favorite, automatic
    /// renaming, notification level). `Ok(None)` when no project holds an
    /// agent record with that id; a refused patch persists nothing.
    pub fn update_agent_record(
        &mut self,
        record_id: &str,
        patch: &AgentRecordPatch,
    ) -> Result<Option<(u32, AgentRecord)>, String> {
        patch.validate()?;
        let updated = self.projects.iter_mut().find_map(|p| {
            let id = p.id;
            p.agents.iter_mut().find(|a| a.record_id == record_id).map(|a| {
                patch.apply(a);
                (id, a.clone())
            })
        });
        if updated.is_some() {
            self.save()?;
        }
        Ok(updated)
    }

    /// Every record id project `id` owns — what a project removal prunes
    /// from the ledger.
    pub fn record_ids_of(&self, id: u32) -> Vec<String> {
        self.records_of(id).into_iter().map(|r| r.record_id).collect()
    }

    /// At launch: every run still `running` was cut off by a crash or a hard
    /// kill. Reconcile them (see `Run::reconcile_crash`) and persist; returns
    /// the run uuids that changed so the ledger can follow.
    pub fn reconcile_crashed_runs(&mut self) -> Result<Vec<String>, String> {
        let changed = reconcile_runs(self.projects.iter_mut().flat_map(runs_of_mut));
        if !changed.is_empty() {
            self.save()?;
        }
        Ok(changed)
    }

    /// The `last_run` slot of the record `record_id`, whatever its kind.
    fn run_slot(&mut self, record_id: &str) -> Option<&mut Option<Run>> {
        self.projects.iter_mut().find_map(|p| run_slot_in(p, record_id))
    }

    /// True when `hash` is the trusted hash on record for `id` — i.e. the
    /// current chappa.yml was already confirmed, so auto-start needs no gate.
    pub fn is_trusted(&self, id: u32, hash: &str) -> bool {
        self.projects
            .iter()
            .find(|p| p.id == id)
            .and_then(|p| p.trusted_hash.as_deref())
            == Some(hash)
    }

    /// Locate a project by its canonical directory path (dedupe helper).
    fn find_by_path(&self, path: &Path) -> Option<&Project> {
        let canon = path.canonicalize().ok();
        self.projects.iter().find(|p| {
            if let (Some(a), Some(b)) = (canon.as_deref(), p.path.canonicalize().ok().as_deref()) {
                a == b
            } else {
                p.path == path
            }
        })
    }

    fn save(&self) -> Result<(), String> {
        crate::atomic::write_json_atomic(&self.path, &self.projects)
    }
}

/// The records of one project row (see [`ProjectStore::records_of`]).
fn records_of(p: &Project) -> Vec<StoredRecord> {
    let commands = p
        .process_overrides
        .iter()
        .flat_map(|m| m.iter())
        .filter_map(|(name, o)| {
            o.record_id.as_ref().map(|record_id| StoredRecord {
                record_id: record_id.clone(),
                kind: RecordKind::Command,
                name: name.clone(),
                last_run: o.last_run.clone(),
                agent: None,
                terminal: None,
            })
        });
    let agents = p.agents.iter().map(|a| StoredRecord {
        record_id: a.record_id.clone(),
        kind: RecordKind::Agent,
        name: a.name.clone(),
        last_run: a.last_run.clone(),
        agent: Some(a.clone()),
        terminal: None,
    });
    let terminals = p.terminals.iter().map(|t| StoredRecord {
        record_id: t.record_id.clone(),
        kind: RecordKind::Terminal,
        name: t.name.clone(),
        last_run: t.last_run.clone(),
        agent: None,
        terminal: Some(t.clone()),
    });
    commands.chain(agents).chain(terminals).collect()
}

/// The `last_run` slot of `record_id` inside one project row.
fn run_slot_in<'a>(p: &'a mut Project, record_id: &str) -> Option<&'a mut Option<Run>> {
    if let Some(o) = p
        .process_overrides
        .as_mut()
        .and_then(|m| m.values_mut().find(|o| o.record_id.as_deref() == Some(record_id)))
    {
        return Some(&mut o.last_run);
    }
    if let Some(a) = p.agents.iter_mut().find(|a| a.record_id == record_id) {
        return Some(&mut a.last_run);
    }
    p.terminals
        .iter_mut()
        .find(|t| t.record_id == record_id)
        .map(|t| &mut t.last_run)
}

/// Every stored run of one project row, mutably (the crash reconcile).
fn runs_of_mut(p: &mut Project) -> impl Iterator<Item = &mut Run> {
    let commands = p
        .process_overrides
        .iter_mut()
        .flat_map(|m| m.values_mut())
        .filter_map(|o| o.last_run.as_mut());
    let agents = p.agents.iter_mut().filter_map(|a| a.last_run.as_mut());
    let terminals = p.terminals.iter_mut().filter_map(|t| t.last_run.as_mut());
    commands.chain(agents).chain(terminals)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::records::TailKind;

    // ---- badge identity ----------------------------------------------

    /// Initials/colour persist as overrides, blanks clear them, and a bad
    /// value is refused before the file changes.
    #[test]
    fn identity_overrides_round_trip_and_validate() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("projects.json");
        fs::write(
            &store_path,
            r#"[{"id":1,"name":"chappa-ai","path":"/chappa-ai"}]"#,
        )
        .unwrap();
        let mut store = ProjectStore::new(store_path.clone());
        // Untouched rows carry neither key.
        assert!(store.get(1).unwrap().initials.is_none());
        assert!(store.get(1).unwrap().color.is_none());
        assert!(!fs::read_to_string(&store_path).unwrap().contains("initials"));

        store.set_identity(1, Some(" CH ".into()), Some("Teal".into())).unwrap();
        let reopened = ProjectStore::new(store_path.clone());
        assert_eq!(reopened.get(1).unwrap().initials.as_deref(), Some("CH"));
        assert_eq!(reopened.get(1).unwrap().color.as_deref(), Some("teal"));

        // Too long / unknown colour: refused, nothing written.
        assert!(store.set_identity(1, Some("FOUR".into()), None).is_err());
        assert!(store.set_identity(1, None, Some("plaid".into())).is_err());
        assert_eq!(store.get(1).unwrap().initials.as_deref(), Some("CH"));
        assert_eq!(store.get(1).unwrap().color.as_deref(), Some("teal"));
        assert!(store.set_identity(9, None, None).is_err());

        // Blank clears: the row goes back to derived and the keys vanish.
        store.set_identity(1, Some("".into()), None).unwrap();
        assert!(store.get(1).unwrap().initials.is_none());
        assert!(store.get(1).unwrap().color.is_none());
        assert!(!fs::read_to_string(&store_path).unwrap().contains("color"));
    }

    // ---- override records, favorites, renaming, backup flag -------

    /// A projects.json (bare level strings) loads into the record
    /// shape, and the next save rewrites it as records without losing the
    /// level.
    #[test]
    fn legacy_bare_level_overrides_still_load() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("projects.json");
        fs::write(
            &store_path,
            r#"[{"id":1,"name":"chappa-ai","path":"/chappa-ai","process_overrides":{"server":"none","watch build":"all"}}]"#,
        )
        .unwrap();
        let mut store = ProjectStore::new(store_path.clone());
        let p = store.get(1).unwrap();
        assert_eq!(p.process_notification_level("server"), Some("none"));
        assert_eq!(p.process_notification_level("watch build"), Some("all"));
        assert!(!p.process_override("server").unwrap().favorite);

        store.set_process_favorite(1, "server", true).unwrap();
        let reopened = ProjectStore::new(store_path);
        let o = reopened.get(1).unwrap().process_override("server").unwrap();
        assert_eq!(o.notification_level.as_deref(), Some("none"));
        assert!(o.favorite);
        assert_eq!(
            reopened
                .get(1)
                .unwrap()
                .process_notification_level("watch build"),
            Some("all")
        );
    }

    /// Favorite + renaming toggles round-trip, coexist with a level on the
    /// same record, and prune: clearing the LAST field of the LAST record
    /// removes the `process_overrides` key entirely (the discipline).
    #[test]
    fn favorite_and_renaming_toggles_round_trip_and_prune() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("projects.json");
        let mut store = ProjectStore::new(store_path.clone());
        store
            .add("/chappa-ai".into(), "chappa-ai".into(), None)
            .unwrap();

        store.set_process_favorite(1, "server", true).unwrap();
        store
            .set_process_auto_rename_disabled(1, "server", true)
            .unwrap();
        store
            .set_notification_level(1, Some("server"), Some("none".into()))
            .unwrap();
        let p = ProjectStore::new(store_path.clone())
            .get(1)
            .cloned()
            .unwrap();
        let o = p.process_override("server").unwrap();
        assert!(o.favorite && o.disable_auto_rename);
        assert_eq!(o.notification_level.as_deref(), Some("none"));
        let text = fs::read_to_string(&store_path).unwrap();
        assert!(text.contains("\"favorite\": true"), "{text}");
        assert!(text.contains("\"disable_auto_rename\": true"), "{text}");

        // Clear one field at a time; the record survives until it is empty.
        store.set_process_favorite(1, "server", false).unwrap();
        assert!(!fs::read_to_string(&store_path)
            .unwrap()
            .contains("favorite"));
        assert!(store.get(1).unwrap().process_override("server").is_some());
        store
            .set_notification_level(1, Some("server"), None)
            .unwrap();
        assert!(store.get(1).unwrap().process_override("server").is_some());
        store
            .set_process_auto_rename_disabled(1, "server", false)
            .unwrap();
        assert_eq!(store.get(1).unwrap().process_overrides, None);
        assert!(!fs::read_to_string(&store_path)
            .unwrap()
            .contains("process_overrides"));

        // Toggling OFF something never set is a no-op, not a phantom record.
        store.set_process_favorite(1, "ghost", false).unwrap();
        assert_eq!(store.get(1).unwrap().process_overrides, None);
        assert!(store.set_process_favorite(99, "x", true).is_err());
    }

    /// The backup flag: false until marked, sticky, per project, and absent
    /// from an untouched row's JSON.
    #[test]
    fn rename_moves_the_override_record_and_delete_prunes_it() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("projects.json");
        fs::write(
            &store_path,
            r#"[{"id":1,"name":"chappa-ai","path":"/chappa-ai","process_overrides":{"server":{"favorite":true,"notification_level":"none"}}}]"#,
        )
        .unwrap();
        let mut store = ProjectStore::new(store_path.clone());

        store.rename_process_override(1, "server", "api").unwrap();
        assert!(store.get(1).unwrap().process_override("server").is_none());
        let moved = store.get(1).unwrap().process_override("api").unwrap();
        assert!(moved.favorite);
        assert_eq!(moved.notification_level.as_deref(), Some("none"));
        // A later command reusing the old name starts clean.
        assert!(ProjectStore::new(store_path.clone())
            .get(1)
            .unwrap()
            .process_override("server")
            .is_none());
        // Unknown old name: a no-op, nothing lost.
        store.rename_process_override(1, "ghost", "api").unwrap();
        assert!(
            store
                .get(1)
                .unwrap()
                .process_override("api")
                .unwrap()
                .favorite
        );

        store.remove_process_override(1, "api").unwrap();
        assert_eq!(store.get(1).unwrap().process_overrides, None);
        assert!(!fs::read_to_string(&store_path)
            .unwrap()
            .contains("process_overrides"));
    }

    #[test]
    fn yml_backed_up_flag_is_sticky_and_per_project() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("projects.json");
        let mut store = ProjectStore::new(store_path.clone());
        store.add("/a".into(), "a".into(), None).unwrap();
        store.add("/b".into(), "b".into(), None).unwrap();
        assert!(!store.is_yml_backed_up(1));
        assert!(!fs::read_to_string(&store_path)
            .unwrap()
            .contains("yml_backed_up"));

        store.mark_yml_backed_up(1).unwrap();
        assert!(store.is_yml_backed_up(1));
        assert!(!store.is_yml_backed_up(2), "per project");
        assert!(ProjectStore::new(store_path).is_yml_backed_up(1));
        assert!(store.mark_yml_backed_up(99).is_err());
    }

    #[test]
    fn crud_roundtrip_through_a_tempdir_config_path() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("appconfig").join("projects.json");

        let mut store = ProjectStore::new(store_path.clone());
        assert!(store.list().is_empty(), "missing file → empty store");

        let p = store
            .add("/projects/chappa-ai".into(), "chappa-ai".into(), None)
            .unwrap();
        assert_eq!(p.id, 1);
        let q = store
            .add("/projects/other".into(), "other".into(), Some("OT".into()))
            .unwrap();
        assert_eq!(q.id, 2);

        // A fresh store over the same path sees the persisted rows.
        let mut reopened = ProjectStore::new(store_path.clone());
        assert_eq!(reopened.list().len(), 2);
        assert_eq!(reopened.get(1).unwrap().name, "chappa-ai");
        assert_eq!(reopened.get(2).unwrap().icon.as_deref(), Some("OT"));

        reopened.remove(1).unwrap();
        assert_eq!(reopened.list().len(), 1);
        // Removing again errors (idempotency is for stop/restart, not CRUD).
        assert!(reopened.remove(1).is_err());
        // The store persisted the removal.
        assert_eq!(ProjectStore::new(store_path.clone()).list().len(), 1);
    }

    #[test]
    fn add_is_idempotent_by_path() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("projects.json");
        let mut store = ProjectStore::new(store_path.clone());
        let first = store.add("/x".into(), "one".into(), None).unwrap();
        let second = store.add("/x".into(), "one".into(), None).unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(store.list().len(), 1);
    }

    #[test]
    fn trust_gate_flows() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("projects.json");
        let mut store = ProjectStore::new(store_path.clone());
        let p = store
            .add("/chappa-ai".into(), "chappa-ai".into(), None)
            .unwrap();

        // Untrusted hash → not trusted → gate arms.
        assert!(!store.is_trusted(p.id, "hash-a"));
        store.mark_trusted(p.id, "hash-a".into()).unwrap();
        assert!(store.is_trusted(p.id, "hash-a"));
        // A different hash (edited yml) → gate re-arms.
        assert!(!store.is_trusted(p.id, "hash-b"));

        // Trust persists across reload.
        let reopened = ProjectStore::new(store_path.clone());
        assert!(reopened.is_trusted(p.id, "hash-a"));
    }

    #[test]
    fn dto_shape_excludes_trust() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = ProjectStore::new(dir.path().join("projects.json"));
        let p = store
            .add("/chappa-ai".into(), "chappa-ai".into(), Some("DU".into()))
            .unwrap();
        let dto = p.dto();
        let json = serde_json::to_value(&dto).unwrap();
        assert_eq!(json["id"], 1);
        assert_eq!(json["name"], "chappa-ai");
        assert_eq!(json["path"], "/chappa-ai");
        assert_eq!(json["icon"], "DU");
        assert!(json.get("trusted_hash").is_none());
        // The notification fields ride the src-tauri DTOs, NOT this
        // one — the project-model wire DTO stays exactly `{id, name, path, icon}`.
        assert!(json.get("notification_level").is_none());
        assert!(json.get("notificationLevel").is_none());
        assert!(json.get("process_overrides").is_none());
        assert!(json.get("processOverrides").is_none());
        assert_eq!(json.as_object().unwrap().len(), 4);
    }

    // ---- rename ----------------------------------------------------

    /// Round-trip: a rename persists, survives a reopen, and touches NOTHING
    /// else on the row (path, icon and the trusted hash are not collateral).
    #[test]
    fn rename_round_trips_and_leaves_the_rest_of_the_row_alone() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("projects.json");
        let mut store = ProjectStore::new(store_path.clone());
        store
            .add("/p/chappa-ai".into(), "chappa-ai".into(), Some("DU".into()))
            .unwrap();
        store.mark_trusted(1, "hash-a".into()).unwrap();

        store.rename(1, "chappa-ai (notes)".into()).unwrap();
        assert_eq!(store.get(1).unwrap().name, "chappa-ai (notes)");

        let reopened = ProjectStore::new(store_path.clone());
        let p = reopened.get(1).unwrap();
        assert_eq!(p.name, "chappa-ai (notes)");
        assert_eq!(p.path, PathBuf::from("/p/chappa-ai"));
        assert_eq!(p.icon.as_deref(), Some("DU"));
        assert!(
            reopened.is_trusted(1, "hash-a"),
            "rename must not clear trust"
        );
    }

    /// Boundary regime: unknown id errors, and every blank-after-trim name is
    /// refused with NOTHING persisted (the file is byte-identical afterwards).
    /// Surrounding whitespace on a real name is trimmed, not rejected.
    #[test]
    fn rename_rejects_unknown_ids_and_blank_names_without_persisting() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("projects.json");
        let mut store = ProjectStore::new(store_path.clone());
        store
            .add("/p/chappa-ai".into(), "chappa-ai".into(), None)
            .unwrap();
        let before = fs::read_to_string(&store_path).unwrap();

        assert!(store.rename(99, "ghost".into()).is_err(), "unknown id");
        // Blank regimes: empty, spaces, a tab, a newline — all printable
        // escapes in source, never a literal control byte.
        for blank in ["", " ", "   ", "\t", "\n", " \t\n "] {
            assert!(
                store.rename(1, blank.to_owned()).is_err(),
                "accepted blank name {blank:?}"
            );
        }
        assert_eq!(fs::read_to_string(&store_path).unwrap(), before);
        assert_eq!(store.get(1).unwrap().name, "chappa-ai");

        // A name with surrounding whitespace is TRIMMED, not refused.
        store.rename(1, "  chappa-ai two  ".into()).unwrap();
        assert_eq!(store.get(1).unwrap().name, "chappa-ai two");
        assert_eq!(
            ProjectStore::new(store_path).get(1).unwrap().name,
            "chappa-ai two"
        );
    }

    // ---- notification levels --------------------------------------

    /// A projects.json written by an earlier build (no `notification_level`, no
    /// `process_overrides`) loads unchanged, reads as "unset", and — crucially
    /// — re-serializes byte-identical when nothing touched the new fields.
    #[test]
    fn old_format_projects_json_loads_unchanged_and_rewrites_identically() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("projects.json");
        let old =
            r#"[{"id":1,"name":"chappa-ai","path":"/chappa-ai","icon":"DU","trusted_hash":"h1"}]"#;
        fs::write(&store_path, old).unwrap();

        let mut store = ProjectStore::new(store_path.clone());
        let p = store.get(1).unwrap();
        assert_eq!(p.name, "chappa-ai");
        assert_eq!(p.trusted_hash.as_deref(), Some("h1"));
        assert_eq!(p.notification_level, None, "unset → None (= 'all')");
        assert_eq!(p.process_overrides, None);

        // A save that never touched the new fields emits the SAME keys. (The
        // atomic writer pretty-prints, so compare parsed values, not bytes —
        // the point is that no key appeared or vanished.)
        store.mark_trusted(1, "h1".into()).unwrap();
        let text = fs::read_to_string(&store_path).unwrap();
        assert!(
            !text.contains("notification_level"),
            "untouched file must not grow the new keys: {text}"
        );
        assert!(!text.contains("process_overrides"), "{text}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&text).unwrap(),
            serde_json::from_str::<serde_json::Value>(old).unwrap()
        );
    }

    /// Round-trip: a project default plus two per-process overrides survive a
    /// close/reopen of the store.
    #[test]
    fn notification_levels_round_trip_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("projects.json");
        let mut store = ProjectStore::new(store_path.clone());
        store
            .add("/chappa-ai".into(), "chappa-ai".into(), None)
            .unwrap();

        store
            .set_notification_level(1, None, Some("important".into()))
            .unwrap();
        store
            .set_notification_level(1, Some("server"), Some("none".into()))
            .unwrap();
        store
            .set_notification_level(1, Some("watch build"), Some("all".into()))
            .unwrap();

        let reopened = ProjectStore::new(store_path.clone());
        let p = reopened.get(1).unwrap();
        assert_eq!(p.notification_level.as_deref(), Some("important"));
        assert_eq!(p.process_notification_level("server"), Some("none"));
        // Process names carry spaces (project-file keys) — map keys, not idents.
        assert_eq!(p.process_notification_level("watch build"), Some("all"));
        assert_eq!(p.process_notification_level("absent"), None);
        assert_eq!(p.process_overrides.as_ref().unwrap().len(), 2);
    }

    /// Clear semantics, boundary regime: clearing the project default resets
    /// it to `None`, and clearing the LAST override removes the whole
    /// `process_overrides` key from the serialized file (not an empty `{}`).
    #[test]
    fn clearing_the_last_override_removes_the_map_key_entirely() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("projects.json");
        let mut store = ProjectStore::new(store_path.clone());
        store
            .add("/chappa-ai".into(), "chappa-ai".into(), None)
            .unwrap();

        store
            .set_notification_level(1, None, Some("none".into()))
            .unwrap();
        store
            .set_notification_level(1, Some("a"), Some("none".into()))
            .unwrap();
        store
            .set_notification_level(1, Some("b"), Some("all".into()))
            .unwrap();
        assert!(fs::read_to_string(&store_path)
            .unwrap()
            .contains("process_overrides"));

        // One of two cleared → the map survives with the other entry.
        store.set_notification_level(1, Some("a"), None).unwrap();
        let text = fs::read_to_string(&store_path).unwrap();
        assert!(text.contains("process_overrides"), "{text}");
        assert!(!text.contains("\"a\""), "{text}");

        // The last one cleared → the key is gone from the JSON entirely.
        store.set_notification_level(1, Some("b"), None).unwrap();
        let text = fs::read_to_string(&store_path).unwrap();
        assert!(!text.contains("process_overrides"), "{text}");
        assert_eq!(store.get(1).unwrap().process_overrides, None);

        // Clearing a process that has NO override (and with no map at all) is
        // a no-op success, not an error.
        store
            .set_notification_level(1, Some("ghost"), None)
            .unwrap();
        assert_eq!(store.get(1).unwrap().process_overrides, None);

        // Clearing the project default drops its key too.
        store.set_notification_level(1, None, None).unwrap();
        let text = fs::read_to_string(&store_path).unwrap();
        assert!(!text.contains("notification_level"), "{text}");
        assert_eq!(
            ProjectStore::new(store_path)
                .get(1)
                .unwrap()
                .notification_level,
            None
        );
    }

    // ---- durable records ------------------------------------------

    /// A command's record id is assigned once, rides its override record,
    /// moves with a rename and goes with the delete; `last_run` persists
    /// through a reopen and a sparse `last_run` loads with defaults.
    #[test]
    fn command_records_ride_the_override_and_survive_rename() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("projects.json");
        let mut store = ProjectStore::new(store_path.clone());
        store.add("/code/app".into(), "app".into(), None).unwrap();
        assert_eq!(store.command_record_id(1, "server"), None);

        let id = store.ensure_command_record_id(1, "server").unwrap();
        assert_eq!(id.len(), 32);
        assert_eq!(store.ensure_command_record_id(1, "server").unwrap(), id);
        assert!(store.ensure_command_record_id(9, "server").is_err());
        assert_eq!(
            store.find_record(&id),
            Some((1, RecordRef { kind: RecordKind::Command, name: "server".into() }))
        );
        // An override that only carries a record id is not "empty": it stays.
        assert!(store.get(1).unwrap().process_override("server").is_some());

        assert!(store.begin_run(&id, Run::begin("run-1", 100, Default::default())).unwrap());
        assert_eq!(
            store.update_run(&id, "run-1", |r| r.finish(200, Some(2), false, vec![serde_json::json!("boom")])).unwrap(),
            RunUpdate::Applied
        );
        let reopened = ProjectStore::new(store_path.clone());
        let run = reopened.get(1).unwrap().process_override("server").unwrap().last_run.clone().unwrap();
        assert_eq!(run.status, term_core::status::ProcessStatus::Failed);
        assert_eq!(run.tail, vec![serde_json::json!("boom")]);
        assert_eq!(reopened.records_of(1)[0].record_id, id);

        store.rename_process_override(1, "server", "api").unwrap();
        assert_eq!(store.command_record_id(1, "api").as_deref(), Some(id.as_str()));
        assert_eq!(store.record_ids_of(1), vec![id.clone()]);
        assert_eq!(store.remove_record(&id).unwrap(), None, "a command record is its definition");
        store.remove_process_override(1, "api").unwrap();
        assert_eq!(store.find_record(&id), None);
        assert_eq!(store.get(1).unwrap().process_overrides, None);

        // A sparse last_run from an older writer loads, never errors.
        fs::write(
            &store_path,
            r#"[{"id":1,"name":"app","path":"/code/app","process_overrides":{"x":{"record_id":"abc","last_run":{"run_uuid":"r","status":"stopped","started_at_ms":1}}}}]"#,
        )
        .unwrap();
        let sparse = ProjectStore::new(store_path);
        let run = sparse.get(1).unwrap().process_override("x").unwrap().last_run.clone().unwrap();
        assert_eq!(run.ended_at_ms, None);
        assert!(run.tail.is_empty());
    }

    /// Agent and terminal records live under their project, round-trip
    /// through the file, follow the run-uuid rule, reconcile a crash at load
    /// and are removed with their project row's bookkeeping intact.
    #[test]
    fn agent_and_terminal_records_live_under_their_project() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("projects.json");
        let mut store = ProjectStore::new(store_path.clone());
        store.add("/code/a".into(), "a".into(), None).unwrap();
        store.add("/code/b".into(), "b".into(), None).unwrap();
        let before = fs::read_to_string(&store_path).unwrap();
        assert!(!before.contains("agents") && !before.contains("terminals"));

        store
            .add_agent_record(
                2,
                AgentRecord {
                    record_id: "agent-1".into(),
                    name: "worker".into(),
                    tool_id: 4,
                    favorite: true,
                    disable_auto_rename: false,
                    notification_level: Some("none".into()),
                    parent_record_id: None,
                    extra_args: vec![],
                    prompt: Some("do the thing".into()),
                    last_run: Some(Run::begin("run-a", 5, TailKind::Events)),
                },
            )
            .unwrap();
        store
            .add_terminal_record(
                1,
                TerminalRecord {
                    record_id: "term-1".into(),
                    name: "pwsh".into(),
                    shell_profile: Some("PowerShell".into()),
                    cwd: Some("/code/a".into()),
                    last_run: None,
                },
            )
            .unwrap();
        assert!(store.add_agent_record(9, AgentRecord {
            record_id: "x".into(), name: "x".into(), tool_id: 1, favorite: false,
            disable_auto_rename: false, notification_level: None, parent_record_id: None,
            extra_args: vec![], prompt: None, last_run: None,
        }).is_err());

        let reopened = ProjectStore::new(store_path.clone());
        let (owner, agent) = reopened.record("agent-1").unwrap();
        assert_eq!((owner, agent.kind), (2, RecordKind::Agent));
        assert_eq!(agent.agent.as_ref().unwrap().prompt.as_deref(), Some("do the thing"));
        assert_eq!(agent.last_run.as_ref().unwrap().tail_kind, TailKind::Events);
        let (owner, terminal) = reopened.record("term-1").unwrap();
        assert_eq!((owner, terminal.kind), (1, RecordKind::Terminal));
        assert_eq!(terminal.terminal.unwrap().shell_profile.as_deref(), Some("PowerShell"));
        assert_eq!(reopened.all_records().len(), 2);
        assert_eq!(reopened.record_ids_of(2), vec!["agent-1".to_owned()]);

        // Late facts: stale uuid dropped, current applied; a crash reconcile
        // on a fresh load closes the still-open run.
        assert_eq!(store.update_run("agent-1", "run-z", |r| r.stop(6)).unwrap(), RunUpdate::Stale);
        let mut crashed = ProjectStore::new(store_path.clone());
        assert_eq!(crashed.reconcile_crashed_runs().unwrap(), vec!["run-a".to_owned()]);
        let run = crashed.record("agent-1").unwrap().1.last_run.unwrap();
        assert_eq!(run.status, term_core::status::ProcessStatus::Exited);
        assert_eq!((run.exit_code, run.success), (None, None));
        // After the reconcile the run is closed: a late exit for it is stale.
        assert_eq!(
            crashed.update_run("agent-1", "run-a", |r| r.finish(7, Some(0), true, vec![])).unwrap(),
            RunUpdate::Stale
        );
        // A new run replaces the head.
        assert!(crashed.begin_run("agent-1", Run::begin("run-b", 8, TailKind::Events)).unwrap());
        assert_eq!(crashed.record("agent-1").unwrap().1.last_run.unwrap().run_uuid, "run-b");

        assert_eq!(crashed.remove_record("agent-1").unwrap(), Some((2, RecordKind::Agent)));
        assert_eq!(crashed.remove_record("term-1").unwrap(), Some((1, RecordKind::Terminal)));
        assert_eq!(crashed.remove_record("term-1").unwrap(), None);
        let after = fs::read_to_string(&store_path).unwrap();
        assert!(!after.contains("agents") && !after.contains("terminals"), "{after}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&after).unwrap(),
            serde_json::from_str::<serde_json::Value>(&before).unwrap(),
            "the row is back to what it was before any record"
        );
    }

    /// An agent record's row state is editable in place: the patch lands on
    /// the right project's agent, persists, leaves the rest alone, and a
    /// refused patch (blank name, unknown level) or an unknown id writes
    /// nothing.
    #[test]
    fn an_agent_record_takes_a_row_state_patch() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("projects.json");
        let mut store = ProjectStore::new(store_path.clone());
        store.add("/code/a".into(), "a".into(), None).unwrap();
        store
            .add_agent_record(
                1,
                AgentRecord {
                    record_id: "agent-1".into(),
                    name: "claude".into(),
                    tool_id: 3,
                    favorite: false,
                    disable_auto_rename: false,
                    notification_level: None,
                    parent_record_id: None,
                    extra_args: vec!["--verbose".into()],
                    prompt: None,
                    last_run: None,
                },
            )
            .unwrap();

        let patch = AgentRecordPatch {
            name: Some(" reviewer ".into()),
            favorite: Some(true),
            disable_auto_rename: Some(true),
            notification_level: Some(Some("important".into())),
        };
        let (owner, agent) = store.update_agent_record("agent-1", &patch).unwrap().unwrap();
        assert_eq!(owner, 1);
        assert_eq!(agent.name, "reviewer");
        assert!(agent.favorite && agent.disable_auto_rename);
        assert_eq!(agent.notification_level.as_deref(), Some("important"));
        assert_eq!(agent.extra_args, vec!["--verbose".to_owned()], "untouched fields stay");

        let reopened = ProjectStore::new(store_path.clone());
        let stored = reopened.record("agent-1").unwrap().1;
        assert_eq!(stored.name, "reviewer");
        assert_eq!(stored.agent.unwrap().notification_level.as_deref(), Some("important"));

        let before = fs::read_to_string(&store_path).unwrap();
        let blank = AgentRecordPatch { name: Some("".into()), ..Default::default() };
        assert!(store.update_agent_record("agent-1", &blank).is_err());
        let loud = AgentRecordPatch { notification_level: Some(Some("All".into())), ..Default::default() };
        assert!(store.update_agent_record("agent-1", &loud).is_err());
        assert_eq!(store.update_agent_record("nope", &patch).unwrap(), None);
        assert_eq!(fs::read_to_string(&store_path).unwrap(), before, "nothing written");

        // Clearing the level is a null on the wire, and drops the key.
        let clear = AgentRecordPatch { notification_level: Some(None), ..Default::default() };
        let (_, agent) = store.update_agent_record("agent-1", &clear).unwrap().unwrap();
        assert_eq!(agent.notification_level, None);
        assert!(!fs::read_to_string(&store_path).unwrap().contains("notification_level"));
    }

    /// Every vocabulary member is accepted; everything else is rejected and
    /// NOTHING is persisted — including case variants of the valid words
    /// (the wire vocabulary is lowercase; "All" is the UI label).
    #[test]
    fn level_vocabulary_is_exactly_all_important_none() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("projects.json");
        let mut store = ProjectStore::new(store_path.clone());
        store
            .add("/chappa-ai".into(), "chappa-ai".into(), None)
            .unwrap();

        for good in NOTIFICATION_LEVELS {
            store
                .set_notification_level(1, None, Some(good.into()))
                .unwrap();
            assert_eq!(
                store.get(1).unwrap().notification_level.as_deref(),
                Some(good)
            );
        }
        // Park a known-good state, then prove every bad input leaves it alone.
        store
            .set_notification_level(1, None, Some("important".into()))
            .unwrap();
        store
            .set_notification_level(1, Some("server"), Some("none".into()))
            .unwrap();
        let before = fs::read_to_string(&store_path).unwrap();

        for bad in ["All", "IMPORTANT", "None", "silent", "", " all", "all "] {
            assert!(
                store
                    .set_notification_level(1, None, Some(bad.into()))
                    .is_err(),
                "project default accepted {bad:?}"
            );
            assert!(
                store
                    .set_notification_level(1, Some("server"), Some(bad.into()))
                    .is_err(),
                "process override accepted {bad:?}"
            );
        }
        assert_eq!(fs::read_to_string(&store_path).unwrap(), before);
        assert_eq!(
            store.get(1).unwrap().notification_level.as_deref(),
            Some("important")
        );
        assert_eq!(
            store.get(1).unwrap().process_notification_level("server"),
            Some("none")
        );

        // An unknown project id errors regardless of level validity.
        assert!(store
            .set_notification_level(99, None, Some("all".into()))
            .is_err());
        assert!(store.set_notification_level(99, None, None).is_err());
    }
}
