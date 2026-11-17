//! The checks before `up`, run for real against the engines.
//!
//! Each engine of an instance is checked on its own: whether it answers,
//! whether a docker daemon is in the container mode the group needs, and
//! whether its memory fits what is about to start **plus** what other
//! instances already run on it. Ports are checked host-wide (a bind probe on
//! loopback, plus the ports other running instances own by their layout),
//! the hostname against the hosts file, and the registry login when a push
//! is planned. A failed check says why with the numbers, and carries the
//! fixing command when one exists, so the view can offer it as a terminal
//! action.
//!
//! Memory budgets differ per engine: a podman machine's free memory is not
//! the host's. The figures come from the engines themselves (`docker info`,
//! `podman info`, `podman machine inspect`); when an engine reports no free
//! figure, the budget is its total minus what other instances reserve.
//!
//! Nothing here fixes anything. The hosts file is never edited, no login is
//! attempted, no daemon is switched.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::cli::{engine_invocation, CliError, EngineCli, Invocation};
use crate::definition::{ContainerOs, EngineRef, StackDef};
use crate::driver::{Check, Checker, EngineKind, Outcome, ResolvedInstance};
use crate::hosts::{self, HostOs};
use crate::intent::{Desired, IntentRecord, INSTANCES_DIR};
use crate::ports;

// ------------------------------------------------------------- engine facts

