//! `chappa-stack run-job <id>`: execute one stored job, in a pane or
//! headless, and leave its three files behind.
//!
//! A job is the whole plan the user confirmed, so the pane shows all of it
//! live: the checks before up, each step's command and output, a line per
//! file written, and a last line with the outcome. In order:
//!
//! 1. the record goes to `running` with this process's pid;
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
//!    into the process table, process stops end their tree;
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
//!    refused before running anything.
//!
//! A `data-safety-check` job runs the checks of the components it names
//! that are running, records the verdicts, applies `stop` / `block-up`
//! the same way, and fails when any verdict is not clean.
//!
//! An activation (a plan that binds a work item) builds from the item it
//! activates: the runner is bound to that item's worktrees before the plan
//! is applied, and `active.json` is written as the first step. A workspace
//! job (Start work, Add repo, Archive, Restore) is a git change to an item
//! folder; see [`crate::workspaces`].

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use indexmap::IndexMap;
use stack_core::checks::EngineChecker;
use stack_core::definition::ScanWhen;
use stack_core::definition::StacksDocument;
use stack_core::driver::{Check, Outcome, ResolvedInstance};
use stack_core::images::ProvenanceCache;
use stack_core::intent::{utc_now, ActiveRecord};
use stack_core::jobs::{JobStep, LocalStep, RunBlock};
use stack_core::planner::{Change, PlanError, Step, Verb};
use stack_core::runner::{ApplyOptions, RunnerError, StackRunner};
use stack_core::safety::Policy;

use crate::data_safety::{self, CheckEntry, SafetyState, Trigger, AFTER_UP_ATTEMPTS};
use crate::procs::{log_path, ProcessEntry, ProcessTable};
use crate::project::Loaded;
use crate::records::{JobRecord, JobResult, JobStatus, JobWork, StepOutcome};
use crate::timeouts::{self, StepKind};
use crate::world::{open_log, World};

/// Exit codes of `run-job`.
pub const EXIT_DONE: i32 = 0;
pub const EXIT_FAILED: i32 = 1;
pub const EXIT_REFUSED: i32 = 2;

/// The environment key every process component's run block carries (the
/// instance environment); engine commands carry none. It is how a run step
/// that starts a long-lived process is told apart from one that finishes.
const INSTANCE_ENV: &str = "CHAPPA_STACK_INSTANCE";

/// Where a job's lines go: the pane (stdout) and the job's log file.
struct Out<'a> {
    pane: &'a mut dyn Write,
    log: Option<File>,
}

