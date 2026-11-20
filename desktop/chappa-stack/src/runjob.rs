//! `chappa-stack run-job <id>`: execute one stored job, in a pane or
//! headless, and leave its three files behind.
//!
//! A job is the whole plan the user confirmed, so the pane shows all of it
//! live: the checks before up, each step's command and output, a line per
//! file written, and a last line with the outcome. In order:
//!
//! 1. the runner takes the job's own lock, refuses a job that is not
//!    `pending` (it ran, or another runner has it), and, for a job that
//!    changes the instance, takes the instance's lease (see
//!    [`crate::lease`]): while another changing job holds it, this one is
//!    refused, not queued. Then the record goes to `running` with this
//!    process's pid;
//! 2. the instance is observed again and the stored plan is applied to it.
//!    Applying asks the engines quick questions (digests, existing images),
//!    which is why the pane first says "resolving images…". If the stack
//!    moved on since the confirm, nothing runs and the job says so;
//! 3. for a plan that starts anything, the checks before up: an engine
//!    that cannot be reached, not enough memory, a port in use or a missing
//!    registry login stop the job before any step; a hostname that does not
//!    resolve is only a warning;
//! 4. the steps, in order: files are written in process, engine commands
//!    run as children with their output streamed (each under its limit
//!    from [`crate::timeouts`]), process components start detached and go
//!    into the process table, process stops end their tree; a script
//!    component's script runs like an engine command but under its own
//!    limit (the definition's `timeouts`), a timeout stops its whole
//!    process tree, and how it went is recorded for the component's state
//!    (`.work/instances/<instance>/scripts.json`);
//! 5. on a failed step the remaining steps are skipped, except the intent
//!    record, which is still written: the user asked for these components
//!    to run, so one that did not start shows as a problem, not as idle;
//! 6. the data-safety checks of the components the plan started (see
//!    [`crate::data_safety`]): under `block-up` right after each start
//!    step, so a verdict that is not clean ends the job before anything
//!    else starts; otherwise once the steps are done. A component the
//!    policy says must not stay up (`stop`, `block-up`) is stopped through
//!    the planner, its volume kept, and the job fails;
//! 7. `<id>.result.json`, then the record's final status; the exit code is
//!    0 done, 1 a step failed (or data safety stopped something), 2
//!    refused before running anything, 3 cancelled (130 when a third
//!    Ctrl+C ended the runner while it was cancelling).
//!
//! **Cancel.** A cancel request (`<id>.cancel`, written by the source) is
//! picked up within a fraction of a second: the step running then is
//! stopped with every process it started (see [`crate::world::StepExec`]),
//! the steps after it are skipped like after a failure (the intent record
//! is still written), and the job ends `cancelled`. The lease is let go
//! only after that, so the next change never starts beside the remains of
//! this one. Work that is not a command (a snapshot hook, a git change)
//! finishes first; the cancel takes effect at the next step. Ctrl+C in the
//! job's terminal asks for the same cancel (see [`crate::interrupt`]).
//!
//! **The intent record** is written as the plan's last step by reading the
//! record as it is then and applying only this plan's change to it, so a
//! record that moved while the job ran keeps what moved.
//!
//! When the slot's work item asks state seeds for a recipe component the
//! plan creates or recreates, that component's recipe is replayed after
//! the steps (scrub, check, seed, the item's state seeds, check), as a
//! resume replays a saved state's.
//!
//! A `data-safety-check` job runs the checks of the components it names
//! that are running, records the verdicts, applies `stop` / `block-up`
//! the same way, and fails when any verdict is not clean.
//!
//! An activation (a plan that binds a work item) builds from the item it
//! activates: the runner is bound to that item's worktrees before the plan
//! is applied, and the active-item record is written as the first step. A workspace
//! job (Start work, Add repo, Archive, Restore) is a git change to an item
//! folder; see [`crate::workspaces`].

use std::cell::RefCell;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use indexmap::IndexMap;
use stack_core::checks::EngineChecker;
use stack_core::definition::DataStrategy;
use stack_core::definition::ScanWhen;
use stack_core::definition::StacksDocument;
use stack_core::driver::{Check, Outcome, ResolvedInstance};
use stack_core::images::ProvenanceCache;
use stack_core::intent::{utc_now, ActiveRecord, IntentRecord};
use stack_core::jobs::{JobStep, LocalStep, RunBlock};
use stack_core::planner::{Change, Plan, PlanError, Step, Verb};
use stack_core::runner::Observation;
use stack_core::runner::{ApplyOptions, RunnerError, StackRunner};
use stack_core::safety::Policy;
use stack_core::script::{LastRun, ScriptRuns, ScriptVerb};
use stack_core::states::{DataSnapshot, RecipeRecord, SavedState, SeedChoice};
use stack_core::workspace::{c8, ItemRecord};

use crate::data_safety::{self, CheckEntry, SafetyState, Trigger, AFTER_UP_ATTEMPTS};
use crate::interrupt::{self, Interrupts};
use crate::lease;
use crate::procs::{log_path, ProcessEntry, ProcessTable};
use crate::project::Loaded;
use crate::records::{JobRecord, JobResult, JobStatus, JobWork, StepOutcome};
use crate::reviews::{self, ReviewOp};
use crate::secret_scan;
use crate::states::{self, Exists, ResumeWork, SaveSpec};
use crate::timeouts::{self, StepKind};
use crate::workspaces::{Git, WorkspaceOp};
use crate::world::{open_log, Cancel, World};

/// Exit codes of `run-job`.
pub const EXIT_DONE: i32 = 0;
pub const EXIT_FAILED: i32 = 1;
pub const EXIT_REFUSED: i32 = 2;
pub const EXIT_CANCELLED: i32 = 3;

/// How often the runner looks for a cancel request.
const CANCEL_POLL: Duration = Duration::from_millis(200);

/// The environment key every process component's run block carries (the
/// instance environment); engine commands carry none. It is how a run step
/// that starts a long-lived process is told apart from one that finishes.
const INSTANCE_ENV: &str = "CHAPPA_STACK_INSTANCE";

/// Where a job's lines go: the pane (stdout) and the job's log file.
///
/// Every line, a step's output included, goes out with the values the
/// runner generated for an instance shown as `••••`: commands and their
/// output can carry the instance env. The values are read again for each
/// line, because the job itself may be what generates them.
struct Out<'a> {
    pane: &'a mut dyn Write,
    log: Option<File>,
    work_dir: PathBuf,
    /// Set once someone cancels the job; every step command watches it.
    cancel: Cancel,
    /// Ctrl+C in the job's terminal, when this runner handles it: told
    /// which step runs, so a press can say what it stops.
    interrupts: Option<&'a Interrupts>,
}

impl Out<'_> {
    fn say(&mut self, text: &str) {
        let shown = stack_core::instance_secrets::existing(&self.work_dir).hide(text);
        crate::world::line(self.pane, &shown);
        if let Some(f) = &mut self.log {
            // One write per line: a Ctrl+C notice shares the file.
            let _ = f.write_all(format!("{shown}\n").as_bytes());
        }
    }

    fn step(&self, now: Option<(usize, usize, &str)>) {
        if let Some(i) = self.interrupts {
            i.step(now);
        }
    }
}

/// Find the record for `id` under any stack's work folder of the project.
pub fn find_record(project_dir: &Path, id: &str) -> Result<(PathBuf, JobRecord), String> {
    let (_, doc) = StacksDocument::load_from_project(project_dir)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("{} defines no stack", project_dir.display()))?;
    for stack in doc.stacks.values() {
        let work_dir = if stack.work_dir.is_absolute() {
            stack.work_dir.clone()
        } else {
            project_dir.join(&stack.work_dir)
        };
        if let Some(rec) = JobRecord::load(&work_dir, id)? {
            return Ok((work_dir, rec));
        }
    }
    Err(format!("no job {id} in {}", project_dir.display()))
}

