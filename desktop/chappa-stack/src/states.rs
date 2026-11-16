//! Saved stack states in the source: save, prune, resume, the volume
//! guard, and what the Work and Environment views offer for them.
//!
//! A state (`<work>/<KEY>/states/<UTC>.json`, see
//! [`stack_core::states`]) records what the item's stack runs and how to
//! get its data back, never the data. Saving runs a component's snapshot
//! `create` hook (and copies the artifact out for `location: host`);
//! pruning runs `delete` for the snapshots no kept state points at;
//! resuming runs `exists` for every snapshot first, then the plan, then
//! `restore` (or the recipe: scrub, check, seed, state seeds, check).
//!
//! | action | where | does |
//! |---|---|---|
//! | `save-state` | the item's overlay, while the item is active in this slot | a job: saves the state now; `mutates` (with a confirm naming the snapshots) when it runs snapshot hooks |
//! | `resume:<state>` | the item's Saved states section | a job: worktrees to the saved commits, images by digest or content key, the plan, then each snapshot restored or recipe replayed; a missing snapshot blocks only its component; a worktree with uncommitted changes refuses the whole resume |
//! | `resume-no-save:<state>` | beside it, when another item has the slot | the same without saving that item's state first |
//! | `resume-tips:<state>` / `resume-exact:<state>` (and their `-no-save` twins) | in place of `resume:` when a worktree's branch moved since the save | stay on the moved branches and build from their tips (the confirm says, per repo, that the code differs from the save), or go to the exact saved commits and leave those worktrees detached; a plain `resume:` is then refused |
//! | `<verb>-no-save` | Environment, beside a mode switch, watch or down, while an item is bound | the verb without the automatic save before it |
//! | `down-volumes` | Environment, when a component keeps a volume | down that also deletes the data volumes; the confirm names the saved states whose snapshots live there (`down` itself always keeps them) |
//!
//! **Automatic saves.** A mode switch, a rebind to another item, `down`
//! and archive save the bound item's state as the first step of their job.
//! A failed save fails the job before anything changes; the `-no-save`
//! twin of the action is how to go ahead anyway.
//!
//! Hooks run through [`crate::world::World::cli`], argv only, under the
//! data-safety hook limit. What a hook prints is never kept or shown: a
//! dump tool's error text can quote a row. The job says only which hook
//! ran and how it ended.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use stack_core::cli::{engine_argv, CliError, Invocation};
use stack_core::definition::{
    ComponentDef, DataDef, DataStrategy, DriverKind, Mode, ScanWhen, SnapshotHooks,
    SnapshotLocation,
};
use stack_core::driver::ResolvedInstance;
use stack_core::images::image_exists;
use stack_core::intent::Desired;
use stack_core::placeholders::{self, Context};
use stack_core::planner::{Change, Plan, ResumeTarget};
use stack_core::runner::{Observation, StackRunner};
use stack_core::states::{
    age, diff_lines, host_data_dir, list_states, prune_plan, snapshot_name, snapshots_in_volumes,
    unique_state_id, ComponentOverride, DataSnapshot, RecipeRecord, SaveReason, SavedState,
    ScriptRecord, StateComponent, StateData, StateItem, STATE_FORMAT,
};
use stack_core::workspace::{c8, ItemRecord};

use crate::actions::{confirm_text, Offered};
use crate::data_safety::{self, CheckStore, SafetyState};
use crate::project::{Binding, Loaded};
use crate::records::JobWork;
use crate::workspaces::{endpoint, fingerprint, job, refused, Accepted, Git};
use crate::world::World;

pub const SAVE_ID: &str = "save-state";
pub const RESUME_PREFIX: &str = "resume:";
pub const RESUME_NO_SAVE_PREFIX: &str = "resume-no-save:";
/// When a branch moved since the save: resume on the branches as they are
/// now, or at the exact saved commits (detached).
pub const RESUME_TIPS_PREFIX: &str = "resume-tips:";
pub const RESUME_TIPS_NO_SAVE_PREFIX: &str = "resume-tips-no-save:";
pub const RESUME_EXACT_PREFIX: &str = "resume-exact:";
pub const RESUME_EXACT_NO_SAVE_PREFIX: &str = "resume-exact-no-save:";
/// The twin of an action that would save first: the same without saving.
pub const NO_SAVE_SUFFIX: &str = "-no-save";
pub const DOWN_VOLUMES_ID: &str = "down-volumes";
pub const SECTION_ID: &str = "saved-states";
/// States listed (and offered for resume) before the rest fold into `more`.
pub const SHOWN: usize = 3;
/// Attempts of a hook run right after its component (re)started, while a
/// database may still be coming up.
pub const AFTER_START_ATTEMPTS: u32 = data_safety::AFTER_UP_ATTEMPTS;
/// How long a resume's blocked components (a missing snapshot, a missing
/// commit) found when it was called are shown in its confirm.
pub const BLOCKS_TTL_SECS: u64 = 600;
/// Snapshots kept per component when `persistence.keep` is not read.
const DEFAULT_SNAPSHOT_KEEP: usize = 10;

/// An automatic or manual save: which item, why, and whether snapshots
/// that live in a volume are skipped (the job deletes that volume).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveSpec {
    pub item: String,
    pub reason: SaveReason,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub skip_in_volume: bool,
}

/// The work item the slot is bound to, when it is one (a review workspace
/// binding has no states).
pub fn bound_item(loaded: &Loaded) -> Option<&str> {
    loaded
        .binding
        .item
        .as_deref()
        .filter(|i| !crate::reviews::is_review_binding(i))
}

/// What a job needs to save, prune or restore.
pub struct Ctx<'a, 'r> {
    pub world: &'a World,
    pub loaded: &'a Loaded,
    pub runner: &'a StackRunner<'r>,
    pub inst: &'a ResolvedInstance,
    pub obs: &'a Observation,
}

// ----------------------------------------------------------------- hooks

/// How a hook ended when it did not succeed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookFailure {
    /// It ran and exited non-zero.
    Exited(String),
    /// It could not be run (not found, timed out, killed).
    NotRun(String),
}

impl HookFailure {
    pub fn text(&self) -> &str {
        match self {
            HookFailure::Exited(t) | HookFailure::NotRun(t) => t,
        }
    }
}

/// Run one hook; its stdout on success. Failures are told in this
/// program's words (exit code, timeout), never the hook's output.
fn run_hook(
    world: &World,
    argv: &[String],
    cwd: &Path,
    attempts: u32,
    what: &str,
    say: &mut dyn FnMut(&str),
) -> Result<String, HookFailure> {
    let attempts = attempts.max(1);
    let mut last = HookFailure::NotRun(format!("the {what} hook did not run"));
    for attempt in 1..=attempts {
        let inv = Invocation::from_argv(argv)
            .cwd(cwd)
            .timeout(data_safety::HOOK_TIMEOUT);
        last = match world.cli.run(&inv) {
            Ok(o) if o.ok() => return Ok(o.stdout),
            Ok(o) => HookFailure::Exited(match o.status {
                Some(c) => format!("the {what} hook exited with code {c}"),
                None => format!("the {what} hook was killed"),
            }),
            Err(CliError::Timeout { seconds, .. }) => {
                HookFailure::NotRun(format!("the {what} hook timed out after {seconds} s"))
            }
            Err(CliError::Spawn { program, message }) => HookFailure::NotRun(format!(
                "the {what} hook could not start `{program}` ({message})"
            )),
            Err(_) => HookFailure::NotRun(format!("the {what} hook failed")),
        };
        if attempt < attempts {
            say(&format!("  {}; trying again", last.text()));
            std::thread::sleep(world.hook_retry);
        }
    }
    Err(last)
}

fn running(obs: &Observation, component: &str) -> bool {
    obs.actual.get(component).is_some_and(|a| a.running)
}

/// The placeholder context of a hook of `component`.
fn hook_context(
    inst: &ResolvedInstance,
    obs: &Observation,
    component: &str,
    snapshot: Option<&str>,
    params: &IndexMap<String, String>,
) -> Context {
    let mut ctx = inst.context_for(component);
    ctx.container = obs
        .ps
        .get(component)
        .map(|p| p.name.clone())
        .filter(|n| !n.is_empty());
    ctx.snapshot = snapshot.map(str::to_string);
    ctx.params = params.clone();
    ctx
}

fn fill(
    argv: &[String],
    ctx: &Context,
    what: &str,
    component: &str,
) -> Result<Vec<String>, String> {
    placeholders::resolve_argv(argv, ctx)
        .map_err(|e| format!("the {what} hook of {component} cannot be filled in ({e})"))
}