/// What one engine said about itself.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineFacts {
    pub reachable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<ContainerOs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_total_mb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_free_mb: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct DockerVersion {
    #[serde(rename = "Server", default)]
    server: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct DockerInfo {
    #[serde(rename = "OSType", default)]
    os_type: String,
    #[serde(rename = "MemTotal", default)]
    mem_total: u64,
    #[serde(rename = "ServerVersion", default)]
    server_version: String,
    #[serde(rename = "ServerErrors", default)]
    server_errors: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct PodmanInfo {
    #[serde(default)]
    host: PodmanHost,
    #[serde(default)]
    version: PodmanVersion,
}

#[derive(Debug, Default, Deserialize)]
struct PodmanHost {
    #[serde(default)]
    os: String,
    #[serde(rename = "memTotal", default)]
    mem_total: u64,
    #[serde(rename = "memFree", default)]
    mem_free: u64,
}

#[derive(Debug, Default, Deserialize)]
struct PodmanVersion {
    #[serde(rename = "Version", default)]
    version: String,
}

fn bytes_to_mb(b: u64) -> u64 {
    b / (1024 * 1024)
}

fn unreachable(reason: String) -> EngineFacts {
    EngineFacts {
        reachable: false,
        reason: Some(reason),
        ..EngineFacts::default()
    }
}

fn failed_reason(e: &CliError) -> String {
    match e {
        CliError::Spawn { program, .. } => format!("`{program}` is not installed or not on PATH"),
        CliError::Timeout { seconds, .. } => format!("no answer within {seconds}s"),
        CliError::Failed { summary, .. } => summary.clone(),
        CliError::BadOutput { message, .. } => message.clone(),
    }
}

/// Ask `engine` about itself. Never errors: an engine that cannot be
/// reached is a fact too.
pub fn engine_facts(cli: &dyn EngineCli, engine: &EngineRef) -> EngineFacts {
    match engine.kind {
        EngineKind::Docker => {
            let version =
                match cli.run(&engine_invocation(engine, &["version", "--format", "json"])) {
                    Ok(out) => out,
                    Err(e) => return unreachable(failed_reason(&e)),
                };
            // `docker version` exits 1 with the client half printed when the
            // daemon is down; the JSON then has `"Server": null`.
            let parsed: Option<DockerVersion> = serde_json::from_str(version.stdout.trim()).ok();
            let server_up = parsed
                .as_ref()
                .is_some_and(|v| v.server.as_ref().is_some_and(|s| !s.is_null()));
            if !server_up {
                let hint = version.hint();
                return unreachable(if hint.is_empty() {
                    "the docker daemon is not running".to_string()
                } else {
                    hint
                });
            }
            let info = match cli.run_ok(&engine_invocation(engine, &["info", "--format", "json"])) {
                Ok(out) => out,
                Err(e) => return unreachable(failed_reason(&e)),
            };
            let info: DockerInfo = match serde_json::from_str(info.stdout.trim()) {
                Ok(i) => i,
                Err(e) => return unreachable(format!("docker info: {e}")),
            };
            if let Some(err) = info.server_errors.first() {
                return unreachable(err.clone());
            }
            EngineFacts {
                reachable: true,
                reason: None,
                os: ContainerOs::parse(&info.os_type),
                version: Some(info.server_version).filter(|v| !v.is_empty()),
                mem_total_mb: Some(bytes_to_mb(info.mem_total)).filter(|m| *m > 0),
                mem_free_mb: None,
            }
        }
        EngineKind::Podman => {
            let info = match cli.run(&engine_invocation(engine, &["info", "--format", "json"])) {
                Ok(out) if out.ok() => out,
                Ok(out) => {
                    let hint = out.hint();
                    return unreachable(if hint.is_empty() {
                        "podman is not reachable".to_string()
                    } else {
                        hint
                    });
                }
                Err(e) => return unreachable(failed_reason(&e)),
            };
            let info: PodmanInfo = match serde_json::from_str(info.stdout.trim()) {
                Ok(i) => i,
                Err(e) => return unreachable(format!("podman info: {e}")),
            };
            EngineFacts {
                reachable: true,
                reason: None,
                os: ContainerOs::parse(&info.host.os).or(Some(ContainerOs::Linux)),
                version: Some(info.version.version).filter(|v| !v.is_empty()),
                mem_total_mb: Some(bytes_to_mb(info.host.mem_total)).filter(|m| *m > 0),
                mem_free_mb: Some(bytes_to_mb(info.host.mem_free)).filter(|m| *m > 0),
            }
        }
        other => unreachable(format!("{other} is not a container engine")),
    }
}

// ----------------------------------------------------------------- memory

/// `5.1 GB` from 1024 up, `512 MB` below.
pub fn format_mb(mb: u64) -> String {
    if mb >= 1024 {
        format!("{:.1} GB", mb as f64 / 1024.0)
    } else {
        format!("{mb} MB")
    }
}

/// Can `needed_mb` more start on an engine that reports `facts`, while
/// other instances already reserve `elsewhere_mb` on it?
///
/// The budget is the engine's free figure when it reports one, else its
/// total minus the reservations; when both are known the smaller wins. With
/// no figure at all the check is skipped, never failed.
pub fn admission(engine: &str, needed_mb: u64, elsewhere_mb: u64, facts: &EngineFacts) -> Outcome {
    let from_total = facts.mem_total_mb.map(|t| t.saturating_sub(elsewhere_mb));
    let available = match (facts.mem_free_mb, from_total) {
        (Some(f), Some(t)) => f.min(t),
        (Some(f), None) => f,
        (None, Some(t)) => t,
        (None, None) => {
            return Outcome::Skipped {
                reason: format!("{engine} reports no memory figure"),
            }
        }
    };
    if needed_mb <= available {
        Outcome::Pass
    } else {
        let mut reason = format!(
            "needs ~{} on {engine}, {} free",
            format_mb(needed_mb),
            format_mb(available)
        );
        if elsewhere_mb > 0 {
            reason.push_str(&format!(
                " (other instances reserve {})",
                format_mb(elsewhere_mb)
            ));
        }
        if let Some(total) = facts.mem_total_mb {
            reason.push_str(&format!(" of {}", format_mb(total)));
        }
        Outcome::Fail { reason, fix: None }
    }
}

// ----------------------------------------------------------- other instances

/// Intent records of this stack's other instances that want anything running.
pub fn other_running_instances(
    stack_name: &str,
    work_dir: &Path,
    exclude: &str,
) -> Vec<IntentRecord> {
    let dir = work_dir.join(INSTANCES_DIR);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(record) = serde_json::from_str::<IntentRecord>(&text) else {
            continue;
        };
        if record.stack != stack_name || record.instance == exclude {
            continue;
        }
        if record
            .components
            .values()
            .any(|c| c.desired == Desired::Running)
        {
            out.push(record);
        }
    }
    out.sort_by_key(|r| r.slot);
    out
}

/// Ports other running instances own, by their layout: port → instance.
pub fn ports_owned_elsewhere(stack: &StackDef, others: &[IntentRecord]) -> BTreeMap<u16, String> {
    let mut out = BTreeMap::new();
    let Ok(layout) = ports::layout(stack) else {
        return out;
    };
    for rec in others {
        let Ok(map) = layout.instance_ports(rec.slot) else {
            continue;
        };
        for (component, ports) in map {
            if rec.desired(&component) != Desired::Running {
                continue;
            }
            for p in ports {
                out.entry(p).or_insert_with(|| rec.instance.clone());
            }
        }
    }
    out
}

/// `memoryMb` other running instances reserve, per engine name.
pub fn memory_elsewhere(stack: &StackDef, others: &[IntentRecord]) -> IndexMap<String, u64> {
    let mut out: IndexMap<String, u64> = IndexMap::new();
    for rec in others {
        for (name, def) in &stack.components {
            if rec.desired(name) != Desired::Running {
                continue;
            }
            let engine = stack.engine_name_for(name).unwrap_or_default();
            *out.entry(engine).or_insert(0) += def.memory_mb.unwrap_or(0) as u64;
        }
    }
    out
}

// ------------------------------------------------------------------- ports

pub trait PortProbe {
    /// True when nothing on this host listens on `port`.
    fn is_free(&self, port: u16) -> bool;
}

/// Binds the port on every address to find out. Both the wildcard and
/// loopback must bind, because a listener on either blocks the publish.
/// The IPv6 wildcard and loopback are tried too: on macOS and the BSDs an
/// IPv4 bind succeeds beside an IPv6 listener on the same port (a dev server
/// listening on `::`, as Node does by default, accepts IPv4 connections too),
/// so the IPv4 binds alone would read that port as free. Only "address in
/// use" counts there, since a host without IPv6 fails those binds for
/// reasons that say nothing about the port.
///
/// It binds a plain TCP socket and never calls `listen()`: listening on the
/// wildcard address is what makes Windows Firewall ask whether to allow the
/// program, and a check that only wants to know whether a port is taken must
/// not raise that prompt. A bind error of any kind ("address in use",
/// "access denied" for a reserved range) reads as taken, and the socket is
/// closed before the next bind, so the probe never holds the port.
///
/// `SO_REUSEADDR` is deliberately left off (std's `TcpListener::bind` turns
/// it on everywhere but Windows): on BSD-derived systems it lets a bind to a
/// specific address succeed beside a live wildcard listener, so a taken port
/// could read as free. Without it a port in `TIME_WAIT` may read as taken
/// for a minute, which errs on the safe side.
#[derive(Debug, Clone, Default)]
pub struct StdPortProbe;

impl StdPortProbe {
    /// Bind `addr` without listening; the socket is dropped (and the port
    /// released) before this returns.
    fn bind(addr: std::net::SocketAddr) -> std::io::Result<()> {
        use socket2::{Domain, Protocol, Socket, Type};
        let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
        if addr.is_ipv6() {
            // The IPv4 binds cover IPv4; this one asks about IPv6 alone.
            socket.set_only_v6(true)?;
        }
        socket.bind(&addr.into())
    }

    /// Whether `addr` binds (any error reads as taken).
    fn binds(addr: std::net::SocketAddr) -> bool {
        Self::bind(addr).is_ok()
    }

    /// Whether `addr` is not held by someone else: only "address in use"
    /// reads as taken (for the IPv6 addresses, see the type's docs).
    fn not_in_use(addr: std::net::SocketAddr) -> bool {
        !matches!(Self::bind(addr), Err(e) if e.kind() == std::io::ErrorKind::AddrInUse)
    }
}

impl PortProbe for StdPortProbe {
    fn is_free(&self, port: u16) -> bool {
        use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
        Self::binds(SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)))
            && Self::binds(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
            && Self::not_in_use(SocketAddr::from((Ipv6Addr::UNSPECIFIED, port)))
            && Self::not_in_use(SocketAddr::from((Ipv6Addr::LOCALHOST, port)))
    }
}

