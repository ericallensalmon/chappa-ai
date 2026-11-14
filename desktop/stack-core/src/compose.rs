//! The `compose` driver: docker and podman, one compose project per group.
//!
//! Each group of an instance is its own compose project, `<instance>-<group>`,
//! run with the group's engine (`docker compose` or `podman compose`), so two
//! engines on one machine never share a project, a network or a name. The
//! project's files are the components' compose files plus one generated
//! override, `.work/instances/<instance>/<group>.override.yml`, that carries
//! everything decided per instance:
//!
//! - the published ports (`portBase + slot × stride + index`), each mapped
//!   to the container port the base file declares for that service;
//! - the instance environment and the component's `env:`;
//! - `extra_hosts: ["<hostname>:<gateway>"]`, so the stack's hostname
//!   resolves to the host from inside every container;
//! - the image: a digest pin for prebuilt components, the content-keyed tag
//!   (plus a `build:` section) for source ones;
//! - labels naming the instance, the component and the gateway baked in.
//!
//! Compose concatenates `ports` lists when merging files, so a base file
//! that publishes ports of its own would publish them *as well*. The base
//! files are therefore expected to declare container ports only (`expose:`,
//! or `ports:` with a target and no host side) and leave publishing to the
//! override; [`base_file_findings`] warns when one does not.
//!
//! # Gateway address
//!
//! Containers reach the host at an address that depends on the engine:
//!
//! | engine | `<gateway>` | stable across reboots |
//! |---|---|---|
//! | docker, Linux containers | the literal `host-gateway`, which the daemon resolves per container at start | yes |
//! | docker, Windows containers | the gateway of the `nat` network (`network inspect nat`) | no |
//! | podman with a machine (Windows, macOS) | what `host.containers.internal` resolves to inside the machine | no |
//! | podman native (Linux) | the literal `host-gateway` | yes |
//!
//! A baked-in literal address goes stale when the engine's network comes
//! back different after a reboot; `start` compares the label with the
//! current address and recreates only those components.
//!
//! # Volumes
//!
//! `down` never passes `--volumes` unless the caller says so explicitly. A
//! named persistence volume holds data (and saved snapshots) that one
//! click must not destroy.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_yaml::Value as Yaml;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::cli::{compose_argv, engine_invocation, CliError, EngineCli, Invocation};
use crate::definition::{ContainerOs, DriverKind, EngineRef, StackDef};
use crate::driver::{Actual, EngineKind, Health, ResolvedInstance};
use crate::images::{SourceBuild, LABEL_COMPONENT, LABEL_GATEWAY, LABEL_INSTANCE};
use crate::intent::INSTANCES_DIR;
use crate::jobs::RunBlock;
use crate::placeholders::{self, PlaceholderError};
use crate::process;

/// The special address docker (and native podman) resolve to the host.
pub const HOST_GATEWAY: &str = "host-gateway";
/// Lines of history a `logs -f` run opens with.
pub const LOGS_TAIL: &str = "200";

// ----------------------------------------------------------------- project

/// One group's compose project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComposeProject {
    pub group: String,
    /// `<instance>-<group>`.
    pub project: String,
    pub engine: EngineRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<ContainerOs>,
    /// The components' compose files, absolute, deduplicated, in definition order.
    pub files: Vec<PathBuf>,
    pub override_path: PathBuf,
    /// Component → service name.
    pub services: IndexMap<String, String>,
}

/// The compose projects of an instance: every `compose` component, grouped.
pub fn projects(
    inst: &ResolvedInstance,
    stack: &StackDef,
    project_dir: &Path,
    work_dir: &Path,
) -> IndexMap<String, ComposeProject> {
    let mut out: IndexMap<String, ComposeProject> = IndexMap::new();
    for (name, c) in &inst.components {
        if c.driver != DriverKind::Compose {
            continue;
        }
        let def = &stack.components[name];
        let entry = out
            .entry(c.group.clone())
            .or_insert_with(|| ComposeProject {
                group: c.group.clone(),
                project: inst.compose_project(&c.group),
                engine: c.engine.clone(),
                os: stack.os_for_engine(&c.engine.name),
                files: Vec::new(),
                override_path: override_path(work_dir, &inst.id, &c.group),
                services: IndexMap::new(),
            });
        if let Some(f) = &def.file {
            let abs = if f.is_absolute() {
                f.clone()
            } else {
                project_dir.join(f)
            };
            if !entry.files.contains(&abs) {
                entry.files.push(abs);
            }
        }
        entry.services.insert(
            name.clone(),
            def.service.clone().unwrap_or_else(|| name.clone()),
        );
    }
    out
}

/// `.work/instances/<instance>/<group>.override.yml` (`default.override.yml`
/// for the implicit group).
pub fn override_path(work_dir: &Path, instance: &str, group: &str) -> PathBuf {
    let g = if group.is_empty() { "default" } else { group };
    work_dir
        .join(INSTANCES_DIR)
        .join(instance)
        .join(format!("{g}.override.yml"))
}

impl ComposeProject {
    pub fn service_of<'c>(&'c self, component: &'c str) -> &'c str {
        self.services
            .get(component)
            .map(String::as_str)
            .unwrap_or(component)
    }

    pub fn component_of(&self, service: &str) -> Option<&str> {
        self.services
            .iter()
            .find(|(_, s)| s.as_str() == service)
            .map(|(c, _)| c.as_str())
    }

    fn all_files(&self) -> Vec<PathBuf> {
        let mut files = self.files.clone();
        files.push(self.override_path.clone());
        files
    }

    /// `<engine> compose -p <project> -f … <args>`.
    pub fn argv(&self, args: &[&str]) -> Vec<String> {
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        compose_argv(&self.engine, &self.project, &self.all_files(), &owned)
    }

    fn with_services(&self, args: Vec<&str>, components: &[&str]) -> Vec<String> {
        let mut all: Vec<&str> = args;
        for c in components {
            all.push(self.service_of(c));
        }
        self.argv(&all)
    }

    /// Create and start: `up -d --no-deps [--force-recreate] <services>`.
    /// `--no-deps` because the plan already ordered the starts and decides
    /// the selection; compose must not start an unselected dependency.
    pub fn up(&self, components: &[&str], force_recreate: bool) -> Vec<String> {
        let mut args = vec!["up", "-d", "--no-deps"];
        if force_recreate {
            args.push("--force-recreate");
        }
        self.with_services(args, components)
    }

