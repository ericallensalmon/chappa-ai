//! `chappa-stack relay`: the relay alone, as a source.
//!
//! For a stack that is not defined in a project (a machine-wide service
//! run by its own scripts, shared by every repo), the relay runs on its own:
//! `chappa-stack relay --engine docker --ports 14000-14099`. It is a source
//! like `chappa-stack serve` (the same identity proof and bearer token, the
//! same launched and attached modes) whose Environment view holds one
//! block: the Relay section, its banners, alerts and actions. The rules are
//! the relay's own ([`crate::relay`]): it binds only gateway addresses,
//! never a wildcard, serves only the gateway's own network, and follows the
//! gateway when it moves. On Windows the firewall check and its actions
//! ([`crate::relay_firewall`]) and "Test from a container"
//! ([`crate::relay_probe`]) come with it.
//!
//! Unlike a stack's relay, which listens only while the instance has
//! something running, this one listens on every given port from the start:
//! it does not know what runs behind it, and a connection to a port nothing
//! serves on loopback is closed.
//!
//! A refresher thread looks every [`TICK`]: each engine's gateway (asked of
//! the engine, remembered for a minute), the listeners reconciled to it,
//! and the firewall check, which runs again when the relay binds, when the
//! gateway or the ports change, and otherwise at most every minute.

use indexmap::IndexMap;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use stack_core::compose::GatewayResolver;
use stack_core::definition::EngineRef;
use stack_core::driver::EngineKind;
use stack_core::intent::utc_now;

use crate::env_view;
use crate::identity::{Token, IDENTITY_PATH, MANIFEST_PATH};
use crate::relay::{self, Relay, RelayProblem, RelayState};
use crate::relay_firewall::{self, normalize, FirewallCheck, PortRange};
use crate::relay_probe::{self, ContainerTest};
use crate::server::{error, identity_reply, Caller, Handler, Reply, CONTRACT};

pub const ENVIRONMENT_PATH: &str = "/api/environment";
pub const ACTIONS_PATH: &str = "/api/actions";
/// How often the app polls the view, in seconds.
pub const POLL_SECONDS: u64 = 15;
/// How often the refresher looks at the gateways.
pub const TICK: Duration = Duration::from_secs(5);
/// The most ports one relay listens on (a listener and its thread each).
pub const MAX_PORTS: u32 = 1024;

/// `14000-14099`, `14000,14010-14020`: ports and inclusive runs, comma
/// separated, at most [`MAX_PORTS`] in all.
pub fn parse_ports(spec: &str) -> Result<Vec<PortRange>, String> {
    let mut out = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if part.eq_ignore_ascii_case("any") {
            return Err("--ports takes ports and ranges, not `any`".to_string());
        }
        out.push(PortRange::parse(part).ok_or_else(|| {
            format!("--ports: `{part}` is not a port (1-65535) or a range like 14000-14099")
        })?);
    }
    let out = normalize(out);
    if out.is_empty() {
        return Err("--ports names no port".to_string());
    }
    let total: u32 = out.iter().map(PortRange::count).sum();
    if total > MAX_PORTS {
        return Err(format!(
            "--ports names {total} ports; the relay listens on at most {MAX_PORTS}"
        ));
    }
    Ok(out)
}

/// `docker`, `podman`, or either with a context: `docker@remote`.
pub fn parse_engine(spec: &str) -> Result<EngineRef, String> {
    let (kind, context) = match spec.split_once('@') {
        Some((k, c)) => (k.trim(), Some(c.trim())),
        None => (spec.trim(), None),
    };
    let kind = match kind {
        "docker" => EngineKind::Docker,
        "podman" => EngineKind::Podman,
        other => {
            return Err(format!(
                "--engine: `{other}` is not a container engine (docker or podman)"
            ))
        }
    };
    let context = match context {
        None => None,
        Some(c)
            if !c.is_empty()
                && !c.starts_with('-')
                && c.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)) =>
        {
            Some(c.to_string())
        }
        Some(c) => return Err(format!("--engine: `{c}` is not a context name")),
    };
    let name = match &context {
        Some(c) => format!("{}@{c}", kind.name()),
        None => kind.name().to_string(),
    };
    Ok(EngineRef {
        name,
        kind,
        context,
    })
}

/// What one relay serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayConfig {
    pub engines: Vec<EngineRef>,
    pub ports: Vec<PortRange>,
}

