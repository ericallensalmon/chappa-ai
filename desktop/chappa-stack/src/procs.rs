//! The process table: which process components of an instance this stack
//! started, by pid, with their logs.
//!
//! A dev server or a `process`-driver command outlives the job that
//! started it, so it runs detached (see [`crate::world::ProcessHost`]) and
//! is remembered in `<work>/instances/<instance>/processes.json`. Status
//! reads the table and asks whether each pid is still alive; a stop ends
//! the process tree and marks the entry stopped, a `down` forgets it. Its output goes to
//! `<work>/instances/<instance>/logs/<component>.log`, which the
//! component's `logs` action follows.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

use stack_core::intent::INSTANCES_DIR;
use stack_core::jobs::write_atomic;
use stack_core::process::ProcessStatus;

use crate::world::ProcessHost;

pub const PROCESSES_FILE: &str = "processes.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessEntry {
    pub pid: u32,
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub log: PathBuf,
    pub started_utc: String,
    /// Set when the stack stopped it (kept, so the row reads "stopped"
    /// rather than "not deployed"); `down` forgets the entry instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_utc: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessTable {
    #[serde(default)]
    pub processes: IndexMap<String, ProcessEntry>,
}

pub fn instance_dir(work_dir: &Path, instance: &str) -> PathBuf {
    work_dir.join(INSTANCES_DIR).join(instance)
}

pub fn table_path(work_dir: &Path, instance: &str) -> PathBuf {
    instance_dir(work_dir, instance).join(PROCESSES_FILE)
}

pub fn log_path(work_dir: &Path, instance: &str, component: &str) -> PathBuf {
    instance_dir(work_dir, instance)
        .join("logs")
        .join(format!("{component}.log"))
}

impl ProcessTable {
    /// A missing or unreadable table is empty: nothing is known to run.
    pub fn load(work_dir: &Path, instance: &str) -> ProcessTable {
        fs::read_to_string(table_path(work_dir, instance))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, work_dir: &Path, instance: &str) -> Result<(), String> {
        let path = table_path(work_dir, instance);
        write_atomic(
            &path,
            &serde_json::to_string_pretty(self).expect("table serializes"),
        )
        .map_err(|e| format!("{}: {e}", path.display()))
    }

    /// What status needs: per component, running or not.
    pub fn statuses(&self, host: &dyn ProcessHost) -> IndexMap<String, ProcessStatus> {
        self.processes
            .iter()
            .map(|(name, e)| {
                (
                    name.clone(),
                    ProcessStatus {
                        running: e.stopped_utc.is_none() && host.alive(e.pid),
                        exit_code: None,
                        started_utc: Some(e.started_utc.clone()),
                        restarts: 0,
                    },
                )
            })
            .collect()
    }
}
