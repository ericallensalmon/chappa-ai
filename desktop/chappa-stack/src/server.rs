//! The source protocol over HTTP: identity, manifest, the Environment
//! view, actions and jobs.
//!
//! | route | auth | answers |
//! |---|---|---|
//! | `GET /chappa/v1/identity?challenge=<64 hex>` | none (that is its point) | `{proof}` |
//! | `GET /chappa/v1/manifest` | bearer | the manifest |
//! | `GET /api/environment` | bearer | the Environment payload |
//! | `POST /api/actions` | bearer | `202 {jobId, run}` with `X-Chappa-Runs-Jobs: 1`, `202 {jobId}` without (the job runs headless), `409` unconfirmed / no longer offered / stale plan, `404` unknown target |
//! | `GET /api/jobs/{id}` | bearer | `{state: running\|done\|failed, log?, exitCode?, error?}` |
//! | `POST /api/jobs/{id}` | bearer | the pane's exit report, recorded; `200 {}` |
//!
//! The listener binds a loopback address only, and every route but the
//! identity check needs the bearer token, compared in constant time.
//! Requests are served by a small fixed pool of threads.
//!
//! No action answers `503` (outcome unknown): every refusal happens before
//! anything runs, and once a job is accepted its outcome is what
//! `GET jobs/{id}` reports.

use serde_json::{json, Value};
use std::io::Read;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

use stack_core::hosts::{check_hosts_file, check_hosts_text, HostOs};
use stack_core::intent::utc_now;
use stack_core::safety::data::DataRules;

use crate::actions::{all_actions, channel_for, Offered};
use crate::background::Background;
use crate::env_view::{self, EnvInputs};
use crate::identity::{is_valid_challenge, Token, IDENTITY_PATH, MANIFEST_PATH};
use crate::procs::ProcessTable;
use crate::project::{Loaded, ProjectConfig};
use crate::records::{self, JobRecord, JobStatus};
use crate::relay::{self, Relay};
use crate::world::World;

pub const ENVIRONMENT_PATH: &str = "/api/environment";
pub const ACTIONS_PATH: &str = "/api/actions";
pub const JOBS_PREFIX: &str = "/api/jobs/";
pub const RUNS_JOBS_HEADER: &str = "X-Chappa-Runs-Jobs";
/// How often the app should poll the Environment view, in seconds.
pub const POLL_SECONDS: u64 = 15;
/// Request threads.
pub const WORKERS: usize = 4;
/// The largest request body read (an exit report carries up to 4 MiB of
/// pane text).
pub const MAX_BODY: u64 = 8 * 1024 * 1024;
pub const CONTRACT: u64 = 1;

/// A loopback IP literal with a port; anything else is refused, so the
/// source can never listen where the LAN reaches it.
pub fn parse_listen_addr(text: &str) -> Result<SocketAddr, String> {
    let addr: SocketAddr = text
        .trim()
        .parse()
        .map_err(|_| format!("`{text}` is not an IP address with a port"))?;
    let loopback = match addr.ip() {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback(),
    };
    if !loopback {
        return Err(format!(
            "refusing to listen on {addr}: a source listens on loopback only"
        ));
    }
    Ok(addr)
}

/// One response: status and JSON body.
pub type Reply = (u16, Value);

fn error(status: u16, kind: &str, message: impl Into<String>) -> Reply {
    (status, json!({ "kind": kind, "message": message.into() }))
}

/// The source's state across requests.
pub struct Source {
    pub world: World,
    pub config: ProjectConfig,
    pub token: Token,
    /// The address string the identity proof covers.
    pub addr: String,
    pub background: Arc<Background>,
    relay: Mutex<Relay>,
    /// Serialises action acceptance, so two clicks cannot both plan
    /// against the same state.
    accepting: Mutex<()>,
}

impl Source {
    pub fn new(world: World, config: ProjectConfig, token: Token, addr: String) -> Arc<Source> {
        let background = Background::new();
        Self::with_background(world, config, token, addr, background)
    }

