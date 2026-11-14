//! Sources: local processes that serve the Work / PRs / Needs you /
//! Environment views over loopback HTTP. This module is the host side: it
//! launches nothing itself (a source is a `chappa.yml` command with a
//! `source:` key, run by `projects.rs` like any other command, or a server
//! started elsewhere that the user attached in Settings), but it owns
//! everything between that process and the views:
//!
//! - **the address and token** a launched source receives in its
//!   environment (`CHAPPA_SOURCE_ADDR`, `CHAPPA_SOURCE_TOKEN`): a free
//!   loopback port and 32 fresh random bytes per launch, new on every
//!   restart;
//! - **identity before the token** ([`identity`]): the first request on
//!   every connection is a tokenless HMAC challenge, and readiness of a
//!   launched source means that challenge was answered correctly;
//! - **the manifest** ([`manifest`]): parsed and validated, with the paths
//!   it names being the ONLY paths ever requested ([`manifest::Endpoint`]
//!   cannot be built any other way), and `contract` mismatches reported as
//!   "needs update";
//! - **the trust gate**: the source's link hosts, extra roots and view list
//!   are shown to the user, and polling starts only once the hash of
//!   exactly that is on record ([`store`]); a changed hash asks again;
//! - **polling**: one worker thread per source, each view at its own
//!   clamped interval, exponential backoff on failure capped at eight
//!   intervals, the last payload kept while a refresh is in flight or
//!   failing (stale-while-refresh);
//! - **crash and restart**: the source's process row dying marks the
//!   source down and keeps its data; the next launch gets a new port and
//!   token and proves itself again.
//!
//! Everything HTTP happens here, in Rust. The webview gets the cached
//! payloads and freshness through Tauri commands that accept only the main
//! window ([`main_window_only`]), and never sees a token or a source URL.
//! Payloads are stored as opaque JSON for the view code; nothing here knows
//! what a work item or a pull request is.

pub mod client;
pub mod identity;
pub mod manifest;
pub mod store;

use std::collections::BTreeMap;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, State};
use term_core::actor::epoch_ms;

use client::{SourceClient, SourceError};
use manifest::{backoff_delay, parse_manifest, trust_summary, Endpoint, Manifest, TrustSummary, VIEW_NAMES, WORKFLOWS_POLL, WORKFLOWS_VIEW};
use store::{AttachedSource, SourceStore};

/// The one webview that may talk to sources. A second window (a terminal
/// dock) is a second webview with the same origin, and it must not reach
/// the source commands just by existing.
pub const MAIN_WINDOW_LABEL: &str = "main";
/// How long a launched source has after its spawn to answer the identity
/// check. Readiness is that answer, not the port being bound.
pub const READY_TIMEOUT: Duration = Duration::from_secs(60);
const READY_POLL_STEP: Duration = Duration::from_millis(250);
/// An attached server may not be running yet: reconnect attempts back off
/// between these bounds.
const ATTACH_RETRY_MIN: Duration = Duration::from_secs(1);
const ATTACH_RETRY_MAX: Duration = Duration::from_secs(60);
/// The event the webview listens to: `{event: "sources"}` when the list or
/// a status changed, `{event: "view", source_id, view}` when a view's
/// payload or freshness changed.
pub const EVENT: &str = "term://sources";

/// Refuse any caller that is not the main window. Pure over the label so it
/// is testable without a window.
pub fn main_window_label_only(label: &str) -> Result<(), String> {
    if label == MAIN_WINDOW_LABEL {
        Ok(())
    } else {
        Err(format!("source commands are accepted from the main window only (caller: `{label}`)"))
    }
}

/// The guard every source command runs first.
pub fn main_window_only(window: &tauri::Window) -> Result<(), String> {
    main_window_label_only(window.label())
}

/// Where `term://sources` goes. The app emits to its windows; tests collect.
pub trait SourcesEmitter: Send + Sync {
    fn emit(&self, payload: Value);
}

pub struct AppSourcesEmitter(pub AppHandle);

impl SourcesEmitter for AppSourcesEmitter {
    fn emit(&self, payload: Value) {
        let _ = self.0.emit(EVENT, payload);
    }
}

struct NullEmitter;

impl SourcesEmitter for NullEmitter {
    fn emit(&self, _: Value) {}
}

/// Where a source stands, as the Settings list shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceStatus {
    /// Launched (or attached) and not yet polling: waiting for the identity
    /// check, the manifest, or the first poll after trust.
    Starting,
    /// The manifest arrived and its hash is not on record: nothing is polled
    /// until the user answers.
    NeedsReview,
    /// The source speaks another contract version.
    NeedsUpdate,
    /// Trusted and the last poll succeeded.
    Up,
    /// Unreachable, its process exited, or it failed the identity check.
    /// The last payloads are kept.
    Down,
}