fn snapshot_hooks(def: &ComponentDef) -> Option<&SnapshotHooks> {
    def.persistence
        .as_ref()
        .and_then(|p| p.snapshot.as_ref())
        .filter(|s| s.create.is_some())
}

/// The engine's name of `component`'s persistence volume: compose prefixes
/// a named volume with its project (`local-0-core_db-data`).
pub fn volume_name(
    runner: &StackRunner<'_>,
    inst: &ResolvedInstance,
    component: &str,
) -> Option<String> {
    let vol = runner
        .stack
        .components
        .get(component)?
        .persistence
        .as_ref()?
        .volume
        .clone()
        .filter(|v| !v.trim().is_empty())?;
    let projects = runner.projects(inst);
    let project = projects.get(&inst.components.get(component)?.group)?;
    Some(format!("{}_{vol}", project.project))
}

fn host_dir(loaded: &Loaded, item_dir: &Path, def: &ComponentDef) -> PathBuf {
    match def.persistence.as_ref().and_then(|p| p.host_dir.as_ref()) {
        Some(d) if d.is_absolute() => d.clone(),
        Some(d) => loaded.dir.join(d),
        None => host_data_dir(item_dir),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// A relative path that stays inside the project.
fn inside(word: &str) -> bool {
    let p = Path::new(word);
    !word.is_empty()
        && !word.contains('{')
        && p.is_relative()
        && p.components().all(|c| matches!(c, Component::Normal(_)))
}

/// What identifies one recipe script: a project file its argv names
/// (hashed, with the commit that last touched it), else the argv itself
/// (the script lives in the image, which the base digest pins).
pub fn script_record(
    git: &Git<'_>,
    project_dir: &Path,
    hook: &str,
    argv: &[String],
) -> ScriptRecord {
    let file = argv
        .iter()
        .find(|w| inside(w) && project_dir.join(w).is_file())
        .cloned();
    let hash = match &file {
        Some(f) => sha256_hex(&fs::read(project_dir.join(f)).unwrap_or_default()),
        None => sha256_hex(argv.join("\0").as_bytes()),
    };
    let commit = file
        .as_ref()
        .and_then(|f| git.last_commit_of(project_dir, f));
    ScriptRecord {
        hook: hook.to_string(),
        argv: argv.to_vec(),
        file: file.map(|f| f.replace('\\', "/")),
        hash,
        commit,
    }
}

// ------------------------------------------------------------------ save

/// A component's data-safety standing as a state records it: the state,
/// when it was checked, the job that checked it, and why it makes the
/// state sensitive (no check, or a last verdict that was not clean).
fn safety_of(
    loaded: &Loaded,
    instance: &str,
    component: &str,
) -> (String, Option<String>, Option<String>, Option<String>) {
    let def = &loaded.stack.components[component];
    if !data_safety::has_check(def) {
        return (
            "unscanned".into(),
            None,
            None,
            Some(format!("{component}: no data-safety check")),
        );
    }
    match CheckStore::load(&loaded.work_dir(), instance) {
        Err(_) => (
            "error".into(),
            None,
            None,
            Some(format!(
                "{component}: its data-safety verdicts cannot be read"
            )),
        ),
        Ok(store) => match store.components.get(component) {
            None => (
                "unscanned".into(),
                None,
                None,
                Some(format!("{component}: not checked yet")),
            ),
            Some(e) => {
                let (state, _) = data_safety::judged(loaded, e);
                let why = (state != SafetyState::Clean)
                    .then(|| format!("{component}: last data-safety check {}", state.as_str()));
                (
                    state.as_str().to_string(),
                    Some(e.scanned_utc.clone()),
                    e.job_id.clone(),
                    why,
                )
            }
        },
    }
}

fn recipe_record(
    cx: &Ctx<'_, '_>,
    rec: &ItemRecord,
    component: &str,
    d: &DataDef,
    saved: Option<&StateComponent>,
) -> RecipeRecord {
    let git = Git::new(&*cx.world.git_cli);
    let mut scripts = Vec::new();
    if let Some(a) = &d.scrub {
        scripts.push(script_record(&git, &cx.loaded.dir, "scrub", a));
    }
    if let Some(a) = &d.seed {
        scripts.push(script_record(&git, &cx.loaded.dir, "seed", a));
    }
    let seeds: Vec<_> = rec
        .data_seeds
        .get(component)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|s| d.state_seeds.contains_key(&s.name))
        .collect();
    for s in &seeds {
        scripts.push(script_record(
            &git,
            &cx.loaded.dir,
            &format!("stateSeeds.{}", s.name),
            &d.state_seeds[&s.name],
        ));
    }
    RecipeRecord {
        base_image: saved.and_then(|c| c.image.clone()),
        scripts,
        seeds,
    }
}

/// Take one component's snapshot: the `create` hook, then (host) a copy
/// out of the container. `Ok((None, note))` when none could be taken for a
/// reason that is not a failure (it does not run; its volume goes anyway).
#[allow(clippy::too_many_arguments)]
fn take_snapshot(
    cx: &Ctx<'_, '_>,
    spec: &SaveSpec,
    item_dir: &Path,
    state_id: &str,
    component: &str,
    def: &ComponentDef,
    hooks: &SnapshotHooks,
    check: &mut dyn FnMut(&str, data_safety::Trigger),
    say: &mut dyn FnMut(&str),
) -> Result<(Option<DataSnapshot>, Option<String>), String> {
    let p = def
        .persistence
        .as_ref()
        .expect("snapshot hooks live under persistence");
    if p.location == SnapshotLocation::Remote {
        return Ok((None, Some("location remote is not supported yet".into())));
    }
    if spec.skip_in_volume && p.location == SnapshotLocation::InVolume {
        return Ok((
            None,
            Some("not snapshotted: this job deletes the volume it would live in".into()),
        ));
    }
    if !running(cx.obs, component) {
        return Ok((
            None,
            Some("not running when saved, so no snapshot; its data stays in its volume".into()),
        ));
    }
    if data_safety::has_check(def)
        && data_safety::check_times(def).contains(&ScanWhen::BeforeSnapshot)
    {
        check(component, data_safety::Trigger::BeforeSnapshot);
    }
    let name = snapshot_name(&spec.item, component, state_id);
    let ctx = hook_context(cx.inst, cx.obs, component, Some(&name), &IndexMap::new());
    let create = fill(
        hooks.create.as_deref().unwrap_or_default(),
        &ctx,
        "create",
        component,
    )?;
    say(&format!("▶ {component}: snapshot {name}"));
    run_hook(cx.world, &create, &cx.loaded.dir, 1, "create", say)
        .map_err(|e| format!("{component}: {}", e.text()))?;
    let path = match &hooks.path {
        Some(t) => Some(
            placeholders::resolve(t, &ctx)
                .map_err(|e| format!("{component}: the snapshot path cannot be filled in ({e})"))?,
        ),
        None => None,
    };
    let mut snap = DataSnapshot {
        component: component.to_string(),
        name: name.clone(),
        location: p.location,
        where_: String::new(),
        volume: None,
        path: path.clone(),
        host_path: None,
        created_utc: cx.world.clock.now_utc(),
        size_bytes: None,
        engine_hint: hooks.hint.clone(),
        removed_utc: None,
        removed_why: None,
    };
    match p.location {
        SnapshotLocation::Host => {
            let (Some(container), Some(path)) = (ctx.container.clone(), path) else {
                return Err(format!(
                    "{component}: a host copy needs the container and `snapshot.path`"
                ));
            };
            let dir = host_dir(cx.loaded, item_dir, def);
            fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            let dest = dir.join(&name);
            let mut argv = engine_argv(&cx.inst.components[component].engine);
            argv.extend([
                "cp".to_string(),
                format!("{container}:{path}"),
                dest.display().to_string(),
            ]);
            say(&format!(
                "▶ {component}: copy the snapshot out to {}",
                dest.display()
            ));
            run_hook(cx.world, &argv, &cx.loaded.dir, 1, "copy-out", say)
                .map_err(|e| format!("{component}: {}", e.text()))?;
            snap.size_bytes = fs::metadata(&dest).ok().map(|m| m.len());
            snap.where_ = dest.display().to_string();
            snap.host_path = Some(dest);
        }
        _ => {
            let volume = volume_name(cx.runner, cx.inst, component);
            snap.where_ = match (&volume, &snap.path) {
                (Some(v), Some(p)) => format!("volume {v} at {p}"),
                (Some(v), None) => format!("volume {v}"),
                (None, Some(p)) => p.clone(),
                (None, None) => "inside the container".to_string(),
            };
            snap.volume = volume;
        }
    }
    say(&format!("✓ {component}: snapshot {name} ({})", snap.where_));
    Ok((Some(snap), None))
}

/// Save the stack state of `spec.item` from what `cx` observed: the
/// components, the per-instance settings, each data component's recipe
/// and snapshot, `sensitive`. Then prune the item's states. `check` runs
/// a component's data-safety check now (before a snapshot, when asked).
pub fn save_state(
    cx: &Ctx<'_, '_>,
    spec: &SaveSpec,
    job_id: Option<&str>,
    actor: Option<&str>,
    check: &mut dyn FnMut(&str, data_safety::Trigger),
    say: &mut dyn FnMut(&str),
) -> Result<SavedState, String> {
    let work_dir = cx.loaded.work_dir();
    let item_dir = ItemRecord::dir(&work_dir, &spec.item);
    let rec = ItemRecord::load(&item_dir)?.ok_or_else(|| format!("no workspace {}", spec.item))?;
    let now = cx.world.clock.now_utc();
    let id = unique_state_id(&item_dir, &now);
    say(&format!(
        "saving the stack state of {} ({})…",
        rec.item_ref.key,
        spec.reason.describe()
    ));
    let mut components = IndexMap::new();
    for name in cx.loaded.stack.components.keys() {
        let cur = cx.obs.state.component(name);
        let intent = cx.obs.intent.components.get(name);
        if !cur.exists && cur.desired == Desired::Absent && intent.is_none() {
            continue;
        }
        let prov = cx.obs.cache.components.get(name);
        if cx.loaded.stack.components[name].driver == DriverKind::Script {
            components.insert(
                name.clone(),
                StateComponent {
                    desired: cur.desired,
                    script: true,
                    ..Default::default()
                },
            );
            continue;
        }
        components.insert(
            name.clone(),
            StateComponent {
                mode: cur.mode,
                git_ref: cur.git_ref.clone(),
                commit: cur.commit.clone(),
                dirty: cur.dirty.clone(),
                image: cur.image.clone(),
                build_id: prov.and_then(|p| p.build_id.clone()),
                desired: if cur.running {
                    Desired::Running
                } else {
                    cur.desired
                },
                repo: cur
                    .mode
                    .filter(|m| m.uses_repo())
                    .and_then(|m| cx.loaded.stack.repo_of(name, m)),
                script: false,
            },
        );
    }
    let overrides: IndexMap<String, ComponentOverride> = cx
        .obs
        .intent
        .components
        .iter()
        .filter(|(_, i)| i.mode.is_some() || i.git_ref.is_some() || i.channel.is_some())
        .map(|(n, i)| {
            (
                n.clone(),
                ComponentOverride {
                    mode: i.mode,
                    git_ref: i.git_ref.clone(),
                    channel: i.channel.clone(),
                },
            )
        })
        .collect();
    let channel = cx
        .obs
        .intent
        .components
        .values()
        .find_map(|c| c.channel.clone());
    let mut data = IndexMap::new();
    let mut because = Vec::new();
    let mut jobs: Vec<String> = job_id.map(str::to_string).into_iter().collect();
    for (name, def) in &cx.loaded.stack.components {
        if def.data.is_none() && def.persistence.is_none() {
            continue;
        }
        let hooks = snapshot_hooks(def);
        let strategy = def
            .data
            .as_ref()
            .map(|d| d.strategy)
            .unwrap_or(if hooks.is_some() {
                DataStrategy::Snapshot
            } else {
                DataStrategy::Recipe
            });
        let (snapshot, note) = match hooks {
            Some(h) => take_snapshot(cx, spec, &item_dir, &id, name, def, h, check, say)?,
            None => (None, None),
        };
        let recipe = def
            .data
            .as_ref()
            .filter(|d| d.strategy == DataStrategy::Recipe)
            .filter(|d| d.scrub.is_some() || d.seed.is_some() || !d.state_seeds.is_empty())
            .map(|d| recipe_record(cx, &rec, name, d, components.get(name)));
        let (state, checked_utc, check_job, why) = safety_of(cx.loaded, &cx.inst.id, name);
        because.extend(why);
        if let Some(j) = check_job {
            if !jobs.contains(&j) {
                jobs.push(j);
            }
        }
        data.insert(
            name.clone(),
            StateData {
                strategy,
                recipe,
                snapshot,
                note,
                data_safety: state,
                checked_utc,
            },
        );
    }
    let state = SavedState {
        format: STATE_FORMAT,
        id: id.clone(),
        saved_utc: now.clone(),
        reason: spec.reason,
        saved_by: actor.map(str::to_string),
        chappa_stack: env!("CARGO_PKG_VERSION").to_string(),
        item: StateItem {
            source: rec.item_ref.source.clone(),
            key: rec.item_ref.key.clone(),
            folder: rec.key.clone(),
        },
        stack: cx.loaded.stack_name.clone(),
        slot: cx.loaded.slot,
        instance: cx.inst.id.clone(),
        channel,
        components,
        overrides,
        data,
        sensitive: !because.is_empty(),
        sensitive_because: because,
        jobs,
    };
    state.save(&item_dir)?;
    say(&format!(
        "✓ saved state {id}{}",
        if state.sensitive { " (sensitive)" } else { "" }
    ));
    prune_item(cx, &item_dir, say);
    Ok(state)
}

/// What the save-state confirm says it will do.
pub fn save_confirm(loaded: &Loaded, obs: &Observation, rec: &ItemRecord) -> (String, bool) {
    let mut parts: Vec<String> = Vec::new();
    for name in loaded.stack.components.keys() {
        let cur = obs.state.component(name);
        if cur.exists || cur.desired != Desired::Absent {
            let c = StateComponent {
                mode: cur.mode,
                git_ref: cur.git_ref.clone(),
                commit: cur.commit.clone(),
                image: cur.image.clone(),
                script: loaded.stack.components[name].driver == DriverKind::Script,
                build_id: obs
                    .cache
                    .components
                    .get(name)
                    .and_then(|p| p.build_id.clone()),
                ..Default::default()
            };
            parts.push(format!("{name} {}", c.describe()));
        }
    }
    let mut text = format!(
        "Save the stack state of {} in slot {}: {}.",
        rec.item_ref.key,
        loaded.slot,
        if parts.is_empty() {
            "nothing is deployed yet".to_string()
        } else {
            parts.join(", ")
        }
    );
    let mut snaps = Vec::new();
    for (name, def) in &loaded.stack.components {
        let (Some(h), Some(p)) = (snapshot_hooks(def), def.persistence.as_ref()) else {
            continue;
        };
        if p.location == SnapshotLocation::Remote || !running(obs, name) {
            continue;
        }
        let into = match p.location {
            SnapshotLocation::Host => "and copies it out to the host".to_string(),
            _ => format!("into its volume {}", p.volume.clone().unwrap_or_default()),
        };
        let hint = h
            .hint
            .as_deref()
            .map(|x| format!(" ({x})"))
            .unwrap_or_default();
        snaps.push(format!(
            "snapshots {name}{hint} {into} with its create hook"
        ));
    }
    let mutates = !snaps.is_empty();
    if mutates {
        text.push(' ');
        text.push_str(&capital(&snaps.join("; ")));
        text.push('.');
    }
    (text, mutates)
}

fn capital(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

// ----------------------------------------------------------------- prune

/// Delete one snapshot's artifact: the host copy, or the `delete` hook in
/// the running container.
pub fn delete_snapshot(
    cx: &Ctx<'_, '_>,
    snap: &DataSnapshot,
    say: &mut dyn FnMut(&str),
) -> Result<(), String> {
    match snap.location {
        SnapshotLocation::Host => match &snap.host_path {
            Some(p) => match fs::remove_file(p) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(format!("{}: {e}", p.display())),
            },
            None => Ok(()),
        },
        SnapshotLocation::InVolume => {
            let def = cx
                .loaded
                .stack
                .components
                .get(&snap.component)
                .ok_or_else(|| format!("the stack no longer has {}", snap.component))?;
            let Some(delete) = snapshot_hooks(def).and_then(|h| h.delete.as_deref()) else {
                return Err(format!("{} has no delete hook", snap.component));
            };
            if !running(cx.obs, &snap.component) {
                return Err(format!(
                    "{} is not running, so its delete hook cannot run",
                    snap.component
                ));
            }
            let ctx = hook_context(
                cx.inst,
                cx.obs,
                &snap.component,
                Some(&snap.name),
                &IndexMap::new(),
            );
            let argv = fill(delete, &ctx, "delete", &snap.component)?;
            run_hook(cx.world, &argv, &cx.loaded.dir, 1, "delete", say)
                .map(|_| ())
                .map_err(|e| e.text().to_string())
        }
        SnapshotLocation::Remote => Err("remote snapshots are not deleted here".into()),
    }
}

/// Keep the newest `work.keepStates` states of the item and the newest
/// `persistence.keep` snapshots per component among them; delete the
/// artifacts no kept state points at. A snapshot that cannot be deleted
/// keeps the state that points at it, so the next prune tries again.
pub fn prune_item(cx: &Ctx<'_, '_>, item_dir: &Path, say: &mut dyn FnMut(&str)) {
    let states = list_states(item_dir);
    let keep = cx.loaded.stack.work_def().keep_states() as usize;
    let keeps = |c: &str| {
        cx.loaded
            .stack
            .components
            .get(c)
            .and_then(|d| d.persistence.as_ref())
            .map(|p| p.keep as usize)
            .unwrap_or(DEFAULT_SNAPSHOT_KEEP)
    };
    let plan = prune_plan(&states, keep, &keeps);
    if plan.is_empty() {
        return;
    }
    let mut failed: HashSet<String> = HashSet::new();
    for snap in &plan.delete_snapshots {
        match delete_snapshot(cx, snap, say) {
            Ok(()) => say(&format!("· pruned snapshot {}", snap.name)),
            Err(e) => {
                say(&format!("· kept snapshot {}: {e}", snap.name));
                failed.insert(snap.key());
            }
        }
    }
    let now = cx.world.clock.now_utc();
    for (sid, component) in &plan.drop_refs {
        let Some(mut s) = states.iter().find(|s| &s.id == sid).cloned() else {
            continue;
        };
        let Some(snap) = s.data.get_mut(component).and_then(|d| d.snapshot.as_mut()) else {
            continue;
        };
        if failed.contains(&snap.key()) {
            continue;
        }
        snap.removed_utc = Some(now.clone());
        snap.removed_why = Some(format!(
            "pruned: only the newest {} snapshots of {component} are kept",
            keeps(component)
        ));
        let _ = s.save(item_dir);
    }
    for sid in &plan.delete_states {
        let Some(s) = states.iter().find(|s| &s.id == sid) else {
            continue;
        };
        if s.live_snapshots().any(|x| failed.contains(&x.key())) {
            say(&format!(
                "· kept state {sid}: its snapshot could not be deleted"
            ));
            continue;
        }
        let _ = fs::remove_file(SavedState::path(item_dir, sid));
        say(&format!("· pruned state {sid}"));
    }
}

/// On archive: a sensitive state keeps only its references. Host copies
/// of its snapshots are deleted and marked removed (an in-volume snapshot
/// stays with the database's own volume).
pub fn drop_sensitive_host_data(item_dir: &Path, now_utc: &str, say: &mut dyn FnMut(&str)) {
    for mut s in list_states(item_dir) {
        if !s.sensitive {
            continue;
        }
        let mut changed = false;
        for d in s.data.values_mut() {
            let Some(snap) = d.snapshot.as_mut().filter(|x| x.live()) else {
                continue;
            };
            if snap.location != SnapshotLocation::Host {
                continue;
            }
            if let Some(p) = &snap.host_path {
                let _ = fs::remove_file(p);
            }
            snap.removed_utc = Some(now_utc.to_string());
            snap.removed_why = Some("removed on archive: the state is sensitive".into());
            say(&format!(
                "· removed the host copy of {} (state {} is sensitive)",
                snap.name, s.id
            ));
            changed = true;
        }
        if changed {
            let _ = s.save(item_dir);
        }
    }
}

// ---------------------------------------------------------------- resume

/// Whether a snapshot's artifact is there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exists {
    Present,
    Missing,
    /// Cannot be asked now (its container does not run); asked again once
    /// the resume has started it.
    Unknown(String),
}

