//! The `stacks:` document: what a project commits to describe its stacks.
//!
//! ```yaml
//! stacks:
//!   app:
//!     slots: { count: 4, portBase: 14000, portStride: 100 }
//!     engine: docker                       # default engine for compose components
//!     groups:
//!       core: { engine: docker }
//!       apis: { engine: podman }           # a second engine on the same machine
//!     hostname: dev.example.test
//!     components:
//!       db:
//!         group: apis
//!         driver: compose
//!         file: stack/db.compose.yml
//!         service: db
//!         modes: { prebuilt: { image: "registry.example/db", tag: "{channel}" } }
//!         persistence: { volume: db-data }
//!       api:
//!         group: apis
//!         driver: compose
//!         file: stack/api.compose.yml
//!         service: api
//!         modes:
//!           prebuilt: { image: "registry.example/api", tag: "{channel}" }
//!           source:   { repo: ../api, context: ., dockerfile: Dockerfile }
//!         default: prebuilt
//!         depends_on: [db]
//!         probe: { url: "http://{hostname}:{port:api}/health/ready", rule: health }
//!         outputs: { url: "http://{hostname}:{port:api}" }
//!         memoryMb: 512
//! ```
//!
//! Parsing is lenient about keys it does not model (they are kept and
//! reported as warnings, so a typo such as `normalyOff` is visible instead
//! of silently inert) and strict about shape (a component without `driver`
//! does not load). [`validate`] then applies the cross-cutting rules and
//! returns every finding at once with a path such as
//! `stacks.app.components.api.depends_on[0]`, so one load fixes one round.
//!
//! The reasons behind the rules that are not obvious:
//! - **cross-engine references must be host URLs**: engines share no
//!   network, so a service name of one engine does not resolve inside the
//!   other's containers. Traffic between them goes over host ports, which
//!   is what `{host}:{port:<component>}` spells.
//! - **source mode needs a registry when it feeds terraform**: an image
//!   built locally is invisible to a cloud provider until it is pushed.
//! - **persistence needs a named volume**: an anonymous volume dies with
//!   the container, taking every snapshot in it along.
//! - **persistent components without a data-safety check warn**: a
//!   database nobody checks is treated as sensitive, and the user should
//!   know the floor they are standing on.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_yaml::Value as YamlValue;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use crate::driver::EngineKind;
use crate::placeholders::{self, Placeholder};
use crate::ports;
use crate::safety::Policy;

/// The project file that normally carries `stacks:`.
pub const PROJECT_FILE_NAME: &str = "chappa.yml";
/// The optional stand-alone home of the same document.
pub const STACK_FILE_NAME: &str = "chappa.stack.yml";
/// Hard cap on either file (larger documents are refused).
pub const MAX_DOC_BYTES: usize = 1 << 20;
/// The stack project's work folder, holding instances, items, jobs.
pub const DEFAULT_WORK_DIR: &str = ".work";
/// Local terraform state per instance lives here, inside the project.
pub const DEFAULT_STATE_DIR: &str = ".chappa/stack-state";
/// Keys used in paths (`.work/<KEY>/`, image tags) are limited to this
/// alphabet and length so they stay short on Windows and safe everywhere.
pub const KEY_MAX_LEN: usize = 40;

const DEFAULT_SLOT_COUNT: u32 = 1;
const DEFAULT_PORT_BASE: u16 = 14000;
const DEFAULT_PORT_STRIDE: u16 = 100;
const DEFAULT_GRACE_SECONDS: u32 = 10;
const DEFAULT_SNAPSHOT_KEEP: u32 = 10;

// ---------------------------------------------------------------- document

/// The whole document: one or more named stacks.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct StacksDocument {
    #[serde(default)]
    pub stacks: IndexMap<String, StackDef>,
    /// Top-level keys this version does not model, kept for write-back.
    #[serde(default, flatten, skip_serializing_if = "IndexMap::is_empty")]
    pub unknown: IndexMap<String, YamlValue>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StackDef {
    #[serde(default)]
    pub slots: SlotsDef,
    /// Default engine for components that name none, directly or via group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    /// Named engines beyond the built-in kinds, e.g. a remote docker context.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub engines: IndexMap<String, EngineDef>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub groups: IndexMap<String, GroupDef>,
    #[serde(default)]
    pub components: IndexMap<String, ComponentDef>,
    /// One name for the stack; every URL uses it. Required with `tls` or `relay`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay: Option<RelayDef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls: Option<TlsDef>,
    /// Where source-built images are pushed when a cloud component needs them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry: Option<String>,
    /// Folder holding the shared clones that component `repo:` paths name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<PathBuf>,
    #[serde(default = "default_work_dir")]
    pub work_dir: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_safety: Option<DataSafetyDef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secrets: Option<SecretsDef>,
    /// Named component sets `up` can be asked for.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub selections: IndexMap<String, Vec<String>>,
    /// Variables every process, agent and terminal of an instance gets,
    /// resolved on the host side (`{slot}`, `{port:x}`, `{hostname}` …).
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub env: IndexMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<ProvenanceDef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<StopDef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_hours: Option<u32>,
    #[serde(default)]
    pub auto_destroy: bool,
    #[serde(default, flatten, skip_serializing_if = "IndexMap::is_empty")]
    pub unknown: IndexMap<String, YamlValue>,
}

impl Default for StackDef {
    fn default() -> Self {
        Self {
            slots: SlotsDef::default(),
            engine: None,
            engines: IndexMap::new(),
            groups: IndexMap::new(),
            components: IndexMap::new(),
            hostname: None,
            relay: None,
            tls: None,
            registry: None,
            root: None,
            work_dir: default_work_dir(),
            data_safety: None,
            secrets: None,
            selections: IndexMap::new(),
            env: IndexMap::new(),
            provenance: None,
            stop: None,
            ttl_hours: None,
            auto_destroy: false,
            unknown: IndexMap::new(),
        }
    }
}

fn default_work_dir() -> PathBuf {
    PathBuf::from(DEFAULT_WORK_DIR)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SlotsDef {
    #[serde(default = "default_slot_count")]
    pub count: u32,
    #[serde(default = "default_port_base")]
    pub port_base: u16,
    #[serde(default = "default_port_stride")]
    pub port_stride: u16,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub placements: IndexMap<String, PlacementDef>,
    /// Slot number to placement name; unassigned slots ask at bring-up.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub assign: IndexMap<u32, String>,
}

impl Default for SlotsDef {
    fn default() -> Self {
        Self {
            count: default_slot_count(),
            port_base: default_port_base(),
            port_stride: default_port_stride(),
            placements: IndexMap::new(),
            assign: IndexMap::new(),
        }
    }
}

fn default_slot_count() -> u32 {
    DEFAULT_SLOT_COUNT
}
fn default_port_base() -> u16 {
    DEFAULT_PORT_BASE
}
fn default_port_stride() -> u16 {
    DEFAULT_PORT_STRIDE
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlacementDef {
    /// Terraform module that creates the slot's host(s) and outputs engine endpoints.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provision: Option<PathBuf>,
    /// Group name to the kind of remote engine registered from the outputs.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub engines: IndexMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_hours: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<BudgetDef>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BudgetDef {
    pub per_slot_per_day: f64,
    #[serde(default = "default_currency")]
    pub currency: String,
}

fn default_currency() -> String {
    "USD".to_string()
}

/// A named engine: a built-in kind plus, optionally, a remote context
/// (`docker context` / podman connection name) it runs through.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineDef {
    pub kind: EngineKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupDef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The container OS this group's images need. A docker daemon switched
    /// to the other mode fails the group's reachability check with that
    /// reason instead of failing every `up` halfway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<ContainerOs>,
}

/// Which kind of containers an engine runs. Docker Desktop on Windows is
/// one or the other at a time; podman is always Linux.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContainerOs {
    Linux,
    Windows,
}

impl ContainerOs {
    pub fn as_str(self) -> &'static str {
        match self {
            ContainerOs::Linux => "linux",
            ContainerOs::Windows => "windows",
        }
    }

    /// Parse the engine's own spelling (`OSType` in `docker info`).
    pub fn parse(s: &str) -> Option<ContainerOs> {
        match s.trim().to_ascii_lowercase().as_str() {
            "linux" => Some(ContainerOs::Linux),
            "windows" => Some(ContainerOs::Windows),
            _ => None,
        }
    }
}

impl fmt::Display for ContainerOs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayDef {
    /// Containers on these engines reach host-published ports through the relay.
    #[serde(default)]
    pub engines: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TlsDef {
    #[serde(default)]
    pub mode: TlsMode,
    #[serde(default)]
    pub components: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TlsMode {
    #[default]
    SelfSigned,
    LocalCa,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataSafetyDef {
    /// Detection rules file, committable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules: Option<PathBuf>,
    #[serde(default = "default_data_policy")]
    pub on_finding: Policy,
    #[serde(default = "default_unscanned_policy")]
    pub unscanned: Policy,
}

fn default_data_policy() -> Policy {
    Policy::BlockAgents
}
fn default_unscanned_policy() -> Policy {
    Policy::Warn
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SecretsDef {
    #[serde(default = "default_data_policy")]
    pub on_finding: Policy,
}

/// Which image labels fill which provenance field, each a list tried in
/// order. These extend the built-in OCI keys, which always come first.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LabelMapDef {
    #[serde(default)]
    pub commit: Vec<String>,
    #[serde(default)]
    pub branch: Vec<String>,
    #[serde(default)]
    pub repo: Vec<String>,
    #[serde(default)]
    pub mode: Vec<String>,
    #[serde(default)]
    pub build_id: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProvenanceDef {
    #[serde(default)]
    pub labels: LabelMapDef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StopDef {
    /// Components stopped first, in this order; the rest follow in reverse
    /// dependency order.
    #[serde(default)]
    pub order: Vec<String>,
    #[serde(default = "default_grace")]
    pub grace_seconds: u32,
}

impl Default for StopDef {
    fn default() -> Self {
        Self {
            order: Vec::new(),
            grace_seconds: default_grace(),
        }
    }
}

fn default_grace() -> u32 {
    DEFAULT_GRACE_SECONDS
}

// -------------------------------------------------------------- components

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DriverKind {
    Compose,
    Terraform,
    Process,
    Script,
}

impl DriverKind {
    /// The engine a component of this driver uses when nothing names one.
    pub fn default_engine(self) -> EngineKind {
        match self {
            DriverKind::Compose => EngineKind::Docker,
            DriverKind::Terraform => EngineKind::Terraform,
            DriverKind::Process => EngineKind::Process,
            DriverKind::Script => EngineKind::Script,
        }
    }

    /// Host ports a component publishes when `ports:` is not given.
    /// Terraform resources live in the cloud and publish nothing locally.
    pub fn default_port_count(self) -> u16 {
        match self {
            DriverKind::Terraform => 0,
            _ => 1,
        }
    }
}

impl fmt::Display for DriverKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            DriverKind::Compose => "compose",
            DriverKind::Terraform => "terraform",
            DriverKind::Process => "process",
            DriverKind::Script => "script",
        })
    }
}

/// How a component gets its bits: a saved image, an image built from a
/// repo at a ref, or a process run from the worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Prebuilt,
    Source,
    Dev,
}

