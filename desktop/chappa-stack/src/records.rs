//! Job records: one id, a record and two files.
//!
//! | where | holds |
//! |---|---|
//! | the record, in the app-private gate storage ([`stack_core::gate_store`]) keyed by the stack's work folder, `jobs/<id>` | the action, target, slot, the exact argv, `mutates`, status, exit code, start and end times, the paths of the other two, and the stored work (the plan the user approved) |
//! | `<work>/jobs/<id>.log` | the console output (the job's own copy; the pane shows the same lines live) |
//! | `<work>/jobs/<id>.result.json` | the structured outcome the views read: the steps with their exit codes and times, the components' resolved provenance, the data-safety verdicts (counts only), the notes |
//!
//! The record is what `run-job` executes, so it never lives in the work
//! folder: anything working in the project can write there, and a job
//! record it planted (or a plan it rewrote) would run as if the person had
//! confirmed it. A `<work>/jobs/<id>.json` an older version left (or
//! anyone wrote) is not read.
//!
//! Ids are `j<UTC>-<random>` (`j20261114T120000Z-3fa9c1`), so a folder
//! listing sorts by time. Retention keeps a record while it is younger
//! than the stack's `jobs.keepDays` OR among the newest `jobs.keepCount`,
//! whichever keeps more, and never prunes a running job or one a saved
//! state refers to.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use stack_core::compose::parse_utc;
use stack_core::definition::{JobsDef, StackDef};
use stack_core::gate_store::{GateProject, Scope};
use stack_core::jobs::write_atomic;
use stack_core::planner::{Change, Plan};

pub const JOBS_DIR: &str = "jobs";
/// How much of a job's log `GET jobs/{id}` returns (its tail).
pub const LOG_TAIL_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobStatus {
    /// Created; its runner has not started yet.
    Pending,
    Running,
    Done,
    Failed,
    /// Stopped on request: the step running then was ended with every
    /// process it started, the steps after it were skipped.
    Cancelled,
    /// Its runner ended before the job did (the app quit, the pane was
    /// closed, the machine went down): the stack may be part-way changed.
    Interrupted,
}

impl JobStatus {
    pub fn finished(self) -> bool {
        !matches!(self, JobStatus::Pending | JobStatus::Running)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            JobStatus::Pending => "pending",
            JobStatus::Running => "running",
            JobStatus::Done => "done",
            JobStatus::Failed => "failed",
            JobStatus::Cancelled => "cancelled",
            JobStatus::Interrupted => "interrupted",
        }
    }
}

/// What the job does, stored when the action is accepted so the runner
/// executes exactly what the confirm dialog described.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum JobWork {
    /// A planned change: the runner re-observes, applies this plan (which
    /// refuses if the stack moved on since) and runs its steps. With
    /// `save`, the bound item's stack state is saved first, and a failed
    /// save fails the job before anything changes.
    Plan {
        change: Change,
        plan: Box<Plan>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        save: Option<crate::states::SaveSpec>,
        /// `down` that also deletes the data volumes.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        remove_volumes: bool,
    },
    /// Save the bound item's stack state now (snapshot hooks included).
    SaveState { spec: crate::states::SaveSpec },
    /// Bring an item's stack back to a saved state.
    Resume {
        resume: Box<crate::states::ResumeWork>,
    },
    /// A PR review workspace: create (and maybe bring a slot up from it),
    /// bring up, or remove (see [`crate::reviews`]).
    Review { op: crate::reviews::ReviewOp },
    /// `compose restart` of one component.
    Restart { component: String },
    /// Remove unreferenced images and prune build caches.
    Gc,
    /// Run the data-safety checks of these components now, record the
    /// verdicts and apply the stack's policy (see [`crate::data_safety`]).
    DataSafety {
        components: Vec<String>,
        /// What asked for it: `manual` or `every`.
        trigger: String,
    },
    /// A git change to an item workspace (see [`crate::workspaces`]).
    Workspace { op: crate::workspaces::WorkspaceOp },
}

impl JobWork {
    /// Whether a job of this kind changes the instance, and so holds the
    /// instance's lease while it runs: two of them side by side would race
    /// each other's engine commands and scripts, and each other's intent
    /// record. A data-safety check only reads (its policy stop runs after
    /// the check, and is safe beside a change), and most workspace and
    /// review jobs are git changes to a folder; those run alongside.
    pub fn takes_lease(&self) -> bool {
        match self {
            JobWork::DataSafety { .. } => false,
            JobWork::Workspace { op } => matches!(
                op,
                crate::workspaces::WorkspaceOp::Archive { save: Some(_), .. }
            ),
            JobWork::Review { op } => match op {
                crate::reviews::ReviewOp::Create { up, save, .. } => *up || save.is_some(),
                crate::reviews::ReviewOp::Remove { .. } => false,
            },
            _ => true,
        }
    }
}

