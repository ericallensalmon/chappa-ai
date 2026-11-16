//! From "what the user asked" to "the exact steps", before anything runs.
//!
//! Every mutating verb has a plan form. The planner takes the definition,
//! what the instance looks like right now (per component: mode, ref,
//! commit, dirty hash, image, desired state, whether a container exists and
//! runs) and the requested change, and produces an ordered [`Plan`] of typed
//! steps. The confirm dialog shows [`Plan::summary`]; apply then executes
//! exactly those steps.
//!
//! A plan carries a fingerprint of the state it was computed from. If the
//! stack changed between plan and apply (another agent switched a mode, a
//! container died), [`Plan::check_against`] fails and the caller re-plans
//! instead of applying stale steps. Same rule as a saved terraform plan.
//!
//! Ordering rules:
//! - starts and recreates follow `depends_on` (dependencies first);
//! - stops run the stack's `stop.order` first, then everything else in
//!   reverse dependency order, each with its grace period;
//! - a component whose image changes drags along every component that
//!   bakes that image in (`{image:<it>}` in its templates): those are
//!   recreated after it. Plain `depends_on` dependents are not touched;
//! - `normallyOff` components are outside the default selection, but a
//!   selected component's dependencies are always included.
//!
//! Script components ([`crate::script`]) take part in the same ordering,
//! but their steps are one [`Step::Script`] per verb and are planned from
//! the intent record alone: the runner cannot see inside a script, so what
//! the scripts last reported never changes a plan (or its fingerprint).
//! `up` always runs the `up` script; a stack-wide verb skips a script
//! component that has no script for it, with a note.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fmt;

use crate::definition::{CheckoutKind, ComponentKind, DriverKind, Mode, StackDef};
use crate::intent::{instance_id, Desired};
use crate::provenance::content_key;
use crate::script::{self, ScriptVerb};

/// One component as the instance has it right now.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComponentCurrent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<Mode>,
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dirty: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(default)]
    pub desired: Desired,
    /// A container (or resource) exists for it.
    #[serde(default)]
    pub exists: bool,
    #[serde(default)]
    pub running: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// The host-gateway address baked in at create time (`extra_hosts`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct InstanceState {
    pub stack: String,
    pub slot: u32,
    #[serde(default)]
    pub components: IndexMap<String, ComponentCurrent>,
}

impl InstanceState {
    pub fn new(stack: &str, slot: u32) -> Self {
        Self {
            stack: stack.to_string(),
            slot,
            components: IndexMap::new(),
        }
    }

    /// A component the state has no entry for is absent.
    pub fn component(&self, name: &str) -> ComponentCurrent {
        self.components.get(name).cloned().unwrap_or_default()
    }

    /// Order-independent hash of the whole state. Two states that differ
    /// only in map order fingerprint the same.
    pub fn fingerprint(&self) -> String {
        let sorted: BTreeMap<&String, &ComponentCurrent> = self.components.iter().collect();
        let doc = serde_json::json!({
            "stack": self.stack,
            "slot": self.slot,
            "components": sorted,
        });
        let text = serde_json::to_string(&doc).expect("state serializes");
        blake3::hash(text.as_bytes()).to_hex()[..16].to_string()
    }
}

/// Which components `up` brings up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind", content = "value")]
pub enum Selection {
    /// Everything not `normallyOff`.
    Default,
    /// A named selection from the definition.
    Named(String),
    /// An explicit list.
    Components(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "verb")]
pub enum Change {
    Up {
        selection: Selection,
        /// Per-component mode choice for this up.
        #[serde(default)]
        modes: IndexMap<String, Mode>,
        /// The prebuilt channel or tag this up resolves `{channel}` with.
        /// Recorded in the intent so later verbs (rebuild, start) reuse it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        channel: Option<String>,
    },
    SwitchMode {
        component: String,
        mode: Mode,
        #[serde(default, rename = "ref")]
        git_ref: Option<String>,
    },
    ChangeRef {
        component: String,
        #[serde(rename = "ref")]
        git_ref: String,
    },
    Rebuild {
        component: String,
    },
    Stop {
        /// Groups left running (`--keep core`).
        #[serde(default)]
        keep_groups: Vec<String>,
    },
    /// Stop only these components (each with its grace period, in the
    /// stack's stop order), leaving the rest as they are.
    StopComponents {
        components: Vec<String>,
    },
    Down {
        #[serde(default)]
        keep_groups: Vec<String>,
    },
    /// Re-acquire the image of every existing component that runs from one
    /// (prebuilt pulled again, source built again) and recreate it. Dev
    /// and process components have no image and are left alone.
    RebuildAll,
    /// After a reboot: start what exists, recreating only what baked in a
    /// gateway address that has since changed.
    Start {
        /// Engine name to its current host-gateway address.
        #[serde(default)]
        gateways: IndexMap<String, String>,
    },
    /// Switch one component to `dev` mode so its watch command runs in a
    /// pane: the same steps as a mode switch, under its own verb so the
    /// apply layer knows to hand back the watch run.
    Watch {
        component: String,
        #[serde(default, rename = "ref")]
        git_ref: Option<String>,
    },
    /// Bind a work item to the instance: every component in `bind` builds
    /// (source) or runs (dev) from the item's branch, in the item's
    /// worktree; every one in `release` goes back to the given mode (it
    /// was bound to a previous item's worktree, which the new one does not
    /// have). The rest keep their mode. A component that exists is
    /// recreated; one that does not only has its mode and ref recorded, so
    /// its next start builds from the item.
    Bind {
        /// The item's folder name, for the notes.
        item: String,
        #[serde(default)]
        bind: IndexMap<String, BindTarget>,
        #[serde(default)]
        release: IndexMap<String, Mode>,
    },
    /// Bring components back to what a saved state recorded: each in its
    /// mode, from its pinned image (a prebuilt digest, a source content
    /// key) or its worktree (dev), wanted running or not as it was. Only
    /// the components named are touched; one whose bits already match is
    /// left alone unless `fresh` asks for a clean container.
    Resume {
        /// The item resumed, for the notes.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item: Option<String>,
        #[serde(default)]
        targets: IndexMap<String, ResumeTarget>,
        /// Script components brought back by running their `up` script
        /// again: a saved state records nothing more of them.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        scripts: Vec<String>,
    },
    /// Run one script of one script component.
    Script {
        component: String,
        #[serde(rename = "script")]
        verb: ScriptVerb,
    },
}

/// What one component is resumed to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumeTarget {
    pub mode: Mode,
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    /// The image (or, for dev, the commit) to run. `None`: build or run
    /// from whatever the repo has now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin: Option<ImagePin>,
    pub desired: Desired,
    /// Recreate even when the image is the same: the data comes from the
    /// image (a recipe), so only a new container starts clean.
    #[serde(default)]
    pub fresh: bool,
}

/// An exact image a component must run, instead of whatever its tag or
/// worktree resolves to now.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImagePin {
    /// prebuilt: the digest (`sha256:…`); source: the content key
    /// (`api:1a2b3c4d`); dev: none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// prebuilt: the tag or build number it was pulled as.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_id: Option<String>,
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dirty: Option<String>,
}

/// What one component is bound to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BindTarget {
    pub mode: Mode,
    #[serde(rename = "ref")]
    pub git_ref: String,
    /// Build (or restart) again even when mode and ref are unchanged: the
    /// branch moved since the instance was built from it.
    #[serde(default)]
    pub rebuild: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verb {
    Up,
    SwitchMode,
    ChangeRef,
    Rebuild,
    Stop,
    Down,
    Start,
    Watch,
    Bind,
    Resume,
    Restart,
}