impl RelayConfig {
    /// Engines once each, in the order given.
    pub fn new(engines: Vec<EngineRef>, ports: Vec<PortRange>) -> RelayConfig {
        let mut unique: Vec<EngineRef> = Vec::new();
        for e in engines {
            if !unique.iter().any(|u| u.name == e.name) {
                unique.push(e);
            }
        }
        RelayConfig {
            engines: unique,
            ports,
        }
    }

    fn port_set(&self) -> BTreeSet<u16> {
        self.ports.iter().flat_map(|r| r.lo..=r.hi).collect()
    }
}

/// The standalone relay source.
pub struct RelaySource {
    token: Token,
    /// The address string the identity proof covers.
    addr: String,
    config: RelayConfig,
    gateways: Arc<dyn GatewayResolver + Send + Sync>,
    relay: Mutex<Relay>,
    /// What the last look planned (problems, engines needing no relay).
    planned: Mutex<RelayState>,
    firewall: FirewallCheck,
    test: ContainerTest,
    stop: AtomicBool,
}

impl RelaySource {
    pub fn new(
        config: RelayConfig,
        token: Token,
        addr: String,
        gateways: Arc<dyn GatewayResolver + Send + Sync>,
        firewall: FirewallCheck,
        test: ContainerTest,
    ) -> Arc<RelaySource> {
        Arc::new(RelaySource {
            token,
            addr,
            config,
            gateways,
            relay: Mutex::new(Relay::default()),
            planned: Mutex::new(RelayState::default()),
            firewall,
            test,
            stop: AtomicBool::new(false),
        })
    }

    /// Look once: the gateways, the listeners, the firewall.
    pub fn tick(&self) {
        let mut found: IndexMap<String, String> = IndexMap::new();
        let mut failed: Vec<RelayProblem> = Vec::new();
        for e in &self.config.engines {
            match self.gateways.gateway(e, None) {
                Ok(g) => {
                    found.insert(e.name.clone(), g.address);
                }
                Err(why) => failed.push(RelayProblem {
                    engine: e.name.clone(),
                    text: format!(
                        "relay for {}: cannot ask the engine for its gateway ({why})",
                        e.name
                    ),
                }),
            }
        }
        let names: Vec<String> = self.config.engines.iter().map(|e| e.name.clone()).collect();
        let (wanted, mut planned) = relay::plan(
            &names,
            &found,
            &self.config.port_set(),
            &relay::reserved_ports(&self.addr),
        );
        planned
            .problems
            .retain(|p| !failed.iter().any(|f| f.engine == p.engine));
        planned.problems.extend(failed);
        let rebound = self
            .relay
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .reconcile(&wanted);
        self.firewall.want(
            relay::gateway_ips(&names, &found),
            self.config.ports.clone(),
            rebound,
        );
        *self.planned.lock().unwrap_or_else(|p| p.into_inner()) = planned;
    }

    /// Look now, then every [`TICK`] until [`RelaySource::shutdown`].
    pub fn start(self: &Arc<Self>) -> thread::JoinHandle<()> {
        self.tick();
        let me = self.clone();
        thread::spawn(move || {
            let step = Duration::from_millis(100);
            let mut waited = Duration::ZERO;
            while !me.stop.load(Ordering::SeqCst) {
                thread::sleep(step);
                waited += step;
                if waited >= TICK {
                    waited = Duration::ZERO;
                    me.tick();
                }
            }
        })
    }