impl Mode {
    pub const ALL: [Mode; 3] = [Mode::Prebuilt, Mode::Source, Mode::Dev];

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Prebuilt => "prebuilt",
            Mode::Source => "source",
            Mode::Dev => "dev",
        }
    }

    pub fn parse(s: &str) -> Option<Mode> {
        Mode::ALL.into_iter().find(|m| m.as_str() == s)
    }

    /// Modes that work from a repo checkout.
    pub fn uses_repo(self) -> bool {
        matches!(self, Mode::Source | Mode::Dev)
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ComponentKind {
    #[default]
    Service,
    /// Runs to completion (migrations, topology init); judged by exit code.
    Oneshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckoutKind {
    /// An editable git worktree; agents commit here.
    #[default]
    Worktree,
    /// A `git archive` of one commit: a read-only build input.
    Export,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModeDef {
    /// prebuilt: image repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// prebuilt: tag template, e.g. `{channel}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// source / dev: the clone, relative to the stack `root` or the project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dockerfile: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkout: Option<CheckoutKind>,
    /// dev: the project command that runs the component from its worktree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// dev: the project command for a rebuild-on-change loop; `command`
    /// when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watch: Option<String>,
    /// prebuilt: argv that lists the builds available for this image as
    /// JSON (`[{tag, digest, commit?, branch?, buildId?, buildUrl?, created}]`).
    /// `{param:image}` is the repository. Without it, only the tag in
    /// force can be inspected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog: Option<Vec<String>>,
    /// prebuilt: argv that turns a tag into `{commit, branch, buildId,
    /// buildUrl}` JSON when the image's labels do not say. `{param:image}`
    /// and `{param:tag}` are filled in. Its answer is cached per digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolve: Option<Vec<String>>,
    #[serde(default, flatten, skip_serializing_if = "IndexMap::is_empty")]
    pub unknown: IndexMap<String, YamlValue>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProbeRule {
    /// 2xx only: real health routes.
    #[default]
    Health,
    /// 2xx or 3xx: web apps whose root redirects.
    Web,
    /// Any HTTP status: "it answered" means "it is listening".
    Any,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeDef {
    pub url: String,
    #[serde(default)]
    pub rule: ProbeRule,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DataStrategy {
    /// Data comes from the image plus scrub → seed → state seeds; a saved
    /// state records the recipe, never the rows.
    #[default]
    Recipe,
    /// Data survives in a named volume and is saved through snapshot hooks.
    Snapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataDef {
    #[serde(default)]
    pub strategy: DataStrategy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scrub: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub state_seeds: IndexMap<String, Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotHooks {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub create: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exists: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delete: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SnapshotLocation {
    #[default]
    InVolume,
    Host,
    Remote,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PersistenceDef {
    /// Must be a named volume; an anonymous one dies with the container.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<SnapshotHooks>,
    #[serde(default)]
    pub location: SnapshotLocation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_dir: Option<PathBuf>,
    #[serde(default = "default_snapshot_keep")]
    pub keep: u32,
}

fn default_snapshot_keep() -> u32 {
    DEFAULT_SNAPSHOT_KEEP
}

/// When a data-safety check runs. `after-up` and friends are names; a
/// periodic trigger is spelled `every: 6h`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ScanTrigger {
    Named(String),
    Every { every: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComponentDataSafetyDef {
    /// Prints `{scrubbed, version?, at?}`: attestation that the scrub ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marker: Option<Vec<String>>,
    /// Prints `{table, column, values[]}` rows for the rule engine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample: Option<Vec<String>>,
    #[serde(default)]
    pub when: Vec<ScanTrigger>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComponentStopDef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grace_seconds: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComponentDef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    pub driver: DriverKind,
    /// Overrides the group's engine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    // compose
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    // process
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    // terraform
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub vars: IndexMap<String, String>,
    // script
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub up: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub down: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Vec<String>>,
    /// Keyed by mode name (`prebuilt`, `source`, `dev`); unknown names are
    /// a validation error rather than a parse failure.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub modes: IndexMap<String, ModeDef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Mode>,
    #[serde(default, rename = "depends_on", skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe: Option<ProbeDef>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub outputs: IndexMap<String, String>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub env: IndexMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mb: Option<u32>,
    #[serde(default)]
    pub kind: ComponentKind,
    /// Left out of the default selection; started on demand.
    #[serde(default)]
    pub normally_off: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_hours: Option<u32>,
    /// Host ports this component publishes (consecutive indexes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ports: Option<u16>,
    /// Pin the first port index instead of taking the next free one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port_index: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<ComponentStopDef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<DataDef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persistence: Option<PersistenceDef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_safety: Option<ComponentDataSafetyDef>,
    #[serde(default, flatten, skip_serializing_if = "IndexMap::is_empty")]
    pub unknown: IndexMap<String, YamlValue>,
}

impl ComponentDef {
    /// A minimal component of one driver; tests and builders start here.
    pub fn new(driver: DriverKind) -> Self {
        Self {
            group: None,
            driver,
            engine: None,
            label: None,
            file: None,
            service: None,
            command: None,
            dir: None,
            vars: IndexMap::new(),
            up: None,
            down: None,
            status: None,
            modes: IndexMap::new(),
            default: None,
            depends_on: Vec::new(),
            probe: None,
            outputs: IndexMap::new(),
            env: IndexMap::new(),
            memory_mb: None,
            kind: ComponentKind::Service,
            normally_off: false,
            ttl_hours: None,
            ports: None,
            port_index: None,
            stop: None,
            data: None,
            persistence: None,
            data_safety: None,
            unknown: IndexMap::new(),
        }
    }

    pub fn mode(&self, mode: Mode) -> Option<&ModeDef> {
        self.modes.get(mode.as_str())
    }

    pub fn has_mode(&self, mode: Mode) -> bool {
        self.modes.contains_key(mode.as_str())
    }

    /// The mode a fresh instance runs this component in: `default`, else
    /// the first declared mode, else `None` when the component has no modes
    /// (a terraform or script component with nothing to choose).
    pub fn default_mode(&self) -> Option<Mode> {
        self.default
            .or_else(|| self.modes.keys().find_map(|k| Mode::parse(k)))
    }

    pub fn port_count(&self) -> u16 {
        self.ports
            .unwrap_or_else(|| self.driver.default_port_count())
    }

    /// Every string that may carry placeholders, with its path suffix.
    pub fn templates(&self) -> Vec<(String, &str)> {
        let mut out = Vec::new();
        for (k, v) in &self.vars {
            out.push((format!("vars.{k}"), v.as_str()));
        }
        for (k, v) in &self.env {
            out.push((format!("env.{k}"), v.as_str()));
        }
        for (k, v) in &self.outputs {
            out.push((format!("outputs.{k}"), v.as_str()));
        }
        if let Some(p) = &self.probe {
            out.push(("probe.url".to_string(), p.url.as_str()));
        }
        for (name, m) in &self.modes {
            if let Some(t) = &m.tag {
                out.push((format!("modes.{name}.tag"), t.as_str()));
            }
        }
        out
    }

    /// Argv hooks with their path suffix, for placeholder checks.
    pub fn hooks(&self) -> Vec<(String, &[String])> {
        fn push<'a>(
            out: &mut Vec<(String, &'a [String])>,
            path: &str,
            hook: &'a Option<Vec<String>>,
        ) {
            if let Some(h) = hook {
                out.push((path.to_string(), h.as_slice()));
            }
        }
        let mut out: Vec<(String, &[String])> = Vec::new();
        push(&mut out, "up", &self.up);
        push(&mut out, "down", &self.down);
        push(&mut out, "status", &self.status);
        if let Some(d) = &self.data {
            push(&mut out, "data.scrub", &d.scrub);
            push(&mut out, "data.seed", &d.seed);
            for (k, v) in &d.state_seeds {
                out.push((format!("data.stateSeeds.{k}"), v.as_slice()));
            }
        }
        if let Some(p) = &self.persistence {
            if let Some(s) = &p.snapshot {
                push(&mut out, "persistence.snapshot.create", &s.create);
                push(&mut out, "persistence.snapshot.restore", &s.restore);
                push(&mut out, "persistence.snapshot.exists", &s.exists);
                push(&mut out, "persistence.snapshot.delete", &s.delete);
            }
        }
        if let Some(ds) = &self.data_safety {
            push(&mut out, "dataSafety.marker", &ds.marker);
            push(&mut out, "dataSafety.sample", &ds.sample);
        }
        for (name, m) in &self.modes {
            push(&mut out, &format!("modes.{name}.catalog"), &m.catalog);
            push(&mut out, &format!("modes.{name}.resolve"), &m.resolve);
        }
        out
    }

    /// The dev-mode command that runs a change loop: `watch`, else `command`.
    pub fn watch_command(&self) -> Option<&str> {
        let dev = self.mode(Mode::Dev)?;
        dev.watch.as_deref().or(dev.command.as_deref())
    }

    /// The command a dev-mode run uses: the dev mode's own, else the
    /// component's `command` (the process driver's spelling).
    pub fn dev_command(&self) -> Option<&str> {
        self.mode(Mode::Dev)
            .and_then(|m| m.command.as_deref())
            .or(self.command.as_deref())
    }
}

// --------------------------------------------------------------- resolution

/// An engine as a component sees it: the name used in the definition, its
/// kind, and the remote context when it is not this machine's.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EngineRef {
    pub name: String,
    pub kind: EngineKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

impl StackDef {
    /// The engine name a component resolves to: its own, else its group's,
    /// else the stack default, else the driver's built-in default. `None`
    /// when the component does not exist.
    pub fn engine_name_for(&self, component: &str) -> Option<String> {
        let c = self.components.get(component)?;
        if let Some(e) = &c.engine {
            return Some(e.clone());
        }
        if let Some(g) = c.group.as_ref().and_then(|g| self.groups.get(g)) {
            if let Some(e) = &g.engine {
                return Some(e.clone());
            }
        }
        // The stack default is a compose engine; other drivers ignore it.
        if c.driver == DriverKind::Compose {
            if let Some(e) = &self.engine {
                return Some(e.clone());
            }
        }
        Some(c.driver.default_engine().name().to_string())
    }

    /// Resolve an engine name to its kind and context: a declared `engines:`
    /// entry, or a built-in kind by name. `None` for an unknown name.
    pub fn engine_ref(&self, name: &str) -> Option<EngineRef> {
        if let Some(def) = self.engines.get(name) {
            return Some(EngineRef {
                name: name.to_string(),
                kind: def.kind,
                context: def.context.clone(),
            });
        }
        EngineKind::parse(name).map(|kind| EngineRef {
            name: name.to_string(),
            kind,
            context: None,
        })
    }

    /// Both steps at once.
    pub fn engine_for(&self, component: &str) -> Option<EngineRef> {
        self.engine_ref(&self.engine_name_for(component)?)
    }

    /// The container OS the groups on `engine` declared, if any did.
    /// Validation refuses two groups asking one engine for different ones.
    pub fn os_for_engine(&self, engine: &str) -> Option<ContainerOs> {
        self.groups
            .values()
            .filter(|g| g.engine.as_deref() == Some(engine))
            .find_map(|g| g.os)
    }

    /// Components whose templates mention `{image:<component>}`: they bake
    /// that image in and must be recreated when it changes.
    pub fn image_dependents(&self, component: &str) -> Vec<String> {
        self.components
            .iter()
            .filter(|(name, _)| name.as_str() != component)
            .filter(|(_, def)| {
                def.templates().iter().any(|(_, t)| {
                    placeholders::references(t)
                        .map(|refs| {
                            refs.iter()
                                .any(|r| *r == Placeholder::Image(component.to_string()))
                        })
                        .unwrap_or(false)
                })
            })
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// The group a component belongs to, or `""` for the implicit default group.
    pub fn group_of(&self, component: &str) -> &str {
        self.components
            .get(component)
            .and_then(|c| c.group.as_deref())
            .unwrap_or("")
    }

    /// Grace period when stopping a component: its own, else the stack's,
    /// else the built-in default.
    pub fn grace_seconds(&self, component: &str) -> u32 {
        self.components
            .get(component)
            .and_then(|c| c.stop.as_ref())
            .and_then(|s| s.grace_seconds)
            .or_else(|| self.stop.as_ref().map(|s| s.grace_seconds))
            .unwrap_or(DEFAULT_GRACE_SECONDS)
    }

    /// Components in dependency order (dependencies first), stable with
    /// respect to the definition order among independent components.
    pub fn dependency_order(&self) -> Result<Vec<String>, Vec<String>> {
        let mut order = Vec::new();
        let mut state: HashMap<&str, u8> = HashMap::new(); // 1 = visiting, 2 = done
        fn visit<'a>(
            stack: &'a StackDef,
            name: &'a str,
            state: &mut HashMap<&'a str, u8>,
            order: &mut Vec<String>,
            path: &mut Vec<String>,
        ) -> Result<(), Vec<String>> {
            match state.get(name) {
                Some(2) => return Ok(()),
                Some(1) => {
                    path.push(name.to_string());
                    return Err(path.clone());
                }
                _ => {}
            }
            state.insert(name, 1);
            path.push(name.to_string());
            if let Some(def) = stack.components.get(name) {
                for dep in &def.depends_on {
                    if let Some((dep_name, _)) = stack.components.get_key_value(dep.as_str()) {
                        visit(stack, dep_name, state, order, path)?;
                    }
                }
            }
            path.pop();
            state.insert(name, 2);
            order.push(name.to_string());
            Ok(())
        }
        for name in self.components.keys() {
            visit(self, name, &mut state, &mut order, &mut Vec::new())?;
        }
        Ok(order)
    }
}

// --------------------------------------------------------------- key rules

/// `[A-Za-z0-9._-]`, 1..=40 chars: safe in a path, an image tag and a
/// compose project name on every platform.
pub fn is_valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= KEY_MAX_LEN
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Force an arbitrary label (a tracker key, a branch) into the key
/// alphabet: every run of other characters becomes one `-`, then the result
/// is cut to the limit. An input with nothing usable becomes `item`.
pub fn sanitize_key(label: &str) -> String {
    let mut out = String::new();
    let mut pending_dash = false;
    for ch in label.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(ch);
        } else {
            pending_dash = true;
        }
    }
    out.truncate(KEY_MAX_LEN);
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "item".to_string()
    } else {
        out
    }
}

// -------------------------------------------------------------- load rules

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} exceeds {MAX_DOC_BYTES} bytes")]
    TooLarge { path: PathBuf },
    #[error("{path}: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("both {PROJECT_FILE_NAME} and {STACK_FILE_NAME} define `stacks:`; keep one")]
    BothDefine,
    #[error("{STACK_FILE_NAME} must have a top-level `stacks:` key")]
    StackFileWithoutStacks,
}

impl StacksDocument {
    /// Parse a stand-alone `stacks:` document.
    pub fn parse(text: &str) -> Result<StacksDocument, String> {
        if is_blank_or_comment_only(text) {
            return Ok(StacksDocument::default());
        }
        serde_yaml::from_str::<StacksDocument>(text).map_err(|e| e.to_string())
    }

    /// Pick the `stacks:` key out of a `chappa.yml` text. `Ok(None)` when
    /// the file has no such key; other keys are not looked at.
    pub fn parse_from_project_file(text: &str) -> Result<Option<StacksDocument>, String> {
        if is_blank_or_comment_only(text) {
            return Ok(None);
        }
        let value: YamlValue = serde_yaml::from_str(text).map_err(|e| e.to_string())?;
        let Some(stacks) = value.get("stacks") else {
            return Ok(None);
        };
        let stacks: IndexMap<String, StackDef> =
            serde_yaml::from_value(stacks.clone()).map_err(|e| format!("stacks: {e}"))?;
        Ok(Some(StacksDocument {
            stacks,
            unknown: IndexMap::new(),
        }))
    }

    /// Where the document is, and what it says. The file rule, in full:
    /// `stacks:` lives in `chappa.yml`; a `chappa.stack.yml` beside it, when
    /// present, *is* the document and `chappa.yml` must not also carry one.
    /// `Ok(None)` when the project defines no stacks at all.
    pub fn load_from_project(dir: &Path) -> Result<Option<(PathBuf, StacksDocument)>, LoadError> {
        let project_path = dir.join(PROJECT_FILE_NAME);
        let stack_path = dir.join(STACK_FILE_NAME);
        let in_project = match read_capped(&project_path)? {
            Some(text) => StacksDocument::parse_from_project_file(&text).map_err(|message| {
                LoadError::Parse {
                    path: project_path.clone(),
                    message,
                }
            })?,
            None => None,
        };
        let in_stack_file = match read_capped(&stack_path)? {
            Some(text) => {
                if is_blank_or_comment_only(&text) {
                    return Err(LoadError::StackFileWithoutStacks);
                }
                let value: YamlValue =
                    serde_yaml::from_str(&text).map_err(|e| LoadError::Parse {
                        path: stack_path.clone(),
                        message: e.to_string(),
                    })?;
                if value.get("stacks").is_none() {
                    return Err(LoadError::StackFileWithoutStacks);
                }
                Some(
                    StacksDocument::parse(&text).map_err(|message| LoadError::Parse {
                        path: stack_path.clone(),
                        message,
                    })?,
                )
            }
            None => None,
        };
        match (in_project, in_stack_file) {
            (Some(_), Some(_)) => Err(LoadError::BothDefine),
            (Some(doc), None) => Ok(Some((project_path, doc))),
            (None, Some(doc)) => Ok(Some((stack_path, doc))),
            (None, None) => Ok(None),
        }
    }
}

fn read_capped(path: &Path) -> Result<Option<String>, LoadError> {
    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(LoadError::Io {
                path: path.to_path_buf(),
                source,
            })
        }
    };
    if meta.len() as usize > MAX_DOC_BYTES {
        return Err(LoadError::TooLarge {
            path: path.to_path_buf(),
        });
    }
    fs::read_to_string(path)
        .map(Some)
        .map_err(|source| LoadError::Io {
            path: path.to_path_buf(),
            source,
        })
}

fn is_blank_or_comment_only(text: &str) -> bool {
    text.lines()
        .map(str::trim)
        .all(|l| l.is_empty() || l.starts_with('#'))
}

// -------------------------------------------------------------- validation

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
}

/// Why a diagnostic was raised. Stable names, so a UI can link each to help.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Rule {
    InvalidKey,
    UnknownKey,
    UnknownComponent,
    UnknownGroup,
    UnknownEngine,
    UnknownMode,
    NoModes,
    DefaultModeMissing,
    DependencyCycle,
    CrossEngineReference,
    SourceModeNeedsRegistry,
    AnonymousPersistenceVolume,
    UnscannedPersistentComponent,
    WorkDirNotIgnored,
    StateDirNotIgnored,
    PortLayout,
    HostnameRequired,
    Placeholder,
    PolicyNotAllowed,
    MissingField,
    EngineOsConflict,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub severity: Severity,
    pub rule: Rule,
    /// Dotted path into the document, e.g. `stacks.app.components.api.depends_on[0]`.
    pub path: String,
    pub message: String,
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sev = match self.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        write!(f, "{sev} at {}: {}", self.path, self.message)
    }
}

/// True when any diagnostic is an error.
pub fn has_errors(diagnostics: &[Diagnostic]) -> bool {
    diagnostics.iter().any(|d| d.severity == Severity::Error)
}

/// What validation may know about the project beyond the document.
#[derive(Debug, Clone, Default)]
pub struct ValidationContext<'a> {
    /// The project's `.gitignore` text, when the caller has it. Without it
    /// the ignore rules are skipped, not failed.
    pub gitignore: Option<&'a str>,
}

