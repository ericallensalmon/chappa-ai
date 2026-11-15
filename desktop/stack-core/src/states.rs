//! Saved stack states: what a work item's stack was, so it can come back.
//!
//! A state is a small JSON file, `<work>/<KEY>/states/<UTC>.json`
//! (`20261114T120000Z.json`), describing everything needed to bring the
//! item's stack back: the bound item and slot; per component its mode, ref,
//! commit, dirty-diff hash, image (a prebuilt digest or a source content
//! key), build id and whether it was wanted running; the per-instance
//! settings the user chose; and, per component that keeps data, *how* to
//! get the data back. Never the data itself, and never a sampled value:
//!
//! - **recipe** (the default): the base image digest, each script the
//!   pipeline runs with its commit and hash, and the state seeds chosen
//!   with their parameters. Resume recreates the container from the image
//!   and replays the pipeline.
//! - **snapshot**: a [`DataSnapshot`] reference, a name and where the
//!   artifact lives (in the component's volume, or copied out to the
//!   host). The artifact stays with the database; the state only points
//!   at it.
//!
//! A state is `sensitive` when a component that keeps data had a last
//! data-safety verdict that was not clean, or has no check at all
//! (unknown counts as sensitive).
//!
//! This module holds the pure parts: the format, listing, the pruning
//! rule (which states and which snapshots go, never one another kept state
//! still points at), the job ids states refer to (so job retention keeps
//! them), the snapshots living in given volumes, and the differences
//! between a state and what runs now.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::compose::parse_utc;
use crate::definition::{DataStrategy, Mode, SnapshotLocation};
use crate::intent::Desired;
use crate::jobs::write_atomic;
use crate::planner::{short_digest, ImagePin, InstanceState};
use crate::workspace::{c8, ARCHIVE_DIR};

/// The folder of an item's states.
pub const STATES_DIR: &str = "states";
/// Under the states folder: snapshots copied out to the host by default.
pub const HOST_DATA_DIR: &str = "data";
/// The state file format this version writes.
pub const STATE_FORMAT: u32 = 1;

/// Why a state was saved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SaveReason {
    Manual,
    BeforeModeSwitch,
    BeforeRebind,
    BeforeDown,
    BeforeArchive,
}

impl SaveReason {
    pub fn as_str(self) -> &'static str {
        match self {
            SaveReason::Manual => "manual",
            SaveReason::BeforeModeSwitch => "before-mode-switch",
            SaveReason::BeforeRebind => "before-rebind",
            SaveReason::BeforeDown => "before-down",
            SaveReason::BeforeArchive => "before-archive",
        }
    }

    /// As the list shows it.
    pub fn describe(self) -> &'static str {
        match self {
            SaveReason::Manual => "saved by hand",
            SaveReason::BeforeModeSwitch => "before a mode switch",
            SaveReason::BeforeRebind => "before another item took the slot",
            SaveReason::BeforeDown => "before down",
            SaveReason::BeforeArchive => "before archive",
        }
    }
}

/// The item a state belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateItem {
    pub source: String,
    pub key: String,
    /// The item's folder name under the work folder.
    pub folder: String,
}

/// One component as it was.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateComponent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<Mode>,
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Hash of the uncommitted diff it was built with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dirty: Option<String>,
    /// prebuilt: the digest; source: the content key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// prebuilt: the tag or build number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_id: Option<String>,
    #[serde(default)]
    pub desired: Desired,
    /// The shared clone it builds or runs from (source and dev).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
}

impl StateComponent {
    /// The exact bits to resume it to.
    pub fn pin(&self) -> ImagePin {
        ImagePin {
            image: self.image.clone(),
            build_id: self.build_id.clone(),
            git_ref: self.git_ref.clone(),
            commit: self.commit.clone(),
            dirty: self.dirty.clone(),
        }
    }

    /// `source task/ABC-1 @1a2b3c4d`, `prebuilt 16 @sha256:111111111111`.
    pub fn describe(&self) -> String {
        describe(
            self.mode,
            self.git_ref.as_deref().or(self.build_id.as_deref()),
            self.commit.as_deref(),
            self.image.as_deref(),
        )
    }
}