    /// Start existing containers.
    pub fn start(&self, components: &[&str]) -> Vec<String> {
        self.with_services(vec!["start"], components)
    }

    /// `stop -t <grace> <service>`.
    pub fn stop(&self, component: &str, grace_seconds: u32) -> Vec<String> {
        let grace = grace_seconds.to_string();
        self.with_services(vec!["stop", "-t", &grace], &[component])
    }

    pub fn restart(&self, component: &str, grace_seconds: u32) -> Vec<String> {
        let grace = grace_seconds.to_string();
        self.with_services(vec!["restart", "-t", &grace], &[component])
    }

    /// Remove stopped-or-running containers of some services, volumes kept.
    pub fn remove(&self, components: &[&str]) -> Vec<String> {
        self.with_services(vec!["rm", "-f", "-s"], components)
    }

    /// Remove the whole project. `--volumes` only on explicit request.
    pub fn down(&self, remove_volumes: bool) -> Vec<String> {
        if remove_volumes {
            self.argv(&["down", "--volumes"])
        } else {
            self.argv(&["down"])
        }
    }

    /// `ps -a --format json`: every container of the project, running or not.
    pub fn ps(&self) -> Invocation {
        Invocation::from_argv(&self.argv(&["ps", "-a", "--format", "json"]))
    }

    /// `inspect` of named containers, through the engine itself.
    pub fn inspect(&self, names: &[String]) -> Invocation {
        let mut args: Vec<&str> = vec!["inspect", "--type", "container"];
        args.extend(names.iter().map(String::as_str));
        engine_invocation(&self.engine, &args)
    }

    /// `logs -f` as a terminal action, never read here.
    pub fn logs(&self, component: &str, cwd: &Path) -> RunBlock {
        RunBlock::new(
            self.with_services(vec!["logs", "-f", "--tail", LOGS_TAIL], &[component]),
            cwd,
        )
        .titled(format!("logs: {component}"))
    }
}

// ------------------------------------------------------------------ override

/// A container-side port the base file declares for a service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortTarget {
    pub target: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    /// The base file publishes this port itself (a host side is given).
    #[serde(default)]
    pub published: bool,
}

#[derive(Debug, Deserialize)]
struct ComposeDoc {
    #[serde(default)]
    services: IndexMap<String, ComposeService>,
}

#[derive(Debug, Default, Deserialize)]
struct ComposeService {
    #[serde(default)]
    ports: Vec<Yaml>,
    #[serde(default)]
    expose: Vec<Yaml>,
}

fn port_number(s: &str) -> Option<u16> {
    // "80" or "80-81" (a range: its first port).
    s.split('-').next()?.trim().parse().ok()
}

/// One short-form `ports` entry: `"80"`, `"8080:80"`, `"127.0.0.1:8080:80"`,
/// `"80/udp"`, `"8080:80/tcp"`.
fn parse_short_port(s: &str) -> Option<PortTarget> {
    let (ports, protocol) = match s.split_once('/') {
        Some((p, proto)) => (p, Some(proto.to_string())),
        None => (s, None),
    };
    let parts: Vec<&str> = ports.split(':').collect();
    let target = port_number(parts.last()?)?;
    Some(PortTarget {
        target,
        protocol,
        published: parts.len() > 1,
    })
}

fn parse_port_entry(v: &Yaml) -> Option<PortTarget> {
    match v {
        Yaml::Number(n) => Some(PortTarget {
            target: u16::try_from(n.as_u64()?).ok()?,
            protocol: None,
            published: false,
        }),
        Yaml::String(s) => parse_short_port(s),
        Yaml::Mapping(m) => {
            let get = |k: &str| m.get(Yaml::String(k.to_string()));
            let target = match get("target")? {
                Yaml::Number(n) => u16::try_from(n.as_u64()?).ok()?,
                Yaml::String(s) => port_number(s)?,
                _ => return None,
            };
            Some(PortTarget {
                target,
                protocol: get("protocol").and_then(|p| p.as_str()).map(str::to_string),
                published: get("published").is_some(),
            })
        }
        _ => None,
    }
}

/// The container ports `service` declares in a compose file's text: its
/// `ports` entries (target side), then its `expose` entries.
pub fn base_targets(compose_text: &str, service: &str) -> Result<Vec<PortTarget>, String> {
    let doc: ComposeDoc = serde_yaml::from_str(compose_text).map_err(|e| e.to_string())?;
    let Some(svc) = doc.services.get(service) else {
        return Ok(Vec::new());
    };
    let mut out: Vec<PortTarget> = svc.ports.iter().filter_map(parse_port_entry).collect();
    for e in &svc.expose {
        if let Some(mut t) = parse_port_entry(e) {
            t.published = false;
            if !out.iter().any(|o| o.target == t.target) {
                out.push(t);
            }
        }
    }
    Ok(out)
}

/// Services whose base file publishes ports itself: those would be
/// published in addition to the instance's.
pub fn base_file_findings(
    project: &ComposeProject,
    texts: &IndexMap<PathBuf, String>,
) -> Vec<String> {
    let mut out = Vec::new();
    for (component, service) in &project.services {
        for (path, text) in texts {
            if let Ok(targets) = base_targets(text, service) {
                let own: Vec<String> = targets
                    .iter()
                    .filter(|t| t.published)
                    .map(|t| t.target.to_string())
                    .collect();
                if !own.is_empty() {
                    out.push(format!(
                        "{component}: `{}` publishes port(s) {} itself; the instance publishes its own, so both will be published",
                        path.display(),
                        own.join(", ")
                    ));
                }
            }
        }
    }
    out
}

/// One service's part of the override.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceOverride {
    pub service: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<BuildSection>,
    /// `(host, target, protocol)`.
    #[serde(default)]
    pub ports: Vec<(u16, u16, Option<String>)>,
    #[serde(default)]
    pub environment: IndexMap<String, String>,
    #[serde(default)]
    pub labels: IndexMap<String, String>,
    #[serde(default)]
    pub extra_hosts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildSection {
    pub context: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dockerfile: Option<PathBuf>,
}