/// Run `exists` (or look at the host copy).
pub fn snapshot_exists(
    world: &World,
    loaded: &Loaded,
    inst: &ResolvedInstance,
    obs: &Observation,
    snap: &DataSnapshot,
) -> Exists {
    match snap.location {
        SnapshotLocation::Host => match &snap.host_path {
            Some(p) if p.is_file() => Exists::Present,
            _ => Exists::Missing,
        },
        SnapshotLocation::Remote => {
            Exists::Unknown("remote snapshots are not supported yet".into())
        }
        SnapshotLocation::InVolume => {
            let Some(def) = loaded.stack.components.get(&snap.component) else {
                return Exists::Unknown(format!("the stack no longer has {}", snap.component));
            };
            let Some(argv) = snapshot_hooks(def).and_then(|h| h.exists.as_deref()) else {
                return Exists::Unknown(format!("{} has no exists hook", snap.component));
            };
            if !running(obs, &snap.component) {
                return Exists::Unknown(format!(
                    "{} is not running; it is checked once the resume starts it",
                    snap.component
                ));
            }
            let ctx = hook_context(
                inst,
                obs,
                &snap.component,
                Some(&snap.name),
                &IndexMap::new(),
            );
            let argv = match fill(argv, &ctx, "exists", &snap.component) {
                Ok(a) => a,
                Err(e) => return Exists::Unknown(e),
            };
            match run_hook(world, &argv, &loaded.dir, 1, "exists", &mut |_| {}) {
                Ok(_) => Exists::Present,
                Err(HookFailure::Exited(_)) => Exists::Missing,
                Err(HookFailure::NotRun(why)) => Exists::Unknown(why),
            }
        }
    }
}

