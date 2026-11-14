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

pub mod actions;
pub mod client;
pub mod identity;
pub mod links;
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

use actions::{action_verdict, actions_for, find_action, ActionVerdict, SourceAction, VerdictContext};
use client::{PostReply, SourceClient, SourceError};
use manifest::{backoff_delay, parse_manifest, trust_summary, Endpoint, Manifest, TrustSummary, VIEW_NAMES, WORKFLOWS_POLL, WORKFLOWS_VIEW};
pub use store::PromptPolicy;
use store::{AttachedSource, SourceStore};

use crate::agent_tools::AgentTool;
use crate::roots::Root;

/// Sent on every actions call: this app hosts a pane for a job's `run`
/// block, so a source may hand the job's CLI back instead of running it.
pub const RUNS_JOBS_HEADER: (&str, &str) = ("X-Chappa-Runs-Jobs", "1");
/// How many finished jobs a source keeps in its log.
pub const JOB_LOG_CAP: usize = 50;
/// The view name a panel page's own actions are addressed by:
/// `panel:<path>`, the path being the panel action's `endpoint`.
pub const PANEL_VIEW_PREFIX: &str = "panel:";
/// How many panel pages a source keeps for their actions: the ones open now
/// (a page and the page it was opened from) plus a few just left. The page
/// fetched least recently goes first.
pub const PANEL_CACHE_CAP: usize = 8;

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
    /// Action ids whose "mutating without a confirm" refusal was logged,
    /// so the log says it once per source and action, not once per render.
    unconfirmable_logged: std::collections::BTreeSet<String>,
    /// The jobs started from this source (202 replies), oldest first.
    jobs: Vec<JobRecord>,
    /// The last page of each panel fetched, so the page's own actions can be
    /// judged and run like a view's.
    panels: PanelCache,
}

impl SourceState {
    fn cancelled(&self, generation: u64) -> bool {
        self.generation != generation
    }

    /// Whether a cached VIEW payload names `path` as a `panel` action's
    /// endpoint: the closed list of panel paths. Panel pages themselves do
    /// not count, so a cached page never widens what may be fetched.
    fn names_panel(&self, path: &str) -> bool {
        self.views.values().filter_map(|v| v.payload.as_ref()).any(|payload| {
            actions::actions_in(payload)
                .0
                .iter()
                .any(|f| matches!(&f.action, SourceAction::Panel { endpoint, .. } if endpoint == path))
        })
    }
}

/// The last payload per panel path, most recently fetched last, at most
/// [`PANEL_CACHE_CAP`] of them.
#[derive(Debug, Default)]
struct PanelCache {
    pages: Vec<(String, Value)>,
}

impl PanelCache {
    fn put(&mut self, path: &str, payload: Value) {
        self.pages.retain(|(p, _)| p != path);
        self.pages.push((path.to_owned(), payload));
        while self.pages.len() > PANEL_CACHE_CAP {
            self.pages.remove(0);
        }
    }

    fn get(&self, path: &str) -> Option<&Value> {
        self.pages.iter().find(|(p, _)| p == path).map(|(_, v)| v)
    }

    fn clear(&mut self) {
        self.pages.clear();
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
            let store = inner.store.as_mut().ok_or_else(|| "sources store not initialized".to_owned())?;
            if !store.is_trusted(&key, hash) {
                // A different manifest may be a different program: the
                // prompts it showed before do not count as shown.
                store.forget_prompts(&key)?;
            }
            store.mark_trusted(&key, hash)?;
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
            sources: inner.sources.values().map(|s| source_dto(s, inner.store.as_ref())).collect(),
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
            let hash = manifest::trust_hash(&manifest);
            let trusted = inner.store.as_ref().is_some_and(|s| s.is_trusted(key, &hash));
            let Some(state) = inner.sources.get_mut(key) else { return RunEnd::Cancelled };
            if state.cancelled(generation) {
                return RunEnd::Cancelled;
            }
            // Panel pages were judged under the trust the user gave; a
            // different (or no) trust starts over once the views refill.
            if !trusted || state.trust.as_ref().map(|t| t.hash.as_str()) != Some(hash.as_str()) {
                state.panels.clear();
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

/// Who a source is, for the runner: what it is called, which project it
/// belongs to, its process row (launched sources), and its trusted hosts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceInfo {
    /// The opaque registry id.
    pub id: String,
    /// The manifest's `source.id` — what the journal's `source:<id>` names.
    pub manifest_id: String,
    pub label: String,
    pub key: String,
    pub project_id: Option<u32>,
    pub project_path: Option<PathBuf>,
    pub term_id: Option<u32>,
    pub link_hosts: Vec<String>,
}

/// An action found in a cached payload together with its verdict.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionDto {
    pub action: SourceAction,
    pub verdict: ActionVerdict,
    /// Whether an `agent` action's prompt must be shown before this run.
    pub needs_prompt: bool,
}

impl Sources {
    // ---- actions, links, panels, jobs (the runner reads these) ----------------

