//! The verbs, end to end: observe the instance, plan a change, turn the plan
//! into job steps, report status.
//!
//! [`StackRunner`] holds what every verb needs (the definition, the project
//! and work folders, the engine CLI, git, the project-command table, the
//! gateway resolver) and exposes:
//!
//! | call | does |
//! |---|---|
//! | [`StackRunner::instance`] | resolve a slot with its intent record |
//! | [`StackRunner::observe`] | `compose ps` + `inspect` per project, process states from the caller, gateways per engine → the planner's [`InstanceState`] |
//! | [`StackRunner::plan`] | the planner over that state (`up`, `start`, `stop`, `down`, `rebuild`, mode switch, `watch`) |
//! | [`StackRunner::apply`] | the plan as [`JobStep`]s: override files to write, commands to run, the records to write afterwards. Refuses a plan whose fingerprint no longer matches the observation |
//! | [`StackRunner::status`] | the nine states with probes, provenance, staleness, uptime, links and a `logs -f` action per component |
//! | [`StackRunner::checks`] | the pre-up checks through a [`Checker`] |
//! | [`StackRunner::gc_plan`] / [`StackRunner::gc_apply`] | unreferenced images and build cache, as a preview then as steps |
//! | [`StackRunner::activate`] | bind a work item to a slot |
//! | [`StackRunner::script_run`] / [`StackRunner::script_confirm`] | a script component's script for one verb, resolved; the confirm that runs it |
//!
//! Script components ([`crate::script`]) are observed through a
//! [`ScriptOutputs`] the caller hands in ([`StackRunner::with_scripts`]):
//! their states come from what their status scripts last reported (or, with
//! none, from the intent record and the last run), never from waiting on a
//! script. Their part of the planner's state is the intent record alone.
//!
//! Nothing long runs here. A build, a pull, an `up`, a dev server all come
//! back as run blocks; the quick engine questions (`image inspect`,
//! `manifest inspect`, `ps`) are asked in place.
//!
//! # Step order of an applied plan
//!
//! 1. the override file of every compose project the plan touches (so the
//!    `up` that follows sees the new images, ports and gateway);
//! 2. the plan's steps, in the planner's order, each as a run (or a
//!    process stop the host performs, or a note when nothing is needed:
//!    an image whose content key already exists is reused, not rebuilt);
//! 3. the provenance cache and the intent record.
//!
//! Checkouts are not performed here: the item-workspace layer owns
//! worktrees. A checkout step whose repo is already on the planned ref is
//! a note; any other is refused with [`RunnerError::CheckoutUnsupported`].
//!
//! A runner may be bound to a work item ([`StackRunner::with_binding`]):
//! a component whose clone the item has a worktree of builds and runs in
//! that worktree. With a [`SharedCheckoutGuard`], building or running from
//! a shared clone the item does not cover is refused unless the clone is
//! allowed, so a stack never quietly builds whatever branch a shared clone
//! happens to be on.

use indexmap::{IndexMap, IndexSet};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::checks::EngineFacts;
use crate::cli::{engine_argv, engine_invocation, CliError, EngineCli, Invocation};
use crate::compose::{
    self, actual_from, parse_inspect, parse_ps, parse_utc, uptime_text, ComposeProject, Gateway,
    GatewayResolver, InspectEntry, OverrideError, OverrideInputs, PsEntry,
};
use crate::definition::{
    ComponentKind, ContainerOs, DriverKind, EngineRef, Mode, ModeDef, StackDef,
};
use crate::driver::{
    checks_for, checks_for_plan, component_state, run_checks, Actual, CheckResult, Checker,
    ComponentStatus, EngineKind, ResolveError, ResolvedInstance,
};
use crate::images::{
    self, image_exists, pinned_reference, resolve_digest, resolve_tag, source_build,
    tagged_reference, CatalogEntry, ProvenanceCache, SourceBuild, LABEL_CONTENT_KEY,
};
use crate::intent::{instance_id, ActiveRecord, Desired, IntentError, IntentRecord};
use crate::jobs::{JobStep, RunBlock};
use crate::placeholders::{self, PlaceholderError};
use crate::planner::{
    plan as plan_change, Change, ComponentCurrent, InstanceState, Plan, PlanError, Step, Verb,
};
use crate::probe::{self, ProbeHttp, ProbeResult, DEFAULT_PROBE_TIMEOUT};
use crate::process::{self, actual_from_process, CommandLookup, ProcessError, ProcessStatus};
use crate::provenance::{
    prebuilt_staleness, worktree_staleness, GitError, GitFacts, LabelMap, Provenance, Staleness,
};
use crate::script::{
    self, AlertTone, NoScriptOutputs, PlanPreview, ReportedAlert, ScriptBinding, ScriptError,
    ScriptOutputs, ScriptReport, ScriptRun, ScriptRuns, ScriptVerb, ScriptView,
};

#[derive(Debug, thiserror::Error)]
pub enum RunnerError {
    #[error(transparent)]
    Resolve(#[from] ResolveError),
    #[error(transparent)]
    Intent(#[from] IntentError),
    #[error(transparent)]
    Plan(#[from] PlanError),
    #[error(transparent)]
    Cli(#[from] CliError),
    #[error(transparent)]
    Git(#[from] GitError),
    #[error(transparent)]
    Process(#[from] ProcessError),
    #[error(transparent)]
    Override(#[from] OverrideError),
    #[error(transparent)]
    Script(#[from] ScriptError),
    #[error("component `{component}`: {source}")]
    Placeholder {
        component: String,
        #[source]
        source: PlaceholderError,
    },
    #[error("component `{component}` has no `{mode}` mode")]
    NoMode { component: String, mode: Mode },
    #[error("component `{component}`: `{mode}` mode names no repo")]
    NoRepo { component: String, mode: Mode },
    #[error("component `{component}` is on `{now}`, not `{wanted}`; checkouts are the workspace layer's job")]
    CheckoutUnsupported {
        component: String,
        wanted: String,
        now: String,
    },
    #[error("component `{component}` has no compose project")]
    NoProject { component: String },
    #[error("component `{component}` would {verb} from the shared clone `{clone}`, on whatever branch that clone has checked out; activate a work item with a `{clone}` worktree, or list `{clone}` in {file} to allow building from the shared clone")]
    SharedCheckout {
        component: String,
        clone: String,
        verb: &'static str,
        file: PathBuf,
    },
    #[error("{0}")]
    Io(String),
}

/// Options an apply may carry beyond the plan.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyOptions {
    /// Pass `--volumes` to `compose down`. Never implied: a named
    /// persistence volume holds data a plain `down` must keep.
    pub remove_volumes: bool,
}

/// What `apply` produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Applied {
    pub steps: Vec<JobStep>,
    pub notes: Vec<String>,
    /// The intent record as it will be once the steps ran.
    pub intent: IntentRecord,
}

impl Applied {
    pub fn runs(&self) -> Vec<&RunBlock> {
        self.steps.iter().filter_map(JobStep::as_run).collect()
    }
}

/// Everything learned about an instance in one look.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Observation {
    pub intent: IntentRecord,
    pub cache: ProvenanceCache,
    pub state: InstanceState,
    pub actual: IndexMap<String, Actual>,
    #[serde(default)]
    pub ps: IndexMap<String, PsEntry>,
    #[serde(default)]
    pub containers: IndexMap<String, InspectEntry>,
    /// Current gateway per container engine name.
    #[serde(default)]
    pub gateways: IndexMap<String, Gateway>,
    /// Engines that could not be asked, with the reason; their components
    /// read as not deployed.
    #[serde(default)]
    pub notes: Vec<String>,
    /// What each script component shows, from its last status report.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub scripts: IndexMap<String, ScriptView>,
}

/// One component in the status report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComponentReport {
    #[serde(flatten)]
    pub status: ComponentStatus,
    pub group: String,
    pub kind: ComponentKind,
    pub desired: Desired,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe: Option<ProbeResult>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub links: IndexMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logs: Option<RunBlock>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// A script component's rows, facts and alerts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<ScriptView>,
}

/// A script component's confirm for one verb, and whether the plan
/// preview it would show is still being worked out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptConfirm {
    pub text: String,
    pub pending: bool,
}

/// The outside world a runner talks to: swap every one for a fake in tests.
#[derive(Clone, Copy)]
pub struct Tools<'a> {
    pub cli: &'a dyn EngineCli,
    pub git: &'a dyn GitFacts,
    pub commands: &'a dyn CommandLookup,
    pub gateways: &'a dyn GatewayResolver,
}

pub struct StackRunner<'a> {
    pub stack_name: &'a str,
    pub stack: &'a StackDef,
    pub project_dir: &'a Path,
    pub cli: &'a dyn EngineCli,
    pub git: &'a dyn GitFacts,
    pub commands: &'a dyn CommandLookup,
    pub gateways: &'a dyn GatewayResolver,
    pub labels: LabelMap,
    pub now_utc: String,
    /// The bound item's worktree per clone name: a component whose repo is
    /// one of these builds and runs there instead of in the shared clone.
    pub worktrees: IndexMap<String, PathBuf>,
    /// When set, building or running from a shared clone (no worktree for
    /// it) is refused unless the clone is allowed. `None`: no guard.
    pub shared_guard: Option<SharedCheckoutGuard>,
    /// Status reports and plan previews of script components.
    pub scripts: &'a dyn ScriptOutputs,
    /// The bound item's key, for scripts (else the intent record's).
    pub item: Option<String>,
    /// The bound item's branch per clone name, for scripts.
    pub branches: IndexMap<String, String>,
}

/// The shared-checkout guard: which shared clones a stack may build from
/// directly, and the file that says so (named in every refusal). Without
/// it, a stack quietly builds whatever branch a shared clone happens to be
/// on.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SharedCheckoutGuard {
    pub allowed: BTreeSet<String>,
    pub file: PathBuf,
}