/// What the override needs to know per component beyond the instance.
#[derive(Debug, Clone)]
pub struct OverrideInputs<'a> {
    /// `{image:<component>}` values: pinned references and content keys.
    pub images: &'a IndexMap<String, String>,
    /// Source builds in force, for `build:` sections.
    pub builds: &'a IndexMap<String, SourceBuild>,
    /// The gateway address baked into `extra_hosts` and the gateway label.
    pub gateway: Option<&'a str>,
    /// Base-file text per path, for container port targets.
    pub base_texts: &'a IndexMap<PathBuf, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OverrideError {
    #[error("component `{component}`: {source}")]
    Placeholder {
        component: String,
        #[source]
        source: PlaceholderError,
    },
    #[error("{path}: {message}")]
    BaseFile { path: PathBuf, message: String },
}

/// The override's services for one project.
pub fn service_overrides(
    project: &ComposeProject,
    inst: &ResolvedInstance,
    stack: &StackDef,
    inputs: &OverrideInputs<'_>,
) -> Result<Vec<ServiceOverride>, OverrideError> {
    let instance_env = process::instance_env(inst, stack, inputs.images);
    let mut out = Vec::new();
    for (component, service) in &project.services {
        let c = &inst.components[component];
        let mut targets: Vec<PortTarget> = Vec::new();
        for path in &project.files {
            if let Some(text) = inputs.base_texts.get(path) {
                let found =
                    base_targets(text, service).map_err(|message| OverrideError::BaseFile {
                        path: path.clone(),
                        message,
                    })?;
                if !found.is_empty() {
                    targets = found;
                    break;
                }
            }
        }
        let ports = c
            .ports
            .iter()
            .enumerate()
            .map(|(i, host)| match targets.get(i) {
                Some(t) => (*host, t.target, t.protocol.clone()),
                None => (*host, *host, None),
            })
            .collect();
        let mut ctx = inst.context_for(component);
        ctx.images = inputs.images.clone();
        let mut environment = instance_env.clone();
        for (k, template) in &c.env {
            let v = placeholders::resolve(template, &ctx).map_err(|source| {
                OverrideError::Placeholder {
                    component: component.clone(),
                    source,
                }
            })?;
            environment.insert(k.clone(), v);
        }
        let mut labels = IndexMap::new();
        labels.insert(LABEL_INSTANCE.to_string(), inst.id.clone());
        labels.insert(LABEL_COMPONENT.to_string(), component.clone());
        let mut extra_hosts = Vec::new();
        if let (Some(hostname), Some(gw)) = (&inst.hostname, inputs.gateway) {
            extra_hosts.push(format!("{hostname}:{gw}"));
            labels.insert(LABEL_GATEWAY.to_string(), gw.to_string());
        }
        out.push(ServiceOverride {
            service: service.clone(),
            image: inputs.images.get(component).cloned(),
            build: inputs.builds.get(component).map(|b| BuildSection {
                context: b.context.clone(),
                dockerfile: b.dockerfile.clone(),
            }),
            ports,
            environment,
            labels,
            extra_hosts,
        });
    }
    Ok(out)
}

/// The override file's YAML.
pub fn render_override(services: &[ServiceOverride]) -> String {
    let mut svc_map = serde_yaml::Mapping::new();
    for s in services {
        let mut m = serde_yaml::Mapping::new();
        if let Some(img) = &s.image {
            m.insert("image".into(), Yaml::String(img.clone()));
        }
        if let Some(b) = &s.build {
            let mut bm = serde_yaml::Mapping::new();
            bm.insert(
                "context".into(),
                Yaml::String(b.context.to_string_lossy().into_owned()),
            );
            if let Some(df) = &b.dockerfile {
                bm.insert(
                    "dockerfile".into(),
                    Yaml::String(df.to_string_lossy().into_owned()),
                );
            }
            m.insert("build".into(), Yaml::Mapping(bm));
        }
        if !s.ports.is_empty() {
            let list: Vec<Yaml> = s
                .ports
                .iter()
                .map(|(host, target, proto)| {
                    Yaml::String(match proto {
                        Some(p) => format!("{host}:{target}/{p}"),
                        None => format!("{host}:{target}"),
                    })
                })
                .collect();
            m.insert("ports".into(), Yaml::Sequence(list));
        }
        if !s.environment.is_empty() {
            let mut em = serde_yaml::Mapping::new();
            for (k, v) in &s.environment {
                em.insert(Yaml::String(k.clone()), Yaml::String(v.clone()));
            }
            m.insert("environment".into(), Yaml::Mapping(em));
        }
        if !s.labels.is_empty() {
            let mut lm = serde_yaml::Mapping::new();
            for (k, v) in &s.labels {
                lm.insert(Yaml::String(k.clone()), Yaml::String(v.clone()));
            }
            m.insert("labels".into(), Yaml::Mapping(lm));
        }
        if !s.extra_hosts.is_empty() {
            m.insert(
                "extra_hosts".into(),
                Yaml::Sequence(s.extra_hosts.iter().cloned().map(Yaml::String).collect()),
            );
        }
        svc_map.insert(Yaml::String(s.service.clone()), Yaml::Mapping(m));
    }
    let mut root = serde_yaml::Mapping::new();
    root.insert("services".into(), Yaml::Mapping(svc_map));
    let body = serde_yaml::to_string(&Yaml::Mapping(root)).expect("override serializes");
    format!("# Generated per instance; do not edit. Regenerated by every up / start.\n{body}")
}

// ---------------------------------------------------------------- ps / inspect

/// One row of `compose ps -a --format json`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PsEntry {
    #[serde(rename = "Name", default)]
    pub name: String,
    #[serde(rename = "Service", default)]
    pub service: String,
    /// `running`, `exited`, `created`, `restarting`, `paused`, `dead`.
    #[serde(rename = "State", default)]
    pub state: String,
    /// `healthy`, `unhealthy`, `starting` or empty.
    #[serde(rename = "Health", default)]
    pub health: String,
    #[serde(rename = "ExitCode", default)]
    pub exit_code: i32,
    #[serde(rename = "Status", default)]
    pub status: String,
    #[serde(rename = "Image", default)]
    pub image: String,
}