/// Put a snapshot back: (host) copy it into the container, then `restore`.
pub fn restore_snapshot(
    cx: &Ctx<'_, '_>,
    snap: &DataSnapshot,
    say: &mut dyn FnMut(&str),
) -> Result<(), String> {
    let c = &snap.component;
    let def = cx
        .loaded
        .stack
        .components
        .get(c)
        .ok_or_else(|| format!("the stack no longer has {c}"))?;
    let restore = snapshot_hooks(def)
        .and_then(|h| h.restore.as_deref())
        .ok_or_else(|| format!("{c} has no restore hook"))?;
    let ctx = hook_context(cx.inst, cx.obs, c, Some(&snap.name), &IndexMap::new());
    if snap.location == SnapshotLocation::Host {
        let (Some(container), Some(path), Some(host)) = (
            ctx.container.clone(),
            snap.path.clone(),
            snap.host_path.clone(),
        ) else {
            return Err(format!(
                "{c}: the host copy cannot be put back (no container or path)"
            ));
        };
        let mut argv = engine_argv(&cx.inst.components[c].engine);
        argv.extend([
            "cp".to_string(),
            host.display().to_string(),
            format!("{container}:{path}"),
        ]);
        say(&format!(
            "▶ {c}: copy {} back into the container",
            snap.name
        ));
        run_hook(
            cx.world,
            &argv,
            &cx.loaded.dir,
            AFTER_START_ATTEMPTS,
            "copy-in",
            say,
        )
        .map_err(|e| e.text().to_string())?;
    }
    let argv = fill(restore, &ctx, "restore", c)?;
    say(&format!("▶ {c}: restore {}", snap.name));
    run_hook(
        cx.world,
        &argv,
        &cx.loaded.dir,
        AFTER_START_ATTEMPTS,
        "restore",
        say,
    )
    .map(|_| ())
    .map_err(|e| e.text().to_string())
}