    /// Stop the refresher and every listener.
    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.relay
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .stop_all();
    }

    pub fn manifest(&self) -> Value {
        json!({
            "contract": CONTRACT,
            "source": { "id": "relay", "label": "Relay", "version": env!("CARGO_PKG_VERSION") },
            "views": {
                "environment": { "list": ENVIRONMENT_PATH, "pollSeconds": POLL_SECONDS },
            },
            "actions": ACTIONS_PATH,
            "linkHosts": [],
        })
    }

    fn state(&self) -> RelayState {
        let planned = self
            .planned
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        self.relay
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .state(planned)
    }

    /// The engine and address "Test from a container" connects to: the
    /// lowest port listening for the first docker engine.
    fn test_target(&self) -> Option<(EngineRef, SocketAddr)> {
        let listening = self
            .relay
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .listening();
        listening.iter().find_map(|w| {
            self.config
                .engines
                .iter()
                .find(|e| e.name == w.engine && e.kind == EngineKind::Docker)
                .map(|e| (e.clone(), w.bind))
        })
    }

    /// The one-block Environment view.
    pub fn environment(&self) -> Value {
        let state = self.state();
        let mut v = json!({
            "freshness": { "asOf": utc_now() },
            "groups": [],
            "components": [],
            "facts": [env_view::relay_section(&state, "none (no engine's gateway is known yet)")],
            "alerts": env_view::relay_alerts(&state),
            "banners": [],
        });
        if let Some(fw) = self.firewall.view() {
            relay_firewall::decorate(&mut v, &fw);
        }
        if let Some((engine, target)) = self.test_target() {
            relay_probe::decorate(&mut v, &self.test, &engine, target);
        }
        v
    }

    fn action(&self, body: &Value) -> Reply {
        let Some(id) = body.get("id").and_then(Value::as_str) else {
            return error(400, "bad-request", "the body names no action `id`");
        };
        if !(FirewallCheck::is_action(id) || id == relay_probe::TEST_ACTION_ID) {
            return error(
                409,
                "not-offered",
                format!("`{id}` is not an action of the relay"),
            );
        }
        if body.get("confirmed").and_then(Value::as_bool) != Some(true) {
            return error(
                409,
                "confirmation-required",
                "this action must be confirmed",
            );
        }
        let fingerprint = body.pointer("/params/fingerprint").and_then(Value::as_str);
        let result = if id == relay_probe::TEST_ACTION_ID {
            match self.test_target() {
                Some((engine, target)) => self.test.run(&engine, target, fingerprint),
                None => Err((
                    409,
                    "the relay is not listening for a docker engine".to_string(),
                )),
            }
        } else {
            self.firewall.run(id, fingerprint)
        };
        match result {
            Ok(message) => (200, json!({ "message": message })),
            Err((status, message)) => error(status, "relay", message),
        }
    }
}

impl Handler for RelaySource {
    fn authorizes(&self, auth: Option<&str>) -> bool {
        self.token.authorizes(auth)
    }

    fn handle_as(
        &self,
        method: &str,
        url: &str,
        auth: Option<&str>,
        _caller: &Caller,
        body: &str,
    ) -> Reply {
        let (path, query) = url.split_once('?').unwrap_or((url, ""));
        if method == "GET" && path == IDENTITY_PATH {
            return identity_reply(&self.token, &self.addr, query);
        }
        if !self.token.authorizes(auth) {
            return error(401, "unauthorized", "a bearer token is required");
        }
        match (method, path) {
            ("GET", MANIFEST_PATH) => (200, self.manifest()),
            ("GET", ENVIRONMENT_PATH) => (200, self.environment()),
            ("POST", ACTIONS_PATH) => match serde_json::from_str::<Value>(body) {
                Ok(v) => self.action(&v),
                Err(e) => error(400, "bad-request", format!("the body is not JSON: {e}")),
            },
            (m, p) => error(404, "not-found", format!("no route for {m} {p}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports_are_single_ports_and_ranges_merged_and_capped() {
        let p = parse_ports("14000-14099").unwrap();
        assert_eq!(relay_firewall::ranges_text(&p), "14000-14099");
        let p = parse_ports("14010-14020, 14000,14021").unwrap();
        assert_eq!(relay_firewall::ranges_text(&p), "14000, 14010-14021");
        for bad in ["", ",", "0", "70000", "14100-14000", "any", "abc", "14000-"] {
            assert!(parse_ports(bad).is_err(), "{bad:?}");
        }
        assert!(parse_ports("10000-11023").is_ok());
        assert!(parse_ports("10000-11024")
            .unwrap_err()
            .contains("at most 1024"));
    }

    #[test]
    fn engines_are_docker_or_podman_with_an_optional_context() {
        let e = parse_engine("docker").unwrap();
        assert_eq!(
            (e.name.as_str(), e.kind, e.context),
            ("docker", EngineKind::Docker, None)
        );
        let e = parse_engine("podman@machine-1").unwrap();
        assert_eq!(e.name, "podman@machine-1");
        assert_eq!(e.context.as_deref(), Some("machine-1"));
        for bad in [
            "terraform",
            "",
            "docker@",
            "docker@-x",
            "docker@a b",
            "docker@a;b",
        ] {
            assert!(parse_engine(bad).is_err(), "{bad:?}");
        }
        let c = RelayConfig::new(
            vec![
                parse_engine("docker").unwrap(),
                parse_engine("docker").unwrap(),
            ],
            parse_ports("14000").unwrap(),
        );
        assert_eq!(c.engines.len(), 1);
    }
}
