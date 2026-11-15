//! Data-safety checks: run each component's `marker` / `sample` hooks,
//! judge what they printed with stack-core's rule engine, keep the verdict
//! on the instance, and say what the app must enforce.
//!
//! A dev database is meant to hold scrubbed data. The user's hooks know how
//! to read their database; the rules (`dataSafety.rules`) say what real
//! data looks like; this module runs the one over the other. **No sampled
//! value is ever kept**: a hook's stdout lives in memory only while the
//! rules run over it and is dropped right after; its stderr is never kept
//! or shown at all (an engine's error text can quote the row it choked on);
//! what is stored and reported is counts, statuses, a marker's own label
//! and the rules file's hash.
//!
//! # When a check runs
//!
//! | `when:` | where |
//! |---|---|
//! | `after-up` (also the default when `when:` is empty) | inside the `run-job` of any plan that started the component, once its start step succeeded. A hook that fails right after up is retried a few times, since a database may still be starting. Under `onFinding: block-up` the check runs right after the start step, before any later step, whatever `when:` says, and a verdict that is not clean fails the job |
//! | `every: <duration>` | the background thread looks once a minute and starts a headless check job for each running component whose last check is older than its interval (or that has none) |
//! | `after-restore`, `before-snapshot` | reserved: accepted by the definition, run by the restore and snapshot jobs once those exist ([`components_for`]) |
//! | the `data-safety-check` action | a job, now, for one component or every running one with a check |
//!
//! # Policy, enforced here
//!
//! - `stop`: a component whose check is not clean is stopped through the
//!   planner and runner (the volume is kept, so it can be inspected), with
//!   the stop in the job's log.
//! - `block-up`: after `up` the check runs before the instance counts as
//!   ready; a verdict that is not clean stops the component and fails the
//!   job, so the instance never becomes reachable. Outside `up` (a periodic
//!   or manual check) it acts like `stop`.
//! - `warn` and `block-agents` are the app's to act on: the payload says
//!   which, in one field.
//!
//! # The wire block
//!
//! The Environment payload carries one top-level `dataSafety` object for
//! the instance it serves:
//!
//! ```json
//! "dataSafety": {
//!   "instance": "local/0",
//!   "state": "clean" | "not-clean" | "acknowledged" | "unscanned" | "error",
//!   "policy": "warn" | "block-agents" | "stop" | "block-up" | null,
//!   "blocksAgents": false,
//!   "lastScanUtc": "2026-11-14T12:00:00Z" | null,
//!   "rulesHash": "<hex>" | null,
//!   "marker": { "scrubbed": true, "version": "2026-03", "at": "2026-03-01T10:00:00Z" } | null,
//!   "findings": [ { "rule": "email-real-domain", "table": "customers", "column": "email", "count": 2, "severity": "bad" } ],
//!   "unscanned": ["db"],
//!   "ack": { "by": "the app", "note": "…", "atUtc": "…" } | null,
//!   "ackActionId": "data-safety-ack"
//! }
//! ```
//!
//! - Each component gets a state: `clean`; `not-clean` (findings, a marker
//!   missing or saying not scrubbed, a sample that failed, was refused or
//!   had no rules to read it); `acknowledged` (not clean, and an
//!   acknowledgement covers exactly this verdict); `unscanned` (it keeps
//!   data, `persistence:`, and declares no check, or it declares one, runs,
//!   and has not been checked yet); `error` (the check could not be judged:
//!   the rules file does not load, a hook's placeholders cannot be filled,
//!   the stored verdicts cannot be read). A component with a check that is
//!   not running and was never checked, and one that neither keeps data nor
//!   declares a check, have nothing to say.
//! - `state` is the worst component state: `error` > `not-clean` >
//!   `unscanned` > `acknowledged` > `clean`; `clean` when no component has
//!   anything to say.
//! - `policy` is the strongest policy in force over the components (`warn`
//!   < `block-agents` < `stop` < `block-up`): `onFinding` for a not-clean
//!   or error component, `unscanned` for an unscanned one, none for clean
//!   and acknowledged ones. `null` when nothing is in force.
//! - **`blocksAgents` is the one field to enforce on**: true exactly when
//!   `policy` is not null and not `warn`. That is, some component is not
//!   clean (or cannot be judged) under an `onFinding` that blocks agents,
//!   or is unscanned under `unscanned: block-agents`.
//! - `lastScanUtc` is the newest check of any component; `rulesHash` the
//!   rules that check used (the current file's when nothing was checked).
//! - `marker` sums up the components with a marker hook that were checked:
//!   `scrubbed` only when every one attests a scrub; `version` and `at`
//!   from the first that names them. `null` when none was.
//! - `findings` lists every finding of every component's last verdict, as
//!   counts: rule, table, column, count, severity. Never a value.
//! - `unscanned` names the unscanned components, in definition order.
//! - `ack` is the acknowledgement in force (the newest, if several).
//! - `ackActionId` names the endpoint action that acknowledges; it is
//!   offered (on the data-safety banner and facts section) only while
//!   `state` is `not-clean`.
//!
//! When the definition cannot be read at all, the block is still sent,
//! with `state: "error"`, `policy: null` and `blocksAgents: false`: the
//! source cannot tell, and the app decides what an unreadable source means
//! (it blocks agents, failing closed).
//!
//! Three fields say where a block applies and why, so the app can keep
//! agents out of the right folders and say what it is waiting for:
//!
//! ```json
//!   "bound": { "source": "tracker", "key": "ABC-1",
//!              "folders": ["/srv/example/.work/ABC-1", "/srv/example/.work/ABC-1/src/api-1a2b3c4d"] } | null,
//!   "project": "/srv/example",
//!   "reasons": [ { "component": "db", "text": "db: 3 findings (email-real-domain ×2, card-number ×1)" } ]
//! ```
//!
//! - `bound` is the item the slot is bound to (`active.json`): its owner's
//!   source and key (`source` is `null` when the item's record cannot be
//!   read) and the absolute folders agents work in for it, the item folder
//!   first, then each worktree. `null` when no item is active; the block
//!   then covers `project`.
//! - `project` is the stack project's folder, absolute.
//! - `reasons` has one line per component with something to say (not
//!   clean, cannot be judged, not checked), in definition order: the alert
//!   text without the policy's effect.
//!
//! # Acknowledging
//!
//! `data-safety-ack` (view-level, `mutates: true`, `note: "required"`)
//! confirms with the findings named by rule, table, column and count. It
//! records who (the `X-Chappa-Actor` request header, else "the app"), the
//! note and the time on every not-clean component, with stack-core's
//! `acknowledge`. It lasts while rescans report exactly the same verdict:
//! any count change, a finding added or gone, a marker or rules change, or
//! a clean rescan ends it.
//!
//! Verdicts live in `<work>/instances/<instance>/data-safety.json`.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use stack_core::cli::{CliError, Invocation};
use stack_core::compose::parse_utc;
use stack_core::definition::{ComponentDef, ScanWhen};
use stack_core::driver::ResolvedInstance;
use stack_core::jobs::write_atomic;
use stack_core::placeholders;
use stack_core::runner::Observation;
use stack_core::safety::data::{
    ack_survives, acknowledge, enforcement, evaluate, on_finding_policy, unscanned_policy,
    DataRules, DataVerdict, EnforcementCause, HookRun, MarkerVerdict, SampleStatus,
    UnscrubbedReason,
};
use stack_core::safety::{Acknowledgement, Finding, Policy};