/// The app's report when the pane that ran the job exits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExitReport {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_utc: Option<String>,
    pub received_utc: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobRecord {
    pub id: String,
    /// The action id (`up`, `stop`, `mode-source` …).
    pub action: String,
    pub label: String,
    /// The component it acts on; `None` for the whole stack.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// The work item the instance is bound to, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<String>,
    pub stack: String,
    pub slot: u32,
    pub instance: String,
    /// What the pane (or the headless runner) runs.
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub mutates: bool,
    /// The confirm text the user accepted.
    pub confirm: String,
    pub status: JobStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub created_utc: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_utc: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_utc: Option<String>,
    /// The runner's pid while it runs, so a runner that died is told apart
    /// from one still at work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner_pid: Option<u32>,
    /// Ran without a pane (the source started the runner itself).
    #[serde(default)]
    pub headless: bool,
    pub log: PathBuf,
    pub result: PathBuf,
    pub work: JobWork,
    /// The definition hash the job was accepted under. The runner refuses
    /// to start when the definition it reads no longer has it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trusted_definition: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<ExitReport>,
    /// Why it failed, in one line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Who asked for it: the actions call's `X-Chappa-Actor`, as kept
    /// (printable ASCII, capped). `None` for a job nobody asked for (the
    /// data-safety schedule) or a call that named no actor. A state the
    /// job saves records it as `savedBy`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
}

/// One step as the runner saw it, for `<id>.result.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StepOutcome {
    pub step: String,
    /// `done`, `failed`, `skipped`.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub seconds: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobResult {
    pub job_id: String,
    pub ok: bool,
    pub exit_code: i32,
    pub steps: Vec<StepOutcome>,
    /// Desired state per component after the job, as recorded.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub components: IndexMap<String, serde_json::Value>,
    /// Resolved provenance per component (mode, ref, commit, image …).
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub provenance: IndexMap<String, serde_json::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// The data-safety verdict per component checked by this job: counts
    /// and statuses, never a sampled value.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub data_safety: IndexMap<String, serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The folder of a stack's job logs, results and locks.
pub fn jobs_dir(work_dir: &Path) -> PathBuf {
    work_dir.join(JOBS_DIR)
}

/// The gate records of the stack whose work folder is `work_dir`.
pub fn gate_of(work_dir: &Path) -> Result<GateProject, String> {
    stack_core::gate_store::for_dir(work_dir).map_err(|e| e.to_string())
}

fn record_name(id: &str) -> String {
    format!("{JOBS_DIR}/{id}")
}

/// The ids of the stack's job records, oldest first.
pub fn record_ids(work_dir: &Path) -> Vec<String> {
    gate_of(work_dir)
        .and_then(|g| g.list(Scope::Project, JOBS_DIR).map_err(|e| e.to_string()))
        .unwrap_or_default()
        .into_iter()
        .filter(|id| is_job_id(id))
        .collect()
}