/// A scripted set of busy ports.
#[derive(Debug, Clone, Default)]
pub struct FakePorts(pub BTreeSet<u16>);

impl PortProbe for FakePorts {
    fn is_free(&self, port: u16) -> bool {
        !self.0.contains(&port)
    }
}

/// Which component of `inst` publishes `port`.
fn component_for_port(inst: &ResolvedInstance, port: u16) -> Option<&str> {
    inst.components
        .iter()
        .find(|(_, c)| c.ports.contains(&port))
        .map(|(n, _)| n.as_str())
}

/// The ports check: busy on this host, or owned by another instance.
pub fn ports_outcome(
    inst: &ResolvedInstance,
    ports_to_check: &[u16],
    probe: &dyn PortProbe,
    owned_elsewhere: &BTreeMap<u16, String>,
) -> Outcome {
    let mut problems = Vec::new();
    for &port in ports_to_check {
        let who = component_for_port(inst, port).unwrap_or("?");
        if let Some(instance) = owned_elsewhere.get(&port) {
            problems.push(format!("{port} ({who}) is owned by {instance}"));
        } else if !probe.is_free(port) {
            problems.push(format!("{port} ({who}) is in use on this host"));
        }
    }
    if problems.is_empty() {
        Outcome::Pass
    } else {
        Outcome::Fail {
            reason: problems.join("; "),
            fix: None,
        }
    }
}

// ---------------------------------------------------------------- registry

#[derive(Debug, Default, Deserialize)]
struct DockerConfig {
    #[serde(default)]
    auths: IndexMap<String, serde_json::Value>,
    #[serde(rename = "credsStore", default)]
    creds_store: Option<String>,
    #[serde(rename = "credHelpers", default)]
    cred_helpers: IndexMap<String, String>,
}

/// Does `key` (an `auths` or helper-listing key) name `registry`?
fn names_registry(key: &str, registry: &str) -> bool {
    let k = key
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/');
    let k = k.strip_suffix("/v1").unwrap_or(k);
    k.eq_ignore_ascii_case(registry) || k.starts_with(&format!("{registry}/"))
}

/// Whether the engine's credential store knows `registry`. For docker that
/// is `config.json` (`auths`, or the listing of its credential helper);
/// podman answers `login --get-login` itself. When neither says yes and
/// `probe_ref` is given, a `manifest inspect` of it decides. The fix is
/// the engine's own login command: credentials stay with the CLI.
pub fn registry_login(
    cli: &dyn EngineCli,
    engine: &EngineRef,
    registry: &str,
    docker_config_dir: Option<&Path>,
    probe_ref: Option<&str>,
) -> Outcome {
    let fix = Some(vec![
        engine.kind.name().to_string(),
        "login".to_string(),
        registry.to_string(),
    ]);
    let fail = |reason: String| Outcome::Fail {
        reason,
        fix: fix.clone(),
    };
    match engine.kind {
        EngineKind::Podman => {
            match cli.run(&engine_invocation(
                engine,
                &["login", "--get-login", registry],
            )) {
                Ok(out) if out.ok() && !out.stdout.trim().is_empty() => Outcome::Pass,
                Ok(_) => fail(format!("not logged in to {registry}")),
                Err(e) => fail(failed_reason(&e)),
            }
        }
        EngineKind::Docker => {
            let config_path = docker_config_dir
                .map(Path::to_path_buf)
                .or_else(|| std::env::var_os("DOCKER_CONFIG").map(PathBuf::from))
                .or_else(|| {
                    std::env::var_os("USERPROFILE")
                        .or_else(|| std::env::var_os("HOME"))
                        .map(|h| PathBuf::from(h).join(".docker"))
                })
                .map(|d| d.join("config.json"));
            let config: DockerConfig = config_path
                .and_then(|p| std::fs::read_to_string(p).ok())
                .and_then(|t| serde_json::from_str(&t).ok())
                .unwrap_or_default();
            if config.auths.iter().any(|(k, v)| {
                names_registry(k, registry)
                    && v.get("auth")
                        .and_then(|a| a.as_str())
                        .is_some_and(|a| !a.is_empty())
            }) {
                return Outcome::Pass;
            }
            let helper = config
                .cred_helpers
                .iter()
                .find(|(k, _)| names_registry(k, registry))
                .map(|(_, h)| h.clone())
                .or(config.creds_store.clone());
            if let Some(helper) = helper {
                let program = format!("docker-credential-{helper}");
                if let Ok(out) = cli.run(&Invocation::new(&program, &["list"])) {
                    if out.ok() {
                        if let Ok(list) =
                            serde_json::from_str::<IndexMap<String, String>>(out.stdout.trim())
                        {
                            if list.keys().any(|k| names_registry(k, registry)) {
                                return Outcome::Pass;
                            }
                        }
                    }
                }
            }
            if let Some(reference) = probe_ref {
                if let Ok(out) = cli.run(&engine_invocation(
                    engine,
                    &["manifest", "inspect", reference],
                )) {
                    if out.ok() {
                        return Outcome::Pass;
                    }
                }
            }
            fail(format!(
                "no login for {registry} in the docker credential store"
            ))
        }
        other => Outcome::Skipped {
            reason: format!("{other} has no registry login"),
        },
    }
}