use crate::procs::instance_dir;
use crate::project::Loaded;
use crate::world::World;

pub const STORE_FILE: &str = "data-safety.json";
/// The acknowledge action's id (the wire block names it too).
pub const ACK_ACTION_ID: &str = "data-safety-ack";
/// The "run the check now" action's id.
pub const CHECK_ACTION_ID: &str = "data-safety-check";
/// How long one hook may run.
pub const HOOK_TIMEOUT: Duration = crate::timeouts::OTHER;
/// Attempts of a hook that fails right after up.
pub const AFTER_UP_ATTEMPTS: u32 = 5;
/// How often (in clock seconds) the background thread looks for due
/// periodic checks.
pub const SCHEDULE_TICK_SECS: u64 = 60;
/// Longest acknowledgement note and actor name kept.
pub const NOTE_CAP: usize = 1000;
pub const ACTOR_CAP: usize = 128;
/// Who acknowledged, when the app does not say.
pub const DEFAULT_ACTOR: &str = "the app";

/// What started a check, as recorded with its verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    AfterUp,
    AfterRestore,
    BeforeSnapshot,
    Every,
    Manual,
}

impl Trigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Trigger::AfterUp => "after-up",
            Trigger::AfterRestore => "after-restore",
            Trigger::BeforeSnapshot => "before-snapshot",
            Trigger::Every => "every",
            Trigger::Manual => "manual",
        }
    }

    pub fn parse(s: &str) -> Trigger {
        match s {
            "after-up" => Trigger::AfterUp,
            "after-restore" => Trigger::AfterRestore,
            "before-snapshot" => Trigger::BeforeSnapshot,
            "every" => Trigger::Every,
            _ => Trigger::Manual,
        }
    }
}