/// Is `id` one of ours? Guards every path built from a request.
pub fn is_job_id(id: &str) -> bool {
    let Some(rest) = id.strip_prefix('j') else {
        return false;
    };
    let Some((stamp, random)) = rest.split_once('-') else {
        return false;
    };
    stamp.len() == 16
        && stamp
            .bytes()
            .all(|b| b.is_ascii_digit() || b == b'T' || b == b'Z')
        && !random.is_empty()
        && random.len() <= 16
        && random.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `j20261114T120000Z-3fa9c1` for `now_utc` = `2026-11-14T12:00:00Z`.
pub fn new_job_id(now_utc: &str) -> Result<String, String> {
    let stamp: String = now_utc.chars().filter(|c| *c != '-' && *c != ':').collect();
    let mut bytes = [0u8; 3];
    getrandom::fill(&mut bytes).map_err(|e| format!("cannot generate a job id: {e}"))?;
    Ok(format!("j{stamp}-{}", crate::identity::to_hex(&bytes)))
}

impl JobRecord {
    /// Whether this job holds the instance's lease while it runs.
    pub fn takes_lease(&self) -> bool {
        self.mutates && self.work.takes_lease()
    }

    pub fn log_path(work_dir: &Path, id: &str) -> PathBuf {
        jobs_dir(work_dir).join(format!("{id}.log"))
    }

    pub fn result_path(work_dir: &Path, id: &str) -> PathBuf {
        jobs_dir(work_dir).join(format!("{id}.result.json"))
    }

    /// The record of job `id` of the stack whose work folder is
    /// `work_dir`, from the gate storage.
    pub fn load(work_dir: &Path, id: &str) -> Result<Option<JobRecord>, String> {
        if !is_job_id(id) {
            return Ok(None);
        }
        gate_of(work_dir)?
            .read(Scope::Project, &record_name(id))
            .map_err(|e| e.to_string())
    }

    pub fn save(&self, work_dir: &Path) -> Result<(), String> {
        if !is_job_id(&self.id) {
            return Err(format!("`{}` is not a job id", self.id));
        }
        gate_of(work_dir)?
            .write(Scope::Project, &record_name(&self.id), self)
            .map_err(|e| e.to_string())
    }

    /// Delete the record (its log and result are the caller's).
    pub fn remove(work_dir: &Path, id: &str) -> Result<bool, String> {
        if !is_job_id(id) {
            return Ok(false);
        }
        gate_of(work_dir)?
            .remove(Scope::Project, &record_name(id))
            .map_err(|e| e.to_string())
    }

    /// Read, change and write back one record. Records are written by the
    /// runner and by the server; the last writer wins per field group, and
    /// each writer only touches its own fields.
    pub fn update(
        work_dir: &Path,
        id: &str,
        f: impl FnOnce(&mut JobRecord),
    ) -> Result<JobRecord, String> {
        let mut rec = Self::load(work_dir, id)?.ok_or_else(|| format!("no job {id}"))?;
        f(&mut rec);
        rec.save(work_dir)?;
        Ok(rec)
    }
}

/// The tail of a job's log, at most [`LOG_TAIL_BYTES`], cut at a line.
pub fn log_tail(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    if bytes.len() <= LOG_TAIL_BYTES {
        return Some(String::from_utf8_lossy(&bytes).into_owned());
    }
    let start = bytes.len() - LOG_TAIL_BYTES;
    let cut = bytes[start..]
        .iter()
        .position(|b| *b == b'\n')
        .map(|i| start + i + 1)
        .unwrap_or(start);
    Some(format!(
        "[earlier output omitted]\n{}",
        String::from_utf8_lossy(&bytes[cut..])
    ))
}

/// Why a job whose runner went away is `interrupted`.
pub const INTERRUPTED: &str =
    "interrupted: the job's runner ended before the job did (the app quit, its pane was closed, or the machine stopped); the stack may be part-way changed, so look before running it again";
/// Why a job nobody started is failed.
pub const NEVER_STARTED: &str =
    "never started: no runner picked the job up (its pane did not open)";
/// The longest error line kept from an exit report.
const REPORTED_ERROR_CAP: usize = 400;

/// What `GET jobs/{id}` answers: `running` until the job ends, then `done`
/// or `failed` with the log. A cancelled or interrupted job is `failed`
/// with `outcome` saying which (the protocol knows two end states). A
/// running record whose runner is gone (`runner_alive` false) reads as
/// interrupted.
pub fn api_state(rec: &JobRecord, runner_alive: bool) -> serde_json::Value {
    let (state, error, outcome) = match rec.status {
        JobStatus::Done => ("done", None, None),
        JobStatus::Failed => ("failed", rec.error.clone(), None),
        JobStatus::Cancelled | JobStatus::Interrupted => {
            ("failed", rec.error.clone(), Some(rec.status.as_str()))
        }
        JobStatus::Pending => ("running", None, None),
        JobStatus::Running if !runner_alive && rec.report.is_none() => (
            "failed",
            Some(INTERRUPTED.to_string()),
            Some(JobStatus::Interrupted.as_str()),
        ),
        JobStatus::Running => ("running", None, None),
    };
    let mut out = serde_json::json!({ "state": state });
    if let Some(o) = outcome {
        out["outcome"] = o.into();
    }
    if state != "running" {
        if let Some(log) = log_tail(&rec.log) {
            out["log"] = log.into();
        }
        if let Some(code) = rec
            .exit_code
            .or(rec.report.as_ref().and_then(|r| r.exit_code))
        {
            out["exitCode"] = code.into();
        }
        if let Some(e) = error {
            out["error"] = e.into();
        }
    }
    out
}

/// One line of at most [`REPORTED_ERROR_CAP`] characters, controls dropped.
fn one_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|c| !c.is_control())
        .take(REPORTED_ERROR_CAP)
        .collect()
}