/// Every finding across every stack in the document.
pub fn validate(doc: &StacksDocument, ctx: &ValidationContext<'_>) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for key in doc.unknown.keys() {
        out.push(Diagnostic {
            severity: Severity::Warning,
            rule: Rule::UnknownKey,
            path: key.clone(),
            message: format!("unknown top-level key `{key}` (kept, ignored)"),
        });
    }
    for (name, stack) in &doc.stacks {
        validate_stack(name, stack, ctx, &mut out);
    }
    out
}

fn validate_stack(
    stack_name: &str,
    stack: &StackDef,
    ctx: &ValidationContext<'_>,
    out: &mut Vec<Diagnostic>,
) {
    let base = format!("stacks.{stack_name}");
    let mut err = |rule: Rule, path: String, message: String| {
        out.push(Diagnostic {
            severity: Severity::Error,
            rule,
            path,
            message,
        })
    };
    if !is_valid_key(stack_name) {
        err(
            Rule::InvalidKey,
            base.clone(),
            format!("stack name `{stack_name}` must match [A-Za-z0-9._-] and be at most {KEY_MAX_LEN} chars"),
        );
    }
    let mut warnings = Vec::new();
    let mut warn = |rule: Rule, path: String, message: String| {
        warnings.push(Diagnostic {
            severity: Severity::Warning,
            rule,
            path,
            message,
        })
    };
    for key in stack.unknown.keys() {
        warn(
            Rule::UnknownKey,
            format!("{base}.{key}"),
            format!("unknown key `{key}` (kept, ignored)"),
        );
    }
    for (g, def) in &stack.groups {
        if !is_valid_key(g) {
            err(
                Rule::InvalidKey,
                format!("{base}.groups.{g}"),
                format!("group name `{g}` is not a valid key"),
            );
        }
        if let Some(e) = &def.engine {
            match stack.engine_ref(e) {
                None => err(
                    Rule::UnknownEngine,
                    format!("{base}.groups.{g}.engine"),
                    format!("unknown engine `{e}`; use docker, podman, terraform, process, script or a name from `engines:`"),
                ),
                Some(engine) => {
                    // Podman runs Linux containers only, and one daemon is in
                    // one mode at a time, so two groups cannot ask the same
                    // engine for different ones.
                    if engine.kind == EngineKind::Podman && def.os == Some(ContainerOs::Windows) {
                        err(
                            Rule::EngineOsConflict,
                            format!("{base}.groups.{g}.os"),
                            "podman runs Linux containers only".into(),
                        );
                    }
                    if let Some(os) = def.os {
                        if let Some((other, odef)) = stack.groups.iter().find(|(og, o)| {
                            *og != g
                                && o.engine.as_deref() == Some(e.as_str())
                                && o.os.is_some_and(|x| x != os)
                        }) {
                            err(
                                Rule::EngineOsConflict,
                                format!("{base}.groups.{g}.os"),
                                format!(
                                    "group `{other}` needs {} containers from the same engine `{e}`",
                                    odef.os.map(|o| o.as_str()).unwrap_or_default()
                                ),
                            );
                        }
                    }
                }
            }
        }
    }
    if let Some(e) = &stack.engine {
        if stack.engine_ref(e).is_none() {
            err(
                Rule::UnknownEngine,
                format!("{base}.engine"),
                format!("unknown engine `{e}`"),
            );
        }
    }
    for (name, def) in &stack.engines {
        if !is_valid_key(name) {
            err(
                Rule::InvalidKey,
                format!("{base}.engines.{name}"),
                format!("engine name `{name}` is not a valid key"),
            );
        }
        if def.context.is_some() && !matches!(def.kind, EngineKind::Docker | EngineKind::Podman) {
            err(
                Rule::UnknownEngine,
                format!("{base}.engines.{name}.context"),
                "only docker and podman engines take a remote context".to_string(),
            );
        }
    }

    // hostname with tls / relay
    if stack.hostname.is_none() && (stack.tls.is_some() || stack.relay.is_some()) {
        err(
            Rule::HostnameRequired,
            format!("{base}.hostname"),
            "`tls` and `relay` need a stack `hostname`".to_string(),
        );
    }
    if let Some(tls) = &stack.tls {
        for (i, c) in tls.components.iter().enumerate() {
            if !stack.components.contains_key(c) {
                err(
                    Rule::UnknownComponent,
                    format!("{base}.tls.components[{i}]"),
                    format!("unknown component `{c}`"),
                );
            }
        }
    }
    if let Some(relay) = &stack.relay {
        for (i, e) in relay.engines.iter().enumerate() {
            match stack.engine_ref(e) {
                Some(r) if matches!(r.kind, EngineKind::Docker | EngineKind::Podman) => {}
                Some(_) => err(
                    Rule::UnknownEngine,
                    format!("{base}.relay.engines[{i}]"),
                    format!("`{e}` is not a container engine; the relay serves docker and podman"),
                ),
                None => err(
                    Rule::UnknownEngine,
                    format!("{base}.relay.engines[{i}]"),
                    format!("unknown engine `{e}`"),
                ),
            }
        }
    }

    // selections / stop order / placements
    for (sel, members) in &stack.selections {
        if !is_valid_key(sel) {
            err(
                Rule::InvalidKey,
                format!("{base}.selections.{sel}"),
                format!("selection name `{sel}` is not a valid key"),
            );
        }
        for (i, m) in members.iter().enumerate() {
            if !stack.components.contains_key(m) {
                err(
                    Rule::UnknownComponent,
                    format!("{base}.selections.{sel}[{i}]"),
                    format!("unknown component `{m}`"),
                );
            }
        }
    }
    if let Some(stop) = &stack.stop {
        for (i, m) in stop.order.iter().enumerate() {
            if !stack.components.contains_key(m) {
                err(
                    Rule::UnknownComponent,
                    format!("{base}.stop.order[{i}]"),
                    format!("unknown component `{m}`"),
                );
            }
        }
    }
    for (slot, placement) in &stack.slots.assign {
        if !stack.slots.placements.contains_key(placement) {
            err(
                Rule::UnknownKey,
                format!("{base}.slots.assign.{slot}"),
                format!("unknown placement `{placement}`"),
            );
        }
        if *slot >= stack.slots.count {
            err(
                Rule::PortLayout,
                format!("{base}.slots.assign.{slot}"),
                format!("slot {slot} is outside `count: {}`", stack.slots.count),
            );
        }
    }
    for (p, def) in &stack.slots.placements {
        for g in def.engines.keys() {
            if !stack.groups.contains_key(g) {
                err(
                    Rule::UnknownGroup,
                    format!("{base}.slots.placements.{p}.engines.{g}"),
                    format!("unknown group `{g}`"),
                );
            }
        }
    }

    // policies: data safety accepts warn|block-agents|stop|block-up, secrets warn|block-agents|block-build
    if let Some(ds) = &stack.data_safety {
        if !ds.on_finding.allowed_for_data() {
            err(
                Rule::PolicyNotAllowed,
                format!("{base}.dataSafety.onFinding"),
                format!(
                    "`{}` is not a data-safety policy (warn, block-agents, stop, block-up)",
                    ds.on_finding
                ),
            );
        }
        if !matches!(ds.unscanned, Policy::Warn | Policy::BlockAgents) {
            err(
                Rule::PolicyNotAllowed,
                format!("{base}.dataSafety.unscanned"),
                format!(
                    "`{}` is not an unscanned policy (warn, block-agents)",
                    ds.unscanned
                ),
            );
        }
    }
    if let Some(s) = &stack.secrets {
        if !s.on_finding.allowed_for_secrets() {
            err(
                Rule::PolicyNotAllowed,
                format!("{base}.secrets.onFinding"),
                format!(
                    "`{}` is not a secret-scan policy (warn, block-agents, block-build)",
                    s.on_finding
                ),
            );
        }
    }

    // components
    let terraform_components: Vec<&String> = stack
        .components
        .iter()
        .filter(|(_, c)| c.driver == DriverKind::Terraform)
        .map(|(n, _)| n)
        .collect();
    for (name, c) in &stack.components {
        let cpath = format!("{base}.components.{name}");
        if !is_valid_key(name) {
            err(
                Rule::InvalidKey,
                cpath.clone(),
                format!("component name `{name}` must match [A-Za-z0-9._-] and be at most {KEY_MAX_LEN} chars"),
            );
        }
        for key in c.unknown.keys() {
            warn(
                Rule::UnknownKey,
                format!("{cpath}.{key}"),
                format!("unknown key `{key}` (kept, ignored)"),
            );
        }
        if let Some(g) = &c.group {
            if !stack.groups.is_empty() && !stack.groups.contains_key(g) {
                warn(
                    Rule::UnknownGroup,
                    format!("{cpath}.group"),
                    format!(
                        "group `{g}` is not declared under `groups:`; it gets the stack defaults"
                    ),
                );
            }
        }
        if let Some(e) = &c.engine {
            if stack.engine_ref(e).is_none() {
                err(
                    Rule::UnknownEngine,
                    format!("{cpath}.engine"),
                    format!("unknown engine `{e}`"),
                );
            }
        }
        // driver-specific required fields
        match c.driver {
            DriverKind::Compose => {
                if c.file.is_none() {
                    err(
                        Rule::MissingField,
                        format!("{cpath}.file"),
                        "compose components need `file`".into(),
                    );
                }
            }
            DriverKind::Process => {
                let has_dev_command = c.mode(Mode::Dev).and_then(|m| m.command.as_ref()).is_some();
                if c.command.is_none() && !has_dev_command {
                    err(
                        Rule::MissingField,
                        format!("{cpath}.command"),
                        "process components need `command` (or `modes.dev.command`)".into(),
                    );
                }
            }
            DriverKind::Terraform => {
                if c.dir.is_none() {
                    err(
                        Rule::MissingField,
                        format!("{cpath}.dir"),
                        "terraform components need `dir`".into(),
                    );
                }
            }
            DriverKind::Script => {
                if c.up.is_none() {
                    err(
                        Rule::MissingField,
                        format!("{cpath}.up"),
                        "script components need at least `up`".into(),
                    );
                }
            }
        }
        // modes
        for (mode_name, m) in &c.modes {
            let mpath = format!("{cpath}.modes.{mode_name}");
            match Mode::parse(mode_name) {
                None => err(
                    Rule::UnknownMode,
                    mpath.clone(),
                    format!("unknown mode `{mode_name}` (prebuilt, source, dev)"),
                ),
                Some(Mode::Prebuilt) if m.image.is_none() => err(
                    Rule::MissingField,
                    format!("{mpath}.image"),
                    "prebuilt mode needs `image`".into(),
                ),
                Some(Mode::Source) | Some(Mode::Dev) if m.repo.is_none() => err(
                    Rule::MissingField,
                    format!("{mpath}.repo"),
                    format!("{mode_name} mode needs `repo`"),
                ),
                _ => {}
            }
            for key in m.unknown.keys() {
                warn(
                    Rule::UnknownKey,
                    format!("{mpath}.{key}"),
                    format!("unknown key `{key}` (kept, ignored)"),
                );
            }
        }
        if let Some(d) = c.default {
            if !c.has_mode(d) {
                err(
                    Rule::DefaultModeMissing,
                    format!("{cpath}.default"),
                    format!("default mode `{d}` is not declared under `modes`"),
                );
            }
        }
        if c.modes.is_empty() && matches!(c.driver, DriverKind::Compose | DriverKind::Process) {
            err(
                Rule::NoModes,
                format!("{cpath}.modes"),
                format!("{} components need at least one mode", c.driver),
            );
        }
        // depends_on
        for (i, dep) in c.depends_on.iter().enumerate() {
            if !stack.components.contains_key(dep) {
                err(
                    Rule::UnknownComponent,
                    format!("{cpath}.depends_on[{i}]"),
                    format!("unknown component `{dep}`"),
                );
            } else if dep == name {
                err(
                    Rule::DependencyCycle,
                    format!("{cpath}.depends_on[{i}]"),
                    "a component cannot depend on itself".into(),
                );
            }
        }
        // placeholders in templates and hooks
        let known: HashSet<&str> = stack.components.keys().map(|k| k.as_str()).collect();
        for (suffix, template) in c.templates() {
            check_template(template, &known, &format!("{cpath}.{suffix}"), &mut err);
        }
        for (suffix, argv) in c.hooks() {
            for (i, word) in argv.iter().enumerate() {
                check_template(word, &known, &format!("{cpath}.{suffix}[{i}]"), &mut err);
            }
        }
        // persistence
        if let Some(p) = &c.persistence {
            if p.volume.as_deref().is_none_or(|v| v.trim().is_empty()) {
                err(
                    Rule::AnonymousPersistenceVolume,
                    format!("{cpath}.persistence.volume"),
                    "persistence needs a named volume; an anonymous one dies with the container"
                        .into(),
                );
            }
            if c.data_safety.is_none() {
                warn(
                    Rule::UnscannedPersistentComponent,
                    format!("{cpath}.dataSafety"),
                    "persistent component has no data-safety check; its data counts as sensitive"
                        .into(),
                );
            }
        }
        if let Some(d) = &c.data {
            if d.strategy == DataStrategy::Snapshot && c.persistence.is_none() {
                err(
                    Rule::MissingField,
                    format!("{cpath}.persistence"),
                    "`data.strategy: snapshot` needs `persistence`".into(),
                );
            }
        }
        // source mode feeding terraform without a registry
        if c.has_mode(Mode::Source) && stack.registry.is_none() {
            for tf in &terraform_components {
                let feeds = stack.components[tf.as_str()]
                    .templates()
                    .iter()
                    .any(|(_, t)| {
                        placeholders::references(t)
                            .map(|r| r.iter().any(|p| *p == Placeholder::Image(name.clone())))
                            .unwrap_or(false)
                    });
                if feeds {
                    err(
                        Rule::SourceModeNeedsRegistry,
                        format!("{cpath}.modes.source"),
                        format!("source mode feeds terraform component `{tf}`; a locally built image needs `registry:` on the stack to be pushed"),
                    );
                }
            }
        }
    }

    // cycles
    if let Err(path) = stack.dependency_order() {
        err(
            Rule::DependencyCycle,
            format!("{base}.components"),
            format!("depends_on cycle: {}", path.join(" -> ")),
        );
    }

    // cross-engine references
    for (name, c) in &stack.components {
        let Some(my_engine) = stack.engine_name_for(name) else {
            continue;
        };
        for (i, dep) in c.depends_on.iter().enumerate() {
            let Some(dep_engine) = stack.engine_name_for(dep) else {
                continue;
            };
            if dep_engine == my_engine {
                continue;
            }
            for (suffix, t) in c.templates() {
                if references_by_service_name(t, dep) || port_without_host(t, dep) {
                    err(
                        Rule::CrossEngineReference,
                        format!("{base}.components.{name}.{suffix}"),
                        format!(
                            "`{dep}` runs on engine `{dep_engine}`, `{name}` on `{my_engine}`; engines share no network, so reach it as `{{host}}:{{port:{dep}}}` (path depends_on[{i}])"
                        ),
                    );
                }
            }
        }
    }

    // ports
    match ports::layout(stack) {
        Ok(_) => {}
        Err(e) => err(Rule::PortLayout, format!("{base}.slots"), e.to_string()),
    }

    // gitignore
    if let Some(ignore) = ctx.gitignore {
        let work = stack.work_dir.to_string_lossy().replace('\\', "/");
        if !gitignore_covers(ignore, &work) {
            warn(
                Rule::WorkDirNotIgnored,
                format!("{base}.workDir"),
                format!("`{work}/` is not in .gitignore; item worktrees, job logs and snapshots would be committed"),
            );
        }
        if !terraform_components.is_empty() && !gitignore_covers(ignore, DEFAULT_STATE_DIR) {
            warn(
                Rule::StateDirNotIgnored,
                format!("{base}.components"),
                format!("`{DEFAULT_STATE_DIR}/` is not in .gitignore; local terraform state would be committed"),
            );
        }
    }
    out.extend(warnings);
}