// ------------------------------------------------------------ definition

/// The component declares a hook to run.
pub fn has_check(c: &ComponentDef) -> bool {
    c.data_safety
        .as_ref()
        .is_some_and(|d| d.marker.is_some() || d.sample.is_some())
}

/// The check times a component declares; `after-up` when it names none.
/// Entries the definition check refuses are left out here.
pub fn check_times(c: &ComponentDef) -> Vec<ScanWhen> {
    let Some(d) = &c.data_safety else {
        return Vec::new();
    };
    if d.when.is_empty() {
        return vec![ScanWhen::AfterUp];
    }
    d.when.iter().filter_map(|w| w.parse().ok()).collect()
}

/// The shortest `every:` interval of a component, if it has one.
pub fn interval(c: &ComponentDef) -> Option<Duration> {
    check_times(c)
        .into_iter()
        .filter_map(|w| match w {
            ScanWhen::Every(d) => Some(d),
            _ => None,
        })
        .min()
}

/// The components with a check that runs at `when` (`AfterUp`,
/// `AfterRestore`, `BeforeSnapshot`), in definition order. Under
/// `block-up` every checked component runs after up.
pub fn components_for(loaded: &Loaded, when: ScanWhen) -> Vec<String> {
    let block_up = on_finding_policy(loaded.stack.data_safety.as_ref()) == Policy::BlockUp;
    loaded
        .stack
        .components
        .iter()
        .filter(|(_, c)| has_check(c))
        .filter(|(_, c)| check_times(c).contains(&when) || (when == ScanWhen::AfterUp && block_up))
        .map(|(n, _)| n.clone())
        .collect()
}

/// Whether the stack's `onFinding` is `block-up`.
pub fn blocks_up(loaded: &Loaded) -> bool {
    on_finding_policy(loaded.stack.data_safety.as_ref()) == Policy::BlockUp
}

// ----------------------------------------------------------------- store

/// One component's last check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckEntry {
    pub scanned_utc: String,
    /// `after-up`, `every`, `manual` …
    pub trigger: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<String>,
    /// The verdict; absent when the check could not be judged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<DataVerdict>,
    /// Why it could not be judged, in this program's own words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The acknowledgement of this exact verdict, while it holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ack: Option<Acknowledgement>,
}

/// Every component's last check on one instance.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckStore {
    #[serde(default)]
    pub components: IndexMap<String, CheckEntry>,
}

pub fn store_path(work_dir: &Path, instance: &str) -> PathBuf {
    instance_dir(work_dir, instance).join(STORE_FILE)
}

impl CheckStore {
    /// A missing file is an empty store; one that cannot be read is an
    /// error, so the instance reads as "cannot be judged" rather than clean.
    pub fn load(work_dir: &Path, instance: &str) -> Result<CheckStore, String> {
        let path = store_path(work_dir, instance);
        match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|e| format!("{} cannot be read: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(CheckStore::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    pub fn save(&self, work_dir: &Path, instance: &str) -> Result<(), String> {
        let path = store_path(work_dir, instance);
        write_atomic(
            &path,
            &serde_json::to_string_pretty(self).expect("store serializes"),
        )
        .map_err(|e| format!("{}: {e}", path.display()))
    }
}

// ----------------------------------------------------------- running it

/// The outcome of one component's check, before it is recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckRun {
    pub verdict: Option<DataVerdict>,
    pub error: Option<String>,
}

impl CheckRun {
    fn error(e: String) -> CheckRun {
        CheckRun {
            verdict: None,
            error: Some(e),
        }
    }
}

/// Run one hook; its stdout when it exited 0. A failure is said in a line
/// of this program's own (exit code, timeout, cannot start), never the
/// hook's error output.
fn run_hook(
    world: &World,
    argv: &[String],
    cwd: &Path,
    attempts: u32,
    what: &str,
    say: &mut dyn FnMut(&str),
) -> Option<String> {
    let attempts = attempts.max(1);
    for attempt in 1..=attempts {
        let inv = Invocation::from_argv(argv).cwd(cwd).timeout(HOOK_TIMEOUT);
        let why = match world.cli.run(&inv) {
            Ok(out) if out.ok() => return Some(out.stdout),
            Ok(out) => match out.status {
                Some(c) => format!("exited with code {c}"),
                None => "was killed".to_string(),
            },
            Err(CliError::Timeout { seconds, .. }) => format!("timed out after {seconds} s"),
            Err(CliError::Spawn { program, message }) => {
                format!("could not start `{program}` ({message})")
            }
            Err(_) => "failed".to_string(),
        };
        let again = if attempt < attempts {
            "; trying again"
        } else {
            ""
        };
        say(&format!("  {what} hook {why}{again}"));
        if attempt < attempts {
            std::thread::sleep(world.hook_retry);
        }
    }
    None
}