/// Parse `ps` output: a JSON array (older compose) or one object per line.
pub fn parse_ps(stdout: &str) -> Result<Vec<PsEntry>, CliError> {
    let text = stdout.trim();
    if text.is_empty() {
        return Ok(Vec::new());
    }
    if text.starts_with('[') {
        return serde_json::from_str(text).map_err(|e| CliError::BadOutput {
            what: "compose ps".into(),
            message: e.to_string(),
        });
    }
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| {
            serde_json::from_str(l).map_err(|e| CliError::BadOutput {
                what: "compose ps".into(),
                message: e.to_string(),
            })
        })
        .collect()
}

#[derive(Debug, Deserialize)]
struct InspectRaw {
    #[serde(rename = "Name", default)]
    name: String,
    #[serde(rename = "RestartCount", default)]
    restart_count: u32,
    #[serde(rename = "State", default)]
    state: InspectState,
    #[serde(rename = "Config", default)]
    config: InspectConfig,
    #[serde(rename = "HostConfig", default)]
    host_config: InspectHostConfig,
    #[serde(rename = "Image", default)]
    image: String,
}

#[derive(Debug, Default, Deserialize)]
struct InspectState {
    #[serde(rename = "Status", default)]
    status: String,
    #[serde(rename = "Running", default)]
    running: bool,
    #[serde(rename = "Restarting", default)]
    restarting: bool,
    #[serde(rename = "ExitCode", default)]
    exit_code: i32,
    #[serde(rename = "StartedAt", default)]
    started_at: String,
    #[serde(rename = "Health", default)]
    health: Option<InspectHealth>,
}

#[derive(Debug, Default, Deserialize)]
struct InspectHealth {
    #[serde(rename = "Status", default)]
    status: String,
}

#[derive(Debug, Default, Deserialize)]
struct InspectConfig {
    #[serde(rename = "Labels", default)]
    labels: Option<IndexMap<String, String>>,
    #[serde(rename = "Image", default)]
    image: String,
}

#[derive(Debug, Default, Deserialize)]
struct InspectHostConfig {
    #[serde(rename = "ExtraHosts", default)]
    extra_hosts: Option<Vec<String>>,
}

/// What `inspect` says about one container.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InspectEntry {
    pub name: String,
    pub running: bool,
    pub restarting: bool,
    pub restarts: u32,
    pub exit_code: i32,
    pub status: String,
    pub health: Health,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default)]
    pub labels: IndexMap<String, String>,
    #[serde(default)]
    pub extra_hosts: Vec<String>,
    /// The image reference the container was created from.
    pub image_ref: String,
    /// The image id (`sha256:…`).
    pub image_id: String,
}

fn health_from(s: &str) -> Health {
    match s {
        "healthy" => Health::Healthy,
        "unhealthy" => Health::Unhealthy,
        "starting" => Health::Starting,
        _ => Health::Unknown,
    }
}

/// Parse `inspect` output (a JSON array).
pub fn parse_inspect(stdout: &str) -> Result<Vec<InspectEntry>, CliError> {
    let text = stdout.trim();
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let raws: Vec<InspectRaw> = serde_json::from_str(text).map_err(|e| CliError::BadOutput {
        what: "inspect".into(),
        message: e.to_string(),
    })?;
    Ok(raws
        .into_iter()
        .map(|r| InspectEntry {
            name: r.name.trim_start_matches('/').to_string(),
            running: r.state.running,
            restarting: r.state.restarting || r.state.status == "restarting",
            restarts: r.restart_count,
            exit_code: r.state.exit_code,
            status: r.state.status,
            health: r
                .state
                .health
                .as_ref()
                .map(|h| health_from(&h.status))
                .unwrap_or_default(),
            started_at: Some(r.state.started_at)
                .filter(|s| !s.is_empty() && !s.starts_with("0001-")),
            labels: r.config.labels.unwrap_or_default(),
            extra_hosts: r.host_config.extra_hosts.unwrap_or_default(),
            image_ref: r.config.image,
            image_id: r.image,
        })
        .collect())
}

impl InspectEntry {
    /// The gateway address this container was created with: the label this
    /// crate writes, else the `extra_hosts` entry for `hostname`.
    pub fn baked_gateway(&self, hostname: Option<&str>) -> Option<String> {
        if let Some(g) = self.labels.get(LABEL_GATEWAY) {
            return Some(g.clone());
        }
        let hostname = hostname?;
        self.extra_hosts.iter().find_map(|h| {
            let (name, addr) = h.split_once(':')?;
            (name.eq_ignore_ascii_case(hostname)).then(|| addr.to_string())
        })
    }
}

/// The engine's view of a component, from `ps` (always present when the
/// container exists) and `inspect` (richer, when it was fetched).
pub fn actual_from(ps: Option<&PsEntry>, inspect: Option<&InspectEntry>) -> Actual {
    match (ps, inspect) {
        (None, None) => Actual::default(),
        (_, Some(i)) => Actual {
            exists: true,
            running: i.running,
            restarting: i.restarting,
            restarts: i.restarts,
            exit_code: (!i.running).then_some(i.exit_code),
            health: i.health,
            probe_up: None,
        },
        (Some(p), None) => {
            let running = p.state == "running";
            Actual {
                exists: true,
                running,
                restarting: p.state == "restarting",
                restarts: 0,
                exit_code: (!running).then_some(p.exit_code),
                health: health_from(&p.health),
                probe_up: None,
            }
        }
    }
}

// ------------------------------------------------------------------- uptime

/// Seconds since the epoch of an RFC 3339 timestamp with optional fraction
/// and `Z` or `±HH:MM` offset (what `inspect` prints).
pub fn parse_utc(s: &str) -> Option<u64> {
    let s = s.trim();
    let (date, rest) = s.split_once('T')?;
    let mut parts = date.split('-');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    let d: u32 = parts.next()?.parse().ok()?;
    let (time, offset_secs) = if let Some(t) = rest.strip_suffix('Z') {
        (t, 0i64)
    } else if let Some(i) = rest.rfind(['+', '-']) {
        let (t, off) = rest.split_at(i);
        let sign = if off.starts_with('-') { -1 } else { 1 };
        let mut hm = off[1..].split(':');
        let h: i64 = hm.next()?.parse().ok()?;
        let mi: i64 = hm.next().unwrap_or("0").parse().ok()?;
        (t, sign * (h * 3600 + mi * 60))
    } else {
        (rest, 0)
    };
    let time = time.split('.').next()?;
    let mut hms = time.split(':');
    let hh: i64 = hms.next()?.parse().ok()?;
    let mm: i64 = hms.next()?.parse().ok()?;
    let ss: i64 = hms.next().unwrap_or("0").parse().ok()?;
    // days_from_civil (proleptic Gregorian)
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + hh * 3600 + mm * 60 + ss - offset_secs;
    u64::try_from(secs).ok()
}