/// Apply the app's exit report (`POST jobs/{id}`): the pane's exit code and
/// end time are recorded; its output becomes the log only when the runner
/// wrote none (the runner's own log is the better copy: no terminal
/// redraws). A runner that never finished its record is settled by the
/// report: a job that was running is `interrupted` (its runner ended with
/// the pane), one that never started is `failed`. The report may carry an
/// `error` (the app could not open the pane), which becomes the job's.
pub fn record_exit_report(
    work_dir: &Path,
    id: &str,
    body: &serde_json::Value,
    now_utc: &str,
) -> Result<JobRecord, String> {
    let exit_code = body
        .get("exitCode")
        .and_then(serde_json::Value::as_i64)
        .map(|c| c as i32);
    let ended = body
        .get("endedUtc")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let output = body.get("output").and_then(serde_json::Value::as_str);
    let reported = body
        .get("error")
        .and_then(serde_json::Value::as_str)
        .map(one_line)
        .filter(|e| !e.is_empty());
    let log = JobRecord::log_path(work_dir, id);
    if let Some(text) = output {
        let empty = fs::metadata(&log).map(|m| m.len() == 0).unwrap_or(true);
        if empty {
            write_atomic(&log, text).map_err(|e| format!("{}: {e}", log.display()))?;
        }
    }
    JobRecord::update(work_dir, id, |rec| {
        rec.report = Some(ExitReport {
            exit_code,
            ended_utc: ended.clone(),
            received_utc: now_utc.to_string(),
        });
        if !rec.status.finished() {
            let was_running = rec.status == JobStatus::Running;
            rec.status = match (exit_code, was_running) {
                (Some(0), _) if reported.is_none() => JobStatus::Done,
                (_, true) => JobStatus::Interrupted,
                (_, false) => JobStatus::Failed,
            };
            rec.exit_code = exit_code;
            rec.ended_utc = ended.or_else(|| Some(now_utc.to_string()));
            if rec.status != JobStatus::Done && rec.error.is_none() {
                rec.error = Some(match (&reported, rec.status, exit_code) {
                    (Some(e), _, _) => e.clone(),
                    (None, JobStatus::Interrupted, _) => INTERRUPTED.to_string(),
                    (None, _, Some(c)) => format!("the job's pane exited with code {c}"),
                    (None, _, None) => "the job's pane ended without an exit code".to_string(),
                });
            }
        }
    })
}

/// Retention for a stack (its `jobs:` setting or the defaults).
pub fn retention(stack: &StackDef) -> JobsDef {
    stack.jobs.clone().unwrap_or_default()
}

/// Job ids a saved state refers to (live and archived items): retention
/// never prunes these.
pub fn referenced_by_saved_states(work_dir: &Path) -> BTreeSet<String> {
    stack_core::states::referenced_jobs(work_dir)
}

/// Delete the records retention does not keep; their ids.
pub fn prune(work_dir: &Path, keep: &JobsDef, now_utc: &str) -> Vec<String> {
    let now = parse_utc(now_utc).unwrap_or(0);
    let referenced = referenced_by_saved_states(work_dir);
    let mut records: Vec<JobRecord> = record_ids(work_dir)
        .into_iter()
        .filter_map(|id| JobRecord::load(work_dir, &id).ok().flatten())
        .collect();
    records.sort_by(|a, b| b.created_utc.cmp(&a.created_utc).then(b.id.cmp(&a.id)));
    let max_age = u64::from(keep.keep_days) * 86_400;
    let mut removed = Vec::new();
    for (rank, rec) in records.iter().enumerate() {
        let age = parse_utc(&rec.created_utc)
            .map(|t| now.saturating_sub(t))
            .unwrap_or(0);
        let kept = rank < keep.keep_count as usize
            || age < max_age
            || !rec.status.finished()
            || referenced.contains(&rec.id);
        if kept {
            continue;
        }
        let _ = JobRecord::remove(work_dir, &rec.id);
        for path in [
            JobRecord::log_path(work_dir, &rec.id),
            JobRecord::result_path(work_dir, &rec.id),
            crate::lease::job_lock_path(work_dir, &rec.id),
            crate::lease::cancel_path(work_dir, &rec.id),
        ] {
            let _ = fs::remove_file(path);
        }
        removed.push(rec.id.clone());
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_sort_by_time_and_are_checked_before_any_path_is_built() {
        let id = new_job_id("2026-11-14T12:00:00Z").unwrap();
        assert!(id.starts_with("j20261114T120000Z-"), "{id}");
        assert!(is_job_id(&id));
        assert!(!is_job_id("j20261114T120000Z-../x"));
        assert!(!is_job_id("../../etc/passwd"));
        assert!(!is_job_id("j2026-ab"));
        assert!(!is_job_id("x20261114T120000Z-ab"));
    }
}