    pub fn with_background(
        world: World,
        mut config: ProjectConfig,
        token: Token,
        addr: String,
        background: Arc<Background>,
    ) -> Arc<Source> {
        if let Ok(abs) = std::path::absolute(&config.dir) {
            config.dir = abs;
        }
        Arc::new(Source {
            world,
            config,
            token,
            addr,
            background,
            relay: Mutex::new(Relay::default()),
            accepting: Mutex::new(()),
        })
    }

    fn load(&self) -> Result<Loaded, String> {
        Loaded::load(
            &self.config.dir,
            self.config.stack.as_deref(),
            self.config.slot,
        )
    }

    /// Answer one request. `auth` is the `Authorization` header value.
    pub fn handle(
        &self,
        method: &str,
        url: &str,
        auth: Option<&str>,
        runs_jobs: bool,
        body: &str,
    ) -> Reply {
        let (path, query) = url.split_once('?').unwrap_or((url, ""));
        if method == "GET" && path == IDENTITY_PATH {
            let challenge = query
                .split('&')
                .find_map(|kv| kv.strip_prefix("challenge="))
                .unwrap_or("");
            if !is_valid_challenge(challenge) {
                return error(
                    400,
                    "bad-challenge",
                    "challenge must be 64 lowercase hex characters",
                );
            }
            return (
                200,
                json!({ "proof": self.token.proof(challenge, &self.addr) }),
            );
        }
        if !self.token.authorizes(auth) {
            return error(401, "unauthorized", "a bearer token is required");
        }
        match (method, path) {
            ("GET", MANIFEST_PATH) => (200, self.manifest()),
            ("GET", ENVIRONMENT_PATH) => (200, self.environment()),
            ("POST", ACTIONS_PATH) => match serde_json::from_str::<Value>(body) {
                Ok(v) => self.action(&v, runs_jobs),
                Err(e) => error(400, "bad-request", format!("the body is not JSON: {e}")),
            },
            (m, p) if p.starts_with(JOBS_PREFIX) => {
                let id = &p[JOBS_PREFIX.len()..];
                match m {
                    "GET" => self.job_get(id),
                    "POST" => match serde_json::from_str::<Value>(body) {
                        Ok(v) => self.job_post(id, &v),
                        Err(e) => error(400, "bad-request", format!("the body is not JSON: {e}")),
                    },
                    _ => error(405, "method", format!("{m} is not allowed here")),
                }
            }
            (m, p) => error(404, "not-found", format!("no route for {m} {p}")),
        }
    }

    pub fn manifest(&self) -> Value {
        let (id, label, hosts) = match self.load() {
            Ok(loaded) => {
                let hosts = loaded
                    .runner(&self.world, &utc_now())
                    .instance(loaded.slot)
                    .map(|(inst, _)| env_view::link_hosts(&loaded, &inst))
                    .unwrap_or_default();
                (
                    loaded.stack_name.clone(),
                    format!("Stack {}", loaded.stack_name),
                    hosts,
                )
            }
            Err(_) => (
                self.config
                    .stack
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
                "Stack".to_string(),
                Vec::new(),
            ),
        };
        json!({
            "contract": CONTRACT,
            "source": { "id": format!("stack:{id}"), "label": label, "version": env!("CARGO_PKG_VERSION") },
            "views": { "environment": { "list": ENVIRONMENT_PATH, "pollSeconds": POLL_SECONDS } },
            "actions": ACTIONS_PATH,
            "jobs": format!("{JOBS_PREFIX}{{id}}"),
            "linkHosts": hosts,
        })
    }

    /// The Environment payload, built fresh. Never waits on a registry or
    /// on `docker info`: those come from the background caches.
    pub fn environment(&self) -> Value {
        let now = utc_now();
        let loaded = match self.load() {
            Ok(l) => l,
            Err(e) => return env_view::unavailable(&e, &now),
        };
        match self.environment_of(&loaded, &now) {
            Ok(v) => v,
            Err(e) => env_view::unavailable(&e, &now),
        }
    }

