//! Who may change an instance now, and whether a job's runner still runs.
//!
//! Each lock below is the operating system's exclusive lock on a file
//! (`File::try_lock`), held for as long as its holder runs. The system
//! drops a lock when its process ends, however it ends, so a lock is never
//! stale and a reused process id can never pass for a runner.
//!
//! | file under `<work>/jobs/` | held or written by | means |
//! |---|---|---|
//! | `<id>.lock` | the runner of job `<id>`, from before it reads the job until it has written the job's end | the runner is alive |
//! | `<instance>.lock` | the runner of the instance's one changing job ([`JobRecord::takes_lease`]) | the instance's lease |
//! | `<instance>.holder.json` | the lease holder, right after it took the lease | which job holds it (a locked file cannot be read on Windows) |
//! | `<id>.cancel` | the source, when someone cancels the job | the runner stops what runs and skips the rest |
//!
//! The source refuses a changing action while the lease is held, or while a
//! changing job it accepted has not started yet (for [`PENDING_GRACE`]),
//! and the runner takes the lease itself before its first step, so a
//! `run-job` started by hand is held to the same rule. Changing work is
//! refused, never queued: a queued change would run against a stack nobody
//! looked at since.
//!
//! Lock files are never deleted while a holder may still run (on Unix a
//! deleted file would let a second holder lock a new file of the same
//! name); a job's own lock goes with its record when retention prunes it.
//! A job's record is settled (`interrupted`, or `failed` when it never
//! started) only by whoever holds that job's lock, so a runner starting at
//! that moment and a source settling the job never both win.

use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use stack_core::compose::parse_utc;
use stack_core::jobs::write_atomic;

use crate::records::{self, is_job_id, jobs_dir, JobRecord, JobStatus};

/// How long an accepted changing job may wait for its runner before it no
/// longer holds the instance (and is settled as never started).
pub const PENDING_GRACE: Duration = Duration::from_secs(120);
/// How long a runner tries for a lock before it gives up. The source only
/// ever holds one for a moment, to look.
pub const ACQUIRE_WAIT: Duration = Duration::from_secs(2);
/// How many records, newest first, the view reads to find the instance's
/// last changing job.
const SCAN_CAP: usize = 200;

pub fn job_lock_path(work_dir: &Path, id: &str) -> PathBuf {
    jobs_dir(work_dir).join(format!("{id}.lock"))
}

pub fn instance_lock_path(work_dir: &Path, instance: &str) -> PathBuf {
    jobs_dir(work_dir).join(format!("{instance}.lock"))
}

pub fn holder_path(work_dir: &Path, instance: &str) -> PathBuf {
    jobs_dir(work_dir).join(format!("{instance}.holder.json"))
}

pub fn cancel_path(work_dir: &Path, id: &str) -> PathBuf {
    jobs_dir(work_dir).join(format!("{id}.cancel"))
}

