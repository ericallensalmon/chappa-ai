//! The source protocol over HTTP: identity, manifest, the Environment
//! view, actions and jobs.
//!
//! | route | auth | answers |
//! |---|---|---|
//! | `GET /chappa/v1/identity?challenge=<64 hex>` | none (that is its point) | `{proof}` |
//! | `GET /chappa/v1/manifest` | bearer | the manifest |
//! | `GET /api/environment` | bearer | the Environment payload |
//! | `POST /api/actions` | bearer | `202 {jobId, run}` with `X-Chappa-Runs-Jobs: 1`, `202 {jobId}` without (the job runs headless), `200 {message}` for an acknowledgement, `409` unconfirmed / no longer offered / stale plan / plan preview still pending / note missing, `404` unknown target |
//! | `GET /api/work` | bearer | the Work view: one overlay per item workspace (see [`crate::workspaces`]) |
//! | `GET /api/work/archived` | bearer | the Archived panel |
//! | `GET /api/work/reviews` | bearer | the Review workspaces panel (see [`crate::reviews`]) |
//! | `POST /api/work/owner` | bearer | an owner row the app polled, `{source, key, item}` (the manifest's `ownerUpdates`): refreshes the workspace's copy of it; `200` also for rows with no workspace |
//! | `GET /api/jobs/{id}` | bearer | `{state: running\|done\|failed, log?, exitCode?, error?}` |
//! | `POST /api/jobs/{id}` | bearer | the pane's exit report, recorded; `200 {}` |
//!
//! `X-Chappa-Actor` on an actions call names who acted (an acknowledgement
//! records it; without it the actor is "the app"). A work item's local
//! status moves record it too, and their human gates read it: only an
//! actor starting `human:` counts as a person (see [`crate::local_status`]).
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

use indexmap::IndexMap;
use std::sync::Weak;

use stack_core::driver::ResolvedInstance;
use stack_core::hosts::{check_hosts_file, check_hosts_text, HostOs};
use stack_core::intent::utc_now;
use stack_core::runner::Observation;

use crate::actions::{channel_for, Offered};
use crate::background::{Background, Periodic};
use crate::data_safety::{
    self, CheckStore, InstanceSafety, SafetyState, ACK_ACTION_ID, ACTOR_CAP, CHECK_ACTION_ID,
    DEFAULT_ACTOR, NOTE_CAP, SCHEDULE_TICK_SECS,
};
use crate::env_view::{self, EnvInputs};
use crate::identity::{is_valid_challenge, Token, IDENTITY_PATH, MANIFEST_PATH};
use crate::procs::ProcessTable;
use crate::project::{Loaded, ProjectConfig};
use crate::records::{self, JobRecord, JobStatus, JobWork};
use crate::relay::{self, Relay};
use crate::reviews;
use crate::secret_scan;
use crate::states;
use crate::workspaces::{
    self, Accepted, WorkCache, WorkInputs, ARCHIVED_PATH, OWNER_PATH, WORK_PATH,
};
use crate::world::World;

pub const ENVIRONMENT_PATH: &str = "/api/environment";
pub const ACTIONS_PATH: &str = "/api/actions";
pub const JOBS_PREFIX: &str = "/api/jobs/";
pub const RUNS_JOBS_HEADER: &str = "X-Chappa-Runs-Jobs";
pub const ACTOR_HEADER: &str = "X-Chappa-Actor";
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