fn check_template(
    template: &str,
    known: &HashSet<&str>,
    path: &str,
    err: &mut impl FnMut(Rule, String, String),
) {
    match placeholders::references(template) {
        Err(e) => err(Rule::Placeholder, path.to_string(), e.to_string()),
        Ok(refs) => {
            for r in refs {
                if let Some(c) = r.component() {
                    if !known.contains(c) {
                        err(
                            Rule::UnknownComponent,
                            path.to_string(),
                            format!("`{r}` names unknown component `{c}`"),
                        );
                    }
                }
            }
        }
    }
}

/// `scheme://dep:port`, `scheme://dep/`, `user@dep:` — a bare service name
/// as a URL host, which only resolves inside the same engine's network.
fn references_by_service_name(template: &str, dep: &str) -> bool {
    for marker in ["://", "@"] {
        let mut rest = template;
        while let Some(idx) = rest.find(marker) {
            let after = &rest[idx + marker.len()..];
            if let Some(tail) = after.strip_prefix(dep) {
                if tail.is_empty() || tail.starts_with([':', '/']) {
                    return true;
                }
            }
            rest = after;
        }
    }
    false
}

/// `{port:dep}` in a value with neither `{host}` nor `{hostname}` beside it.
fn port_without_host(template: &str, dep: &str) -> bool {
    let Ok(refs) = placeholders::references(template) else {
        return false;
    };
    let mentions_port = refs
        .iter()
        .any(|r| matches!(r, Placeholder::Port { component, .. } if component == dep));
    let has_host = refs
        .iter()
        .any(|r| matches!(r, Placeholder::Host | Placeholder::Hostname));
    mentions_port && !has_host
}