impl<'a> StackRunner<'a> {
    pub fn new(
        stack_name: &'a str,
        stack: &'a StackDef,
        project_dir: &'a Path,
        tools: Tools<'a>,
        now_utc: &str,
    ) -> Self {
        let labels = stack
            .provenance
            .as_ref()
            .map(|p| LabelMap::with_overrides(&p.labels))
            .unwrap_or_default();
        Self {
            stack_name,
            stack,
            project_dir,
            cli: tools.cli,
            git: tools.git,
            commands: tools.commands,
            gateways: tools.gateways,
            labels,
            now_utc: now_utc.to_string(),
            worktrees: IndexMap::new(),
            shared_guard: None,
            scripts: &NoScriptOutputs,
            item: None,
            branches: IndexMap::new(),
        }
    }

    /// The same runner reading script components' status and plans from
    /// `scripts`.
    pub fn with_scripts(mut self, scripts: &'a dyn ScriptOutputs) -> Self {
        self.scripts = scripts;
        self
    }

    /// The same runner telling scripts which item is bound and its branch
    /// per clone (the worktrees come from [`StackRunner::with_binding`]).
    pub fn with_item(mut self, item: Option<String>, branches: IndexMap<String, String>) -> Self {
        self.item = item;
        self.branches = branches;
        self
    }

    /// What a script component's scripts see of the bound item.
    pub fn script_binding(&self, inst: &ResolvedInstance) -> ScriptBinding {
        ScriptBinding {
            item: self.item.clone().or_else(|| inst.item.clone()),
            branches: self.branches.clone(),
            worktrees: self.worktrees.clone(),
            row: None,
            rows: Vec::new(),
        }
    }

    /// `verb` of the script component `component`, resolved for `inst`.
    pub fn script_run(
        &self,
        inst: &ResolvedInstance,
        cache: &ProvenanceCache,
        component: &str,
        verb: ScriptVerb,
        extra: &[String],
    ) -> Result<ScriptRun, ScriptError> {
        self.script_run_on(inst, cache, component, verb, None, &[], extra)
    }

    /// [`StackRunner::script_run`] for a verb run on one reported row
    /// (`{row}` = `row`; `None` = the whole component) or, for `up`, on the
    /// picked rows (`{rows}`; empty = everything).
    #[allow(clippy::too_many_arguments)]
    pub fn script_run_on(
        &self,
        inst: &ResolvedInstance,
        cache: &ProvenanceCache,
        component: &str,
        verb: ScriptVerb,
        row: Option<&str>,
        rows: &[String],
        extra: &[String],
    ) -> Result<ScriptRun, ScriptError> {
        let mut binding = self.script_binding(inst);
        binding.row = row.map(str::to_string);
        binding.rows = rows.to_vec();
        script::prepare(
            inst,
            self.stack,
            self.project_dir,
            component,
            verb,
            &binding,
            &self.known_images(cache),
            extra,
        )
    }

    /// What the Environment shows for a script component right now.
    pub fn script_view(
        &self,
        inst: &ResolvedInstance,
        cache: &ProvenanceCache,
        component: &str,
        runs: &ScriptRuns,
    ) -> ScriptView {
        let c = &inst.components[component];
        let has_status = self
            .stack
            .components
            .get(component)
            .is_some_and(|d| script::has(d, ScriptVerb::Status));
        let mut slow = None;
        let report = if has_status {
            match self.script_run(inst, cache, component, ScriptVerb::Status, &[]) {
                Ok(run) => {
                    // Only past the stack's own threshold: a run a little
                    // over one refresh turn is not worth a warning.
                    let after = self.stack.status_slow_after();
                    slow = self.scripts.slow(&run).filter(|took| *took >= after);
                    self.scripts.status(&run)
                }
                Err(e) => Some(ScriptReport::Failed {
                    reason: e.to_string(),
                }),
            }
        } else {
            None
        };
        let mut view = script::view_with_rows(
            component,
            c.kind,
            c.desired,
            &c.rows,
            has_status,
            report.as_ref(),
            runs.components.get(component),
        );
        if let Some(took) = slow {
            let secs = took.as_secs();
            view.slow_secs = Some(secs);
            view.alerts.push(ReportedAlert {
                tone: AlertTone::Warn,
                text: format!(
                    "{component}: status is slow; a script of this stack has been running for {secs} s, so the last known state is shown"
                ),
            });
        }
        view
    }

    /// The confirm of running `verb` of a script component: the plan
    /// script's preview when it has one, else the resolved argv.
    pub fn script_confirm(
        &self,
        inst: &ResolvedInstance,
        obs: &Observation,
        component: &str,
        verb: ScriptVerb,
    ) -> String {
        self.script_confirm_of(inst, obs, component, verb).text
    }

    /// [`StackRunner::script_confirm`], and whether the plan script's
    /// preview it would show has not arrived yet (the action is then
    /// offered disabled until it has).
    pub fn script_confirm_of(
        &self,
        inst: &ResolvedInstance,
        obs: &Observation,
        component: &str,
        verb: ScriptVerb,
    ) -> ScriptConfirm {
        let def = self.stack.components.get(component);
        let label = def
            .and_then(|d| d.label.clone())
            .unwrap_or_else(|| component.to_string());
        let run = self.script_run(inst, &obs.cache, component, verb, &[]);
        let with_plan = def.is_some_and(|d| script::has(d, ScriptVerb::Plan));
        let preview = match (&run, with_plan) {
            (Ok(_), true) => match self.script_run(
                inst,
                &obs.cache,
                component,
                ScriptVerb::Plan,
                &[verb.as_str().to_string()],
            ) {
                Ok(p) => self.scripts.plan(&p),
                Err(e) => Some(PlanPreview::Failed {
                    reason: e.to_string(),
                }),
            },
            _ => None,
        };
        ScriptConfirm {
            pending: with_plan && run.is_ok() && preview.is_none(),
            text: script::confirm(&label, verb, run.as_ref(), with_plan, preview.as_ref()),
        }
    }

    /// The confirm of running `verb` of a script component for the picked
    /// rows (`{rows}`): the resolved argv, like a per-row verb's.
    pub fn script_rows_confirm(
        &self,
        inst: &ResolvedInstance,
        obs: &Observation,
        component: &str,
        verb: ScriptVerb,
        rows: &[String],
    ) -> String {
        let label = self
            .stack
            .components
            .get(component)
            .and_then(|d| d.label.clone())
            .unwrap_or_else(|| component.to_string());
        let run = self.script_run_on(inst, &obs.cache, component, verb, None, rows, &[]);
        script::confirm(
            &format!("{label} ({})", rows.join(", ")),
            verb,
            run.as_ref(),
            false,
            None,
        )
    }

    /// The confirm of running `verb` of a script component on one reported
    /// row: the resolved argv (a `plan` script is not asked per row, so
    /// this is never pending).
    pub fn script_row_confirm(
        &self,
        inst: &ResolvedInstance,
        obs: &Observation,
        component: &str,
        verb: ScriptVerb,
        row: &str,
    ) -> String {
        let label = self
            .stack
            .components
            .get(component)
            .and_then(|d| d.label.clone())
            .unwrap_or_else(|| component.to_string());
        let run = self.script_run_on(inst, &obs.cache, component, verb, Some(row), &[], &[]);
        script::confirm(
            &format!("{row} (reported by {label})"),
            verb,
            run.as_ref(),
            false,
            None,
        )
    }