/// A lock held; dropping it releases it.
#[derive(Debug)]
pub struct Held(#[allow(dead_code)] File);

fn open_lock(path: &Path, create: bool) -> std::io::Result<File> {
    if create {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .truncate(false)
        .open(path)
}

/// Take the lock at `path` if nobody holds it; `None` when someone does.
pub fn try_hold(path: &Path) -> Result<Option<Held>, String> {
    let f = open_lock(path, true).map_err(|e| format!("{}: {e}", path.display()))?;
    match f.try_lock() {
        Ok(()) => Ok(Some(Held(f))),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(format!("{}: {e}", path.display())),
    }
}

/// [`try_hold`], trying again for up to `wait`.
pub fn hold_within(path: &Path, wait: Duration) -> Result<Option<Held>, String> {
    let deadline = Instant::now() + wait;
    loop {
        if let Some(h) = try_hold(path)? {
            return Ok(Some(h));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Whether someone holds the lock at `path` now: `None` when the file does
/// not exist (nobody ever took it). A file that cannot be opened counts as
/// held, so nothing is settled on a guess.
pub fn held(path: &Path) -> Option<bool> {
    if !path.exists() {
        return None;
    }
    match open_lock(path, false) {
        Ok(f) => Some(!matches!(f.try_lock(), Ok(()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => Some(true),
    }
}

/// Which job holds an instance's lease, as its holder wrote it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Holder {
    pub job_id: String,
    pub label: String,
    pub since_utc: String,
    pub pid: u32,
}

pub fn read_holder(work_dir: &Path, instance: &str) -> Option<Holder> {
    let text = fs::read_to_string(holder_path(work_dir, instance)).ok()?;
    serde_json::from_str(&text).ok()
}

/// The instance's lease, held by a runner; its holder file goes when it is
/// dropped (before the lock is released).
#[derive(Debug)]
pub struct Lease {
    holder: PathBuf,
    _lock: Held,
}

impl Drop for Lease {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.holder);
    }
}

/// "local: bring up is running since 10:02:11 UTC".
pub fn busy_text(label: &str, since_utc: &str) -> String {
    format!("{label} is running since {}", clock_of(since_utc))
}

/// `HH:MM:SS UTC` of an ISO time, or the text itself when it is not one.
pub fn clock_of(utc: &str) -> String {
    match utc.split_once('T') {
        Some((_, t)) if t.len() >= 8 => format!("{} UTC", &t[..8]),
        _ => utc.to_string(),
    }
}

/// Take `rec`'s instance lease for a runner, or say who holds it.
pub fn take_lease(work_dir: &Path, rec: &JobRecord, since_utc: &str) -> Result<Lease, String> {
    let lock = hold_within(&instance_lock_path(work_dir, &rec.instance), ACQUIRE_WAIT)?;
    let Some(lock) = lock else {
        return Err(match read_holder(work_dir, &rec.instance) {
            Some(h) => busy_text(&h.label, &h.since_utc),
            None => format!("another job is changing {}", rec.instance),
        });
    };
    let holder = Holder {
        job_id: rec.id.clone(),
        label: rec.label.clone(),
        since_utc: since_utc.to_string(),
        pid: std::process::id(),
    };
    let path = holder_path(work_dir, &rec.instance);
    write_atomic(
        &path,
        &serde_json::to_string_pretty(&holder).expect("holder serializes"),
    )
    .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(Lease {
        holder: path,
        _lock: lock,
    })
}

/// Who asked for a cancel, and when.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    pub requested_utc: String,
}

pub fn request_cancel(
    work_dir: &Path,
    id: &str,
    actor: Option<&str>,
    now_utc: &str,
) -> Result<(), String> {
    let req = CancelRequest {
        actor: actor.map(str::to_string),
        requested_utc: now_utc.to_string(),
    };
    let path = cancel_path(work_dir, id);
    write_atomic(
        &path,
        &serde_json::to_string_pretty(&req).expect("cancel serializes"),
    )
    .map_err(|e| format!("{}: {e}", path.display()))
}

pub fn cancel_requested(work_dir: &Path, id: &str) -> Option<CancelRequest> {
    let text = fs::read_to_string(cancel_path(work_dir, id)).ok()?;
    Some(serde_json::from_str(&text).unwrap_or(CancelRequest {
        actor: None,
        requested_utc: String::new(),
    }))
}

/// "cancelled by human:sam" (or just "cancelled").
pub fn cancelled_text(req: Option<&CancelRequest>) -> String {
    match req.and_then(|r| r.actor.as_deref()) {
        Some(a) => format!("cancelled by {a}"),
        None => "cancelled".to_string(),
    }
}

fn age(rec: &JobRecord, now_utc: &str) -> Duration {
    let now = parse_utc(now_utc).unwrap_or(0);
    let created = parse_utc(&rec.created_utc).unwrap_or(now);
    Duration::from_secs(now.saturating_sub(created))
}

/// Whether `rec`'s runner still runs: its lock is held; for a record whose
/// runner took no lock (an older runner), its pid is alive.
pub fn runner_alive(work_dir: &Path, rec: &JobRecord, pid_alive: &dyn Fn(u32) -> bool) -> bool {
    match held(&job_lock_path(work_dir, &rec.id)) {
        Some(h) => h,
        None => rec.runner_pid.is_some_and(pid_alive),
    }
}

/// Whether an unfinished `rec` still counts: its runner runs, or it waits
/// for one (pending, within [`PENDING_GRACE`], not cancelled).
pub fn live(
    work_dir: &Path,
    rec: &JobRecord,
    now_utc: &str,
    pid_alive: &dyn Fn(u32) -> bool,
) -> bool {
    if rec.status.finished() {
        return false;
    }
    if runner_alive(work_dir, rec, pid_alive) {
        return true;
    }
    rec.status == JobStatus::Pending
        && age(rec, now_utc) < PENDING_GRACE
        && cancel_requested(work_dir, &rec.id).is_none()
}

/// Settle job `id` when nothing runs it any more: a running job whose
/// runner ended becomes `interrupted`, a pending one past its grace (or
/// cancelled before it started) `failed` (`cancelled`). Done under the
/// job's own lock. The record as settled, or `None` when it still counts
/// or was finished already.
pub fn settle(
    work_dir: &Path,
    id: &str,
    now_utc: &str,
    pid_alive: &dyn Fn(u32) -> bool,
) -> Option<JobRecord> {
    let rec = JobRecord::load(work_dir, id).ok().flatten()?;
    if live(work_dir, &rec, now_utc, pid_alive) || rec.status.finished() {
        return None;
    }
    let _held = try_hold(&job_lock_path(work_dir, id)).ok().flatten()?;
    // Under the lock: read again, in case a runner finished it meanwhile.
    let rec = JobRecord::load(work_dir, id).ok().flatten()?;
    if rec.status.finished() {
        return None;
    }
    let cancel = cancel_requested(work_dir, id);
    JobRecord::update(work_dir, id, |r| {
        let (status, why) = match (r.status, &cancel) {
            (JobStatus::Pending, Some(c)) => (
                JobStatus::Cancelled,
                format!("{} before it started", cancelled_text(Some(c))),
            ),
            (JobStatus::Pending, None) => (JobStatus::Failed, records::NEVER_STARTED.to_string()),
            _ => (JobStatus::Interrupted, records::INTERRUPTED.to_string()),
        };
        r.status = status;
        r.ended_utc = Some(now_utc.to_string());
        if r.error.is_none() {
            r.error = Some(why);
        }
    })
    .ok()
}

/// The ids of the records under `<work>/jobs/`, newest first.
fn ids_newest_first(work_dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(jobs_dir(work_dir)) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let id = name.strip_suffix(".json")?;
            is_job_id(id).then(|| id.to_string())
        })
        .collect();
    ids.sort_unstable_by(|a, b| b.cmp(a));
    ids
}

/// Settle every unfinished record whose runner is gone (at source start).
/// The ids settled.
pub fn settle_all(work_dir: &Path, now_utc: &str, pid_alive: &dyn Fn(u32) -> bool) -> Vec<String> {
    ids_newest_first(work_dir)
        .into_iter()
        .filter(|id| {
            JobRecord::load(work_dir, id)
                .ok()
                .flatten()
                .is_some_and(|r| !r.status.finished())
        })
        .filter(|id| settle(work_dir, id, now_utc, pid_alive).is_some())
        .collect()
}

/// The instance's changing jobs as the views show them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct JobsView {
    /// The changing job that holds (or is about to hold) the instance.
    pub active: Option<JobRecord>,
    /// Someone asked the active job to stop.
    pub cancelling: bool,
    /// The newest changing job that ended.
    pub last: Option<JobRecord>,
}

/// Look at the instance's changing jobs, settling on the way the ones whose
/// runner is gone (so a job killed with the app reads `interrupted` the next
/// time anyone looks, not `running` forever).
pub fn view(
    work_dir: &Path,
    instance: &str,
    now_utc: &str,
    pid_alive: &dyn Fn(u32) -> bool,
) -> JobsView {
    let mut out = JobsView::default();
    if let Some(h) = read_holder(work_dir, instance) {
        match held(&instance_lock_path(work_dir, instance)) {
            Some(true) => {
                out.active = JobRecord::load(work_dir, &h.job_id)
                    .ok()
                    .flatten()
                    .filter(|r| !r.status.finished());
            }
            _ => {
                // Its holder ended without letting go cleanly.
                settle(work_dir, &h.job_id, now_utc, pid_alive);
                if let Ok(Some(_lock)) = try_hold(&instance_lock_path(work_dir, instance)) {
                    let _ = fs::remove_file(holder_path(work_dir, instance));
                }
            }
        }
    }
    for id in ids_newest_first(work_dir).into_iter().take(SCAN_CAP) {
        let Some(mut rec) = JobRecord::load(work_dir, &id).ok().flatten() else {
            continue;
        };
        if rec.instance != instance || !rec.takes_lease() {
            continue;
        }
        if !rec.status.finished() {
            if live(work_dir, &rec, now_utc, pid_alive) {
                if out.active.is_none() {
                    out.active = Some(rec);
                }
                continue;
            }
            match settle(work_dir, &id, now_utc, pid_alive) {
                Some(settled) => rec = settled,
                None => continue,
            }
        }
        out.last = Some(rec);
        break;
    }
    out.cancelling = out
        .active
        .as_ref()
        .is_some_and(|a| cancel_requested(work_dir, &a.id).is_some());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lock_is_held_until_its_holder_lets_go_and_a_missing_file_is_nobody() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("jobs").join("x.lock");
        assert_eq!(held(&path), None);
        let first = try_hold(&path).unwrap().expect("free");
        assert_eq!(held(&path), Some(true));
        assert!(try_hold(&path).unwrap().is_none(), "held by the first");
        drop(first);
        assert_eq!(held(&path), Some(false));
        assert!(try_hold(&path).unwrap().is_some());
    }

    #[test]
    fn the_busy_text_names_the_job_and_its_start_clock_time() {
        assert_eq!(
            busy_text("local: bring up", "2026-11-14T10:02:11Z"),
            "local: bring up is running since 10:02:11 UTC"
        );
        assert_eq!(clock_of("soon"), "soon");
    }
}