/// A `.gitignore` line that covers `dir`: the dir itself, with or without
/// a leading `/` or trailing `/`, or any of its parents.
fn gitignore_covers(gitignore: &str, dir: &str) -> bool {
    let dir = dir.trim_matches('/');
    let mut parents: Vec<&str> = Vec::new();
    let mut end = dir.len();
    loop {
        parents.push(&dir[..end]);
        match dir[..end].rfind('/') {
            Some(i) => end = i,
            None => break,
        }
    }
    gitignore
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with('!'))
        .map(|l| l.trim_start_matches('/').trim_end_matches('/'))
        .any(|l| parents.iter().any(|p| *p == l || format!("**/{p}") == l))
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) const EXAMPLE: &str = r#"
stacks:
  app:
    slots: { count: 4, portBase: 14000, portStride: 100 }
    engine: docker
    hostname: dev.example.test
    registry: registry.example/dev
    groups:
      core: { engine: docker }
      apis: { engine: podman }
    selections:
      linux-only: [db, api]
    stop: { order: [db], graceSeconds: 20 }
    provenance:
      labels:
        commit: [example.vendor.sha]
    dataSafety:
      rules: stack/data-safety.yml
      onFinding: block-agents
    components:
      db:
        group: apis
        driver: compose
        file: stack/db.compose.yml
        service: db
        modes: { prebuilt: { image: "registry.example/db", tag: "{channel}" } }
        persistence:
          volume: db-data
          snapshot:
            create: [docker, exec, "{container}", /scripts/snapshot.sh, "{snapshot}"]
        dataSafety:
          marker: [docker, exec, "{container}", /scripts/scrub-marker.sh]
          sample: [docker, exec, "{container}", /scripts/sample.sh]
          when: [after-up, after-restore, every: 6h]
        stop: { graceSeconds: 30 }
        memoryMb: 1024
      api:
        group: apis
        driver: compose
        file: stack/api.compose.yml
        service: api
        modes:
          prebuilt: { image: "registry.example/api", tag: "{channel}" }
          source:   { repo: ../api, context: ., dockerfile: Dockerfile }
        default: prebuilt
        depends_on: [db]
        env: { DB_URL: "postgres://{host}:{port:db}/app" }
        probe: { url: "http://{hostname}:{port:api}/health/ready", rule: health }
        outputs: { url: "http://{hostname}:{port:api}" }
        memoryMb: 512
      web:
        group: core
        driver: process
        command: web-dev
        modes: { dev: { repo: ../web } }
        depends_on: [api]
        env: { API_URL: "http://{hostname}:{port:api}" }
        probe: { url: "http://{hostname}:{port:web}/", rule: web }
      migrate:
        group: apis
        driver: compose
        file: stack/api.compose.yml
        service: migrate
        kind: oneshot
        ports: 0
        modes: { prebuilt: { image: "registry.example/api", tag: "{channel}" } }
        depends_on: [db]
      worker:
        group: apis
        driver: compose
        file: stack/api.compose.yml
        service: worker
        normallyOff: true
        modes: { prebuilt: { image: "registry.example/api", tag: "{channel}" } }
        depends_on: [db]
      infra:
        group: cloud
        driver: terraform
        dir: stack/infra
        vars: { image_api: "{image:api}" }
        ttlHours: 8