fn hook_run(o: &Option<Option<String>>) -> HookRun<'_> {
    match o {
        None => HookRun::NotDefined,
        Some(None) => HookRun::Failed,
        Some(Some(s)) => HookRun::Stdout(s),
    }
}

/// Run `component`'s hooks and judge them. `obs` supplies the container
/// name for `{container}`; `attempts` > 1 retries a hook that fails.
pub fn run_check(
    world: &World,
    loaded: &Loaded,
    inst: &ResolvedInstance,
    obs: Option<&Observation>,
    component: &str,
    attempts: u32,
    say: &mut dyn FnMut(&str),
) -> CheckRun {
    let Some(def) = loaded
        .stack
        .components
        .get(component)
        .and_then(|c| c.data_safety.as_ref())
    else {
        return CheckRun::error(format!("{component} declares no data-safety check"));
    };
    let rules = match loaded
        .stack
        .data_safety
        .as_ref()
        .and_then(|d| d.rules.as_ref())
    {
        Some(path) => match DataRules::load(&loaded.dir.join(path)) {
            Ok(r) => Some(r),
            Err(e) => return CheckRun::error(format!("the rules file cannot be used: {e}")),
        },
        None => None,
    };
    let mut ctx = inst.context_for(component);
    ctx.container = obs
        .and_then(|o| o.ps.get(component))
        .map(|p| p.name.clone())
        .filter(|n| !n.is_empty());
    let fill = |argv: &Option<Vec<String>>, what: &str| -> Result<Option<Vec<String>>, String> {
        match argv {
            None => Ok(None),
            Some(a) => placeholders::resolve_argv(a, &ctx).map(Some).map_err(|e| {
                format!("the {what} hook cannot be filled in ({e}); is {component} running?")
            }),
        }
    };
    let marker_argv = match fill(&def.marker, "marker") {
        Ok(a) => a,
        Err(e) => return CheckRun::error(e),
    };
    let sample_argv = match fill(&def.sample, "sample") {
        Ok(a) => a,
        Err(e) => return CheckRun::error(e),
    };
    // The hooks' stdout exists only inside this block: it is judged and
    // dropped, and only the verdict leaves.
    let verdict = {
        let marker_out = marker_argv
            .as_ref()
            .map(|a| run_hook(world, a, &loaded.dir, attempts, "marker", say));
        let sample_out = sample_argv
            .as_ref()
            .map(|a| run_hook(world, a, &loaded.dir, attempts, "sample", say));
        evaluate(rules.as_ref(), hook_run(&marker_out), hook_run(&sample_out))
    };
    match verdict {
        Some(v) => CheckRun {
            verdict: Some(v),
            error: None,
        },
        None => CheckRun::error(format!("{component} declares no data-safety hook")),
    }
}

/// Record a check on the instance. An acknowledgement stays only when the
/// new verdict is exactly the one acknowledged.
pub fn record(
    work_dir: &Path,
    instance: &str,
    component: &str,
    run: &CheckRun,
    trigger: Trigger,
    job_id: Option<&str>,
    now_utc: &str,
) -> Result<CheckEntry, String> {
    // A store that cannot be read is replaced: its acknowledgements are
    // lost, which re-raises a block rather than lifting one.
    let mut store = CheckStore::load(work_dir, instance).unwrap_or_default();
    let ack = match (
        &run.verdict,
        store.components.get(component).and_then(|e| e.ack.clone()),
    ) {
        (Some(v), Some(a)) if ack_survives(&a, v) => Some(a),
        _ => None,
    };
    let entry = CheckEntry {
        scanned_utc: now_utc.to_string(),
        trigger: trigger.as_str().to_string(),
        job_id: job_id.map(str::to_string),
        verdict: run.verdict.clone(),
        error: run.error.clone(),
        ack,
    };
    store
        .components
        .insert(component.to_string(), entry.clone());
    store.save(work_dir, instance)?;
    Ok(entry)
}

// ------------------------------------------------------------- the state

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SafetyState {
    Clean,
    Acknowledged,
    Unscanned,
    NotClean,
    Error,
}