fn describe(
    mode: Option<Mode>,
    label: Option<&str>,
    commit: Option<&str>,
    image: Option<&str>,
) -> String {
    let mut parts = vec![mode.map(|m| m.as_str()).unwrap_or("no mode").to_string()];
    if let Some(l) = label {
        parts.push(l.to_string());
    }
    match (mode, commit, image) {
        (Some(Mode::Prebuilt), _, Some(d)) => parts.push(format!("@{}", short_digest(d))),
        (_, Some(c), _) => parts.push(format!("@{}", c8(c))),
        (_, None, Some(i)) => parts.push(format!("@{i}")),
        _ => {}
    }
    parts.join(" ")
}

/// What the user set on the instance for one component (its intent):
/// brought back as it was.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComponentOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<Mode>,
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
}

/// A state seed an item asked for, with its parameters (`{param:x}`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeedChoice {
    pub name: String,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub params: IndexMap<String, String>,
}

/// One script of a recipe: the hook it is, its argv as defined, and what
/// identifies it (a file of the project hashed, or the argv itself when
/// the script lives in the image, which the base digest pins).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScriptRecord {
    /// `scrub`, `seed`, `stateSeeds.<name>`.
    pub hook: String,
    pub argv: Vec<String>,
    /// A project file the argv names, relative to the project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// SHA-256 of that file, else of the argv.
    pub hash: String,
    /// The project's commit when it was saved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

/// How a recipe component's data is rebuilt.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecipeRecord {
    /// The image the database comes from (its digest or content key).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_image: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scripts: Vec<ScriptRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub seeds: Vec<SeedChoice>,
}

/// Where one snapshot went. The state points at it; the artifact lives
/// with the database (in its volume) or in a host folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataSnapshot {
    pub component: String,
    /// The `{snapshot}` value.
    pub name: String,
    pub location: SnapshotLocation,
    /// The volume and path, or the host path, as shown.
    #[serde(rename = "where")]
    pub where_: String,
    /// The engine's name of the volume it lives in (`in-volume`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<String>,
    /// The in-container path the hooks use (for a host copy, where it is
    /// copied back to before `restore`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The host copy (`host`), absolute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_path: Option<PathBuf>,
    pub created_utc: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_hint: Option<String>,
    /// Set once the artifact was deleted (pruned, or dropped when a
    /// sensitive state was archived): the reference stays, the data is gone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removed_utc: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removed_why: Option<String>,
}

impl DataSnapshot {
    /// What two states share when they point at the same artifact.
    pub fn key(&self) -> String {
        format!(
            "{}|{:?}|{}|{}",
            self.component, self.location, self.where_, self.name
        )
    }

    pub fn live(&self) -> bool {
        self.removed_utc.is_none()
    }
}

/// One component's data as saved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateData {
    pub strategy: DataStrategy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipe: Option<RecipeRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<DataSnapshot>,
    /// Why there is no snapshot, when hooks exist but none was taken.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The component's data-safety state when it was saved (`clean`,
    /// `not-clean`, `unscanned` …).
    pub data_safety: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_utc: Option<String>,
}

/// A saved state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedState {
    pub format: u32,
    /// The file stem: `20261114T120000Z` (a `-2` follows on a collision).
    pub id: String,
    pub saved_utc: String,
    pub reason: SaveReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved_by: Option<String>,
    /// The version of chappa-stack that saved it.
    pub chappa_stack: String,
    pub item: StateItem,
    pub stack: String,
    pub slot: u32,
    pub instance: String,
    /// The prebuilt channel the instance used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(default)]
    pub components: IndexMap<String, StateComponent>,
    /// The per-instance settings (the intent's mode, ref, channel).
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub overrides: IndexMap<String, ComponentOverride>,
    /// Per component that keeps data: how to get it back.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub data: IndexMap<String, StateData>,
    pub sensitive: bool,
    /// One line per reason, component names only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sensitive_because: Vec<String>,
    /// Job ids this state refers to (the job that saved it, the checks it
    /// relied on): job retention never prunes them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub jobs: Vec<String>,
}

impl SavedState {
    pub fn path(item_dir: &Path, id: &str) -> PathBuf {
        states_dir(item_dir).join(format!("{id}.json"))
    }