#[derive(Debug, Clone)]
enum Origin {
    Launched {
        project_id: u32,
        project_path: PathBuf,
        command: String,
        addr: String,
        token: String,
        term_id: Option<u32>,
    },
    Attached {
        addr: String,
        token_file: PathBuf,
        /// What the token-file check could not promise on this platform.
        warning: Option<String>,
    },
}

impl Origin {
    fn addr(&self) -> &str {
        match self {
            Origin::Launched { addr, .. } | Origin::Attached { addr, .. } => addr,
        }
    }
}

/// One view (or the `workflows` pseudo-view) of one source.
struct ViewState {
    endpoint: Endpoint,
    poll: Duration,
    /// The last successful payload, kept across failures and restarts.
    payload: Option<Value>,
    as_of_ms: Option<u64>,
    /// When the next fetch is due (0 = now).
    next_due_ms: u64,
    refreshing: bool,
    failures: u32,
    last_error: Option<String>,
}

struct SourceState {
    /// The trust-record key (project path + command, or the attached address).
    key: String,
    /// The opaque id the webview addresses the source by.
    id: String,
    /// Bumped on every relaunch, stop, detach and shutdown. A worker thread
    /// carries the generation it was started under and ends when it differs.
    generation: u64,
    origin: Origin,
    status: SourceStatus,
    last_error: Option<String>,
    manifest: Option<Manifest>,
    trust: Option<TrustSummary>,
    trusted: bool,
    views: BTreeMap<String, ViewState>,
}

impl SourceState {
    fn cancelled(&self, generation: u64) -> bool {
        self.generation != generation
    }
}

struct Inner {
    store: Option<SourceStore>,
    sources: BTreeMap<String, SourceState>,
    emitter: Arc<dyn SourcesEmitter>,
    shutting_down: bool,
}

/// The managed state. Cheap to clone (an `Arc`); worker threads hold one.
#[derive(Clone)]
pub struct Sources {
    inner: Arc<(Mutex<Inner>, Condvar)>,
}

impl Default for Sources {
    fn default() -> Self {
        Self {
            inner: Arc::new((
                Mutex::new(Inner {
                    store: None,
                    sources: BTreeMap::new(),
                    emitter: Arc::new(NullEmitter),
                    shutting_down: false,
                }),
                Condvar::new(),
            )),
        }
    }
}

/// What a launched source receives in its environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchEnv {
    pub addr: String,
    pub token: String,
}

impl LaunchEnv {
    /// The two variables, ready to extend a spawn spec's env with.
    pub fn env(&self) -> Vec<(String, String)> {
        vec![
            (identity::ADDR_ENV.to_owned(), self.addr.clone()),
            (identity::TOKEN_ENV.to_owned(), self.token.clone()),
        ]
    }
}

impl Sources {
    /// Called once from setup: the store at `<app-config>/sources.json`, the
    /// app as the event sink, and every attached source starts connecting.
    pub fn init(&self, app: &AppHandle, config_dir: PathBuf) {
        self.init_with(SourceStore::new(config_dir.join(store::FILE_NAME)), Arc::new(AppSourcesEmitter(app.clone())));
    }

