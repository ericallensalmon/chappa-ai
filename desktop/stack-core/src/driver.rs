//! The engine-facing contract, and the fakes that let everything above it
//! be tested without an engine.
//!
//! A *driver* applies plans and reports status for the components of one
//! engine kind (compose over docker or podman, terraform, a process, a
//! script). Real drivers live outside this crate and shell out to the
//! engine's CLI; here are the trait they implement, the resolved instance
//! they receive, the nine component states they report, the pre-up checks
//! as data, and a recording [`FakeDriver`].
//!
//! # Component states
//!
//! | state | when |
//! |---|---|
//! | `up` | running; probe passes (or there is none) |
//! | `starting` | running, health still pending |
//! | `unhealthy` | running, probe or healthcheck failing |
//! | `crashloop` | restarting now, or restarted 3+ times, running or not |
//! | `stopped-problem` | not running, but **wanted** running |
//! | `stopped-idle` | not running, not wanted running |
//! | `oneshot-ok` | a one-shot component that exited 0 |
//! | `oneshot-failed` | a one-shot component that exited non-zero |
//! | `not-deployed` | nothing exists for it |
//!
//! Problem versus idle is **desired versus actual** from the intent record,
//! never the healthcheck (a component without one would hide its crashes)
//! and never the exit code (137 and 0 alike mean "not running").

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::fmt;
use std::path::PathBuf;

use crate::definition::{ComponentKind, DriverKind, EngineRef, Mode, ProbeDef, StackDef};
use crate::intent::{instance_id, Desired, IntentRecord};
use crate::placeholders::{Context, HostSide};
use crate::planner::{Change, InstanceState, Plan, PlanError, Step, Verb};
use crate::ports::{self, PortError};
use crate::provenance::{Provenance, Staleness};

/// The kinds of engine a component can run on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EngineKind {
    Docker,
    Podman,
    Terraform,
    Process,
    Script,
}

impl EngineKind {
    pub const ALL: [EngineKind; 5] = [
        EngineKind::Docker,
        EngineKind::Podman,
        EngineKind::Terraform,
        EngineKind::Process,
        EngineKind::Script,
    ];

    pub fn name(self) -> &'static str {
        match self {
            EngineKind::Docker => "docker",
            EngineKind::Podman => "podman",
            EngineKind::Terraform => "terraform",
            EngineKind::Process => "process",
            EngineKind::Script => "script",
        }
    }

    pub fn parse(s: &str) -> Option<EngineKind> {
        EngineKind::ALL.into_iter().find(|k| k.name() == s)
    }

    /// Docker and podman run containers that publish host ports and need
    /// the host-gateway treatment.
    pub fn is_container_engine(self) -> bool {
        matches!(self, EngineKind::Docker | EngineKind::Podman)
    }
}

impl fmt::Display for EngineKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

// -------------------------------------------------------- resolved instance

/// A component with every per-instance decision made: engine, mode, ports,
/// desired state. Templates stay templates; [`ResolvedInstance::context_for`]
/// fills them when the caller knows the remaining values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedComponent {
    pub id: String,
    pub group: String,
    pub engine: EngineRef,
    pub driver: DriverKind,
    pub kind: ComponentKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<Mode>,
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    pub desired: Desired,
    pub normally_off: bool,
    pub ports: Vec<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mb: Option<u32>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub env: IndexMap<String, String>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub outputs: IndexMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe: Option<ProbeDef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedInstance {
    pub stack: String,
    pub slot: u32,
    /// `<stack>-<slot>`.
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<String>,
    pub components: IndexMap<String, ResolvedComponent>,
    /// The values behind `{secret:<name>}`. Filled by the runner from the
    /// work folder, never serialized: an instance goes out on the wire.
    #[serde(skip)]
    pub secrets: crate::instance_secrets::Secrets,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    #[error(transparent)]
    Ports(#[from] PortError),
    #[error("component `{component}` names engine `{engine}`, which is not declared")]
    UnknownEngine { component: String, engine: String },
}

/// Definition × slot × intent. The intent supplies modes, refs and desired
/// states; without one every component is absent in its default mode.
pub fn resolve(
    stack_name: &str,
    stack: &StackDef,
    slot: u32,
    intent: Option<&IntentRecord>,
) -> Result<ResolvedInstance, ResolveError> {
    let layout = ports::layout(stack)?;
    let port_map = layout.instance_ports(slot)?;
    let mut components = IndexMap::new();
    for (name, def) in &stack.components {
        let engine_name = stack.engine_name_for(name).unwrap_or_default();
        let engine = stack
            .engine_ref(&engine_name)
            .ok_or_else(|| ResolveError::UnknownEngine {
                component: name.clone(),
                engine: engine_name.clone(),
            })?;
        let ci = intent.and_then(|i| i.components.get(name));
        components.insert(
            name.clone(),
            ResolvedComponent {
                id: name.clone(),
                group: def.group.clone().unwrap_or_default(),
                engine,
                driver: def.driver,
                kind: def.kind,
                mode: ci.and_then(|c| c.mode).or_else(|| def.default_mode()),
                git_ref: ci.and_then(|c| c.git_ref.clone()),
                desired: ci.map(|c| c.desired).unwrap_or_default(),
                normally_off: def.normally_off,
                ports: port_map.get(name).cloned().unwrap_or_default(),
                memory_mb: def.memory_mb,
                env: def.env.clone(),
                outputs: def.outputs.clone(),
                probe: def.probe.clone(),
            },
        );
    }
    Ok(ResolvedInstance {
        stack: stack_name.to_string(),
        slot,
        id: instance_id(stack_name, slot),
        hostname: stack.hostname.clone(),
        item: intent.and_then(|i| i.item.clone()),
        components,
        secrets: Default::default(),
    })
}

impl ResolvedInstance {
    /// Each engine owns a separate compose project: `<instance>-<group>`.
    pub fn compose_project(&self, group: &str) -> String {
        if group.is_empty() {
            self.id.clone()
        } else {
            format!("{}-{group}", self.id)
        }
    }

    /// Distinct engines in use, in first-use order.
    pub fn engines(&self) -> Vec<EngineRef> {
        let mut out: Vec<EngineRef> = Vec::new();
        for c in self.components.values() {
            if !out.iter().any(|e| e.name == c.engine.name) {
                out.push(c.engine.clone());
            }
        }
        out
    }

    /// `memoryMb` summed per engine: each engine has its own budget (a
    /// podman VM's free memory is not the host's).
    pub fn memory_by_engine(&self) -> IndexMap<String, u32> {
        let mut out: IndexMap<String, u32> = IndexMap::new();
        for c in self.components.values() {
            *out.entry(c.engine.name.clone()).or_insert(0) += c.memory_mb.unwrap_or(0);
        }
        out
    }

    /// Every host port of the instance.
    pub fn ports(&self) -> BTreeSet<u16> {
        self.components
            .values()
            .flat_map(|c| c.ports.iter().copied())
            .collect()
    }

    /// A placeholder context as seen from inside `component` (so `{host}`
    /// is right for its engine), with slot, ports, hostname and item filled
    /// in. The caller adds images, refs, channel and params as it learns
    /// them.
    pub fn context_for(&self, component: &str) -> Context {
        let mut ctx = Context {
            slot: Some(self.slot),
            hostname: self.hostname.clone(),
            item: self.item.clone(),
            instance: Some(self.id.clone()),
            secrets: self.secrets.clone(),
            ..Context::default()
        };
        for (name, c) in &self.components {
            ctx.ports.insert(name.clone(), c.ports.clone());
            if let Some(r) = &c.git_ref {
                ctx.refs.insert(name.clone(), r.clone());
            }
        }
        ctx.host = Some(
            self.components
                .get(component)
                .map(|c| HostSide::for_engine(c.engine.kind))
                .unwrap_or(HostSide::Host),
        );
        ctx
    }

    /// The context for strings used on the host itself (probes, the agent env).
    pub fn host_context(&self) -> Context {
        let mut ctx = self.context_for("");
        ctx.host = Some(HostSide::Host);
        ctx
    }
}

// ---------------------------------------------------------- component state

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ComponentState {
    Up,
    Starting,
    Unhealthy,
    Crashloop,
    StoppedProblem,
    StoppedIdle,
    OneshotOk,
    OneshotFailed,
    NotDeployed,
}

/// Badge tone a state renders with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tone {
    Ok,
    Warn,
    Bad,
    Muted,
}

