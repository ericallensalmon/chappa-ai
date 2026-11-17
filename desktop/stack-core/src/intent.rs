//! What each instance is *meant* to be running.
//!
//! Status alone cannot tell a crashed service from one somebody stopped on
//! purpose, and a health check cannot either (it only says how to judge a
//! running one). So every verb that changes an instance — up, start, stop,
//! down, a mode switch — writes the desired state of every component here,
//! and status compares it with what the engine reports: not running but
//! wanted is `stopped-problem`, not running and not wanted is
//! `stopped-idle`. A component stopped by hand outside the stack keeps its
//! recorded intent and therefore shows as a problem until it is started
//! again or stopped through the stack. That is deliberate: the stack's own
//! view is the source of truth.
//!
//! Files, under the stack project's work folder (`.work/` by default):
//!
//! ```text
//! .work/instances/<stack>-<slot>.json    one intent record per instance
//! .work/active.json                      the active work item per slot
//! ```
//!
//! Both are written atomically (temp file + rename) so a crash mid-write
//! leaves the previous record, never a truncated one.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::definition::Mode;
use crate::planner::Plan;

pub const INSTANCES_DIR: &str = "instances";
pub const ACTIVE_FILE: &str = "active.json";

/// Desired state of one component.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Desired {
    Running,
    Stopped,
    /// Never brought up, or removed by `down`.
    #[default]
    Absent,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComponentIntent {
    #[serde(default)]
    pub desired: Desired,
    /// Per reported row of a script component, what a verb run on that row
    /// asked for (`stop` on one row, `up` of picked rows). It overrides
    /// `desired` for that row only, until a verb on the whole component
    /// clears it.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub rows: IndexMap<String, Desired>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<Mode>,
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IntentRecord {
    pub instance: String,
    pub stack: String,
    pub slot: u32,
    /// The bound work item, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<String>,
    #[serde(default)]
    pub components: IndexMap<String, ComponentIntent>,
    pub updated_utc: String,
}

#[derive(Debug, thiserror::Error)]
pub enum IntentError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

/// `<stack>-<slot>`: also the compose project prefix and the terraform workspace.
pub fn instance_id(stack: &str, slot: u32) -> String {
    format!("{stack}-{slot}")
}

impl IntentRecord {
    pub fn new(stack: &str, slot: u32, now_utc: &str) -> Self {
        Self {
            instance: instance_id(stack, slot),
            stack: stack.to_string(),
            slot,
            item: None,
            placement: None,
            components: IndexMap::new(),
            updated_utc: now_utc.to_string(),
        }
    }

    pub fn path(work_dir: &Path, instance: &str) -> PathBuf {
        work_dir
            .join(INSTANCES_DIR)
            .join(format!("{instance}.json"))
    }

    /// `Ok(None)` when no record exists yet: a fresh instance wants nothing.
    pub fn load(work_dir: &Path, instance: &str) -> Result<Option<Self>, IntentError> {
        let path = Self::path(work_dir, instance);
        read_json(&path)
    }

    pub fn save(&self, work_dir: &Path) -> Result<PathBuf, IntentError> {
        let path = Self::path(work_dir, &self.instance);
        write_json_atomic(&path, self)?;
        Ok(path)
    }

    pub fn desired(&self, component: &str) -> Desired {
        self.components
            .get(component)
            .map(|c| c.desired)
            .unwrap_or_default()
    }

    /// Fold a plan's outcome in: desired states, modes and refs of every
    /// component the plan touched. Called when the plan was applied, so the
    /// record describes what the user asked for, whether or not the engine
    /// then kept it that way.
    pub fn record_plan(&mut self, plan: &Plan, now_utc: &str) {
        for (name, desired) in &plan.desired {
            let c = self.components.entry(name.clone()).or_default();
            c.desired = *desired;
            // A verb on the whole component replaces what was asked of its
            // rows one by one.
            if !plan.row_desired.contains_key(name) {
                c.rows.clear();
            }
        }
        for (name, rows) in &plan.row_desired {
            let c = self.components.entry(name.clone()).or_default();
            for (row, desired) in rows {
                c.rows.insert(row.clone(), *desired);
            }
        }
        for (name, mode) in &plan.modes {
            self.components.entry(name.clone()).or_default().mode = Some(*mode);
        }
        for (name, git_ref) in &plan.refs {
            self.components.entry(name.clone()).or_default().git_ref = Some(git_ref.clone());
        }
        if let Some(channel) = &plan.channel {
            for (name, mode) in &plan.modes {
                if *mode == Mode::Prebuilt {
                    self.components.entry(name.clone()).or_default().channel =
                        Some(channel.clone());
                }
            }
        }
        self.updated_utc = now_utc.to_string();
    }
}

/// `active.json`: which work item each slot is working on.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ActiveRecord {
    /// Keyed by slot number (as a string: JSON object keys).
    #[serde(default)]
    pub slots: IndexMap<String, ActiveSlot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveSlot {
    pub item: String,
    pub instance: String,
    pub since_utc: String,
}

impl ActiveRecord {
    pub fn path(work_dir: &Path) -> PathBuf {
        work_dir.join(ACTIVE_FILE)
    }

    /// An absent file is an empty record.
    pub fn load(work_dir: &Path) -> Result<Self, IntentError> {
        Ok(read_json(&Self::path(work_dir))?.unwrap_or_default())
    }

    pub fn save(&self, work_dir: &Path) -> Result<PathBuf, IntentError> {
        let path = Self::path(work_dir);
        write_json_atomic(&path, self)?;
        Ok(path)
    }

    pub fn get(&self, slot: u32) -> Option<&ActiveSlot> {
        self.slots.get(&slot.to_string())
    }

    pub fn set(&mut self, slot: u32, item: &str, instance: &str, now_utc: &str) {
        self.slots.insert(
            slot.to_string(),
            ActiveSlot {
                item: item.to_string(),
                instance: instance.to_string(),
                since_utc: now_utc.to_string(),
            },
        );
    }

    pub fn clear(&mut self, slot: u32) -> Option<ActiveSlot> {
        self.slots.shift_remove(&slot.to_string())
    }

    /// Slots currently bound to `item`.
    pub fn slots_for(&self, item: &str) -> Vec<u32> {
        self.slots
            .iter()
            .filter(|(_, s)| s.item == item)
            .filter_map(|(k, _)| k.parse().ok())
            .collect()
    }
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>, IntentError> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(IntentError::Io {
                path: path.to_path_buf(),
                source,
            })
        }
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|source| IntentError::Json {
            path: path.to_path_buf(),
            source,
        })
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), IntentError> {
    let io_err = |source: io::Error| IntentError::Io {
        path: path.to_path_buf(),
        source,
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(io_err)?;
    }
    let text = serde_json::to_string_pretty(value).map_err(|source| IntentError::Json {
        path: path.to_path_buf(),
        source,
    })?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, text.as_bytes()).map_err(io_err)?;
    fs::rename(&tmp, path).map_err(io_err)
}