    /// Refuse a plan whose scripts cannot be resolved now (a placeholder
    /// that needs a bound item, a PowerShell file named without its
    /// interpreter), before anything is stored or run.
    pub fn check_scripts(
        &self,
        inst: &ResolvedInstance,
        obs: &Observation,
        plan: &Plan,
    ) -> Result<(), RunnerError> {
        for step in &plan.steps {
            if let Step::Script {
                component,
                verb,
                row,
                rows,
            } = step
            {
                self.script_run_on(
                    inst,
                    &obs.cache,
                    component,
                    *verb,
                    row.as_deref(),
                    rows,
                    &[],
                )?;
            }
        }
        Ok(())
    }

    /// The same runner bound to an item's worktrees, with the guard.
    pub fn with_binding(
        mut self,
        worktrees: IndexMap<String, PathBuf>,
        guard: Option<SharedCheckoutGuard>,
    ) -> Self {
        self.worktrees = worktrees;
        self.shared_guard = guard;
        self
    }

    /// The stack's work folder, absolute.
    pub fn work_dir(&self) -> PathBuf {
        if self.stack.work_dir.is_absolute() {
            self.stack.work_dir.clone()
        } else {
            self.project_dir.join(&self.stack.work_dir)
        }
    }

    fn now_secs(&self) -> u64 {
        parse_utc(&self.now_utc).unwrap_or(0)
    }

    /// Resolve `slot` with its intent record (a fresh one when none exists)
    /// and its generated secrets (made on first use).
    pub fn instance(&self, slot: u32) -> Result<(ResolvedInstance, IntentRecord), RunnerError> {
        let id = instance_id(self.stack_name, slot);
        let work_dir = self.work_dir();
        let intent = IntentRecord::load(&work_dir, &id)?
            .unwrap_or_else(|| IntentRecord::new(self.stack_name, slot, &self.now_utc));
        let mut inst = crate::driver::resolve(self.stack_name, self.stack, slot, Some(&intent))?;
        inst.secrets = crate::instance_secrets::load_or_create(
            &work_dir,
            &id,
            &crate::instance_secrets::names(self.stack),
        )
        .map_err(|e| RunnerError::Io(format!("the generated secrets of {id}: {e}")))?;
        Ok((inst, intent))
    }

    pub fn projects(&self, inst: &ResolvedInstance) -> IndexMap<String, ComposeProject> {
        compose::projects(inst, self.stack, self.project_dir, &self.work_dir())
    }