impl SafetyState {
    pub fn as_str(self) -> &'static str {
        match self {
            SafetyState::Clean => "clean",
            SafetyState::NotClean => "not-clean",
            SafetyState::Acknowledged => "acknowledged",
            SafetyState::Unscanned => "unscanned",
            SafetyState::Error => "error",
        }
    }
}

fn policy_rank(p: Policy) -> u8 {
    match p {
        Policy::Warn => 0,
        Policy::BlockAgents => 1,
        Policy::Stop => 2,
        Policy::BlockUp => 3,
        // Not a data policy; read as block-agents (as the engine does).
        Policy::BlockBuild => 1,
    }
}

/// The stronger of two policies.
pub fn stronger(a: Option<Policy>, b: Option<Policy>) -> Option<Policy> {
    match (a, b) {
        (Some(x), Some(y)) => Some(if policy_rank(y) > policy_rank(x) {
            y
        } else {
            x
        }),
        (x, None) => x,
        (None, y) => y,
    }
}

/// One component's part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentSafety {
    pub id: String,
    pub state: SafetyState,
    pub policy: Option<Policy>,
    pub has_check: bool,
    pub running: bool,
    pub entry: Option<CheckEntry>,
    /// For an error state: why.
    pub error: Option<String>,
}

/// The instance's data safety, as the payload and the actions see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceSafety {
    /// `<stack>/<slot>`.
    pub instance: String,
    /// Components with something to say, in definition order.
    pub components: Vec<ComponentSafety>,
    /// Components with a check that run now (the check action's targets).
    pub checkable: Vec<String>,
    pub state: SafetyState,
    pub policy: Option<Policy>,
    pub on_finding: Policy,
    pub unscanned_policy: Policy,
    /// Whether the stack mentions data safety at all (a `dataSafety:`
    /// block, a component check, or a component that keeps data).
    pub configured: bool,
    /// The rules file's hash now, or why it does not load.
    pub current_rules: Option<Result<String, String>>,
}

/// `<stack>/<slot>`, the wire's instance name.
pub fn instance_name(stack: &str, slot: u32) -> String {
    format!("{stack}/{slot}")
}