// ----------------------------------------------------------------- checker

/// The real [`Checker`]: engines through the CLI, ports through a probe,
/// the hosts file from disk (or a given text), other instances from the
/// work folder.
pub struct EngineChecker<'a> {
    pub cli: &'a dyn EngineCli,
    pub stack: &'a StackDef,
    pub stack_name: &'a str,
    pub inst: &'a ResolvedInstance,
    pub work_dir: &'a Path,
    pub ports: &'a dyn PortProbe,
    /// Hosts file text to check instead of reading the machine's.
    pub hosts_text: Option<String>,
    pub docker_config_dir: Option<PathBuf>,
    /// A reference to `manifest inspect` when the credential store is silent.
    pub registry_probe_ref: Option<String>,
    facts: RefCell<IndexMap<String, EngineFacts>>,
}

impl<'a> EngineChecker<'a> {
    pub fn new(
        cli: &'a dyn EngineCli,
        stack_name: &'a str,
        stack: &'a StackDef,
        inst: &'a ResolvedInstance,
        work_dir: &'a Path,
        ports: &'a dyn PortProbe,
    ) -> Self {
        Self {
            cli,
            stack,
            stack_name,
            inst,
            work_dir,
            ports,
            hosts_text: None,
            docker_config_dir: None,
            registry_probe_ref: None,
            facts: RefCell::new(IndexMap::new()),
        }
    }

    /// Facts for an engine of the instance, fetched once.
    pub fn facts_for(&self, engine_name: &str) -> EngineFacts {
        if let Some(f) = self.facts.borrow().get(engine_name) {
            return f.clone();
        }
        let engine = self
            .inst
            .engines()
            .into_iter()
            .find(|e| e.name == engine_name)
            .or_else(|| self.stack.engine_ref(engine_name));
        let facts = match engine {
            Some(e) => engine_facts(self.cli, &e),
            None => unreachable(format!("unknown engine `{engine_name}`")),
        };
        self.facts
            .borrow_mut()
            .insert(engine_name.to_string(), facts.clone());
        facts
    }

    /// Every engine's facts, in instance order.
    pub fn all_facts(&self) -> IndexMap<String, EngineFacts> {
        self.inst
            .engines()
            .iter()
            .map(|e| (e.name.clone(), self.facts_for(&e.name)))
            .collect()
    }

    fn others(&self) -> Vec<IntentRecord> {
        other_running_instances(self.stack_name, self.work_dir, &self.inst.id)
    }
}

impl Checker for EngineChecker<'_> {
    fn check(&self, check: &Check) -> Outcome {
        match check {
            Check::EngineReachable { engine } => {
                let facts = self.facts_for(engine);
                if !facts.reachable {
                    return Outcome::Fail {
                        reason: facts.reason.unwrap_or_else(|| "not reachable".to_string()),
                        fix: None,
                    };
                }
                if let (Some(needed), Some(actual)) = (self.stack.os_for_engine(engine), facts.os) {
                    if needed != actual {
                        return Outcome::Fail {
                            reason: format!(
                                "{engine} runs {actual} containers; its group needs {needed} containers"
                            ),
                            fix: None,
                        };
                    }
                }
                Outcome::Pass
            }
            Check::Memory { engine, needed_mb } => {
                let facts = self.facts_for(engine);
                if !facts.reachable {
                    return Outcome::Skipped {
                        reason: format!("{engine} is not reachable"),
                    };
                }
                let elsewhere = memory_elsewhere(self.stack, &self.others())
                    .get(engine)
                    .copied()
                    .unwrap_or(0);
                admission(engine, *needed_mb as u64, elsewhere, &facts)
            }
            Check::PortsFree { ports } => {
                let owned = ports_owned_elsewhere(self.stack, &self.others());
                ports_outcome(self.inst, ports, self.ports, &owned)
            }
            Check::RegistryLogin { registry } => {
                // The engine that pushes is the one the source-built component
                // runs on; failing that, the first container engine.
                let pushing = self
                    .inst
                    .components
                    .values()
                    .find(|c| {
                        c.mode == Some(crate::definition::Mode::Source)
                            && c.engine.kind.is_container_engine()
                            && !self.stack.image_dependents(&c.id).is_empty()
                    })
                    .map(|c| c.engine.clone());
                let engine = pushing.or_else(|| {
                    self.inst
                        .engines()
                        .into_iter()
                        .find(|e| e.kind.is_container_engine())
                });
                match engine {
                    Some(e) => registry_login(
                        self.cli,
                        &e,
                        registry,
                        self.docker_config_dir.as_deref(),
                        self.registry_probe_ref.as_deref(),
                    ),
                    None => Outcome::Skipped {
                        reason: "no container engine pushes".to_string(),
                    },
                }
            }
            Check::HostnameResolves { hostname } => {
                let check = match &self.hosts_text {
                    Some(text) => hosts::check_hosts_text(text, hostname, HostOs::current()),
                    None => hosts::check_hosts_file(hostname),
                };
                if check.ok() {
                    Outcome::Pass
                } else {
                    Outcome::Fail {
                        reason: check.reason(),
                        fix: check.fix.map(|f| f.argv),
                    }
                }
            }
            Check::TerraformInit { dir } => Outcome::Skipped {
                reason: format!(
                    "terraform components ({}) are not driven yet",
                    dir.display()
                ),
            },
        }
    }
}