/// Rebuild a recipe component's data in its fresh container: scrub, the
/// data-safety check, the general seed, the state seeds, the check again.
/// `check` runs the component's check and says whether it came out clean
/// (`None`: the component has no check, so nothing gates the seeds).
pub fn replay_recipe(
    cx: &Ctx<'_, '_>,
    component: &str,
    record: Option<&RecipeRecord>,
    check: &mut dyn FnMut(&str) -> Option<bool>,
    say: &mut dyn FnMut(&str),
) -> Result<(), String> {
    let def = cx
        .loaded
        .stack
        .components
        .get(component)
        .ok_or_else(|| format!("the stack no longer has {component}"))?;
    let Some(d) = def.data.as_ref() else {
        return Err(format!("{component} no longer declares a recipe"));
    };
    let git = Git::new(&*cx.world.git_cli);
    let changed = |hook: &str, argv: &[String]| {
        let now = script_record(&git, &cx.loaded.dir, hook, argv);
        record
            .and_then(|r| r.scripts.iter().find(|s| s.hook == hook))
            .is_some_and(|was| was.hash != now.hash)
    };
    let step = |hook: &str,
                argv: &[String],
                params: &IndexMap<String, String>,
                say: &mut dyn FnMut(&str)|
     -> Result<(), String> {
        if changed(hook, argv) {
            say(&format!(
                "note: {component}'s {hook} script changed since the state was saved"
            ));
        }
        let ctx = hook_context(cx.inst, cx.obs, component, None, params);
        let argv = fill(argv, &ctx, hook, component)?;
        say(&format!("▶ {component}: {hook}"));
        run_hook(
            cx.world,
            &argv,
            &cx.loaded.dir,
            AFTER_START_ATTEMPTS,
            hook,
            say,
        )
        .map(|_| ())
        .map_err(|e| format!("{component}: {}", e.text()))
    };
    let none = IndexMap::new();
    let gate = |when: &str,
                check: &mut dyn FnMut(&str) -> Option<bool>,
                say: &mut dyn FnMut(&str)|
     -> Result<(), String> {
        match check(component) {
            Some(true) => Ok(()),
            Some(false) => Err(format!(
                "data safety: {component} is not clean {when}, so the recipe stops there"
            )),
            None => {
                say(&format!(
                    "· {component}: no data-safety check {when}; its data counts as sensitive"
                ));
                Ok(())
            }
        }
    };
    if let Some(a) = &d.scrub {
        step("scrub", a, &none, say)?;
    }
    gate("after scrub", check, say)?;
    if let Some(a) = &d.seed {
        step("seed", a, &none, say)?;
    }
    for s in record.map(|r| r.seeds.as_slice()).unwrap_or_default() {
        match d.state_seeds.get(&s.name) {
            Some(a) => step(&format!("stateSeeds.{}", s.name), a, &s.params, say)?,
            None => say(&format!(
                "· {component}: state seed {} is no longer defined; skipped",
                s.name
            )),
        }
    }
    gate("after the seeds", check, say)
}

/// A worktree to put on a saved commit before the plan runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckoutAt {
    pub component: String,
    pub repo: String,
    pub worktree: PathBuf,
    pub commit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

/// A resume, as confirmed and stored with its job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumeWork {
    /// The item's folder.
    pub item: String,
    pub state: String,
    pub change: Change,
    pub plan: Plan,
    #[serde(default)]
    pub checkouts: Vec<CheckoutAt>,
    /// Components whose snapshot is restored after the plan.
    #[serde(default)]
    pub restores: Vec<String>,
    /// Components whose recipe is replayed after the plan.
    #[serde(default)]
    pub recipes: Vec<String>,
    /// Components left as they are, with why.
    #[serde(default)]
    pub blocked: IndexMap<String, String>,
    /// The slot is bound to the item first (it was not active).
    #[serde(default)]
    pub bind: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub save: Option<SaveSpec>,
}

/// A resume offered or asked for: what the job will do, its confirm and
/// the fingerprint the call must bring back.
#[derive(Debug, Clone)]
pub struct ResumeProposal {
    pub work: ResumeWork,
    pub confirm: String,
    pub fingerprint: String,
    /// The branches that moved since the save.
    pub moved: Vec<Moved>,
}

/// How a resume treats a branch whose tip is no longer the saved commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeVariant {
    /// No branch moved: everything goes to the saved commits, which are
    /// the branch tips.
    Plain,
    /// Stay on each moved branch and build from its tip; the code differs
    /// from the save there.
    Tips,
    /// Go to the exact saved commits; a moved branch's worktree is left
    /// detached there.
    Exact,
}

impl ResumeVariant {
    /// The action id prefix of this variant (and its "without saving" twin).
    pub fn prefix(self, no_save: bool) -> &'static str {
        match (self, no_save) {
            (ResumeVariant::Plain, false) => RESUME_PREFIX,
            (ResumeVariant::Plain, true) => RESUME_NO_SAVE_PREFIX,
            (ResumeVariant::Tips, false) => RESUME_TIPS_PREFIX,
            (ResumeVariant::Tips, true) => RESUME_TIPS_NO_SAVE_PREFIX,
            (ResumeVariant::Exact, false) => RESUME_EXACT_PREFIX,
            (ResumeVariant::Exact, true) => RESUME_EXACT_NO_SAVE_PREFIX,
        }
    }

    /// `(variant, without saving, state id)` of a resume action id.
    pub fn parse_id(id: &str) -> Option<(ResumeVariant, bool, &str)> {
        for v in [
            ResumeVariant::Plain,
            ResumeVariant::Tips,
            ResumeVariant::Exact,
        ] {
            for no_save in [false, true] {
                if let Some(state) = id.strip_prefix(v.prefix(no_save)) {
                    return Some((v, no_save, state));
                }
            }
        }
        None
    }

    fn label(self) -> &'static str {
        match self {
            ResumeVariant::Plain => "",
            ResumeVariant::Tips => " at branch tips",
            ResumeVariant::Exact => " exact commits",
        }
    }
}

/// A repo whose branch tip is no longer the commit the state saved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Moved {
    pub component: String,
    pub repo: String,
    pub branch: String,
    pub saved: String,
    pub tip: String,
}

/// Blocked components found when a resume was called, kept so the next
/// view shows the confirm the call will accept.
#[derive(Default)]
pub struct ResumeBlocks(Mutex<IndexMap<String, Seen>>);

/// Blocked components, and when (clock seconds) they were found.
type Seen = (IndexMap<String, String>, u64);

impl ResumeBlocks {
    pub fn get(&self, item: &str, state: &str, now: u64) -> IndexMap<String, String> {
        let map = self.0.lock().unwrap_or_else(|p| p.into_inner());
        match map.get(&format!("{item}/{state}")) {
            Some((b, at)) if now.saturating_sub(*at) < BLOCKS_TTL_SECS => b.clone(),
            _ => IndexMap::new(),
        }
    }

    pub fn put(&self, item: &str, state: &str, blocks: IndexMap<String, String>, now: u64) {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(format!("{item}/{state}"), (blocks, now));
    }
}

/// `2026-11-14 12:00 UTC`.
pub fn shown_time(utc: &str) -> String {
    let t = utc.replace('T', " ");
    let t = t.trim_end_matches('Z');
    format!("{} UTC", t.get(..16).unwrap_or(t))
}

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        one.to_string()
    } else {
        many.to_string()
    }
}