/// `3d 2h`, `2h 5m`, `5m`, `40s`.
pub fn uptime_text(started_at: &str, now_secs: u64) -> Option<String> {
    let start = parse_utc(started_at)?;
    let d = now_secs.saturating_sub(start);
    Some(if d >= 86_400 {
        format!("{}d {}h", d / 86_400, (d % 86_400) / 3600)
    } else if d >= 3600 {
        format!("{}h {}m", d / 3600, (d % 3600) / 60)
    } else if d >= 60 {
        format!("{}m", d / 60)
    } else {
        format!("{d}s")
    })
}

// ------------------------------------------------------------------ gateway

/// The address containers of an engine reach the host at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Gateway {
    pub address: String,
    /// An alias the engine resolves at container start (`host-gateway`):
    /// it cannot go stale, so `start` never recreates for it.
    pub stable: bool,
}

impl Gateway {
    pub fn alias() -> Self {
        Self {
            address: HOST_GATEWAY.to_string(),
            stable: true,
        }
    }

    pub fn literal(address: &str) -> Self {
        Self {
            address: address.to_string(),
            stable: false,
        }
    }
}

pub trait GatewayResolver {
    /// `os` is the engine's container OS when already known (saves a call).
    fn gateway(&self, engine: &EngineRef, os: Option<ContainerOs>) -> Result<Gateway, String>;
}

/// Scripted gateways by engine name; unknown engines get the alias.
#[derive(Debug, Default)]
pub struct FakeGateways(pub IndexMap<String, Gateway>);

impl GatewayResolver for FakeGateways {
    fn gateway(&self, engine: &EngineRef, _os: Option<ContainerOs>) -> Result<Gateway, String> {
        Ok(self
            .0
            .get(&engine.name)
            .cloned()
            .unwrap_or_else(Gateway::alias))
    }
}

/// The rule table from the module docs, asked of the engines.
pub struct CliGatewayResolver<'a> {
    pub cli: &'a dyn EngineCli,
}

#[derive(Debug, Deserialize)]
struct IpamConfig {
    #[serde(rename = "Gateway", default)]
    gateway: String,
}

impl GatewayResolver for CliGatewayResolver<'_> {
    fn gateway(&self, engine: &EngineRef, os: Option<ContainerOs>) -> Result<Gateway, String> {
        match engine.kind {
            EngineKind::Docker => {
                let os = match os {
                    Some(o) => o,
                    None => {
                        let out = self
                            .cli
                            .run_ok(&engine_invocation(
                                engine,
                                &["info", "--format", "{{.OSType}}"],
                            ))
                            .map_err(|e| e.to_string())?;
                        ContainerOs::parse(&out.stdout).unwrap_or(ContainerOs::Linux)
                    }
                };
                match os {
                    ContainerOs::Linux => Ok(Gateway::alias()),
                    ContainerOs::Windows => {
                        let out = self
                            .cli
                            .run_ok(&engine_invocation(
                                engine,
                                &[
                                    "network",
                                    "inspect",
                                    "nat",
                                    "--format",
                                    "{{json .IPAM.Config}}",
                                ],
                            ))
                            .map_err(|e| e.to_string())?;
                        let configs: Vec<IpamConfig> = serde_json::from_str(out.stdout.trim())
                            .map_err(|e| format!("nat network: {e}"))?;
                        configs
                            .iter()
                            .map(|c| c.gateway.trim())
                            .find(|g| !g.is_empty())
                            .map(Gateway::literal)
                            .ok_or_else(|| "the nat network has no gateway".to_string())
                    }
                }
            }
            EngineKind::Podman => {
                let machine = self
                    .cli
                    .run(&engine_invocation(engine, &["machine", "inspect"]))
                    .map_err(|e| e.to_string())?;
                if !machine.ok() {
                    return Ok(Gateway::alias());
                }
                let out = self
                    .cli
                    .run_ok(&engine_invocation(
                        engine,
                        &[
                            "machine",
                            "ssh",
                            "--",
                            "getent",
                            "hosts",
                            "host.containers.internal",
                        ],
                    ))
                    .map_err(|e| e.to_string())?;
                out.stdout
                    .split_whitespace()
                    .next()
                    .map(Gateway::literal)
                    .ok_or_else(|| {
                        "host.containers.internal does not resolve inside the podman machine"
                            .to_string()
                    })
            }
            _ => Err(format!("{} runs no containers", engine.kind)),
        }
    }
}