    /// `Ok(None)` for an id that is not one, or no such file.
    pub fn load(item_dir: &Path, id: &str) -> Result<Option<SavedState>, String> {
        if !is_state_id(id) {
            return Ok(None);
        }
        let path = Self::path(item_dir, id);
        match fs::read_to_string(&path) {
            Ok(t) => serde_json::from_str(&t)
                .map(Some)
                .map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    pub fn save(&self, item_dir: &Path) -> Result<PathBuf, String> {
        let path = Self::path(item_dir, &self.id);
        let text = serde_json::to_string_pretty(self).expect("state serializes");
        write_atomic(&path, &text).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(path)
    }

    /// The snapshots it still points at.
    pub fn live_snapshots(&self) -> impl Iterator<Item = &DataSnapshot> {
        self.data
            .values()
            .filter_map(|d| d.snapshot.as_ref())
            .filter(|s| s.live())
    }
}

pub fn states_dir(item_dir: &Path) -> PathBuf {
    item_dir.join(STATES_DIR)
}

/// The default folder of host copies: `<item>/states/data/`.
pub fn host_data_dir(item_dir: &Path) -> PathBuf {
    states_dir(item_dir).join(HOST_DATA_DIR)
}

/// `2026-11-14T12:00:00Z` → `20261114T120000Z`.
pub fn state_id(now_utc: &str) -> String {
    now_utc
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect()
}

/// [`state_id`], with `-2`, `-3` … when a state of that second exists.
pub fn unique_state_id(item_dir: &Path, now_utc: &str) -> String {
    let base = state_id(now_utc);
    if !SavedState::path(item_dir, &base).exists() {
        return base;
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|id| !SavedState::path(item_dir, id).exists())
        .expect("a free id")
}

/// Is `id` one of ours? Guards every path built from a request.
pub fn is_state_id(id: &str) -> bool {
    let (stamp, n) = match id.split_once('-') {
        Some((s, n)) => (s, Some(n)),
        None => (id, None),
    };
    stamp.len() == 16
        && stamp
            .bytes()
            .all(|b| b.is_ascii_digit() || b == b'T' || b == b'Z')
        && n.is_none_or(|n| !n.is_empty() && n.len() <= 4 && n.bytes().all(|b| b.is_ascii_digit()))
}

/// An item's states, newest first; files that cannot be read are skipped.
pub fn list_states(item_dir: &Path) -> Vec<SavedState> {
    let Ok(entries) = fs::read_dir(states_dir(item_dir)) else {
        return Vec::new();
    };
    let mut out: Vec<SavedState> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let id = name.strip_suffix(".json")?.to_string();
            SavedState::load(item_dir, &id).ok().flatten()
        })
        .collect();
    out.sort_by_key(|s| std::cmp::Reverse(order_key(&s.id)));
    out
}

fn order_key(id: &str) -> (String, u32) {
    match id.split_once('-') {
        Some((s, n)) => (s.to_string(), n.parse().unwrap_or(0)),
        None => (id.to_string(), 0),
    }
}

/// The `{snapshot}` value: `<item>-<component>-<state id>`, which only
/// holds `[A-Za-z0-9._-]` because each part does.
pub fn snapshot_name(item_folder: &str, component: &str, state: &str) -> String {
    format!("{item_folder}-{component}-{state}")
}

/// What pruning does.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrunePlan {
    /// States (ids) whose files go.
    pub delete_states: Vec<String>,
    /// Kept states whose snapshot of a component is dropped (it is past
    /// the component's `keep`): `(state, component)`.
    pub drop_refs: Vec<(String, String)>,
    /// Artifacts to delete: no kept state points at them any more.
    pub delete_snapshots: Vec<DataSnapshot>,
}

impl PrunePlan {
    pub fn is_empty(&self) -> bool {
        self.delete_states.is_empty() && self.drop_refs.is_empty()
    }
}

/// Keep the newest `keep_states` states (`states` newest first) and, per
/// component, the newest `keep_snapshots(component)` snapshots among them.
/// An artifact is deleted only when no kept reference points at it, and
/// only an `in-volume` or `host` one (a remote one is never deleted here).
pub fn prune_plan(
    states: &[SavedState],
    keep_states: usize,
    keep_snapshots: &dyn Fn(&str) -> usize,
) -> PrunePlan {
    let keep_states = keep_states.max(1);
    let mut plan = PrunePlan::default();
    let (kept, doomed) = states.split_at(keep_states.min(states.len()));
    let mut seen: IndexMap<String, usize> = IndexMap::new();
    let mut live: HashSet<String> = HashSet::new();
    let mut candidates: Vec<DataSnapshot> = Vec::new();
    for s in kept {
        for snap in s.live_snapshots() {
            let n = seen.entry(snap.component.clone()).or_default();
            *n += 1;
            if *n > keep_snapshots(&snap.component).max(1) {
                plan.drop_refs.push((s.id.clone(), snap.component.clone()));
                candidates.push(snap.clone());
            } else {
                live.insert(snap.key());
            }
        }
    }
    for s in doomed {
        plan.delete_states.push(s.id.clone());
        candidates.extend(s.live_snapshots().cloned());
    }
    let mut queued: HashSet<String> = HashSet::new();
    for snap in candidates {
        let key = snap.key();
        if live.contains(&key) || snap.location == SnapshotLocation::Remote {
            continue;
        }
        if queued.insert(key) {
            plan.delete_snapshots.push(snap);
        }
    }
    plan
}