    fn environment_of(&self, loaded: &Loaded, now: &str) -> Result<Value, String> {
        let runner = loaded.runner(&self.world, now);
        let (inst, _) = runner.instance(loaded.slot).map_err(|e| e.to_string())?;
        let work_dir = loaded.work_dir();
        let table = ProcessTable::load(&work_dir, &inst.id);
        let obs = runner
            .observe(&inst, &table.statuses(&*self.world.procs))
            .map_err(|e| e.to_string())?;
        let reports = runner.status_with(&inst, &obs, &*self.world.http, &*self.background);
        let channel = channel_for(&obs, &self.config.channel);
        let offered: Vec<Offered> = if loaded.has_errors() {
            Vec::new()
        } else {
            all_actions(&runner, &inst, &obs, &channel)
        };
        let mut facts = indexmap::IndexMap::new();
        for e in inst.engines() {
            if !e.kind.is_container_engine() {
                continue;
            }
            self.background.want_engine(&e);
            facts.insert(e.name.clone(), self.background.engine_facts(&e.name));
        }
        let hosts = loaded
            .stack
            .hostname
            .as_deref()
            .map(|h| match &self.world.hosts_text {
                Some(text) => check_hosts_text(text, h, HostOs::current()),
                None => check_hosts_file(h),
            });
        let relay_state = loaded.stack.relay.as_ref().map(|def| {
            let gateways = obs
                .gateways
                .iter()
                .map(|(e, g)| (e.clone(), g.address.clone()))
                .collect();
            let anything_running = obs.actual.values().any(|a| a.running);
            let ports = if anything_running {
                inst.components
                    .values()
                    .filter(|c| !def.engines.contains(&c.engine.name))
                    .flat_map(|c| c.ports.iter().copied())
                    .collect()
            } else {
                Default::default()
            };
            let (wanted, planned) = relay::plan(&def.engines, &gateways, &ports);
            let mut relay = self.relay.lock().unwrap_or_else(|p| p.into_inner());
            relay.reconcile(&wanted);
            relay.state(planned)
        });
        let rules_hash = loaded
            .stack
            .data_safety
            .as_ref()
            .and_then(|d| d.rules.as_ref())
            .and_then(|r| DataRules::load(&loaded.dir.join(r)).ok())
            .map(|r| r.hash);
        let errors: Vec<(String, String)> = obs
            .notes
            .iter()
            .map(|n| ("engines".to_string(), n.clone()))
            .collect();
        let inputs = EnvInputs {
            loaded,
            inst: &inst,
            obs: &obs,
            reports: &reports,
            offered: &offered,
            channel: &channel,
            facts,
            hosts,
            relay: relay_state,
            processes: &table,
            exe: &self.world.exe,
            rules_hash,
            now_utc: now,
        };
        Ok(env_view::environment(&inputs, &errors))
    }