/// Container names that belong to any instance of `stack`, from `ps`
/// labels: used to find what other instances run.
pub fn instances_in(ps_names: &[PsEntry]) -> BTreeSet<String> {
    ps_names
        .iter()
        .filter_map(|p| {
            p.name
                .rsplit_once('-')
                .map(|(prefix, _)| prefix.to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::FakeCli;
    use crate::definition::StacksDocument;
    use crate::driver::resolve;
    use crate::intent::{ComponentIntent, Desired, IntentRecord};

    fn stack() -> StackDef {
        let text = r#"
stacks:
  app:
    slots: { count: 3, portBase: 14000, portStride: 100 }
    hostname: dev.example.test
    groups:
      core: { engine: docker, os: windows }
      apis: { engine: podman }
    components:
      db:
        group: core
        driver: compose
        file: stack/core.compose.yml
        service: postgres
        modes: { prebuilt: { image: "postgres", tag: "16" } }
        persistence: { volume: db-data }
        ports: 2
      migrate:
        group: core
        driver: compose
        file: stack/core.compose.yml
        kind: oneshot
        ports: 0
        depends_on: [db]
        modes: { prebuilt: { image: "registry.example/api", tag: "{channel}" } }
      api:
        group: apis
        driver: compose
        file: stack/apis.compose.yml
        modes:
          prebuilt: { image: "registry.example/api", tag: "{channel}" }
          source: { repo: ../api }
        env: { DB_URL: "postgres://{host}:{port:db}/app", SELF: "{image:api}" }
        depends_on: [db]
      web:
        group: core
        driver: process
        command: web-dev
        modes: { dev: { repo: ../web } }
"#;
        StacksDocument::parse(text)
            .unwrap()
            .stacks
            .shift_remove("app")
            .unwrap()
    }

    fn inst() -> ResolvedInstance {
        let mut intent = IntentRecord::new("app", 1, "2026-01-01T00:00:00Z");
        for c in ["db", "api", "migrate"] {
            intent.components.insert(
                c.into(),
                ComponentIntent {
                    desired: Desired::Running,
                    ..Default::default()
                },
            );
        }
        resolve("app", &stack(), 1, Some(&intent)).unwrap()
    }

    const CORE_FILE: &str = r#"
services:
  postgres:
    image: postgres:16
    expose: ["5432"]
    volumes: [db-data:/var/lib/postgresql/data]
  migrate:
    image: registry.example/api
    command: [migrate]
volumes:
  db-data: {}
"#;

    #[test]
    fn projects_are_one_per_group_with_the_groups_engine_and_files() {
        let st = stack();
        let projects = projects(&inst(), &st, Path::new("/proj"), Path::new("/proj/.work"));
        assert_eq!(
            projects.keys().cloned().collect::<Vec<_>>(),
            ["core", "apis"]
        );
        let core = &projects["core"];
        assert_eq!(core.project, "app-1-core");
        assert_eq!(core.engine.kind, EngineKind::Docker);
        assert_eq!(core.os, Some(ContainerOs::Windows));
        assert_eq!(
            core.files,
            [PathBuf::from("/proj").join("stack/core.compose.yml")]
        );
        assert_eq!(
            core.override_path,
            PathBuf::from("/proj/.work")
                .join("instances")
                .join("app-1")
                .join("core.override.yml")
        );
        assert_eq!(core.services["db"], "postgres");
        assert_eq!(core.services["migrate"], "migrate");
        assert_eq!(core.component_of("postgres"), Some("db"));
        assert!(
            !core.services.contains_key("web"),
            "process components are not compose services"
        );
        let apis = &projects["apis"];
        assert_eq!(apis.engine.kind, EngineKind::Podman);
        assert_eq!(apis.os, None);
        assert_eq!(
            override_path(Path::new("w"), "app-0", ""),
            PathBuf::from("w/instances/app-0/default.override.yml")
        );
    }

    #[test]
    fn verbs_produce_exact_argv() {
        let st = stack();
        let projects = projects(&inst(), &st, Path::new("/proj"), Path::new("/proj/.work"));
        let core = &projects["core"];
        let base = format!(
            "{}",
            PathBuf::from("/proj")
                .join("stack/core.compose.yml")
                .display()
        );
        let ov = format!("{}", core.override_path.display());
        let prefix = [
            "docker",
            "compose",
            "-p",
            "app-1-core",
            "-f",
            &base,
            "-f",
            &ov,
        ];
        let join = |rest: &[&str]| -> Vec<String> {
            prefix
                .iter()
                .chain(rest.iter())
                .map(|s| s.to_string())
                .collect()
        };
        assert_eq!(
            core.up(&["db", "migrate"], false),
            join(&["up", "-d", "--no-deps", "postgres", "migrate"])
        );
        assert_eq!(
            core.up(&["db"], true),
            join(&["up", "-d", "--no-deps", "--force-recreate", "postgres"])
        );
        assert_eq!(core.start(&["db"]), join(&["start", "postgres"]));
        assert_eq!(core.stop("db", 30), join(&["stop", "-t", "30", "postgres"]));
        assert_eq!(
            core.restart("db", 10),
            join(&["restart", "-t", "10", "postgres"])
        );
        assert_eq!(
            core.remove(&["migrate"]),
            join(&["rm", "-f", "-s", "migrate"])
        );
        assert_eq!(core.down(false), join(&["down"]));
        assert_eq!(core.down(true), join(&["down", "--volumes"]));
        assert_eq!(core.ps().argv(), join(&["ps", "-a", "--format", "json"]));
        assert_eq!(
            core.inspect(&["app-1-core-postgres-1".into()]).argv(),
            [
                "docker",
                "inspect",
                "--type",
                "container",
                "app-1-core-postgres-1"
            ]
            .map(String::from)
        );
        let logs = core.logs("db", Path::new("/proj"));
        assert_eq!(
            logs.argv,
            join(&["logs", "-f", "--tail", "200", "postgres"])
        );
        assert_eq!(logs.title.as_deref(), Some("logs: db"));
        let apis = &projects["apis"];
        assert_eq!(apis.up(&["api"], false)[0], "podman");
        assert_eq!(apis.up(&["api"], false)[3], "app-1-apis");
    }

    #[test]
    fn base_targets_handle_every_ports_spelling() {
        let text = r#"
services:
  s:
    ports:
      - "80"
      - "8080:81"
      - "127.0.0.1:8080:82/udp"
      - 83
      - target: 84
        published: 8084
        protocol: tcp
      - target: 85
      - "9000-9001:86-87"
    expose: ["90", 91, "80"]
  t: {}
"#;
        let t = base_targets(text, "s").unwrap();
        let got: Vec<(u16, Option<&str>, bool)> = t
            .iter()
            .map(|p| (p.target, p.protocol.as_deref(), p.published))
            .collect();
        assert_eq!(
            got,
            vec![
                (80, None, false),
                (81, None, true),
                (82, Some("udp"), true),
                (83, None, false),
                (84, Some("tcp"), true),
                (85, None, false),
                (86, None, true),
                (90, None, false),
                (91, None, false),
            ]
        );
        assert!(base_targets(text, "t").unwrap().is_empty());
        assert!(base_targets(text, "missing").unwrap().is_empty());
        assert!(base_targets("services: [", "s").is_err());
    }

    #[test]
    fn override_carries_ports_env_hosts_labels_images_and_builds() {
        let st = stack();
        let i = inst();
        let projects = projects(&i, &st, Path::new("/proj"), Path::new("/proj/.work"));
        let core = &projects["core"];
        let mut images = IndexMap::new();
        images.insert("db".to_string(), "postgres@sha256:dddd".to_string());
        images.insert("api".to_string(), "api:01234567".to_string());
        let mut builds = IndexMap::new();
        builds.insert(
            "api".to_string(),
            SourceBuild {
                component: "api".into(),
                key: "api:01234567".into(),
                commit: "0123456789abcdef0123456789abcdef01234567".into(),
                dirty: None,
                git_ref: Some("main".into()),
                context: PathBuf::from("/w/api"),
                dockerfile: Some(PathBuf::from("/w/api/Dockerfile")),
                source: None,
            },
        );
        let mut texts = IndexMap::new();
        texts.insert(core.files[0].clone(), CORE_FILE.to_string());
        let inputs = OverrideInputs {
            images: &images,
            builds: &builds,
            gateway: Some("172.20.0.1"),
            base_texts: &texts,
        };
        let services = service_overrides(core, &i, &st, &inputs).unwrap();
        assert_eq!(services.len(), 2);
        let db = &services[0];
        assert_eq!(db.service, "postgres");
        assert_eq!(db.image.as_deref(), Some("postgres@sha256:dddd"));
        assert_eq!(
            db.ports,
            vec![(14100, 5432, None), (14101, 14101, None)],
            "first port maps to the exposed target, the second has no target and maps to itself"
        );
        assert_eq!(db.extra_hosts, vec!["dev.example.test:172.20.0.1"]);
        assert_eq!(db.labels[LABEL_INSTANCE], "app-1");
        assert_eq!(db.labels[LABEL_COMPONENT], "db");
        assert_eq!(db.labels[LABEL_GATEWAY], "172.20.0.1");
        assert_eq!(db.environment["CHAPPA_STACK_SLOT"], "1");
        assert!(db.build.is_none());
        let migrate = &services[1];
        assert!(migrate.ports.is_empty());
        assert_eq!(
            migrate.image, None,
            "no image known yet: the base file's stands"
        );

        let apis = &projects["apis"];
        let services = service_overrides(apis, &i, &st, &inputs).unwrap();
        let api = &services[0];
        assert_eq!(api.image.as_deref(), Some("api:01234567"));
        assert_eq!(api.build.as_ref().unwrap().context, PathBuf::from("/w/api"));
        assert_eq!(
            api.environment["DB_URL"], "postgres://host.containers.internal:14100/app",
            "podman side of {{host}}"
        );
        assert_eq!(api.environment["SELF"], "api:01234567");
        assert_eq!(
            api.ports,
            vec![(14102, 14102, None)],
            "no base text: host maps to itself"
        );

        let yaml = render_override(&services);
        assert!(yaml.starts_with("# Generated per instance"));
        let parsed: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
        let svc = &parsed["services"]["api"];
        assert_eq!(svc["image"].as_str(), Some("api:01234567"));
        assert_eq!(svc["ports"][0].as_str(), Some("14102:14102"));
        assert_eq!(
            svc["extra_hosts"][0].as_str(),
            Some("dev.example.test:172.20.0.1")
        );
        assert_eq!(
            svc["build"]["dockerfile"]
                .as_str()
                .map(|s| s.ends_with("Dockerfile")),
            Some(true)
        );
        assert_eq!(svc["labels"][LABEL_GATEWAY].as_str(), Some("172.20.0.1"));

        let no_gateway = OverrideInputs {
            images: &images,
            builds: &builds,
            gateway: None,
            base_texts: &texts,
        };
        let services = service_overrides(core, &i, &st, &no_gateway).unwrap();
        assert!(services[0].extra_hosts.is_empty());
        assert!(!services[0].labels.contains_key(LABEL_GATEWAY));

        let empty = IndexMap::new();
        let unresolved = OverrideInputs {
            images: &empty,
            builds: &builds,
            gateway: None,
            base_texts: &texts,
        };
        let err = service_overrides(apis, &i, &st, &unresolved).unwrap_err();
        assert!(
            matches!(err, OverrideError::Placeholder { ref component, .. } if component == "api")
        );
    }

    #[test]
    fn base_file_findings_flag_self_published_ports() {
        let st = stack();
        let projects = projects(&inst(), &st, Path::new("/proj"), Path::new("/proj/.work"));
        let core = &projects["core"];
        let mut texts = IndexMap::new();
        texts.insert(core.files[0].clone(), CORE_FILE.to_string());
        assert!(base_file_findings(core, &texts).is_empty());
        texts.insert(
            core.files[0].clone(),
            "services:\n  postgres:\n    ports: [\"5432:5432\"]\n".to_string(),
        );
        let f = base_file_findings(core, &texts);
        assert_eq!(f.len(), 1);
        assert!(f[0].starts_with("db: "));
        assert!(f[0].contains("5432"));
    }

    const PS_NDJSON: &str = r#"{"Command":"postgres","CreatedAt":"2026-11-13 10:00:00 +0000 UTC","ExitCode":0,"Health":"healthy","ID":"aaa","Image":"postgres@sha256:dddd","Labels":"","LocalVolumes":"1","Mounts":"","Name":"app-1-core-postgres-1","Names":"","Networks":"app-1-core_default","Ports":"","Project":"app-1-core","Publishers":null,"RunningFor":"2 hours ago","Service":"postgres","Size":"0B","State":"running","Status":"Up 2 hours (healthy)"}
{"ExitCode":0,"Health":"","Name":"app-1-core-migrate-1","Service":"migrate","State":"exited","Status":"Exited (0) 2 hours ago"}
"#;
    const PS_ARRAY: &str = r#"[{"Name":"app-1-apis-api-1","Service":"api","State":"restarting","Health":"","ExitCode":1,"Status":"Restarting (1) 3 seconds ago"}]"#;

    #[test]
    fn ps_parsing_accepts_ndjson_and_arrays() {
        let rows = parse_ps(PS_NDJSON).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].service, "postgres");
        assert_eq!(rows[0].health, "healthy");
        assert_eq!(rows[1].state, "exited");
        let rows = parse_ps(PS_ARRAY).unwrap();
        assert_eq!(rows[0].state, "restarting");
        assert!(parse_ps("").unwrap().is_empty());
        assert!(parse_ps("not json").is_err());
        let a = actual_from(parse_ps(PS_NDJSON).unwrap().first(), None);
        assert!(a.exists && a.running && a.exit_code.is_none());
        assert_eq!(a.health, Health::Healthy);
        let m = actual_from(parse_ps(PS_NDJSON).unwrap().get(1), None);
        assert!(m.exists && !m.running);
        assert_eq!(m.exit_code, Some(0));
        let r = actual_from(parse_ps(PS_ARRAY).unwrap().first(), None);
        assert!(r.restarting);
        assert_eq!(
            instances_in(&parse_ps(PS_NDJSON).unwrap())
                .into_iter()
                .collect::<Vec<_>>(),
            ["app-1-core-migrate", "app-1-core-postgres"]
        );
    }

    const INSPECT: &str = r#"[
      {"Id":"aaa","Name":"/app-1-core-postgres-1","RestartCount":2,"Image":"sha256:imgid",
       "State":{"Status":"running","Running":true,"Restarting":false,"ExitCode":0,"StartedAt":"2026-11-13T08:00:00.123456789Z","Health":{"Status":"healthy"}},
       "Config":{"Image":"postgres@sha256:dddd","Labels":{"chappa.stack.component":"db","chappa.stack.gateway":"172.20.0.1"}},
       "HostConfig":{"ExtraHosts":["dev.example.test:172.20.0.1"]}},
      {"Id":"bbb","Name":"app-1-core-migrate-1","RestartCount":0,"Image":"sha256:other",
       "State":{"Status":"exited","Running":false,"Restarting":false,"ExitCode":3,"StartedAt":"0001-01-01T00:00:00Z","Health":null},
       "Config":{"Image":"registry.example/api:main","Labels":null},
       "HostConfig":{"ExtraHosts":null}}
    ]"#;

    #[test]
    fn inspect_parsing_fills_actual_and_the_baked_gateway() {
        let entries = parse_inspect(INSPECT).unwrap();
        assert_eq!(entries.len(), 2);
        let pg = &entries[0];
        assert_eq!(pg.name, "app-1-core-postgres-1", "leading slash stripped");
        assert_eq!(pg.restarts, 2);
        assert_eq!(pg.health, Health::Healthy);
        assert_eq!(
            pg.baked_gateway(Some("dev.example.test")).as_deref(),
            Some("172.20.0.1")
        );
        assert_eq!(pg.image_ref, "postgres@sha256:dddd");
        let a = actual_from(None, Some(pg));
        assert!(a.running && a.exit_code.is_none());
        assert_eq!(a.restarts, 2);
        let mg = &entries[1];
        assert_eq!(mg.started_at, None, "the zero time is no start");
        assert_eq!(mg.health, Health::Unknown);
        assert_eq!(mg.baked_gateway(Some("dev.example.test")), None);
        let a = actual_from(None, Some(mg));
        assert_eq!(a.exit_code, Some(3));
        // extra_hosts alone (no label) still yields the gateway, case-insensitively
        let mut only_hosts = pg.clone();
        only_hosts.labels.clear();
        only_hosts.extra_hosts = vec!["DEV.example.test:host-gateway".into()];
        assert_eq!(
            only_hosts
                .baked_gateway(Some("dev.example.test"))
                .as_deref(),
            Some("host-gateway")
        );
        assert_eq!(only_hosts.baked_gateway(None), None);
        assert!(parse_inspect("").unwrap().is_empty());
    }

    #[test]
    fn utc_parsing_and_uptime_text() {
        assert_eq!(parse_utc("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_utc("2026-11-13T08:00:00.123456789Z"),
            Some(1_794_556_800)
        );
        assert_eq!(parse_utc("2026-11-13T09:00:00+01:00"), Some(1_794_556_800));
        assert_eq!(parse_utc("2026-11-13T07:30:00-00:30"), Some(1_794_556_800));
        assert_eq!(parse_utc("garbage"), None);
        let now = 1_794_556_800 + 2 * 3600 + 5 * 60;
        assert_eq!(
            uptime_text("2026-11-13T08:00:00Z", now).as_deref(),
            Some("2h 5m")
        );
        assert_eq!(
            uptime_text("2026-11-13T08:00:00Z", 1_794_556_800 + 40).as_deref(),
            Some("40s")
        );
        assert_eq!(
            uptime_text("2026-11-13T08:00:00Z", 1_794_556_800 + 600).as_deref(),
            Some("10m")
        );
        assert_eq!(
            uptime_text("2026-11-10T08:00:00Z", now).as_deref(),
            Some("3d 2h")
        );
        assert_eq!(uptime_text("0001-01-01T00:00:00Z", now), None);
        // round trip with the intent formatter
        assert_eq!(
            crate::intent::format_utc(1_794_556_800),
            "2026-11-13T08:00:00Z"
        );
    }

    fn engine(kind: EngineKind) -> EngineRef {
        EngineRef {
            name: kind.name().into(),
            kind,
            context: None,
        }
    }

    #[test]
    fn gateway_rules_per_engine() {
        let cli = FakeCli::new()
            .on(&["docker", "info", "--format", "{{.OSType}}"], "windows\n")
            .on(
                &["docker", "network", "inspect", "nat"],
                "[{\"Subnet\":\"172.20.0.0/16\",\"Gateway\":\"172.20.0.1\"}]\n",
            )
            .on(
                &["podman", "machine", "inspect"],
                "[{\"Name\":\"podman-machine-default\"}]",
            )
            .on(
                &[
                    "podman",
                    "machine",
                    "ssh",
                    "--",
                    "getent",
                    "hosts",
                    "host.containers.internal",
                ],
                "192.168.127.254  host.containers.internal\n",
            );
        let r = CliGatewayResolver { cli: &cli };
        assert_eq!(
            r.gateway(&engine(EngineKind::Docker), Some(ContainerOs::Linux))
                .unwrap(),
            Gateway::alias()
        );
        assert_eq!(
            r.gateway(&engine(EngineKind::Docker), None).unwrap(),
            Gateway::literal("172.20.0.1"),
            "OS mode asked of the daemon, then the nat gateway"
        );
        assert_eq!(
            r.gateway(&engine(EngineKind::Podman), None).unwrap(),
            Gateway::literal("192.168.127.254")
        );
        let native = FakeCli::new().failing(&["podman", "machine", "inspect"], 125, "no machine");
        let r = CliGatewayResolver { cli: &native };
        assert_eq!(
            r.gateway(&engine(EngineKind::Podman), None).unwrap(),
            Gateway::alias()
        );
        assert!(r.gateway(&engine(EngineKind::Terraform), None).is_err());
        let fake = FakeGateways::default();
        assert_eq!(
            fake.gateway(&engine(EngineKind::Docker), None).unwrap(),
            Gateway::alias()
        );
    }
}