    fn trusted_state<'a>(inner: &'a Inner, source_id: &str) -> Result<&'a SourceState, String> {
        let state = inner
            .sources
            .values()
            .find(|s| s.id == source_id)
            .ok_or_else(|| format!("no source {source_id}"))?;
        if !state.trusted || state.manifest.is_none() {
            return Err(format!("source {} is not trusted; review it under Settings ▸ Sources first", state.id));
        }
        Ok(state)
    }

    fn info_of(state: &SourceState) -> SourceInfo {
        let manifest = state.manifest.as_ref();
        let (project_id, project_path, term_id) = match &state.origin {
            Origin::Launched { project_id, project_path, term_id, .. } => (Some(*project_id), Some(project_path.clone()), *term_id),
            Origin::Attached { .. } => (None, None, None),
        };
        SourceInfo {
            id: state.id.clone(),
            manifest_id: manifest.map(|m| m.id.clone()).unwrap_or_else(|| state.id.clone()),
            label: manifest.map(|m| m.label.clone()).unwrap_or_else(|| state.key.clone()),
            key: state.key.clone(),
            project_id,
            project_path,
            term_id,
            link_hosts: manifest.map(|m| m.link_hosts.clone()).unwrap_or_default(),
        }
    }

    /// Who a trusted source is.
    pub fn info(&self, source_id: &str) -> Result<SourceInfo, String> {
        let inner = self.lock();
        Self::trusted_state(&inner, source_id).map(Self::info_of)
    }

    /// The extra roots of every trusted source: `(registry id, path)`.
    pub fn trusted_roots(&self) -> Vec<(String, String)> {
        let inner = self.lock();
        inner
            .sources
            .values()
            .filter(|s| s.trusted)
            .flat_map(|s| s.manifest.iter().flat_map(|m| m.roots.iter()).map(move |r| (s.id.clone(), r.clone())))
            .collect()
    }

    pub fn prompt_policy(&self, source_id: &str) -> Result<PromptPolicy, String> {
        let inner = self.lock();
        let state = inner.sources.values().find(|s| s.id == source_id).ok_or_else(|| format!("no source {source_id}"))?;
        Ok(inner.store.as_ref().map(|st| st.prompt_policy(&state.key)).unwrap_or_default())
    }

    pub fn set_prompt_policy(&self, source_id: &str, policy: PromptPolicy) -> Result<(), String> {
        {
            let mut inner = self.lock();
            let key = inner
                .sources
                .values()
                .find(|s| s.id == source_id)
                .map(|s| s.key.clone())
                .ok_or_else(|| format!("no source {source_id}"))?;
            inner
                .store
                .as_mut()
                .ok_or_else(|| "sources store not initialized".to_owned())?
                .set_prompt_policy(&key, policy)?;
        }
        self.emit_sources();
        Ok(())
    }

    /// Whether `action_id`'s prompt must be shown before it runs now:
    /// always under `PromptPolicy::Always`, else until it was accepted once.
    pub fn prompt_needs_dialog(&self, source_id: &str, action_id: &str) -> Result<bool, String> {
        let inner = self.lock();
        let state = Self::trusted_state(&inner, source_id)?;
        let store = inner.store.as_ref().ok_or_else(|| "sources store not initialized".to_owned())?;
        Ok(match store.prompt_policy(&state.key) {
            PromptPolicy::Always => true,
            PromptPolicy::FirstTime => !store.prompt_shown(&state.key, action_id),
        })
    }

    /// The user accepted `action_id`'s prompt.
    pub fn mark_prompt_shown(&self, source_id: &str, action_id: &str) -> Result<(), String> {
        {
            let mut inner = self.lock();
            let key = Self::trusted_state(&inner, source_id)?.key.clone();
            inner
                .store
                .as_mut()
                .ok_or_else(|| "sources store not initialized".to_owned())?
                .mark_prompt_shown(&key, action_id)?;
        }
        self.emit_sources();
        Ok(())
    }

    /// The cached payload a view name addresses: a view's last answer, or,
    /// for `panel:<path>`, the last page of that panel. A panel page counts
    /// only while a cached view payload still names its path.
    fn cached_payload(state: &SourceState, view: &str) -> Result<Value, String> {
        if let Some(path) = view.strip_prefix(PANEL_VIEW_PREFIX) {
            if !state.names_panel(path) {
                return Err(format!("source {} names no panel at `{path}`", state.id));
            }
            return state
                .panels
                .get(path)
                .cloned()
                .ok_or_else(|| format!("source {} has no page at `{path}` yet; open it first", state.id));
        }
        state
            .views
            .get(view)
            .and_then(|v| v.payload.clone())
            .ok_or_else(|| format!("source {} has no `{view}` data yet", state.id))
    }

    /// The actions under `key` in the cached `view` payload (or panel page,
    /// `panel:<path>`), judged. A mutating endpoint without a confirm is
    /// left out (logged once).
    pub fn actions(&self, source_id: &str, view: &str, key: Option<&str>, tools: &[AgentTool], roots: &[Root]) -> Result<Vec<ActionDto>, String> {
        let (found, hosts, label, policy, shown) = {
            let inner = self.lock();
            let state = Self::trusted_state(&inner, source_id)?;
            let payload = Self::cached_payload(state, view)?;
            let manifest = state.manifest.as_ref().expect("trusted implies a manifest");
            let store = inner.store.as_ref();
            (
                actions_for(&payload, key),
                manifest.link_hosts.clone(),
                manifest.label.clone(),
                store.map(|s| s.prompt_policy(&state.key)).unwrap_or_default(),
                store.map(|s| s.prompts_shown(&state.key)).unwrap_or_default(),
            )
        };
        let ctx = VerdictContext { tools, roots, link_hosts: &hosts };
        let mut out = Vec::new();
        let mut hide: Vec<String> = Vec::new();
        for f in found {
            let verdict = action_verdict(&f.action, &ctx);
            if verdict.hidden {
                hide.push(f.action.id().to_owned());
                continue;
            }
            let needs_prompt = matches!(f.action, SourceAction::Agent { .. })
                && match policy {
                    PromptPolicy::Always => true,
                    PromptPolicy::FirstTime => !shown.iter().any(|id| id == f.action.id()),
                };
            out.push(ActionDto { action: f.action, verdict, needs_prompt });
        }
        if !hide.is_empty() {
            let mut inner = self.lock();
            if let Some(state) = inner.sources.values_mut().find(|s| s.id == source_id) {
                for id in hide {
                    if state.unconfirmable_logged.insert(id.clone()) {
                        log::warn!("[sources] {label}: action `{id}` mutates without a confirm text and is not offered");
                    }
                }
            }
        }
        Ok(out)
    }

    /// The action `action_id` under `key` in the cached `view` payload, with
    /// the source it belongs to. The runner judges it again before acting.
    pub fn action(&self, source_id: &str, view: &str, key: Option<&str>, action_id: &str) -> Result<(SourceAction, SourceInfo), String> {
        let inner = self.lock();
        let state = Self::trusted_state(&inner, source_id)?;
        let payload = Self::cached_payload(state, view)?;
        let found = find_action(&payload, key, action_id).ok_or_else(|| {
            format!(
                "source {} offers no action `{action_id}` on {} of its `{view}` view",
                state.id,
                key.map(|k| format!("`{k}`")).unwrap_or_else(|| "the view itself".to_owned())
            )
        })?;
        Ok((found.action, Self::info_of(state)))
    }

    /// Rule for links a trusted source hands over: on a host the user
    /// trusted with the manifest, https — or plain http to this machine
    /// (see `sources::links`). A plain-http name is looked up here, with a
    /// deadline, after the lock is released; call it off the UI thread.
    pub fn link_check(&self, source_id: &str, url: &str) -> Result<(), String> {
        let hosts = {
            let inner = self.lock();
            let state = Self::trusted_state(&inner, source_id)?;
            state.manifest.as_ref().map(|m| m.link_hosts.clone()).unwrap_or_default()
        };
        links::source_link_check(url, &hosts).map(|_| ())
    }

    /// A fresh, verified client for a trusted source, plus its manifest.
    /// Built per call (a POST must never ride a stale keep-alive), off the
    /// lock.
    fn client_for(&self, source_id: &str) -> Result<(SourceClient, Manifest), String> {
        let (origin, manifest) = {
            let inner = self.lock();
            let state = Self::trusted_state(&inner, source_id)?;
            (state.origin.clone(), state.manifest.clone().expect("trusted implies a manifest"))
        };
        let client = match &origin {
            Origin::Launched { addr, token, .. } => SourceClient::new(addr, token)?,
            Origin::Attached { addr, token_file, .. } => {
                let (token, _) = read_attached_token(token_file)?;
                SourceClient::new(addr, &token)?
            }
        };
        Ok((client, manifest))
    }

    /// `POST {actions}` with the app's runs-jobs header. The status is the
    /// outcome: 200 done, 202 started, 503 uncertain, anything else the
    /// source's refusal.
    pub fn call_endpoint(&self, source_id: &str, body: &Value) -> Result<PostReply, String> {
        let (mut client, manifest) = self.client_for(source_id)?;
        let endpoint = manifest.actions.ok_or_else(|| "this source declares no `actions` endpoint".to_owned())?;
        client.post(&endpoint, &[RUNS_JOBS_HEADER], body).map_err(|e| e.to_string())
    }

    fn job_endpoint(manifest: &Manifest, job_id: &str) -> Result<Endpoint, String> {
        let jobs = manifest.jobs.as_ref().ok_or_else(|| "this source declares no `jobs` endpoint".to_owned())?;
        if !jobs.has_placeholder("id") {
            return Err("the source's `jobs` path has no `{id}` placeholder".into());
        }
        Ok(jobs.with_placeholder("id", job_id))
    }

    /// `GET jobs/{id}`: `{state, log?}`.
    pub fn job_get(&self, source_id: &str, job_id: &str) -> Result<Value, String> {
        let (mut client, manifest) = self.client_for(source_id)?;
        client.get(&Self::job_endpoint(&manifest, job_id)?).map_err(|e| e.to_string())
    }

    /// `POST jobs/{id}` with a run's exit report.
    pub fn job_report(&self, source_id: &str, job_id: &str, report: &Value) -> Result<PostReply, String> {
        let (mut client, manifest) = self.client_for(source_id)?;
        client.post(&Self::job_endpoint(&manifest, job_id)?, &[], report).map_err(|e| e.to_string())
    }

    /// Fetch a panel. `path` must be the `endpoint` of a `panel` action in
    /// one of the source's cached view payloads — the payload is the closed
    /// list for panels, as the manifest is for views — and must pass the
    /// same path rules. The page is kept (see [`PANEL_CACHE_CAP`]) so its
    /// own actions can be judged and run as the `panel:<path>` view.
    pub fn panel(&self, source_id: &str, path: &str) -> Result<Value, String> {
        let (endpoint, generation) = {
            let inner = self.lock();
            let state = Self::trusted_state(&inner, source_id)?;
            if !state.names_panel(path) {
                return Err(format!("source {} names no panel at `{path}`", state.id));
            }
            (Endpoint::named_in_payload(path)?, state.generation)
        };
        let (mut client, _) = self.client_for(source_id)?;
        let page = client.get(&endpoint).map_err(|e| e.to_string())?;
        if page.is_object() {
            let mut inner = self.lock();
            // A source relaunched or re-trusted meanwhile keeps nothing from
            // before; the page is still returned for this one look.
            if let Some(state) = inner.sources.values_mut().find(|s| s.id == source_id && s.generation == generation && s.trusted) {
                state.panels.put(path, page.clone());
            }
        }
        Ok(page)
    }

    /// Record a job the source started.
    pub fn job_started(&self, source_id: &str, job: JobRecord) {
        let job_id = job.job_id.clone();
        {
            let mut inner = self.lock();
            let Some(state) = inner.sources.values_mut().find(|s| s.id == source_id) else { return };
            state.jobs.retain(|j| j.job_id != job.job_id);
            state.jobs.push(job);
            // Finished jobs roll off; running ones stay.
            while state.jobs.len() > JOB_LOG_CAP {
                match state.jobs.iter().position(|j| j.state != "running") {
                    Some(i) => {
                        state.jobs.remove(i);
                    }
                    None => break,
                }
            }
        }
        self.emit(json!({"event": "job", "source_id": source_id, "job_id": job_id}));
    }

    /// Update a job record in place.
    pub fn job_update(&self, source_id: &str, job_id: &str, update: impl FnOnce(&mut JobRecord)) {
        {
            let mut inner = self.lock();
            let Some(state) = inner.sources.values_mut().find(|s| s.id == source_id) else { return };
            let Some(job) = state.jobs.iter_mut().find(|j| j.job_id == job_id) else { return };
            update(job);
        }
        self.emit(json!({"event": "job", "source_id": source_id, "job_id": job_id}));
    }

    pub fn job(&self, source_id: &str, job_id: &str) -> Option<JobRecord> {
        let inner = self.lock();
        inner.sources.values().find(|s| s.id == source_id)?.jobs.iter().find(|j| j.job_id == job_id).cloned()
    }

    /// Whether the app is shutting down (job threads end then).
    pub fn is_shutting_down(&self) -> bool {
        self.lock().shutting_down
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
        unconfirmable_logged: Default::default(),
        jobs: Vec::new(),
        panels: PanelCache::default(),
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
    /// When this source's agent prompts are shown before a run.
    pub prompt_policy: PromptPolicy,
    /// Action ids whose prompt the user has already accepted once.
    pub prompts_shown: Vec<String>,
    /// The jobs started from this source, oldest first.
    pub jobs: Vec<JobRecord>,
}