    /// `POST {actions}`: plan again, check it is what was confirmed, store
    /// it as a job, and hand back the pane to run it in (or run it here).
    pub fn action(&self, body: &Value, runs_jobs: bool) -> Reply {
        let _one_at_a_time = self.accepting.lock().unwrap_or_else(|p| p.into_inner());
        let Some(id) = body.get("id").and_then(Value::as_str) else {
            return error(400, "bad-request", "the body names no action `id`");
        };
        let view = body
            .pointer("/target/view")
            .and_then(Value::as_str)
            .unwrap_or("environment");
        if view != "environment" {
            return error(
                404,
                "unknown-target",
                format!("no actions on the `{view}` view"),
            );
        }
        let key = body
            .pointer("/target/key")
            .and_then(Value::as_str)
            .map(str::to_string);
        let confirmed = body.get("confirmed").and_then(Value::as_bool) == Some(true);
        let fingerprint = body.pointer("/params/fingerprint").and_then(Value::as_str);
        let now = utc_now();
        let loaded = match self.load() {
            Ok(l) => l,
            Err(e) => return error(409, "definition", e),
        };
        if loaded.has_errors() {
            return error(
                409,
                "definition",
                "the stack definition has errors; fix them first",
            );
        }
        let runner = loaded.runner(&self.world, &now);
        let (inst, _) = match runner.instance(loaded.slot) {
            Ok(i) => i,
            Err(e) => return error(409, "definition", e.to_string()),
        };
        let work_dir = loaded.work_dir();
        let table = ProcessTable::load(&work_dir, &inst.id);
        let obs = match runner.observe(&inst, &table.statuses(&*self.world.procs)) {
            Ok(o) => o,
            Err(e) => return error(409, "observe", format!("cannot look at the stack: {e}")),
        };
        let what = key
            .as_deref()
            .map(|k| format!("`{k}`"))
            .unwrap_or_else(|| "the stack".to_string());
        let channel = channel_for(&obs, &self.config.channel);
        let offered = all_actions(&runner, &inst, &obs, &channel);
        let Some(chosen) = offered.into_iter().find(|o| o.id == id && o.key == key) else {
            return error(
                409,
                "not-offered",
                format!("`{id}` is not offered for {what} now (the stack changed, or there is nothing to do); look again"),
            );
        };
        if !confirmed {
            return error(
                409,
                "confirmation-required",
                "a mutating action must be confirmed",
            );
        }
        let current = obs.state.fingerprint();
        if fingerprint != Some(current.as_str()) {
            return error(
                409,
                "stale-plan",
                format!(
                    "the stack changed since this was shown; what would run now is: {}",
                    chosen.confirm
                ),
            );
        }
        let job_id = match records::new_job_id(&now) {
            Ok(i) => i,
            Err(e) => return error(500, "job", e),
        };
        let argv = vec![
            self.world.exe.display().to_string(),
            "run-job".to_string(),
            job_id.clone(),
        ];
        let title = match &chosen.key {
            Some(k) => format!("{} {k}: {}", loaded.stack_name, chosen.label.to_lowercase()),
            None => format!("{}: {}", loaded.stack_name, chosen.label.to_lowercase()),
        };
        let rec = JobRecord {
            id: job_id.clone(),
            action: chosen.id.clone(),
            label: title.clone(),
            target: chosen.key.clone(),
            item: obs.intent.item.clone(),
            stack: loaded.stack_name.clone(),
            slot: loaded.slot,
            instance: inst.id.clone(),
            argv: argv.clone(),
            cwd: loaded.dir.clone(),
            mutates: true,
            confirm: chosen.confirm.clone(),
            status: JobStatus::Pending,
            exit_code: None,
            created_utc: now.clone(),
            started_utc: None,
            ended_utc: None,
            runner_pid: None,
            headless: !runs_jobs,
            log: JobRecord::log_path(&work_dir, &job_id),
            result: JobRecord::result_path(&work_dir, &job_id),
            work: chosen.work.clone(),
            report: None,
            error: None,
        };
        if let Err(e) = rec.save(&work_dir) {
            return error(500, "job", format!("cannot record the job: {e}"));
        }
        records::prune(&work_dir, &records::retention(&loaded.stack), &now);
        self.background.invalidate_facts();
        if runs_jobs {
            return (
                202,
                json!({
                    "jobId": job_id,
                    "run": {
                        "argv": argv,
                        "cwd": loaded.dir.display().to_string(),
                        "title": title,
                    },
                }),
            );
        }
        match self.world.spawner.spawn(&self.world, &loaded.dir, &job_id) {
            Ok(()) => (202, json!({ "jobId": job_id })),
            Err(e) => {
                let _ = JobRecord::update(&work_dir, &job_id, |r| {
                    r.status = JobStatus::Failed;
                    r.error = Some(e.clone());
                    r.ended_utc = Some(utc_now());
                });
                error(500, "spawn", e)
            }
        }
    }

    fn work_dir(&self) -> Result<PathBuf, Reply> {
        self.load()
            .map(|l| l.work_dir())
            .map_err(|e| error(409, "definition", e))
    }