    fn project_of<'p>(
        &self,
        projects: &'p IndexMap<String, ComposeProject>,
        inst: &ResolvedInstance,
        component: &str,
    ) -> Result<&'p ComposeProject, RunnerError> {
        let group = &inst.components[component].group;
        projects.get(group).ok_or_else(|| RunnerError::NoProject {
            component: component.to_string(),
        })
    }

    /// The repo of `component` in `mode`: the bound item's worktree of that
    /// clone when there is one, else the mode's `repo`, under the stack
    /// `root` (or the project) when relative.
    pub fn repo_path(&self, component: &str, mode: Mode) -> Result<PathBuf, RunnerError> {
        let m = self.mode_def(component, mode)?;
        let repo = m.repo.as_ref().ok_or_else(|| RunnerError::NoRepo {
            component: component.to_string(),
            mode,
        })?;
        if let Some(w) = crate::definition::repo_name(repo).and_then(|n| self.worktrees.get(&n)) {
            return Ok(w.clone());
        }
        Ok(self.stack.repo_dir(self.project_dir, repo))
    }

    /// Refuse building (`verb`) `component` in `mode` from a shared clone
    /// the guard does not allow. A worktree of the bound item, a mode with
    /// no repo, and a runner without a guard are always fine.
    pub fn check_shared(
        &self,
        component: &str,
        mode: Mode,
        verb: &'static str,
    ) -> Result<(), RunnerError> {
        let Some(guard) = &self.shared_guard else {
            return Ok(());
        };
        if !mode.uses_repo() {
            return Ok(());
        }
        let Some(clone) = self.stack.repo_of(component, mode) else {
            return Ok(());
        };
        if self.worktrees.contains_key(&clone) || guard.allowed.contains(&clone) {
            return Ok(());
        }
        Err(RunnerError::SharedCheckout {
            component: component.to_string(),
            clone,
            verb,
            file: guard.file.clone(),
        })
    }

    /// [`StackRunner::check_shared`] over every step of a plan that builds
    /// or runs from a repo, so a refusal comes before anything is stored.
    pub fn guard_plan(
        &self,
        inst: &ResolvedInstance,
        obs: &Observation,
        plan: &Plan,
    ) -> Result<(), RunnerError> {
        for step in &plan.steps {
            let component = step.component();
            let mode = plan
                .modes
                .get(component)
                .copied()
                .or_else(|| self.current_mode(inst, &obs.cache, component));
            match step {
                Step::Build { .. } => self.check_shared(component, Mode::Source, "build")?,
                Step::Checkout { .. } => {
                    self.check_shared(component, mode.unwrap_or(Mode::Source), "check out")?
                }
                Step::Start { .. } | Step::Recreate { .. }
                    if Self::runs_as_process(inst, component, mode) =>
                {
                    self.check_shared(component, mode.unwrap_or(Mode::Dev), "run")?
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn mode_def(&self, component: &str, mode: Mode) -> Result<&'a ModeDef, RunnerError> {
        self.stack
            .components
            .get(component)
            .and_then(|c| c.mode(mode))
            .ok_or_else(|| RunnerError::NoMode {
                component: component.to_string(),
                mode,
            })
    }

    /// The mode a component is in right now: what it was last acquired as,
    /// else what the intent says, else the definition's default.
    pub fn current_mode(
        &self,
        inst: &ResolvedInstance,
        cache: &ProvenanceCache,
        component: &str,
    ) -> Option<Mode> {
        cache
            .components
            .get(component)
            .map(|p| p.mode)
            .or(inst.components[component].mode)
    }

    /// The image reference each component's cached provenance stands for:
    /// a source build's content key as is; a prebuilt one as
    /// `<repo>@<digest>` (else `<repo>:<tag>` until a digest is known).
    /// The cache keeps only the digest of a prebuilt image, and an override
    /// or `{image:x}` needs the whole reference: a bare `sha256:…` is no
    /// image an engine can pull.
    pub fn known_images(&self, cache: &ProvenanceCache) -> IndexMap<String, String> {
        let mut out = IndexMap::new();
        for (component, p) in &cache.components {
            let reference = match p.mode {
                Mode::Source => p.image.clone(),
                Mode::Prebuilt => self
                    .stack
                    .components
                    .get(component)
                    .and_then(|c| c.mode(Mode::Prebuilt))
                    .and_then(|m| m.image.as_deref())
                    .and_then(|repo| match (&p.image, &p.build_id) {
                        (Some(d), _) => Some(pinned_reference(repo, d)),
                        (None, Some(tag)) => Some(tagged_reference(repo, tag)),
                        (None, None) => None,
                    }),
                Mode::Dev => None,
            };
            if let Some(r) = reference {
                out.insert(component.clone(), r);
            }
        }
        out
    }

    /// Whether a component runs as a host process (the `process` driver, or
    /// any component in `dev` mode).
    pub fn runs_as_process(inst: &ResolvedInstance, component: &str, mode: Option<Mode>) -> bool {
        inst.components[component].driver == DriverKind::Process || mode == Some(Mode::Dev)
    }

    // ------------------------------------------------------------ observe

    /// Look at the instance: engines for compose components, the caller's
    /// process table for the rest.
    pub fn observe(
        &self,
        inst: &ResolvedInstance,
        processes: &IndexMap<String, ProcessStatus>,
    ) -> Result<Observation, RunnerError> {
        let work_dir = self.work_dir();
        let intent = IntentRecord::load(&work_dir, &inst.id)?
            .unwrap_or_else(|| IntentRecord::new(self.stack_name, inst.slot, &self.now_utc));
        let cache = ProvenanceCache::load(&work_dir, &inst.id);
        let projects = self.projects(inst);
        let mut notes = Vec::new();
        let mut ps: IndexMap<String, PsEntry> = IndexMap::new();
        let mut containers: IndexMap<String, InspectEntry> = IndexMap::new();
        for project in projects.values() {
            let out = match self.cli.run(&project.ps()) {
                Ok(out) if out.ok() => out,
                Ok(out) => {
                    notes.push(format!(
                        "{}: `compose ps` failed ({})",
                        project.engine.name,
                        out.summary()
                    ));
                    continue;
                }
                Err(e) => {
                    notes.push(format!("{}: {e}", project.engine.name));
                    continue;
                }
            };
            let rows = parse_ps(&out.stdout)?;
            let names: Vec<String> = rows.iter().map(|r| r.name.clone()).collect();
            let mut inspected: IndexMap<String, InspectEntry> = IndexMap::new();
            if !names.is_empty() {
                match self.cli.run(&project.inspect(&names)) {
                    Ok(out) if out.ok() => {
                        for e in parse_inspect(&out.stdout)? {
                            inspected.insert(e.name.clone(), e);
                        }
                    }
                    Ok(out) => notes.push(format!(
                        "{}: inspect failed ({})",
                        project.engine.name,
                        out.summary()
                    )),
                    Err(e) => notes.push(format!("{}: {e}", project.engine.name)),
                }
            }
            for row in rows {
                if let Some(component) = project.component_of(&row.service) {
                    if let Some(i) = inspected.shift_remove(&row.name) {
                        containers.insert(component.to_string(), i);
                    }
                    ps.insert(component.to_string(), row);
                }
            }
        }
        let mut gateways = IndexMap::new();
        for engine in inst.engines() {
            if !engine.kind.is_container_engine() {
                continue;
            }
            let os = self.stack.os_for_engine(&engine.name);
            match self.gateways.gateway(&engine, os) {
                Ok(g) => {
                    gateways.insert(engine.name.clone(), g);
                }
                Err(reason) => notes.push(format!("{}: gateway unknown ({reason})", engine.name)),
            }
        }
        let mut state = InstanceState::new(self.stack_name, inst.slot);
        let mut actual = IndexMap::new();
        let runs = ScriptRuns::load(&work_dir, &inst.id);
        let mut scripts = IndexMap::new();
        for (name, c) in &inst.components {
            if c.driver == DriverKind::Script {
                // The planner sees the intent only: a script's own report
                // must not move a confirmed plan's fingerprint.
                let view = self.script_view(inst, &cache, name, &runs);
                actual.insert(
                    name.clone(),
                    Actual {
                        exists: view.exists(),
                        running: view.running(),
                        exit_code: view.rows.first().and_then(|r| r.exit_code),
                        ..Actual::default()
                    },
                );
                state.components.insert(
                    name.clone(),
                    ComponentCurrent {
                        desired: c.desired,
                        exists: c.desired != Desired::Absent,
                        running: c.desired == Desired::Running,
                        ..ComponentCurrent::default()
                    },
                );
                scripts.insert(name.clone(), view);
                continue;
            }
            let mode = self.current_mode(inst, &cache, name);
            let a = if Self::runs_as_process(inst, name, mode) {
                actual_from_process(processes.get(name))
            } else {
                actual_from(ps.get(name), containers.get(name))
            };
            let prov = self.provenance_of(inst, &cache, &containers, name);
            let ci = intent.components.get(name);
            state.components.insert(
                name.clone(),
                ComponentCurrent {
                    mode: prov.as_ref().map(|p| p.mode).or(mode),
                    git_ref: ci
                        .and_then(|c| c.git_ref.clone())
                        .or_else(|| prov.as_ref().and_then(|p| p.git_ref.clone())),
                    commit: prov.as_ref().and_then(|p| p.commit.clone()),
                    dirty: prov.as_ref().and_then(|p| p.dirty.clone()),
                    image: prov.as_ref().and_then(|p| p.image.clone()),
                    desired: c.desired,
                    exists: a.exists,
                    running: a.running,
                    exit_code: a.exit_code,
                    gateway: containers
                        .get(name)
                        .and_then(|i| i.baked_gateway(inst.hostname.as_deref())),
                },
            );
            actual.insert(name.clone(), a);
        }
        Ok(Observation {
            intent,
            cache,
            state,
            actual,
            ps,
            containers,
            gateways,
            notes,
            scripts,
        })
    }

    /// The cache's record of a component, else what its container's labels
    /// say, else nothing.
    fn provenance_of(
        &self,
        inst: &ResolvedInstance,
        cache: &ProvenanceCache,
        containers: &IndexMap<String, InspectEntry>,
        component: &str,
    ) -> Option<Provenance> {
        if let Some(p) = cache.components.get(component) {
            return Some(p.clone());
        }
        let i = containers.get(component)?;
        if i.labels.is_empty() {
            return None;
        }
        let (tag, digest) = split_reference(&i.image_ref);
        let mode_hint = inst.components[component].mode;
        let mut p = images::provenance_from_container_labels(&self.labels, &i.labels, tag, digest);
        if !i.labels.contains_key(LABEL_CONTENT_KEY) {
            if let Some(m) = mode_hint {
                if m != Mode::Source {
                    p.mode = m;
                }
            }
        }
        Some(p)
    }

    // --------------------------------------------------------------- plan

    pub fn plan(&self, obs: &Observation, change: &Change) -> Result<Plan, RunnerError> {
        Ok(plan_change(self.stack, &obs.state, change)?)
    }

    /// The `start` change with the gateways the observation found, so the
    /// planner can recreate what baked in a stale one.
    pub fn start_change(obs: &Observation) -> Change {
        Change::Start {
            gateways: obs
                .gateways
                .iter()
                .map(|(e, g)| (e.clone(), g.address.clone()))
                .collect(),
        }
    }

    // -------------------------------------------------------------- apply

    /// Turn a plan into steps. Refuses when the observation no longer
    /// matches the plan's fingerprint.
    pub fn apply(
        &self,
        inst: &ResolvedInstance,
        obs: &Observation,
        plan: &Plan,
        opts: &ApplyOptions,
    ) -> Result<Applied, RunnerError> {
        plan.check_against(&obs.state)?;
        self.guard_plan(inst, obs, plan)?;
        let work_dir = self.work_dir();
        let projects = self.projects(inst);
        let mut cache = obs.cache.clone();
        let mut images = self.known_images(&cache);
        let mut builds: IndexMap<String, SourceBuild> = IndexMap::new();
        let mut steps: Vec<JobStep> = Vec::new();
        let mut notes: Vec<String> = Vec::new();
        let mut touched: IndexSet<String> = IndexSet::new();
        let mut removes: IndexMap<String, Vec<String>> = IndexMap::new();
        // Two components on one image (a database and its migration) need
        // one digest lookup and one pull, not two.
        let mut digests: IndexMap<(String, String), Option<String>> = IndexMap::new();
        let mut pulled: IndexSet<(String, String)> = IndexSet::new();
        let watch = plan.verb == Verb::Watch;

        for step in &plan.steps {
            let component = step.component().to_string();
            let c = &inst.components[&component];
            let before = self.current_mode(inst, &obs.cache, &component);
            let after = plan.modes.get(&component).copied().or(before);
            match step {
                Step::Checkout { git_ref, .. } => {
                    let mode = after.unwrap_or(Mode::Source);
                    let repo = self.repo_path(&component, mode)?;
                    let now = self
                        .git
                        .current_branch(&repo)?
                        .unwrap_or_else(|| "a detached HEAD".to_string());
                    if now == *git_ref {
                        steps.push(JobStep::note(format!("{component}: already on {git_ref}")));
                    } else {
                        return Err(RunnerError::CheckoutUnsupported {
                            component,
                            wanted: git_ref.clone(),
                            now,
                        });
                    }
                }
                Step::Build { key, .. } => {
                    // A pinned content key (a resumed state) that the engine
                    // still has is used as is, whatever the repo holds now.
                    if let Some(p) = plan.pins.get(&component) {
                        if let Some(pinned) = &p.image {
                            if image_exists(self.cli, &c.engine, pinned)? {
                                steps.push(JobStep::note(format!(
                                    "{component}: {pinned} exists, reused (saved state)"
                                )));
                                images.insert(component.clone(), pinned.clone());
                                let mut prov = Provenance::new(Mode::Source);
                                prov.git_ref = p.git_ref.clone();
                                prov.commit = p.commit.clone();
                                prov.dirty = p.dirty.clone();
                                prov.image = Some(pinned.clone());
                                cache.remember(&component, prov, &self.now_utc);
                                touched.insert(c.group.clone());
                                continue;
                            }
                        }
                    }
                    let mode = self.mode_def(&component, Mode::Source)?;
                    let repo = self.repo_path(&component, Mode::Source)?;
                    let build = source_build(&component, mode, &repo, self.git, None)?;
                    if let Some(k) = key {
                        if *k != build.key {
                            notes.push(format!(
                                "{component}: content key is {} now (planned {k})",
                                build.key
                            ));
                        }
                    }
                    if image_exists(self.cli, &c.engine, &build.key)? {
                        steps.push(JobStep::note(format!(
                            "{component}: {} exists, reused",
                            build.key
                        )));
                    } else {
                        let argv = images::build_argv(&c.engine, &build);
                        steps.push(JobStep::run(
                            &component,
                            RunBlock::new(argv, &repo)
                                .titled(format!("build {component} ({})", build.key)),
                        ));
                    }
                    images.insert(component.clone(), build.key.clone());
                    cache.remember(
                        &component,
                        build.provenance(Some(&self.now_utc)),
                        &self.now_utc,
                    );
                    builds.insert(component.clone(), build);
                    touched.insert(c.group.clone());
                }
                Step::Pull { image, .. } => {
                    let mode = self.mode_def(&component, Mode::Prebuilt)?;
                    // A pinned digest (a resumed state) is pulled as is: no
                    // tag is resolved, so a channel that moved on since does
                    // not change what comes back.
                    let pin = plan.pins.get(&component).filter(|p| p.image.is_some());
                    let (tag, reference, digest) = if let Some(p) = pin {
                        let tag = p.build_id.clone().unwrap_or_else(|| "pinned".to_string());
                        let d = p.image.clone();
                        (tag, pinned_reference(image, d.as_deref().unwrap_or("")), d)
                    } else {
                        let channel = plan.channel.clone().or_else(|| {
                            obs.intent
                                .components
                                .get(&component)
                                .and_then(|i| i.channel.clone())
                        });
                        let tag = resolve_tag(mode, channel.as_deref()).map_err(|source| {
                            RunnerError::Placeholder {
                                component: component.clone(),
                                source,
                            }
                        })?;
                        let reference = tagged_reference(image, &tag);
                        let os = self.stack.os_for_engine(&c.engine.name);
                        let lookup = (c.engine.name.clone(), reference.clone());
                        let digest = match digests.get(&lookup) {
                            Some(d) => d.clone(),
                            None => {
                                let d = resolve_digest(self.cli, &c.engine, &reference, os)?;
                                digests.insert(lookup, d.clone());
                                d
                            }
                        };
                        (tag, reference, digest)
                    };
                    let pinned = match &digest {
                        Some(d) => pinned_reference(image, d),
                        None => {
                            notes.push(format!(
                                "{component}: pinned by tag until the first pull resolves a digest"
                            ));
                            reference.clone()
                        }
                    };
                    let mut prov = Provenance::new(Mode::Prebuilt);
                    prov.build_id = Some(tag.clone());
                    prov.image = digest.clone();
                    prov.built_utc = None;
                    if let Some(d) = &digest {
                        let entry = match cache.resolved.get(d) {
                            Some(e) => Some(e.clone()),
                            None => self.run_resolve_hook(inst, mode, &component, &tag)?,
                        };
                        if let Some(e) = entry {
                            e.fill(&mut prov);
                            cache.resolved.insert(d.clone(), e);
                        }
                    }
                    if pulled.insert((c.engine.name.clone(), pinned.clone())) {
                        let mut argv = engine_argv(&c.engine);
                        argv.push("pull".to_string());
                        argv.push(pinned.clone());
                        steps.push(JobStep::run(
                            &component,
                            RunBlock::new(argv, self.project_dir)
                                .titled(format!("pull {component} ({tag})")),
                        ));
                    } else {
                        steps.push(JobStep::note(format!(
                            "{component}: {pinned} is pulled by an earlier step"
                        )));
                    }
                    images.insert(component.clone(), pinned);
                    cache.remember(&component, prov, &self.now_utc);
                    touched.insert(c.group.clone());
                }
                Step::Start { .. } | Step::Recreate { .. } => {
                    let was_process = Self::runs_as_process(inst, &component, before);
                    let is_process = Self::runs_as_process(inst, &component, after);
                    let cur = obs.state.component(&component);
                    if is_process {
                        // A process is not replaced by starting it again: the
                        // old one (another worktree, another ref) stops first.
                        if was_process && cur.running && matches!(step, Step::Recreate { .. }) {
                            steps.push(JobStep::StopProcess {
                                component: component.clone(),
                                grace_seconds: self.stack.grace_seconds(&component),
                            });
                        }
                        if c.driver == DriverKind::Compose && cur.exists && !was_process {
                            let project = self.project_of(&projects, inst, &component)?;
                            steps.push(JobStep::run(
                                &component,
                                RunBlock::new(project.remove(&[&component]), self.project_dir)
                                    .titled(format!("remove {component} container (dev mode)")),
                            ));
                        }
                        let mode = after.unwrap_or(Mode::Dev);
                        let repo = self.repo_path(&component, mode).ok();
                        let run = process::run_block(
                            inst,
                            self.stack,
                            process::RunRequest {
                                component: &component,
                                mode,
                                repo: repo.as_deref(),
                                watch,
                            },
                            self.commands,
                            self.project_dir,
                            &images,
                        )?;
                        if mode == Mode::Dev {
                            let mut prov = Provenance::new(Mode::Dev);
                            if let Some(r) = &repo {
                                if let Ok(commit) = self.git.head_commit(r) {
                                    prov.commit = Some(commit);
                                    prov.dirty = self.git.dirty_hash(r).ok().flatten();
                                    prov.git_ref = self.git.current_branch(r).ok().flatten();
                                }
                            }
                            cache.remember(&component, prov, &self.now_utc);
                        }
                        steps.push(JobStep::run(&component, run));
                    } else {
                        if was_process && cur.exists {
                            steps.push(JobStep::StopProcess {
                                component: component.clone(),
                                grace_seconds: self.stack.grace_seconds(&component),
                            });
                        }
                        let project = self.project_of(&projects, inst, &component)?;
                        touched.insert(c.group.clone());
                        let container_exists = obs.ps.contains_key(&component);
                        let argv = match step {
                            Step::Recreate { .. } => project.up(&[&component], true),
                            _ if container_exists => project.start(&[&component]),
                            _ => project.up(&[&component], false),
                        };
                        let title = match step {
                            Step::Recreate { reason, .. } => {
                                format!("recreate {component} ({reason})")
                            }
                            _ if container_exists => format!("start {component}"),
                            _ => format!("up {component}"),
                        };
                        steps.push(JobStep::run(
                            &component,
                            RunBlock::new(argv, self.project_dir).titled(title),
                        ));
                    }
                }
                Step::Stop { grace_seconds, .. } => {
                    if Self::runs_as_process(inst, &component, before) {
                        steps.push(JobStep::StopProcess {
                            component: component.clone(),
                            grace_seconds: *grace_seconds,
                        });
                    } else {
                        let project = self.project_of(&projects, inst, &component)?;
                        steps.push(JobStep::run(
                            &component,
                            RunBlock::new(
                                project.stop(&component, *grace_seconds),
                                self.project_dir,
                            )
                            .titled(format!("stop {component} ({grace_seconds}s)")),
                        ));
                    }
                }
                Step::Script {
                    verb, row, rows, ..
                } => {
                    let run = self.script_run_on(
                        inst,
                        &obs.cache,
                        &component,
                        *verb,
                        row.as_deref(),
                        rows,
                        &[],
                    )?;
                    steps.push(JobStep::Script {
                        run: run.run_block(),
                        component: component.clone(),
                        verb: *verb,
                        timeout_seconds: run.timeout_secs,
                    });
                }
                Step::Remove { .. } => {
                    if Self::runs_as_process(inst, &component, before) {
                        if obs.state.component(&component).running {
                            steps.push(JobStep::StopProcess {
                                component: component.clone(),
                                grace_seconds: self.stack.grace_seconds(&component),
                            });
                        }
                    } else {
                        removes
                            .entry(c.group.clone())
                            .or_default()
                            .push(component.clone());
                    }
                    cache.forget(&component, &self.now_utc);
                }
            }
        }

        for (group, comps) in &removes {
            let Some(project) = projects.get(group) else {
                continue;
            };
            let whole = project.services.keys().all(|c| comps.contains(c));
            let names: Vec<&str> = comps.iter().map(String::as_str).collect();
            let (argv, title) = if whole && plan.verb == Verb::Down {
                let title = if opts.remove_volumes {
                    format!("down {} (volumes removed)", project.project)
                } else {
                    format!("down {}", project.project)
                };
                (project.down(opts.remove_volumes), title)
            } else {
                (
                    project.remove(&names),
                    format!("remove {}", comps.join(", ")),
                )
            };
            steps.push(JobStep::run(
                &comps[0],
                RunBlock::new(argv, self.project_dir).titled(title),
            ));
        }

        // Override files first: every run below reads them.
        let mut head: Vec<JobStep> = Vec::new();
        for group in &touched {
            let Some(project) = projects.get(group) else {
                continue;
            };
            let gateway = obs
                .gateways
                .get(&project.engine.name)
                .map(|g| g.address.clone());
            if gateway.is_none() && inst.hostname.is_some() {
                notes.push(format!(
                    "{}: no gateway address known; {} will not resolve inside its containers",
                    project.engine.name,
                    inst.hostname.clone().unwrap_or_default()
                ));
            }
            let base_texts = self.base_texts(project);
            let inputs = OverrideInputs {
                images: &images,
                builds: &builds,
                gateway: gateway.as_deref(),
                base_texts: &base_texts,
            };
            notes.extend(compose::base_file_findings(project, &base_texts));
            let services = compose::service_overrides(project, inst, self.stack, &inputs)?;
            head.push(JobStep::write(
                &format!("{} override", project.project),
                project.override_path.clone(),
                compose::render_override(&services),
            ));
        }
        head.extend(steps);
        let mut steps = head;

        let mut intent = obs.intent.clone();
        intent.record_plan(plan, &self.now_utc);
        steps.push(JobStep::write(
            "provenance cache",
            ProvenanceCache::path(&work_dir, &inst.id),
            cache.to_json(),
        ));
        steps.push(JobStep::write(
            "intent record",
            IntentRecord::path(&work_dir, &inst.id),
            serde_json::to_string_pretty(&intent).expect("intent serializes"),
        ));
        notes.extend(plan.notes.iter().cloned());
        Ok(Applied {
            steps,
            notes,
            intent,
        })
    }

    fn base_texts(&self, project: &ComposeProject) -> IndexMap<PathBuf, String> {
        project
            .files
            .iter()
            .filter_map(|f| std::fs::read_to_string(f).ok().map(|t| (f.clone(), t)))
            .collect()
    }

    fn run_resolve_hook(
        &self,
        inst: &ResolvedInstance,
        mode: &ModeDef,
        component: &str,
        tag: &str,
    ) -> Result<Option<CatalogEntry>, RunnerError> {
        let ctx = inst.context_for(component);
        let Some(argv) =
            images::resolve_argv(mode, &ctx, tag).map_err(|source| RunnerError::Placeholder {
                component: component.to_string(),
                source,
            })?
        else {
            return Ok(None);
        };
        let out = self
            .cli
            .run_ok(&Invocation::from_argv(&argv).cwd(self.project_dir))?;
        Ok(Some(images::parse_resolve(&out.stdout, tag)?))
    }

    /// Run the `catalog` hook (or the default manifest inspect) for a
    /// prebuilt component and parse what it lists.
    pub fn catalog(
        &self,
        inst: &ResolvedInstance,
        component: &str,
        tag: &str,
    ) -> Result<Vec<CatalogEntry>, RunnerError> {
        let mode = self.mode_def(component, Mode::Prebuilt)?;
        let c = &inst.components[component];
        let ctx = inst.context_for(component);
        let argv = images::catalog_argv(mode, &c.engine, &ctx, tag).map_err(|source| {
            RunnerError::Placeholder {
                component: component.to_string(),
                source,
            }
        })?;
        let out = self
            .cli
            .run_ok(&Invocation::from_argv(&argv).cwd(self.project_dir))?;
        Ok(images::parse_catalog(
            &out.stdout,
            tag,
            self.stack.os_for_engine(&c.engine.name),
        ))
    }

    // ------------------------------------------------------------- status

    /// The report per component, probes included. The `newer prebuilt`
    /// check asks the registry in place; see [`StackRunner::status_with`]
    /// for a report that never waits on one.
    pub fn status(
        &self,
        inst: &ResolvedInstance,
        obs: &Observation,
        http: &dyn ProbeHttp,
    ) -> Vec<ComponentReport> {
        self.status_with(inst, obs, http, &LiveDigests(self.cli))
    }

    /// [`StackRunner::status`] with the channel digests coming from
    /// `digests`. A registry round trip can take many seconds per image, so
    /// a host that answers status often hands in a cache it refreshes off
    /// the hot path; a digest it does not know yet means no staleness badge
    /// yet, never a wait.
    pub fn status_with(
        &self,
        inst: &ResolvedInstance,
        obs: &Observation,
        http: &dyn ProbeHttp,
        digests: &dyn PrebuiltDigests,
    ) -> Vec<ComponentReport> {
        let projects = self.projects(inst);
        let now = self.now_secs();
        let mut host_ctx = inst.host_context();
        host_ctx.images = self.known_images(&obs.cache);
        let mut out = Vec::new();
        for (name, c) in &inst.components {
            let links: IndexMap<String, String> = c
                .outputs
                .iter()
                .filter_map(|(k, t)| {
                    placeholders::resolve(t, &host_ctx)
                        .ok()
                        .map(|v| (k.clone(), v))
                })
                .collect();
            if let Some(view) = obs.scripts.get(name) {
                let first = view.rows.first().filter(|_| !view.multi);
                out.push(ComponentReport {
                    status: ComponentStatus {
                        id: name.clone(),
                        state: view.state(),
                        uptime: first
                            .filter(|r| r.state != crate::driver::ComponentState::NotDeployed)
                            .filter(|_| view.running())
                            .and_then(|r| r.since.as_deref())
                            .and_then(|s| uptime_text(s, now)),
                        restarts: None,
                        exit_code: first.and_then(|r| r.exit_code),
                        provenance: None,
                        stale: Vec::new(),
                    },
                    group: c.group.clone(),
                    kind: c.kind,
                    desired: c.desired,
                    probe: None,
                    links,
                    logs: None,
                    notes: Vec::new(),
                    script: Some(view.clone()),
                });
                continue;
            }
            let mut actual = obs.actual.get(name).cloned().unwrap_or_default();
            let mut notes = Vec::new();
            let mut probe_result = None;
            if let (Some(p), true) = (&c.probe, actual.running) {
                match placeholders::resolve(&p.url, &host_ctx) {
                    Ok(url) => {
                        let r = probe::probe(http, &url, p.rule, DEFAULT_PROBE_TIMEOUT);
                        actual.probe_up = r.up();
                        probe_result = Some(r);
                    }
                    Err(e) => notes.push(format!("probe url: {e}")),
                }
            }
            let state = component_state(c.kind, c.desired, &actual);
            let provenance = self.provenance_of(inst, &obs.cache, &obs.containers, name);
            let mut stale = Vec::new();
            if let Some(p) = &provenance {
                match p.mode {
                    Mode::Source | Mode::Dev => {
                        if let Ok(repo) = self.repo_path(name, p.mode) {
                            match worktree_staleness(p, &repo, self.git) {
                                Ok(s) => stale = s,
                                Err(e) => notes.push(format!("staleness: {e}")),
                            }
                        }
                    }
                    Mode::Prebuilt => {
                        if let Some(s) = self.newer_prebuilt(inst, obs, name, p, digests) {
                            stale.push(s);
                        }
                    }
                }
            }
            let uptime = if actual.running {
                obs.containers
                    .get(name)
                    .and_then(|i| i.started_at.as_deref())
                    .and_then(|s| uptime_text(s, now))
            } else {
                None
            };
            let logs = if c.driver == DriverKind::Compose
                && !Self::runs_as_process(inst, name, provenance.as_ref().map(|p| p.mode))
            {
                projects
                    .get(&c.group)
                    .map(|p| p.logs(name, self.project_dir))
            } else {
                None
            };
            out.push(ComponentReport {
                status: ComponentStatus {
                    id: name.clone(),
                    state,
                    uptime,
                    restarts: actual.exists.then_some(actual.restarts),
                    exit_code: actual.exit_code,
                    provenance,
                    stale,
                },
                group: c.group.clone(),
                kind: c.kind,
                desired: c.desired,
                probe: probe_result,
                links,
                logs,
                notes,
                script: None,
            });
        }
        out
    }

    /// `newer prebuilt` when the component's channel resolves to another
    /// digest than the one running. Needs a channel and a known digest.
    fn newer_prebuilt(
        &self,
        inst: &ResolvedInstance,
        obs: &Observation,
        component: &str,
        prov: &Provenance,
        digests: &dyn PrebuiltDigests,
    ) -> Option<Staleness> {
        let lookup = self.channel_lookup(inst, obs, component)?;
        let now = digests.current_digest(&lookup.engine, &lookup.reference, lookup.os)?;
        prebuilt_staleness(prov, &now)
    }

    /// The channel reference a prebuilt component tracks (`repo:<tag>` with
    /// the intent's channel), when it has one.
    fn channel_lookup(
        &self,
        inst: &ResolvedInstance,
        obs: &Observation,
        component: &str,
    ) -> Option<ChannelLookup> {
        let mode = self.mode_def(component, Mode::Prebuilt).ok()?;
        let channel = obs.intent.components.get(component)?.channel.as_deref();
        let tag = resolve_tag(mode, channel).ok()?;
        let reference = tagged_reference(mode.image.as_deref()?, &tag);
        let c = inst.components.get(component)?;
        Some(ChannelLookup {
            component: component.to_string(),
            engine: c.engine.clone(),
            reference,
            os: self.stack.os_for_engine(&c.engine.name),
        })
    }

    /// Every channel reference the `newer prebuilt` check would ask about
    /// for this observation: what a cache refreshes in the background.
    pub fn channel_lookups(
        &self,
        inst: &ResolvedInstance,
        obs: &Observation,
    ) -> Vec<ChannelLookup> {
        inst.components
            .keys()
            .filter(|name| {
                obs.cache
                    .components
                    .get(*name)
                    .is_some_and(|p| p.mode == Mode::Prebuilt)
            })
            .filter_map(|name| self.channel_lookup(inst, obs, name))
            .collect()
    }

    // ------------------------------------------------------------- checks

    /// The checks before `up`: for a plan, only what it brings up; without
    /// one, the whole instance.
    pub fn checks(
        &self,
        inst: &ResolvedInstance,
        obs: Option<&Observation>,
        plan: Option<&Plan>,
        checker: &dyn Checker,
    ) -> Vec<CheckResult> {
        let checks = match (plan, obs) {
            (Some(p), Some(o)) => checks_for_plan(inst, self.stack, p, &o.state),
            _ => checks_for(inst, self.stack),
        };
        run_checks(checker, &checks)
    }

    // --------------------------------------------------------------- misc

    /// A component's `logs -f` as a terminal action.
    pub fn logs(&self, inst: &ResolvedInstance, component: &str) -> Option<RunBlock> {
        let c = inst.components.get(component)?;
        if c.driver != DriverKind::Compose {
            return None;
        }
        self.projects(inst)
            .get(&c.group)
            .map(|p| p.logs(component, self.project_dir))
    }

    /// `compose restart -t <grace>` of one component, outside the plan model
    /// (it changes nothing the intent records).
    pub fn restart(&self, inst: &ResolvedInstance, component: &str) -> Option<RunBlock> {
        let c = inst.components.get(component)?;
        if c.driver != DriverKind::Compose {
            return None;
        }
        let grace = self.stack.grace_seconds(component);
        self.projects(inst).get(&c.group).map(|p| {
            RunBlock::new(p.restart(component, grace), self.project_dir)
                .titled(format!("restart {component}"))
        })
    }

    /// Bind `item` to `slot`: `active.json` and the instance's intent.
    pub fn activate(&self, slot: u32, item: &str) -> Result<Vec<JobStep>, RunnerError> {
        let work_dir = self.work_dir();
        let id = instance_id(self.stack_name, slot);
        let mut active = ActiveRecord::load(&work_dir)?;
        active.set(slot, item, &id, &self.now_utc);
        let mut intent = IntentRecord::load(&work_dir, &id)?
            .unwrap_or_else(|| IntentRecord::new(self.stack_name, slot, &self.now_utc));
        intent.item = Some(item.to_string());
        intent.updated_utc = self.now_utc.clone();
        Ok(vec![
            JobStep::write(
                "active item",
                ActiveRecord::path(&work_dir),
                serde_json::to_string_pretty(&active).expect("active serializes"),
            ),
            JobStep::write(
                "intent record",
                IntentRecord::path(&work_dir, &id),
                serde_json::to_string_pretty(&intent).expect("intent serializes"),
            ),
        ])
    }

    /// Engine facts for the summary line, through the checker's own cache
    /// when it is an [`crate::checks::EngineChecker`]; otherwise asked here.
    pub fn engine_facts(&self, inst: &ResolvedInstance) -> IndexMap<String, EngineFacts> {
        inst.engines()
            .iter()
            .filter(|e| e.kind.is_container_engine())
            .map(|e| (e.name.clone(), crate::checks::engine_facts(self.cli, e)))
            .collect()
    }

    // ----------------------------------------------------------------- gc

    /// What `gc` would remove per engine, and what it keeps and why.
    pub fn gc_plan(&self, keep: &[String]) -> Result<Vec<GcPlan>, RunnerError> {
        let work_dir = self.work_dir();
        let pinned = self.pinned_images(&work_dir);
        let mut engines: IndexMap<String, EngineRef> = IndexMap::new();
        for name in self.stack.components.keys() {
            if let Some(e) = self.stack.engine_for(name) {
                if e.kind.is_container_engine() {
                    engines.entry(e.name.clone()).or_insert(e);
                }
            }
        }
        let prebuilt_repos: BTreeSet<String> = self
            .stack
            .components
            .values()
            .filter_map(|c| c.mode(Mode::Prebuilt).and_then(|m| m.image.clone()))
            .collect();
        let mut plans = Vec::new();
        for engine in engines.values() {
            let referenced = self.referenced_images(engine)?;
            let mut rows: Vec<ImageRow> = Vec::new();
            let ours = self.cli.run_ok(&engine_invocation(
                engine,
                &[
                    "image",
                    "ls",
                    "--digests",
                    "--format",
                    "json",
                    "--filter",
                    &format!("label={LABEL_CONTENT_KEY}"),
                ],
            ))?;
            rows.extend(parse_image_list(&ours.stdout)?);
            for repo in &prebuilt_repos {
                let out = self.cli.run_ok(&engine_invocation(
                    engine,
                    &["image", "ls", "--digests", "--format", "json", repo],
                ))?;
                for row in parse_image_list(&out.stdout)? {
                    if !rows
                        .iter()
                        .any(|r| r.id == row.id && r.reference() == row.reference())
                    {
                        rows.push(row);
                    }
                }
            }
            let dangling = self.cli.run_ok(&engine_invocation(
                engine,
                &[
                    "image",
                    "ls",
                    "--format",
                    "json",
                    "--filter",
                    "dangling=true",
                ],
            ))?;
            let dangling_rows = parse_image_list(&dangling.stdout)?;
            let mut plan = GcPlan {
                engine: engine.name.clone(),
                remove: Vec::new(),
                kept: Vec::new(),
                prune_build_cache: true,
            };
            for row in rows {
                let reference = row.reference();
                let by_digest = row
                    .digest
                    .as_deref()
                    .map(|d| pinned_reference(&row.repository, d));
                let keep_reason = if referenced.contains(&row.id)
                    || referenced.contains(&reference)
                    || by_digest.as_ref().is_some_and(|p| referenced.contains(p))
                {
                    Some("in use by a running container")
                } else if pinned.contains(&reference)
                    || by_digest.as_ref().is_some_and(|p| pinned.contains(p))
                {
                    Some("pinned by an instance")
                } else if keep.contains(&reference)
                    || by_digest.as_ref().is_some_and(|p| keep.contains(p))
                {
                    Some("referenced by a kept state")
                } else {
                    None
                };
                match keep_reason {
                    Some(r) => plan.kept.push(GcKept {
                        reference,
                        reason: r.to_string(),
                    }),
                    None => plan.remove.push(GcImage {
                        reference,
                        id: row.id,
                        size: row.size,
                        reason:
                            "not referenced by a running instance, a kept state or a pinned channel"
                                .to_string(),
                    }),
                }
            }
            for row in dangling_rows {
                if !plan.remove.iter().any(|r| r.id == row.id) && !referenced.contains(&row.id) {
                    plan.remove.push(GcImage {
                        reference: row.reference(),
                        id: row.id,
                        size: row.size,
                        reason: "dangling".to_string(),
                    });
                }
            }
            plans.push(plan);
        }
        Ok(plans)
    }

    /// The steps that carry out a gc plan: one `image rm` per engine, then
    /// the engine's build-cache prune.
    pub fn gc_apply(&self, plans: &[GcPlan]) -> Vec<JobStep> {
        let mut steps = Vec::new();
        for plan in plans {
            let Some(engine) = self.stack.engine_ref(&plan.engine) else {
                continue;
            };
            if !plan.remove.is_empty() {
                let mut argv = engine_argv(&engine);
                argv.push("image".to_string());
                argv.push("rm".to_string());
                for img in &plan.remove {
                    argv.push(img.id.clone());
                }
                steps.push(JobStep::run(
                    "gc",
                    RunBlock::new(argv, self.project_dir).titled(format!(
                        "remove {} image(s) on {}",
                        plan.remove.len(),
                        plan.engine
                    )),
                ));
            }
            if plan.prune_build_cache {
                let mut argv = engine_argv(&engine);
                match engine.kind {
                    EngineKind::Docker => argv.extend(["builder", "prune", "-f"].map(String::from)),
                    _ => argv.extend(["image", "prune", "-f"].map(String::from)),
                }
                steps.push(JobStep::run(
                    "gc",
                    RunBlock::new(argv, self.project_dir)
                        .titled(format!("prune build cache on {}", plan.engine)),
                ));
            }
        }
        steps
    }

    /// Image references and ids of every container on the engine.
    fn referenced_images(&self, engine: &EngineRef) -> Result<BTreeSet<String>, RunnerError> {
        let out = self.cli.run_ok(&engine_invocation(
            engine,
            &["ps", "-a", "--format", "json", "--no-trunc"],
        ))?;
        Ok(parse_container_images(&out.stdout))
    }
}

/// Where the `newer prebuilt` check gets the digest a channel resolves to
/// right now.
pub trait PrebuiltDigests {
    /// `None` when unknown: the check is skipped, never failed.
    fn current_digest(
        &self,
        engine: &EngineRef,
        reference: &str,
        os: Option<ContainerOs>,
    ) -> Option<String>;
}

/// Ask the registry in place, through the engine CLI.
pub struct LiveDigests<'a>(pub &'a dyn EngineCli);

impl PrebuiltDigests for LiveDigests<'_> {
    fn current_digest(
        &self,
        engine: &EngineRef,
        reference: &str,
        os: Option<ContainerOs>,
    ) -> Option<String> {
        resolve_digest(self.0, engine, reference, os).ok()?
    }
}