/// Plan the resume of `state` of the item in `dir`. `live` asks git, the
/// engine and the `exists` hooks now (an action call); otherwise `known`
/// holds what the last call found blocked and `tips` where each repo's
/// branch pointed when the workspaces were last looked at (a view).
///
/// A repo whose branch moved since the save is in [`ResumeProposal::moved`]
/// whatever the variant; a live [`ResumeVariant::Plain`] call is refused
/// then (the view offers the other two instead), and a live call of the
/// other two is refused when nothing moved.
#[allow(clippy::too_many_arguments)]
pub fn resume_proposal(
    world: &World,
    loaded: &Loaded,
    obs: &Observation,
    rec: &ItemRecord,
    state: &SavedState,
    live: bool,
    known: &IndexMap<String, String>,
    tips: &IndexMap<String, String>,
    variant: ResumeVariant,
    no_save: bool,
    now_utc: &str,
) -> Result<ResumeProposal, (u16, &'static str, String)> {
    let work_dir = loaded.work_dir();
    let bound = Binding::of_item(&work_dir, &rec.key);
    let runner = loaded
        .plain_runner(world, now_utc)
        .with_binding(bound.worktrees.clone(), Some(loaded.guard()));
    let (inst, _) = runner
        .instance(loaded.slot)
        .map_err(|e| (409, "definition", e.to_string()))?;
    let git = Git::new(&*world.git_cli);
    let mut blocked: IndexMap<String, String> = if live { IndexMap::new() } else { known.clone() };
    let mut targets: IndexMap<String, ResumeTarget> = IndexMap::new();
    let mut checkouts = Vec::new();
    let mut dirty = Vec::new();
    let mut restores = Vec::new();
    let mut recipes = Vec::new();
    let mut from_worktrees = Vec::new();
    let mut moved: Vec<Moved> = Vec::new();
    // Under `Tips`, the tip a moved component builds from instead.
    let mut at_tip: IndexMap<String, String> = IndexMap::new();
    // Script components wanted running: their `up` script runs again.
    let mut scripts: Vec<String> = Vec::new();
    for (name, c) in &state.components {
        if blocked.contains_key(name) {
            continue;
        }
        let Some(def) = loaded.stack.components.get(name) else {
            blocked.insert(name.clone(), "the stack no longer has it".into());
            continue;
        };
        if c.script || def.driver == DriverKind::Script {
            if def.driver != DriverKind::Script {
                blocked.insert(name.clone(), "it is no longer a script component".into());
            } else if c.desired == Desired::Running {
                scripts.push(name.clone());
            }
            continue;
        }
        let Some(mode) = c.mode else {
            continue;
        };
        if !def.has_mode(mode) {
            blocked.insert(name.clone(), format!("it no longer has a {mode} mode"));
            continue;
        }
        let wanted = c.desired == Desired::Running;
        if wanted && mode.uses_repo() {
            let repo = c.repo.clone().or_else(|| loaded.stack.repo_of(name, mode));
            let wt = repo.as_ref().and_then(|r| bound.worktrees.get(r)).cloned();
            let branch = repo
                .as_ref()
                .and_then(|r| rec.repos.get(r))
                .map(|r| r.branch.clone())
                .or_else(|| c.git_ref.clone());
            if let (Some(wt), Some(commit), Some(b), Some(r)) = (&wt, &c.commit, &branch, &repo) {
                let tip = if live {
                    git.rev_parse(wt, &format!("refs/heads/{b}")).ok()
                } else {
                    tips.get(r).cloned()
                };
                if let Some(tip) = tip.filter(|t| t != commit) {
                    moved.push(Moved {
                        component: name.clone(),
                        repo: r.clone(),
                        branch: b.clone(),
                        saved: commit.clone(),
                        tip,
                    });
                }
            }
            let tip_here = moved
                .iter()
                .find(|m| m.component == *name && variant == ResumeVariant::Tips)
                .cloned();
            if wt.is_some() && tip_here.is_none() {
                from_worktrees.push(name.clone());
            }
            if let (Some(m), Some(wt)) = (&tip_here, &wt) {
                // Stay on the branch, at its tip: no pin, built from it.
                if live {
                    match git.uncommitted(wt) {
                        Ok(n) if n > 0 => dirty.push(format!(
                            "{} has {n} uncommitted change{} ({})",
                            m.repo,
                            if n == 1 { "" } else { "s" },
                            wt.display()
                        )),
                        Ok(_) => {}
                        Err(e) => dirty.push(format!("{} cannot be checked ({e})", wt.display())),
                    }
                    checkouts.push(CheckoutAt {
                        component: name.clone(),
                        repo: m.repo.clone(),
                        worktree: wt.clone(),
                        commit: m.tip.clone(),
                        branch: Some(m.branch.clone()),
                    });
                }
                at_tip.insert(name.clone(), m.tip.clone());
            } else if live {
                let engine = &inst.components[name].engine;
                let image_ok = mode == Mode::Source
                    && c.image
                        .as_deref()
                        .is_some_and(|i| image_exists(&*world.cli, engine, i).unwrap_or(false));
                match (&wt, &c.commit) {
                    (Some(wt), Some(commit)) => {
                        match git.uncommitted(wt) {
                            Ok(n) if n > 0 => dirty.push(format!(
                                "{} has {n} uncommitted change{} ({})",
                                repo.clone().unwrap_or_default(),
                                if n == 1 { "" } else { "s" },
                                wt.display()
                            )),
                            Ok(_) => {}
                            Err(e) => {
                                dirty.push(format!("{} cannot be checked ({e})", wt.display()))
                            }
                        }
                        let have = git.rev_parse(wt, commit).is_ok();
                        if !have && !image_ok {
                            blocked.insert(
                                name.clone(),
                                format!(
                                    "commit {} is not in its worktree of {}; fetch it, or resume a newer state",
                                    c8(commit),
                                    repo.clone().unwrap_or_default()
                                ),
                            );
                            continue;
                        }
                        if mode == Mode::Source && !image_ok && c.dirty.is_some() {
                            blocked.insert(
                                name.clone(),
                                "it was built from uncommitted changes and that image is gone"
                                    .into(),
                            );
                            continue;
                        }
                        if have
                            && git.rev_parse(wt, "HEAD").ok().as_deref() != Some(commit.as_str())
                        {
                            checkouts.push(CheckoutAt {
                                component: name.clone(),
                                repo: repo.clone().unwrap_or_default(),
                                worktree: wt.clone(),
                                commit: commit.clone(),
                                branch: c.git_ref.clone(),
                            });
                        }
                    }
                    (None, Some(commit)) if !image_ok => {
                        // No worktree of the item: the shared clone is never
                        // moved, so it must already be at the commit.
                        let clone = runner.repo_path(name, mode).ok();
                        let head = clone.as_ref().and_then(|p| git.rev_parse(p, "HEAD").ok());
                        if head.as_deref() != Some(commit.as_str()) {
                            blocked.insert(
                                name.clone(),
                                format!(
                                    "the item has no worktree of {} and the shared clone is not at {}",
                                    repo.clone().unwrap_or_default(),
                                    c8(commit)
                                ),
                            );
                            continue;
                        }
                    }
                    _ => {}
                }
            }
        }
        if wanted && mode == Mode::Prebuilt && c.image.is_none() {
            blocked.insert(name.clone(), "no digest was recorded for it".into());
            continue;
        }
        let mut fresh = false;
        if let Some(d) = state.data.get(name) {
            match d.snapshot.as_ref().filter(|s| s.live()) {
                Some(snap) if wanted => {
                    if live && snapshot_exists(world, loaded, &inst, obs, snap) == Exists::Missing {
                        blocked.insert(
                            name.clone(),
                            format!(
                                "snapshot {} is missing; it was expected in {}",
                                snap.name, snap.where_
                            ),
                        );
                        continue;
                    }
                    restores.push(name.clone());
                }
                None if wanted && d.recipe.is_some() => {
                    recipes.push(name.clone());
                    fresh = true;
                }
                _ => {}
            }
        }
        let pin = if at_tip.contains_key(name) {
            None
        } else {
            Some(c.pin())
        };
        targets.insert(
            name.clone(),
            ResumeTarget {
                mode,
                git_ref: c.git_ref.clone(),
                pin,
                desired: c.desired,
                fresh,
            },
        );
    }
    // A moved branch only counts for a component that is resumed.
    moved.retain(|m| targets.contains_key(&m.component));
    if live {
        match (variant, moved.is_empty()) {
            (ResumeVariant::Plain, false) => {
                return Err((
                    409,
                    "stale-plan",
                    format!(
                        "since the state was saved, {}; choose Resume at branch tips or Resume exact commits",
                        moved_text(&moved)
                    ),
                ))
            }
            (ResumeVariant::Tips | ResumeVariant::Exact, true) => {
                return Err((
                    409,
                    "stale-plan",
                    "no branch moved since the state was saved; use Resume".to_string(),
                ))
            }
            _ => {}
        }
    }
    if !dirty.is_empty() {
        return Err((
            409,
            "dirty",
            format!(
                "not resumed: commit or stash first, a worktree the state builds from has uncommitted changes: {}",
                dirty.join("; ")
            ),
        ));
    }
    let change = Change::Resume {
        item: Some(rec.key.clone()),
        targets: targets.clone(),
        scripts: scripts.clone(),
    };
    let mut plan = runner
        .plan(obs, &change)
        .map_err(|e| (409, "plan", e.to_string()))?;
    plan.channel = state.channel.clone();
    runner
        .guard_plan(&inst, obs, &plan)
        .map_err(|e| (409, "shared-checkout", e.to_string()))?;

    let active = loaded.binding.item.as_deref() == Some(rec.key.as_str());
    let bind = !active;
    let previous = bound_item(loaded)
        .filter(|p| *p != rec.key)
        .map(str::to_string);
    let save = match (&previous, no_save) {
        (Some(p), false) => Some(SaveSpec {
            item: p.clone(),
            reason: SaveReason::BeforeRebind,
            skip_in_volume: false,
        }),
        _ => None,
    };
    let mut parts = vec![format!(
        "Resume {} from the state saved {} ({}, {}).",
        rec.item_ref.key,
        shown_time(&state.saved_utc),
        age(&state.saved_utc, now_utc),
        state.reason.describe()
    )];
    if bind {
        parts.push(match (&previous, &save) {
            (Some(p), Some(_)) => format!(
                "Saves the stack state of {p} first, then binds slot {} to {}.",
                loaded.slot, rec.item_ref.key
            ),
            (Some(p), None) => format!(
                "Binds slot {} to {} without saving the stack state of {p} first.",
                loaded.slot, rec.item_ref.key
            ),
            (None, _) => format!("Binds slot {} to {}.", loaded.slot, rec.item_ref.key),
        });
    } else {
        parts.push("What runs now is replaced and not saved; save it first to keep it.".into());
    }
    let mut shown = state.clone();
    shown
        .components
        .retain(|n, _| targets.contains_key(n) || scripts.contains(n));
    for (n, tip) in &at_tip {
        if let Some(c) = shown.components.get_mut(n) {
            c.commit = Some(tip.clone());
            c.image = None;
            c.dirty = None;
        }
    }
    let diff = diff_lines(&shown, &obs.state);
    if diff.iter().all(|l| l.starts_with("unchanged: ")) {
        parts.push("Nothing differs from what runs now.".into());
    } else {
        parts.push(format!("Changes: {}.", diff.join("; ")));
    }
    if !moved.is_empty() {
        parts.push(format!("Since the save, {}.", moved_text(&moved)));
        let each: Vec<String> = moved
            .iter()
            .map(|m| match variant {
                ResumeVariant::Tips => format!(
                    "{} builds from {} at {}, not the saved {}, so its code differs from the save",
                    m.repo,
                    m.branch,
                    c8(&m.tip),
                    c8(&m.saved)
                ),
                _ => format!(
                    "the worktree of {} is left detached at {} (switch it back to {} afterwards)",
                    m.repo,
                    c8(&m.saved),
                    m.branch
                ),
            })
            .collect();
        parts.push(match variant {
            ResumeVariant::Tips => format!("Resumes at the branch tips: {}.", each.join("; ")),
            _ => format!("Resumes the exact saved commits: {}.", each.join("; ")),
        });
    }
    if !from_worktrees.is_empty() {
        parts.push(format!(
            "The worktrees of {} go to the saved commits.",
            from_worktrees.join(", ")
        ));
    }
    for r in &restores {
        if let Some(s) = state.data.get(r).and_then(|d| d.snapshot.as_ref()) {
            parts.push(format!(
                "Restores {r} from snapshot {} ({} old): this overwrites the database {r}.",
                s.name,
                age(&s.created_utc, now_utc).trim_end_matches(" ago")
            ));
        }
    }
    if !recipes.is_empty() {
        let seeds: Vec<String> = recipes
            .iter()
            .filter_map(|r| state.data.get(r).and_then(|d| d.recipe.as_ref()))
            .flat_map(|x| x.seeds.iter().map(|s| s.name.clone()))
            .collect();
        parts.push(format!(
            "{} {} rebuilt from {} recipe (scrub, seed{}): data typed in during the session is lost unless a state seed recreates it.",
            recipes.join(", "),
            plural(recipes.len(), "is", "are"),
            plural(recipes.len(), "its", "their"),
            if seeds.is_empty() {
                String::new()
            } else {
                format!(", state seeds {}", seeds.join(", "))
            }
        ));
    }
    if !scripts.is_empty() {
        parts.push(format!(
            "{} {}: {} `up` script{} again, with the environment of {}'s worktrees.",
            scripts.join(", "),
            stack_core::states::SCRIPT_RESTORE_NOTE,
            plural(scripts.len(), "its", "their"),
            plural(scripts.len(), " runs", "s run"),
            rec.item_ref.key
        ));
    }
    for (c, why) in &blocked {
        parts.push(format!("Not resumed: {c} ({why})."));
    }
    let steps = confirm_text(&plan);
    if !steps.is_empty() {
        parts.push(steps);
    }
    if state.sensitive {
        parts.push(format!(
            "The state is sensitive ({}).",
            state.sensitive_because.join("; ")
        ));
    }
    let confirm = parts.join(" ");
    let fp = fingerprint(&obs.state.fingerprint(), &confirm);
    Ok(ResumeProposal {
        work: ResumeWork {
            item: rec.key.clone(),
            state: state.id.clone(),
            change,
            plan,
            checkouts,
            restores,
            recipes,
            blocked,
            bind,
            save,
        },
        confirm,
        fingerprint: fp,
        moved,
    })
}