/// Seconds since the epoch as `YYYY-MM-DDTHH:MM:SSZ`.
pub fn format_utc(secs: u64) -> String {
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Civil-from-days (proleptic Gregorian), valid for the whole u64 range we use.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Now, as the records store it.
pub fn utc_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_utc(secs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::planner::{Plan, Verb};

    const NOW: &str = "2026-03-01T10:00:00Z";

    #[test]
    fn instance_id_shape() {
        assert_eq!(instance_id("app", 0), "app-0");
        assert_eq!(instance_id("app", 12), "app-12");
    }

    #[test]
    fn fresh_instance_wants_nothing() {
        let r = IntentRecord::new("app", 1, NOW);
        assert_eq!(r.instance, "app-1");
        assert_eq!(r.desired("api"), Desired::Absent);
    }

    #[test]
    fn load_missing_is_none_and_save_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        assert!(IntentRecord::load(dir.path(), "app-0").unwrap().is_none());
        let mut r = IntentRecord::new("app", 0, NOW);
        r.item = Some("KEY-12".into());
        r.components.insert(
            "api".into(),
            ComponentIntent {
                desired: Desired::Running,
                rows: IndexMap::new(),
                mode: Some(Mode::Source),
                git_ref: Some("feature/x".into()),
                channel: None,
            },
        );
        r.components.insert(
            "db".into(),
            ComponentIntent {
                desired: Desired::Stopped,
                ..Default::default()
            },
        );
        let path = r.save(dir.path()).unwrap();
        assert_eq!(path, IntentRecord::path(dir.path(), "app-0"));
        assert!(path.ends_with(Path::new(INSTANCES_DIR).join("app-0.json")));
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"ref\": \"feature/x\""), "{text}");
        assert!(text.contains("\"updatedUtc\""));
        assert!(
            !text.contains("channel"),
            "absent optionals are not written"
        );
        let back = IntentRecord::load(dir.path(), "app-0").unwrap().unwrap();
        assert_eq!(back, r);
        assert!(
            !path.with_extension("json.tmp").exists(),
            "temp file is renamed away"
        );
    }

    #[test]
    fn save_replaces_an_existing_record() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = IntentRecord::new("app", 0, NOW);
        r.save(dir.path()).unwrap();
        r.components.insert(
            "api".into(),
            ComponentIntent {
                desired: Desired::Running,
                ..Default::default()
            },
        );
        r.save(dir.path()).unwrap();
        assert_eq!(
            IntentRecord::load(dir.path(), "app-0")
                .unwrap()
                .unwrap()
                .desired("api"),
            Desired::Running
        );
    }

    #[test]
    fn corrupt_record_is_an_error_naming_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = IntentRecord::path(dir.path(), "app-0");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{ not json").unwrap();
        let e = IntentRecord::load(dir.path(), "app-0").unwrap_err();
        assert!(matches!(e, IntentError::Json { .. }));
        assert!(e.to_string().contains("app-0.json"));
    }

    #[test]
    fn record_plan_folds_desired_modes_and_refs() {
        let mut r = IntentRecord::new("app", 0, NOW);
        let mut plan = Plan::empty("app-0", Verb::Up, "fp");
        plan.desired.insert("api".into(), Desired::Running);
        plan.desired.insert("db".into(), Desired::Running);
        plan.modes.insert("api".into(), Mode::Source);
        plan.refs.insert("api".into(), "feature/x".into());
        r.record_plan(&plan, "2026-03-01T11:00:00Z");
        assert_eq!(r.desired("api"), Desired::Running);
        assert_eq!(r.components["api"].mode, Some(Mode::Source));
        assert_eq!(r.components["api"].git_ref.as_deref(), Some("feature/x"));
        assert_eq!(r.components["db"].mode, None);
        assert_eq!(r.updated_utc, "2026-03-01T11:00:00Z");

        let mut stop = Plan::empty("app-0", Verb::Stop, "fp");
        stop.desired.insert("api".into(), Desired::Stopped);
        r.record_plan(&stop, NOW);
        assert_eq!(r.desired("api"), Desired::Stopped);
        assert_eq!(
            r.components["api"].mode,
            Some(Mode::Source),
            "mode survives a stop"
        );
        assert_eq!(r.desired("db"), Desired::Running);
    }

    #[test]
    fn active_record_shape_and_ops() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = ActiveRecord::load(dir.path()).unwrap();
        assert!(a.slots.is_empty());
        a.set(0, "KEY-12", "app-0", NOW);
        a.set(2, "KEY-12", "app-2", NOW);
        a.set(1, "KEY-7", "app-1", NOW);
        let path = a.save(dir.path()).unwrap();
        assert_eq!(path, dir.path().join(ACTIVE_FILE));
        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(json["slots"]["0"]["item"], "KEY-12");
        assert_eq!(json["slots"]["0"]["instance"], "app-0");
        assert_eq!(json["slots"]["0"]["sinceUtc"], NOW);
        let back = ActiveRecord::load(dir.path()).unwrap();
        assert_eq!(back, a);
        assert_eq!(back.get(1).unwrap().item, "KEY-7");
        assert_eq!(back.slots_for("KEY-12"), vec![0, 2]);
        let mut b = back;
        assert!(b.clear(1).is_some());
        assert!(b.clear(1).is_none());
        assert!(b.get(1).is_none());
    }

    #[test]
    fn utc_formatting_at_known_points() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(format_utc(1_709_251_199), "2024-02-29T23:59:59Z");
        assert_eq!(format_utc(1_767_225_600), "2026-01-01T00:00:00Z");
        let now = utc_now();
        assert_eq!(now.len(), 20);
        assert!(now.ends_with('Z'));
        assert!(now.starts_with("20"));
    }
}