/// One channel reference to resolve: which component, on which engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelLookup {
    pub component: String,
    pub engine: EngineRef,
    pub reference: String,
    pub os: Option<ContainerOs>,
}

/// `repo:tag` → (tag, None); `repo@sha256:…` → (None, digest).
fn split_reference(reference: &str) -> (Option<&str>, Option<&str>) {
    if let Some((_, digest)) = reference.split_once('@') {
        return (None, Some(digest));
    }
    match reference.rsplit_once(':') {
        Some((_, tag)) if !tag.contains('/') => (Some(tag), None),
        _ => (None, None),
    }
}

// --------------------------------------------------------------------- gc

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GcImage {
    pub reference: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GcKept {
    pub reference: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GcPlan {
    pub engine: String,
    pub remove: Vec<GcImage>,
    pub kept: Vec<GcKept>,
    pub prune_build_cache: bool,
}

impl GcPlan {
    pub fn summary(&self) -> String {
        format!(
            "{}: remove {} image(s), keep {}{}",
            self.engine,
            self.remove.len(),
            self.kept.len(),
            if self.prune_build_cache {
                ", prune build cache"
            } else {
                ""
            }
        )
    }
}

/// One image as `image ls --format json` lists it, docker or podman.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ImageRow {
    pub id: String,
    pub repository: String,
    pub tag: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
}

impl ImageRow {
    /// `repo:tag`, or `repo@digest` for an untagged image, or the id.
    pub fn reference(&self) -> String {
        if !self.tag.is_empty() && self.tag != "<none>" {
            format!("{}:{}", self.repository, self.tag)
        } else if let (false, Some(d)) = (
            self.repository.is_empty() || self.repository == "<none>",
            &self.digest,
        ) {
            format!("{}@{}", self.repository, d)
        } else {
            self.id.clone()
        }
    }
}

#[derive(Debug, Deserialize)]
struct DockerImageRow {
    #[serde(rename = "ID", default)]
    id: String,
    #[serde(rename = "Repository", default)]
    repository: String,
    #[serde(rename = "Tag", default)]
    tag: String,
    #[serde(rename = "Digest", default)]
    digest: String,
    #[serde(rename = "Size", default)]
    size: String,
}

#[derive(Debug, Deserialize)]
struct PodmanImageRow {
    #[serde(rename = "Id", default)]
    id: String,
    #[serde(rename = "Names", default)]
    names: Vec<String>,
    #[serde(rename = "Digest", default)]
    digest: String,
    #[serde(rename = "Size", default)]
    size: Option<u64>,
}

/// Parse `image ls --format json`: docker prints one object per line with
/// `Repository`/`Tag`; podman prints an array with `Names`.
pub fn parse_image_list(stdout: &str) -> Result<Vec<ImageRow>, CliError> {
    let text = stdout.trim();
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let bad = |e: serde_json::Error| CliError::BadOutput {
        what: "image ls".into(),
        message: e.to_string(),
    };
    if text.starts_with('[') {
        let rows: Vec<PodmanImageRow> = serde_json::from_str(text).map_err(bad)?;
        let mut out = Vec::new();
        for r in rows {
            let digest = Some(r.digest).filter(|d| !d.is_empty());
            let size = r.size.map(|s| format!("{}MB", s / 1_000_000));
            if r.names.is_empty() {
                out.push(ImageRow {
                    id: r.id,
                    repository: String::new(),
                    tag: String::new(),
                    digest,
                    size,
                });
                continue;
            }
            for n in r.names {
                let (repository, tag) = match n.rsplit_once(':') {
                    Some((repo, tag)) if !tag.contains('/') => (repo.to_string(), tag.to_string()),
                    _ => (n.clone(), String::new()),
                };
                out.push(ImageRow {
                    id: r.id.clone(),
                    repository,
                    tag,
                    digest: digest.clone(),
                    size: size.clone(),
                });
            }
        }
        return Ok(out);
    }
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| {
            let r: DockerImageRow = serde_json::from_str(l).map_err(bad)?;
            Ok(ImageRow {
                id: r.id,
                repository: r.repository,
                tag: r.tag,
                digest: Some(r.digest).filter(|d| !d.is_empty() && d != "<none>"),
                size: Some(r.size).filter(|s| !s.is_empty()),
            })
        })
        .collect()
}