/// Run job `id` of the project in `project_dir`; the exit code.
pub fn run_job(world: &World, project_dir: &Path, id: &str, pane: &mut dyn Write) -> i32 {
    run_job_with(world, project_dir, id, pane, None)
}

/// [`run_job`] in a terminal whose Ctrl+C `interrupts` handles: a press
/// cancels the job (see [`crate::interrupt`]).
pub fn run_job_with(
    world: &World,
    project_dir: &Path,
    id: &str,
    pane: &mut dyn Write,
    interrupts: Option<&Interrupts>,
) -> i32 {
    let code = run_job_inner(world, project_dir, id, pane, interrupts);
    if let Some(i) = interrupts {
        i.end();
    }
    code
}

fn run_job_inner(
    world: &World,
    project_dir: &Path,
    id: &str,
    pane: &mut dyn Write,
    interrupts: Option<&Interrupts>,
) -> i32 {
    let (work_dir, _) = match find_record(project_dir, id) {
        Ok(found) => found,
        Err(e) => {
            crate::world::line(pane, &format!("chappa-stack: {e}"));
            return EXIT_REFUSED;
        }
    };
    // The job's own lock first: with it held nobody else runs or settles
    // this job, and the record read under it is the one to go by.
    let _job_lock =
        match lease::hold_within(&lease::job_lock_path(&work_dir, id), lease::ACQUIRE_WAIT) {
            Ok(Some(h)) => h,
            Ok(None) => {
                crate::world::line(
                    pane,
                    &format!("chappa-stack: job {id} is already being run by another runner"),
                );
                return EXIT_REFUSED;
            }
            Err(e) => {
                crate::world::line(pane, &format!("chappa-stack: {e}"));
                return EXIT_REFUSED;
            }
        };
    let rec = match JobRecord::load(&work_dir, id) {
        Ok(Some(r)) => r,
        Ok(None) => {
            crate::world::line(pane, &format!("chappa-stack: no job {id}"));
            return EXIT_REFUSED;
        }
        Err(e) => {
            crate::world::line(pane, &format!("chappa-stack: {e}"));
            return EXIT_REFUSED;
        }
    };
    if rec.status != JobStatus::Pending {
        let why = if rec.status.finished() {
            format!("job {id} already ran ({})", rec.status.as_str())
        } else {
            format!("job {id} was started before and is not waiting to run")
        };
        crate::world::line(pane, &format!("chappa-stack: {why}"));
        return EXIT_REFUSED;
    }
    let log = open_log(&rec.log, true).ok();
    let mut out = Out {
        pane,
        log,
        work_dir: work_dir.clone(),
        cancel: Cancel::new(),
        interrupts,
    };
    if let Some(i) = interrupts {
        i.begin(interrupt::Job {
            work_dir: work_dir.clone(),
            id: id.to_string(),
            label: rec.label.clone(),
            cancel: out.cancel.clone(),
            log: out.log.as_ref().and_then(|f| f.try_clone().ok()),
        });
    }
    if let Some(req) = lease::cancel_requested(&work_dir, id) {
        let why = format!("{} before it started", lease::cancelled_text(Some(&req)));
        out.say(&format!("✗ {why}"));
        end_record(
            &work_dir,
            id,
            JobStatus::Cancelled,
            EXIT_CANCELLED,
            Some(why),
        );
        return EXIT_CANCELLED;
    }
    let started = utc_now();
    let _lease = if rec.takes_lease() {
        match lease::take_lease(&work_dir, &rec, &started) {
            Ok(l) => Some(l),
            Err(holder) => {
                let why = format!(
                    "not run: {holder}; nothing was changed. Run it again once that job ended"
                );
                out.say(&format!("✗ {why}"));
                end_record(&work_dir, id, JobStatus::Failed, EXIT_REFUSED, Some(why));
                return EXIT_REFUSED;
            }
        }
    } else {
        None
    };
    let _ = JobRecord::update(&work_dir, id, |r| {
        r.status = JobStatus::Running;
        r.started_utc = Some(started.clone());
        r.runner_pid = Some(std::process::id());
    });
    let watcher = watch_for_cancel(&work_dir, id, &out.cancel);
    out.say(&format!("{} · job {id}", rec.label));
    let t0 = Instant::now();
    let mut result = JobResult {
        job_id: id.to_string(),
        ok: false,
        exit_code: EXIT_FAILED,
        steps: Vec::new(),
        components: IndexMap::new(),
        provenance: IndexMap::new(),
        notes: Vec::new(),
        data_safety: IndexMap::new(),
        error: None,
    };
    let outcome = execute(world, project_dir, &work_dir, &rec, &mut out, &mut result);
    let cancelled = out.cancel.is_set();
    let code = match outcome {
        Ok(()) if !cancelled => EXIT_DONE,
        Ok(()) => {
            // Cancelled after its last command: what ran, ran.
            let why = lease::cancelled_text(lease::cancel_requested(&work_dir, id).as_ref());
            out.say(&format!("✗ {why}, after its last step"));
            result.error = Some(why);
            EXIT_CANCELLED
        }
        Err((code, why)) => {
            let (code, why) = if cancelled {
                let who = lease::cancelled_text(lease::cancel_requested(&work_dir, id).as_ref());
                (EXIT_CANCELLED, format!("{who}: {why}"))
            } else {
                (code, why)
            };
            out.say(&format!("✗ {why}"));
            result.error = Some(why);
            code
        }
    };
    watcher.stop();
    result.ok = code == EXIT_DONE;
    result.exit_code = code;
    let secs = t0.elapsed().as_secs_f64();
    match code {
        EXIT_DONE => out.say(&format!("✓ done in {secs:.1} s")),
        EXIT_CANCELLED => out.say(&format!(
            "✗ cancelled after {secs:.1} s ({})",
            cancelled_steps(&result.steps)
        )),
        _ => out.say(&format!("job ended with code {code} after {secs:.1} s")),
    }
    let _ = stack_core::jobs::write_atomic(
        &rec.result,
        &serde_json::to_string_pretty(&result).expect("result serializes"),
    );
    let status = match code {
        EXIT_DONE => JobStatus::Done,
        EXIT_CANCELLED => JobStatus::Cancelled,
        _ => JobStatus::Failed,
    };
    end_record(&work_dir, id, status, code, result.error.clone());
    let _ = std::fs::remove_file(lease::cancel_path(&work_dir, id));
    code
}

/// How far a cancelled job's steps got, for its last line: "step 2/3
/// stopped, 1 skipped".
fn cancelled_steps(steps: &[StepOutcome]) -> String {
    let n = steps.len();
    let skipped = steps.iter().filter(|s| s.status == "skipped").count();
    match steps.iter().position(|s| s.status == "failed") {
        Some(i) => format!("step {}/{n} stopped, {skipped} skipped", i + 1),
        None if skipped > 0 => format!("{skipped} of {n} steps skipped"),
        None if n == 0 => "before its first step".to_string(),
        None => "after its last step".to_string(),
    }
}

/// The record's end: status, exit code, end time, the error.
fn end_record(work_dir: &Path, id: &str, status: JobStatus, code: i32, error: Option<String>) {
    let ended = utc_now();
    let _ = JobRecord::update(work_dir, id, |r| {
        r.status = status;
        r.exit_code = Some(code);
        r.ended_utc = Some(ended.clone());
        r.error = error.clone();
    });
}