impl Out<'_> {
    fn say(&mut self, text: &str) {
        crate::world::line(self.pane, text);
        if let Some(f) = &mut self.log {
            let _ = writeln!(f, "{text}");
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
    let (work_dir, rec) = match find_record(project_dir, id) {
        Ok(found) => found,
        Err(e) => {
            crate::world::line(pane, &format!("chappa-stack: {e}"));
            return EXIT_REFUSED;
        }
    };
    if rec.status.finished() {
        crate::world::line(
            pane,
            &format!("chappa-stack: job {id} already ran ({:?})", rec.status),
        );
        return EXIT_REFUSED;
    }
    let log = open_log(&rec.log, true).ok();
    let mut out = Out { pane, log };
    let started = utc_now();
    let _ = JobRecord::update(&work_dir, id, |r| {
        r.status = JobStatus::Running;
        r.started_utc = Some(started.clone());
        r.runner_pid = Some(std::process::id());
    });
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
    let code = match execute(world, project_dir, &work_dir, &rec, &mut out, &mut result) {
        Ok(()) => EXIT_DONE,
        Err((code, why)) => {
            out.say(&format!("✗ {why}"));
            result.error = Some(why);
            code
        }
    };
    result.ok = code == EXIT_DONE;
    result.exit_code = code;
    let secs = t0.elapsed().as_secs_f64();
    if code == EXIT_DONE {
        out.say(&format!("✓ done in {secs:.1} s"));
    } else {
        out.say(&format!("job ended with code {code} after {secs:.1} s"));
    }
    let _ = stack_core::jobs::write_atomic(
        &rec.result,
        &serde_json::to_string_pretty(&result).expect("result serializes"),
    );
    let ended = utc_now();
    let _ = JobRecord::update(&work_dir, id, |r| {
        r.status = if code == EXIT_DONE {
            JobStatus::Done
        } else {
            JobStatus::Failed
        };
        r.exit_code = Some(code);
        r.ended_utc = Some(ended.clone());
        r.error = result.error.clone();
    });
    code
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
    if let JobWork::Workspace { op } = &rec.work {
        return crate::workspaces::run_op(world, &loaded, op, &mut |l| out.say(l));
    }
    let binds: Option<String> = match &rec.work {
        JobWork::Plan {
            change: Change::Bind { item, .. },
            ..
        } => Some(item.clone()),
        _ => None,
    };
    if let Some(item) = &binds {
        loaded.bind_item(Some(item));
    }
    let now = utc_now();
    let runner = loaded.runner(world, &now);
    let (inst, _) = runner
        .instance(rec.slot)
        .map_err(|e| (EXIT_REFUSED, e.to_string()))?;
    let steps: Vec<JobStep> = match &rec.work {
        JobWork::Plan { plan, .. } => {
            let acquires = plan
                .steps
                .iter()
                .any(|s| matches!(s, Step::Pull { .. } | Step::Build { .. }));
            out.say(if acquires {
                "resolving images…"
            } else {
                "looking at the stack…"
            });
            let table = ProcessTable::load(work_dir, &inst.id);
            let mut obs = runner
                .observe(&inst, &table.statuses(&*world.procs))
                .map_err(|e| (EXIT_REFUSED, e.to_string()))?;
            if let Some(item) = &binds {
                obs.intent.item = Some(item.clone());
            }
            for note in &obs.notes {
                out.say(&format!("note: {note}"));
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
                let checks = runner.checks(&inst, Some(&obs), Some(plan), &checker);
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
            let applied = runner
                .apply(&inst, &obs, plan, &ApplyOptions::default())
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
            if let Some(item) = &binds {
                let mut active =
                    ActiveRecord::load(work_dir).map_err(|e| (EXIT_REFUSED, e.to_string()))?;
                active.set(rec.slot, item, &inst.id, &now);
                steps.insert(
                    0,
                    JobStep::write(
                        "active item",
                        ActiveRecord::path(work_dir),
                        serde_json::to_string_pretty(&active).expect("active serializes"),
                    ),
                );
            }
            steps
        }
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
        JobWork::Workspace { .. } => return Ok(()),
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
    };
    out.say(&format!("{} step(s):", steps.len()));
    for s in &steps {
        out.say(&format!("  · {}", s.describe()));
    }
    let forget_processes =
        matches!(&rec.work, JobWork::Plan { plan, .. } if plan.verb == Verb::Down);
    // Data-safety checks after up: under `block-up` right after each start
    // step (a verdict that is not clean ends the job there), otherwise once
    // the steps are done.
    let (gated, after_up): (Vec<String>, Vec<String>) = match &rec.work {
        JobWork::Plan { plan, .. } => {
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
                .filter(|c| started.contains(&c.as_str()))
                .collect();
            if data_safety::blocks_up(&loaded) {
                (wanted, Vec::new())
            } else {
                (Vec::new(), wanted)
            }
        }
        _ => (Vec::new(), Vec::new()),
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
        run_steps(
            world,
            work_dir,
            &inst.id,
            &steps,
            forget_processes,
            out,
            result,
            &mut gate,
        )
    };
    for c in &after_up {
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
    if let JobWork::Plan { plan, .. } = &rec.work {
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
    let cache = ProvenanceCache::load(work_dir, &inst.id);
    for (c, p) in &cache.components {
        result.provenance.insert(
            c.clone(),
            serde_json::to_value(p).unwrap_or(serde_json::Value::Null),
        );
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

/// The component a run step starts, if it is a start (`compose up` of a
/// container, or a process component's start).
fn start_of(step: &JobStep) -> Option<&str> {
    match step {
        JobStep::Run { run, component }
            if run.env.contains_key(INSTANCE_ENV) || StepKind::of(run) == StepKind::ComposeUp =>
        {
            Some(component)
        }
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
    run_steps(
        world,
        work_dir,
        &inst.id,
        &applied.steps,
        false,
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

/// Run steps in order. After each step that succeeded, `gate` may end the
/// run with a reason (the remaining steps are then skipped like after a
/// failure).
#[allow(clippy::too_many_arguments)]
fn run_steps(
    world: &World,
    work_dir: &Path,
    instance: &str,
    steps: &[JobStep],
    forget_processes: bool,
    out: &mut Out<'_>,
    result: &mut JobResult,
    gate: &mut dyn FnMut(&JobStep, &mut Out<'_>) -> Option<String>,
) -> Result<(), Failure> {
    let mut failure: Option<String> = None;
    for step in steps {
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
        let outcome = run_step(world, work_dir, instance, step, forget_processes, out);
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
    out: &mut Out<'_>,
) -> Result<Option<i32>, (Option<i32>, String)> {
    match step {
        JobStep::Local { step: local } => {
            local.perform().map_err(|e| (None, e.to_string()))?;
            match local {
                LocalStep::WriteFile { .. } => out.say(&format!("✓ {}", local.describe())),
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
            let code = world
                .steps
                .run(run, kind.limit(), &mut |l| out.say(&format!("  {l}")))
                .map_err(|e| (None, e))?;
            if code == 0 {
                Ok(Some(0))
            } else {
                Err((Some(code), format!("exited with code {code}")))
            }
        }
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