impl ComponentState {
    pub const ALL: [ComponentState; 9] = [
        ComponentState::Up,
        ComponentState::Starting,
        ComponentState::Unhealthy,
        ComponentState::Crashloop,
        ComponentState::StoppedProblem,
        ComponentState::StoppedIdle,
        ComponentState::OneshotOk,
        ComponentState::OneshotFailed,
        ComponentState::NotDeployed,
    ];

    pub fn tone(self) -> Tone {
        match self {
            ComponentState::Up => Tone::Ok,
            ComponentState::Starting => Tone::Warn,
            ComponentState::Unhealthy
            | ComponentState::Crashloop
            | ComponentState::StoppedProblem
            | ComponentState::OneshotFailed => Tone::Bad,
            ComponentState::StoppedIdle
            | ComponentState::OneshotOk
            | ComponentState::NotDeployed => Tone::Muted,
        }
    }

    /// Red states: they count toward the Environment alert dot.
    pub fn is_problem(self) -> bool {
        self.tone() == Tone::Bad
    }

    /// Hidden by default behind a "show not deployed" toggle.
    pub fn hidden_by_default(self) -> bool {
        self == ComponentState::NotDeployed
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ComponentState::Up => "up",
            ComponentState::Starting => "starting",
            ComponentState::Unhealthy => "unhealthy",
            ComponentState::Crashloop => "crashloop",
            ComponentState::StoppedProblem => "stopped-problem",
            ComponentState::StoppedIdle => "stopped-idle",
            ComponentState::OneshotOk => "oneshot-ok",
            ComponentState::OneshotFailed => "oneshot-failed",
            ComponentState::NotDeployed => "not-deployed",
        }
    }
}

impl fmt::Display for ComponentState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Engine-reported health of a running component.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Health {
    /// No healthcheck.
    #[default]
    Unknown,
    Starting,
    Healthy,
    Unhealthy,
}

/// What the engine says about a component, before interpretation.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Actual {
    pub exists: bool,
    pub running: bool,
    pub restarting: bool,
    pub restarts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub health: Health,
    /// Probe result, when a probe was run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe_up: Option<bool>,
}

/// Restarts at or past this count read as a crash loop.
pub const CRASHLOOP_RESTARTS: u32 = 3;