impl Change {
    pub fn verb(&self) -> Verb {
        match self {
            Change::Up { .. } => Verb::Up,
            Change::SwitchMode { .. } => Verb::SwitchMode,
            Change::ChangeRef { .. } => Verb::ChangeRef,
            Change::Rebuild { .. } => Verb::Rebuild,
            Change::Stop { .. } | Change::StopComponents { .. } => Verb::Stop,
            Change::Down { .. } => Verb::Down,
            Change::RebuildAll => Verb::Rebuild,
            Change::Start { .. } => Verb::Start,
            Change::Watch { .. } => Verb::Watch,
            Change::Bind { .. } => Verb::Bind,
            Change::Resume { .. } => Verb::Resume,
            Change::Script { verb, .. } => match verb {
                ScriptVerb::Start => Verb::Start,
                ScriptVerb::Stop => Verb::Stop,
                ScriptVerb::Restart => Verb::Restart,
                ScriptVerb::Rebuild => Verb::Rebuild,
                ScriptVerb::Down => Verb::Down,
                ScriptVerb::Up | ScriptVerb::Status | ScriptVerb::Plan => Verb::Up,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum RecreateReason {
    ModeChanged {
        from: Option<Mode>,
        to: Mode,
    },
    RefChanged {
        from: Option<String>,
        to: String,
    },
    /// This component bakes in the image of `of`, which changed.
    PinnedImageChanged {
        of: String,
    },
    GatewayChanged {
        from: String,
        to: String,
    },
    Rebuilt,
    /// Brought back to a saved state.
    Resumed,
}

impl fmt::Display for RecreateReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecreateReason::ModeChanged { from: Some(a), to } => write!(f, "mode {a} → {to}"),
            RecreateReason::ModeChanged { from: None, to } => write!(f, "mode {to}"),
            RecreateReason::RefChanged { from: Some(a), to } => write!(f, "ref {a} → {to}"),
            RecreateReason::RefChanged { from: None, to } => write!(f, "ref {to}"),
            RecreateReason::PinnedImageChanged { of } => write!(f, "pins image of {of}"),
            RecreateReason::GatewayChanged { from, to } => write!(f, "gateway {from} → {to}"),
            RecreateReason::Rebuilt => f.write_str("rebuilt"),
            RecreateReason::Resumed => f.write_str("saved state"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "step")]
pub enum Step {
    /// Make the repo checkout the build or dev run needs.
    Checkout {
        component: String,
        #[serde(rename = "ref")]
        git_ref: String,
        kind: CheckoutKind,
    },
    /// Build the source image. `key` is the content key when the commit is
    /// already known; otherwise the builder computes it after checkout.
    Build {
        component: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
    },
    /// Pull the prebuilt image. `tag` is the definition's tag template,
    /// resolved by the driver with the channel in force.
    Pull {
        component: String,
        image: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tag: Option<String>,
    },
    Recreate {
        component: String,
        reason: RecreateReason,
    },
    Start {
        component: String,
    },
    Stop {
        component: String,
        grace_seconds: u32,
    },
    Remove {
        component: String,
    },
    /// Run a script component's script for `verb`.
    Script {
        component: String,
        #[serde(rename = "script")]
        verb: ScriptVerb,
    },
}

impl Step {
    pub fn component(&self) -> &str {
        match self {
            Step::Checkout { component, .. }
            | Step::Build { component, .. }
            | Step::Pull { component, .. }
            | Step::Recreate { component, .. }
            | Step::Start { component }
            | Step::Stop { component, .. }
            | Step::Remove { component }
            | Step::Script { component, .. } => component,
        }
    }

    /// The step kind as a word: `Build`, `Recreate` …
    pub fn verb_label(&self) -> &'static str {
        match self {
            Step::Checkout { .. } => "Checkout",
            Step::Build { .. } => "Build",
            Step::Pull { .. } => "Pull",
            Step::Recreate { .. } => "Recreate",
            Step::Start { .. } => "Start",
            Step::Stop { .. } => "Stop",
            Step::Remove { .. } => "Remove",
            Step::Script { .. } => "Run",
        }
    }

    /// What the summary shows in parentheses after the component.
    pub fn detail(&self) -> Option<String> {
        match self {
            Step::Checkout { git_ref, kind, .. } => Some(match kind {
                CheckoutKind::Worktree => git_ref.clone(),
                CheckoutKind::Export => format!("{git_ref}, export"),
            }),
            Step::Build { key: Some(k), .. } => Some(k.clone()),
            Step::Build { key: None, .. } => None,
            Step::Pull { image, tag, .. } => Some(match tag {
                Some(t) => format!("{image}:{t}"),
                None => image.clone(),
            }),
            Step::Recreate { reason, .. } => Some(reason.to_string()),
            Step::Start { .. } | Step::Remove { .. } => None,
            Step::Stop { grace_seconds, .. } => Some(format!("{grace_seconds}s")),
            Step::Script { verb, .. } => Some(format!("{verb} script")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub instance: String,
    pub verb: Verb,
    pub steps: Vec<Step>,
    /// Things the plan decided not to do, and why; shown under the steps.
    #[serde(default)]
    pub notes: Vec<String>,
    /// [`InstanceState::fingerprint`] of the state planned against.
    pub fingerprint: String,
    /// Desired state after apply, for every component the plan touches.
    #[serde(default)]
    pub desired: IndexMap<String, Desired>,
    /// Mode after apply, for components whose mode the plan decides.
    #[serde(default)]
    pub modes: IndexMap<String, Mode>,
    /// Ref after apply, for components the plan checks out.
    #[serde(default)]
    pub refs: IndexMap<String, String>,
    /// The prebuilt channel in force for this plan's pulls, when `up` chose one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    /// Exact images per component (a resumed state's), used instead of
    /// resolving the tag or reading the worktree.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub pins: IndexMap<String, ImagePin>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    #[error("unknown component `{0}`")]
    UnknownComponent(String),
    #[error("unknown selection `{0}`")]
    UnknownSelection(String),
    #[error("component `{component}` has no `{mode}` mode")]
    UnknownMode { component: String, mode: Mode },
    #[error("component `{component}` declares no modes")]
    NoModes { component: String },
    #[error("component `{component}` is in `{mode}` mode, which has no ref to change")]
    NotRepoMode { component: String, mode: Mode },
    #[error("depends_on cycle: {}", .0.join(" -> "))]
    Cycle(Vec<String>),
    #[error("the stack changed since this plan was made (planned against {expected}, now {actual}); plan again")]
    StateChanged { expected: String, actual: String },
    #[error("component `{component}` has no `{verb}` script")]
    NoScript { component: String, verb: ScriptVerb },
}

impl Plan {
    pub fn empty(instance: &str, verb: Verb, fingerprint: &str) -> Self {
        Self {
            instance: instance.to_string(),
            verb,
            steps: Vec::new(),
            notes: Vec::new(),
            fingerprint: fingerprint.to_string(),
            desired: IndexMap::new(),
            modes: IndexMap::new(),
            refs: IndexMap::new(),
            channel: None,
            pins: IndexMap::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// Distinct components touched, in step order.
    pub fn components(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        self.steps
            .iter()
            .map(|s| s.component().to_string())
            .filter(|c| seen.insert(c.clone()))
            .collect()
    }

    /// Refuse to apply against a state other than the one planned.
    pub fn check_against(&self, state: &InstanceState) -> Result<(), PlanError> {
        let actual = state.fingerprint();
        if actual == self.fingerprint {
            Ok(())
        } else {
            Err(PlanError::StateChanged {
                expected: self.fingerprint.clone(),
                actual,
            })
        }
    }

    /// One line per step kind, in first-occurrence order, then the notes:
    ///
    /// ```text
    /// Checkout: api (feature/x)
    /// Build: api (api:01234567)
    /// Recreate: api (mode prebuilt → source), infra (pins image of api)
    /// ```
    pub fn summary(&self) -> String {
        let mut lines: Vec<String> = Vec::new();
        if self.steps.is_empty() {
            lines.push("Nothing to do".to_string());
        } else {
            let mut groups: IndexMap<&'static str, Vec<String>> = IndexMap::new();
            for step in &self.steps {
                let item = match step.detail() {
                    Some(d) => format!("{} ({d})", step.component()),
                    None => step.component().to_string(),
                };
                groups.entry(step.verb_label()).or_default().push(item);
            }
            for (verb, items) in groups {
                lines.push(format!("{verb}: {}", items.join(", ")));
            }
        }
        for note in &self.notes {
            lines.push(format!("Note: {note}"));
        }
        lines.join("\n")
    }

    fn push(&mut self, step: Step) {
        match &step {
            Step::Start { component } | Step::Recreate { component, .. } => {
                self.desired.insert(component.clone(), Desired::Running);
            }
            Step::Stop { component, .. } => {
                self.desired.insert(component.clone(), Desired::Stopped);
            }
            Step::Remove { component } => {
                self.desired.insert(component.clone(), Desired::Absent);
            }
            Step::Checkout {
                component, git_ref, ..
            } => {
                self.refs.insert(component.clone(), git_ref.clone());
            }
            Step::Script { component, verb } => {
                if let Some(d) = verb.desired_after() {
                    self.desired.insert(component.clone(), d);
                }
            }
            Step::Build { .. } | Step::Pull { .. } => {}
        }
        self.steps.push(step);
    }

    /// Run `verb` of a script component when it has that script; otherwise
    /// say that a stack-wide verb leaves it as it is.
    fn script_or_note(&mut self, stack: &StackDef, component: &str, verb: ScriptVerb) {
        if script::has(&stack.components[component], verb) {
            self.push(Step::Script {
                component: component.to_string(),
                verb,
            });
        } else {
            self.notes.push(format!(
                "{component} has no {verb} script, so {verb} leaves it as it is"
            ));
        }
    }

    fn has_step_for(&self, component: &str, pred: impl Fn(&Step) -> bool) -> bool {
        self.steps
            .iter()
            .any(|s| s.component() == component && pred(s))
    }
}

/// Compute the plan for `change` against `state`.
pub fn plan(stack: &StackDef, state: &InstanceState, change: &Change) -> Result<Plan, PlanError> {
    let order = stack.dependency_order().map_err(PlanError::Cycle)?;
    let mut plan = Plan::empty(
        &instance_id(&state.stack, state.slot),
        change.verb(),
        &state.fingerprint(),
    );
    match change {
        Change::Up {
            selection,
            modes,
            channel,
        } => {
            plan.channel = channel.clone();
            plan_up(stack, state, &order, selection, modes, &mut plan)?
        }
        Change::SwitchMode {
            component,
            mode,
            git_ref,
        } => plan_switch(stack, state, &order, component, *mode, git_ref, &mut plan)?,
        Change::Watch { component, git_ref } => plan_switch(
            stack,
            state,
            &order,
            component,
            Mode::Dev,
            git_ref,
            &mut plan,
        )?,
        Change::ChangeRef { component, git_ref } => {
            require(stack, component)?;
            let cur = state.component(component);
            let mode = cur
                .mode
                .or_else(|| stack.components[component].default_mode())
                .ok_or_else(|| PlanError::NoModes {
                    component: component.clone(),
                })?;
            if !mode.uses_repo() {
                return Err(PlanError::NotRepoMode {
                    component: component.clone(),
                    mode,
                });
            }
            // The commit at the new ref is unknown until checkout: no content key yet.
            let fresh = ComponentCurrent {
                commit: None,
                dirty: None,
                ..cur.clone()
            };
            acquire(
                stack,
                component,
                mode,
                Some(git_ref.clone()),
                &fresh,
                &mut plan,
            );
            if cur.exists {
                plan.push(Step::Recreate {
                    component: component.clone(),
                    reason: RecreateReason::RefChanged {
                        from: cur.git_ref.clone(),
                        to: git_ref.clone(),
                    },
                });
            } else {
                plan.push(Step::Start {
                    component: component.clone(),
                });
            }
            recreate_image_dependents(
                stack,
                state,
                &order,
                std::slice::from_ref(component),
                &mut plan,
            );
        }
        Change::Script { component, verb } => {
            require(stack, component)?;
            if !ScriptVerb::MUTATING.contains(verb)
                || !script::has(&stack.components[component], *verb)
            {
                return Err(PlanError::NoScript {
                    component: component.clone(),
                    verb: *verb,
                });
            }
            plan.push(Step::Script {
                component: component.clone(),
                verb: *verb,
            });
        }
        Change::Rebuild { component } if is_script(stack, component) => {
            if !script::has(&stack.components[component], ScriptVerb::Rebuild) {
                return Err(PlanError::NoScript {
                    component: component.clone(),
                    verb: ScriptVerb::Rebuild,
                });
            }
            plan.push(Step::Script {
                component: component.clone(),
                verb: ScriptVerb::Rebuild,
            });
        }
        Change::Rebuild { component } => {
            require(stack, component)?;
            let cur = state.component(component);
            let mode = cur
                .mode
                .or_else(|| stack.components[component].default_mode())
                .ok_or_else(|| PlanError::NoModes {
                    component: component.clone(),
                })?;
            // A rebuild always re-acquires: forget the image so `acquire` emits it.
            let forget = ComponentCurrent {
                image: None,
                mode: None,
                ..cur.clone()
            };
            acquire(
                stack,
                component,
                mode,
                cur.git_ref.clone(),
                &forget,
                &mut plan,
            );
            if cur.exists {
                plan.push(Step::Recreate {
                    component: component.clone(),
                    reason: RecreateReason::Rebuilt,
                });
            } else {
                plan.push(Step::Start {
                    component: component.clone(),
                });
            }
            recreate_image_dependents(
                stack,
                state,
                &order,
                std::slice::from_ref(component),
                &mut plan,
            );
        }
        Change::Stop { keep_groups } => {
            for name in stop_order(stack, &order, |n| {
                state.component(n).exists && !keep_groups.iter().any(|g| g == stack.group_of(n))
            }) {
                let cur = state.component(&name);
                if is_script(stack, &name) {
                    if cur.desired == Desired::Running {
                        plan.script_or_note(stack, &name, ScriptVerb::Stop);
                    }
                    continue;
                }
                if cur.running {
                    plan.push(Step::Stop {
                        grace_seconds: stack.grace_seconds(&name),
                        component: name,
                    });
                } else if cur.desired == Desired::Running
                    && !(stack.components[&name].kind == ComponentKind::Oneshot
                        && cur.exit_code == Some(0))
                {
                    // Stopped outside the stack while wanted running: the user
                    // is now stopping the stack, so the intent follows and the
                    // component stops showing as a problem.
                    plan.notes
                        .push(format!("{name} was already stopped; recorded as stopped"));
                    plan.desired.insert(name, Desired::Stopped);
                }
            }
        }
        Change::StopComponents { components } => {
            for name in components {
                require(stack, name)?;
            }
            for name in stop_order(stack, &order, |n| {
                components.iter().any(|c| c == n) && state.component(n).exists
            }) {
                let cur = state.component(&name);
                if is_script(stack, &name) {
                    if cur.desired == Desired::Running {
                        plan.script_or_note(stack, &name, ScriptVerb::Stop);
                    }
                    continue;
                }
                if cur.running {
                    plan.push(Step::Stop {
                        grace_seconds: stack.grace_seconds(&name),
                        component: name,
                    });
                } else if cur.desired == Desired::Running
                    && !(stack.components[&name].kind == ComponentKind::Oneshot
                        && cur.exit_code == Some(0))
                {
                    plan.notes
                        .push(format!("{name} was already stopped; recorded as stopped"));
                    plan.desired.insert(name, Desired::Stopped);
                }
            }
        }
        Change::RebuildAll => {
            let mut rebuilt: Vec<String> = Vec::new();
            for name in &order {
                let cur = state.component(name);
                if !cur.exists {
                    continue;
                }
                if is_script(stack, name) {
                    plan.script_or_note(stack, name, ScriptVerb::Rebuild);
                    continue;
                }
                let Some(mode) = cur.mode.or_else(|| stack.components[name].default_mode()) else {
                    continue;
                };
                if mode == Mode::Dev {
                    continue;
                }
                let forget = ComponentCurrent {
                    image: None,
                    mode: None,
                    ..cur.clone()
                };
                acquire(stack, name, mode, cur.git_ref.clone(), &forget, &mut plan);
                plan.push(Step::Recreate {
                    component: name.clone(),
                    reason: RecreateReason::Rebuilt,
                });
                rebuilt.push(name.clone());
            }
            recreate_image_dependents(stack, state, &order, &rebuilt, &mut plan);
        }
        Change::Down { keep_groups } => {
            let targets = stop_order(stack, &order, |n| {
                state.component(n).exists && !keep_groups.iter().any(|g| g == stack.group_of(n))
            });
            for name in &targets {
                if state.component(name).running && !is_script(stack, name) {
                    plan.push(Step::Stop {
                        component: name.clone(),
                        grace_seconds: stack.grace_seconds(name),
                    });
                }
            }
            for name in targets {
                if is_script(stack, &name) {
                    plan.script_or_note(stack, &name, ScriptVerb::Down);
                } else {
                    plan.push(Step::Remove { component: name });
                }
            }
        }
        Change::Bind {
            item,
            bind,
            release,
        } => plan_bind(stack, state, &order, item, bind, release, &mut plan)?,
        Change::Resume {
            targets, scripts, ..
        } => plan_resume(stack, state, &order, targets, scripts, &mut plan)?,
        Change::Start { gateways } => {
            for name in &order {
                let cur = state.component(name);
                if cur.desired != Desired::Running {
                    continue;
                }
                if is_script(stack, name) {
                    plan.script_or_note(stack, name, ScriptVerb::Start);
                    continue;
                }
                if !cur.exists {
                    plan.notes
                        .push(format!("{name} has nothing to start; bring it up first"));
                    continue;
                }
                let def = &stack.components[name];
                let engine = stack.engine_name_for(name).unwrap_or_default();
                if let (Some(now), Some(baked)) = (gateways.get(&engine), &cur.gateway) {
                    if now != baked {
                        plan.push(Step::Recreate {
                            component: name.clone(),
                            reason: RecreateReason::GatewayChanged {
                                from: baked.clone(),
                                to: now.clone(),
                            },
                        });
                        continue;
                    }
                }
                if cur.running {
                    continue;
                }
                if def.kind == ComponentKind::Oneshot && cur.exit_code == Some(0) {
                    plan.notes.push(format!("{name} already ran to completion"));
                    continue;
                }
                plan.push(Step::Start {
                    component: name.clone(),
                });
            }
        }
    }
    Ok(plan)
}

/// A mode switch on one component: acquire the new mode's bits, recreate
/// (or start) it, then recreate whatever pins its image.
fn plan_switch(
    stack: &StackDef,
    state: &InstanceState,
    order: &[String],
    component: &str,
    mode: Mode,
    git_ref: &Option<String>,
    plan: &mut Plan,
) -> Result<(), PlanError> {
    require(stack, component)?;
    if !stack.components[component].has_mode(mode) {
        return Err(PlanError::UnknownMode {
            component: component.to_string(),
            mode,
        });
    }
    let cur = state.component(component);
    acquire(
        stack,
        component,
        mode,
        git_ref.clone().or(cur.git_ref.clone()),
        &cur,
        plan,
    );
    if cur.exists {
        plan.push(Step::Recreate {
            component: component.to_string(),
            reason: RecreateReason::ModeChanged {
                from: cur.mode,
                to: mode,
            },
        });
    } else {
        plan.push(Step::Start {
            component: component.to_string(),
        });
    }
    recreate_image_dependents(stack, state, order, &[component.to_string()], plan);
    Ok(())
}

/// [`Change::Bind`]: components in definition order, then whatever pins
/// the image of one that changed.
fn plan_bind(
    stack: &StackDef,
    state: &InstanceState,
    order: &[String],
    item: &str,
    bind: &IndexMap<String, BindTarget>,
    release: &IndexMap<String, Mode>,
    plan: &mut Plan,
) -> Result<(), PlanError> {
    for (name, t) in bind {
        require(stack, name)?;
        if !stack.components[name].has_mode(t.mode) {
            return Err(PlanError::UnknownMode {
                component: name.clone(),
                mode: t.mode,
            });
        }
        if !t.mode.uses_repo() {
            return Err(PlanError::NotRepoMode {
                component: name.clone(),
                mode: t.mode,
            });
        }
    }
    for (name, mode) in release {
        require(stack, name)?;
        if !stack.components[name].has_mode(*mode) {
            return Err(PlanError::UnknownMode {
                component: name.clone(),
                mode: *mode,
            });
        }
    }
    let mut changed: Vec<String> = Vec::new();
    for name in order {
        let cur = state.component(name);
        if let Some(t) = bind.get(name) {
            let verb = if t.mode == Mode::Dev {
                "runs"
            } else {
                "builds"
            };
            if !cur.exists {
                plan.modes.insert(name.clone(), t.mode);
                plan.refs.insert(name.clone(), t.git_ref.clone());
                plan.notes.push(format!(
                    "{name} is not deployed; it {verb} from {} of {item} when it starts",
                    t.git_ref
                ));
                continue;
            }
            let same_mode = cur.mode == Some(t.mode);
            let same_ref = cur.git_ref.as_deref() == Some(t.git_ref.as_str());
            if same_mode && same_ref && !t.rebuild {
                plan.modes.insert(name.clone(), t.mode);
                plan.notes
                    .push(format!("{name} already {verb} from {}", t.git_ref));
                continue;
            }
            // The commit at the bound ref is the worktree's, known only once
            // the build looks; a rebuild forgets the image so it is made again.
            let fresh = ComponentCurrent {
                commit: None,
                dirty: None,
                image: if t.rebuild { None } else { cur.image.clone() },
                git_ref: if t.rebuild && same_ref {
                    None
                } else {
                    cur.git_ref.clone()
                },
                ..cur.clone()
            };
            acquire(stack, name, t.mode, Some(t.git_ref.clone()), &fresh, plan);
            let reason = if !same_mode {
                RecreateReason::ModeChanged {
                    from: cur.mode,
                    to: t.mode,
                }
            } else if !same_ref {
                RecreateReason::RefChanged {
                    from: cur.git_ref.clone(),
                    to: t.git_ref.clone(),
                }
            } else {
                RecreateReason::Rebuilt
            };
            plan.push(Step::Recreate {
                component: name.clone(),
                reason,
            });
            changed.push(name.clone());
        } else if let Some(mode) = release.get(name) {
            if !cur.exists {
                plan.modes.insert(name.clone(), *mode);
                continue;
            }
            if cur.mode == Some(*mode) {
                continue;
            }
            acquire(stack, name, *mode, None, &cur, plan);
            plan.push(Step::Recreate {
                component: name.clone(),
                reason: RecreateReason::ModeChanged {
                    from: cur.mode,
                    to: *mode,
                },
            });
            changed.push(name.clone());
        }
    }
    recreate_image_dependents(stack, state, order, &changed, plan);
    Ok(())
}

/// [`Change::Resume`]: the named components in dependency order. One the
/// state wants running gets its pinned bits when they differ from what
/// runs now (a pull by digest, the content key reused or rebuilt, nothing
/// for dev) and is recreated or started; one recorded stopped or absent is
/// stopped if it runs, never removed (its data stays). Whatever pins a
/// changed image is recreated after.
fn plan_resume(
    stack: &StackDef,
    state: &InstanceState,
    order: &[String],
    targets: &IndexMap<String, ResumeTarget>,
    scripts: &[String],
    plan: &mut Plan,
) -> Result<(), PlanError> {
    for name in scripts {
        require(stack, name)?;
        if !is_script(stack, name) {
            return Err(PlanError::NoScript {
                component: name.clone(),
                verb: ScriptVerb::Up,
            });
        }
    }
    for (name, t) in targets {
        require(stack, name)?;
        if !stack.components[name].has_mode(t.mode) {
            return Err(PlanError::UnknownMode {
                component: name.clone(),
                mode: t.mode,
            });
        }
    }
    let mut changed: Vec<String> = Vec::new();
    for name in order {
        if scripts.contains(name) {
            plan.push(Step::Script {
                component: name.clone(),
                verb: ScriptVerb::Up,
            });
            continue;
        }
        let Some(t) = targets.get(name) else {
            continue;
        };
        let cur = state.component(name);
        let def = &stack.components[name];
        plan.modes.insert(name.clone(), t.mode);
        if t.desired != Desired::Running {
            if cur.running {
                plan.push(Step::Stop {
                    component: name.clone(),
                    grace_seconds: stack.grace_seconds(name),
                });
            } else if cur.desired == Desired::Running {
                plan.desired.insert(name.clone(), Desired::Stopped);
            }
            continue;
        }
        if let Some(r) = &t.git_ref {
            plan.refs.insert(name.clone(), r.clone());
        }
        if let Some(p) = &t.pin {
            plan.pins.insert(name.clone(), p.clone());
        }
        let pin = t.pin.clone().unwrap_or_default();
        let same_bits = cur.mode == Some(t.mode)
            && match t.mode {
                Mode::Dev => pin.commit.is_some() && cur.commit == pin.commit,
                _ => pin.image.is_some() && cur.image == pin.image,
            };
        if same_bits && !t.fresh {
            if cur.running {
                plan.notes.push(format!("{name} already runs as saved"));
                continue;
            }
            if def.kind == ComponentKind::Oneshot && cur.exit_code == Some(0) {
                plan.notes.push(format!("{name} already ran to completion"));
                continue;
            }
        }
        if !same_bits {
            match t.mode {
                Mode::Prebuilt => {
                    let m = def.mode(Mode::Prebuilt);
                    // Shown in the summary: the build it was and the digest
                    // it is pulled by (apply uses the pin, not this).
                    let tag = match (&pin.build_id, &pin.image) {
                        (Some(b), Some(d)) => Some(format!("{b}@{}", short_digest(d))),
                        (None, Some(d)) => Some(format!("@{}", short_digest(d))),
                        _ => m.and_then(|m| m.tag.clone()),
                    };
                    plan.push(Step::Pull {
                        component: name.clone(),
                        image: m.and_then(|m| m.image.clone()).unwrap_or_default(),
                        tag,
                    });
                }
                Mode::Source => plan.push(Step::Build {
                    component: name.clone(),
                    key: pin.image.clone(),
                }),
                Mode::Dev => {}
            }
        }
        if cur.exists && (!same_bits || t.fresh) {
            plan.push(Step::Recreate {
                component: name.clone(),
                reason: RecreateReason::Resumed,
            });
            if !same_bits {
                changed.push(name.clone());
            }
        } else {
            plan.push(Step::Start {
                component: name.clone(),
            });
        }
    }
    recreate_image_dependents(stack, state, order, &changed, plan);
    Ok(())
}

/// `sha256:` and the first 12 hex digits, as shown.
pub fn short_digest(digest: &str) -> String {
    let (algo, hex) = digest.split_once(':').unwrap_or(("", digest));
    let hex: String = hex.chars().take(12).collect();
    if algo.is_empty() {
        hex
    } else {
        format!("{algo}:{hex}")
    }
}

fn is_script(stack: &StackDef, component: &str) -> bool {
    stack
        .components
        .get(component)
        .is_some_and(|c| c.driver == DriverKind::Script)
}

fn require(stack: &StackDef, component: &str) -> Result<(), PlanError> {
    if stack.components.contains_key(component) {
        Ok(())
    } else {
        Err(PlanError::UnknownComponent(component.to_string()))
    }
}

/// Resolve a selection to component names (definition order), including
/// every dependency of a selected component.
pub fn resolve_selection(
    stack: &StackDef,
    selection: &Selection,
) -> Result<Vec<String>, PlanError> {
    let picked: Vec<String> = match selection {
        Selection::Default => stack
            .components
            .iter()
            .filter(|(_, c)| !c.normally_off)
            .map(|(n, _)| n.clone())
            .collect(),
        Selection::Named(name) => stack
            .selections
            .get(name)
            .cloned()
            .ok_or_else(|| PlanError::UnknownSelection(name.clone()))?,
        Selection::Components(list) => list.clone(),
    };
    let mut set: HashSet<String> = HashSet::new();
    let mut stackv: Vec<String> = Vec::new();
    for name in picked {
        require(stack, &name)?;
        stackv.push(name);
    }
    while let Some(name) = stackv.pop() {
        if !set.insert(name.clone()) {
            continue;
        }
        for dep in &stack.components[&name].depends_on {
            if stack.components.contains_key(dep) {
                stackv.push(dep.clone());
            }
        }
    }
    Ok(stack
        .components
        .keys()
        .filter(|k| set.contains(*k))
        .cloned()
        .collect())
}

fn plan_up(
    stack: &StackDef,
    state: &InstanceState,
    order: &[String],
    selection: &Selection,
    modes: &IndexMap<String, Mode>,
    plan: &mut Plan,
) -> Result<(), PlanError> {
    let selected: HashSet<String> = resolve_selection(stack, selection)?.into_iter().collect();
    for (name, mode) in modes {
        require(stack, name)?;
        if !stack.components[name].has_mode(*mode) {
            return Err(PlanError::UnknownMode {
                component: name.clone(),
                mode: *mode,
            });
        }
    }
    let mut image_changed: Vec<String> = Vec::new();
    for name in order {
        if !selected.contains(name) {
            continue;
        }
        let def = &stack.components[name];
        if def.driver == DriverKind::Script {
            plan.push(Step::Script {
                component: name.clone(),
                verb: ScriptVerb::Up,
            });
            continue;
        }
        let cur = state.component(name);
        let mode = modes
            .get(name)
            .copied()
            .or(cur.mode)
            .or_else(|| def.default_mode());
        let mode_changed = matches!((cur.mode, mode), (Some(a), Some(b)) if a != b);
        let needs_image = !cur.exists || mode_changed || (mode.is_some() && cur.image.is_none());
        if needs_image {
            if let Some(m) = mode {
                let before = plan.steps.len();
                acquire(stack, name, m, cur.git_ref.clone(), &cur, plan);
                if plan.steps.len() > before {
                    image_changed.push(name.clone());
                }
            }
        }
        if !cur.exists {
            plan.push(Step::Start {
                component: name.clone(),
            });
        } else if mode_changed {
            plan.push(Step::Recreate {
                component: name.clone(),
                reason: RecreateReason::ModeChanged {
                    from: cur.mode,
                    to: mode.expect("mode_changed implies a mode"),
                },
            });
        } else if !cur.running {
            if def.kind == ComponentKind::Oneshot && cur.exit_code == Some(0) {
                plan.notes.push(format!("{name} already ran to completion"));
            } else {
                plan.push(Step::Start {
                    component: name.clone(),
                });
            }
        }
        if let Some(m) = mode {
            plan.modes.insert(name.clone(), m);
        }
    }
    recreate_image_dependents(stack, state, order, &image_changed, plan);
    Ok(())
}

/// Emit the steps that get `component` its bits in `mode`: checkout and
/// build for source, pull for prebuilt, checkout for dev. Skips what the
/// current state already has (same mode, image present, ref unchanged).
fn acquire(
    stack: &StackDef,
    component: &str,
    mode: Mode,
    git_ref: Option<String>,
    cur: &ComponentCurrent,
    plan: &mut Plan,
) {
    let def = &stack.components[component];
    let same_mode = cur.mode == Some(mode);
    let same_ref = git_ref.is_none() || git_ref == cur.git_ref;
    let have_image = cur.image.is_some();
    plan.modes.insert(component.to_string(), mode);
    match mode {
        Mode::Prebuilt => {
            if same_mode && have_image {
                return;
            }
            let m = def.mode(Mode::Prebuilt);
            plan.push(Step::Pull {
                component: component.to_string(),
                image: m.and_then(|m| m.image.clone()).unwrap_or_default(),
                tag: m.and_then(|m| m.tag.clone()),
            });
        }
        Mode::Source => {
            if same_mode && same_ref && have_image {
                return;
            }
            let kind = def
                .mode(Mode::Source)
                .and_then(|m| m.checkout)
                .unwrap_or_default();
            if let Some(r) = &git_ref {
                plan.push(Step::Checkout {
                    component: component.to_string(),
                    git_ref: r.clone(),
                    kind,
                });
            }
            let key = if same_ref {
                cur.commit
                    .as_deref()
                    .map(|c| content_key(component, c, cur.dirty.as_deref()))
            } else {
                None
            };
            plan.push(Step::Build {
                component: component.to_string(),
                key,
            });
        }
        Mode::Dev => {
            if same_mode && same_ref {
                return;
            }
            let kind = def
                .mode(Mode::Dev)
                .and_then(|m| m.checkout)
                .unwrap_or_default();
            if let Some(r) = &git_ref {
                plan.push(Step::Checkout {
                    component: component.to_string(),
                    git_ref: r.clone(),
                    kind,
                });
            }
        }
    }
}

/// Recreate every existing component that bakes in the image of one in
/// `changed`, in dependency order, unless the plan already handles it.
fn recreate_image_dependents(
    stack: &StackDef,
    state: &InstanceState,
    order: &[String],
    changed: &[String],
    plan: &mut Plan,
) {
    for name in order {
        for of in changed {
            if !stack.image_dependents(of).iter().any(|d| d == name) {
                continue;
            }
            if !state.component(name).exists {
                continue;
            }
            if plan.has_step_for(name, |s| {
                matches!(s, Step::Recreate { .. } | Step::Start { .. })
            }) {
                continue;
            }
            plan.push(Step::Recreate {
                component: name.clone(),
                reason: RecreateReason::PinnedImageChanged { of: of.clone() },
            });
            break;
        }
    }
}

/// `stop.order` members first (in that order), then the rest of the
/// candidates in reverse dependency order.
fn stop_order(stack: &StackDef, order: &[String], candidate: impl Fn(&str) -> bool) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(stop) = &stack.stop {
        for name in &stop.order {
            if stack.components.contains_key(name) && candidate(name) && !out.contains(name) {
                out.push(name.clone());
            }
        }
    }
    for name in order.iter().rev() {
        if candidate(name) && !out.contains(name) {
            out.push(name.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A source-built image tag with a dirty-tree hash, built here so no
    /// source line holds a component name beside a varied hex string (a
    /// secret scanner reads that as a key).
    fn dirty_tag(dirty: &str) -> String {
        format!("{}:{}-dirty-{dirty}", "api", "01234567")
    }
    use crate::definition::{ComponentDef, DriverKind, ModeDef, StacksDocument, StopDef};

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn stack() -> StackDef {
        // db <- api <- web; migrate (oneshot) and worker (normallyOff) depend on db;
        // infra (terraform) pins {image:api}. api has prebuilt + source; web is dev.
        let text = r#"
stacks:
  app:
    slots: { count: 2, portBase: 14000, portStride: 100 }
    registry: registry.example/dev
    groups:
      core: { engine: docker }
      apis: { engine: podman }
    selections:
      linux-only: [db, api]
    stop: { order: [db], graceSeconds: 20 }
    components:
      db:
        group: apis
        driver: compose
        file: s.yml
        modes: { prebuilt: { image: "registry.example/db", tag: "{channel}" } }
        stop: { graceSeconds: 30 }
      api:
        group: apis
        driver: compose
        file: s.yml
        modes:
          prebuilt: { image: "registry.example/api", tag: "{channel}" }
          source:   { repo: ../api }
        default: prebuilt
        depends_on: [db]
      web:
        group: core
        driver: process
        command: web-dev
        modes: { dev: { repo: ../web, checkout: export } }
        depends_on: [api]
      migrate:
        group: apis
        driver: compose
        file: s.yml
        kind: oneshot
        ports: 0
        modes: { prebuilt: { image: "registry.example/api", tag: "{channel}" } }
        depends_on: [db]
      worker:
        group: apis
        driver: compose
        file: s.yml
        normallyOff: true
        modes: { prebuilt: { image: "registry.example/api", tag: "{channel}" } }
        depends_on: [db]
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

    fn running(mode: Mode) -> ComponentCurrent {
        ComponentCurrent {
            mode: Some(mode),
            image: Some("sha256:img".into()),
            desired: Desired::Running,
            exists: true,
            running: true,
            ..Default::default()
        }
    }

    fn running_state() -> InstanceState {
        let mut s = InstanceState::new("app", 1);
        s.components.insert("db".into(), running(Mode::Prebuilt));
        s.components.insert("api".into(), running(Mode::Prebuilt));
        let mut web = running(Mode::Dev);
        web.image = None;
        web.git_ref = Some("main".into());
        s.components.insert("web".into(), web);
        let mut migrate = running(Mode::Prebuilt);
        migrate.running = false;
        migrate.exit_code = Some(0);
        s.components.insert("migrate".into(), migrate);
        let mut infra = running(Mode::Prebuilt);
        infra.mode = None;
        s.components.insert("infra".into(), infra);
        s
    }

    fn labels(plan: &Plan) -> Vec<String> {
        plan.steps
            .iter()
            .map(|s| format!("{} {}", s.verb_label().to_lowercase(), s.component()))
            .collect()
    }

    fn up_default() -> Change {
        Change::Up {
            selection: Selection::Default,
            modes: IndexMap::new(),
            channel: None,
        }
    }

    #[test]
    fn up_from_nothing_follows_dependency_order_and_skips_normally_off() {
        let st = stack();
        let state = InstanceState::new("app", 0);
        let p = plan(&st, &state, &up_default()).unwrap();
        assert_eq!(p.instance, "app-0");
        assert_eq!(p.verb, Verb::Up);
        assert_eq!(
            labels(&p),
            [
                "pull db",
                "start db",
                "pull api",
                "start api",
                "start web",
                "pull migrate",
                "start migrate",
                "start infra",
            ]
        );
        assert!(!p.components().contains(&"worker".to_string()));
        assert_eq!(p.desired["db"], Desired::Running);
        assert_eq!(p.modes["api"], Mode::Prebuilt);
        assert_eq!(p.modes["web"], Mode::Dev);
        assert!(!p.modes.contains_key("infra"), "infra has no modes");
        assert!(
            p.refs.is_empty(),
            "no ref known for web yet, so no checkout"
        );
        assert_eq!(p.fingerprint, state.fingerprint());
        assert_eq!(p.summary(), "Pull: db (registry.example/db:{channel}), api (registry.example/api:{channel}), migrate (registry.example/api:{channel})\nStart: db, api, web, migrate, infra");
    }

    #[test]
    fn up_on_a_running_stack_is_a_no_op_except_stopped_services() {
        let st = stack();
        let mut state = running_state();
        let p = plan(&st, &state, &up_default()).unwrap();
        assert!(p.is_empty(), "{}", p.summary());
        assert_eq!(p.notes, vec!["migrate already ran to completion"]);
        assert_eq!(
            p.summary(),
            "Nothing to do\nNote: migrate already ran to completion"
        );
        // a stopped service gets started, nothing else moves
        state.components["api"].running = false;
        let p = plan(&st, &state, &up_default()).unwrap();
        assert_eq!(labels(&p), ["start api"]);
        // a failed oneshot runs again
        state.components["migrate"].exit_code = Some(1);
        let p = plan(&st, &state, &up_default()).unwrap();
        assert_eq!(labels(&p), ["start api", "start migrate"]);
    }

    #[test]
    fn up_with_named_selection_includes_dependencies_and_rejects_unknown() {
        let st = stack();
        let state = InstanceState::new("app", 0);
        let p = plan(
            &st,
            &state,
            &Change::Up {
                selection: Selection::Named("linux-only".into()),
                modes: IndexMap::new(),
                channel: None,
            },
        )
        .unwrap();
        assert_eq!(p.components(), ["db", "api"]);
        let e = plan(
            &st,
            &state,
            &Change::Up {
                selection: Selection::Named("nope".into()),
                modes: IndexMap::new(),
                channel: None,
            },
        )
        .unwrap_err();
        assert_eq!(e, PlanError::UnknownSelection("nope".into()));
        // explicit list pulls in deps even when normallyOff is irrelevant: worker needs db
        let p = plan(
            &st,
            &state,
            &Change::Up {
                selection: Selection::Components(vec!["worker".into()]),
                modes: IndexMap::new(),
                channel: None,
            },
        )
        .unwrap();
        assert_eq!(p.components(), ["db", "worker"]);
        // web pulls api pulls db
        let p = plan(
            &st,
            &state,
            &Change::Up {
                selection: Selection::Components(vec!["web".into()]),
                modes: IndexMap::new(),
                channel: None,
            },
        )
        .unwrap();
        assert_eq!(p.components(), ["db", "api", "web"]);
        let e = plan(
            &st,
            &state,
            &Change::Up {
                selection: Selection::Components(vec!["cache".into()]),
                modes: IndexMap::new(),
                channel: None,
            },
        )
        .unwrap_err();
        assert_eq!(e, PlanError::UnknownComponent("cache".into()));
    }

    #[test]
    fn up_with_mode_override_on_running_component_recreates_and_drags_pinned_dependents() {
        let st = stack();
        let mut state = running_state();
        state.components["api"].git_ref = Some("main".into());
        state.components["api"].commit = Some(SHA.into());
        let mut modes = IndexMap::new();
        modes.insert("api".to_string(), Mode::Source);
        let p = plan(
            &st,
            &state,
            &Change::Up {
                selection: Selection::Default,
                modes,
                channel: None,
            },
        )
        .unwrap();
        assert_eq!(
            labels(&p),
            [
                "checkout api",
                "build api",
                "recreate api",
                "recreate infra"
            ]
        );
        match &p.steps[1] {
            Step::Build { key, .. } => assert_eq!(key.as_deref(), Some("api:01234567")),
            other => panic!("{other:?}"),
        }
        match &p.steps[2] {
            Step::Recreate { reason, .. } => assert_eq!(
                *reason,
                RecreateReason::ModeChanged {
                    from: Some(Mode::Prebuilt),
                    to: Mode::Source
                }
            ),
            other => panic!("{other:?}"),
        }
        match &p.steps[3] {
            Step::Recreate { reason, .. } => assert_eq!(
                *reason,
                RecreateReason::PinnedImageChanged { of: "api".into() }
            ),
            other => panic!("{other:?}"),
        }
        assert_eq!(p.modes["api"], Mode::Source);
        assert_eq!(p.refs["api"], "main");
        assert_eq!(
            p.summary(),
            "Checkout: api (main)\nBuild: api (api:01234567)\nRecreate: api (mode prebuilt → source), infra (pins image of api)\nNote: migrate already ran to completion"
        );
        // unknown mode for the component
        let mut modes = IndexMap::new();
        modes.insert("db".to_string(), Mode::Source);
        let e = plan(
            &st,
            &state,
            &Change::Up {
                selection: Selection::Default,
                modes,
                channel: None,
            },
        )
        .unwrap_err();
        assert_eq!(
            e,
            PlanError::UnknownMode {
                component: "db".into(),
                mode: Mode::Source
            }
        );
    }

    #[test]
    fn switch_mode_only_touches_the_component_and_its_image_dependents() {
        let st = stack();
        let state = running_state();
        let p = plan(
            &st,
            &state,
            &Change::SwitchMode {
                component: "api".into(),
                mode: Mode::Source,
                git_ref: Some("feature/x".into()),
            },
        )
        .unwrap();
        assert_eq!(p.verb, Verb::SwitchMode);
        assert_eq!(
            labels(&p),
            [
                "checkout api",
                "build api",
                "recreate api",
                "recreate infra"
            ]
        );
        // web depends on api but does not pin its image: untouched
        assert!(!p.components().contains(&"web".to_string()));
        match &p.steps[1] {
            Step::Build { key, .. } => {
                assert_eq!(*key, None, "new ref: commit unknown until checkout")
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(p.summary(), "Checkout: api (feature/x)\nBuild: api\nRecreate: api (mode prebuilt → source), infra (pins image of api)");

        // back to prebuilt: pull + recreate (+ infra)
        let mut state = running_state();
        state.components["api"].mode = Some(Mode::Source);
        let p = plan(
            &st,
            &state,
            &Change::SwitchMode {
                component: "api".into(),
                mode: Mode::Prebuilt,
                git_ref: None,
            },
        )
        .unwrap();
        assert_eq!(labels(&p), ["pull api", "recreate api", "recreate infra"]);

        // dependents that do not exist yet are not recreated
        let mut state = running_state();
        state.components.shift_remove("infra");
        let p = plan(
            &st,
            &state,
            &Change::SwitchMode {
                component: "api".into(),
                mode: Mode::Source,
                git_ref: None,
            },
        )
        .unwrap();
        assert_eq!(labels(&p), ["build api", "recreate api"]);

        // switching a component that is not up starts it
        let state = InstanceState::new("app", 0);
        let p = plan(
            &st,
            &state,
            &Change::SwitchMode {
                component: "api".into(),
                mode: Mode::Source,
                git_ref: Some("main".into()),
            },
        )
        .unwrap();
        assert_eq!(labels(&p), ["checkout api", "build api", "start api"]);

        let e = plan(
            &st,
            &state,
            &Change::SwitchMode {
                component: "db".into(),
                mode: Mode::Dev,
                git_ref: None,
            },
        )
        .unwrap_err();
        assert_eq!(
            e,
            PlanError::UnknownMode {
                component: "db".into(),
                mode: Mode::Dev
            }
        );
        let e = plan(
            &st,
            &state,
            &Change::SwitchMode {
                component: "zzz".into(),
                mode: Mode::Dev,
                git_ref: None,
            },
        )
        .unwrap_err();
        assert_eq!(e, PlanError::UnknownComponent("zzz".into()));
    }

    #[test]
    fn change_ref_rebuilds_source_and_rechecks_out_dev() {
        let st = stack();
        let mut state = running_state();
        state.components["api"].mode = Some(Mode::Source);
        state.components["api"].git_ref = Some("main".into());
        let p = plan(
            &st,
            &state,
            &Change::ChangeRef {
                component: "api".into(),
                git_ref: "feature/x".into(),
            },
        )
        .unwrap();
        assert_eq!(
            labels(&p),
            [
                "checkout api",
                "build api",
                "recreate api",
                "recreate infra"
            ]
        );
        match &p.steps[2] {
            Step::Recreate { reason, .. } => assert_eq!(
                *reason,
                RecreateReason::RefChanged {
                    from: Some("main".into()),
                    to: "feature/x".into()
                }
            ),
            other => panic!("{other:?}"),
        }
        assert_eq!(p.refs["api"], "feature/x");

        // dev component: checkout as export (its mode says so), then recreate
        let p = plan(
            &st,
            &state,
            &Change::ChangeRef {
                component: "web".into(),
                git_ref: "feature/x".into(),
            },
        )
        .unwrap();
        assert_eq!(labels(&p), ["checkout web", "recreate web"]);
        match &p.steps[0] {
            Step::Checkout { kind, .. } => assert_eq!(*kind, CheckoutKind::Export),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            p.summary(),
            "Checkout: web (feature/x, export)\nRecreate: web (ref main → feature/x)"
        );

        // prebuilt has no ref
        let e = plan(
            &st,
            &state,
            &Change::ChangeRef {
                component: "db".into(),
                git_ref: "x".into(),
            },
        )
        .unwrap_err();
        assert_eq!(
            e,
            PlanError::NotRepoMode {
                component: "db".into(),
                mode: Mode::Prebuilt
            }
        );
        let e = plan(
            &st,
            &state,
            &Change::ChangeRef {
                component: "infra".into(),
                git_ref: "x".into(),
            },
        )
        .unwrap_err();
        assert_eq!(
            e,
            PlanError::NoModes {
                component: "infra".into()
            }
        );
    }

    #[test]
    fn rebuild_reacquires_even_when_nothing_changed() {
        let st = stack();
        let mut state = running_state();
        state.components["api"].mode = Some(Mode::Source);
        state.components["api"].git_ref = Some("main".into());
        state.components["api"].commit = Some(SHA.into());
        state.components["api"].dirty = Some("fedcba9876543210".into());
        let p = plan(
            &st,
            &state,
            &Change::Rebuild {
                component: "api".into(),
            },
        )
        .unwrap();
        assert_eq!(
            labels(&p),
            [
                "checkout api",
                "build api",
                "recreate api",
                "recreate infra"
            ]
        );
        match &p.steps[1] {
            Step::Build { key, .. } => {
                assert_eq!(key.clone(), Some(dirty_tag("fedcba98")))
            }
            other => panic!("{other:?}"),
        }
        match &p.steps[2] {
            Step::Recreate { reason, .. } => assert_eq!(*reason, RecreateReason::Rebuilt),
            other => panic!("{other:?}"),
        }
        // prebuilt: a rebuild is a fresh pull
        let p = plan(
            &st,
            &state,
            &Change::Rebuild {
                component: "db".into(),
            },
        )
        .unwrap();
        assert_eq!(labels(&p), ["pull db", "recreate db"]);
    }

    #[test]
    fn stop_honours_order_then_reverse_dependencies_with_grace() {
        let st = stack();
        let state = running_state();
        let p = plan(
            &st,
            &state,
            &Change::Stop {
                keep_groups: vec![],
            },
        )
        .unwrap();
        assert_eq!(p.verb, Verb::Stop);
        // db first (stop.order), then infra, web, api in reverse dependency order; migrate is not running
        assert_eq!(
            labels(&p),
            ["stop db", "stop infra", "stop web", "stop api"]
        );
        let grace: Vec<u32> = p
            .steps
            .iter()
            .map(|s| match s {
                Step::Stop { grace_seconds, .. } => *grace_seconds,
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(grace, [30, 20, 20, 20]);
        assert_eq!(p.desired["db"], Desired::Stopped);
        assert!(!p.desired.contains_key("migrate"));
        assert_eq!(
            p.summary(),
            "Stop: db (30s), infra (20s), web (20s), api (20s)"
        );

        // keep a group
        let p = plan(
            &st,
            &state,
            &Change::Stop {
                keep_groups: vec!["apis".into()],
            },
        )
        .unwrap();
        assert_eq!(labels(&p), ["stop infra", "stop web"]);

        // a component stopped by hand while wanted running: no step, but the
        // intent follows the user's stop so it stops reading as a problem
        let mut hand_stopped = running_state();
        hand_stopped.components["api"].running = false;
        hand_stopped.components["api"].exit_code = Some(137);
        let p = plan(
            &st,
            &hand_stopped,
            &Change::Stop {
                keep_groups: vec![],
            },
        )
        .unwrap();
        assert_eq!(labels(&p), ["stop db", "stop infra", "stop web"]);
        assert_eq!(p.desired["api"], Desired::Stopped);
        assert_eq!(
            p.notes,
            vec!["api was already stopped; recorded as stopped"]
        );
        assert!(
            !p.desired.contains_key("migrate"),
            "a completed oneshot is not re-recorded"
        );

        // without an explicit order it is pure reverse dependency order
        let mut st2 = st.clone();
        st2.stop = None;
        let p = plan(
            &st2,
            &state,
            &Change::Stop {
                keep_groups: vec![],
            },
        )
        .unwrap();
        assert_eq!(
            labels(&p),
            ["stop infra", "stop web", "stop api", "stop db"]
        );
        assert!(matches!(
            p.steps[0],
            Step::Stop {
                grace_seconds: 10,
                ..
            }
        ));
        assert!(matches!(
            p.steps[3],
            Step::Stop {
                grace_seconds: 30,
                ..
            }
        ));
    }

    #[test]
    fn down_stops_then_removes_everything_that_exists() {
        let st = stack();
        let state = running_state();
        let p = plan(
            &st,
            &state,
            &Change::Down {
                keep_groups: vec![],
            },
        )
        .unwrap();
        assert_eq!(
            labels(&p),
            [
                "stop db",
                "stop infra",
                "stop web",
                "stop api",
                "remove db",
                "remove infra",
                "remove migrate",
                "remove web",
                "remove api"
            ]
        );
        assert_eq!(p.desired["db"], Desired::Absent);
        assert_eq!(p.desired["migrate"], Desired::Absent);
        let p = plan(
            &st,
            &state,
            &Change::Down {
                keep_groups: vec!["core".into(), "apis".into()],
            },
        )
        .unwrap();
        assert_eq!(labels(&p), ["stop infra", "remove infra"]);
        let p = plan(
            &st,
            &InstanceState::new("app", 0),
            &Change::Down {
                keep_groups: vec![],
            },
        )
        .unwrap();
        assert!(p.is_empty());
    }

    #[test]
    fn start_after_reboot_recreates_only_components_with_a_stale_gateway() {
        let st = stack();
        let mut state = running_state();
        for c in state.components.values_mut() {
            c.running = false; // the machine rebooted
        }
        state.components["api"].gateway = Some("172.17.0.1".into());
        state.components["db"].gateway = Some("172.17.0.1".into());
        state.components["web"].gateway = None;
        let mut gateways = IndexMap::new();
        gateways.insert("podman".to_string(), "172.18.0.1".to_string());
        gateways.insert("docker".to_string(), "192.168.65.2".to_string());
        let p = plan(&st, &state, &Change::Start { gateways }).unwrap();
        assert_eq!(p.verb, Verb::Start);
        assert_eq!(
            labels(&p),
            ["recreate db", "recreate api", "start web", "start infra"]
        );
        match &p.steps[0] {
            Step::Recreate { reason, .. } => assert_eq!(
                *reason,
                RecreateReason::GatewayChanged {
                    from: "172.17.0.1".into(),
                    to: "172.18.0.1".into()
                }
            ),
            other => panic!("{other:?}"),
        }
        assert_eq!(p.notes, vec!["migrate already ran to completion"]);

        // same gateway: plain starts
        let mut same = IndexMap::new();
        same.insert("podman".to_string(), "172.17.0.1".to_string());
        let p = plan(&st, &state, &Change::Start { gateways: same }).unwrap();
        assert_eq!(
            labels(&p),
            ["start db", "start api", "start web", "start infra"]
        );

        // components not wanted running are left alone; missing containers are noted
        state.components["web"].desired = Desired::Stopped;
        state.components["infra"].exists = false;
        let p = plan(
            &st,
            &state,
            &Change::Start {
                gateways: IndexMap::new(),
            },
        )
        .unwrap();
        assert_eq!(labels(&p), ["start db", "start api"]);
        assert!(p
            .notes
            .iter()
            .any(|n| n.contains("infra has nothing to start")));

        // already running: nothing
        let p = plan(
            &st,
            &running_state(),
            &Change::Start {
                gateways: IndexMap::new(),
            },
        )
        .unwrap();
        assert!(p.is_empty());
    }

    #[test]
    fn fingerprint_is_order_independent_and_change_sensitive() {
        let a = running_state();
        let mut b = InstanceState::new("app", 1);
        for (k, v) in a.components.iter().rev() {
            b.components.insert(k.clone(), v.clone());
        }
        assert_eq!(a.fingerprint(), b.fingerprint());
        assert_eq!(a.fingerprint().len(), 16);
        let mut c = a.clone();
        c.components["api"].running = false;
        assert_ne!(a.fingerprint(), c.fingerprint());
        let mut d = a.clone();
        d.slot = 2;
        assert_ne!(a.fingerprint(), d.fingerprint());
    }

    #[test]
    fn plan_refuses_a_changed_state() {
        let st = stack();
        let state = running_state();
        let p = plan(
            &st,
            &state,
            &Change::Stop {
                keep_groups: vec![],
            },
        )
        .unwrap();
        assert!(p.check_against(&state).is_ok());
        let mut changed = state.clone();
        changed.components["db"].running = false;
        match p.check_against(&changed).unwrap_err() {
            PlanError::StateChanged { expected, actual } => {
                assert_eq!(expected, p.fingerprint);
                assert_eq!(actual, changed.fingerprint());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn cycle_is_a_plan_error() {
        let mut st = stack();
        st.components["db"].depends_on = vec!["web".into()];
        let e = plan(&st, &InstanceState::new("app", 0), &up_default()).unwrap_err();
        assert!(matches!(e, PlanError::Cycle(_)));
        assert!(e.to_string().contains("db -> web -> api -> db"));
    }

    #[test]
    fn steps_and_plan_serialize_with_tags() {
        let st = stack();
        let p = plan(
            &st,
            &InstanceState::new("app", 0),
            &Change::SwitchMode {
                component: "api".into(),
                mode: Mode::Source,
                git_ref: Some("main".into()),
            },
        )
        .unwrap();
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["verb"], "switch-mode");
        assert_eq!(json["steps"][0]["step"], "checkout");
        assert_eq!(json["steps"][0]["ref"], "main");
        assert_eq!(json["steps"][0]["kind"], "worktree");
        assert_eq!(json["steps"][2]["step"], "start");
        let back: Plan = serde_json::from_value(json).unwrap();
        assert_eq!(back, p);

        let r = Step::Recreate {
            component: "infra".into(),
            reason: RecreateReason::PinnedImageChanged { of: "api".into() },
        };
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json["step"], "recreate");
        assert_eq!(json["reason"]["kind"], "pinned-image-changed");
        assert_eq!(json["reason"]["of"], "api");
        assert_eq!(serde_json::from_value::<Step>(json).unwrap(), r);

        let ch = Change::Up {
            selection: Selection::Named("linux-only".into()),
            modes: IndexMap::new(),
            channel: None,
        };
        let json = serde_json::to_value(&ch).unwrap();
        assert_eq!(json["verb"], "up");
        assert_eq!(json["selection"]["kind"], "named");
        assert_eq!(json["selection"]["value"], "linux-only");
        assert_eq!(serde_json::from_value::<Change>(json).unwrap(), ch);
    }

    #[test]
    fn builder_style_definitions_plan_too() {
        // A stack built in code rather than parsed: the planner does not care.
        let mut st = StackDef::default();
        let mut api = ComponentDef::new(DriverKind::Compose);
        api.file = Some("s.yml".into());
        api.modes.insert(
            "source".into(),
            ModeDef {
                repo: Some("../api".into()),
                ..ModeDef::default()
            },
        );
        st.components.insert("api".into(), api);
        st.stop = Some(StopDef::default());
        let p = plan(&st, &InstanceState::new("mini", 0), &up_default()).unwrap();
        assert_eq!(labels(&p), ["build api", "start api"]);
        assert_eq!(p.instance, "mini-0");
    }

    #[test]
    fn stop_components_stops_only_those_in_stop_order() {
        let st = stack();
        let state = running_state();
        let p = plan(
            &st,
            &state,
            &Change::StopComponents {
                components: vec!["api".into(), "db".into()],
            },
        )
        .unwrap();
        assert_eq!(p.verb, Verb::Stop);
        assert_eq!(labels(&p), ["stop db", "stop api"]);
        assert_eq!(p.desired["api"], Desired::Stopped);
        assert!(!p.desired.contains_key("web"));
        // Already down while wanted running: only the intent follows.
        let mut down = running_state();
        down.components["api"].running = false;
        let p = plan(
            &st,
            &down,
            &Change::StopComponents {
                components: vec!["api".into()],
            },
        )
        .unwrap();
        assert!(p.steps.is_empty());
        assert_eq!(p.desired["api"], Desired::Stopped);
        assert!(plan(
            &st,
            &state,
            &Change::StopComponents {
                components: vec!["nope".into()]
            }
        )
        .is_err());
    }

    #[test]
    fn rebuild_all_reacquires_every_image_and_leaves_dev_and_imageless_alone() {
        let st = stack();
        let p = plan(&st, &running_state(), &Change::RebuildAll).unwrap();
        assert_eq!(p.verb, Verb::Rebuild);
        assert_eq!(
            labels(&p),
            [
                "pull db",
                "recreate db",
                "pull api",
                "recreate api",
                "pull migrate",
                "recreate migrate",
                "recreate infra"
            ]
        );
    }

    fn bind(item: &str, targets: &[(&str, Mode, &str, bool)], release: &[(&str, Mode)]) -> Change {
        Change::Bind {
            item: item.into(),
            bind: targets
                .iter()
                .map(|(c, m, r, rebuild)| {
                    (
                        c.to_string(),
                        BindTarget {
                            mode: *m,
                            git_ref: r.to_string(),
                            rebuild: *rebuild,
                        },
                    )
                })
                .collect(),
            release: release.iter().map(|(c, m)| (c.to_string(), *m)).collect(),
        }
    }

    #[test]
    fn binding_an_item_rebuilds_only_its_repos_components() {
        let st = stack();
        let state = running_state();
        let p = plan(
            &st,
            &state,
            &bind(
                "ABC-1",
                &[
                    ("api", Mode::Source, "task/ABC-1-fix", false),
                    ("web", Mode::Dev, "task/ABC-1-fix", false),
                ],
                &[],
            ),
        )
        .unwrap();
        assert_eq!(p.verb, Verb::Bind);
        assert_eq!(
            labels(&p),
            [
                "checkout api",
                "build api",
                "recreate api",
                "checkout web",
                "recreate web",
                "recreate infra"
            ]
        );
        assert_eq!(p.refs["api"], "task/ABC-1-fix");
        assert_eq!(p.modes["api"], Mode::Source);
        assert!(!p.modes.contains_key("db"), "db keeps its mode");
        match &p.steps[2] {
            Step::Recreate { reason, .. } => assert_eq!(
                *reason,
                RecreateReason::ModeChanged {
                    from: Some(Mode::Prebuilt),
                    to: Mode::Source
                }
            ),
            other => panic!("{other:?}"),
        }

        // Rebinding: api moves to the other item's branch, web is released.
        let mut bound = state.clone();
        bound.components["api"].mode = Some(Mode::Source);
        bound.components["api"].git_ref = Some("task/ABC-1-fix".into());
        let p = plan(
            &st,
            &bound,
            &bind(
                "ABC-2",
                &[("api", Mode::Source, "task/ABC-2-other", false)],
                &[],
            ),
        )
        .unwrap();
        assert_eq!(
            labels(&p),
            [
                "checkout api",
                "build api",
                "recreate api",
                "recreate infra"
            ]
        );
        match &p.steps[2] {
            Step::Recreate { reason, .. } => assert_eq!(
                *reason,
                RecreateReason::RefChanged {
                    from: Some("task/ABC-1-fix".into()),
                    to: "task/ABC-2-other".into()
                }
            ),
            other => panic!("{other:?}"),
        }

        // Same mode and ref: nothing to do but say so; a rebuild builds again.
        let p = plan(
            &st,
            &bound,
            &bind(
                "ABC-1",
                &[("api", Mode::Source, "task/ABC-1-fix", false)],
                &[],
            ),
        )
        .unwrap();
        assert!(p.steps.is_empty());
        assert_eq!(p.notes, ["api already builds from task/ABC-1-fix"]);
        let p = plan(
            &st,
            &bound,
            &bind(
                "ABC-1",
                &[("api", Mode::Source, "task/ABC-1-fix", true)],
                &[],
            ),
        )
        .unwrap();
        assert_eq!(
            labels(&p),
            [
                "checkout api",
                "build api",
                "recreate api",
                "recreate infra"
            ]
        );
        assert!(matches!(&p.steps[1], Step::Build { key: None, .. }));

        // A component that is not deployed only has its binding recorded.
        let fresh = InstanceState::new("app", 0);
        let p = plan(
            &st,
            &fresh,
            &bind(
                "ABC-1",
                &[("api", Mode::Source, "task/ABC-1-fix", false)],
                &[],
            ),
        )
        .unwrap();
        assert!(p.steps.is_empty());
        assert_eq!(p.refs["api"], "task/ABC-1-fix");
        assert_eq!(
            p.notes,
            ["api is not deployed; it builds from task/ABC-1-fix of ABC-1 when it starts"]
        );

        // Released components go back to the given mode.
        let p = plan(&st, &bound, &bind("ABC-3", &[], &[("api", Mode::Prebuilt)])).unwrap();
        assert_eq!(labels(&p), ["pull api", "recreate api", "recreate infra"]);

        let e = plan(
            &st,
            &state,
            &bind("X", &[("db", Mode::Source, "b", false)], &[]),
        )
        .unwrap_err();
        assert_eq!(
            e,
            PlanError::UnknownMode {
                component: "db".into(),
                mode: Mode::Source
            }
        );
    }

    #[test]
    fn resuming_pins_the_saved_bits_and_leaves_what_matches() {
        let st = stack();
        let state = running_state();
        let pin = |image: &str| ImagePin {
            image: Some(image.into()),
            build_id: Some("main".into()),
            ..Default::default()
        };
        let mut targets = IndexMap::new();
        // db runs the saved digest already: nothing to do.
        targets.insert(
            "db".to_string(),
            ResumeTarget {
                mode: Mode::Prebuilt,
                git_ref: None,
                pin: Some(pin("sha256:img")),
                desired: Desired::Running,
                fresh: false,
            },
        );
        // api was a source build: rebuilt (or reused) by its content key.
        targets.insert(
            "api".to_string(),
            ResumeTarget {
                mode: Mode::Source,
                git_ref: Some("task/ABC-1".into()),
                pin: Some(ImagePin {
                    image: Some("api:1a2b3c4d".into()),
                    commit: Some("1a2b3c4d".into()),
                    ..Default::default()
                }),
                desired: Desired::Running,
                fresh: false,
            },
        );
        // web was not running when saved: stopped, never removed.
        targets.insert(
            "web".to_string(),
            ResumeTarget {
                mode: Mode::Dev,
                git_ref: None,
                pin: None,
                desired: Desired::Stopped,
                fresh: false,
            },
        );
        let change = Change::Resume {
            item: Some("ABC-1".into()),
            targets: targets.clone(),
            scripts: Vec::new(),
        };
        let plan = plan(&st, &state, &change).unwrap();
        assert_eq!(plan.verb, Verb::Resume);
        assert_eq!(
            labels(&plan),
            ["build api", "recreate api", "stop web", "recreate infra"]
        );
        assert!(matches!(&plan.steps[0], Step::Build { key: Some(k), .. } if k == "api:1a2b3c4d"));
        assert!(plan.notes.contains(&"db already runs as saved".to_string()));
        assert_eq!(plan.pins["api"].image.as_deref(), Some("api:1a2b3c4d"));
        assert_eq!(plan.refs["api"], "task/ABC-1");
        assert_eq!(plan.desired["web"], Desired::Stopped);
        assert!(!plan.steps.iter().any(|s| matches!(s, Step::Remove { .. })));

        // A recipe component is recreated even on the same image (a clean
        // container); a prebuilt one on another digest is pulled by it.
        targets["db"].fresh = true;
        targets["db"].pin = Some(pin("sha256:other0000000000"));
        let plan = super::plan(
            &st,
            &state,
            &Change::Resume {
                item: None,
                targets,
                scripts: Vec::new(),
            },
        )
        .unwrap();
        assert_eq!(labels(&plan)[..2], ["pull db", "recreate db"]);
        assert_eq!(
            plan.steps[0].detail().as_deref(),
            Some("registry.example/db:main@sha256:other0000000")
        );
        assert!(
            plan.summary().contains("Recreate: db (saved state)"),
            "{}",
            plan.summary()
        );
    }
}