/// One job a source started with a 202 reply: polled (`run: false`) or
/// hosted in a pane of the source's project (`run: true`), and reported
/// back to the source on exit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobRecord {
    pub job_id: String,
    pub action_id: String,
    pub view: String,
    pub key: Option<String>,
    pub started_utc: String,
    /// `running` | `done` | `failed` | `lost` (the source stopped answering).
    pub state: String,
    pub run: bool,
    pub term_id: Option<u32>,
    pub exit_code: Option<i32>,
    pub ended_utc: Option<String>,
    /// The source's `log` for a polled job; the exit report's note for a run.
    pub log: Option<String>,
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

fn source_dto(state: &SourceState, store: Option<&SourceStore>) -> SourceDto {
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
        prompt_policy: store.map(|s| s.prompt_policy(&state.key)).unwrap_or_default(),
        prompts_shown: store.map(|s| s.prompts_shown(&state.key)).unwrap_or_default(),
        jobs: state.jobs.clone(),
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
    fn the_panel_cache_keeps_the_most_recent_pages_per_path() {
        let mut cache = PanelCache::default();
        cache.put("/a", json!({"title": "A1"}));
        cache.put("/b", json!({"title": "B"}));
        cache.put("/a", json!({"title": "A2"}));
        assert_eq!(cache.get("/a").unwrap()["title"], "A2", "a refetch replaces the page");
        assert_eq!(cache.pages.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(), ["/b", "/a"], "and makes it the most recent");
        for i in 0..PANEL_CACHE_CAP {
            cache.put(&format!("/p{i}"), json!({}));
        }
        assert_eq!(cache.pages.len(), PANEL_CACHE_CAP);
        assert!(cache.get("/b").is_none() && cache.get("/a").is_none(), "the least recently fetched went first");
        assert!(cache.get(&format!("/p{}", PANEL_CACHE_CAP - 1)).is_some());
        cache.clear();
        assert!(cache.get("/p0").is_none());
    }

    #[test]
    fn a_panel_page_is_addressed_only_while_a_view_names_it() {
        let mut state = fresh_state("k", Origin::Attached { addr: "127.0.0.1:1".into(), token_file: PathBuf::from("t"), warning: None });
        let named = json!({"items": [{"key": "ABC-1", "actions": [{"kind": "panel", "id": "v", "label": "V", "endpoint": "/api/panel/validate"}]}]});
        state.views.insert(
            "work".into(),
            ViewState {
                endpoint: Endpoint::named_in_payload("/api/work").unwrap(),
                poll: Duration::from_secs(60),
                payload: Some(named),
                as_of_ms: None,
                next_due_ms: 0,
                refreshing: false,
                failures: 0,
                last_error: None,
            },
        );
        let view = format!("{PANEL_VIEW_PREFIX}/api/panel/validate");
        assert!(Sources::cached_payload(&state, &view).unwrap_err().contains("open it first"), "named but not fetched yet");
        state.panels.put("/api/panel/validate", json!({"title": "Validate", "actions": []}));
        assert_eq!(Sources::cached_payload(&state, &view).unwrap()["title"], "Validate");
        // A cached page of a path no view names answers nothing.
        state.panels.put("/api/panel/deeper", json!({"title": "Deeper"}));
        assert!(Sources::cached_payload(&state, "panel:/api/panel/deeper").unwrap_err().contains("names no panel"));
        // The view stops naming the panel: its page no longer answers.
        state.views.get_mut("work").unwrap().payload = Some(json!({"items": []}));
        assert!(Sources::cached_payload(&state, &view).unwrap_err().contains("names no panel"));
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