"#;

    pub(crate) fn example() -> StacksDocument {
        StacksDocument::parse(EXAMPLE).expect("example parses")
    }

    pub(crate) fn example_stack() -> StackDef {
        example().stacks["app"].clone()
    }

    fn errors(doc: &StacksDocument) -> Vec<Diagnostic> {
        validate(doc, &ValidationContext::default())
            .into_iter()
            .filter(|d| d.severity == Severity::Error)
            .collect()
    }

    fn by_rule(diags: &[Diagnostic], rule: Rule) -> Vec<&Diagnostic> {
        diags.iter().filter(|d| d.rule == rule).collect()
    }

    #[test]
    fn example_parses_with_defaults_and_order() {
        let stack = example_stack();
        assert_eq!(stack.slots.count, 4);
        assert_eq!(stack.slots.port_base, 14000);
        assert_eq!(stack.slots.port_stride, 100);
        assert_eq!(stack.work_dir, PathBuf::from(".work"));
        let names: Vec<&String> = stack.components.keys().collect();
        assert_eq!(names, ["db", "api", "web", "migrate", "worker", "infra"]);
        let api = &stack.components["api"];
        assert_eq!(api.default, Some(Mode::Prebuilt));
        assert_eq!(api.depends_on, vec!["db"]);
        assert_eq!(api.probe.as_ref().unwrap().rule, ProbeRule::Health);
        assert_eq!(api.kind, ComponentKind::Service);
        assert!(!api.normally_off);
        assert_eq!(stack.components["migrate"].kind, ComponentKind::Oneshot);
        assert!(stack.components["worker"].normally_off);
        let db = &stack.components["db"];
        assert_eq!(
            db.persistence.as_ref().unwrap().volume.as_deref(),
            Some("db-data")
        );
        assert_eq!(
            db.persistence.as_ref().unwrap().location,
            SnapshotLocation::InVolume
        );
        assert_eq!(db.persistence.as_ref().unwrap().keep, 10);
        let when = &db.data_safety.as_ref().unwrap().when;
        assert_eq!(
            when,
            &vec![
                ScanTrigger::Named("after-up".into()),
                ScanTrigger::Named("after-restore".into()),
                ScanTrigger::Every { every: "6h".into() },
            ]
        );
        assert_eq!(
            stack.data_safety.as_ref().unwrap().on_finding,
            Policy::BlockAgents
        );
        assert_eq!(stack.data_safety.as_ref().unwrap().unscanned, Policy::Warn);
        assert_eq!(stack.stop.as_ref().unwrap().order, vec!["db"]);
        assert_eq!(
            stack.provenance.as_ref().unwrap().labels.commit,
            vec!["example.vendor.sha"]
        );
    }

    #[test]
    fn example_validates_clean_of_errors_and_warns_only_about_the_undeclared_group() {
        let diags = validate(&example(), &ValidationContext::default());
        let errs: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .collect();
        assert!(errs.is_empty(), "{errs:#?}");
        let warns: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == Severity::Warning)
            .collect();
        assert_eq!(warns.len(), 1, "{warns:#?}");
        assert_eq!(warns[0].rule, Rule::UnknownGroup);
        assert_eq!(warns[0].path, "stacks.app.components.infra.group");
    }

    #[test]
    fn blank_document_is_empty_not_an_error() {
        assert_eq!(
            StacksDocument::parse("").unwrap(),
            StacksDocument::default()
        );
        assert_eq!(
            StacksDocument::parse("# nothing\n\n").unwrap(),
            StacksDocument::default()
        );
        assert_eq!(
            StacksDocument::parse_from_project_file("# nothing").unwrap(),
            None
        );
    }

    #[test]
    fn project_file_without_stacks_key_yields_none() {
        let text = "name: demo\nprocesses:\n  dev:\n    command: npm start\n";
        assert_eq!(StacksDocument::parse_from_project_file(text).unwrap(), None);
    }

    #[test]
    fn project_file_with_stacks_key_ignores_the_other_keys() {
        let text = format!("name: demo\nprocesses:\n  dev:\n    command: npm start\n{EXAMPLE}");
        let doc = StacksDocument::parse_from_project_file(&text)
            .unwrap()
            .unwrap();
        assert!(doc.unknown.is_empty());
        assert_eq!(doc.stacks.len(), 1);
    }

    #[test]
    fn component_without_driver_fails_to_parse() {
        let text = "stacks:\n  app:\n    components:\n      api: { file: x.yml }\n";
        let e = StacksDocument::parse(text).unwrap_err();
        assert!(e.contains("driver"), "{e}");
    }

    #[test]
    fn unknown_keys_survive_round_trip_and_warn() {
        let text = "stacks:\n  app:\n    components:\n      api:\n        driver: compose\n        file: a.yml\n        modes: { prebuilt: { image: img } }\n        normalyOff: true\n";
        let doc = StacksDocument::parse(text).unwrap();
        let api = &doc.stacks["app"].components["api"];
        assert!(api.unknown.contains_key("normalyOff"));
        let diags = validate(&doc, &ValidationContext::default());
        let w = by_rule(&diags, Rule::UnknownKey);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].path, "stacks.app.components.api.normalyOff");
        assert_eq!(w[0].severity, Severity::Warning);
        let back = serde_yaml::to_string(&doc).unwrap();
        assert!(back.contains("normalyOff: true"));
        assert_eq!(StacksDocument::parse(&back).unwrap(), doc);
    }

    #[test]
    fn depends_on_unknown_component_has_indexed_path() {
        let mut doc = example();
        doc.stacks["app"].components["api"].depends_on = vec!["db".into(), "cache".into()];
        let errs = errors(&doc);
        let e = by_rule(&errs, Rule::UnknownComponent);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].path, "stacks.app.components.api.depends_on[1]");
        assert!(e[0].message.contains("`cache`"));
    }

    #[test]
    fn dependency_cycle_is_reported_once_with_the_path() {
        let mut doc = example();
        doc.stacks["app"].components["db"].depends_on = vec!["web".into()];
        let errs = errors(&doc);
        let e = by_rule(&errs, Rule::DependencyCycle);
        assert_eq!(e.len(), 1);
        assert!(
            e[0].message.contains("db -> web -> api -> db"),
            "{}",
            e[0].message
        );
    }

    #[test]
    fn self_dependency_is_a_cycle() {
        let mut doc = example();
        doc.stacks["app"].components["db"].depends_on = vec!["db".into()];
        let errs = errors(&doc);
        assert!(!by_rule(&errs, Rule::DependencyCycle).is_empty());
    }

    #[test]
    fn dependency_order_puts_dependencies_first_and_keeps_definition_order() {
        let order = example_stack().dependency_order().unwrap();
        assert_eq!(order, ["db", "api", "web", "migrate", "worker", "infra"]);
    }

    #[test]
    fn cross_engine_reference_by_service_name_is_refused_and_host_url_accepted() {
        // web (docker) depends on api (podman).
        let mut doc = example();
        doc.stacks["app"].components["web"]
            .env
            .insert("API_URL".into(), "http://api:8080".into());
        let errs = errors(&doc);
        let e = by_rule(&errs, Rule::CrossEngineReference);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].path, "stacks.app.components.web.env.API_URL");
        assert!(
            e[0].message.contains("{host}:{port:api}"),
            "{}",
            e[0].message
        );

        let mut doc = example();
        doc.stacks["app"].components["web"]
            .env
            .insert("API_URL".into(), "http://{host}:{port:api}".into());
        assert!(by_rule(&errors(&doc), Rule::CrossEngineReference).is_empty());
    }

    #[test]
    fn cross_engine_port_without_host_is_refused() {
        let mut doc = example();
        doc.stacks["app"].components["web"]
            .env
            .insert("API_PORT_URL".into(), "http://api-host:{port:api}".into());
        let errs = errors(&doc);
        assert_eq!(by_rule(&errs, Rule::CrossEngineReference).len(), 1);
    }

    #[test]
    fn same_engine_service_name_reference_is_fine() {
        // api and db are both podman.
        let mut doc = example();
        doc.stacks["app"].components["api"]
            .env
            .insert("DB_URL".into(), "postgres://db:5432/app".into());
        assert!(by_rule(&errors(&doc), Rule::CrossEngineReference).is_empty());
    }

    #[test]
    fn service_name_matching_is_exact() {
        assert!(references_by_service_name("http://api:80", "api"));
        assert!(references_by_service_name("http://api/", "api"));
        assert!(references_by_service_name("http://api", "api"));
        assert!(references_by_service_name("postgres://u:p@db:5432/x", "db"));
        assert!(!references_by_service_name("http://api-gateway:80", "api"));
        assert!(!references_by_service_name("http://myapi:80", "api"));
        assert!(!references_by_service_name("api", "api"));
    }

    #[test]
    fn source_mode_feeding_terraform_needs_registry() {
        let mut doc = example();
        doc.stacks["app"].registry = None;
        let errs = errors(&doc);
        let e = by_rule(&errs, Rule::SourceModeNeedsRegistry);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].path, "stacks.app.components.api.modes.source");
        assert!(e[0].message.contains("`infra`"));
        // db has no source mode: not flagged even without a registry.
        assert!(!errs
            .iter()
            .any(|d| d.path.starts_with("stacks.app.components.db.modes")));
    }

    #[test]
    fn anonymous_persistence_volume_is_an_error() {
        for volume in [None, Some("".to_string()), Some("  ".to_string())] {
            let mut doc = example();
            doc.stacks["app"].components["db"]
                .persistence
                .as_mut()
                .unwrap()
                .volume = volume.clone();
            let errs = errors(&doc);
            let e = by_rule(&errs, Rule::AnonymousPersistenceVolume);
            assert_eq!(e.len(), 1, "{volume:?}");
            assert_eq!(e[0].path, "stacks.app.components.db.persistence.volume");
        }
    }

    #[test]
    fn persistent_component_without_data_safety_warns() {
        let mut doc = example();
        doc.stacks["app"].components["db"].data_safety = None;
        let diags = validate(&doc, &ValidationContext::default());
        let w = by_rule(&diags, Rule::UnscannedPersistentComponent);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].severity, Severity::Warning);
        assert_eq!(w[0].path, "stacks.app.components.db.dataSafety");
    }

    #[test]
    fn gitignore_rules_warn_only_when_text_is_given() {
        let doc = example();
        let none = validate(&doc, &ValidationContext::default());
        assert!(by_rule(&none, Rule::WorkDirNotIgnored).is_empty());
        assert!(by_rule(&none, Rule::StateDirNotIgnored).is_empty());

        let empty = validate(
            &doc,
            &ValidationContext {
                gitignore: Some("node_modules/\n"),
            },
        );
        assert_eq!(by_rule(&empty, Rule::WorkDirNotIgnored).len(), 1);
        assert_eq!(by_rule(&empty, Rule::StateDirNotIgnored).len(), 1);

        let table = [
            ".work/\n.chappa/stack-state/\n",
            "/.work\n/.chappa/stack-state\n",
            ".work\n.chappa/\n",
            "**/.work\n.chappa\n",
        ];
        for ignore in table {
            let diags = validate(
                &doc,
                &ValidationContext {
                    gitignore: Some(ignore),
                },
            );
            assert!(
                by_rule(&diags, Rule::WorkDirNotIgnored).is_empty(),
                "{ignore:?}"
            );
            assert!(
                by_rule(&diags, Rule::StateDirNotIgnored).is_empty(),
                "{ignore:?}"
            );
        }
        // A negation does not count as covering.
        let diags = validate(
            &doc,
            &ValidationContext {
                gitignore: Some("!.work/\n"),
            },
        );
        assert_eq!(by_rule(&diags, Rule::WorkDirNotIgnored).len(), 1);
    }

    #[test]
    fn state_dir_warning_only_with_terraform_components() {
        let mut doc = example();
        doc.stacks["app"].components.shift_remove("infra");
        let diags = validate(
            &doc,
            &ValidationContext {
                gitignore: Some(".work/\n"),
            },
        );
        assert!(by_rule(&diags, Rule::StateDirNotIgnored).is_empty());
    }

    #[test]
    fn duplicate_port_index_and_small_stride_are_errors() {
        let mut doc = example();
        doc.stacks["app"].components["api"].port_index = Some(0);
        doc.stacks["app"].components["db"].port_index = Some(0);
        let errs = errors(&doc);
        let e = by_rule(&errs, Rule::PortLayout);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].path, "stacks.app.slots");
        assert!(e[0].message.contains("index 0"), "{}", e[0].message);

        let mut doc = example();
        doc.stacks["app"].slots.port_stride = 2; // db, api, web need 3
        let errs = errors(&doc);
        let e = by_rule(&errs, Rule::PortLayout);
        assert_eq!(e.len(), 1);
        assert!(e[0].message.contains("stride"), "{}", e[0].message);
    }

    #[test]
    fn hostname_required_with_tls_or_relay() {
        for (tls, relay) in [(true, false), (false, true)] {
            let mut doc = example();
            let stack = &mut doc.stacks["app"];
            stack.hostname = None;
            stack.tls = tls.then(|| TlsDef {
                mode: TlsMode::SelfSigned,
                components: vec!["api".into()],
            });
            stack.relay = relay.then(|| RelayDef {
                engines: vec!["docker".into()],
            });
            let errs = errors(&doc);
            let e = by_rule(&errs, Rule::HostnameRequired);
            assert_eq!(e.len(), 1, "tls={tls} relay={relay}");
            assert_eq!(e[0].path, "stacks.app.hostname");
        }
        let mut doc = example();
        doc.stacks["app"].tls = Some(TlsDef {
            mode: TlsMode::LocalCa,
            components: vec!["api".into()],
        });
        doc.stacks["app"].relay = Some(RelayDef {
            engines: vec!["docker".into()],
        });
        assert!(by_rule(&errors(&doc), Rule::HostnameRequired).is_empty());
    }

    #[test]
    fn tls_and_relay_members_are_checked() {
        let mut doc = example();
        doc.stacks["app"].tls = Some(TlsDef {
            mode: TlsMode::SelfSigned,
            components: vec!["nope".into()],
        });
        doc.stacks["app"].relay = Some(RelayDef {
            engines: vec!["terraform".into(), "mystery".into()],
        });
        let errs = errors(&doc);
        assert_eq!(
            by_rule(&errs, Rule::UnknownComponent)
                .iter()
                .filter(|d| d.path == "stacks.app.tls.components[0]")
                .count(),
            1
        );
        let eng = by_rule(&errs, Rule::UnknownEngine);
        assert_eq!(eng.len(), 2);
        assert_eq!(eng[0].path, "stacks.app.relay.engines[0]");
        assert_eq!(eng[1].path, "stacks.app.relay.engines[1]");
    }

    #[test]
    fn key_rules_at_the_limits() {
        assert!(is_valid_key("a"));
        assert!(is_valid_key("A-z.0_9"));
        assert!(is_valid_key(&"x".repeat(40)));
        assert!(!is_valid_key(&"x".repeat(41)));
        assert!(!is_valid_key(""));
        assert!(!is_valid_key("has space"));
        assert!(!is_valid_key("slash/y"));
        assert!(!is_valid_key("ünïcode"));

        assert_eq!(sanitize_key("KEY-12"), "KEY-12");
        assert_eq!(sanitize_key("feature/some thing!"), "feature-some-thing");
        assert_eq!(sanitize_key("  ***  "), "item");
        assert_eq!(sanitize_key(&"a".repeat(50)).len(), 40);
        assert_eq!(
            sanitize_key(&format!("{}-x", "a".repeat(39))),
            "a".repeat(39)
        );
        assert!(is_valid_key(&sanitize_key("🚀 launch/it")));
    }

    #[test]
    fn invalid_names_are_flagged_at_every_level() {
        let text = "stacks:\n  \"bad name\":\n    groups: { \"g/1\": {} }\n    selections: { \"s 1\": [] }\n    components:\n      \"c:1\":\n        driver: script\n        up: [run]\n";
        let doc = StacksDocument::parse(text).unwrap();
        let errs = errors(&doc);
        let paths: Vec<&str> = by_rule(&errs, Rule::InvalidKey)
            .iter()
            .map(|d| d.path.as_str())
            .collect();
        assert_eq!(
            paths,
            [
                "stacks.bad name",
                "stacks.bad name.groups.g/1",
                "stacks.bad name.selections.s 1",
                "stacks.bad name.components.c:1"
            ]
        );
    }

    #[test]
    fn placeholder_errors_carry_the_template_path() {
        let mut doc = example();
        doc.stacks["app"].components["api"]
            .outputs
            .insert("bad".into(), "http://{hostnme}".into());
        doc.stacks["app"].components["api"]
            .outputs
            .insert("gone".into(), "{port:cache}".into());
        doc.stacks["app"].components["db"]
            .data_safety
            .as_mut()
            .unwrap()
            .sample = Some(vec!["docker".into(), "exec".into(), "{contaner}".into()]);
        let errs = errors(&doc);
        let p = by_rule(&errs, Rule::Placeholder);
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].path, "stacks.app.components.db.dataSafety.sample[2]");
        assert_eq!(p[1].path, "stacks.app.components.api.outputs.bad");
        assert!(p[1].message.contains("hostnme"));
        let u = by_rule(&errs, Rule::UnknownComponent);
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].path, "stacks.app.components.api.outputs.gone");
    }

    #[test]
    fn mode_rules() {
        let mut doc = example();
        let api = &mut doc.stacks["app"].components["api"];
        api.modes.insert("staging".into(), ModeDef::default());
        api.default = Some(Mode::Dev);
        let errs = errors(&doc);
        assert_eq!(
            by_rule(&errs, Rule::UnknownMode)[0].path,
            "stacks.app.components.api.modes.staging"
        );
        assert_eq!(
            by_rule(&errs, Rule::DefaultModeMissing)[0].path,
            "stacks.app.components.api.default"
        );

        let mut doc = example();
        doc.stacks["app"].components["api"].modes.clear();
        doc.stacks["app"].components["api"].default = None;
        let errs = errors(&doc);
        assert_eq!(
            by_rule(&errs, Rule::NoModes)[0].path,
            "stacks.app.components.api.modes"
        );

        let mut doc = example();
        doc.stacks["app"].components["api"].modes["prebuilt"].image = None;
        doc.stacks["app"].components["api"].modes["source"].repo = None;
        let errs = errors(&doc);
        let m: Vec<&str> = by_rule(&errs, Rule::MissingField)
            .iter()
            .map(|d| d.path.as_str())
            .collect();
        assert_eq!(
            m,
            [
                "stacks.app.components.api.modes.prebuilt.image",
                "stacks.app.components.api.modes.source.repo"
            ]
        );
    }

    #[test]
    fn driver_required_fields() {
        let text = "stacks:\n  app:\n    components:\n      a: { driver: compose, modes: { prebuilt: { image: i } } }\n      b: { driver: process, modes: { dev: { repo: ../b } } }\n      c: { driver: terraform }\n      d: { driver: script }\n";
        let doc = StacksDocument::parse(text).unwrap();
        let errs = errors(&doc);
        let m: Vec<&str> = by_rule(&errs, Rule::MissingField)
            .iter()
            .map(|d| d.path.as_str())
            .collect();
        assert_eq!(
            m,
            [
                "stacks.app.components.a.file",
                "stacks.app.components.b.command",
                "stacks.app.components.c.dir",
                "stacks.app.components.d.up"
            ]
        );
    }

    #[test]
    fn policies_are_checked_per_use() {
        let mut doc = example();
        doc.stacks["app"].data_safety.as_mut().unwrap().on_finding = Policy::BlockBuild;
        doc.stacks["app"].data_safety.as_mut().unwrap().unscanned = Policy::Stop;
        doc.stacks["app"].secrets = Some(SecretsDef {
            on_finding: Policy::BlockUp,
        });
        let errs = errors(&doc);
        let p: Vec<&str> = by_rule(&errs, Rule::PolicyNotAllowed)
            .iter()
            .map(|d| d.path.as_str())
            .collect();
        assert_eq!(
            p,
            [
                "stacks.app.dataSafety.onFinding",
                "stacks.app.dataSafety.unscanned",
                "stacks.app.secrets.onFinding"
            ]
        );
    }

    #[test]
    fn engine_resolution_precedence() {
        let mut stack = example_stack();
        assert_eq!(stack.engine_name_for("api").as_deref(), Some("podman"));
        assert_eq!(stack.engine_name_for("web").as_deref(), Some("docker"));
        assert_eq!(stack.engine_name_for("infra").as_deref(), Some("terraform"));
        assert_eq!(
            stack.engine_for("infra").unwrap().kind,
            EngineKind::Terraform
        );
        // component override beats group
        stack.components["api"].engine = Some("docker".into());
        assert_eq!(stack.engine_name_for("api").as_deref(), Some("docker"));
        // stack default applies to compose components without a group engine
        stack.components["api"].engine = None;
        stack.components["api"].group = None;
        assert_eq!(stack.engine_name_for("api").as_deref(), Some("docker"));
        stack.engine = Some("podman".into());
        assert_eq!(stack.engine_name_for("api").as_deref(), Some("podman"));
        // ... but not to a process component
        stack.components["web"].group = None;
        assert_eq!(stack.engine_name_for("web").as_deref(), Some("process"));
        // declared engines resolve by name with their context
        stack.engines.insert(
            "remote-linux".into(),
            EngineDef {
                kind: EngineKind::Podman,
                context: Some("slot-host".into()),
            },
        );
        stack.components["api"].engine = Some("remote-linux".into());
        let r = stack.engine_for("api").unwrap();
        assert_eq!(r.kind, EngineKind::Podman);
        assert_eq!(r.context.as_deref(), Some("slot-host"));
        assert_eq!(stack.engine_name_for("missing"), None);
    }

    #[test]
    fn unknown_engine_names_are_errors() {
        let mut doc = example();
        doc.stacks["app"].groups["apis"].engine = Some("lxd".into());
        doc.stacks["app"].components["web"].engine = Some("vm".into());
        doc.stacks["app"].engines.insert(
            "tf-remote".into(),
            EngineDef {
                kind: EngineKind::Terraform,
                context: Some("x".into()),
            },
        );
        let errs = errors(&doc);
        let p: Vec<&str> = by_rule(&errs, Rule::UnknownEngine)
            .iter()
            .map(|d| d.path.as_str())
            .collect();
        assert_eq!(
            p,
            [
                "stacks.app.groups.apis.engine",
                "stacks.app.engines.tf-remote.context",
                "stacks.app.components.web.engine"
            ]
        );
    }

    #[test]
    fn image_dependents_are_found_through_templates() {
        let stack = example_stack();
        assert_eq!(stack.image_dependents("api"), vec!["infra"]);
        assert!(stack.image_dependents("db").is_empty());
    }

    #[test]
    fn grace_seconds_precedence() {
        let stack = example_stack();
        assert_eq!(stack.grace_seconds("db"), 30);
        assert_eq!(stack.grace_seconds("api"), 20);
        let mut bare = stack.clone();
        bare.stop = None;
        assert_eq!(bare.grace_seconds("api"), 10);
    }

    #[test]
    fn default_mode_falls_back_to_first_declared() {
        let stack = example_stack();
        assert_eq!(stack.components["api"].default_mode(), Some(Mode::Prebuilt));
        assert_eq!(stack.components["web"].default_mode(), Some(Mode::Dev));
        assert_eq!(stack.components["infra"].default_mode(), None);
    }

    #[test]
    fn port_count_defaults_by_driver() {
        let stack = example_stack();
        assert_eq!(stack.components["api"].port_count(), 1);
        assert_eq!(stack.components["web"].port_count(), 1);
        assert_eq!(stack.components["infra"].port_count(), 0);
        assert_eq!(stack.components["migrate"].port_count(), 0);
    }

    #[test]
    fn slot_assign_is_checked_against_placements_and_count() {
        let mut doc = example();
        doc.stacks["app"]
            .slots
            .placements
            .insert("local".into(), PlacementDef::default());
        doc.stacks["app"].slots.assign.insert(0, "local".into());
        doc.stacks["app"].slots.assign.insert(9, "cloud".into());
        let errs = errors(&doc);
        assert!(errs
            .iter()
            .any(|d| d.path == "stacks.app.slots.assign.9" && d.rule == Rule::UnknownKey));
        assert!(errs
            .iter()
            .any(|d| d.path == "stacks.app.slots.assign.9" && d.rule == Rule::PortLayout));
        assert!(!errs.iter().any(|d| d.path == "stacks.app.slots.assign.0"));
    }

    #[test]
    fn placement_block_parses_with_integer_slot_keys() {
        let text = "stacks:\n  app:\n    slots:\n      count: 3\n      placements:\n        local: {}\n        cloud:\n          provision: stack/infra/slot-host\n          engines: { core: docker-context }\n          ttlHours: 8\n          budget: { perSlotPerDay: 15, currency: USD }\n      assign: { 0: local, 1: cloud }\n    groups: { core: {} }\n";
        let doc = StacksDocument::parse(text).unwrap();
        let slots = &doc.stacks["app"].slots;
        assert_eq!(slots.assign.get(&0).map(String::as_str), Some("local"));
        assert_eq!(slots.assign.get(&1).map(String::as_str), Some("cloud"));
        assert_eq!(
            slots.placements["cloud"]
                .budget
                .as_ref()
                .unwrap()
                .per_slot_per_day,
            15.0
        );
        assert!(errors(&doc).is_empty());
    }

    #[test]
    fn diagnostics_display_and_serialize() {
        let d = Diagnostic {
            severity: Severity::Error,
            rule: Rule::CrossEngineReference,
            path: "stacks.app.components.web.env.X".into(),
            message: "m".into(),
        };
        assert_eq!(d.to_string(), "error at stacks.app.components.web.env.X: m");
        let json = serde_json::to_string(&d).unwrap();
        assert!(json.contains("\"rule\":\"cross-engine-reference\""));
        assert!(has_errors(std::slice::from_ref(&d)));
        assert!(!has_errors(&[Diagnostic {
            severity: Severity::Warning,
            ..d
        }]));
    }

    mod file_rule {
        use super::*;

        #[test]
        fn none_when_neither_file_defines_stacks() {
            let dir = tempfile::tempdir().unwrap();
            assert!(StacksDocument::load_from_project(dir.path())
                .unwrap()
                .is_none());
            fs::write(dir.path().join(PROJECT_FILE_NAME), "processes: {}\n").unwrap();
            assert!(StacksDocument::load_from_project(dir.path())
                .unwrap()
                .is_none());
        }

        #[test]
        fn stacks_in_project_file() {
            let dir = tempfile::tempdir().unwrap();
            fs::write(
                dir.path().join(PROJECT_FILE_NAME),
                format!("processes: {{}}\n{EXAMPLE}"),
            )
            .unwrap();
            let (path, doc) = StacksDocument::load_from_project(dir.path())
                .unwrap()
                .unwrap();
            assert_eq!(path, dir.path().join(PROJECT_FILE_NAME));
            assert_eq!(doc.stacks.len(), 1);
        }

        #[test]
        fn stacks_in_stack_file() {
            let dir = tempfile::tempdir().unwrap();
            fs::write(dir.path().join(PROJECT_FILE_NAME), "processes: {}\n").unwrap();
            fs::write(dir.path().join(STACK_FILE_NAME), EXAMPLE).unwrap();
            let (path, doc) = StacksDocument::load_from_project(dir.path())
                .unwrap()
                .unwrap();
            assert_eq!(path, dir.path().join(STACK_FILE_NAME));
            assert!(doc.stacks.contains_key("app"));
        }

        #[test]
        fn both_files_defining_stacks_is_refused() {
            let dir = tempfile::tempdir().unwrap();
            fs::write(dir.path().join(PROJECT_FILE_NAME), EXAMPLE).unwrap();
            fs::write(dir.path().join(STACK_FILE_NAME), EXAMPLE).unwrap();
            let e = StacksDocument::load_from_project(dir.path()).unwrap_err();
            assert!(matches!(e, LoadError::BothDefine), "{e}");
            assert!(e.to_string().contains(STACK_FILE_NAME));
        }

        #[test]
        fn stack_file_must_carry_the_stacks_key() {
            let dir = tempfile::tempdir().unwrap();
            fs::write(dir.path().join(STACK_FILE_NAME), "app:\n  components: {}\n").unwrap();
            assert!(matches!(
                StacksDocument::load_from_project(dir.path()).unwrap_err(),
                LoadError::StackFileWithoutStacks
            ));
            fs::write(dir.path().join(STACK_FILE_NAME), "# empty\n").unwrap();
            assert!(matches!(
                StacksDocument::load_from_project(dir.path()).unwrap_err(),
                LoadError::StackFileWithoutStacks
            ));
        }

        #[test]
        fn oversized_file_is_refused() {
            let dir = tempfile::tempdir().unwrap();
            let big = format!("stacks: {{}}\n# {}\n", "x".repeat(MAX_DOC_BYTES));
            fs::write(dir.path().join(STACK_FILE_NAME), big).unwrap();
            assert!(matches!(
                StacksDocument::load_from_project(dir.path()).unwrap_err(),
                LoadError::TooLarge { .. }
            ));
        }

        #[test]
        fn parse_errors_name_the_file() {
            let dir = tempfile::tempdir().unwrap();
            fs::write(
                dir.path().join(STACK_FILE_NAME),
                "stacks:\n  app:\n    components:\n      a: { nope: 1 }\n",
            )
            .unwrap();
            let e = StacksDocument::load_from_project(dir.path()).unwrap_err();
            match e {
                LoadError::Parse { path, message } => {
                    assert!(path.ends_with(STACK_FILE_NAME));
                    assert!(message.contains("driver"), "{message}");
                }
                other => panic!("{other}"),
            }
        }
    }
}