/// One line per engine, the way the view header shows it:
/// `windows: docker reachable (v29.6) · linux: podman reachable (4.6 GB free of 11.7 GB)`.
pub fn engine_summary(facts: &IndexMap<String, EngineFacts>) -> String {
    facts
        .iter()
        .map(|(name, f)| {
            let os = f.os.map(|o| format!("{o}: ")).unwrap_or_default();
            if !f.reachable {
                return format!(
                    "{os}{name} unreachable ({})",
                    f.reason.clone().unwrap_or_default()
                );
            }
            let mut extras = Vec::new();
            if let Some(v) = &f.version {
                extras.push(format!("v{v}"));
            }
            match (f.mem_free_mb, f.mem_total_mb) {
                (Some(free), Some(total)) => {
                    extras.push(format!("{} free of {}", format_mb(free), format_mb(total)))
                }
                (None, Some(total)) => extras.push(format!("{} total", format_mb(total))),
                (Some(free), None) => extras.push(format!("{} free", format_mb(free))),
                (None, None) => {}
            }
            if extras.is_empty() {
                format!("{os}{name} reachable")
            } else {
                format!("{os}{name} reachable ({})", extras.join(", "))
            }
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::FakeCli;
    use crate::definition::StacksDocument;
    use crate::driver::{checks_for, resolve, run_checks};
    use crate::intent::ComponentIntent;
    use std::net::TcpListener;

    fn stack() -> StackDef {
        let text = r#"
stacks:
  app:
    slots: { count: 3, portBase: 14000, portStride: 100 }
    hostname: dev.example.test
    registry: registry.example
    groups:
      core: { engine: docker, os: windows }
      apis: { engine: podman }
    components:
      db:
        group: core
        driver: compose
        file: s.yml
        modes: { prebuilt: { image: "postgres", tag: "16" } }
        memoryMb: 1024
      api:
        group: apis
        driver: compose
        file: s.yml
        modes:
          prebuilt: { image: "registry.example/api", tag: "{channel}" }
          source: { repo: ../api }
        memoryMb: 2048
      worker:
        group: apis
        driver: compose
        file: s.yml
        modes: { prebuilt: { image: "registry.example/worker", tag: "{channel}" } }
        memoryMb: 1024
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

    fn inst(slot: u32, api_source: bool) -> ResolvedInstance {
        let mut intent = IntentRecord::new("app", slot, "2026-01-01T00:00:00Z");
        for c in ["db", "api", "worker"] {
            intent.components.insert(
                c.into(),
                ComponentIntent {
                    desired: Desired::Running,
                    ..Default::default()
                },
            );
        }
        if api_source {
            intent.components["api"].mode = Some(crate::definition::Mode::Source);
        }
        resolve("app", &stack(), slot, Some(&intent)).unwrap()
    }

    const DOCKER_VERSION_UP: &str =
        r#"{"Client":{"Version":"29.6.2"},"Server":{"Version":"29.6.2","Os":"windows"}}"#;
    const DOCKER_VERSION_DOWN: &str = r#"{"Client":{"Version":"29.6.2"},"Server":null}"#;
    const DOCKER_INFO_WINDOWS: &str =
        r#"{"OSType":"windows","MemTotal":17179869184,"ServerVersion":"29.6.2","ServerErrors":[]}"#;
    const DOCKER_INFO_LINUX: &str =
        r#"{"OSType":"linux","MemTotal":8589934592,"ServerVersion":"29.6.2"}"#;
    const PODMAN_INFO: &str = r#"{"host":{"os":"linux","memTotal":12540706816,"memFree":4852711424},"version":{"Version":"5.3.1"}}"#;

    #[test]
    fn engine_facts_from_docker_and_podman_json() {
        let cli = FakeCli::new()
            .on(&["docker", "version"], DOCKER_VERSION_UP)
            .on(&["docker", "info"], DOCKER_INFO_WINDOWS)
            .on(&["podman", "info"], PODMAN_INFO);
        let d = engine_facts(&cli, &stack().engine_ref("docker").unwrap());
        assert_eq!(
            d,
            EngineFacts {
                reachable: true,
                reason: None,
                os: Some(ContainerOs::Windows),
                version: Some("29.6.2".into()),
                mem_total_mb: Some(16384),
                mem_free_mb: None,
            }
        );
        let p = engine_facts(&cli, &stack().engine_ref("podman").unwrap());
        assert_eq!(p.os, Some(ContainerOs::Linux));
        assert_eq!(p.mem_total_mb, Some(11959));
        assert_eq!(p.mem_free_mb, Some(4627));
        assert_eq!(p.version.as_deref(), Some("5.3.1"));

        let down = FakeCli::new().failing(
            &["docker", "version"],
            1,
            "failed to connect to the docker API at npipe:////./pipe/dockerDesktopLinuxEngine",
        );
        let f = engine_facts(&down, &stack().engine_ref("docker").unwrap());
        assert!(!f.reachable);
        assert!(f.reason.unwrap().starts_with("failed to connect"));

        let half = FakeCli::new().on(&["docker", "version"], DOCKER_VERSION_DOWN);
        let f = engine_facts(&half, &stack().engine_ref("docker").unwrap());
        assert_eq!(
            f.reason.as_deref(),
            Some("the docker daemon is not running")
        );

        let missing = FakeCli::new().without("podman");
        let f = engine_facts(&missing, &stack().engine_ref("podman").unwrap());
        assert_eq!(
            f.reason.as_deref(),
            Some("`podman` is not installed or not on PATH")
        );
        let summary = engine_summary(&IndexMap::from([
            ("docker".to_string(), d),
            ("podman".to_string(), p),
        ]));
        assert_eq!(
            summary,
            "windows: docker reachable (v29.6.2, 16.0 GB total) · linux: podman reachable (v5.3.1, 4.5 GB free of 11.7 GB)"
        );
    }

    #[test]
    fn admission_arithmetic_table() {
        let both = EngineFacts {
            reachable: true,
            mem_total_mb: Some(11959),
            mem_free_mb: Some(4628),
            ..EngineFacts::default()
        };
        assert_eq!(
            admission("podman", 4628, 0, &both),
            Outcome::Pass,
            "exactly the free amount fits"
        );
        let fail = admission("podman", 5222, 0, &both);
        assert_eq!(
            fail,
            Outcome::Fail {
                reason: "needs ~5.1 GB on podman, 4.5 GB free of 11.7 GB".into(),
                fix: None
            }
        );
        // reservations shrink the total-based budget below the free figure
        let fail = admission("podman", 4000, 8500, &both);
        assert_eq!(
            fail,
            Outcome::Fail {
                reason: "needs ~3.9 GB on podman, 3.4 GB free (other instances reserve 8.3 GB) of 11.7 GB".into(),
                fix: None
            }
        );
        let total_only = EngineFacts {
            reachable: true,
            mem_total_mb: Some(16384),
            ..EngineFacts::default()
        };
        assert_eq!(admission("docker", 1024, 15360, &total_only), Outcome::Pass);
        assert!(matches!(
            admission("docker", 1025, 15360, &total_only),
            Outcome::Fail { .. }
        ));
        let free_only = EngineFacts {
            reachable: true,
            mem_free_mb: Some(512),
            ..EngineFacts::default()
        };
        assert_eq!(
            admission("x", 512, 100_000, &free_only),
            Outcome::Pass,
            "reservations only matter against a total"
        );
        assert!(matches!(
            admission("x", 0, 0, &EngineFacts::default()),
            Outcome::Skipped { .. }
        ));
        assert_eq!(format_mb(1023), "1023 MB");
        assert_eq!(format_mb(1024), "1.0 GB");
    }

    fn write_record(work_dir: &Path, slot: u32, running: &[&str]) {
        let mut rec = IntentRecord::new("app", slot, "2026-01-01T00:00:00Z");
        for c in running {
            rec.components.insert(
                c.to_string(),
                ComponentIntent {
                    desired: Desired::Running,
                    ..Default::default()
                },
            );
        }
        rec.save(work_dir).unwrap();
    }

    #[test]
    fn other_instances_contribute_ports_and_memory_per_engine() {
        let dir = tempfile::tempdir().unwrap();
        let st = stack();
        write_record(dir.path(), 0, &["db", "api"]);
        write_record(dir.path(), 2, &[]);
        let mut other_stack = IntentRecord::new("other", 1, "2026-01-01T00:00:00Z");
        other_stack.components.insert(
            "x".into(),
            ComponentIntent {
                desired: Desired::Running,
                ..Default::default()
            },
        );
        other_stack.save(dir.path()).unwrap();
        std::fs::write(dir.path().join("instances").join("junk.json"), "{").unwrap();

        let others = other_running_instances("app", dir.path(), "app-1");
        assert_eq!(
            others
                .iter()
                .map(|r| r.instance.as_str())
                .collect::<Vec<_>>(),
            ["app-0"]
        );
        assert!(other_running_instances("app", dir.path(), "app-0").is_empty());

        let owned = ports_owned_elsewhere(&st, &others);
        assert_eq!(
            owned.get(&14000).map(String::as_str),
            Some("app-0"),
            "db of slot 0"
        );
        assert_eq!(
            owned.get(&14001).map(String::as_str),
            Some("app-0"),
            "api of slot 0"
        );
        assert!(
            !owned.contains_key(&14002),
            "worker is not wanted running there"
        );
        let mem = memory_elsewhere(&st, &others);
        assert_eq!(mem["docker"], 1024);
        assert_eq!(mem["podman"], 2048);
    }

    #[test]
    fn ports_outcome_names_component_and_holder() {
        let i = inst(1, false);
        let busy = FakePorts(BTreeSet::from([14102]));
        let mut owned = BTreeMap::new();
        owned.insert(14100u16, "app-0".to_string());
        let out = ports_outcome(&i, &[14100, 14101, 14102], &busy, &owned);
        assert_eq!(
            out,
            Outcome::Fail {
                reason: "14100 (db) is owned by app-0; 14102 (worker) is in use on this host"
                    .into(),
                fix: None
            }
        );
        assert_eq!(ports_outcome(&i, &[14101], &busy, &owned), Outcome::Pass);
    }

    #[test]
    fn std_port_probe_sees_a_real_listener_on_either_address_and_never_keeps_the_port() {
        // A listener on loopback, then one on the wildcard address: taken.
        // Once it is dropped the port is free and the probe must not keep
        // it: a real listener binds each address right after the probe,
        // twice over. A released ephemeral port can be picked up by another
        // process's outgoing connection at any moment, so that half retries
        // on a fresh port instead of failing on someone else's socket.
        //
        // macOS frees a closed socket's port a moment after the close, so
        // the probe may read a just-released port as taken for a while
        // (the safe side); the wait below allows for that.
        fn binds(host: &str, port: u16) -> Result<(), String> {
            TcpListener::bind((host, port)) // dropped on return
                .map(drop)
                .map_err(|e| format!("a listener on {host}:{port} after the probe: {e}"))
        }
        fn probe_reads_free(port: u16) -> Result<(), String> {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !StdPortProbe.is_free(port) {
                if std::time::Instant::now() > deadline {
                    return Err(format!("{port} still read as taken 5 s after the release"));
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Ok(())
        }
        let released_and_kept_free = |port: u16| -> Result<(), String> {
            for _ in 0..2 {
                probe_reads_free(port)?;
                binds("0.0.0.0", port)?;
                binds("127.0.0.1", port)?;
            }
            Ok(())
        };
        for host in ["127.0.0.1", "0.0.0.0"] {
            let mut tries = Vec::new();
            let freed = (0..5).any(|_| {
                let listener = TcpListener::bind((host, 0)).unwrap();
                let port = listener.local_addr().unwrap().port();
                assert!(!StdPortProbe.is_free(port), "{host}:{port} is listened on");
                drop(listener);
                let outcome = released_and_kept_free(port);
                let ok = outcome.is_ok();
                tries.extend(outcome.err());
                ok
            });
            assert!(
                freed,
                "{host}: a released port never read free and bindable: {tries:?}"
            );
        }
    }

    #[test]
    fn std_port_probe_sees_an_ipv6_listener() {
        // A dev server on `::` or `::1` holds the port for IPv4 clients too
        // on some systems, while an IPv4 bind beside it still succeeds.
        for host in ["::1", "::"] {
            let Ok(listener) = TcpListener::bind((host, 0)) else {
                continue; // no IPv6 on this host
            };
            let port = listener.local_addr().unwrap().port();
            assert!(
                !StdPortProbe.is_free(port),
                "[{host}]:{port} is listened on"
            );
        }
    }

    #[test]
    fn registry_login_reads_config_helpers_podman_and_probe() {
        let dir = tempfile::tempdir().unwrap();
        let docker = stack().engine_ref("docker").unwrap();
        let podman = stack().engine_ref("podman").unwrap();
        let fix = Some(vec![
            "docker".to_string(),
            "login".to_string(),
            "registry.example".to_string(),
        ]);

        // no config at all, no probe → fail with the login command
        let cli = FakeCli::new();
        let out = registry_login(&cli, &docker, "registry.example", Some(dir.path()), None);
        assert_eq!(
            out,
            Outcome::Fail {
                reason: "no login for registry.example in the docker credential store".into(),
                fix: fix.clone()
            }
        );

        // auths entry (https form, trailing slash) with an auth blob
        std::fs::write(
            dir.path().join("config.json"),
            r#"{"auths":{"https://registry.example/":{"auth":"dXNlcjpwYXNz"}}}"#,
        )
        .unwrap();
        assert_eq!(
            registry_login(&cli, &docker, "registry.example", Some(dir.path()), None),
            Outcome::Pass
        );
        assert!(matches!(
            registry_login(&cli, &docker, "other.example", Some(dir.path()), None),
            Outcome::Fail { fix: Some(f), .. } if f[2] == "other.example"
        ));

        // credsStore: the helper's listing decides
        std::fs::write(
            dir.path().join("config.json"),
            r#"{"credsStore":"desktop","auths":{"registry.example":{}}}"#,
        )
        .unwrap();
        let cli = FakeCli::new().on(
            &["docker-credential-desktop", "list"],
            r#"{"https://registry.example":"user","https://index.docker.io/v1/":"u"}"#,
        );
        assert_eq!(
            registry_login(&cli, &docker, "registry.example", Some(dir.path()), None),
            Outcome::Pass
        );
        assert!(matches!(
            registry_login(&cli, &docker, "ghcr.example", Some(dir.path()), None),
            Outcome::Fail { .. }
        ));
        assert_eq!(
            cli.calls_matching(&["docker-credential-desktop", "list"])
                .len(),
            2
        );

        // credHelpers for the registry wins over credsStore
        std::fs::write(
            dir.path().join("config.json"),
            r#"{"credsStore":"desktop","credHelpers":{"registry.example":"acr"}}"#,
        )
        .unwrap();
        let cli = FakeCli::new().on(
            &["docker-credential-acr", "list"],
            r#"{"registry.example":"token"}"#,
        );
        assert_eq!(
            registry_login(&cli, &docker, "registry.example", Some(dir.path()), None),
            Outcome::Pass
        );

        // probe fallback
        std::fs::write(dir.path().join("config.json"), "{}").unwrap();
        let cli = FakeCli::new().on(
            &["docker", "manifest", "inspect", "registry.example/api:main"],
            "{}",
        );
        assert_eq!(
            registry_login(
                &cli,
                &docker,
                "registry.example",
                Some(dir.path()),
                Some("registry.example/api:main")
            ),
            Outcome::Pass
        );
        let cli = FakeCli::new().failing(&["docker", "manifest", "inspect"], 1, "unauthorized");
        assert!(matches!(
            registry_login(
                &cli,
                &docker,
                "registry.example",
                Some(dir.path()),
                Some("registry.example/api:main")
            ),
            Outcome::Fail { .. }
        ));

        // podman
        let cli = FakeCli::new().on(
            &["podman", "login", "--get-login", "registry.example"],
            "user\n",
        );
        assert_eq!(
            registry_login(&cli, &podman, "registry.example", None, None),
            Outcome::Pass
        );
        let cli = FakeCli::new().failing(
            &["podman", "login", "--get-login"],
            125,
            "Error: not logged into registry.example",
        );
        assert_eq!(
            registry_login(&cli, &podman, "registry.example", None, None),
            Outcome::Fail {
                reason: "not logged in to registry.example".into(),
                fix: Some(vec![
                    "podman".into(),
                    "login".into(),
                    "registry.example".into()
                ])
            }
        );
    }

    #[test]
    fn engine_checker_end_to_end_over_fakes() {
        let dir = tempfile::tempdir().unwrap();
        write_record(dir.path(), 0, &["db", "api", "worker"]);
        let st = stack();
        let i = inst(1, true);
        let cli = FakeCli::new()
            .on(&["docker", "version"], DOCKER_VERSION_UP)
            .on(&["docker", "info"], DOCKER_INFO_LINUX)
            .on(&["podman", "info"], PODMAN_INFO)
            .on(&["podman", "login", "--get-login"], "user\n");
        let busy = FakePorts(BTreeSet::from([14102]));
        let mut checker = EngineChecker::new(&cli, "app", &st, &i, dir.path(), &busy);
        checker.hosts_text = Some("127.0.0.1 localhost\n".to_string());
        let checks = checks_for(&i, &st);
        let results = run_checks(&checker, &checks);
        let by_key: IndexMap<String, Outcome> = results
            .iter()
            .map(|r| (r.check.key(), r.outcome.clone()))
            .collect();
        assert_eq!(
            by_key["reachable:docker"],
            Outcome::Fail {
                reason: "docker runs linux containers; its group needs windows containers".into(),
                fix: None
            }
        );
        assert_eq!(by_key["reachable:podman"], Outcome::Pass);
        assert_eq!(
            by_key["memory:docker"],
            Outcome::Pass,
            "docker total 8 GB minus 1 GB elsewhere fits 1 GB"
        );
        // podman: api 2048 + worker 1024 wanted here, 3072 reserved by slot 0:
        // total 11959 - 3072 = 8887 is more than the 4628 free, so free wins
        // and the 3072 fit. Make the other slot heavier to see the refusal.
        assert_eq!(by_key["memory:podman"], Outcome::Pass);
        write_record(dir.path(), 2, &["db", "api", "worker"]);
        let fresh = EngineChecker::new(&cli, "app", &st, &i, dir.path(), &busy);
        assert_eq!(
            fresh.check(&Check::Memory {
                engine: "podman".into(),
                needed_mb: 3072
            }),
            Outcome::Pass
        );
        let out = fresh.check(&Check::Memory {
            engine: "podman".into(),
            needed_mb: 6000,
        });
        assert_eq!(
            out,
            Outcome::Fail {
                reason: "needs ~5.9 GB on podman, 4.5 GB free (other instances reserve 6.0 GB) of 11.7 GB".into(),
                fix: None
            }
        );
        assert_eq!(
            by_key["ports"],
            Outcome::Fail {
                reason: "14102 (worker) is in use on this host".into(),
                fix: None
            }
        );
        assert!(
            matches!(&by_key["hostname:dev.example.test"], Outcome::Fail { fix: Some(f), .. } if !f.is_empty())
        );
        assert_eq!(
            by_key["registry:registry.example"],
            Outcome::Pass,
            "podman is the first container engine here"
        );
        assert!(matches!(
            by_key["terraform-init:infra"],
            Outcome::Skipped { .. }
        ));
        // facts were fetched once per engine per checker (two checkers asked podman)
        assert_eq!(cli.calls_matching(&["docker", "info"]).len(), 1);
        assert_eq!(cli.calls_matching(&["podman", "info"]).len(), 2);
        let summary = engine_summary(&checker.all_facts());
        assert!(summary.starts_with("linux: docker reachable"));
    }
}