/// The thread that turns a cancel request into [`Cancel::set`].
struct Watcher {
    done: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Watcher {
    fn stop(mut self) {
        self.done.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn watch_for_cancel(work_dir: &Path, id: &str, cancel: &Cancel) -> Watcher {
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let path = lease::cancel_path(work_dir, id);
    let (flag, cancel) = (done.clone(), cancel.clone());
    let thread = thread::Builder::new()
        .name("cancel watch".into())
        .spawn(move || {
            while !flag.load(std::sync::atomic::Ordering::SeqCst) {
                if path.exists() {
                    cancel.set();
                    return;
                }
                thread::sleep(CANCEL_POLL);
            }
        })
        .ok();
    Watcher { done, thread }
}

type Failure = (i32, String);

fn execute(
    world: &World,
    project_dir: &Path,
    work_dir: &Path,
    rec: &JobRecord,
    out: &mut Out<'_>,
    result: &mut JobResult,
) -> Result<(), Failure> {
    let mut loaded = Loaded::load(project_dir, Some(&rec.stack), rec.slot)
        .map_err(|e| (EXIT_REFUSED, format!("cannot load the stack: {e}")))?;
    if let Some(trusted) = &rec.trusted_definition {
        if loaded.definition_hash.as_ref() != Some(trusted) {
            return Err((EXIT_REFUSED, crate::server::UNTRUSTED_JOB.to_string()));
        }
    }
    if let JobWork::Workspace { op } = &rec.work {
        if let WorkspaceOp::Archive {
            save: Some(spec), ..
        } = op
        {
            auto_save(world, project_dir, rec, spec, out)?;
        }
        let done = crate::workspaces::run_op(world, &loaded, op, &mut |l| out.say(l));
        // A worktree that now exists is scanned before anyone works in it.
        let opened = match op {
            WorkspaceOp::StartWork { item, .. }
            | WorkspaceOp::AddRepo { item, .. }
            | WorkspaceOp::Restore { item } => Some(item.as_str()),
            _ => None,
        };
        if let (Ok(()), Some(item)) = (&done, opened) {
            secret_scan::scan_item(world, &loaded, item, &mut |l| out.say(l));
        }
        return done;
    }
    if let JobWork::SaveState { spec } = &rec.work {
        return save_now(world, project_dir, rec, spec, out).map(|_| ());
    }
    // A review job is git first; with `up`, the slot is brought up from
    // the review worktree after, planned now that the worktree exists.
    let mut review_up: Option<(Change, Option<SaveSpec>)> = None;
    if let JobWork::Review { op } = &rec.work {
        match reviews::run_op(world, &loaded, op, &mut |l| out.say(l))? {
            None => return Ok(()),
            Some(review) => {
                let change = reviews::bring_up_change(&loaded, &review).ok_or_else(|| {
                    (
                        EXIT_FAILED,
                        format!("no component builds or runs from {}", review.clone),
                    )
                })?;
                let save = match op {
                    ReviewOp::Create { save, .. } => save.clone(),
                    ReviewOp::Remove { .. } => None,
                };
                review_up = Some((change, save));
            }
        }
    }
    let resume: Option<&ResumeWork> = match &rec.work {
        JobWork::Resume { resume } => Some(resume),
        _ => None,
    };
    // Everything that runs a plan: the change, the plan confirmed (none
    // for a bring-up planned here), the automatic save, `down -v`.
    let planned: Option<(Change, Option<Plan>, Option<SaveSpec>, bool)> = match &rec.work {
        JobWork::Plan {
            change,
            plan,
            save,
            remove_volumes,
        } => Some((
            change.clone(),
            Some((**plan).clone()),
            save.clone(),
            *remove_volumes,
        )),
        JobWork::Resume { resume } => Some((
            resume.change.clone(),
            Some(resume.plan.clone()),
            resume.save.clone(),
            false,
        )),
        JobWork::Review { .. } => review_up.map(|(c, s)| (c, None, s, false)),
        _ => None,
    };
    if let Some((_, _, Some(spec), _)) = &planned {
        auto_save(world, project_dir, rec, spec, out)?;
    }
    if let Some(r) = resume {
        checkout_worktrees(world, &loaded, r, out)?;
    }
    // The item (or review) the plan builds from, and whether the slot is
    // bound to it as the job's first step.
    let (binds, writes_active): (Option<String>, bool) = match planned.as_ref().map(|p| &p.0) {
        Some(Change::Bind { item, .. }) => (Some(item.clone()), true),
        Some(Change::Resume {
            item: Some(item), ..
        }) => (Some(item.clone()), resume.is_none_or(|r| r.bind)),
        _ => (None, false),
    };
    if let Some(item) = &binds {
        loaded.bind_item(Some(item));
        // Activating an item points agents and builds at its worktrees:
        // scan them first.
        secret_scan::scan_item(world, &loaded, item, &mut |l| out.say(l));
    }
    let now = utc_now();
    let runner = loaded.runner(world, &now);
    let (inst, _) = runner
        .instance(rec.slot)
        .map_err(|e| (EXIT_REFUSED, e.to_string()))?;
    let mut the_plan: Option<Plan> = None;
    // Recipe components the plan starts fresh whose data the bound item
    // asks state seeds for (a resume replays its own recipes instead).
    let mut seeded: Vec<(String, Option<RecipeRecord>)> = Vec::new();
    let steps: Vec<JobStep> = match &planned {
        Some((change, stored, _, remove_volumes)) => {
            out.say(
                if stored.as_ref().is_some_and(|p| {
                    p.steps
                        .iter()
                        .any(|s| matches!(s, Step::Pull { .. } | Step::Build { .. }))
                }) {
                    "resolving images…"
                } else {
                    "looking at the stack…"
                },
            );
            let table = ProcessTable::load(work_dir, &inst.id);
            let mut obs = runner
                .observe(&inst, &table.statuses(&*world.procs))
                .map_err(|e| (EXIT_REFUSED, e.to_string()))?;
            if writes_active {
                if let Some(item) = &binds {
                    obs.intent.item = Some(item.clone());
                }
            }
            for note in &obs.notes {
                out.say(&format!("note: {note}"));
            }
            let plan = match stored {
                Some(p) => p.clone(),
                None => runner
                    .plan(&obs, change)
                    .map_err(|e| (EXIT_REFUSED, format!("cannot plan: {e}")))?,
            };
            if resume.is_none() {
                seeded = seeded_by_item(&loaded, work_dir, &plan, &obs);
            }
            let starts = plan
                .steps
                .iter()
                .any(|s| matches!(s, Step::Start { .. } | Step::Recreate { .. }));
            if starts {
                let mut checker = EngineChecker::new(
                    &*world.cli,
                    &loaded.stack_name,
                    &loaded.stack,
                    &inst,
                    work_dir,
                    &*world.ports,
                );
                checker.hosts_text = world.hosts_text.clone();
                let checks = runner.checks(&inst, Some(&obs), Some(&plan), &checker);
                let mut blocked = Vec::new();
                for c in &checks {
                    let name = c.check.key();
                    match &c.outcome {
                        Outcome::Pass => out.say(&format!("check {name}: ok")),
                        Outcome::Skipped { reason } => {
                            out.say(&format!("check {name}: skipped ({reason})"))
                        }
                        Outcome::Fail { reason, fix } => {
                            let hard = !matches!(c.check, Check::HostnameResolves { .. });
                            out.say(&format!(
                                "check {name}: {} {reason}",
                                if hard { "FAILED" } else { "warning:" }
                            ));
                            if let Some(fix) = fix {
                                out.say(&format!("  fix: {}", fix.join(" ")));
                            }
                            if hard {
                                blocked.push(format!("{name}: {reason}"));
                            }
                        }
                    }
                }
                if !blocked.is_empty() {
                    return Err((
                        EXIT_REFUSED,
                        format!(
                            "not started, a check before up failed ({})",
                            blocked.join("; ")
                        ),
                    ));
                }
            }
            // Secrets in a build context end up in the image: scan each
            // context before anything runs (under `block-build` a finding
            // stops the job here), and what the stack hands to components.
            secret_scan::gate_builds(world, &loaded, &runner, &plan, &mut |l| out.say(l))
                .map_err(|e| (EXIT_FAILED, e))?;
            if starts || plan.steps.iter().any(|s| matches!(s, Step::Build { .. })) {
                secret_scan::scan_env_for(world, &loaded, &runner, &inst, &mut |l| out.say(l));
            }
            let options = ApplyOptions {
                remove_volumes: *remove_volumes,
            };
            let applied = runner
                .apply(&inst, &obs, &plan, &options)
                .map_err(|e| match e {
                    RunnerError::Plan(PlanError::StateChanged { .. }) => (
                        EXIT_REFUSED,
                        "the stack changed since this plan was confirmed; nothing was run. Look again and confirm the new plan"
                            .to_string(),
                    ),
                    other => (EXIT_REFUSED, format!("cannot apply the plan: {other}")),
                })?;
            for note in &applied.notes {
                out.say(&format!("note: {note}"));
                result.notes.push(note.clone());
            }
            for (c, ci) in &applied.intent.components {
                result.components.insert(
                    c.clone(),
                    serde_json::to_value(ci).unwrap_or(serde_json::Value::Null),
                );
            }
            let mut steps = applied.steps;
            if let (Some(item), true) = (&binds, writes_active) {
                let gate = loaded.gate().map_err(|e| (EXIT_REFUSED, e))?;
                let mut active =
                    ActiveRecord::load(&gate).map_err(|e| (EXIT_REFUSED, e.to_string()))?;
                active.set(rec.slot, item, &inst.id, &now);
                steps.insert(0, active.save_step(&gate));
            }
            the_plan = Some(plan);
            steps
        }
        None => match &rec.work {
            JobWork::Restart { component } => {
                let run = runner.restart(&inst, component).ok_or_else(|| {
                (
                    EXIT_REFUSED,
                    format!("{component} is not a compose component; restart it by stopping and starting it"),
                )
            })?;
                vec![JobStep::run(component, run)]
            }
            JobWork::DataSafety {
                components,
                trigger,
            } => {
                return data_safety_job(
                    world,
                    &loaded,
                    &runner,
                    &inst,
                    work_dir,
                    rec,
                    components,
                    Trigger::parse(trigger),
                    out,
                    result,
                );
            }
            JobWork::Gc => {
                out.say("listing images…");
                let plans = runner
                    .gc_plan(&[])
                    .map_err(|e| (EXIT_REFUSED, format!("cannot list images: {e}")))?;
                for p in &plans {
                    out.say(&p.summary());
                    for r in &p.remove {
                        out.say(&format!("  remove {} ({})", r.reference, r.reason));
                    }
                    for k in &p.kept {
                        out.say(&format!("  keep {} ({})", k.reference, k.reason));
                    }
                }
                runner.gc_apply(&plans)
            }
            _ => return Ok(()),
        },
    };
    out.say(&format!("{} step(s):", steps.len()));
    for s in &steps {
        out.say(&format!("  · {}", s.describe()));
    }
    let forget_processes = the_plan.as_ref().is_some_and(|p| p.verb == Verb::Down);
    // Data-safety checks after up: under `block-up` right after each start
    // step (a verdict that is not clean ends the job there), otherwise once
    // the steps are done.
    // A resume checks what it restores or rebuilds itself, after that.
    let mut restored: Vec<&str> = resume
        .map(|r| {
            r.restores
                .iter()
                .chain(&r.recipes)
                .map(String::as_str)
                .collect()
        })
        .unwrap_or_default();
    restored.extend(seeded.iter().map(|(c, _)| c.as_str()));
    let (gated, after_up): (Vec<String>, Vec<String>) = match &the_plan {
        Some(plan) => {
            let started: Vec<&str> = plan
                .steps
                .iter()
                .filter_map(|s| match s {
                    Step::Start { component } | Step::Recreate { component, .. } => {
                        Some(component.as_str())
                    }
                    _ => None,
                })
                .collect();
            let wanted: Vec<String> = data_safety::components_for(&loaded, ScanWhen::AfterUp)
                .into_iter()
                .filter(|c| started.contains(&c.as_str()) && !restored.contains(&c.as_str()))
                .collect();
            if data_safety::blocks_up(&loaded) {
                (wanted, Vec::new())
            } else {
                (Vec::new(), wanted)
            }
        }
        None => (Vec::new(), Vec::new()),
    };
    let mut started_ok: Vec<String> = Vec::new();
    let mut checked: Vec<(String, CheckEntry)> = Vec::new();
    let mut blocked: Vec<String> = Vec::new();
    let steps_result = {
        let mut gate = |step: &JobStep, out: &mut Out<'_>| -> Option<String> {
            let c = start_of(step)?;
            if !started_ok.iter().any(|s| s == c) {
                started_ok.push(c.to_string());
            }
            if !gated.iter().any(|g| g == c) || checked.iter().any(|(d, _)| d == c) {
                return None;
            }
            let entry = check_now(
                world,
                &loaded,
                &runner,
                &inst,
                work_dir,
                c,
                Trigger::AfterUp,
                &rec.id,
                AFTER_UP_ATTEMPTS,
                out,
            );
            let (state, _) = data_safety::judged(&loaded, &entry);
            checked.push((c.to_string(), entry));
            if matches!(state, SafetyState::NotClean | SafetyState::Error) {
                blocked.push(c.to_string());
                Some(format!(
                    "data safety: {c} is not clean, so up stops before the instance is ready (policy block-up)"
                ))
            } else {
                None
            }
        };
        let merge = the_plan.as_ref().map(|plan| IntentChange {
            plan,
            item: binds.clone().filter(|_| writes_active),
        });
        run_steps(
            world,
            work_dir,
            &inst.id,
            &steps,
            forget_processes,
            merge.as_ref(),
            out,
            result,
            &mut gate,
        )
    };
    for c in &after_up {
        if out.cancel.is_set() {
            break;
        }
        if started_ok.contains(c) && !checked.iter().any(|(d, _)| d == c) {
            let entry = check_now(
                world,
                &loaded,
                &runner,
                &inst,
                work_dir,
                c,
                Trigger::AfterUp,
                &rec.id,
                AFTER_UP_ATTEMPTS,
                out,
            );
            checked.push((c.clone(), entry));
        }
    }
    let mut to_stop = blocked.clone();
    let mut stopped_by_policy = Vec::new();
    for (c, entry) in &checked {
        let (state, policy) = data_safety::judged(&loaded, entry);
        result
            .data_safety
            .insert(c.clone(), data_safety::result_value(entry, state));
        let bad = matches!(state, SafetyState::NotClean | SafetyState::Error);
        if bad && policy == Some(Policy::Stop) && !to_stop.contains(c) {
            to_stop.push(c.clone());
            stopped_by_policy.push(c.clone());
        }
    }
    let stop_result = if to_stop.is_empty() {
        Ok(())
    } else {
        stop_for_data_safety(world, &runner, &inst, work_dir, &to_stop, out, result)
    };
    steps_result?;
    stop_result?;
    if let Some(plan) = &the_plan {
        // A process that was already stopped gets no stop step from `down`;
        // its entry goes with the rest of the instance.
        if forget_processes {
            let mut table = ProcessTable::load(work_dir, &inst.id);
            let before = table.processes.len();
            table.processes.retain(|c, _| {
                !plan
                    .steps
                    .iter()
                    .any(|s| matches!(s, Step::Remove { component } if component == c))
            });
            if table.processes.len() != before {
                table
                    .save(work_dir, &inst.id)
                    .map_err(|e| (EXIT_FAILED, e))?;
            }
        }
    }
    let resume_failures = match resume {
        Some(r) => resume_data(
            world, &loaded, &runner, &inst, work_dir, rec, r, out, result,
        ),
        None => Vec::new(),
    };
    let seed_failures = rebuild_data(
        world,
        &loaded,
        &runner,
        &inst,
        work_dir,
        rec,
        &[],
        &seeded,
        (Trigger::AfterUp, "after up"),
        out,
        result,
    );
    let cache = ProvenanceCache::load(work_dir, &inst.id);
    for (c, p) in &cache.components {
        result.provenance.insert(
            c.clone(),
            serde_json::to_value(p).unwrap_or(serde_json::Value::Null),
        );
    }
    if !resume_failures.is_empty() {
        return Err((
            EXIT_FAILED,
            format!("resumed, except: {}", resume_failures.join("; ")),
        ));
    }
    if !seed_failures.is_empty() {
        return Err((
            EXIT_FAILED,
            format!(
                "the item's state seeds did not all run: {}",
                seed_failures.join("; ")
            ),
        ));
    }
    if !stopped_by_policy.is_empty() {
        return Err((
            EXIT_FAILED,
            format!(
                "data safety: {} not clean, so stopped (policy stop; the volume is kept)",
                stopped_by_policy.join(", ")
            ),
        ));
    }
    Ok(())
}

/// Save `spec` now: the runner bound to its item, the instance observed
/// afresh, snapshot hooks run (with a data-safety check first where the
/// component asks for one before snapshots).
fn save_now(
    world: &World,
    project_dir: &Path,
    rec: &JobRecord,
    spec: &SaveSpec,
    out: &mut Out<'_>,
) -> Result<SavedState, Failure> {
    let mut loaded = Loaded::load(project_dir, Some(&rec.stack), rec.slot)
        .map_err(|e| (EXIT_REFUSED, format!("cannot load the stack: {e}")))?;
    loaded.bind_item(Some(&spec.item));
    let now = utc_now();
    let runner = loaded.runner(world, &now);
    let (inst, _) = runner
        .instance(rec.slot)
        .map_err(|e| (EXIT_REFUSED, e.to_string()))?;
    let work_dir = loaded.work_dir();
    let table = ProcessTable::load(&work_dir, &inst.id);
    let obs = runner
        .observe(&inst, &table.statuses(&*world.procs))
        .map_err(|e| (EXIT_FAILED, format!("cannot look at the stack: {e}")))?;
    let cx = states::Ctx {
        world,
        loaded: &loaded,
        runner: &runner,
        inst: &inst,
        obs: &obs,
    };
    let out = RefCell::new(out);
    let mut check = |c: &str, trigger: Trigger| {
        let mut o = out.borrow_mut();
        check_now(
            world, &loaded, &runner, &inst, &work_dir, c, trigger, &rec.id, 1, &mut o,
        );
    };
    let mut say = |l: &str| out.borrow_mut().say(l);
    states::save_state(
        &cx,
        spec,
        Some(&rec.id),
        rec.actor.as_deref(),
        &mut check,
        &mut say,
    )
    .map_err(|e| (EXIT_FAILED, e))
}

/// The automatic save before a destructive change; its failure ends the
/// job before anything changed.
fn auto_save(
    world: &World,
    project_dir: &Path,
    rec: &JobRecord,
    spec: &SaveSpec,
    out: &mut Out<'_>,
) -> Result<(), Failure> {
    save_now(world, project_dir, rec, spec, out)
        .map(|_| ())
        .map_err(|(_, why)| {
            (
                EXIT_FAILED,
                format!(
                    "the stack state of {} could not be saved first ({why}); nothing else was done. To go ahead anyway, use the action's \"without saving\" variant",
                    spec.item
                ),
            )
        })
}

/// Put each worktree a resume builds from on its saved commit; refused,
/// with nothing moved, when any has uncommitted changes now.
fn checkout_worktrees(
    world: &World,
    loaded: &Loaded,
    r: &ResumeWork,
    out: &mut Out<'_>,
) -> Result<(), Failure> {
    let git = Git::new(&*world.git_cli, loaded);
    for c in &r.checkouts {
        match git.uncommitted(&c.worktree) {
            Ok(0) => {}
            Ok(n) => {
                return Err((
                    EXIT_REFUSED,
                    format!(
                    "{} has {n} uncommitted change(s) now; nothing was done. Commit or stash first",
                    c.worktree.display()
                ),
                ))
            }
            Err(e) => return Err((EXIT_REFUSED, e)),
        }
    }
    for c in &r.checkouts {
        out.say(&format!("▶ {}: worktree to {}", c.repo, c8(&c.commit)));
        git.checkout_at(&c.worktree, &c.commit, c.branch.as_deref())
            .map_err(|e| (EXIT_FAILED, format!("{}: {e}", c.repo)))?;
    }
    Ok(())
}

/// After a resume's plan ran: each snapshot checked with `exists` and
/// restored, then checked (`after-restore`); each recipe replayed. A
/// component that fails is listed; the others go on.
#[allow(clippy::too_many_arguments)]
fn resume_data(
    world: &World,
    loaded: &Loaded,
    runner: &StackRunner<'_>,
    inst: &ResolvedInstance,
    work_dir: &Path,
    rec: &JobRecord,
    r: &ResumeWork,
    out: &mut Out<'_>,
    result: &mut JobResult,
) -> Vec<String> {
    let mut failures = Vec::new();
    for (c, why) in &r.blocked {
        out.say(&format!("· {c}: not resumed ({why})"));
    }
    if r.restores.is_empty() && r.recipes.is_empty() {
        return failures;
    }
    let item_dir = ItemRecord::dir(work_dir, &r.item);
    let state = match SavedState::load(&item_dir, &r.state) {
        Ok(Some(s)) => s,
        Ok(None) => return vec![format!("the saved state {} is gone", r.state)],
        Err(e) => return vec![e],
    };
    let snapshots: Vec<(String, DataSnapshot)> = r
        .restores
        .iter()
        .filter_map(|c| {
            let snap = state.data.get(c).and_then(|d| d.snapshot.clone())?;
            Some((c.clone(), snap))
        })
        .collect();
    let recipes: Vec<(String, Option<RecipeRecord>)> = r
        .recipes
        .iter()
        .map(|c| (c.clone(), state.data.get(c).and_then(|d| d.recipe.clone())))
        .collect();
    failures.extend(rebuild_data(
        world,
        loaded,
        runner,
        inst,
        work_dir,
        rec,
        &snapshots,
        &recipes,
        (Trigger::AfterRestore, "after the resume"),
        out,
        result,
    ));
    failures
}

/// The recipe components a plan creates or recreates whose data the
/// bound work item asks state seeds for, each with the seeds it asks for.
/// A fresh container starts from the image's data, so its recipe is
/// replayed with those seeds once the steps ran.
fn seeded_by_item(
    loaded: &Loaded,
    work_dir: &Path,
    plan: &Plan,
    obs: &Observation,
) -> Vec<(String, Option<RecipeRecord>)> {
    let Some(item) = states::bound_item(loaded) else {
        return Vec::new();
    };
    let Ok(Some(rec)) = ItemRecord::load(&ItemRecord::dir(work_dir, item)) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (c, def) in &loaded.stack.components {
        let Some(d) = def
            .data
            .as_ref()
            .filter(|d| d.strategy == DataStrategy::Recipe)
        else {
            continue;
        };
        let seeds: Vec<SeedChoice> = rec
            .data_seeds
            .get(c)
            .into_iter()
            .flatten()
            .filter(|s| d.state_seeds.contains_key(&s.name))
            .cloned()
            .collect();
        if seeds.is_empty() {
            continue;
        }
        let fresh = plan.steps.iter().any(|s| match s {
            Step::Recreate { component, .. } => component == c,
            Step::Start { component } => component == c && !obs.state.component(c).exists,
            _ => false,
        });
        if fresh {
            out.push((
                c.clone(),
                Some(RecipeRecord {
                    base_image: None,
                    scripts: Vec::new(),
                    seeds,
                }),
            ));
        }
    }
    out
}

/// Bring data back after the steps ran: each snapshot checked with
/// `exists` and restored, then checked; each recipe replayed (scrub,
/// check, seed, state seeds, check). A component that fails is listed;
/// the others go on. A component whose check comes out not clean under
/// `stop` / `block-up` is stopped, its volume kept. `when` names the
/// check's trigger and the moment, for the failure line.
#[allow(clippy::too_many_arguments)]
fn rebuild_data(
    world: &World,
    loaded: &Loaded,
    runner: &StackRunner<'_>,
    inst: &ResolvedInstance,
    work_dir: &Path,
    rec: &JobRecord,
    snapshots: &[(String, DataSnapshot)],
    recipes: &[(String, Option<RecipeRecord>)],
    when: (Trigger, &str),
    out: &mut Out<'_>,
    result: &mut JobResult,
) -> Vec<String> {
    let mut failures = Vec::new();
    if snapshots.is_empty() && recipes.is_empty() {
        return failures;
    }
    let (trigger, moment) = when;
    let table = ProcessTable::load(work_dir, &inst.id);
    let obs = match runner.observe(inst, &table.statuses(&*world.procs)) {
        Ok(o) => o,
        Err(e) => return vec![format!("cannot look at the stack: {e}")],
    };
    let cx = states::Ctx {
        world,
        loaded,
        runner,
        inst,
        obs: &obs,
    };
    let checked: RefCell<Vec<(String, CheckEntry)>> = RefCell::new(Vec::new());
    let out = RefCell::new(out);
    let check = |c: &str| -> Option<bool> {
        let def = loaded.stack.components.get(c)?;
        if !data_safety::has_check(def) {
            return None;
        }
        let entry = {
            let mut o = out.borrow_mut();
            check_now(
                world,
                loaded,
                runner,
                inst,
                work_dir,
                c,
                trigger,
                &rec.id,
                AFTER_UP_ATTEMPTS,
                &mut o,
            )
        };
        let (st, _) = data_safety::judged(loaded, &entry);
        checked.borrow_mut().push((c.to_string(), entry));
        Some(st == SafetyState::Clean)
    };
    for (c, snap) in snapshots {
        let snap = snap.clone();
        let why = match states::snapshot_exists(world, loaded, inst, &obs, &snap) {
            Exists::Present => None,
            Exists::Missing => Some(format!(
                "{c}: snapshot {} is missing (expected in {}); not restored",
                snap.name, snap.where_
            )),
            Exists::Unknown(e) => Some(format!(
                "{c}: snapshot {} cannot be checked ({e}); not restored",
                snap.name
            )),
        };
        if let Some(why) = why {
            out.borrow_mut().say(&format!("✗ {why}"));
            failures.push(why);
            continue;
        }
        let restored = states::restore_snapshot(&cx, &snap, &mut |l| out.borrow_mut().say(l));
        match restored {
            Ok(()) => {
                out.borrow_mut()
                    .say(&format!("✓ {c}: restored {}", snap.name));
                check(c);
            }
            Err(e) => {
                out.borrow_mut().say(&format!("✗ {c}: {e}"));
                failures.push(format!("{c}: {e}"));
            }
        }
    }
    for (c, record) in recipes {
        let record = record.as_ref();
        let mut gate = |x: &str| check(x);
        let replayed =
            states::replay_recipe(&cx, c, record, &mut gate, &mut |l| out.borrow_mut().say(l));
        match replayed {
            Ok(()) => out
                .borrow_mut()
                .say(&format!("✓ {c}: data rebuilt from its recipe")),
            Err(e) => {
                out.borrow_mut().say(&format!("✗ {e}"));
                failures.push(e);
            }
        }
    }
    let out = out.into_inner();
    let mut to_stop = Vec::new();
    for (c, entry) in checked.into_inner() {
        let (st, policy) = data_safety::judged(loaded, &entry);
        result
            .data_safety
            .insert(c.clone(), data_safety::result_value(&entry, st));
        if matches!(st, SafetyState::NotClean | SafetyState::Error)
            && matches!(policy, Some(Policy::Stop | Policy::BlockUp))
            && !to_stop.contains(&c)
        {
            to_stop.push(c);
        }
    }
    if !to_stop.is_empty() {
        if let Err((_, e)) =
            stop_for_data_safety(world, runner, inst, work_dir, &to_stop, out, result)
        {
            failures.push(e);
        }
        failures.push(format!(
            "data safety: {} not clean {moment}, so stopped (the volume is kept)",
            to_stop.join(", ")
        ));
    }
    failures
}

/// The component a run step starts, if it is a start (`compose up` of a
/// container, or a process component's start).
fn start_of(step: &JobStep) -> Option<&str> {
    match step {
        JobStep::Run { run, component }
            if run.env.contains_key(INSTANCE_ENV) || StepKind::of(run) == StepKind::ComposeUp =>
        {
            Some(component)
        }
        JobStep::Script {
            component, verb, ..
        } if verb.starts() => Some(component),
        _ => None,
    }
}

/// Check one component now and record the verdict, saying what came of it
/// (never what a hook printed).
#[allow(clippy::too_many_arguments)]
fn check_now(
    world: &World,
    loaded: &Loaded,
    runner: &StackRunner<'_>,
    inst: &ResolvedInstance,
    work_dir: &Path,
    component: &str,
    trigger: Trigger,
    job_id: &str,
    attempts: u32,
    out: &mut Out<'_>,
) -> CheckEntry {
    out.say(&format!(
        "data safety: checking {component} ({})",
        trigger.as_str()
    ));
    let table = ProcessTable::load(work_dir, &inst.id);
    let obs = runner.observe(inst, &table.statuses(&*world.procs)).ok();
    let run = data_safety::run_check(
        world,
        loaded,
        inst,
        obs.as_ref(),
        component,
        attempts,
        &mut |l| out.say(l),
    );
    let now = world.clock.now_utc();
    let entry = match data_safety::record(
        work_dir,
        &inst.id,
        component,
        &run,
        trigger,
        Some(job_id),
        &now,
    ) {
        Ok(e) => e,
        Err(e) => {
            out.say(&format!("  cannot record the verdict: {e}"));
            CheckEntry {
                scanned_utc: now,
                trigger: trigger.as_str().to_string(),
                job_id: Some(job_id.to_string()),
                verdict: run.verdict.clone(),
                error: run.error.clone(),
                ack: None,
            }
        }
    };
    match (&entry.verdict, &entry.error) {
        (Some(v), _) => {
            let mut line = format!(
                "{} {component}: {}",
                if v.clean { "✓" } else { "✗" },
                data_safety::verdict_line(v)
            );
            if let Some(a) = &entry.ack {
                line.push_str(&format!(" (still acknowledged by {})", a.who));
            }
            out.say(&line);
        }
        (None, e) => out.say(&format!(
            "✗ {component}: the check could not be judged: {}",
            e.as_deref().unwrap_or("no verdict")
        )),
    }
    entry
}

/// Stop components whose data is not clean, through the planner, keeping
/// their volumes.
fn stop_for_data_safety(
    world: &World,
    runner: &StackRunner<'_>,
    inst: &ResolvedInstance,
    work_dir: &Path,
    components: &[String],
    out: &mut Out<'_>,
    result: &mut JobResult,
) -> Result<(), Failure> {
    out.say(&format!(
        "data safety: stopping {} (the volume is kept)",
        components.join(", ")
    ));
    let table = ProcessTable::load(work_dir, &inst.id);
    let obs = runner
        .observe(inst, &table.statuses(&*world.procs))
        .map_err(|e| {
            (
                EXIT_FAILED,
                format!("cannot look at the stack to stop it: {e}"),
            )
        })?;
    let change = Change::StopComponents {
        components: components.to_vec(),
    };
    let plan = runner
        .plan(&obs, &change)
        .map_err(|e| (EXIT_FAILED, format!("cannot plan the stop: {e}")))?;
    let applied = runner
        .apply(inst, &obs, &plan, &ApplyOptions::default())
        .map_err(|e| (EXIT_FAILED, format!("cannot apply the stop: {e}")))?;
    for s in &applied.steps {
        out.say(&format!("  · {}", s.describe()));
    }
    let merge = IntentChange {
        plan: &plan,
        item: None,
    };
    run_steps(
        world,
        work_dir,
        &inst.id,
        &applied.steps,
        false,
        Some(&merge),
        out,
        result,
        &mut |_, _| None,
    )
}

/// A `data-safety-check` job: check each named component that runs,
/// record the verdicts, and stop the ones `stop` (or `block-up`, which
/// outside `up` means the same) says must not stay up. Fails when any is
/// not clean.
#[allow(clippy::too_many_arguments)]
fn data_safety_job(
    world: &World,
    loaded: &Loaded,
    runner: &StackRunner<'_>,
    inst: &ResolvedInstance,
    work_dir: &Path,
    rec: &JobRecord,
    components: &[String],
    trigger: Trigger,
    out: &mut Out<'_>,
    result: &mut JobResult,
) -> Result<(), Failure> {
    let table = ProcessTable::load(work_dir, &inst.id);
    let obs = runner
        .observe(inst, &table.statuses(&*world.procs))
        .map_err(|e| (EXIT_REFUSED, e.to_string()))?;
    let mut checked = Vec::new();
    for c in components {
        let declared = loaded
            .stack
            .components
            .get(c)
            .is_some_and(data_safety::has_check);
        if !declared {
            out.say(&format!("· {c}: declares no data-safety check"));
            continue;
        }
        if !obs.actual.get(c).is_some_and(|a| a.running) {
            out.say(&format!("· {c}: not running; nothing to check"));
            continue;
        }
        let entry = check_now(
            world, loaded, runner, inst, work_dir, c, trigger, &rec.id, 1, out,
        );
        checked.push((c.clone(), entry));
    }
    let mut to_stop = Vec::new();
    let mut bad = Vec::new();
    for (c, entry) in &checked {
        let (state, policy) = data_safety::judged(loaded, entry);
        result
            .data_safety
            .insert(c.clone(), data_safety::result_value(entry, state));
        if matches!(state, SafetyState::NotClean | SafetyState::Error) {
            bad.push(c.clone());
            if matches!(policy, Some(Policy::Stop | Policy::BlockUp)) {
                to_stop.push(c.clone());
            }
        }
    }
    if !to_stop.is_empty() {
        stop_for_data_safety(world, runner, inst, work_dir, &to_stop, out, result)?;
    }
    if !bad.is_empty() {
        return Err((
            EXIT_FAILED,
            format!("data safety: {} not clean", bad.join(", ")),
        ));
    }
    Ok(())
}

fn is_intent_write(step: &JobStep) -> bool {
    matches!(step, JobStep::Local { step: LocalStep::WriteFile { what, .. } } if what == "intent record")
}

/// The change a plan makes to the intent record, applied to the record as
/// it is when the step runs (not to the copy read when the job started).
struct IntentChange<'a> {
    plan: &'a Plan,
    /// The work item the job binds the slot to, when it binds one.
    item: Option<String>,
}

impl IntentChange<'_> {
    /// Read the instance's intent record, apply this change, write it. With
    /// no record on disk (or one that cannot be read), the step's own copy
    /// is written as planned.
    fn write(&self, work_dir: &Path, instance: &str, planned: &str) -> std::io::Result<()> {
        let path = IntentRecord::path(work_dir, instance);
        let current = IntentRecord::load(work_dir, instance).ok().flatten();
        let text = match current {
            Some(mut rec) => {
                let when = serde_json::from_str::<IntentRecord>(planned)
                    .map(|p| p.updated_utc)
                    .unwrap_or_else(|_| utc_now());
                rec.record_plan(self.plan, &when);
                if let Some(item) = &self.item {
                    rec.item = Some(item.clone());
                }
                serde_json::to_string_pretty(&rec).expect("intent serializes")
            }
            None => planned.to_string(),
        };
        stack_core::jobs::write_atomic(&path, &text)
    }
}