/// "api's branch task/ABC-1 moved from 1a2b3c4d to 5e6f7a8b", per repo.
fn moved_text(moved: &[Moved]) -> String {
    moved
        .iter()
        .map(|m| {
            format!(
                "{}'s branch {} moved from {} to {}",
                m.repo,
                m.branch,
                c8(&m.saved),
                c8(&m.tip)
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

// ----------------------------------------------------------- the views

/// What the overlay of one item gets from its states.
pub struct OverlayParts {
    pub badges: Vec<Value>,
    pub section: Option<Value>,
    pub actions: Vec<Value>,
}

fn state_row(s: &SavedState, now_utc: &str) -> Value {
    let mut bits = vec![age(&s.saved_utc, now_utc), s.reason.describe().to_string()];
    if let Some(by) = &s.saved_by {
        bits.push(format!("by {by}"));
    }
    let snaps = s.live_snapshots().count();
    if snaps > 0 {
        bits.push(format!(
            "{snaps} snapshot{}",
            if snaps == 1 { "" } else { "s" }
        ));
    }
    let mut row = json!({ "label": shown_time(&s.saved_utc), "value": bits.join(" · ") });
    let mut badges = Vec::new();
    if s.sensitive {
        badges.push(
            json!({ "text": "sensitive", "tone": "warn", "title": s.sensitive_because.join("; ") }),
        );
    }
    let gone: Vec<String> = s
        .data
        .values()
        .filter_map(|d| d.snapshot.as_ref())
        .filter(|x| !x.live())
        .map(|x| {
            format!(
                "{}: {}",
                x.component,
                x.removed_why.clone().unwrap_or_default()
            )
        })
        .collect();
    if !gone.is_empty() {
        badges
            .push(json!({ "text": "snapshot removed", "tone": "muted", "title": gone.join("; ") }));
    }
    if !badges.is_empty() {
        row["badges"] = Value::Array(badges);
    }
    row
}

/// The Saved states section, its count badge, Save and the Resume actions.
///
/// `tips` is where each repo's branch pointed when the workspaces were last
/// looked at: a state whose branches moved since gets the two variants
/// instead of one Resume.
#[allow(clippy::too_many_arguments)]
pub fn overlay_parts(
    world: &World,
    loaded: &Loaded,
    obs: Option<&Observation>,
    dir: &Path,
    rec: &ItemRecord,
    blocks: &ResumeBlocks,
    tips: &IndexMap<String, String>,
    now_utc: &str,
) -> OverlayParts {
    let states = list_states(dir);
    let active = loaded.binding.item.as_deref() == Some(rec.key.as_str());
    let mut actions = Vec::new();
    if active {
        if let Some(obs) = obs {
            let (confirm, mutates) = save_confirm(loaded, obs, rec);
            actions.push(endpoint(
                SAVE_ID,
                "Save state",
                mutates,
                Some(confirm),
                json!({}),
            ));
        }
    }
    let mut badges = Vec::new();
    let mut section = None;
    if !states.is_empty() {
        badges.push(json!({
            "text": format!("{} saved state{}", states.len(), if states.len() == 1 { "" } else { "s" }),
            "tone": "info",
        }));
        let rows: Vec<Value> = states.iter().map(|s| state_row(s, now_utc)).collect();
        let mut sec = json!({
            "id": SECTION_ID,
            "title": "Saved states",
            "rows": rows[..rows.len().min(SHOWN)],
        });
        if rows.len() > SHOWN {
            sec["more"] = json!({
                "label": format!("{} older", rows.len() - SHOWN),
                "rows": rows[SHOWN..],
            });
        }
        let mut resumes = Vec::new();
        if let Some(obs) = obs {
            let now = world.clock.now_secs();
            for s in states.iter().take(SHOWN) {
                let known = blocks.get(&rec.key, &s.id, now);
                let propose = |variant, no_save| {
                    resume_proposal(
                        world, loaded, obs, rec, s, false, &known, tips, variant, no_save, now_utc,
                    )
                };
                let Ok(plain) = propose(ResumeVariant::Plain, false) else {
                    continue;
                };
                let variants = if plain.moved.is_empty() {
                    vec![ResumeVariant::Plain]
                } else {
                    vec![ResumeVariant::Tips, ResumeVariant::Exact]
                };
                for variant in variants {
                    let Ok(p) = propose(variant, false) else {
                        continue;
                    };
                    let saves = p.work.save.clone();
                    let when = shown_time(&s.saved_utc);
                    resumes.push(endpoint(
                        &format!("{}{}", variant.prefix(false), s.id),
                        &format!("Resume {when}{}", variant.label()),
                        true,
                        Some(p.confirm),
                        json!({ "fingerprint": p.fingerprint, "state": s.id }),
                    ));
                    // Only when resuming would save another item's state first.
                    if let (Some(other), Ok(p)) = (saves, propose(variant, true)) {
                        resumes.push(endpoint(
                            &format!("{}{}", variant.prefix(true), s.id),
                            &format!(
                                "Resume {when}{} without saving {}",
                                variant.label(),
                                other.item
                            ),
                            true,
                            Some(p.confirm),
                            json!({ "fingerprint": p.fingerprint, "state": s.id }),
                        ));
                    }
                }
            }
        }
        if !resumes.is_empty() {
            sec["actions"] = Value::Array(resumes);
        }
        section = Some(sec);
    }
    OverlayParts {
        badges,
        section,
        actions,
    }
}

/// Whether `id` is one of this module's overlay actions.
pub fn handles(id: &str) -> bool {
    id == SAVE_ID || ResumeVariant::parse_id(id).is_some()
}

/// Accept (or refuse) `save-state` and `resume:*` on an item's overlay.
#[allow(clippy::too_many_arguments)]
pub fn accept(
    world: &World,
    loaded: &Loaded,
    obs: Option<&Observation>,
    dir: &Path,
    rec: &ItemRecord,
    id: &str,
    params: &Value,
    confirmed: bool,
    blocks: &ResumeBlocks,
    now_utc: &str,
) -> Accepted {
    let Some(obs) = obs else {
        return refused(409, "observe", "cannot look at the stack now");
    };
    if id == SAVE_ID {
        if loaded.binding.item.as_deref() != Some(rec.key.as_str()) {
            return refused(
                409,
                "not-offered",
                format!(
                    "{} is not active in slot {}; Save state saves the stack the slot runs for it",
                    rec.item_ref.key, loaded.slot
                ),
            );
        }
        let (confirm, mutates) = save_confirm(loaded, obs, rec);
        if mutates && !confirmed {
            return refused(
                409,
                "confirmation-required",
                "a mutating action must be confirmed",
            );
        }
        return Accepted::Job {
            offered: Box::new(Offered {
                id: SAVE_ID.to_string(),
                key: Some(rec.key.clone()),
                label: "Save state".to_string(),
                confirm,
                work: JobWork::SaveState {
                    spec: SaveSpec {
                        item: rec.key.clone(),
                        reason: SaveReason::Manual,
                        skip_in_volume: false,
                    },
                },
            }),
            mutates,
        };
    }
    let Some((variant, no_save, sid)) = ResumeVariant::parse_id(id) else {
        return refused(409, "not-offered", format!("`{id}` is not a resume action"));
    };
    let state = match SavedState::load(dir, sid) {
        Ok(Some(s)) => s,
        Ok(None) => {
            return refused(
                409,
                "not-offered",
                format!("{} has no saved state {sid}", rec.item_ref.key),
            )
        }
        Err(e) => return refused(500, "state", e),
    };
    let p = match resume_proposal(
        world,
        loaded,
        obs,
        rec,
        &state,
        true,
        &IndexMap::new(),
        &IndexMap::new(),
        variant,
        no_save,
        now_utc,
    ) {
        Ok(p) => p,
        Err((status, kind, message)) => return refused(status, kind, message),
    };
    blocks.put(
        &rec.key,
        &state.id,
        p.work.blocked.clone(),
        world.clock.now_secs(),
    );
    if !confirmed {
        return refused(
            409,
            "confirmation-required",
            "a mutating action must be confirmed",
        );
    }
    if params.get("fingerprint").and_then(Value::as_str) != Some(p.fingerprint.as_str()) {
        return refused(
            409,
            "stale-plan",
            format!(
                "the stack, the item or its snapshots changed since this was shown; what would run now is: {}",
                p.confirm
            ),
        );
    }
    job(
        id,
        &rec.key,
        &format!("Resume {}{}", shown_time(&state.saved_utc), variant.label()),
        p.confirm,
        JobWork::Resume {
            resume: Box::new(p.work),
        },
    )
}

// ----------------------------------------------------- Environment side

/// Why an Environment action saves the bound item first.
fn save_reason(o: &Offered) -> Option<SaveReason> {
    match &o.work {
        JobWork::Plan { change, .. } => match change {
            Change::SwitchMode { .. } | Change::Watch { .. } => Some(SaveReason::BeforeModeSwitch),
            Change::Down { .. } => Some(SaveReason::BeforeDown),
            _ => None,
        },
        _ => None,
    }
}

/// While an item is bound, every mode switch, watch and down saves its
/// state first, and gets a `-no-save` twin that does not.
pub fn with_auto_save(loaded: &Loaded, offered: Vec<Offered>) -> Vec<Offered> {
    let Some(item) = bound_item(loaded).map(str::to_string) else {
        return offered;
    };
    let mut out = Vec::new();
    for o in offered {
        let Some(reason) = save_reason(&o) else {
            out.push(o);
            continue;
        };
        let mut saving = o.clone();
        if let JobWork::Plan {
            save,
            remove_volumes,
            ..
        } = &mut saving.work
        {
            *save = Some(SaveSpec {
                item: item.clone(),
                reason,
                skip_in_volume: *remove_volumes,
            });
        }
        saving.confirm = format!(
            "Saves the stack state of {item} first (if that fails, nothing else runs). {}",
            o.confirm
        );
        let mut plain = o;
        plain.id = format!("{}{NO_SAVE_SUFFIX}", plain.id);
        plain.label = format!("{} without saving", plain.label);
        plain.confirm = format!(
            "{} Without saving the stack state of {item} first.",
            plain.confirm
        );
        out.push(saving);
        out.push(plain);
    }
    out
}

/// `down` that also deletes the data volumes, offered beside `down` when a
/// component keeps one. Its confirm lists the saved states whose snapshots
/// live in them.
pub fn down_volumes_action(
    loaded: &Loaded,
    runner: &StackRunner<'_>,
    inst: &ResolvedInstance,
    obs: &Observation,
) -> Option<Offered> {
    let volumes: Vec<String> = loaded
        .stack
        .components
        .keys()
        .filter_map(|c| volume_name(runner, inst, c))
        .collect();
    if volumes.is_empty() {
        return None;
    }
    let change = Change::Down {
        keep_groups: Vec::new(),
    };
    let plan = runner.plan(obs, &change).ok()?;
    if plan.steps.is_empty() {
        return None;
    }
    let uses = snapshots_in_volumes(&loaded.work_dir(), &volumes);
    let listing = if uses.is_empty() {
        "No saved state keeps a snapshot there.".to_string()
    } else {
        format!(
            "These saved states keep snapshots there and lose them: {}. Down (without this) keeps the volumes.",
            uses.iter()
                .map(|u| format!("{} {} ({})", u.item, u.state, u.component))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let steps = confirm_text(&plan).replace(
        " Containers are removed; volumes are kept.",
        " Containers and these volumes are removed.",
    );
    Some(Offered {
        id: DOWN_VOLUMES_ID.to_string(),
        key: None,
        label: "Down and delete data".to_string(),
        confirm: format!(
            "Down and DELETE the data volumes {}. {listing} {steps}",
            volumes.join(", ")
        ),
        work: JobWork::Plan {
            change,
            plan: Box::new(plan),
            save: None,
            remove_volumes: true,
        },
    })
}

/// Every Environment action offered now: the stack verbs, `down-volumes`
/// after `down`, and the automatic save (with its `-no-save` twins) while
/// an item is bound.
pub fn offered(
    loaded: &Loaded,
    runner: &StackRunner<'_>,
    inst: &ResolvedInstance,
    obs: &Observation,
    channel: &str,
) -> Vec<Offered> {
    let mut all = crate::actions::all_actions(runner, inst, obs, channel);
    if let Some(dv) = down_volumes_action(loaded, runner, inst, obs) {
        let at = all
            .iter()
            .position(|o| o.id == "down" && o.key.is_none())
            .map(|i| i + 1)
            .unwrap_or(all.len());
        all.insert(at, dv);
    }
    with_auto_save(loaded, all)
}