/// Every item folder with states, live and archived.
fn state_folders(work_dir: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    for (base, prefix) in [
        (work_dir.to_path_buf(), ""),
        (work_dir.join(ARCHIVE_DIR), "_archive/"),
    ] {
        let Ok(entries) = fs::read_dir(&base) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() && states_dir(&p).is_dir() {
                out.push((format!("{prefix}{}", e.file_name().to_string_lossy()), p));
            }
        }
    }
    out.sort();
    out
}

/// The job ids any state on disk refers to, archived ones included: job
/// retention never prunes these.
pub fn referenced_jobs(work_dir: &Path) -> BTreeSet<String> {
    state_folders(work_dir)
        .iter()
        .flat_map(|(_, dir)| list_states(dir))
        .flat_map(|s| s.jobs)
        .collect()
}

/// A saved state whose snapshot lives in a volume about to go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeUse {
    /// The item folder (`_archive/<KEY>` for an archived one).
    pub item: String,
    pub state: String,
    pub component: String,
    pub snapshot: String,
    pub volume: String,
}

/// The saved states (live and archived) with an `in-volume` snapshot in
/// one of `volumes`, oldest item first, newest state first.
pub fn snapshots_in_volumes(work_dir: &Path, volumes: &[String]) -> Vec<VolumeUse> {
    let mut out = Vec::new();
    for (item, dir) in state_folders(work_dir) {
        for s in list_states(&dir) {
            for snap in s.live_snapshots() {
                let Some(v) = &snap.volume else {
                    continue;
                };
                if snap.location == SnapshotLocation::InVolume && volumes.contains(v) {
                    out.push(VolumeUse {
                        item: item.clone(),
                        state: s.id.clone(),
                        component: snap.component.clone(),
                        snapshot: snap.name.clone(),
                        volume: v.clone(),
                    });
                }
            }
        }
    }
    out
}

/// `just now`, `5 min ago`, `3 h ago`, `2 d ago`.
pub fn age(from_utc: &str, now_utc: &str) -> String {
    let (Some(a), Some(b)) = (parse_utc(from_utc), parse_utc(now_utc)) else {
        return from_utc.to_string();
    };
    let s = b.saturating_sub(a);
    match s {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{} min ago", s / 60),
        3600..=86_399 => format!("{} h ago", s / 3600),
        _ => format!("{} d ago", s / 86_400),
    }
}