/// The one place the nine states are decided.
pub fn component_state(kind: ComponentKind, desired: Desired, actual: &Actual) -> ComponentState {
    if !actual.exists {
        return ComponentState::NotDeployed;
    }
    if actual.restarting || actual.restarts >= CRASHLOOP_RESTARTS {
        return ComponentState::Crashloop;
    }
    match kind {
        ComponentKind::Oneshot => {
            if actual.running {
                ComponentState::Starting
            } else {
                match actual.exit_code {
                    Some(0) => ComponentState::OneshotOk,
                    Some(_) => ComponentState::OneshotFailed,
                    // Created but never ran: it has not failed, and it is not done.
                    None => {
                        if desired == Desired::Running {
                            ComponentState::StoppedProblem
                        } else {
                            ComponentState::StoppedIdle
                        }
                    }
                }
            }
        }
        ComponentKind::Service => {
            if actual.running {
                if actual.health == Health::Unhealthy || actual.probe_up == Some(false) {
                    ComponentState::Unhealthy
                } else if actual.health == Health::Starting {
                    ComponentState::Starting
                } else {
                    ComponentState::Up
                }
            } else if desired == Desired::Running {
                ComponentState::StoppedProblem
            } else {
                ComponentState::StoppedIdle
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComponentStatus {
    pub id: String,
    pub state: ComponentState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uptime: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restarts: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Provenance>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stale: Vec<Staleness>,
}

/// Counts per state, for the view header (zero states omitted).
pub fn summarize(statuses: &[ComponentStatus]) -> IndexMap<ComponentState, usize> {
    let mut out = IndexMap::new();
    for s in statuses {
        *out.entry(s.state).or_insert(0) += 1;
    }
    out
}

// ------------------------------------------------------------------ driver

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StepFailure {
    pub step: Step,
    pub reason: String,
}

/// What applying a plan did. `done` holds the steps that completed, in
/// order; `failed` the first one that did not (nothing after it ran).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyReport {
    pub done: Vec<Step>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed: Option<StepFailure>,
}

impl ApplyReport {
    pub fn ok(&self) -> bool {
        self.failed.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DriverError {
    #[error(transparent)]
    Plan(#[from] PlanError),
    #[error("`{verb:?}` was asked of a plan for `{found:?}`")]
    WrongVerb { verb: Verb, found: Verb },
    #[error("{driver} driver does not support `{verb:?}`")]
    Unsupported { driver: DriverKind, verb: Verb },
    #[error("{0}")]
    Engine(String),
}

/// The verbs, each taking the resolved instance. Every mutating verb
/// takes the plan it executes, so what the user confirmed is exactly what
/// runs; the per-verb methods only check the plan's verb before handing it
/// to [`Driver::apply`].
pub trait Driver {
    /// Plan `change` against `state` for this instance.
    fn plan(
        &self,
        inst: &ResolvedInstance,
        state: &InstanceState,
        change: &Change,
    ) -> Result<Plan, DriverError>;

    /// Execute a plan's steps in order, stopping at the first failure.
    fn apply(&mut self, inst: &ResolvedInstance, plan: &Plan) -> Result<ApplyReport, DriverError>;

    /// Current state of every component of this instance.
    fn status(&self, inst: &ResolvedInstance) -> Result<Vec<ComponentStatus>, DriverError>;

    fn up(&mut self, inst: &ResolvedInstance, plan: &Plan) -> Result<ApplyReport, DriverError> {
        expect_verb(plan, Verb::Up)?;
        self.apply(inst, plan)
    }

    fn start(&mut self, inst: &ResolvedInstance, plan: &Plan) -> Result<ApplyReport, DriverError> {
        expect_verb(plan, Verb::Start)?;
        self.apply(inst, plan)
    }

    fn stop(&mut self, inst: &ResolvedInstance, plan: &Plan) -> Result<ApplyReport, DriverError> {
        expect_verb(plan, Verb::Stop)?;
        self.apply(inst, plan)
    }

    fn down(&mut self, inst: &ResolvedInstance, plan: &Plan) -> Result<ApplyReport, DriverError> {
        expect_verb(plan, Verb::Down)?;
        self.apply(inst, plan)
    }

    fn rebuild(
        &mut self,
        inst: &ResolvedInstance,
        plan: &Plan,
    ) -> Result<ApplyReport, DriverError> {
        expect_verb(plan, Verb::Rebuild)?;
        self.apply(inst, plan)
    }
}

fn expect_verb(plan: &Plan, verb: Verb) -> Result<(), DriverError> {
    if plan.verb == verb {
        Ok(())
    } else {
        Err(DriverError::WrongVerb {
            verb,
            found: plan.verb,
        })
    }
}

// ------------------------------------------------------------------ checks

/// A pre-up check, as data. Each engine is checked on its own: reachability
/// (including a docker daemon in the wrong OS mode), memory against that
/// engine's own budget, and so on.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "check")]
pub enum Check {
    EngineReachable { engine: String },
    Memory { engine: String, needed_mb: u32 },
    PortsFree { ports: Vec<u16> },
    RegistryLogin { registry: String },
    TerraformInit { dir: PathBuf },
    HostnameResolves { hostname: String },
}

impl Check {
    /// A short stable key, for fakes and for de-duplication.
    pub fn key(&self) -> String {
        match self {
            Check::EngineReachable { engine } => format!("reachable:{engine}"),
            Check::Memory { engine, .. } => format!("memory:{engine}"),
            Check::PortsFree { .. } => "ports".to_string(),
            Check::RegistryLogin { registry } => format!("registry:{registry}"),
            Check::TerraformInit { dir } => format!("terraform-init:{}", dir.display()),
            Check::HostnameResolves { hostname } => format!("hostname:{hostname}"),
        }
    }

    pub fn engine(&self) -> Option<&str> {
        match self {
            Check::EngineReachable { engine } | Check::Memory { engine, .. } => Some(engine),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "outcome")]
pub enum Outcome {
    Pass,
    Fail {
        reason: String,
        /// A command the user can run to fix it (`docker login …`,
        /// `terraform init`), offered as a terminal action.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fix: Option<Vec<String>>,
    },
    Skipped {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckResult {
    pub check: Check,
    pub outcome: Outcome,
}

impl CheckResult {
    pub fn blocks(&self) -> bool {
        matches!(self.outcome, Outcome::Fail { .. })
    }
}

/// Runs one check. Real checkers call the engines; [`FakeChecker`] is scripted.
pub trait Checker {
    fn check(&self, check: &Check) -> Outcome;
}

/// The checks an instance needs before `up`, in the order they are shown.
pub fn checks_for(inst: &ResolvedInstance, stack: &StackDef) -> Vec<Check> {
    let mut out = Vec::new();
    let memory = inst.memory_by_engine();
    for engine in inst.engines() {
        out.push(Check::EngineReachable {
            engine: engine.name.clone(),
        });
        let needed = memory.get(&engine.name).copied().unwrap_or(0);
        if needed > 0 {
            out.push(Check::Memory {
                engine: engine.name.clone(),
                needed_mb: needed,
            });
        }
    }
    let ports: Vec<u16> = inst.ports().into_iter().collect();
    if !ports.is_empty() {
        out.push(Check::PortsFree { ports });
    }
    if let Some(hostname) = &stack.hostname {
        out.push(Check::HostnameResolves {
            hostname: hostname.clone(),
        });
    }
    if let Some(registry) = &stack.registry {
        let pushes = inst
            .components
            .values()
            .any(|c| c.mode == Some(Mode::Source) && !stack.image_dependents(&c.id).is_empty());
        if pushes {
            out.push(Check::RegistryLogin {
                registry: registry.clone(),
            });
        }
    }
    for (name, def) in &stack.components {
        if def.driver == DriverKind::Terraform {
            if let Some(dir) = &def.dir {
                let _ = name;
                out.push(Check::TerraformInit { dir: dir.clone() });
            }
        }
    }
    out
}

/// The checks before applying `plan`: memory and ports only for the
/// components it starts or recreates and that are not running now, the
/// registry only when it builds a pushed image, the hostname when the stack
/// has one, terraform init for the terraform components it touches.
pub fn checks_for_plan(
    inst: &ResolvedInstance,
    stack: &StackDef,
    plan: &Plan,
    state: &InstanceState,
) -> Vec<Check> {
    let mut out = Vec::new();
    let starting: Vec<&ResolvedComponent> = plan
        .steps
        .iter()
        .filter(|s| matches!(s, Step::Start { .. } | Step::Recreate { .. }))
        .filter(|s| !state.component(s.component()).running)
        .filter_map(|s| inst.components.get(s.component()))
        .collect();
    let mut memory: IndexMap<String, u32> = IndexMap::new();
    let mut ports: BTreeSet<u16> = BTreeSet::new();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for c in &starting {
        if !seen.insert(c.id.as_str()) {
            continue;
        }
        *memory.entry(c.engine.name.clone()).or_insert(0) += c.memory_mb.unwrap_or(0);
        ports.extend(c.ports.iter().copied());
    }
    for engine in inst.engines() {
        // Process and script components run on the host: there is no
        // engine of theirs to reach.
        if !engine.kind.is_container_engine()
            || !plan
                .components()
                .iter()
                .any(|c| inst.components[c].engine.name == engine.name)
        {
            continue;
        }
        out.push(Check::EngineReachable {
            engine: engine.name.clone(),
        });
        let needed = memory.get(&engine.name).copied().unwrap_or(0);
        if needed > 0 {
            out.push(Check::Memory {
                engine: engine.name.clone(),
                needed_mb: needed,
            });
        }
    }
    if !ports.is_empty() {
        out.push(Check::PortsFree {
            ports: ports.into_iter().collect(),
        });
    }
    if let Some(hostname) = &stack.hostname {
        if !starting.is_empty() {
            out.push(Check::HostnameResolves {
                hostname: hostname.clone(),
            });
        }
    }
    if let Some(registry) = &stack.registry {
        let pushes = plan.steps.iter().any(|s| {
            matches!(s, Step::Build { .. }) && !stack.image_dependents(s.component()).is_empty()
        });
        if pushes {
            out.push(Check::RegistryLogin {
                registry: registry.clone(),
            });
        }
    }
    for name in plan.components() {
        if let Some(def) = stack.components.get(&name) {
            if def.driver == DriverKind::Terraform {
                if let Some(dir) = &def.dir {
                    out.push(Check::TerraformInit { dir: dir.clone() });
                }
            }
        }
    }
    out
}

pub fn run_checks(checker: &dyn Checker, checks: &[Check]) -> Vec<CheckResult> {
    checks
        .iter()
        .map(|c| CheckResult {
            check: c.clone(),
            outcome: checker.check(c),
        })
        .collect()
}

/// One line naming each engine's reachability, the way the view header
/// shows it: `docker reachable · podman unreachable (…)`.
pub fn summary_line(results: &[CheckResult]) -> String {
    let parts: Vec<String> = results
        .iter()
        .filter_map(|r| match (&r.check, &r.outcome) {
            (Check::EngineReachable { engine }, Outcome::Pass) => {
                Some(format!("{engine} reachable"))
            }
            (Check::EngineReachable { engine }, Outcome::Fail { reason, .. }) => {
                Some(format!("{engine} unreachable ({reason})"))
            }
            (Check::EngineReachable { engine }, Outcome::Skipped { .. }) => {
                Some(format!("{engine} not checked"))
            }
            _ => None,
        })
        .collect();
    parts.join(" · ")
}

/// Scripted outcomes by [`Check::key`]; everything else passes.
#[derive(Debug, Default)]
pub struct FakeChecker {
    pub outcomes: IndexMap<String, Outcome>,
}

impl FakeChecker {
    pub fn failing(mut self, key: &str, reason: &str, fix: Option<Vec<&str>>) -> Self {
        self.outcomes.insert(
            key.to_string(),
            Outcome::Fail {
                reason: reason.to_string(),
                fix: fix.map(|f| f.iter().map(|s| s.to_string()).collect()),
            },
        );
        self
    }
}

impl Checker for FakeChecker {
    fn check(&self, check: &Check) -> Outcome {
        self.outcomes
            .get(&check.key())
            .cloned()
            .unwrap_or(Outcome::Pass)
    }
}

// -------------------------------------------------------------- fake driver

/// A driver that records every call and keeps a toy engine state, so plan
/// and status logic can be exercised end to end.
#[derive(Debug, Default)]
pub struct FakeDriver {
    /// `"<verb> <component>"` per step applied, plus `"status"` per status call.
    pub calls: RefCell<Vec<String>>,
    pub actual: IndexMap<String, Actual>,
    pub provenance: IndexMap<String, Provenance>,
    /// A component whose next step fails with this reason.
    pub fail_on: Option<(String, String)>,
}

impl FakeDriver {
    /// Project the toy engine state into the planner's view.
    pub fn instance_state(&self, inst: &ResolvedInstance) -> InstanceState {
        let mut state = InstanceState::new(&inst.stack, inst.slot);
        for (name, c) in &inst.components {
            let a = self.actual.get(name).cloned().unwrap_or_default();
            let p = self.provenance.get(name);
            state.components.insert(
                name.clone(),
                crate::planner::ComponentCurrent {
                    mode: p.map(|p| p.mode).or(c.mode),
                    git_ref: p
                        .and_then(|p| p.git_ref.clone())
                        .or_else(|| c.git_ref.clone()),
                    commit: p.and_then(|p| p.commit.clone()),
                    dirty: p.and_then(|p| p.dirty.clone()),
                    image: p.and_then(|p| p.image.clone()),
                    desired: c.desired,
                    exists: a.exists,
                    running: a.running,
                    exit_code: a.exit_code,
                    gateway: None,
                },
            );
        }
        state
    }

    fn record(&self, step: &Step) {
        let verb = match step {
            Step::Checkout { .. } => "checkout",
            Step::Build { .. } => "build",
            Step::Pull { .. } => "pull",
            Step::Recreate { .. } => "recreate",
            Step::Start { .. } => "start",
            Step::Stop { .. } => "stop",
            Step::Remove { .. } => "remove",
            Step::Script { verb, .. } => verb.as_str(),
        };
        self.calls
            .borrow_mut()
            .push(format!("{verb} {}", step.component()));
    }
}

impl Driver for FakeDriver {
    fn plan(
        &self,
        inst: &ResolvedInstance,
        state: &InstanceState,
        change: &Change,
    ) -> Result<Plan, DriverError> {
        let _ = inst;
        let stack = state_stack(inst);
        Ok(crate::planner::plan(&stack, state, change)?)
    }

    fn apply(&mut self, inst: &ResolvedInstance, plan: &Plan) -> Result<ApplyReport, DriverError> {
        let mut report = ApplyReport::default();
        for step in &plan.steps {
            if let Some((component, reason)) = &self.fail_on {
                if step.component() == component {
                    report.failed = Some(StepFailure {
                        step: step.clone(),
                        reason: reason.clone(),
                    });
                    return Ok(report);
                }
            }
            self.record(step);
            let kind = inst
                .components
                .get(step.component())
                .map(|c| c.kind)
                .unwrap_or_default();
            let a = self.actual.entry(step.component().to_string()).or_default();
            match step {
                Step::Start { .. } | Step::Recreate { .. } => {
                    a.exists = true;
                    a.running = kind == ComponentKind::Service;
                    a.exit_code = (kind == ComponentKind::Oneshot).then_some(0);
                    a.restarts = 0;
                    a.restarting = false;
                }
                Step::Stop { .. } => {
                    a.running = false;
                    a.exit_code = Some(0);
                }
                Step::Remove { .. } => {
                    *a = Actual::default();
                }
                // The toy engine treats a script like the verb it stands for.
                Step::Script { verb, .. } => match verb.desired_after() {
                    Some(Desired::Running) => {
                        a.exists = true;
                        a.running = kind == ComponentKind::Service;
                    }
                    Some(Desired::Stopped) => a.running = false,
                    Some(Desired::Absent) => *a = Actual::default(),
                    None => {}
                },
                Step::Build { key, .. } => {
                    let p = self
                        .provenance
                        .entry(step.component().to_string())
                        .or_insert_with(|| Provenance::new(Mode::Source));
                    p.mode = Mode::Source;
                    p.image = Some(format!(
                        "built:{}",
                        key.clone().unwrap_or_else(|| "pending".into())
                    ));
                }
                Step::Pull { image, .. } => {
                    let p = self
                        .provenance
                        .entry(step.component().to_string())
                        .or_insert_with(|| Provenance::new(Mode::Prebuilt));
                    p.mode = Mode::Prebuilt;
                    p.image = Some(format!("pulled:{image}"));
                }
                Step::Checkout { git_ref, .. } => {
                    let p = self
                        .provenance
                        .entry(step.component().to_string())
                        .or_insert_with(|| Provenance::new(Mode::Dev));
                    p.git_ref = Some(git_ref.clone());
                }
            }
            report.done.push(step.clone());
        }
        Ok(report)
    }

    fn status(&self, inst: &ResolvedInstance) -> Result<Vec<ComponentStatus>, DriverError> {
        self.calls.borrow_mut().push("status".to_string());
        Ok(inst
            .components
            .values()
            .map(|c| {
                let a = self.actual.get(&c.id).cloned().unwrap_or_default();
                ComponentStatus {
                    id: c.id.clone(),
                    state: component_state(c.kind, c.desired, &a),
                    uptime: None,
                    restarts: a.exists.then_some(a.restarts),
                    exit_code: a.exit_code,
                    provenance: self.provenance.get(&c.id).cloned(),
                    stale: Vec::new(),
                }
            })
            .collect())
    }
}

/// The fake has no definition of its own; it rebuilds the little it needs
/// (names, kinds, groups, dependencies are not known here) from the
/// resolved instance. Tests that need real ordering plan through
/// [`crate::planner::plan`] with the definition directly.
fn state_stack(inst: &ResolvedInstance) -> StackDef {
    let mut stack = StackDef::default();
    for (name, c) in &inst.components {
        let mut def = crate::definition::ComponentDef::new(c.driver);
        def.kind = c.kind;
        def.group = Some(c.group.clone()).filter(|g| !g.is_empty());
        def.normally_off = c.normally_off;
        if let Some(m) = c.mode {
            def.modes.insert(
                m.as_str().to_string(),
                crate::definition::ModeDef::default(),
            );
            def.default = Some(m);
        }
        stack.components.insert(name.clone(), def);
    }
    stack
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::definition::StacksDocument;
    use crate::intent::ComponentIntent;
    use crate::planner::Selection;

    fn stack() -> StackDef {
        let text = r#"
stacks:
  app:
    slots: { count: 3, portBase: 14000, portStride: 100 }
    hostname: dev.example.test
    registry: registry.example/dev
    groups:
      core: { engine: docker }
      apis: { engine: podman }
    components:
      db:
        group: apis
        driver: compose
        file: s.yml
        modes: { prebuilt: { image: "registry.example/db", tag: "{channel}" } }
        memoryMb: 1024
      api:
        group: apis
        driver: compose
        file: s.yml
        modes:
          prebuilt: { image: "registry.example/api", tag: "{channel}" }
          source:   { repo: ../api }
        depends_on: [db]
        env: { DB_URL: "postgres://{host}:{port:db}/app" }
        outputs: { url: "http://{hostname}:{port:api}" }
        memoryMb: 512
      web:
        group: core
        driver: process
        command: web-dev
        modes: { dev: { repo: ../web } }
        depends_on: [api]
        env: { API_URL: "http://{host}:{port:api}" }
        memoryMb: 256
      migrate:
        group: apis
        driver: compose
        file: s.yml
        kind: oneshot
        ports: 0
        modes: { prebuilt: { image: "registry.example/api", tag: "{channel}" } }
      infra:
        driver: terraform
        dir: infra
        vars: { image_api: "{image:api}" }
"#;
        StacksDocument::parse(text)
            .unwrap()
            .stacks
            .shift_remove("app")
            .unwrap()
    }

    fn intent_running() -> IntentRecord {
        let mut i = IntentRecord::new("app", 1, "2026-01-01T00:00:00Z");
        i.item = Some("KEY-12".into());
        for c in ["db", "api", "web", "migrate"] {
            i.components.insert(
                c.into(),
                ComponentIntent {
                    desired: Desired::Running,
                    ..Default::default()
                },
            );
        }
        i.components["api"].mode = Some(Mode::Source);
        i.components["api"].git_ref = Some("feature/x".into());
        i
    }

    #[test]
    fn engine_kind_names_round_trip() {
        for k in EngineKind::ALL {
            assert_eq!(EngineKind::parse(k.name()), Some(k));
            assert_eq!(serde_yaml::from_str::<EngineKind>(k.name()).unwrap(), k);
        }
        assert_eq!(EngineKind::parse("lxd"), None);
        assert!(EngineKind::Docker.is_container_engine());
        assert!(EngineKind::Podman.is_container_engine());
        assert!(!EngineKind::Terraform.is_container_engine());
    }

    #[test]
    fn resolve_fills_engines_ports_modes_and_desired() {
        let st = stack();
        let inst = resolve("app", &st, 1, Some(&intent_running())).unwrap();
        assert_eq!(inst.id, "app-1");
        assert_eq!(inst.hostname.as_deref(), Some("dev.example.test"));
        assert_eq!(inst.item.as_deref(), Some("KEY-12"));
        let api = &inst.components["api"];
        assert_eq!(api.engine.kind, EngineKind::Podman);
        assert_eq!(api.engine.name, "podman");
        assert_eq!(api.group, "apis");
        assert_eq!(api.mode, Some(Mode::Source));
        assert_eq!(api.git_ref.as_deref(), Some("feature/x"));
        assert_eq!(api.desired, Desired::Running);
        assert_eq!(api.ports, vec![14101]);
        assert_eq!(inst.components["db"].ports, vec![14100]);
        assert_eq!(inst.components["web"].ports, vec![14102]);
        assert!(inst.components["migrate"].ports.is_empty());
        assert_eq!(
            inst.components["db"].mode,
            Some(Mode::Prebuilt),
            "default mode without intent entry details"
        );
        assert_eq!(inst.components["infra"].desired, Desired::Absent);
        assert_eq!(inst.components["infra"].engine.kind, EngineKind::Terraform);
        assert_eq!(inst.components["infra"].group, "");
        // without an intent, everything is absent in its default mode
        let bare = resolve("app", &st, 0, None).unwrap();
        assert!(bare
            .components
            .values()
            .all(|c| c.desired == Desired::Absent));
        assert_eq!(bare.components["api"].mode, Some(Mode::Prebuilt));
        assert_eq!(bare.item, None);
    }

    #[test]
    fn resolve_reports_port_and_engine_problems() {
        let mut st = stack();
        st.slots.port_stride = 1;
        assert!(matches!(
            resolve("app", &st, 0, None).unwrap_err(),
            ResolveError::Ports(PortError::StrideTooSmall { .. })
        ));
        let mut st = stack();
        st.components["db"].engine = Some("lxd".into());
        assert_eq!(
            resolve("app", &st, 0, None).unwrap_err(),
            ResolveError::UnknownEngine {
                component: "db".into(),
                engine: "lxd".into()
            }
        );
        assert!(matches!(
            resolve("app", &stack(), 3, None).unwrap_err(),
            ResolveError::Ports(PortError::SlotOutOfRange { slot: 3, count: 3 })
        ));
    }

    #[test]
    fn compose_projects_engines_memory_and_ports() {
        let inst = resolve("app", &stack(), 2, None).unwrap();
        assert_eq!(inst.compose_project("apis"), "app-2-apis");
        assert_eq!(inst.compose_project("core"), "app-2-core");
        assert_eq!(inst.compose_project(""), "app-2");
        let engines: Vec<String> = inst.engines().into_iter().map(|e| e.name).collect();
        assert_eq!(engines, ["podman", "docker", "terraform"]);
        let mem = inst.memory_by_engine();
        assert_eq!(mem["podman"], 1024 + 512);
        assert_eq!(mem["docker"], 256);
        assert_eq!(mem["terraform"], 0);
        assert_eq!(
            inst.ports().into_iter().collect::<Vec<_>>(),
            vec![14200, 14201, 14202]
        );
    }

    #[test]
    fn context_resolves_host_per_engine_and_templates() {
        let inst = resolve("app", &stack(), 1, Some(&intent_running())).unwrap();
        let from_api = inst.context_for("api");
        assert_eq!(from_api.host, Some(HostSide::Podman));
        assert_eq!(from_api.slot, Some(1));
        assert_eq!(from_api.item.as_deref(), Some("KEY-12"));
        assert_eq!(from_api.refs["api"], "feature/x");
        assert_eq!(
            crate::placeholders::resolve(&inst.components["api"].env["DB_URL"], &from_api).unwrap(),
            "postgres://host.containers.internal:14100/app"
        );
        let from_web = inst.context_for("web");
        assert_eq!(from_web.host, Some(HostSide::Docker));
        assert_eq!(
            crate::placeholders::resolve(&inst.components["web"].env["API_URL"], &from_web)
                .unwrap(),
            "http://host.docker.internal:14101"
        );
        let host = inst.host_context();
        assert_eq!(host.host, Some(HostSide::Host));
        assert_eq!(
            crate::placeholders::resolve(&inst.components["api"].outputs["url"], &host).unwrap(),
            "http://dev.example.test:14101"
        );
        assert_eq!(inst.context_for("infra").host, Some(HostSide::Host));
        assert_eq!(inst.context_for("unknown").host, Some(HostSide::Host));
    }

    #[test]
    fn nine_states_table() {
        use ComponentKind::*;
        use ComponentState::*;
        use Desired::*;
        let a = |exists, running, restarting, restarts, exit_code, health, probe_up| Actual {
            exists,
            running,
            restarting,
            restarts,
            exit_code,
            health,
            probe_up,
        };
        let table = [
            // services
            (
                Service,
                Running,
                a(false, false, false, 0, None, Health::Unknown, None),
                NotDeployed,
            ),
            (
                Service,
                Absent,
                a(false, false, false, 0, None, Health::Unknown, None),
                NotDeployed,
            ),
            (
                Service,
                Running,
                a(true, true, false, 0, None, Health::Unknown, None),
                Up,
            ),
            (
                Service,
                Running,
                a(true, true, false, 0, None, Health::Healthy, Some(true)),
                Up,
            ),
            (
                Service,
                Stopped,
                a(true, true, false, 0, None, Health::Unknown, None),
                Up,
            ),
            (
                Service,
                Running,
                a(true, true, false, 0, None, Health::Starting, None),
                Starting,
            ),
            (
                Service,
                Running,
                a(true, true, false, 0, None, Health::Unhealthy, None),
                Unhealthy,
            ),
            (
                Service,
                Running,
                a(true, true, false, 0, None, Health::Healthy, Some(false)),
                Unhealthy,
            ),
            (
                Service,
                Running,
                a(true, true, false, 0, None, Health::Starting, Some(false)),
                Unhealthy,
            ),
            (
                Service,
                Running,
                a(true, true, true, 0, None, Health::Healthy, None),
                Crashloop,
            ),
            (
                Service,
                Running,
                a(true, true, false, 3, None, Health::Healthy, None),
                Crashloop,
            ),
            (
                Service,
                Running,
                a(true, false, false, 3, Some(1), Health::Unknown, None),
                Crashloop,
            ),
            (
                Service,
                Running,
                a(true, true, false, 2, None, Health::Healthy, None),
                Up,
            ),
            (
                Service,
                Running,
                a(true, false, false, 0, Some(137), Health::Unknown, None),
                StoppedProblem,
            ),
            (
                Service,
                Running,
                a(true, false, false, 0, Some(0), Health::Unknown, None),
                StoppedProblem,
            ),
            (
                Service,
                Stopped,
                a(true, false, false, 0, Some(137), Health::Unknown, None),
                StoppedIdle,
            ),
            (
                Service,
                Absent,
                a(true, false, false, 0, Some(0), Health::Unknown, None),
                StoppedIdle,
            ),
            // oneshots
            (
                Oneshot,
                Running,
                a(false, false, false, 0, None, Health::Unknown, None),
                NotDeployed,
            ),
            (
                Oneshot,
                Running,
                a(true, true, false, 0, None, Health::Unknown, None),
                Starting,
            ),
            (
                Oneshot,
                Running,
                a(true, false, false, 0, Some(0), Health::Unknown, None),
                OneshotOk,
            ),
            (
                Oneshot,
                Stopped,
                a(true, false, false, 0, Some(0), Health::Unknown, None),
                OneshotOk,
            ),
            (
                Oneshot,
                Running,
                a(true, false, false, 0, Some(2), Health::Unknown, None),
                OneshotFailed,
            ),
            (
                Oneshot,
                Running,
                a(true, false, false, 0, None, Health::Unknown, None),
                StoppedProblem,
            ),
            (
                Oneshot,
                Stopped,
                a(true, false, false, 0, None, Health::Unknown, None),
                StoppedIdle,
            ),
            (
                Oneshot,
                Running,
                a(true, false, true, 0, Some(1), Health::Unknown, None),
                Crashloop,
            ),
        ];
        for (i, (kind, desired, actual, want)) in table.iter().enumerate() {
            assert_eq!(
                component_state(*kind, *desired, actual),
                *want,
                "row {i}: {actual:?}"
            );
        }
    }

    #[test]
    fn state_tones_and_wire_names() {
        assert_eq!(ComponentState::Up.tone(), Tone::Ok);
        assert_eq!(ComponentState::Starting.tone(), Tone::Warn);
        for s in [
            ComponentState::Unhealthy,
            ComponentState::Crashloop,
            ComponentState::StoppedProblem,
            ComponentState::OneshotFailed,
        ] {
            assert!(s.is_problem(), "{s}");
        }
        for s in [
            ComponentState::StoppedIdle,
            ComponentState::OneshotOk,
            ComponentState::NotDeployed,
        ] {
            assert_eq!(s.tone(), Tone::Muted, "{s}");
            assert!(!s.is_problem());
        }
        assert!(ComponentState::NotDeployed.hidden_by_default());
        assert!(!ComponentState::StoppedIdle.hidden_by_default());
        for s in ComponentState::ALL {
            assert_eq!(
                serde_json::to_string(&s).unwrap(),
                format!("\"{}\"", s.as_str())
            );
            assert_eq!(
                serde_json::from_str::<ComponentState>(&format!("\"{s}\"")).unwrap(),
                s
            );
        }
    }

    #[test]
    fn checks_for_instance_cover_each_engine_separately() {
        let st = stack();
        let inst = resolve("app", &st, 1, Some(&intent_running())).unwrap();
        let checks = checks_for(&inst, &st);
        assert_eq!(
            checks,
            vec![
                Check::EngineReachable {
                    engine: "podman".into()
                },
                Check::Memory {
                    engine: "podman".into(),
                    needed_mb: 1536
                },
                Check::EngineReachable {
                    engine: "docker".into()
                },
                Check::Memory {
                    engine: "docker".into(),
                    needed_mb: 256
                },
                Check::EngineReachable {
                    engine: "terraform".into()
                },
                Check::PortsFree {
                    ports: vec![14100, 14101, 14102]
                },
                Check::HostnameResolves {
                    hostname: "dev.example.test".into()
                },
                Check::RegistryLogin {
                    registry: "registry.example/dev".into()
                },
                Check::TerraformInit {
                    dir: PathBuf::from("infra")
                },
            ]
        );
        // api in prebuilt mode: nothing is pushed, so no registry login check
        let bare = resolve("app", &st, 0, None).unwrap();
        assert!(!checks_for(&bare, &st)
            .iter()
            .any(|c| matches!(c, Check::RegistryLogin { .. })));
        assert_eq!(
            Check::Memory {
                engine: "podman".into(),
                needed_mb: 1
            }
            .key(),
            "memory:podman"
        );
        assert_eq!(Check::PortsFree { ports: vec![] }.engine(), None);
    }

    #[test]
    fn fake_checker_and_summary_line() {
        let st = stack();
        let inst = resolve("app", &st, 1, Some(&intent_running())).unwrap();
        let checker = FakeChecker::default()
            .failing(
                "reachable:docker",
                "daemon is in Linux-containers mode",
                None,
            )
            .failing(
                "registry:registry.example/dev",
                "not logged in",
                Some(vec!["docker", "login", "registry.example"]),
            );
        let results = run_checks(&checker, &checks_for(&inst, &st));
        let blocking: Vec<String> = results
            .iter()
            .filter(|r| r.blocks())
            .map(|r| r.check.key())
            .collect();
        assert_eq!(
            blocking,
            ["reachable:docker", "registry:registry.example/dev"]
        );
        match &results[7].outcome {
            Outcome::Fail { fix: Some(fix), .. } => {
                assert_eq!(fix, &["docker", "login", "registry.example"])
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            summary_line(&results),
            "podman reachable · docker unreachable (daemon is in Linux-containers mode) · terraform reachable"
        );
        let json = serde_json::to_value(&results[0]).unwrap();
        assert_eq!(json["check"]["check"], "engine-reachable");
        assert_eq!(json["outcome"]["outcome"], "pass");
    }

    #[test]
    fn fake_driver_applies_plans_and_reports_status() {
        let st = stack();
        let mut intent = IntentRecord::new("app", 0, "2026-01-01T00:00:00Z");
        let inst = resolve("app", &st, 0, Some(&intent)).unwrap();
        let mut drv = FakeDriver::default();

        // everything not deployed
        let status = drv.status(&inst).unwrap();
        assert!(status
            .iter()
            .all(|s| s.state == ComponentState::NotDeployed));
        assert_eq!(summarize(&status)[&ComponentState::NotDeployed], 5);

        // plan + up through the real planner with the real definition
        let state = drv.instance_state(&inst);
        let plan = crate::planner::plan(
            &st,
            &state,
            &Change::Up {
                selection: Selection::Default,
                modes: IndexMap::new(),
                channel: None,
            },
        )
        .unwrap();
        let report = drv.up(&inst, &plan).unwrap();
        assert!(report.ok());
        assert_eq!(report.done.len(), plan.steps.len());
        assert_eq!(
            *drv.calls.borrow(),
            vec![
                "status",
                "pull db",
                "start db",
                "pull api",
                "start api",
                "start web",
                "pull migrate",
                "start migrate",
                "start infra"
            ]
        );
        intent.record_plan(&plan, "2026-01-01T00:01:00Z");
        let inst = resolve("app", &st, 0, Some(&intent)).unwrap();
        let status = drv.status(&inst).unwrap();
        let by_id: IndexMap<&str, ComponentState> =
            status.iter().map(|s| (s.id.as_str(), s.state)).collect();
        assert_eq!(by_id["db"], ComponentState::Up);
        assert_eq!(by_id["api"], ComponentState::Up);
        assert_eq!(by_id["migrate"], ComponentState::OneshotOk);
        assert_eq!(
            status[1].provenance.as_ref().unwrap().image.as_deref(),
            Some("pulled:registry.example/api")
        );

        // a wrong verb is refused before anything runs
        assert_eq!(
            drv.stop(&inst, &plan).unwrap_err(),
            DriverError::WrongVerb {
                verb: Verb::Stop,
                found: Verb::Up
            }
        );

        // stop by hand (outside the stack): intent still says running → stopped-problem
        drv.actual["api"].running = false;
        drv.actual["api"].exit_code = Some(137);
        let status = drv.status(&inst).unwrap();
        assert_eq!(
            status.iter().find(|s| s.id == "api").unwrap().state,
            ComponentState::StoppedProblem
        );

        // stop through the stack: desired follows → stopped-idle
        let state = drv.instance_state(&inst);
        let stop = crate::planner::plan(
            &st,
            &state,
            &Change::Stop {
                keep_groups: vec![],
            },
        )
        .unwrap();
        drv.stop(&inst, &stop).unwrap();
        intent.record_plan(&stop, "2026-01-01T00:02:00Z");
        let inst = resolve("app", &st, 0, Some(&intent)).unwrap();
        let status = drv.status(&inst).unwrap();
        assert!(
            status
                .iter()
                .filter(|s| s.id != "migrate")
                .all(|s| s.state == ComponentState::StoppedIdle),
            "{status:?}"
        );
        assert_eq!(
            status.iter().find(|s| s.id == "migrate").unwrap().state,
            ComponentState::OneshotOk
        );

        // down removes everything
        let state = drv.instance_state(&inst);
        let down = crate::planner::plan(
            &st,
            &state,
            &Change::Down {
                keep_groups: vec![],
            },
        )
        .unwrap();
        drv.down(&inst, &down).unwrap();
        assert!(drv
            .status(&inst)
            .unwrap()
            .iter()
            .all(|s| s.state == ComponentState::NotDeployed));
    }

    #[test]
    fn fake_driver_stops_at_the_first_failure() {
        let st = stack();
        let inst = resolve("app", &st, 0, None).unwrap();
        let mut drv = FakeDriver {
            fail_on: Some(("api".into(), "pull: unauthorized".into())),
            ..Default::default()
        };
        let plan = crate::planner::plan(
            &st,
            &drv.instance_state(&inst),
            &Change::Up {
                selection: Selection::Default,
                modes: IndexMap::new(),
                channel: None,
            },
        )
        .unwrap();
        let report = drv.up(&inst, &plan).unwrap();
        assert!(!report.ok());
        assert_eq!(
            report.done.len(),
            2,
            "db pulled and started, then api failed"
        );
        let failed = report.failed.unwrap();
        assert_eq!(failed.step.component(), "api");
        assert_eq!(failed.reason, "pull: unauthorized");
        assert!(matches!(failed.step, Step::Pull { .. }));
        assert_eq!(*drv.calls.borrow(), vec!["pull db", "start db"]);
        assert!(
            !drv.actual.contains_key("web"),
            "nothing after the failure ran"
        );
    }

    #[test]
    fn fake_driver_plan_uses_the_resolved_instance_only() {
        let st = stack();
        let inst = resolve("app", &st, 0, None).unwrap();
        let drv = FakeDriver::default();
        let plan = drv
            .plan(
                &inst,
                &drv.instance_state(&inst),
                &Change::Up {
                    selection: Selection::Components(vec!["db".into()]),
                    modes: IndexMap::new(),
                    channel: None,
                },
            )
            .unwrap();
        assert_eq!(plan.components(), ["db"]);
        assert_eq!(plan.verb, Verb::Up);
    }

    #[test]
    fn apply_report_and_status_serialize() {
        let r = ApplyReport {
            done: vec![Step::Start {
                component: "db".into(),
            }],
            failed: None,
        };
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["done"][0]["step"], "start");
        assert!(json.get("failed").is_none());
        let s = ComponentStatus {
            id: "db".into(),
            state: ComponentState::StoppedProblem,
            uptime: None,
            restarts: Some(0),
            exit_code: Some(137),
            provenance: None,
            stale: vec![],
        };
        let json = serde_json::to_value(&s).unwrap();
        assert_eq!(json["state"], "stopped-problem");
        assert_eq!(json["exitCode"], 137);
        assert!(json.get("stale").is_none());
    }
}