/// A folder as the app compares it: absolute (against the current
/// directory when the source was started with a relative project), with no
/// Windows verbatim prefix added.
fn absolute_text(path: &Path) -> String {
    std::path::absolute(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

/// The stack project's folder, as the block's `project` field: what a
/// block covers when no item is active, since agents started in the stack
/// project itself are as close to the data as anything.
pub fn project_dir(dir: &Path) -> Value {
    Value::String(absolute_text(dir))
}

/// The block's `bound` field: the item the slot is bound to (its owner's
/// source and key, from `item.json`) and the folders agents would work in
/// for it, the item folder first and then each worktree, absolute. `null`
/// when no item is active. An item whose record cannot be read is still
/// named, by its folder, with `source: null`: its folder is what matters.
pub fn bound(loaded: &Loaded) -> Value {
    let Some(item) = loaded.binding.item.as_deref() else {
        return Value::Null;
    };
    let dir = stack_core::workspace::ItemRecord::dir(&loaded.work_dir(), item);
    let (source, key) = match stack_core::workspace::ItemRecord::load(&dir) {
        Ok(Some(rec)) => (Value::String(rec.item_ref.source), rec.item_ref.key),
        _ => (Value::Null, item.to_string()),
    };
    let mut folders = vec![absolute_text(&dir)];
    folders.extend(loaded.binding.worktrees.values().map(|w| absolute_text(w)));
    json!({ "source": source, "key": key, "folders": folders })
}

/// The rules file's hash now (`None` when the stack names none).
pub fn current_rules(loaded: &Loaded) -> Option<Result<String, String>> {
    let path = loaded.stack.data_safety.as_ref()?.rules.as_ref()?;
    Some(
        DataRules::load(&loaded.dir.join(path))
            .map(|r| r.hash)
            .map_err(|e| e.to_string()),
    )
}

/// Put together the instance's state from the definition, what runs and
/// the stored verdicts.
pub fn summarize(
    loaded: &Loaded,
    running: &dyn Fn(&str) -> bool,
    store: &Result<CheckStore, String>,
    current_rules: Option<Result<String, String>>,
) -> InstanceSafety {
    let def = loaded.stack.data_safety.as_ref();
    let on_finding = on_finding_policy(def);
    let unscanned = unscanned_policy(def);
    let mut components = Vec::new();
    let mut checkable = Vec::new();
    let mut configured = def.is_some();
    for (name, c) in &loaded.stack.components {
        let checked = has_check(c);
        let persistent = c.persistence.is_some();
        configured |= checked || persistent;
        let is_running = running(name);
        if checked && is_running {
            checkable.push(name.clone());
        }
        let entry = store.as_ref().ok().and_then(|s| s.components.get(name));
        let (state, policy, error) = if !checked {
            if !persistent {
                continue;
            }
            (SafetyState::Unscanned, Some(unscanned), None)
        } else if let Err(e) = store {
            (SafetyState::Error, Some(on_finding), Some(e.clone()))
        } else {
            match entry {
                None if is_running => (SafetyState::Unscanned, Some(unscanned), None),
                None => continue,
                Some(e) => match (&e.verdict, &e.error) {
                    (Some(v), _) => {
                        let en = enforcement(def, Some(v), e.ack.as_ref());
                        let state = match en.cause {
                            EnforcementCause::Clean => SafetyState::Clean,
                            EnforcementCause::Acknowledged => SafetyState::Acknowledged,
                            EnforcementCause::NotClean => SafetyState::NotClean,
                            EnforcementCause::Unscanned => SafetyState::Unscanned,
                        };
                        (state, en.policy, None)
                    }
                    (None, err) => (
                        SafetyState::Error,
                        Some(on_finding),
                        Some(
                            err.clone()
                                .unwrap_or_else(|| "the check left no verdict".into()),
                        ),
                    ),
                },
            }
        };
        components.push(ComponentSafety {
            id: name.clone(),
            state,
            policy,
            has_check: checked,
            running: is_running,
            entry: entry.cloned(),
            error,
        });
    }
    let state = components
        .iter()
        .map(|c| c.state)
        .max()
        .unwrap_or(SafetyState::Clean);
    let policy = components
        .iter()
        .fold(None, |acc, c| stronger(acc, c.policy));
    InstanceSafety {
        instance: instance_name(&loaded.stack_name, loaded.slot),
        components,
        checkable,
        state,
        policy,
        on_finding,
        unscanned_policy: unscanned,
        configured,
        current_rules,
    }
}

impl InstanceSafety {
    /// The one field the app enforces on.
    pub fn blocks_agents(&self) -> bool {
        self.policy.is_some_and(Policy::blocks_agents)
    }

    pub fn component(&self, id: &str) -> Option<&ComponentSafety> {
        self.components.iter().find(|c| c.id == id)
    }

    fn not_clean(&self) -> impl Iterator<Item = &ComponentSafety> {
        self.components
            .iter()
            .filter(|c| c.state == SafetyState::NotClean)
    }

    /// The newest check of any component.
    pub fn last_scan(&self) -> Option<&str> {
        self.components
            .iter()
            .filter_map(|c| c.entry.as_ref().map(|e| e.scanned_utc.as_str()))
            .max()
    }

    /// The rules hash the newest verdict used, else the current file's.
    pub fn rules_hash(&self) -> Option<String> {
        let newest = self
            .components
            .iter()
            .filter_map(|c| c.entry.as_ref())
            .filter(|e| e.verdict.is_some())
            .max_by(|a, b| a.scanned_utc.cmp(&b.scanned_utc));
        match newest.and_then(|e| e.verdict.as_ref()) {
            Some(v) => v.rules_hash.clone(),
            None => self.current_rules.clone().and_then(Result::ok),
        }
    }

    fn marker(&self) -> Value {
        let verdicts: Vec<&MarkerVerdict> = self
            .components
            .iter()
            .filter_map(|c| c.entry.as_ref()?.verdict.as_ref())
            .map(|v| &v.marker)
            .filter(|m| !matches!(m, MarkerVerdict::NotDefined))
            .collect();
        if verdicts.is_empty() {
            return Value::Null;
        }
        let scrubbed = verdicts
            .iter()
            .all(|m| matches!(m, MarkerVerdict::Scrubbed { .. }));
        let mut out = json!({ "scrubbed": scrubbed });
        if let Some(v) = verdicts.iter().find_map(|m| m.version()) {
            out["version"] = v.into();
        }
        if let Some(at) = verdicts.iter().find_map(|m| marker_at(m)) {
            out["at"] = at.into();
        }
        out
    }

    fn findings(&self) -> Vec<&Finding> {
        self.components
            .iter()
            .filter_map(|c| c.entry.as_ref()?.verdict.as_ref())
            .flat_map(|v| v.findings.iter())
            .collect()
    }

    fn ack(&self) -> Value {
        self.components
            .iter()
            .filter(|c| c.state == SafetyState::Acknowledged)
            .filter_map(|c| c.entry.as_ref()?.ack.as_ref())
            .max_by(|a, b| a.at_utc.cmp(&b.at_utc))
            .map(|a| json!({ "by": a.who, "note": a.why, "atUtc": a.at_utc }))
            .unwrap_or(Value::Null)
    }

    /// The unscanned components, in definition order.
    pub fn unscanned(&self) -> Vec<&str> {
        self.components
            .iter()
            .filter(|c| c.state == SafetyState::Unscanned)
            .map(|c| c.id.as_str())
            .collect()
    }

    /// The `dataSafety` block (see the module docs).
    pub fn wire(&self) -> Value {
        json!({
            "instance": self.instance,
            "state": self.state.as_str(),
            "policy": self.policy.map(Policy::as_str),
            "blocksAgents": self.blocks_agents(),
            "lastScanUtc": self.last_scan(),
            "rulesHash": self.rules_hash(),
            "marker": self.marker(),
            "findings": self.findings(),
            "unscanned": self.unscanned(),
            "ack": self.ack(),
            "ackActionId": ACK_ACTION_ID,
        })
    }

    /// What an acknowledgement would cover now: a hash over every not-clean
    /// component's verdict keys, so a confirm given for one verdict cannot
    /// acknowledge another.
    pub fn ack_fingerprint(&self) -> String {
        let mut h = Sha256::new();
        for c in self.not_clean() {
            if let Some(v) = c.entry.as_ref().and_then(|e| e.verdict.as_ref()) {
                h.update(c.id.as_bytes());
                h.update(b"\n");
                for k in v.ack_keys() {
                    h.update(k.as_bytes());
                    h.update(b"\n");
                }
            }
        }
        crate::identity::to_hex(&h.finalize())
    }

    /// The confirm text: what is acknowledged, named by rule, table, column
    /// and count.
    pub fn ack_confirm(&self) -> String {
        let parts: Vec<String> = self
            .not_clean()
            .filter_map(|c| {
                let v = c.entry.as_ref()?.verdict.as_ref()?;
                Some(format!("{}: {}", c.id, problems(v, true).join("; ")))
            })
            .collect();
        format!(
            "Acknowledge what the data-safety check found on {}: {}. Agents may be pointed at it again until a check finds anything different.",
            self.instance,
            parts.join("; ")
        )
    }

    /// The acknowledge action, while there is something to acknowledge.
    pub fn ack_action(&self) -> Option<Value> {
        (self.state == SafetyState::NotClean).then(|| {
            json!({
                "kind": "endpoint",
                "id": ACK_ACTION_ID,
                "label": "Acknowledge findings",
                "mutates": true,
                "confirm": self.ack_confirm(),
                "note": "required",
                "params": { "fingerprint": self.ack_fingerprint() },
            })
        })
    }

    /// Acknowledge every not-clean component's verdict; how many.
    pub fn acknowledge_all(
        &self,
        work_dir: &Path,
        instance_id: &str,
        who: &str,
        why: &str,
        now_utc: &str,
    ) -> Result<usize, String> {
        let mut store = CheckStore::load(work_dir, instance_id)?;
        let mut n = 0;
        for c in self.not_clean() {
            let Some(entry) = store.components.get_mut(&c.id) else {
                continue;
            };
            let Some(v) = &entry.verdict else { continue };
            entry.ack = Some(acknowledge(v, who, why, now_utc));
            n += 1;
        }
        store.save(work_dir, instance_id)?;
        Ok(n)
    }
}

fn marker_at(m: &MarkerVerdict) -> Option<&str> {
    match m {
        MarkerVerdict::Scrubbed { at, .. } | MarkerVerdict::Unscrubbed { at, .. } => at.as_deref(),
        _ => None,
    }
}

/// The "run the check now" action, for one component or (`None`) all.
pub fn check_action(component: Option<&str>) -> Value {
    json!({
        "kind": "endpoint",
        "id": CHECK_ACTION_ID,
        "label": match component {
            Some(_) => "Run data-safety check",
            None => "Run data-safety checks",
        },
        "mutates": false,
    })
}

// ---------------------------------------------------------------- words

/// `3 findings (email-real-domain ×2, card-number ×1)`, counts summed per
/// rule. With `places`, each finding is named by table and column instead.
pub fn findings_text(findings: &[Finding], places: bool) -> Option<String> {
    if findings.is_empty() {
        return None;
    }
    let total: u64 = findings.iter().map(|f| u64::from(f.count)).sum();
    let items: Vec<String> = if places {
        findings
            .iter()
            .map(|f| format!("{} in {} ×{}", f.rule, f.location, f.count))
            .collect()
    } else {
        let mut per_rule: IndexMap<&str, u64> = IndexMap::new();
        for f in findings {
            *per_rule.entry(f.rule.as_str()).or_default() += u64::from(f.count);
        }
        per_rule
            .into_iter()
            .map(|(r, n)| format!("{r} ×{n}"))
            .collect()
    };
    Some(format!(
        "{total} finding{} ({})",
        if total == 1 { "" } else { "s" },
        items.join(", ")
    ))
}

/// Why a verdict is not clean, one phrase per problem.
pub fn problems(v: &DataVerdict, places: bool) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(f) = findings_text(&v.findings, places) {
        out.push(f);
    }
    if let MarkerVerdict::Unscrubbed { reason, .. } = &v.marker {
        out.push(
            match reason {
                UnscrubbedReason::Missing => "no scrub marker",
                UnscrubbedReason::NotScrubbed => "the marker says the data is not scrubbed",
                UnscrubbedReason::Unparsable => {
                    "the marker hook printed something that is not a marker"
                }
            }
            .to_string(),
        );
    }
    match &v.sample {
        SampleStatus::Failed => out.push("the sample hook failed".into()),
        SampleStatus::NoRules => out.push("the sample was not checked: no rules file".into()),
        SampleStatus::Refused { reason } => out.push(format!("the sample was refused: {reason}")),
        _ => {}
    }
    out
}

/// One line for a verdict: `clean (120 values checked)` or the problems.
pub fn verdict_line(v: &DataVerdict) -> String {
    if v.clean {
        let mut s = "clean".to_string();
        if matches!(v.sample, SampleStatus::Scanned) {
            s.push_str(&format!(" ({} values checked)", v.scanned_values));
        }
        if let Some(ver) = v.marker.version() {
            s.push_str(&format!(", scrub marker {ver}"));
        }
        s
    } else {
        format!("not clean: {}", problems(v, false).join("; "))
    }
}

/// What a policy does to agents, for alert text.
pub fn policy_effect(p: Option<Policy>) -> Option<&'static str> {
    match p {
        Some(Policy::BlockAgents) | Some(Policy::BlockBuild) => Some("agents blocked"),
        Some(Policy::Stop) => Some("stopped, agents blocked"),
        Some(Policy::BlockUp) => Some("kept from coming up, agents blocked"),
        _ => None,
    }
}