    /// Tests and headless services: an explicit store and emitter.
    pub fn init_with(&self, store: SourceStore, emitter: Arc<dyn SourcesEmitter>) {
        let attached: Vec<AttachedSource> = store.attached().to_vec();
        {
            let mut inner = self.lock();
            inner.store = Some(store);
            inner.emitter = emitter;
        }
        for source in attached {
            if let Err(e) = self.start_attached(source) {
                log::warn!("[sources] attached source not started: {e}");
            }
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        match self.inner.0.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn notify(&self) {
        self.inner.1.notify_all();
    }

    fn emit(&self, payload: Value) {
        let emitter = self.lock().emitter.clone();
        emitter.emit(payload);
    }

    fn emit_sources(&self) {
        self.emit(json!({"event": "sources"}));
    }

    /// App exit: every worker ends at its next step; nothing is polled again.
    pub fn shutdown(&self) {
        {
            let mut inner = self.lock();
            inner.shutting_down = true;
            for state in inner.sources.values_mut() {
                state.generation += 1;
            }
        }
        self.notify();
    }

    // ---- launched sources (driven by projects.rs) --------------------------

    /// Before a `source:` command spawns: a free loopback port and a fresh
    /// token for THIS launch. A source that was launched before keeps its
    /// cached payloads but gets a new address and token (the old worker, if
    /// any, ends), and proves itself again before anything is polled.
    pub fn prepare_launch(&self, project_id: u32, project_path: &Path, command: &str) -> Result<LaunchEnv, String> {
        let port = free_loopback_port()?;
        let addr = format!("127.0.0.1:{port}");
        let token = identity::new_token_hex()?;
        let key = SourceStore::launched_key(project_path, command);
        {
            let mut inner = self.lock();
            let origin = Origin::Launched {
                project_id,
                project_path: project_path.to_path_buf(),
                command: command.to_owned(),
                addr: addr.clone(),
                token: token.clone(),
                term_id: None,
            };
            let state = inner.sources.entry(key.clone()).or_insert_with(|| fresh_state(&key, origin.clone()));
            state.generation += 1;
            state.origin = origin;
            state.status = SourceStatus::Starting;
            state.last_error = None;
            state.trusted = false;
            for view in state.views.values_mut() {
                view.refreshing = false;
                view.failures = 0;
                view.next_due_ms = 0;
            }
        }
        self.notify();
        self.emit_sources();
        Ok(LaunchEnv { addr, token })
    }

    /// The spawn succeeded: remember the terminal and start proving identity.
    pub fn launched(&self, project_path: &Path, command: &str, term_id: u32) {
        let key = SourceStore::launched_key(project_path, command);
        let generation = {
            let mut inner = self.lock();
            let Some(state) = inner.sources.get_mut(&key) else { return };
            if let Origin::Launched { term_id: slot, .. } = &mut state.origin {
                *slot = Some(term_id);
            }
            state.generation
        };
        self.spawn_worker(key, generation);
    }

    /// The spawn failed after [`Sources::prepare_launch`]: nothing to wait for.
    pub fn launch_failed(&self, project_path: &Path, command: &str, error: &str) {
        self.process_gone(project_path, command, &format!("could not be started: {error}"));
    }

    /// The source's process row ended (exited, stopped, project closed):
    /// the source is down, its worker ends, its payloads stay.
    pub fn process_gone(&self, project_path: &Path, command: &str, reason: &str) {
        let key = SourceStore::launched_key(project_path, command);
        {
            let mut inner = self.lock();
            let Some(state) = inner.sources.get_mut(&key) else { return };
            state.generation += 1;
            state.status = SourceStatus::Down;
            state.last_error = Some(format!("the source process {reason}"));
            for view in state.views.values_mut() {
                view.refreshing = false;
            }
        }
        self.notify();
        self.emit_sources();
    }

    // ---- attached sources ------------------------------------------------------

    /// Attach a server started elsewhere. The address must be loopback and
    /// the token file must hold the 64-hex token; on Unix it must be
    /// owner-only, elsewhere the listing carries a warning. Stored, so it
    /// comes back at the next launch.
    pub fn attach(&self, addr: &str, token_file: &Path) -> Result<(), String> {
        let addr = manifest::parse_loopback_addr(addr)?.to_string();
        read_attached_token(token_file)?;
        let source = AttachedSource { addr: addr.clone(), token_file: token_file.to_path_buf() };
        {
            let mut inner = self.lock();
            inner
                .store
                .as_mut()
                .ok_or_else(|| "sources store not initialized".to_owned())?
                .add_attached(source.clone())?;
        }
        self.start_attached(source)
    }

    fn start_attached(&self, source: AttachedSource) -> Result<(), String> {
        let key = SourceStore::attached_key(&source.addr);
        let warning = read_attached_token(&source.token_file).map(|(_, warning)| warning).unwrap_or(None);
        let generation = {
            let mut inner = self.lock();
            let origin = Origin::Attached { addr: source.addr.clone(), token_file: source.token_file.clone(), warning };
            let state = inner.sources.entry(key.clone()).or_insert_with(|| fresh_state(&key, origin.clone()));
            state.generation += 1;
            state.origin = origin;
            state.status = SourceStatus::Starting;
            state.last_error = None;
            state.trusted = false;
            state.generation
        };
        self.notify();
        self.emit_sources();
        self.spawn_worker(key, generation);
        Ok(())
    }

    /// Detach: the worker ends, the entry goes, and so does its trust record
    /// (attaching the same address later asks again).
    pub fn detach(&self, source_id: &str) -> Result<(), String> {
        {
            let mut inner = self.lock();
            let key = inner
                .sources
                .values()
                .find(|s| s.id == source_id)
                .map(|s| s.key.clone())
                .ok_or_else(|| format!("no source {source_id}"))?;
            let addr = match &inner.sources[&key].origin {
                Origin::Attached { addr, .. } => addr.clone(),
                Origin::Launched { .. } => return Err("a launched source is detached by removing `source:` from its command".into()),
            };
            if let Some(state) = inner.sources.get_mut(&key) {
                state.generation += 1;
            }
            inner.sources.remove(&key);
            let store = inner.store.as_mut().ok_or_else(|| "sources store not initialized".to_owned())?;
            store.remove_attached(&addr)?;
            store.forget_trust(&key)?;
        }
        self.notify();
        self.emit_sources();
        Ok(())
    }

    // ---- trust ---------------------------------------------------------------

    /// The trust answer. `trust = true` records `hash` for the source and
    /// polling begins; it must be the hash the dialog showed AND the hash of
    /// the manifest now held, or nothing is recorded (a manifest that
    /// changed while the dialog sat open brings its own prompt). `false`
    /// records nothing; the gate stays armed.
    pub fn trust_answer(&self, source_id: &str, trust: bool, hash: &str) -> Result<(), String> {
        {
            let mut inner = self.lock();
            let key = inner
                .sources
                .values()
                .find(|s| s.id == source_id)
                .map(|s| s.key.clone())
                .ok_or_else(|| format!("no source {source_id}"))?;
            let current = inner.sources[&key].trust.as_ref().map(|t| t.hash.clone());
            if current.as_deref() != Some(hash) {
                return Err("the source's manifest changed after the prompt was shown; nothing was trusted".into());
            }
            if !trust {
                return Ok(());
            }
            inner
                .store
                .as_mut()
                .ok_or_else(|| "sources store not initialized".to_owned())?
                .mark_trusted(&key, hash)?;
            let state = inner.sources.get_mut(&key).expect("looked up above");
            state.trusted = true;
            if state.status == SourceStatus::NeedsReview {
                state.status = SourceStatus::Starting;
            }
            for view in state.views.values_mut() {
                view.next_due_ms = 0;
                view.failures = 0;
            }
        }
        self.notify();
        self.emit_sources();
        Ok(())
    }

    // ---- what the webview reads ------------------------------------------------

    pub fn listing(&self) -> SourcesListing {
        let inner = self.lock();
        let hidden_views = inner
            .store
            .as_ref()
            .map(|s| s.hidden_views().iter().cloned().collect())
            .unwrap_or_default();
        SourcesListing {
            sources: inner.sources.values().map(source_dto).collect(),
            hidden_views,
        }
    }

    /// One view's cached payload and freshness. A hidden view still answers:
    /// hiding is a rail decision, the data stays.
    pub fn view(&self, source_id: &str, view: &str) -> Result<SourceViewDto, String> {
        let inner = self.lock();
        let state = inner
            .sources
            .values()
            .find(|s| s.id == source_id)
            .ok_or_else(|| format!("no source {source_id}"))?;
        let v = state
            .views
            .get(view)
            .ok_or_else(|| format!("source {} serves no `{view}` view", state.id))?;
        Ok(SourceViewDto {
            payload: v.payload.clone(),
            freshness: freshness_dto(&state.manifest, v),
        })
    }

    /// Hide or show a rail view everywhere. Hidden keeps data and trust.
    pub fn set_view_hidden(&self, view: &str, hidden: bool) -> Result<(), String> {
        if !VIEW_NAMES.contains(&view) {
            return Err(format!("`{view}` is not a view (one of {})", VIEW_NAMES.join(", ")));
        }
        {
            let mut inner = self.lock();
            inner
                .store
                .as_mut()
                .ok_or_else(|| "sources store not initialized".to_owned())?
                .set_view_hidden(view, hidden)?;
        }
        self.emit_sources();
        Ok(())
    }

    // ---- the worker ----------------------------------------------------------------

    fn spawn_worker(&self, key: String, generation: u64) {
        let sources = self.clone();
        let name = format!("source {key}");
        if let Err(e) = std::thread::Builder::new()
            .name(name.clone())
            .spawn(move || sources.run(key, generation))
        {
            log::error!("[sources] cannot start a worker thread for {name}: {e}");
        }
    }

    fn cancelled(&self, key: &str, generation: u64) -> bool {
        let inner = self.lock();
        inner.shutting_down || inner.sources.get(key).map_or(true, |s| s.cancelled(generation))
    }

    /// Mark the source down with `error`; payloads stay.
    fn set_down(&self, key: &str, generation: u64, status: SourceStatus, error: String) {
        {
            let mut inner = self.lock();
            let Some(state) = inner.sources.get_mut(key) else { return };
            if state.cancelled(generation) {
                return;
            }
            state.status = status;
            state.last_error = Some(error);
            for view in state.views.values_mut() {
                view.refreshing = false;
            }
        }
        self.emit_sources();
    }

    /// One source's life, from the identity check to the poll loop. Ends
    /// when the generation moves (relaunch, stop, detach, shutdown). A
    /// launched source that fails ends here and waits for its next launch;
    /// an attached one keeps retrying, because its process restarts outside
    /// the app's view.
    fn run(self, key: String, generation: u64) {
        let mut attempt: u32 = 0;
        loop {
            if self.cancelled(&key, generation) {
                return;
            }
            let outcome = self.run_once(&key, generation);
            let attached = matches!(self.lock().sources.get(&key).map(|s| &s.origin), Some(Origin::Attached { .. }));
            match outcome {
                RunEnd::Cancelled => return,
                RunEnd::Failed if !attached => return,
                RunEnd::Failed => {
                    attempt = attempt.saturating_add(1);
                    let delay = backoff_delay(ATTACH_RETRY_MIN, attempt).min(ATTACH_RETRY_MAX);
                    if self.sleep_unless_cancelled(&key, generation, delay) {
                        return;
                    }
                }
            }
        }
    }

    /// Wait up to `delay`, returning early (true) when the generation moves.
    fn sleep_unless_cancelled(&self, key: &str, generation: u64, delay: Duration) -> bool {
        let deadline = Instant::now() + delay;
        let mut guard = self.lock();
        loop {
            if guard.shutting_down || guard.sources.get(key).map_or(true, |s| s.cancelled(generation)) {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            guard = match self.inner.1.wait_timeout(guard, deadline - now) {
                Ok((g, _)) => g,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
    }

    fn run_once(&self, key: &str, generation: u64) -> RunEnd {
        // The client is built from the launch record / token file, off the lock.
        let origin = match self.lock().sources.get(key) {
            Some(state) if !state.cancelled(generation) => state.origin.clone(),
            _ => return RunEnd::Cancelled,
        };
        let built = match &origin {
            Origin::Launched { addr, token, .. } => SourceClient::new(addr, token),
            Origin::Attached { addr, token_file, .. } => {
                read_attached_token(token_file).and_then(|(token, _)| SourceClient::new(addr, &token))
            }
        };
        let mut client = match built {
            Ok(client) => client,
            Err(e) => {
                self.set_down(key, generation, SourceStatus::Down, e);
                return RunEnd::Failed;
            }
        };

        // Identity. A launched source has READY_TIMEOUT to bind and answer;
        // a wrong proof is final for this launch (someone else holds the
        // port, or the source is broken), not something to wait out.
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            if self.cancelled(key, generation) {
                return RunEnd::Cancelled;
            }
            match client.verify() {
                Ok(()) => break,
                Err(SourceError::Unreachable(why)) => {
                    if matches!(origin, Origin::Attached { .. }) {
                        self.set_down(key, generation, SourceStatus::Down, format!("not reachable at {}: {why}", origin.addr()));
                        return RunEnd::Failed;
                    }
                    if Instant::now() >= deadline {
                        self.set_down(
                            key,
                            generation,
                            SourceStatus::Down,
                            format!(
                                "did not answer the identity check at {} within {} s",
                                origin.addr(),
                                READY_TIMEOUT.as_secs()
                            ),
                        );
                        return RunEnd::Failed;
                    }
                    if self.sleep_unless_cancelled(key, generation, READY_POLL_STEP) {
                        return RunEnd::Cancelled;
                    }
                }
                Err(e) => {
                    self.set_down(key, generation, SourceStatus::Down, e.to_string());
                    return RunEnd::Failed;
                }
            }
        }

        // Manifest.
        let manifest = match client.manifest().map_err(|e| e.to_string()).and_then(|v| parse_manifest(&v).map_err(|e| e.to_string())) {
            Ok(manifest) => manifest,
            Err(text) => {
                let status = if text.contains("contract version") { SourceStatus::NeedsUpdate } else { SourceStatus::Down };
                self.set_down(key, generation, status, text);
                return RunEnd::Failed;
            }
        };
        {
            let mut inner = self.lock();
            let trusted = inner
                .store
                .as_ref()
                .is_some_and(|s| s.is_trusted(key, &manifest::trust_hash(&manifest)));
            let Some(state) = inner.sources.get_mut(key) else { return RunEnd::Cancelled };
            if state.cancelled(generation) {
                return RunEnd::Cancelled;
            }
            let mut views = BTreeMap::new();
            let mut declared: Vec<(String, Endpoint, Duration)> = manifest
                .views
                .iter()
                .map(|(name, v)| (name.clone(), v.list.clone(), v.poll))
                .collect();
            if let Some(workflows) = &manifest.workflows {
                declared.push((WORKFLOWS_VIEW.to_owned(), workflows.clone(), WORKFLOWS_POLL));
            }
            for (name, endpoint, poll) in declared {
                let previous = state.views.remove(&name);
                views.insert(
                    name,
                    ViewState {
                        endpoint,
                        poll,
                        payload: previous.as_ref().and_then(|p| p.payload.clone()),
                        as_of_ms: previous.as_ref().and_then(|p| p.as_of_ms),
                        next_due_ms: 0,
                        refreshing: false,
                        failures: 0,
                        last_error: None,
                    },
                );
            }
            state.views = views;
            state.trust = Some(trust_summary(&manifest));
            state.manifest = Some(manifest);
            state.trusted = trusted;
            state.last_error = None;
            state.status = if trusted { SourceStatus::Starting } else { SourceStatus::NeedsReview };
        }
        self.emit_sources();

        // Poll.
        loop {
            let due: Vec<(String, Endpoint, Duration)> = {
                let mut guard = self.lock();
                loop {
                    if guard.shutting_down {
                        return RunEnd::Cancelled;
                    }
                    let Some(state) = guard.sources.get_mut(key) else { return RunEnd::Cancelled };
                    if state.cancelled(generation) {
                        return RunEnd::Cancelled;
                    }
                    let now = epoch_ms();
                    let wait = if !state.trusted {
                        None // park until the trust answer wakes us
                    } else {
                        let due: Vec<(String, Endpoint, Duration)> = state
                            .views
                            .iter_mut()
                            .filter(|(_, v)| v.next_due_ms <= now && !v.refreshing)
                            .map(|(name, v)| {
                                v.refreshing = true;
                                (name.clone(), v.endpoint.clone(), v.poll)
                            })
                            .collect();
                        if !due.is_empty() {
                            break due;
                        }
                        state
                            .views
                            .values()
                            .map(|v| v.next_due_ms.saturating_sub(now))
                            .min()
                            .map(Duration::from_millis)
                    };
                    guard = match wait {
                        Some(delay) => match self.inner.1.wait_timeout(guard, delay.max(Duration::from_millis(10))) {
                            Ok((g, _)) => g,
                            Err(poisoned) => poisoned.into_inner().0,
                        },
                        None => match self.inner.1.wait(guard) {
                            Ok(g) => g,
                            Err(poisoned) => poisoned.into_inner(),
                        },
                    };
                }
            };
            for (name, endpoint, poll) in due {
                let result = client.get(&endpoint);
                if self.apply_poll(key, generation, &name, poll, result) {
                    return RunEnd::Cancelled;
                }
            }
        }
    }

    /// Store one fetch outcome. Returns true when the worker was cancelled.
    fn apply_poll(&self, key: &str, generation: u64, view: &str, poll: Duration, result: Result<Value, SourceError>) -> bool {
        let source_id = {
            let mut inner = self.lock();
            let Some(state) = inner.sources.get_mut(key) else { return true };
            if state.cancelled(generation) {
                return true;
            }
            let now = epoch_ms();
            let Some(v) = state.views.get_mut(view) else { return false };
            v.refreshing = false;
            match result {
                Ok(payload) if payload.is_object() => {
                    v.payload = Some(payload);
                    v.as_of_ms = Some(now);
                    v.failures = 0;
                    v.last_error = None;
                    v.next_due_ms = now + poll.as_millis() as u64;
                    state.status = SourceStatus::Up;
                    state.last_error = None;
                }
                Ok(_) => {
                    v.failures += 1;
                    v.last_error = Some("the view did not answer a JSON object".into());
                    v.next_due_ms = now + backoff_delay(poll, v.failures).as_millis() as u64;
                }
                Err(e) => {
                    v.failures += 1;
                    v.last_error = Some(e.to_string());
                    v.next_due_ms = now + backoff_delay(poll, v.failures).as_millis() as u64;
                    // The connection is gone or the token is refused: the
                    // SOURCE is down. A per-view error status leaves the
                    // source up with that one view marked.
                    let whole_source = matches!(e, SourceError::Unreachable(_) | SourceError::Untrusted(_))
                        || matches!(e, SourceError::Http { status: 401 | 403, .. });
                    if whole_source {
                        state.status = SourceStatus::Down;
                        state.last_error = Some(e.to_string());
                    }
                }
            }
            state.id.clone()
        };
        self.emit(json!({"event": "view", "source_id": source_id, "view": view}));
        false
    }
}

enum RunEnd {
    Cancelled,
    Failed,
}

fn fresh_state(key: &str, origin: Origin) -> SourceState {
    SourceState {
        key: key.to_owned(),
        id: project_model::hash::content_hash(key.as_bytes())[..16].to_owned(),
        generation: 0,
        origin,
        status: SourceStatus::Starting,
        last_error: None,
        manifest: None,
        trust: None,
        trusted: false,
        views: BTreeMap::new(),
    }
}

/// Bind an ephemeral loopback port and release it. The source binds the same
/// port a moment later; the window in between is the usual one every
/// "pick a free port" scheme has, and the identity check is what makes a
/// hijack of that window harmless (the token never reaches the hijacker).
fn free_loopback_port() -> Result<u16, String> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("cannot pick a loopback port: {e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    drop(listener);
    Ok(port)
}

/// The attached token: the file's trimmed content, which must decode as the
/// 32-byte token. On Unix a file another user can read is refused outright
/// (that user could then impersonate the source to the app). Elsewhere the
/// permission model is not checked and the listing says so.
pub fn read_attached_token(path: &Path) -> Result<(String, Option<String>), String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("cannot read the token file {}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("{} is not a file", path.display()));
    }
    let warning = token_file_permission_check(path, &meta)?;
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read the token file {}: {e}", path.display()))?;
    let token = text.trim().to_owned();
    if identity::decode_token(&token).is_none() {
        return Err(format!("{} does not hold a 64-character lowercase hex token", path.display()));
    }
    Ok((token, warning))
}

#[cfg(unix)]
fn token_file_permission_check(path: &Path, meta: &std::fs::Metadata) -> Result<Option<String>, String> {
    use std::os::unix::fs::PermissionsExt;
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(format!(
            "the token file {} is readable by other users (mode {mode:o}); make it owner-only (chmod 600) first",
            path.display()
        ));
    }
    Ok(None)
}

#[cfg(not(unix))]
fn token_file_permission_check(path: &Path, _meta: &std::fs::Metadata) -> Result<Option<String>, String> {
    Ok(Some(format!(
        "the permissions of {} were not checked on this platform; keep it in a folder only your account can read",
        path.display()
    )))
}

// ---- DTOs ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourcesListing {
    pub sources: Vec<SourceDto>,
    /// Rail views the user switched off (data and trust kept).
    pub hidden_views: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OriginDto {
    Launched { project_id: u32, project_path: String, command: String },
    Attached { addr: String, token_file: String },
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceDto {
    pub id: String,
    /// The manifest's label once it arrived, else the command name or address.
    pub label: String,
    pub origin: OriginDto,
    pub status: SourceStatus,
    pub last_error: Option<String>,
    pub trusted: bool,
    /// What the trust dialog shows (present once the manifest arrived).
    pub trust: Option<TrustSummary>,
    /// `source.id` / `source.version` from the manifest.
    pub manifest_id: Option<String>,
    pub version: Option<String>,
    pub views: Vec<ViewDto>,
    /// Platform caveats about an attached token file.
    pub warning: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewDto {
    pub name: String,
    pub poll_seconds: u64,
    pub as_of: Option<String>,
    pub next_utc: Option<String>,
    pub refreshing: bool,
    pub last_error: Option<String>,
    pub has_payload: bool,
}

/// The contract's `Freshness` shape, built by the app over its own poll
/// bookkeeping (not the source's own `freshness` field, which stays inside
/// the payload for the view code).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FreshnessDto {
    pub as_of: Option<String>,
    pub refreshing: bool,
    pub next_utc: Option<String>,
    pub errors: Vec<FreshnessError>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FreshnessError {
    pub source: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceViewDto {
    pub payload: Option<Value>,
    pub freshness: FreshnessDto,
}

fn source_dto(state: &SourceState) -> SourceDto {
    let (origin, fallback_label, warning) = match &state.origin {
        Origin::Launched { project_id, project_path, command, .. } => (
            OriginDto::Launched {
                project_id: *project_id,
                project_path: project_path.display().to_string(),
                command: command.clone(),
            },
            command.clone(),
            None,
        ),
        Origin::Attached { addr, token_file, warning } => (
            OriginDto::Attached { addr: addr.clone(), token_file: token_file.display().to_string() },
            addr.clone(),
            warning.clone(),
        ),
    };
    SourceDto {
        id: state.id.clone(),
        label: state.manifest.as_ref().map(|m| m.label.clone()).unwrap_or(fallback_label),
        origin,
        status: state.status,
        last_error: state.last_error.clone(),
        trusted: state.trusted,
        trust: state.trust.clone(),
        manifest_id: state.manifest.as_ref().map(|m| m.id.clone()),
        version: state.manifest.as_ref().map(|m| m.version.clone()),
        views: state
            .views
            .iter()
            .map(|(name, v)| ViewDto {
                name: name.clone(),
                poll_seconds: v.poll.as_secs(),
                as_of: v.as_of_ms.map(iso_utc),
                next_utc: next_utc(v),
                refreshing: v.refreshing,
                last_error: v.last_error.clone(),
                has_payload: v.payload.is_some(),
            })
            .collect(),
        warning,
    }
}

fn next_utc(v: &ViewState) -> Option<String> {
    (v.next_due_ms > 0).then(|| iso_utc(v.next_due_ms))
}

fn freshness_dto(manifest: &Option<Manifest>, v: &ViewState) -> FreshnessDto {
    FreshnessDto {
        as_of: v.as_of_ms.map(iso_utc),
        refreshing: v.refreshing,
        next_utc: next_utc(v),
        errors: v
            .last_error
            .iter()
            .map(|message| FreshnessError {
                source: manifest.as_ref().map(|m| m.id.clone()).unwrap_or_default(),
                message: message.clone(),
            })
            .collect(),
    }
}

/// `ms` since the Unix epoch as `YYYY-MM-DDTHH:MM:SS.mmmZ` (the contract's
/// `Utc`). Days-to-civil by the usual proleptic-Gregorian arithmetic.
pub fn iso_utc(ms: u64) -> String {
    let secs = ms / 1000;
    let millis = ms % 1000;
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, rem % 3600 / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!("{year:04}-{month:02}-{d:02}T{h:02}:{m:02}:{s:02}.{millis:03}Z")
}

// ---- Tauri commands ------------------------------------------------------------------

#[tauri::command]
pub fn list_sources(window: tauri::Window, sources: State<'_, Sources>) -> Result<SourcesListing, String> {
    main_window_only(&window)?;
    Ok(sources.listing())
}

#[tauri::command]
pub fn source_trust_answer(
    window: tauri::Window,
    sources: State<'_, Sources>,
    source_id: String,
    trust: bool,
    hash: String,
) -> Result<SourcesListing, String> {
    main_window_only(&window)?;
    sources.trust_answer(&source_id, trust, &hash)?;
    Ok(sources.listing())
}

#[tauri::command]
pub fn source_view(window: tauri::Window, sources: State<'_, Sources>, source_id: String, view: String) -> Result<SourceViewDto, String> {
    main_window_only(&window)?;
    sources.view(&source_id, &view)
}

#[tauri::command]
pub fn set_source_view_hidden(
    window: tauri::Window,
    sources: State<'_, Sources>,
    view: String,
    hidden: bool,
) -> Result<SourcesListing, String> {
    main_window_only(&window)?;
    sources.set_view_hidden(&view, hidden)?;
    Ok(sources.listing())
}

#[tauri::command]
pub fn attach_source(window: tauri::Window, sources: State<'_, Sources>, addr: String, token_file: String) -> Result<SourcesListing, String> {
    main_window_only(&window)?;
    sources.attach(&addr, Path::new(&token_file))?;
    Ok(sources.listing())
}

#[tauri::command]
pub fn detach_source(window: tauri::Window, sources: State<'_, Sources>, source_id: String) -> Result<SourcesListing, String> {
    main_window_only(&window)?;
    sources.detach(&source_id)?;
    Ok(sources.listing())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_main_window_may_call_source_commands() {
        assert!(main_window_label_only("main").is_ok());
        for other in ["dock", "Main", "main2", "", "settings"] {
            let err = main_window_label_only(other).unwrap_err();
            assert!(err.contains(other) || other.is_empty(), "{err}");
        }
    }

    #[test]
    fn launch_env_carries_exactly_the_two_variables() {
        let env = LaunchEnv { addr: "127.0.0.1:5000".into(), token: "ab".repeat(32) };
        assert_eq!(
            env.env(),
            vec![
                ("CHAPPA_SOURCE_ADDR".to_owned(), "127.0.0.1:5000".to_owned()),
                ("CHAPPA_SOURCE_TOKEN".to_owned(), "ab".repeat(32)),
            ]
        );
    }

    #[test]
    fn timestamps_render_as_contract_utc() {
        assert_eq!(iso_utc(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_utc(951_782_400_000), "2000-02-29T00:00:00.000Z");
        assert_eq!(iso_utc(1_700_000_000_123), "2023-11-14T22:13:20.123Z");
        assert_eq!(iso_utc(4_102_444_799_000), "2099-12-31T23:59:59.000Z");
    }

    #[test]
    fn a_token_file_must_hold_the_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        assert!(read_attached_token(&path).is_err(), "missing file");
        std::fs::write(&path, "not a token\n").unwrap();
        assert!(read_attached_token(&path).unwrap_err().contains("64-character"));
        std::fs::write(&path, format!("{}\n", "0a".repeat(32))).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(read_attached_token(&path).unwrap_err().contains("readable by other users"));
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            let (token, warning) = read_attached_token(&path).unwrap();
            assert_eq!(token, "0a".repeat(32));
            assert!(warning.is_none());
        }
        #[cfg(not(unix))]
        {
            let (token, warning) = read_attached_token(&path).unwrap();
            assert_eq!(token, "0a".repeat(32));
            assert!(warning.unwrap().contains("not checked"));
        }
    }

    #[test]
    fn source_ids_are_stable_opaque_and_distinct_per_key() {
        let a = fresh_state("/p::feed", Origin::Attached { addr: "x".into(), token_file: "t".into(), warning: None });
        let b = fresh_state("/p::feed", Origin::Attached { addr: "x".into(), token_file: "t".into(), warning: None });
        let c = fresh_state("/p::other", Origin::Attached { addr: "x".into(), token_file: "t".into(), warning: None });
        assert_eq!(a.id, b.id);
        assert_ne!(a.id, c.id);
        assert_eq!(a.id.len(), 16);
        assert!(a.id.bytes().all(|b| b.is_ascii_hexdigit()));
    }
}