/// Run steps in order. After each step that succeeded, `gate` may end the
/// run with a reason (the remaining steps are then skipped like after a
/// failure). A cancel skips the rest the same way.
#[allow(clippy::too_many_arguments)]
fn run_steps(
    world: &World,
    work_dir: &Path,
    instance: &str,
    steps: &[JobStep],
    forget_processes: bool,
    merge: Option<&IntentChange<'_>>,
    out: &mut Out<'_>,
    result: &mut JobResult,
    gate: &mut dyn FnMut(&JobStep, &mut Out<'_>) -> Option<String>,
) -> Result<(), Failure> {
    let mut failure: Option<String> = None;
    for (at, step) in steps.iter().enumerate() {
        if failure.is_none() && out.cancel.is_set() {
            failure = Some("cancelled before this step".to_string());
        }
        if failure.is_some() && !is_intent_write(step) {
            result.steps.push(StepOutcome {
                step: step.describe(),
                status: "skipped".into(),
                exit_code: None,
                seconds: 0.0,
                error: None,
            });
            continue;
        }
        let t = Instant::now();
        let what = step.describe();
        out.step(Some((at + 1, steps.len(), &what)));
        let outcome = run_step(
            world,
            work_dir,
            instance,
            step,
            forget_processes,
            merge,
            out,
        );
        out.step(None);
        let seconds = (t.elapsed().as_secs_f64() * 10.0).round() / 10.0;
        match outcome {
            Ok(code) => {
                result.steps.push(StepOutcome {
                    step: step.describe(),
                    status: "done".into(),
                    exit_code: code,
                    seconds,
                    error: None,
                });
                if let Some(why) = gate(step, out) {
                    out.say(&format!("✗ {why}"));
                    if failure.is_none() {
                        failure = Some(why);
                    }
                }
            }
            Err((code, why)) => {
                out.say(&format!("✗ {}: {why}", step.describe()));
                result.steps.push(StepOutcome {
                    step: step.describe(),
                    status: "failed".into(),
                    exit_code: code,
                    seconds,
                    error: Some(why.clone()),
                });
                if failure.is_none() {
                    failure = Some(format!("{}: {why}", step.describe()));
                }
            }
        }
    }
    match failure {
        Some(why) => Err((EXIT_FAILED, why)),
        None => Ok(()),
    }
}

/// One step; `Ok(exit code)` for commands, `Ok(None)` for the rest.
fn run_step(
    world: &World,
    work_dir: &Path,
    instance: &str,
    step: &JobStep,
    forget_processes: bool,
    merge: Option<&IntentChange<'_>>,
    out: &mut Out<'_>,
) -> Result<Option<i32>, (Option<i32>, String)> {
    match step {
        JobStep::Local {
            step: local @ LocalStep::WriteFile { contents, .. },
        } if merge.is_some() && is_intent_write(step) => {
            merge
                .expect("checked")
                .write(work_dir, instance, contents)
                .map_err(|e| (None, e.to_string()))?;
            out.say(&format!("✓ {}", local.describe()));
            Ok(None)
        }
        JobStep::Local { step: local } => {
            local.perform().map_err(|e| (None, e.to_string()))?;
            match local {
                LocalStep::WriteFile { .. } | LocalStep::WriteGate { .. } => {
                    out.say(&format!("✓ {}", local.describe()))
                }
                LocalStep::Note { text } => out.say(&format!("· {text}")),
            }
            Ok(None)
        }
        JobStep::Run { run, component } if run.env.contains_key(INSTANCE_ENV) => {
            start_process(world, work_dir, instance, component, run, out)?;
            Ok(None)
        }
        JobStep::Run { run, component } => {
            let kind = StepKind::of(run);
            out.say(&format!(
                "▶ {component}: {}",
                run.title.clone().unwrap_or_else(|| run.command_line())
            ));
            out.say(&format!("  $ {}", run.command_line()));
            let cancel = out.cancel.clone();
            let code = world
                .steps
                .run(run, kind.limit(), &cancel, &mut |l| {
                    out.say(&format!("  {l}"))
                })
                .map_err(|e| (None, e))?;
            if code == 0 {
                Ok(Some(0))
            } else {
                Err((Some(code), format!("exited with code {code}")))
            }
        }
        JobStep::Script {
            run,
            component,
            verb,
            timeout_seconds,
        } => run_script(
            world,
            work_dir,
            instance,
            run,
            component,
            *verb,
            *timeout_seconds,
            out,
        ),
        JobStep::StopProcess {
            component,
            grace_seconds,
        } => {
            let mut table = ProcessTable::load(work_dir, instance);
            match table.processes.get(component).cloned() {
                Some(entry) if entry.stopped_utc.is_none() || forget_processes => {
                    out.say(&format!(
                        "▶ {component}: stop process {} ({grace_seconds} s grace)",
                        entry.pid
                    ));
                    if world.procs.alive(entry.pid) {
                        world
                            .procs
                            .stop(entry.pid, Duration::from_secs(u64::from(*grace_seconds)))
                            .map_err(|e| (None, e))?;
                    }
                    if forget_processes {
                        table.processes.shift_remove(component);
                    } else if let Some(e) = table.processes.get_mut(component) {
                        e.stopped_utc = Some(utc_now());
                    }
                    table.save(work_dir, instance).map_err(|e| (None, e))?;
                }
                _ => out.say(&format!("· {component}: no process is running")),
            }
            Ok(None)
        }
    }
}

/// Run one script of a script component under its limit, its output
/// streamed, and record how it went for the component's state.
#[allow(clippy::too_many_arguments)]
fn run_script(
    world: &World,
    work_dir: &Path,
    instance: &str,
    run: &RunBlock,
    component: &str,
    verb: ScriptVerb,
    timeout_seconds: u64,
    out: &mut Out<'_>,
) -> Result<Option<i32>, (Option<i32>, String)> {
    let limit = Duration::from_secs(timeout_seconds.max(1));
    out.say(&format!(
        "▶ {component}: {verb} script (limit {})",
        timeouts::describe(limit)
    ));
    out.say(&format!(
        "  $ {}  (in {})",
        run.command_line(),
        run.cwd.display()
    ));
    let cancel = out.cancel.clone();
    let outcome = world
        .steps
        .run_tree(run, limit, &cancel, &mut |l| out.say(&format!("  {l}")));
    let (result, last) = match outcome {
        Ok(0) => (Ok(Some(0)), (true, Some(0), None)),
        Ok(code) => (
            Err((
                Some(code),
                format!("the {verb} script exited with code {code}"),
            )),
            (false, Some(code), None),
        ),
        Err(e) => {
            let why = format!("the {verb} script: {e}");
            (Err((None, why.clone())), (false, None, Some(why)))
        }
    };
    let (ok, exit_code, error) = last;
    let record = LastRun {
        verb,
        ok,
        exit_code,
        ended_utc: utc_now(),
        error,
    };
    if let Err(e) = ScriptRuns::record(work_dir, instance, component, record) {
        out.say(&format!("  cannot record how the script went: {e}"));
    }
    result
}

/// Start a process component detached, remember it, and make sure it
/// survives its first moments (a dev command that dies at once is a
/// failed step, with the end of its log).
fn start_process(
    world: &World,
    work_dir: &Path,
    instance: &str,
    component: &str,
    run: &RunBlock,
    out: &mut Out<'_>,
) -> Result<(), (Option<i32>, String)> {
    let log = log_path(work_dir, instance, component);
    let title = run.title.clone().unwrap_or_else(|| run.command_line());
    let title = title
        .strip_prefix(&format!("{component}: "))
        .unwrap_or(&title);
    out.say(&format!("▶ {component}: start {title}"));
    out.say(&format!(
        "  $ {}  (in {})",
        run.command_line(),
        run.cwd.display()
    ));
    let mut table = ProcessTable::load(work_dir, instance);
    if let Some(old) = table.processes.get(component) {
        if old.stopped_utc.is_none() && world.procs.alive(old.pid) {
            out.say(&format!("· {component}: already running as {}", old.pid));
            return Ok(());
        }
    }
    let deadline = Instant::now() + timeouts::PROCESS_START;
    let pid = world.procs.spawn(run, &log).map_err(|e| (None, e))?;
    table.processes.insert(
        component.to_string(),
        ProcessEntry {
            pid,
            argv: run.argv.clone(),
            cwd: run.cwd.clone(),
            log: log.clone(),
            started_utc: utc_now(),
            stopped_utc: None,
        },
    );
    table.save(work_dir, instance).map_err(|e| (None, e))?;
    let settle = Instant::now() + world.settle.min(timeouts::PROCESS_START);
    while Instant::now() < settle.min(deadline) {
        if !world.procs.alive(pid) {
            let tail = std::fs::read_to_string(&log)
                .map(|t| {
                    let lines: Vec<&str> = t.lines().collect();
                    lines[lines.len().saturating_sub(10)..].join("\n")
                })
                .unwrap_or_default();
            for l in tail.lines() {
                out.say(&format!("  {l}"));
            }
            return Err((None, "the process exited right after it started".into()));
        }
        thread::sleep(Duration::from_millis(100));
    }
    out.say(&format!(
        "✓ {component}: running as process {pid}, output in {}",
        log.display()
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_intent_write_applies_only_its_plan_to_the_record_as_it_is_then() {
        use stack_core::intent::Desired;
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path();
        // When this job started nothing was recorded; it plans api up.
        let mut plan = Plan {
            instance: "app-0".into(),
            verb: Verb::Up,
            steps: Vec::new(),
            notes: Vec::new(),
            fingerprint: String::new(),
            desired: IndexMap::new(),
            row_desired: IndexMap::new(),
            modes: IndexMap::new(),
            refs: IndexMap::new(),
            channel: None,
            pins: IndexMap::new(),
        };
        plan.desired.insert("api".into(), Desired::Running);
        let mut planned = IntentRecord::new("app", 0, "2026-11-14T12:00:00Z");
        planned.record_plan(&plan, "2026-11-14T12:00:00Z");
        let planned = serde_json::to_string_pretty(&planned).unwrap();
        // Meanwhile another change stopped db.
        let mut moved = IntentRecord::new("app", 0, "2026-11-14T12:00:05Z");
        moved.components.entry("db".into()).or_default().desired = Desired::Stopped;
        moved.components.entry("api".into()).or_default().desired = Desired::Absent;
        moved.save(work).unwrap();
        let change = IntentChange {
            plan: &plan,
            item: Some("ITEM-7".into()),
        };
        change.write(work, "app-0", &planned).unwrap();
        let now = IntentRecord::load(work, "app-0").unwrap().unwrap();
        assert_eq!(now.components["db"].desired, Desired::Stopped, "kept");
        assert_eq!(
            now.components["api"].desired,
            Desired::Running,
            "this job's"
        );
        assert_eq!(now.item.as_deref(), Some("ITEM-7"));
        // With no record on disk, the planned copy is written as it is.
        std::fs::remove_file(IntentRecord::path(work, "app-0")).unwrap();
        change.write(work, "app-0", &planned).unwrap();
        let text = std::fs::read_to_string(IntentRecord::path(work, "app-0")).unwrap();
        assert_eq!(text, planned);
    }

    #[test]
    fn a_job_line_shows_a_generated_value_as_the_mask_in_the_pane_and_the_log() {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join(".work");
        let log_path = tmp.path().join("job.log");
        let mut pane: Vec<u8> = Vec::new();
        let mut out = Out {
            pane: &mut pane,
            log: Some(File::create(&log_path).unwrap()),
            work_dir: work.clone(),
            cancel: Cancel::new(),
            interrupts: None,
        };
        out.say("before any value exists");
        // Generated while the job runs (as an instance resolution does).
        let names = ["db".to_string()].into();
        let v = stack_core::instance_secrets::load_or_create(&work, "app-0", &names)
            .unwrap()
            .get("db")
            .unwrap()
            .to_owned();
        out.say(&format!("  $ psql postgres://app:{v}@db/app"));
        drop(out);
        let pane = String::from_utf8(pane).unwrap();
        let log = std::fs::read_to_string(&log_path).unwrap();
        for shown in [&pane, &log] {
            assert!(shown.contains("before any value exists"), "{shown}");
            assert!(shown.contains("postgres://app:••••@db/app"), "{shown}");
            assert!(!shown.contains(&v), "{shown}");
        }
    }
}