/// A recorded check as `<id>.result.json` holds it: the state and the
/// verdict's counts.
pub fn result_value(entry: &CheckEntry, state: SafetyState) -> Value {
    json!({
        "state": state.as_str(),
        "scannedUtc": entry.scanned_utc,
        "trigger": entry.trigger,
        "verdict": entry.verdict,
        "error": entry.error,
    })
}

/// A checked component's state and policy right after a check, as the
/// job decides what to enforce.
pub fn judged(loaded: &Loaded, entry: &CheckEntry) -> (SafetyState, Option<Policy>) {
    let def = loaded.stack.data_safety.as_ref();
    match &entry.verdict {
        None => (SafetyState::Error, Some(on_finding_policy(def))),
        Some(v) => {
            let en = enforcement(def, Some(v), entry.ack.as_ref());
            let state = match en.cause {
                EnforcementCause::Clean => SafetyState::Clean,
                EnforcementCause::Acknowledged => SafetyState::Acknowledged,
                _ => SafetyState::NotClean,
            };
            (state, en.policy)
        }
    }
}

// -------------------------------------------------------------- schedule

/// Components whose `every:` interval has passed since their last check
/// (or that were never checked), at `now_secs`. Whether they run is the
/// caller's to check.
pub fn due(loaded: &Loaded, store: &CheckStore, now_secs: u64) -> Vec<String> {
    loaded
        .stack
        .components
        .iter()
        .filter(|(_, c)| has_check(c))
        .filter_map(|(name, c)| {
            let every = interval(c)?;
            let last = store
                .components
                .get(name)
                .and_then(|e| parse_utc(&e.scanned_utc));
            match last {
                Some(t) if t.saturating_add(every.as_secs()) > now_secs => None,
                _ => Some(name.clone()),
            }
        })
        .collect()
}

/// Whether any component checks periodically (so the schedule is worth a
/// look at all).
pub fn any_periodic(loaded: &Loaded) -> bool {
    loaded
        .stack
        .components
        .values()
        .any(|c| has_check(c) && interval(c).is_some())
}

/// An acknowledgement note or actor name as kept: control characters
/// removed, trimmed, capped.
pub fn clean_text(s: &str, cap: usize) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .chars()
        .take(cap)
        .collect()
}