/// What the request headers say about the caller.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Caller {
    /// `X-Chappa-Runs-Jobs: 1`: the caller runs a job's CLI in a pane.
    pub runs_jobs: bool,
    /// `X-Chappa-Actor`, when sent.
    pub actor: Option<String>,
}

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
    /// When the data-safety schedule last looked (clock seconds).
    last_tick: Mutex<Option<u64>>,
    /// Periodic check jobs started and maybe still running, per component.
    in_flight: Mutex<IndexMap<String, String>>,
    /// What git says about the item workspaces, filled off the request path.
    pub work: WorkCache,
    /// When the workspace facts were last looked at.
    last_work_tick: Mutex<Option<std::time::Instant>>,
    /// What the last resume calls found blocked, for the next confirm.
    pub blocks: states::ResumeBlocks,
    /// This source, for the secret rescan thread the refresher starts.
    me: std::sync::OnceLock<Weak<Source>>,
    /// When the secret rescan last started.
    last_secret_tick: Mutex<Option<std::time::Instant>>,
    /// A secret rescan is running (one at a time).
    secret_busy: Arc<std::sync::atomic::AtomicBool>,
}

/// The least time between two looks at the workspaces on the refresher
/// thread (each entry has its own, longer, time to live).
pub const WORK_TICK: std::time::Duration = std::time::Duration::from_secs(2);

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
        let source = Arc::new(Source {
            world,
            config,
            token,
            addr,
            background: background.clone(),
            relay: Mutex::new(Relay::default()),
            accepting: Mutex::new(()),
            last_tick: Mutex::new(None),
            in_flight: Mutex::new(IndexMap::new()),
            work: WorkCache::default(),
            last_work_tick: Mutex::new(None),
            blocks: states::ResumeBlocks::default(),
            me: std::sync::OnceLock::new(),
            last_secret_tick: Mutex::new(None),
            secret_busy: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        });
        let _ = source.me.set(Arc::downgrade(&source));
        let weak: Weak<dyn Periodic + Send + Sync> = Arc::downgrade(&source) as Weak<Source>;
        background.add_periodic(weak);
        source
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
        let caller = Caller {
            runs_jobs,
            actor: None,
        };
        self.handle_as(method, url, auth, &caller, body)
    }

    /// [`Source::handle`] with everything the headers say about the caller.
    pub fn handle_as(
        &self,
        method: &str,
        url: &str,
        auth: Option<&str>,
        caller: &Caller,
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
            ("GET", WORK_PATH) => self.work_view(),
            ("GET", reviews::REVIEWS_PATH) => self.reviews_view(),
            ("GET", ARCHIVED_PATH) => match self.load() {
                Ok(l) => (200, workspaces::archived_panel(&l, &utc_now())),
                Err(e) => error(409, "definition", e),
            },
            ("POST", OWNER_PATH) => match serde_json::from_str::<Value>(body) {
                Ok(v) => self.owner_update(&v),
                Err(e) => error(400, "bad-request", format!("the body is not JSON: {e}")),
            },
            ("POST", ACTIONS_PATH) => match serde_json::from_str::<Value>(body) {
                Ok(v) => self.action_as(&v, caller),
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
        let loaded = self.load().ok();
        let mut item_actions = workspaces::item_actions(loaded.as_ref());
        if let Some(list) = item_actions.as_array_mut() {
            list.extend(reviews::item_actions(loaded.as_ref()));
        }
        let (id, label, hosts) = match loaded {
            Some(loaded) => {
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
            None => (
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
            "views": {
                "environment": { "list": ENVIRONMENT_PATH, "pollSeconds": POLL_SECONDS },
                "work": { "list": WORK_PATH, "pollSeconds": workspaces::WORK_POLL_SECONDS },
            },
            "itemActions": item_actions,
            "ownerUpdates": OWNER_PATH,
            "actions": ACTIONS_PATH,
            "jobs": format!("{JOBS_PREFIX}{{id}}"),
            "linkHosts": hosts,
        })
    }

    /// The Environment payload, built fresh. Never waits on a registry or
    /// on `docker info`: those come from the background caches.
    pub fn environment(&self) -> Value {
        let now = utc_now();
        let instance = data_safety::instance_name(
            self.config.stack.as_deref().unwrap_or("unknown"),
            self.config.slot,
        );
        let loaded = match self.load() {
            Ok(l) => l,
            Err(e) => return env_view::unavailable(&e, &now, &instance, &self.config.dir),
        };
        match self.environment_of(&loaded, &now) {
            Ok(v) => v,
            Err(e) => env_view::unavailable(&e, &now, &loaded_instance(&loaded), &loaded.dir),
        }
    }

    fn environment_of(&self, loaded: &Loaded, now: &str) -> Result<Value, String> {
        let runner = loaded
            .runner(&self.world, now)
            .with_scripts(&*self.background);
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
            states::offered(loaded, &runner, &inst, &obs, &channel)
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
        let safety = Self::safety(loaded, &work_dir, &inst, &obs);
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
            safety: &safety,
            now_utc: now,
        };
        let mut v = env_view::environment(&inputs, &errors);
        if let Some(alerts) = v.get_mut("alerts").and_then(Value::as_array_mut) {
            alerts.extend(workspaces::drift_alerts(&self.work.snapshot()));
        }
        if let (Some(facts), Some(section)) = (
            v.get_mut("facts").and_then(Value::as_array_mut),
            reviews::environment_section(loaded, now),
        ) {
            facts.push(section);
        }
        let secrets = secret_scan::view_of(&self.world, loaded);
        if let Some(alerts) = v.get_mut("alerts").and_then(Value::as_array_mut) {
            alerts.extend(secrets.alerts());
        }
        if let (Some(banners), Some(b)) = (
            v.get_mut("banners").and_then(Value::as_array_mut),
            secrets.banner(),
        ) {
            banners.push(b);
        }
        if secrets.active() {
            if let Some(facts) = v.get_mut("facts").and_then(Value::as_array_mut) {
                facts.push(secrets.facts());
            }
        }
        v["secretScan"] = secrets.wire(loaded);
        Ok(v)
    }

    /// `GET /api/work`: the item workspaces as overlays. Reads git facts
    /// from the cache only; the engines are asked once, for the plans the
    /// Activate and Rebuild changed confirms show.
    pub fn work_view(&self) -> Reply {
        let now = self.world.clock.now_utc();
        let loaded = match self.load() {
            Ok(l) => l,
            Err(e) => {
                return (
                    200,
                    json!({
                        "freshness": { "asOf": now, "errors": [{ "source": "definition", "message": e }] },
                        "items": [],
                    }),
                )
            }
        };
        let runner = loaded.runner(&self.world, &now);
        let obs = runner.instance(loaded.slot).ok().and_then(|(inst, _)| {
            let table = ProcessTable::load(&loaded.work_dir(), &inst.id);
            runner
                .observe(&inst, &table.statuses(&*self.world.procs))
                .ok()
        });
        let facts = self.work.snapshot();
        let inputs = WorkInputs {
            world: &self.world,
            blocks: &self.blocks,
            loaded: &loaded,
            runner: &runner,
            obs: if loaded.has_errors() {
                None
            } else {
                obs.as_ref()
            },
            facts: &facts,
            now_utc: &now,
        };
        (200, workspaces::work_list(&inputs))
    }

    /// `GET /api/work/reviews`: the Review workspaces panel.
    pub fn reviews_view(&self) -> Reply {
        let now = self.world.clock.now_utc();
        let loaded = match self.load() {
            Ok(l) => l,
            Err(e) => return error(409, "definition", e),
        };
        let runner = loaded.runner(&self.world, &now);
        let obs = runner.instance(loaded.slot).ok().and_then(|(inst, _)| {
            let table = ProcessTable::load(&loaded.work_dir(), &inst.id);
            runner
                .observe(&inst, &table.statuses(&*self.world.procs))
                .ok()
        });
        let obs = if loaded.has_errors() {
            None
        } else {
            obs.as_ref()
        };
        (200, reviews::panel(&self.world, &loaded, obs, &now))
    }

    /// Look at the item workspaces with git now (the refresher calls this;
    /// tests call it directly).
    pub fn refresh_work(&self) {
        if let Ok(loaded) = self.load() {
            self.work.refresh(&self.world, &loaded);
        }
    }

    /// `POST /api/work/owner`: under the same one-at-a-time lock as the
    /// actions, since both write `item.json`.
    pub fn owner_update(&self, body: &Value) -> Reply {
        let _one_at_a_time = self.accepting.lock().unwrap_or_else(|p| p.into_inner());
        match self.work_dir() {
            Ok(w) => workspaces::owner_update(&w, body, &self.world.clock.now_utc()),
            Err(r) => r,
        }
    }

    /// A Work view action: `start-work` from another source's row, an
    /// overlay's, the drift banner's or the Archived panel's.
    fn work_action(&self, body: &Value, caller: &Caller) -> Reply {
        let now = self.world.clock.now_utc();
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
        let (inst, intent) = match runner.instance(loaded.slot) {
            Ok(i) => i,
            Err(e) => return error(409, "definition", e.to_string()),
        };
        let table = ProcessTable::load(&loaded.work_dir(), &inst.id);
        let obs = runner
            .observe(&inst, &table.statuses(&*self.world.procs))
            .ok();
        let facts = self.work.snapshot();
        let inputs = WorkInputs {
            world: &self.world,
            blocks: &self.blocks,
            loaded: &loaded,
            runner: &runner,
            obs: obs.as_ref(),
            facts: &facts,
            now_utc: &now,
        };
        let id = body.get("id").and_then(Value::as_str).unwrap_or("");
        let accepted = if reviews::handles(id) {
            reviews::accept(
                &self.world,
                &loaded,
                obs.as_ref(),
                id,
                body.get("target").unwrap_or(&Value::Null),
                body.get("params").unwrap_or(&Value::Null),
                body.get("confirmed").and_then(Value::as_bool) == Some(true),
                &now,
            )
        } else if view_of(body) == "prs" {
            Accepted::Refused(
                404,
                "unknown-target",
                format!("no action `{id}` on PR rows"),
            )
        } else {
            workspaces::accept(&self.world, &inputs, body, caller.actor.as_deref())
        };
        self.work.invalidate();
        match accepted {
            Accepted::Done(message) => (200, json!({ "message": message })),
            Accepted::Refused(status, kind, message) => error(status, kind, message),
            Accepted::Job { offered, mutates } => {
                // A workspace job needs no observation; its record only
                // names the bound item.
                let blank;
                let obs = match &obs {
                    Some(o) => o,
                    None => {
                        blank = Observation {
                            intent,
                            cache: Default::default(),
                            state: stack_core::planner::InstanceState::new(
                                &loaded.stack_name,
                                loaded.slot,
                            ),
                            actual: IndexMap::new(),
                            ps: IndexMap::new(),
                            containers: IndexMap::new(),
                            gateways: IndexMap::new(),
                            notes: Vec::new(),
                            scripts: IndexMap::new(),
                        };
                        &blank
                    }
                };
                self.start_job(&loaded, &inst, obs, &offered, mutates, caller, &now)
            }
        }
    }

    /// The instance's data safety now.
    fn safety(
        loaded: &Loaded,
        work_dir: &std::path::Path,
        inst: &ResolvedInstance,
        obs: &Observation,
    ) -> InstanceSafety {
        let store = CheckStore::load(work_dir, &inst.id);
        let running = |c: &str| obs.actual.get(c).is_some_and(|a| a.running);
        data_safety::summarize(loaded, &running, &store, data_safety::current_rules(loaded))
    }

    /// `POST {actions}` from a caller that sent no actor.
    pub fn action(&self, body: &Value, runs_jobs: bool) -> Reply {
        let caller = Caller {
            runs_jobs,
            actor: None,
        };
        self.action_as(body, &caller)
    }

    /// `POST {actions}`: plan again, check it is what was confirmed, store
    /// it as a job, and hand back the pane to run it in (or run it here).
    /// The data-safety actions are answered by [`Self::ack_action`] and
    /// [`Self::check_action`].
    pub fn action_as(&self, body: &Value, caller: &Caller) -> Reply {
        let _one_at_a_time = self.accepting.lock().unwrap_or_else(|p| p.into_inner());
        let Some(id) = body.get("id").and_then(Value::as_str) else {
            return error(400, "bad-request", "the body names no action `id`");
        };
        let view = body
            .pointer("/target/view")
            .and_then(Value::as_str)
            .unwrap_or("environment");
        if view == "work" || view == "prs" || view.starts_with("panel:") {
            return self.work_action(body, caller);
        }
        if view != "environment" {
            return error(
                404,
                "unknown-target",
                format!("no actions on the `{view}` view"),
            );
        }
        let mut key = body
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
        if id == secret_scan::ACK_ACTION_ID {
            return self.secret_ack(body, caller, &loaded);
        }
        // A row a script component reports (`stack/api`) acts through the
        // script component's own verbs.
        if let Some(k) = key.as_deref() {
            let owner = stack_core::script::component_of_row(k);
            if owner != k
                && loaded
                    .stack
                    .components
                    .get(owner)
                    .is_some_and(|c| c.driver == stack_core::definition::DriverKind::Script)
            {
                key = Some(owner.to_string());
            }
        }
        let runner = loaded
            .runner(&self.world, &now)
            .with_scripts(&*self.background);
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
        if id == ACK_ACTION_ID {
            return self.ack_action(body, caller, &loaded, &inst, &obs);
        }
        if id == CHECK_ACTION_ID {
            return self.check_action(key.as_deref(), caller, &loaded, &inst, &obs, &now);
        }
        let channel = channel_for(&obs, &self.config.channel);
        let offered = states::offered(&loaded, &runner, &inst, &obs, &channel);
        let Some(chosen) = offered.into_iter().find(|o| o.id == id && o.key == key) else {
            return error(
                409,
                "not-offered",
                format!("`{id}` is not offered for {what} now (the stack changed, or there is nothing to do); look again"),
            );
        };
        // Offered disabled while a plan preview is pending: nobody has seen
        // what it would run, so it is not accepted (the app refuses it too).
        if let Some(why) = &chosen.disabled {
            return error(
                409,
                "plan-pending",
                format!("{why}; look again once the view shows it"),
            );
        }
        if !confirmed {
            return error(
                409,
                "confirmation-required",
                "a mutating action must be confirmed",
            );
        }
        let current = chosen.fingerprint_for(&obs.state.fingerprint());
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
        if let JobWork::Plan { plan, .. } = &chosen.work {
            if let Err(e) = runner.guard_plan(&inst, &obs, plan) {
                return error(409, "shared-checkout", e.to_string());
            }
            if let Err(e) = runner.check_scripts(&inst, &obs, plan) {
                return error(409, "script", e.to_string());
            }
        }
        self.start_job(&loaded, &inst, &obs, &chosen, true, caller, &now)
    }

    /// Store `chosen` as a job and hand back the pane to run it in, or run
    /// it here when the caller hosts no pane.
    #[allow(clippy::too_many_arguments)]
    fn start_job(
        &self,
        loaded: &Loaded,
        inst: &ResolvedInstance,
        obs: &Observation,
        chosen: &Offered,
        mutates: bool,
        caller: &Caller,
        now: &str,
    ) -> Reply {
        let runs_jobs = caller.runs_jobs;
        let work_dir = loaded.work_dir();
        let job_id = match records::new_job_id(now) {
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
            mutates,
            confirm: chosen.confirm.clone(),
            status: JobStatus::Pending,
            exit_code: None,
            created_utc: now.to_string(),
            started_utc: None,
            ended_utc: None,
            runner_pid: None,
            headless: !runs_jobs,
            log: JobRecord::log_path(&work_dir, &job_id),
            result: JobRecord::result_path(&work_dir, &job_id),
            work: chosen.work.clone(),
            report: None,
            error: None,
            actor: caller
                .actor
                .as_deref()
                .map(|a| stack_core::workflow::clean_actor(Some(a))),
        };
        if let Err(e) = rec.save(&work_dir) {
            return error(500, "job", format!("cannot record the job: {e}"));
        }
        records::prune(&work_dir, &records::retention(&loaded.stack), now);
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

    /// `data-safety-ack`: record who acknowledged what the last checks
    /// found, with a note, on every not-clean component. Offered only while
    /// the instance is `not-clean`; refused unconfirmed, without a note, or
    /// when the findings changed since the confirm was shown.
    fn ack_action(
        &self,
        body: &Value,
        caller: &Caller,
        loaded: &Loaded,
        inst: &ResolvedInstance,
        obs: &Observation,
    ) -> Reply {
        let work_dir = loaded.work_dir();
        let safety = Self::safety(loaded, &work_dir, inst, obs);
        if safety.state != SafetyState::NotClean {
            return error(
                409,
                "not-offered",
                format!(
                    "there is nothing to acknowledge: the data-safety state of {} is {}",
                    safety.instance,
                    safety.state.as_str()
                ),
            );
        }
        if body.get("confirmed").and_then(Value::as_bool) != Some(true) {
            return error(
                409,
                "confirmation-required",
                "an acknowledgement must be confirmed",
            );
        }
        let fingerprint = body.pointer("/params/fingerprint").and_then(Value::as_str);
        if fingerprint != Some(safety.ack_fingerprint().as_str()) {
            return error(
                409,
                "stale-plan",
                format!(
                    "the findings changed since this was shown; now: {}",
                    safety.ack_confirm()
                ),
            );
        }
        let note = data_safety::clean_text(
            body.get("note").and_then(Value::as_str).unwrap_or(""),
            NOTE_CAP,
        );
        if note.is_empty() {
            return error(
                409,
                "note-required",
                "an acknowledgement needs a note saying why",
            );
        }
        let who = caller
            .actor
            .as_deref()
            .map(|a| data_safety::clean_text(a, ACTOR_CAP))
            .filter(|a| !a.is_empty())
            .unwrap_or_else(|| DEFAULT_ACTOR.to_string());
        let at = self.world.clock.now_utc();
        match safety.acknowledge_all(&work_dir, &inst.id, &who, &note, &at) {
            Ok(n) => (
                200,
                json!({
                    "message": format!(
                        "Acknowledged the findings of {n} component{} on {}",
                        if n == 1 { "" } else { "s" },
                        safety.instance
                    )
                }),
            ),
            Err(e) => error(500, "data-safety", e),
        }
    }

    /// `secret-scan-ack`: write the findings into the project's baseline and
    /// acknowledge the worktree scans that did not finish. Offered while
    /// there is something to acknowledge; refused unconfirmed, without a
    /// note, or when what it would cover changed since the confirm.
    fn secret_ack(&self, body: &Value, caller: &Caller, loaded: &Loaded) -> Reply {
        let view = secret_scan::view_of(&self.world, loaded);
        if view.ack_action().is_none() {
            return error(
                409,
                "not-offered",
                format!(
                    "there is nothing to acknowledge: the secret scan of {} is {}",
                    view.instance, view.state
                ),
            );
        }
        if body.get("confirmed").and_then(Value::as_bool) != Some(true) {
            return error(
                409,
                "confirmation-required",
                "an acknowledgement must be confirmed",
            );
        }
        let fingerprint = body.pointer("/params/fingerprint").and_then(Value::as_str);
        if fingerprint != Some(view.ack_fingerprint().as_str()) {
            return error(
                409,
                "stale-plan",
                format!(
                    "the findings changed since this was shown; now: {}",
                    view.ack_confirm()
                ),
            );
        }
        let note = data_safety::clean_text(
            body.get("note").and_then(Value::as_str).unwrap_or(""),
            NOTE_CAP,
        );
        if note.is_empty() {
            return error(
                409,
                "note-required",
                "an acknowledgement needs a note saying why",
            );
        }
        let who = caller
            .actor
            .as_deref()
            .map(|a| data_safety::clean_text(a, ACTOR_CAP))
            .filter(|a| !a.is_empty())
            .unwrap_or_else(|| DEFAULT_ACTOR.to_string());
        match view.acknowledge(loaded, &who, &note, &self.world.clock.now_utc()) {
            Ok((findings, errors)) => {
                let mut parts = Vec::new();
                if findings > 0 {
                    parts.push(format!(
                        "{findings} finding{}",
                        if findings == 1 { "" } else { "s" }
                    ));
                }
                if errors > 0 {
                    parts.push(format!(
                        "{errors} unfinished scan{}",
                        if errors == 1 { "" } else { "s" }
                    ));
                }
                (
                    200,
                    json!({ "message": format!("Acknowledged {} on {}", parts.join(" and "), view.instance) }),
                )
            }
            Err(e) => error(500, "secret-scan", e),
        }
    }

    /// One secret rescan pass now (the refresher starts it on its own
    /// thread every [`secret_scan::RESCAN_EVERY`]; tests call it). Whether
    /// anything was scanned.
    pub fn secret_scan_pass(&self) -> bool {
        let Ok(loaded) = self.load() else {
            return false;
        };
        if loaded.has_errors() {
            return false;
        }
        let now = self.world.clock.now_utc();
        let runner = loaded.runner(&self.world, &now);
        let inst = runner.instance(loaded.slot).ok().map(|(i, _)| i);
        secret_scan::rescan(&self.world, &loaded, &runner, inst.as_ref())
    }

    /// Start a rescan pass on its own thread when one is due and none runs.
    fn secret_scan_tick(&self) {
        {
            let mut last = self
                .last_secret_tick
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if last.is_some_and(|t| t.elapsed() < secret_scan::RESCAN_EVERY) {
                return;
            }
            *last = Some(std::time::Instant::now());
        }
        use std::sync::atomic::Ordering;
        let Some(me) = self.me.get().and_then(Weak::upgrade) else {
            return;
        };
        if self.secret_busy.swap(true, Ordering::SeqCst) {
            return;
        }
        let busy = self.secret_busy.clone();
        let spawned = thread::Builder::new()
            .name("secret rescan".into())
            .spawn(move || {
                me.secret_scan_pass();
                busy.store(false, Ordering::SeqCst);
            });
        if spawned.is_err() {
            self.secret_busy.store(false, Ordering::SeqCst);
        }
    }

    /// `data-safety-check`: check one component (`key`) or every running
    /// one with a check, now, as a job.
    fn check_action(
        &self,
        key: Option<&str>,
        caller: &Caller,
        loaded: &Loaded,
        inst: &ResolvedInstance,
        obs: &Observation,
        now: &str,
    ) -> Reply {
        let work_dir = loaded.work_dir();
        let safety = Self::safety(loaded, &work_dir, inst, obs);
        let components: Vec<String> = match key {
            Some(k) if safety.checkable.iter().any(|c| c == k) => vec![k.to_string()],
            Some(k) => {
                return error(
                    409,
                    "not-offered",
                    format!("`{k}` has no data-safety check, or is not running"),
                )
            }
            None if safety.checkable.is_empty() => {
                return error(
                    409,
                    "not-offered",
                    "no component with a data-safety check is running",
                )
            }
            None => safety.checkable.clone(),
        };
        let chosen = Offered {
            id: CHECK_ACTION_ID.to_string(),
            key: key.map(str::to_string),
            label: "Data-safety check".to_string(),
            confirm: format!("Check {} now.", components.join(", ")),
            work: JobWork::DataSafety {
                components,
                trigger: data_safety::Trigger::Manual.as_str().to_string(),
            },
            disabled: None,
        };
        self.start_job(loaded, inst, obs, &chosen, false, caller, now)
    }

    /// The `every:` schedule: at most once per [`SCHEDULE_TICK_SECS`] of
    /// the world's clock, start a headless check job for the running
    /// components whose interval has passed. The job ids started.
    pub fn data_safety_tick(&self) -> Vec<String> {
        let now_secs = self.world.clock.now_secs();
        {
            let mut last = self.last_tick.lock().unwrap_or_else(|p| p.into_inner());
            if last.is_some_and(|t| now_secs < t.saturating_add(SCHEDULE_TICK_SECS)) {
                return Vec::new();
            }
            *last = Some(now_secs);
        }
        let Ok(loaded) = self.load() else {
            return Vec::new();
        };
        if loaded.has_errors() || !data_safety::any_periodic(&loaded) {
            return Vec::new();
        }
        let work_dir = loaded.work_dir();
        let Ok(runner_inst) = loaded.runner(&self.world, &utc_now()).instance(loaded.slot) else {
            return Vec::new();
        };
        let inst = runner_inst.0;
        let store = CheckStore::load(&work_dir, &inst.id).unwrap_or_default();
        let due = data_safety::due(&loaded, &store, now_secs);
        if due.is_empty() {
            return Vec::new();
        }
        let mut in_flight = self.in_flight.lock().unwrap_or_else(|p| p.into_inner());
        in_flight.retain(|_, id| {
            JobRecord::load(&work_dir, id)
                .ok()
                .flatten()
                .is_some_and(|r| !r.status.finished())
        });
        let due: Vec<String> = due
            .into_iter()
            .filter(|c| !in_flight.contains_key(c))
            .collect();
        if due.is_empty() {
            return Vec::new();
        }
        let now = self.world.clock.now_utc();
        let runner = loaded.runner(&self.world, &now);
        let table = ProcessTable::load(&work_dir, &inst.id);
        let Ok(obs) = runner.observe(&inst, &table.statuses(&*self.world.procs)) else {
            return Vec::new();
        };
        let due: Vec<String> = due
            .into_iter()
            .filter(|c| obs.actual.get(c).is_some_and(|a| a.running))
            .collect();
        if due.is_empty() {
            return Vec::new();
        }
        let chosen = Offered {
            id: CHECK_ACTION_ID.to_string(),
            key: None,
            label: "Data-safety check".to_string(),
            confirm: format!("Check {} (every:).", due.join(", ")),
            work: JobWork::DataSafety {
                components: due.clone(),
                trigger: data_safety::Trigger::Every.as_str().to_string(),
            },
            disabled: None,
        };
        let (status, body) = self.start_job(
            &loaded,
            &inst,
            &obs,
            &chosen,
            false,
            &Caller::default(),
            &now,
        );
        match body.get("jobId").and_then(Value::as_str) {
            Some(id) if status == 202 => {
                for c in &due {
                    in_flight.insert(c.clone(), id.to_string());
                }
                vec![id.to_string()]
            }
            _ => Vec::new(),
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
                self.work.invalidate();
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

/// The view an actions call names (`environment` when it names none).
fn view_of(body: &Value) -> &str {
    body.pointer("/target/view")
        .and_then(Value::as_str)
        .unwrap_or("environment")
}

fn loaded_instance(loaded: &Loaded) -> String {
    data_safety::instance_name(&loaded.stack_name, loaded.slot)
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
    let caller = Caller {
        runs_jobs: header(&request, RUNS_JOBS_HEADER).map(str::trim) == Some("1"),
        actor: header(&request, ACTOR_HEADER).map(str::to_string),
    };
    let mut body = String::new();
    if method == "POST" && source.token.authorizes(auth.as_deref()) {
        let _ = request.as_reader().take(MAX_BODY).read_to_string(&mut body);
    }
    let (status, value) = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        source.handle_as(&method, &url, auth.as_deref(), &caller, &body)
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

impl Periodic for Source {
    fn tick(&self) {
        self.data_safety_tick();
        self.secret_scan_tick();
        let due = {
            let mut last = self
                .last_work_tick
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let due = last.is_none_or(|t| t.elapsed() >= WORK_TICK);
            if due {
                *last = Some(std::time::Instant::now());
            }
            due
        };
        if due {
            self.refresh_work();
        }
    }
}