#[derive(Debug, Deserialize)]
struct ContainerRow {
    #[serde(rename = "Image", default)]
    image: String,
    #[serde(rename = "ImageID", default)]
    image_id: String,
    #[serde(rename = "ID", default)]
    id: String,
}

/// Image references and ids out of `ps -a --format json` (either engine).
pub fn parse_container_images(stdout: &str) -> BTreeSet<String> {
    let text = stdout.trim();
    let rows: Vec<ContainerRow> = if text.starts_with('[') {
        serde_json::from_str(text).unwrap_or_default()
    } else {
        text.lines()
            .filter_map(|l| serde_json::from_str(l.trim()).ok())
            .collect()
    };
    let mut out = BTreeSet::new();
    for r in rows {
        if !r.image.is_empty() {
            out.insert(r.image.clone());
            if let Some(stripped) = r.image.strip_prefix("sha256:") {
                out.insert(stripped.to_string());
                out.insert(stripped[..stripped.len().min(12)].to_string());
            }
        }
        if !r.image_id.is_empty() {
            let bare = r.image_id.trim_start_matches("sha256:");
            out.insert(bare.to_string());
            out.insert(bare[..bare.len().min(12)].to_string());
        }
        let _ = r.id;
    }
    out
}

impl StackRunner<'_> {
    /// Every image an instance of this work folder still pins (its cache),
    /// as whole references (see [`StackRunner::known_images`]).
    fn pinned_images(&self, work_dir: &Path) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        let dir = work_dir.join(crate::intent::INSTANCES_DIR);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return out;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string();
            let cache = ProvenanceCache::load(work_dir, &name);
            out.extend(self.known_images(&cache).into_values());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_list_parsing_both_engines() {
        let docker = r#"{"Containers":"N/A","CreatedAt":"2026-11-13 10:00:00 +0000 UTC","Digest":"sha256:aaa","ID":"111111111111","Repository":"api","Size":"120MB","Tag":"01234567"}
{"Digest":"<none>","ID":"222222222222","Repository":"<none>","Size":"50MB","Tag":"<none>"}
"#;
        let rows = parse_image_list(docker).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].reference(), "api:01234567");
        assert_eq!(rows[0].digest.as_deref(), Some("sha256:aaa"));
        assert_eq!(rows[1].reference(), "222222222222");
        assert_eq!(rows[1].digest, None);
        let podman = r#"[{"Id":"333","Names":["registry.example/api:main","registry.example/api:4711"],"Digest":"sha256:bbb","Size":123456789},{"Id":"444","Names":[],"Digest":"","Size":1}]"#;
        let rows = parse_image_list(podman).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].reference(), "registry.example/api:main");
        assert_eq!(rows[1].reference(), "registry.example/api:4711");
        assert_eq!(rows[0].size.as_deref(), Some("123MB"));
        assert_eq!(rows[2].reference(), "444");
        assert!(parse_image_list("").unwrap().is_empty());
        assert!(parse_image_list("nope").is_err());
    }

    #[test]
    fn container_images_cover_refs_and_ids() {
        let docker = r#"{"ID":"c1","Image":"api:01234567","Names":"app-1-apis-api-1"}
{"ID":"c2","Image":"sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"}"#;
        let set = parse_container_images(docker);
        assert!(set.contains("api:01234567"));
        assert!(set.contains("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"));
        assert!(set.contains("0123456789ab"));
        let podman = r#"[{"Id":"c3","Image":"registry.example/db@sha256:dddd","ImageID":"sha256:eeeeeeeeeeeeeeee"}]"#;
        let set = parse_container_images(podman);
        assert!(set.contains("registry.example/db@sha256:dddd"));
        assert!(set.contains("eeeeeeeeeeee"));
    }

    #[test]
    fn reference_splitting() {
        assert_eq!(
            split_reference("registry.example/api:main"),
            (Some("main"), None)
        );
        assert_eq!(
            split_reference("registry.example/api@sha256:aa"),
            (None, Some("sha256:aa"))
        );
        assert_eq!(split_reference("localhost:5000/api"), (None, None));
        assert_eq!(split_reference("postgres"), (None, None));
    }
}