/// How `state` differs from what runs now, one line per component that
/// changes (`api: source task/ABC-1 @1a2b3c4d (now prebuilt main
/// @sha256:…)`), then one line naming the components already as saved.
pub fn diff_lines(state: &SavedState, now: &InstanceState) -> Vec<String> {
    let mut lines = Vec::new();
    let mut same = Vec::new();
    for (name, c) in &state.components {
        let cur = now.component(name);
        let cur_text = if !cur.exists && !cur.running {
            "not deployed".to_string()
        } else {
            let label = cur.git_ref.as_deref();
            let d = describe(cur.mode, label, cur.commit.as_deref(), cur.image.as_deref());
            if cur.running {
                d
            } else {
                format!("{d}, stopped")
            }
        };
        if c.desired != Desired::Running {
            if cur.running {
                lines.push(format!("{name}: stopped, as when saved (now {cur_text})"));
            }
            continue;
        }
        let bits_same = cur.mode == c.mode
            && match c.mode {
                Some(Mode::Dev) => c.commit.is_some() && cur.commit == c.commit,
                _ => c.image.is_some() && cur.image == c.image,
            };
        if bits_same && cur.running {
            same.push(name.clone());
        } else {
            lines.push(format!("{name}: {} (now {cur_text})", c.describe()));
        }
    }
    if !same.is_empty() {
        lines.push(format!("unchanged: {}", same.join(", ")));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner::ComponentCurrent;

    const T1: &str = "2026-11-14T12:00:00Z";

    fn snap(component: &str, name: &str, location: SnapshotLocation) -> DataSnapshot {
        DataSnapshot {
            component: component.into(),
            name: name.into(),
            location,
            where_: format!("volume app-0-core_db-data at /snap/{name}"),
            volume: Some("app-0-core_db-data".into()),
            path: Some(format!("/snap/{name}")),
            host_path: None,
            created_utc: T1.into(),
            size_bytes: Some(10),
            engine_hint: None,
            removed_utc: None,
            removed_why: None,
        }
    }

    pub(crate) fn state(id: &str, snapshot: Option<DataSnapshot>) -> SavedState {
        let mut components = IndexMap::new();
        components.insert(
            "api".to_string(),
            StateComponent {
                mode: Some(Mode::Source),
                git_ref: Some("task/ABC-1".into()),
                commit: Some("1a2b3c4d5e6f".into()),
                image: Some("api:1a2b3c4d".into()),
                desired: Desired::Running,
                repo: Some("api".into()),
                ..Default::default()
            },
        );
        components.insert(
            "db".to_string(),
            StateComponent {
                mode: Some(Mode::Prebuilt),
                image: Some(
                    "sha256:1111111111111111111111111111111111111111111111111111111111111111"
                        .into(),
                ),
                build_id: Some("16".into()),
                desired: Desired::Running,
                ..Default::default()
            },
        );
        let mut data = IndexMap::new();
        data.insert(
            "db".to_string(),
            StateData {
                strategy: DataStrategy::Snapshot,
                recipe: None,
                snapshot,
                note: None,
                data_safety: "clean".into(),
                checked_utc: Some(T1.into()),
            },
        );
        SavedState {
            format: STATE_FORMAT,
            id: id.into(),
            saved_utc: T1.into(),
            reason: SaveReason::Manual,
            saved_by: None,
            chappa_stack: "0.1.0".into(),
            item: StateItem {
                source: "tracker".into(),
                key: "ABC-1".into(),
                folder: "ABC-1".into(),
            },
            stack: "app".into(),
            slot: 0,
            instance: "app-0".into(),
            channel: None,
            components,
            overrides: IndexMap::new(),
            data,
            sensitive: false,
            sensitive_because: Vec::new(),
            jobs: vec![format!("j{id}-abcdef")],
        }
    }

    #[test]
    fn a_state_round_trips_and_ids_guard_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let item = tmp.path().join("ABC-1");
        assert_eq!(state_id(T1), "20261114T120000Z");
        let id = unique_state_id(&item, T1);
        assert_eq!(id, "20261114T120000Z");
        let s = state(
            &id,
            Some(snap("db", "ABC-1-db-x", SnapshotLocation::InVolume)),
        );
        s.save(&item).unwrap();
        assert_eq!(unique_state_id(&item, T1), "20261114T120000Z-2");
        assert_eq!(SavedState::load(&item, &id).unwrap().unwrap(), s);
        let text = fs::read_to_string(SavedState::path(&item, &id)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["components"]["api"]["ref"], "task/ABC-1");
        assert_eq!(
            v["data"]["db"]["snapshot"]["where"],
            "volume app-0-core_db-data at /snap/ABC-1-db-x"
        );
        assert_eq!(v["data"]["db"]["snapshot"]["location"], "in-volume");
        assert_eq!(v["reason"], "manual");
        for bad in [
            "../x",
            "20261114T120000Z/../a",
            "x",
            "20261114T120000Z-",
            "20261114T120000Z-ab",
        ] {
            assert!(!is_state_id(bad), "{bad}");
            assert_eq!(SavedState::load(&item, bad).unwrap(), None);
        }
        assert!(is_state_id("20261114T120000Z-12"));
        // Newest first, with collision suffixes after their second.
        state("20261114T120000Z-2", None).save(&item).unwrap();
        state("20261113T090000Z", None).save(&item).unwrap();
        let ids: Vec<String> = list_states(&item).into_iter().map(|s| s.id).collect();
        assert_eq!(
            ids,
            ["20261114T120000Z-2", "20261114T120000Z", "20261113T090000Z"]
        );
    }

    #[test]
    fn pruning_keeps_the_newest_and_never_deletes_a_shared_snapshot() {
        let shared = snap("db", "shared", SnapshotLocation::InVolume);
        let states = vec![
            state(
                "20261114T120000Z",
                Some(snap("db", "s4", SnapshotLocation::InVolume)),
            ),
            state("20261113T120000Z", Some(shared.clone())),
            state("20261112T120000Z", Some(shared.clone())),
            state(
                "20261111T120000Z",
                Some(snap("db", "s1", SnapshotLocation::Host)),
            ),
            state(
                "20261110T120000Z",
                Some(snap("db", "r", SnapshotLocation::Remote)),
            ),
        ];
        let plan = prune_plan(&states, 2, &|_| 10);
        assert_eq!(
            plan.delete_states,
            ["20261112T120000Z", "20261111T120000Z", "20261110T120000Z"]
        );
        let names: Vec<&str> = plan
            .delete_snapshots
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        // `shared` is still the kept 20261113's; the remote one is never deleted here.
        assert_eq!(names, ["s1"]);
        assert!(plan.drop_refs.is_empty());

        // keep: 1 per component drops the older kept state's reference;
        // its artifact goes only if no kept reference remains.
        let plan = prune_plan(&states, 3, &|_| 1);
        assert_eq!(
            plan.drop_refs,
            [
                ("20261113T120000Z".to_string(), "db".to_string()),
                ("20261112T120000Z".to_string(), "db".to_string())
            ]
        );
        let names: Vec<&str> = plan
            .delete_snapshots
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(names, ["shared", "s1"]);

        // A removed artifact is not deleted twice.
        let mut gone = snap("db", "gone", SnapshotLocation::InVolume);
        gone.removed_utc = Some(T1.into());
        let plan = prune_plan(
            &[
                state("20261114T120000Z", None),
                state("20261101T120000Z", Some(gone)),
            ],
            1,
            &|_| 10,
        );
        assert!(plan.delete_snapshots.is_empty());
        assert_eq!(plan.delete_states, ["20261101T120000Z"]);
    }

    #[test]
    fn job_references_and_volume_uses_cover_archived_items() {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path();
        state(
            "20261114T120000Z",
            Some(snap("db", "a", SnapshotLocation::InVolume)),
        )
        .save(&work.join("ABC-1"))
        .unwrap();
        let mut host = snap("db", "h", SnapshotLocation::Host);
        host.volume = None;
        state("20261101T120000Z", Some(host))
            .save(&work.join(ARCHIVE_DIR).join("ABC-2"))
            .unwrap();
        let jobs = referenced_jobs(work);
        assert_eq!(
            jobs.into_iter().collect::<Vec<_>>(),
            ["j20261101T120000Z-abcdef", "j20261114T120000Z-abcdef"]
        );
        let uses = snapshots_in_volumes(work, &["app-0-core_db-data".to_string()]);
        assert_eq!(uses.len(), 1);
        assert_eq!(
            (uses[0].item.as_str(), uses[0].snapshot.as_str()),
            ("ABC-1", "a")
        );
        assert!(snapshots_in_volumes(work, &["other".to_string()]).is_empty());
    }

    #[test]
    fn differences_from_what_runs_now() {
        let s = state("20261114T120000Z", None);
        let mut now = InstanceState::new("app", 0);
        now.components.insert(
            "api".into(),
            ComponentCurrent {
                mode: Some(Mode::Prebuilt),
                git_ref: None,
                image: Some("sha256:2222222222222222".into()),
                exists: true,
                running: true,
                ..Default::default()
            },
        );
        now.components.insert(
            "db".into(),
            ComponentCurrent {
                mode: Some(Mode::Prebuilt),
                image: s.components["db"].image.clone(),
                exists: true,
                running: true,
                ..Default::default()
            },
        );
        assert_eq!(
            diff_lines(&s, &now),
            [
                "api: source task/ABC-1 @1a2b3c4d (now prebuilt @sha256:222222222222)",
                "unchanged: db"
            ]
        );
        let empty = InstanceState::new("app", 0);
        assert_eq!(
            diff_lines(&s, &empty)[1],
            "db: prebuilt 16 @sha256:111111111111 (now not deployed)"
        );
        assert_eq!(age("2026-11-14T10:00:00Z", T1), "2 h ago");
        assert_eq!(age(T1, T1), "just now");
        assert_eq!(age("2026-11-10T12:00:00Z", T1), "4 d ago");
    }
}