    pub fn job_get(&self, id: &str) -> Reply {
        let work_dir = match self.work_dir() {
            Ok(w) => w,
            Err(r) => return r,
        };
        match JobRecord::load(&work_dir, id) {
            Ok(Some(rec)) => (
                200,
                records::api_state(&rec, |pid| {
                    pid == std::process::id() || self.world.procs.alive(pid)
                }),
            ),
            Ok(None) => error(404, "unknown-job", format!("no job {id}")),
            Err(e) => error(500, "job", e),
        }
    }

    pub fn job_post(&self, id: &str, body: &Value) -> Reply {
        let work_dir = match self.work_dir() {
            Ok(w) => w,
            Err(r) => return r,
        };
        if !matches!(JobRecord::load(&work_dir, id), Ok(Some(_))) {
            return error(404, "unknown-job", format!("no job {id}"));
        }
        match records::record_exit_report(&work_dir, id, body, &utc_now()) {
            Ok(_) => {
                self.background.invalidate_facts();
                (200, json!({}))
            }
            Err(e) => error(500, "job", e),
        }
    }

    /// Apply job-record retention now (at start; it also runs after every
    /// accepted action). The ids removed.
    pub fn prune_jobs(&self) -> Vec<String> {
        match self.load() {
            Ok(l) => records::prune(&l.work_dir(), &records::retention(&l.stack), &utc_now()),
            Err(_) => Vec::new(),
        }
    }

    /// Stop the relay and the refresher (on shutdown).
    pub fn shutdown(&self) {
        self.relay
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .stop_all();
        self.background.shutdown();
    }
}

/// Bind the listener on a loopback address.
pub fn bind(addr: &str) -> Result<(tiny_http::Server, SocketAddr), String> {
    let wanted = parse_listen_addr(addr)?;
    let server =
        tiny_http::Server::http(wanted).map_err(|e| format!("cannot listen on {wanted}: {e}"))?;
    let bound = server
        .server_addr()
        .to_ip()
        .ok_or("the listener has no IP address")?;
    Ok((server, bound))
}

/// Serve until the server is dropped or fails.
pub fn serve(server: tiny_http::Server, source: Arc<Source>) {
    let server = Arc::new(server);
    let pool: Vec<_> = (1..WORKERS)
        .map(|_| {
            let (server, source) = (server.clone(), source.clone());
            thread::spawn(move || accept_loop(&server, &source))
        })
        .collect();
    accept_loop(&server, &source);
    for t in pool {
        let _ = t.join();
    }
}

fn accept_loop(server: &tiny_http::Server, source: &Source) {
    while let Ok(request) = server.recv() {
        respond(source, request);
    }
}

fn header<'a>(request: &'a tiny_http::Request, name: &'static str) -> Option<&'a str> {
    request
        .headers()
        .iter()
        .find(|h| h.field.equiv(name))
        .map(|h| h.value.as_str())
}

fn respond(source: &Source, mut request: tiny_http::Request) {
    let method = request.method().as_str().to_string();
    let url = request.url().to_string();
    let auth = header(&request, "Authorization").map(str::to_string);
    let runs_jobs = header(&request, RUNS_JOBS_HEADER).map(str::trim) == Some("1");
    let mut body = String::new();
    if method == "POST" && source.token.authorizes(auth.as_deref()) {
        let _ = request.as_reader().take(MAX_BODY).read_to_string(&mut body);
    }
    let (status, value) = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        source.handle(&method, &url, auth.as_deref(), runs_jobs, &body)
    })) {
        Ok(r) => r,
        Err(_) => error(500, "internal", "the request handler failed"),
    };
    let path = url.split('?').next().unwrap_or("");
    eprintln!("{method} {path} {status}");
    let bytes = serde_json::to_vec(&value).unwrap_or_default();
    let response = tiny_http::Response::from_data(bytes)
        .with_status_code(status)
        .with_header(
            tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
                .expect("static header"),
        );
    let _ = request.respond(response);
}
