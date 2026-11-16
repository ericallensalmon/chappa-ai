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
//!   token and proves itself again;
//! - **owner updates** ([`owner_updates`]): after an owning source's Work
//!   poll, the rows a trusted overlay source adds to are posted to it when
//!   they changed, if its manifest asks for them;
//! - **data-safety verdicts across restarts** ([`verdicts`]): every
//!   Environment answer's `dataSafety` block is kept on disk, so a block
//!   is in force from the moment the app starts, before the source is back,
//!   and a source once trusted as a stack blocks its folder from launch
//!   until its first answer. A launched source whose project is removed is
//!   forgotten with its record ([`Sources::forget_launched`]); one that
//!   `chappa.yml` stops declaring keeps its record in force until the new
//!   content is trusted ([`Sources::undeclare_launched`],
//!   [`Sources::content_trusted`]).
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
pub mod owner_updates;
pub mod store;
pub mod verdicts;

use std::collections::BTreeMap;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, State};
use term_core::actor::epoch_ms;

use actions::{
    action_verdict, actions_for, find_action, ActionVerdict, SourceAction, VerdictContext,
};
use client::{PostReply, SourceClient, SourceError};
use manifest::{
    backoff_delay, parse_manifest, trust_summary, Endpoint, Manifest, TrustSummary, VIEW_NAMES,
    WORKFLOWS_POLL, WORKFLOWS_VIEW,
};
use owner_updates::{pushes_after_poll, sent_key, OverlaySide, OwnerPush, PushQueue};
use store::{AttachedSource, SourceStore};
pub use store::{KeyMatch, PromptPolicy};
use verdicts::{RememberedStack, SavedVerdict, Undeclared, VerdictStore};

use crate::agent_tools::AgentTool;
use crate::data_safety::{
    self, active_block, parse_block, parse_secret_block, ActiveBlock, ENVIRONMENT_VIEW,
};
use crate::roots::Root;

/// Sent on every actions call: this app hosts a pane for a job's `run`
/// block, so a source may hand the job's CLI back instead of running it.
pub const RUNS_JOBS_HEADER: (&str, &str) = ("X-Chappa-Runs-Jobs", "1");
/// Sent on every actions call: who asked for it. `human:<name>` for a call
/// the person made in the app, `agent:<actor id>` for one an agent caused.
/// A source may let only people make some moves (a workflow gate), and it
/// records who acknowledged a finding.
pub const ACTOR_HEADER: &str = "X-Chappa-Actor";
/// The longest actor header value sent.
pub const ACTOR_HEADER_CAP: usize = 80;

/// Who caused a call to a source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallActor {
    /// The person at the app, by their OS user name.
    Human(String),
    /// An agent, by its actor id.
    Agent(String),
}

impl CallActor {
    /// The person running the app: the OS user name (no setting).
    pub fn human() -> Self {
        let name = ["USERNAME", "USER", "LOGNAME"]
            .iter()
            .find_map(|v| {
                std::env::var(v)
                    .ok()
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_else(|| "unknown".to_owned());
        CallActor::Human(name)
    }

    /// The header value: printable ASCII only (anything else becomes `_`),
    /// at most [`ACTOR_HEADER_CAP`] characters.
    pub fn header_value(&self) -> String {
        let raw = match self {
            CallActor::Human(name) => format!("human:{name}"),
            CallActor::Agent(id) => format!("agent:{id}"),
        };
        raw.chars()
            .map(|c| if (' '..='~').contains(&c) { c } else { '_' })
            .take(ACTOR_HEADER_CAP)
            .collect()
    }
}

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
        Err(format!(
            "source commands are accepted from the main window only (caller: `{label}`)"
        ))
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

pub(crate) struct NullEmitter;

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
    /// The folder a launched source works on: its `--project`, else the
    /// command's working folder. What a stack source's data safety covers
    /// before it has said anything.
    launch_folder: Option<PathBuf>,
    /// The hash of each owner row last sent to this source as an owner
    /// update (see [`owner_updates`]); forgotten on relaunch and re-trust,
    /// so it hears every row once more.
    owner_sent: BTreeMap<String, String>,
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
    /// Each source's last data-safety verdict, on disk.
    verdicts: Option<VerdictStore>,
    /// Owner rows waiting to be sent to the overlay sources that add to them.
    owner_queue: PushQueue,
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
                    verdicts: None,
                    owner_queue: PushQueue::default(),
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
        self.init_with(
            SourceStore::new(config_dir.join(store::FILE_NAME)),
            Arc::new(AppSourcesEmitter(app.clone())),
        );
    }

    /// Tests and headless services: an explicit store and emitter.
    pub fn init_with(&self, store: SourceStore, emitter: Arc<dyn SourcesEmitter>) {
        let attached: Vec<AttachedSource> = store.attached().to_vec();
        {
            let mut inner = self.lock();
            inner.verdicts = Some(VerdictStore::beside(store.path()));
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
    pub fn prepare_launch(
        &self,
        project_id: u32,
        project_path: &Path,
        command: &str,
    ) -> Result<LaunchEnv, String> {
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
            let state = inner
                .sources
                .entry(key.clone())
                .or_insert_with(|| fresh_state(&key, origin.clone()));
            state.generation += 1;
            state.origin = origin;
            state.status = SourceStatus::Starting;
            state.last_error = None;
            state.trusted = false;
            state.owner_sent.clear();
            for view in state.views.values_mut() {
                view.refreshing = false;
                view.failures = 0;
                view.next_due_ms = 0;
            }
            // Running again, so declared again: its record is an ordinary one.
            let id = state.id.clone();
            if let Some(verdicts) = inner.verdicts.as_mut() {
                if let Err(e) = verdicts.set_undeclared(&id, None) {
                    log::warn!("[data-safety] cannot update the record of {id}: {e}");
                }
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
            let Some(state) = inner.sources.get_mut(&key) else {
                return;
            };
            if let Origin::Launched { term_id: slot, .. } = &mut state.origin {
                *slot = Some(term_id);
            }
            state.generation
        };
        self.spawn_worker(key, generation);
    }

    /// Where a launched source works: its `--project` folder, else the
    /// command's working folder (see [`launch_folder_of`]). Data safety
    /// covers it for a stack source that has not answered yet.
    pub fn set_launch_folder(&self, project_path: &Path, command: &str, folder: PathBuf) {
        let key = SourceStore::launched_key(project_path, command);
        if let Some(state) = self.lock().sources.get_mut(&key) {
            state.launch_folder = Some(folder);
        }
    }

    /// The spawn failed after [`Sources::prepare_launch`]: nothing to wait for.
    pub fn launch_failed(&self, project_path: &Path, command: &str, error: &str) {
        self.process_gone(
            project_path,
            command,
            &format!("could not be started: {error}"),
        );
    }

    /// The source's process row ended (exited, stopped, project closed):
    /// the source is down, its worker ends, its payloads stay.
    pub fn process_gone(&self, project_path: &Path, command: &str, reason: &str) {
        let key = SourceStore::launched_key(project_path, command);
        {
            let mut inner = self.lock();
            let Some(state) = inner.sources.get_mut(&key) else {
                return;
            };
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

    /// A launched source whose project was removed from the app: its worker
    /// ends, it leaves the listing, and what was kept for it goes (its saved
    /// verdict and whether it was a stack), so a stack that is gone stops
    /// blocking its folder. Its trust record stays: declaring the same
    /// command again under the same manifest needs no new prompt. A source
    /// that only failed or crashed is not forgotten ([`Sources::process_gone`]
    /// keeps everything).
    pub fn forget_launched(&self, project_path: &Path, command: &str) {
        let key = SourceStore::launched_key(project_path, command);
        let id = source_id_of(&key);
        {
            let mut inner = self.lock();
            if let Some(mut state) = inner.sources.remove(&key) {
                state.generation += 1;
            }
            if let Some(verdicts) = inner.verdicts.as_mut() {
                if let Err(e) = verdicts.remove(&id) {
                    log::warn!("[data-safety] cannot drop the record of {id}: {e}");
                }
            }
        }
        self.notify();
        self.emit_sources();
    }

    /// A launched source whose `source:` command its project's `chappa.yml`
    /// no longer declares (the command is gone, or kept without the key):
    /// its worker ends and it leaves the listing, but what was kept for it
    /// stays and its saved block stays in force, marked as removed, until the
    /// project's new content is trusted ([`Sources::content_trusted`]). A
    /// change nobody approved must not lift a data-safety block.
    pub fn undeclare_launched(&self, project_path: &Path, command: &str) {
        let key = SourceStore::launched_key(project_path, command);
        let id = source_id_of(&key);
        {
            let mut inner = self.lock();
            if let Some(mut state) = inner.sources.remove(&key) {
                state.generation += 1;
            }
            if let Some(verdicts) = inner.verdicts.as_mut().filter(|v| v.holds(&id)) {
                let mark = Undeclared {
                    project: project_path.to_path_buf(),
                    command: command.to_owned(),
                };
                if let Err(e) = verdicts.set_undeclared(&id, Some(mark)) {
                    log::warn!("[data-safety] cannot mark {id} as removed: {e}");
                }
            }
        }
        self.notify();
        self.emit_sources();
    }

    /// The project at `project_path` has trusted content now, whose
    /// `source:` commands are `declared`: the records of its sources waiting
    /// on that go, except for any the content declares again (those stay as
    /// ordinary records).
    pub fn content_trusted(&self, project_path: &Path, declared: &[String]) {
        let changed = {
            let mut inner = self.lock();
            let Some(verdicts) = inner.verdicts.as_mut() else {
                return;
            };
            let waiting = verdicts.undeclared_in(project_path);
            for (id, u) in &waiting {
                let done = if declared.contains(&u.command) {
                    verdicts.set_undeclared(id, None)
                } else {
                    verdicts.remove(id)
                };
                if let Err(e) = done {
                    log::warn!("[data-safety] cannot update the record of {id}: {e}");
                }
            }
            !waiting.is_empty()
        };
        if changed {
            self.emit_sources();
        }
    }

    /// The project at `project_path` was removed from the app: the records
    /// of its sources that were waiting for trusted content go too.
    pub fn forget_project(&self, project_path: &Path) {
        let mut inner = self.lock();
        let Some(verdicts) = inner.verdicts.as_mut() else {
            return;
        };
        for (id, _) in verdicts.undeclared_in(project_path) {
            if let Err(e) = verdicts.remove(&id) {
                log::warn!("[data-safety] cannot drop the record of {id}: {e}");
            }
        }
    }

    // ---- attached sources ------------------------------------------------------

    /// Attach a server started elsewhere. The address must be loopback and
    /// the token file must hold the 64-hex token; on Unix it must be
    /// owner-only, elsewhere the listing carries a warning. Stored, so it
    /// comes back at the next launch.
    pub fn attach(&self, addr: &str, token_file: &Path) -> Result<(), String> {
        let addr = manifest::parse_loopback_addr(addr)?.to_string();
        read_attached_token(token_file)?;
        let source = AttachedSource {
            addr: addr.clone(),
            token_file: token_file.to_path_buf(),
        };
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
        let warning = read_attached_token(&source.token_file)
            .map(|(_, warning)| warning)
            .unwrap_or(None);
        let generation = {
            let mut inner = self.lock();
            let origin = Origin::Attached {
                addr: source.addr.clone(),
                token_file: source.token_file.clone(),
                warning,
            };
            let state = inner
                .sources
                .entry(key.clone())
                .or_insert_with(|| fresh_state(&key, origin.clone()));
            state.generation += 1;
            state.origin = origin;
            state.status = SourceStatus::Starting;
            state.last_error = None;
            state.trusted = false;
            state.owner_sent.clear();
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
                Origin::Launched { .. } => {
                    return Err(
                        "a launched source is detached by removing `source:` from its command"
                            .into(),
                    )
                }
            };
            if let Some(state) = inner.sources.get_mut(&key) {
                state.generation += 1;
            }
            inner.sources.remove(&key);
            if let Some(verdicts) = inner.verdicts.as_mut() {
                verdicts.remove(source_id)?;
            }
            let store = inner
                .store
                .as_mut()
                .ok_or_else(|| "sources store not initialized".to_owned())?;
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
                return Err(
                    "the source's manifest changed after the prompt was shown; nothing was trusted"
                        .into(),
                );
            }
            if !trust {
                return Ok(());
            }
            let store = inner
                .store
                .as_mut()
                .ok_or_else(|| "sources store not initialized".to_owned())?;
            if !store.is_trusted(&key, hash) {
                // A different manifest may be a different program: the
                // prompts it showed before do not count as shown.
                store.forget_prompts(&key)?;
            }
            store.mark_trusted(&key, hash)?;
            let state = inner.sources.get_mut(&key).expect("looked up above");
            state.trusted = true;
            state.owner_sent.clear();
            if state.status == SourceStatus::NeedsReview {
                state.status = SourceStatus::Starting;
            }
            for view in state.views.values_mut() {
                view.next_due_ms = 0;
                view.failures = 0;
            }
            remember_kind(&mut inner, &key);
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
            sources: inner
                .sources
                .values()
                .map(|s| source_dto(s, inner.store.as_ref()))
                .collect(),
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
            return Err(format!(
                "`{view}` is not a view (one of {})",
                VIEW_NAMES.join(", ")
            ));
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
        inner.shutting_down
            || inner
                .sources
                .get(key)
                .map_or(true, |s| s.cancelled(generation))
    }

    /// Mark the source down with `error`; payloads stay.
    fn set_down(&self, key: &str, generation: u64, status: SourceStatus, error: String) {
        {
            let mut inner = self.lock();
            let Some(state) = inner.sources.get_mut(key) else {
                return;
            };
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
            let attached = matches!(
                self.lock().sources.get(&key).map(|s| &s.origin),
                Some(Origin::Attached { .. })
            );
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
            if guard.shutting_down
                || guard
                    .sources
                    .get(key)
                    .map_or(true, |s| s.cancelled(generation))
            {
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
            Origin::Attached {
                addr, token_file, ..
            } => read_attached_token(token_file)
                .and_then(|(token, _)| SourceClient::new(addr, &token)),
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
                        self.set_down(
                            key,
                            generation,
                            SourceStatus::Down,
                            format!("not reachable at {}: {why}", origin.addr()),
                        );
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
        let manifest = match client
            .manifest()
            .map_err(|e| e.to_string())
            .and_then(|v| parse_manifest(&v).map_err(|e| e.to_string()))
        {
            Ok(manifest) => manifest,
            Err(text) => {
                let status = if text.contains("contract version") {
                    SourceStatus::NeedsUpdate
                } else {
                    SourceStatus::Down
                };
                self.set_down(key, generation, status, text);
                return RunEnd::Failed;
            }
        };
        {
            let mut inner = self.lock();
            let hash = manifest::trust_hash(&manifest);
            let trusted = inner
                .store
                .as_ref()
                .is_some_and(|s| s.is_trusted(key, &hash));
            let Some(state) = inner.sources.get_mut(key) else {
                return RunEnd::Cancelled;
            };
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
            state.status = if trusted {
                SourceStatus::Starting
            } else {
                SourceStatus::NeedsReview
            };
            remember_kind(&mut inner, key);
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
                    let Some(state) = guard.sources.get_mut(key) else {
                        return RunEnd::Cancelled;
                    };
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
                        Some(delay) => match self
                            .inner
                            .1
                            .wait_timeout(guard, delay.max(Duration::from_millis(10)))
                        {
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
    fn apply_poll(
        &self,
        key: &str,
        generation: u64,
        view: &str,
        poll: Duration,
        result: Result<Value, SourceError>,
    ) -> bool {
        let source_id = {
            let mut inner = self.lock();
            let Some(state) = inner.sources.get_mut(key) else {
                return true;
            };
            if state.cancelled(generation) {
                return true;
            }
            let now = epoch_ms();
            let fresh = matches!(&result, Ok(payload) if payload.is_object());
            // An Environment answer is this source's data-safety verdict now,
            // clean or not; kept on disk for the next start.
            let verdict = match &result {
                Ok(payload) if payload.is_object() && view == ENVIRONMENT_VIEW => {
                    Some(Self::verdict_of(state, payload, now))
                }
                _ => None,
            };
            let Some(v) = state.views.get_mut(view) else {
                return false;
            };
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
                    let whole_source =
                        matches!(e, SourceError::Unreachable(_) | SourceError::Untrusted(_))
                            || matches!(
                                e,
                                SourceError::Http {
                                    status: 401 | 403,
                                    ..
                                }
                            );
                    if whole_source {
                        state.status = SourceStatus::Down;
                        state.last_error = Some(e.to_string());
                    }
                }
            }
            let id = state.id.clone();
            if let (Some(verdict), Some(store)) = (verdict, inner.verdicts.as_mut()) {
                if let Err(e) = store.put(&id, verdict) {
                    log::warn!("[data-safety] cannot keep the verdict of {id}: {e}");
                }
            }
            let start_sender =
                fresh && view == "work" && Self::queue_owner_updates(&mut inner, key);
            (id, start_sender)
        };
        let (source_id, start_sender) = source_id;
        if start_sender {
            self.spawn_owner_sender();
        }
        self.emit(json!({"event": "view", "source_id": source_id, "view": view}));
        false
    }

    /// After a Work poll of the source `key`: queue each of its rows that a
    /// trusted overlay source asking for owner updates adds to and has not
    /// been sent in this form. Answers whether a sender must be started.
    fn queue_owner_updates(inner: &mut Inner, key: &str) -> bool {
        let pushes: Vec<OwnerPush> = {
            let Some(owner) = inner.sources.get(key) else {
                return false;
            };
            let (Some(manifest), Some(work)) = (
                owner.manifest.as_ref(),
                owner.views.get("work").and_then(|v| v.payload.as_ref()),
            ) else {
                return false;
            };
            let overlays: Vec<OverlaySide<'_>> = inner
                .sources
                .values()
                .filter(|s| s.key != key)
                .map(|s| OverlaySide {
                    id: &s.id,
                    accepts: s.trusted
                        && s.manifest
                            .as_ref()
                            .is_some_and(|m| m.owner_updates.is_some()),
                    work: s.views.get("work").and_then(|v| v.payload.as_ref()),
                    sent: &s.owner_sent,
                })
                .collect();
            pushes_after_poll(&manifest.id, work, &overlays)
        };
        let mut start = false;
        for push in pushes {
            if let Some(target) = inner.sources.values_mut().find(|s| s.id == push.target) {
                // Recorded when queued: a send that fails is not repeated
                // until the row changes again.
                target
                    .owner_sent
                    .insert(sent_key(&push.owner, &push.key), push.hash.clone());
            }
            start |= inner.owner_queue.add(push);
        }
        start
    }

    /// A sender for the owner-update queue: waits out the debounce, then
    /// posts what is queued, off the poll thread.
    fn spawn_owner_sender(&self) {
        let sources = self.clone();
        let spawned = std::thread::Builder::new()
            .name("source owner updates".into())
            .spawn(move || {
                std::thread::sleep(owner_updates::DEBOUNCE);
                if sources.is_shutting_down() {
                    return;
                }
                let batch = sources.lock().owner_queue.take();
                for push in batch {
                    sources.send_owner_update(&push);
                }
            });
        if let Err(e) = spawned {
            log::error!("[sources] cannot start the owner-update sender: {e}");
        }
    }

    /// `POST {ownerUpdates}` with one owner row. Only a `2xx` counts; any
    /// other answer or a failure is logged and dropped.
    fn send_owner_update(&self, push: &OwnerPush) {
        let sent = self
            .client_for(&push.target)
            .and_then(|(mut client, manifest)| {
                let endpoint = manifest
                    .owner_updates
                    .ok_or_else(|| "it no longer asks for owner updates".to_owned())?;
                client
                    .post(&endpoint, &[], &push.body())
                    .map_err(|e| e.to_string())
            });
        match sent {
            Ok(reply) if (200..300).contains(&reply.status) => {}
            Ok(reply) => log::warn!(
                "[sources] owner update {}:{} to {} refused: {}",
                push.owner,
                push.key,
                push.target,
                reply.as_error()
            ),
            Err(e) => log::warn!(
                "[sources] owner update {}:{} to {} not sent: {e}",
                push.owner,
                push.key,
                push.target
            ),
        }
    }

    /// The record an Environment answer leaves in [`verdicts`].
    fn verdict_of(state: &SourceState, payload: &Value, now: u64) -> SavedVerdict {
        let info = Self::info_of(state);
        SavedVerdict {
            block: parse_block(payload),
            secret_scan: parse_secret_block(payload),
            source_label: info.label,
            manifest_id: info.manifest_id,
            fallback: state.launch_folder.clone().or(info.project_path),
            saved_ms: now,
        }
    }
}

/// The folder a source command works on: the value of `--project <dir>` (or
/// `--project=<dir>`) among its arguments, read against `cwd` when
/// relative, else `cwd` itself. A command given as one shell string is
/// split on whitespace for this.
pub fn launch_folder_of(cwd: &Path, args: &[String]) -> PathBuf {
    let words: Vec<&str> = args.iter().flat_map(|a| a.split_whitespace()).collect();
    let named = words
        .iter()
        .enumerate()
        .find_map(|(i, w)| match w.strip_prefix("--project") {
            Some("") => words.get(i + 1).copied(),
            Some(rest) => rest.strip_prefix('='),
            None => None,
        });
    match named
        .map(|n| n.trim_matches(['"', '\'']))
        .filter(|n| !n.is_empty())
    {
        Some(dir) => {
            let dir = Path::new(dir);
            if dir.is_absolute() {
                dir.to_path_buf()
            } else {
                cwd.join(dir)
            }
        }
        None => cwd.to_path_buf(),
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

/// What an endpoint call's `target` carries besides `view` and `key` when
/// it names a Work or PR row. For an item action (offered on another
/// source's row) and for an overlay's actions (its own, its sections',
/// its banners'): `source` is the owning source's manifest id, `item` the
/// owner's row exactly as it was last received (absent when the owner does
/// not list it, or is not trusted), and `key` the owner's key. For the
/// actions of a row the calling source owns: its own manifest id and the
/// row as it sent it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CallTarget {
    pub key: Option<String>,
    pub source: Option<String>,
    pub item: Option<Value>,
}

impl CallTarget {
    /// The `target` object of the call body.
    pub fn body(&self, view: &str, key: Option<&str>) -> Value {
        let mut target = json!({ "view": view, "key": self.key.as_deref().or(key) });
        if let Some(source) = &self.source {
            target["source"] = Value::String(source.clone());
        }
        if let Some(item) = &self.item {
            target["item"] = item.clone();
        }
        target
    }
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
            return Err(format!(
                "source {} is not trusted; review it under Settings ▸ Sources first",
                state.id
            ));
        }
        Ok(state)
    }

    fn info_of(state: &SourceState) -> SourceInfo {
        let manifest = state.manifest.as_ref();
        let (project_id, project_path, term_id) = match &state.origin {
            Origin::Launched {
                project_id,
                project_path,
                term_id,
                ..
            } => (Some(*project_id), Some(project_path.clone()), *term_id),
            Origin::Attached { .. } => (None, None, None),
        };
        SourceInfo {
            id: state.id.clone(),
            manifest_id: manifest
                .map(|m| m.id.clone())
                .unwrap_or_else(|| state.id.clone()),
            label: manifest
                .map(|m| m.label.clone())
                .unwrap_or_else(|| state.key.clone()),
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
            .flat_map(|s| {
                s.manifest
                    .iter()
                    .flat_map(|m| m.roots.iter())
                    .map(move |r| (s.id.clone(), r.clone()))
            })
            .collect()
    }

    pub fn prompt_policy(&self, source_id: &str) -> Result<PromptPolicy, String> {
        let inner = self.lock();
        let state = inner
            .sources
            .values()
            .find(|s| s.id == source_id)
            .ok_or_else(|| format!("no source {source_id}"))?;
        Ok(inner
            .store
            .as_ref()
            .map(|st| st.prompt_policy(&state.key))
            .unwrap_or_default())
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

    /// Whether this source's work item keys join pull requests that mention
    /// them, and with which pattern (blank = the default).
    pub fn set_key_match(
        &self,
        source_id: &str,
        enabled: bool,
        pattern: Option<&str>,
    ) -> Result<(), String> {
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
                .set_key_match(&key, enabled, pattern)?;
        }
        self.emit_sources();
        Ok(())
    }

    /// Whether `action_id`'s prompt must be shown before it runs now:
    /// always under `PromptPolicy::Always`, else until it was accepted once.
    pub fn prompt_needs_dialog(&self, source_id: &str, action_id: &str) -> Result<bool, String> {
        let inner = self.lock();
        let state = Self::trusted_state(&inner, source_id)?;
        let store = inner
            .store
            .as_ref()
            .ok_or_else(|| "sources store not initialized".to_owned())?;
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
            return state.panels.get(path).cloned().ok_or_else(|| {
                format!(
                    "source {} has no page at `{path}` yet; open it first",
                    state.id
                )
            });
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
    pub fn actions(
        &self,
        source_id: &str,
        view: &str,
        key: Option<&str>,
        tools: &[AgentTool],
        roots: &[Root],
    ) -> Result<Vec<ActionDto>, String> {
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
                store
                    .map(|s| s.prompt_policy(&state.key))
                    .unwrap_or_default(),
                store
                    .map(|s| s.prompts_shown(&state.key))
                    .unwrap_or_default(),
            )
        };
        let ctx = VerdictContext {
            tools,
            roots,
            link_hosts: &hosts,
        };
        let blocks = self.data_safety_blocks();
        let mut out = Vec::new();
        let mut hide: Vec<String> = Vec::new();
        for f in found {
            let verdict = data_safety_verdict(&f.action, action_verdict(&f.action, &ctx), &blocks);
            if verdict.hidden {
                hide.push(f.action.id().to_owned());
                continue;
            }
            let needs_prompt = matches!(f.action, SourceAction::Agent { .. })
                && match policy {
                    PromptPolicy::Always => true,
                    PromptPolicy::FirstTime => !shown.iter().any(|id| id == f.action.id()),
                };
            out.push(ActionDto {
                action: f.action,
                verdict,
                needs_prompt,
            });
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
    pub fn action(
        &self,
        source_id: &str,
        view: &str,
        key: Option<&str>,
        action_id: &str,
    ) -> Result<(SourceAction, SourceInfo), String> {
        self.action_for_call(source_id, view, key, action_id)
            .map(|(action, info, _)| (action, info))
    }

    /// [`Sources::action`] plus what its call's `target` carries: for an
    /// action of a work entry that adds to another source's item, that item's
    /// owner and the owner's row as last received.
    pub fn action_for_call(
        &self,
        source_id: &str,
        view: &str,
        key: Option<&str>,
        action_id: &str,
    ) -> Result<(SourceAction, SourceInfo, CallTarget), String> {
        let inner = self.lock();
        let state = Self::trusted_state(&inner, source_id)?;
        let payload = Self::cached_payload(state, view)?;
        let found = find_action(&payload, key, action_id).ok_or_else(|| {
            format!(
                "source {} offers no action `{action_id}` on {} of its `{view}` view",
                state.id,
                key.map(|k| format!("`{k}`"))
                    .unwrap_or_else(|| "the view itself".to_owned())
            )
        })?;
        let info = Self::info_of(state);
        Ok((
            found.action,
            info.clone(),
            Self::row_target(&inner, &info, &payload, view, key),
        ))
    }

    /// What a call naming the `view` row `key` of a source's payload
    /// carries about that row, whichever of the row's actions it is (its
    /// own, a section's, a banner's): for a work entry that adds to
    /// another source's item, that owner, its key and its row as last
    /// received (no row while the owner does not list it); for a row the
    /// source owns itself (a work item, a pull request), the source and the
    /// row as it sent it. Nothing for the view's own actions.
    fn row_target(
        inner: &Inner,
        info: &SourceInfo,
        payload: &Value,
        view: &str,
        key: Option<&str>,
    ) -> CallTarget {
        let Some(key) = key else {
            return CallTarget::default();
        };
        if view == "work" {
            if let Some((owner, owner_key)) =
                actions::work_entry(payload, key).and_then(actions::overlay_ref)
            {
                return CallTarget {
                    key: Some(owner_key.to_owned()),
                    source: Some(owner.to_owned()),
                    item: Self::owner_row(inner, owner, "work", owner_key).map(|(_, row)| row),
                };
            }
        }
        if view == "work" || view == "prs" {
            if let Some(row) = actions::owned_row(payload, view, key) {
                return CallTarget {
                    key: Some(key.to_owned()),
                    source: Some(info.manifest_id.clone()),
                    item: Some(row.clone()),
                };
            }
        }
        CallTarget::default()
    }

    /// The row `key` of the `view` that the trusted source with manifest id
    /// `owner` itself lists, as last received, with that source. Sources are
    /// asked in listing order (the Settings order), so the first one that
    /// lists the row answers.
    fn owner_row<'a>(
        inner: &'a Inner,
        owner: &str,
        view: &str,
        key: &str,
    ) -> Option<(&'a SourceState, Value)> {
        inner
            .sources
            .values()
            .filter(|s| s.trusted && s.manifest.as_ref().is_some_and(|m| m.id == owner))
            .find_map(|s| {
                let payload = s.views.get(view)?.payload.as_ref()?;
                actions::owned_row(payload, view, key).map(|row| (s, row.clone()))
            })
    }

    /// The item actions `source_id` offers on the `view` row `key` of the
    /// source `owner_id`, their confirm texts filled from that row, and the
    /// call target. Refused when either source is not trusted, when the owner
    /// is the declaring source itself, or when the owner does not list the
    /// row. Empty when the declaring source already adds to the row (its
    /// overlay brings its own actions) or offers nothing there.
    fn item_actions_on(
        inner: &Inner,
        source_id: &str,
        view: &str,
        owner_id: &str,
        key: &str,
    ) -> Result<(Vec<SourceAction>, SourceInfo, CallTarget), String> {
        let state = Self::trusted_state(inner, source_id)?;
        let owner = Self::trusted_state(inner, owner_id)?;
        if owner.id == state.id {
            return Err(format!(
                "source {} offers its item actions on other sources' rows only",
                state.id
            ));
        }
        let manifest = state.manifest.as_ref().expect("trusted implies a manifest");
        let owner_manifest = owner.manifest.as_ref().expect("trusted implies a manifest");
        let row = owner
            .views
            .get(view)
            .and_then(|v| v.payload.as_ref())
            .and_then(|p| actions::owned_row(p, view, key))
            .ok_or_else(|| {
                format!(
                    "{} lists no `{key}` in its `{view}` view",
                    owner_manifest.label
                )
            })?;
        let target = CallTarget {
            key: Some(key.to_owned()),
            source: Some(owner_manifest.id.clone()),
            item: Some(row.clone()),
        };
        let adds_to_it = view == "work"
            && state
                .views
                .get("work")
                .and_then(|v| v.payload.as_ref())
                .is_some_and(|p| actions::has_overlay(p, &owner_manifest.id, key));
        if adds_to_it {
            return Ok((Vec::new(), Self::info_of(state), target));
        }
        let title = row.get("title").and_then(Value::as_str);
        let offered = manifest
            .item_actions
            .iter()
            .filter(|a| a.offered_on(view, &owner_manifest.id))
            .map(|a| match &a.action {
                SourceAction::Endpoint {
                    id,
                    label,
                    mutates,
                    confirm,
                    params,
                    note,
                    inputs,
                    disabled,
                } => SourceAction::Endpoint {
                    id: id.clone(),
                    label: label.clone(),
                    mutates: *mutates,
                    confirm: confirm
                        .as_deref()
                        .map(|c| actions::fill_confirm(c, key, title, &owner_manifest.label)),
                    params: params.clone(),
                    note: *note,
                    inputs: inputs.clone(),
                    disabled: disabled.clone(),
                },
                other => other.clone(),
            })
            .collect();
        Ok((offered, Self::info_of(state), target))
    }

    /// The judged item actions `source_id` offers on the `view` row `key`
    /// of the source `owner_id` (see [`Sources::item_actions_on`]).
    pub fn item_actions(
        &self,
        source_id: &str,
        view: &str,
        owner_id: &str,
        key: &str,
        tools: &[AgentTool],
        roots: &[Root],
    ) -> Result<Vec<ActionDto>, String> {
        let (offered, info) = {
            let inner = self.lock();
            let (offered, info, _) = Self::item_actions_on(&inner, source_id, view, owner_id, key)?;
            (offered, info)
        };
        let ctx = VerdictContext {
            tools,
            roots,
            link_hosts: &info.link_hosts,
        };
        let blocks = self.data_safety_blocks();
        Ok(offered
            .into_iter()
            .map(|action| {
                let verdict = data_safety_verdict(&action, action_verdict(&action, &ctx), &blocks);
                ActionDto {
                    action,
                    verdict,
                    needs_prompt: false,
                }
            })
            .collect())
    }

    /// The item action `action_id` of `source_id` on the `view` row `key` of
    /// `owner_id`, for the runner. Only an id the source's trusted manifest
    /// declares for that row can be named: the webview cannot invent one.
    pub fn item_action(
        &self,
        source_id: &str,
        view: &str,
        owner_id: &str,
        key: &str,
        action_id: &str,
    ) -> Result<(SourceAction, SourceInfo, CallTarget), String> {
        let inner = self.lock();
        let (offered, info, target) =
            Self::item_actions_on(&inner, source_id, view, owner_id, key)?;
        let action = offered.into_iter().find(|a| a.id() == action_id).ok_or_else(|| {
            format!("source {} offers no item action `{action_id}` on `{key}` of that source's `{view}` view", info.id)
        })?;
        Ok((action, info, target))
    }

    /// Rule for links a trusted source hands over: on a host the user
    /// trusted with the manifest, https — or plain http to this machine
    /// (see `sources::links`). A plain-http name is looked up here, with a
    /// deadline, after the lock is released; call it off the UI thread.
    pub fn link_check(&self, source_id: &str, url: &str) -> Result<(), String> {
        let hosts = {
            let inner = self.lock();
            let state = Self::trusted_state(&inner, source_id)?;
            state
                .manifest
                .as_ref()
                .map(|m| m.link_hosts.clone())
                .unwrap_or_default()
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
            (
                state.origin.clone(),
                state.manifest.clone().expect("trusted implies a manifest"),
            )
        };
        let client = match &origin {
            Origin::Launched { addr, token, .. } => SourceClient::new(addr, token)?,
            Origin::Attached {
                addr, token_file, ..
            } => {
                let (token, _) = read_attached_token(token_file)?;
                SourceClient::new(addr, &token)?
            }
        };
        Ok((client, manifest))
    }

    /// `POST {actions}` with the app's runs-jobs header and who asked
    /// ([`ACTOR_HEADER`]). The status is the outcome: 200 done, 202
    /// started, 503 uncertain, anything else the source's refusal.
    pub fn call_endpoint(
        &self,
        source_id: &str,
        body: &Value,
        actor: &CallActor,
    ) -> Result<PostReply, String> {
        let (mut client, manifest) = self.client_for(source_id)?;
        let endpoint = manifest
            .actions
            .ok_or_else(|| "this source declares no `actions` endpoint".to_owned())?;
        let who = actor.header_value();
        client
            .post(
                &endpoint,
                &[RUNS_JOBS_HEADER, (ACTOR_HEADER, who.as_str())],
                body,
            )
            .map_err(|e| e.to_string())
    }

    fn job_endpoint(manifest: &Manifest, job_id: &str) -> Result<Endpoint, String> {
        let jobs = manifest
            .jobs
            .as_ref()
            .ok_or_else(|| "this source declares no `jobs` endpoint".to_owned())?;
        if !jobs.has_placeholder("id") {
            return Err("the source's `jobs` path has no `{id}` placeholder".into());
        }
        Ok(jobs.with_placeholder("id", job_id))
    }

    /// `GET jobs/{id}`: `{state, log?}`.
    pub fn job_get(&self, source_id: &str, job_id: &str) -> Result<Value, String> {
        let (mut client, manifest) = self.client_for(source_id)?;
        client
            .get(&Self::job_endpoint(&manifest, job_id)?)
            .map_err(|e| e.to_string())
    }

    /// `POST jobs/{id}` with a run's exit report.
    pub fn job_report(
        &self,
        source_id: &str,
        job_id: &str,
        report: &Value,
    ) -> Result<PostReply, String> {
        let (mut client, manifest) = self.client_for(source_id)?;
        client
            .post(&Self::job_endpoint(&manifest, job_id)?, &[], report)
            .map_err(|e| e.to_string())
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
            if let Some(state) = inner
                .sources
                .values_mut()
                .find(|s| s.id == source_id && s.generation == generation && s.trusted)
            {
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
            let Some(state) = inner.sources.values_mut().find(|s| s.id == source_id) else {
                return;
            };
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
            let Some(state) = inner.sources.values_mut().find(|s| s.id == source_id) else {
                return;
            };
            let Some(job) = state.jobs.iter_mut().find(|j| j.job_id == job_id) else {
                return;
            };
            update(job);
        }
        self.emit(json!({"event": "job", "source_id": source_id, "job_id": job_id}));
    }

    pub fn job(&self, source_id: &str, job_id: &str) -> Option<JobRecord> {
        let inner = self.lock();
        inner
            .sources
            .values()
            .find(|s| s.id == source_id)?
            .jobs
            .iter()
            .find(|j| j.job_id == job_id)
            .cloned()
    }

    /// Whether the app is shutting down (job threads end then).
    pub fn is_shutting_down(&self) -> bool {
        self.lock().shutting_down
    }

    /// Poll every view of `source_id` again now, not at its next interval:
    /// an action the source just did may have changed what they show (an
    /// acknowledgement that lifts a data-safety block should not wait out a
    /// two-minute poll).
    pub fn refresh_soon(&self, source_id: &str) {
        {
            let mut inner = self.lock();
            if let Some(state) = inner.sources.values_mut().find(|s| s.id == source_id) {
                for view in state.views.values_mut() {
                    view.next_due_ms = 0;
                }
            }
        }
        self.notify();
    }

    // ---- data safety ------------------------------------------------------------

    /// The data-safety blocks in force (see [`crate::data_safety`]), one per
    /// source:
    ///
    /// - a source that answered its Environment view since the app started:
    ///   the block of its LAST payload, which counts while the source is
    ///   down, untrusted or failing its Environment poll (marked as the last
    ///   known one), until it answers with one that lifts it;
    /// - one that has not answered yet: the verdict saved from its last
    ///   answer before ([`verdicts`]), last known; one saved as not blocking
    ///   allows;
    /// - a trusted stack source (manifest id `stack:…`) with no saved
    ///   verdict, a new one or one never heard from: its folder is blocked
    ///   until its first answer; so is one remembered as a stack from an
    ///   earlier trusted manifest, from its launch on, before (or without)
    ///   a trusted manifest this run;
    /// - a saved verdict of a source not registered (yet) this run, last
    ///   known.
    ///
    /// Each source can hold two blocks at once, its `dataSafety` and its
    /// `secretScan`; both follow these rules, each on its own. The "never
    /// heard from" hold is data safety's alone (it already keeps agents out
    /// of the whole folder until the first answer, which carries both).
    pub fn data_safety_blocks(&self) -> Vec<ActiveBlock> {
        let inner = self.lock();
        let saved = |id: &str| inner.verdicts.as_ref().and_then(|v| v.get(id));
        let remembered = |id: &str| inner.verdicts.as_ref().and_then(|v| v.stack(id));
        let mut out = Vec::new();
        for state in inner.sources.values() {
            let info = Self::info_of(state);
            let fallback = state
                .launch_folder
                .clone()
                .or_else(|| info.project_path.clone());
            let view = state.views.get(ENVIRONMENT_VIEW);
            if let Some(payload) = view.and_then(|v| v.payload.as_ref()) {
                let current = state.trusted
                    && state.status == SourceStatus::Up
                    && view.is_some_and(|v| v.last_error.is_none());
                for block in [parse_block(payload), parse_secret_block(payload)]
                    .into_iter()
                    .flatten()
                {
                    out.extend(active_block(
                        &state.id,
                        &info.label,
                        &block,
                        current,
                        fallback.as_deref(),
                    ));
                }
                continue;
            }
            match saved(&state.id) {
                Some(v) => {
                    let label = state
                        .manifest
                        .as_ref()
                        .map(|m| m.label.as_str())
                        .unwrap_or(&v.source_label);
                    out.extend(saved_blocks(&state.id, label, v));
                }
                None => out.extend(Self::unheard_block(
                    state,
                    &info,
                    fallback,
                    remembered(&state.id),
                )),
            }
        }
        if let Some(store) = &inner.verdicts {
            for (id, v) in store.all() {
                if !inner.sources.values().any(|s| &s.id == id) {
                    let removed = store.undeclared(id).is_some();
                    for wire in [v.block.as_ref(), v.secret_scan.as_ref()]
                        .into_iter()
                        .flatten()
                    {
                        let Some(mut b) =
                            active_block(id, &v.source_label, wire, false, v.fallback.as_deref())
                        else {
                            continue;
                        };
                        if removed {
                            b.reason = data_safety::removed_reason(wire);
                        }
                        out.push(b);
                    }
                }
            }
        }
        out
    }

    /// A stack source that has never sent a verdict (none saved, no answer
    /// yet): its folder is kept from agents until it answers. A stack is one
    /// whose trusted manifest says so, or, before such a manifest is in
    /// hand, one `remembered` as a stack from an earlier one.
    fn unheard_block(
        state: &SourceState,
        info: &SourceInfo,
        folder: Option<PathBuf>,
        remembered: Option<&RememberedStack>,
    ) -> Option<ActiveBlock> {
        let (manifest_id, label) = match state.manifest.as_ref() {
            Some(manifest) if state.trusted && is_stack_manifest(manifest) => {
                (manifest.id.as_str(), info.label.as_str())
            }
            _ => {
                let r = remembered?;
                (r.manifest_id.as_str(), r.source_label.as_str())
            }
        };
        let folder = folder?;
        Some(ActiveBlock {
            kind: data_safety::BlockKind::DataSafety,
            source_id: state.id.clone(),
            source_label: label.to_owned(),
            instance: manifest_id
                .strip_prefix(STACK_SOURCE_PREFIX)
                .unwrap_or(manifest_id)
                .to_owned(),
            state: data_safety::UNHEARD_STATE.to_owned(),
            current: false,
            reason: data_safety::unheard_reason(label),
            folders: vec![crate::roots::display(&folder)],
            bound: None,
            component: None,
            ack_action_id: None,
            covered: vec![folder],
        })
    }
}

/// The manifest id prefix of a stack source: one whose data safety is
/// enforced from before its first answer.
pub const STACK_SOURCE_PREFIX: &str = "stack:";

/// A stack source: a `stack:…` manifest id serving the Environment view
/// (one that serves none could never answer, so is never held to it).
fn is_stack_manifest(manifest: &Manifest) -> bool {
    manifest.id.starts_with(STACK_SOURCE_PREFIX) && manifest.views.contains_key(ENVIRONMENT_VIEW)
}

/// Keep what the source `key`'s manifest, now trusted, says about being a
/// stack, for its next launch: set when it is one, cleared when it is not.
/// A manifest that is not trusted changes nothing (fail closed).
fn remember_kind(inner: &mut Inner, key: &str) {
    let Some(state) = inner.sources.get(key) else {
        return;
    };
    let Some(manifest) = state.manifest.as_ref().filter(|_| state.trusted) else {
        return;
    };
    let stack = is_stack_manifest(manifest).then(|| RememberedStack {
        manifest_id: manifest.id.clone(),
        source_label: manifest.label.clone(),
    });
    let id = state.id.clone();
    if let Some(store) = inner.verdicts.as_mut() {
        if let Err(e) = store.set_stack(&id, stack) {
            log::warn!("[data-safety] cannot keep whether {id} is a stack source: {e}");
        }
    }
}

/// A saved verdict's blocks in force (data safety and secret scan), as the
/// last known ones.
fn saved_blocks(id: &str, label: &str, v: &SavedVerdict) -> Vec<ActiveBlock> {
    [v.block.as_ref(), v.secret_scan.as_ref()]
        .into_iter()
        .flatten()
        .filter_map(|b| active_block(id, label, b, false, v.fallback.as_deref()))
        .collect()
}

/// An agent action whose folder data safety blocks is disabled with the
/// reason: the launch would be refused anyway, and the button says so first.
fn data_safety_verdict(
    action: &SourceAction,
    mut verdict: ActionVerdict,
    blocks: &[ActiveBlock],
) -> ActionVerdict {
    if let (SourceAction::Agent { .. }, true, Some(cwd)) =
        (action, verdict.enabled, verdict.cwd.as_ref())
    {
        if let Some(b) = data_safety::blocking(blocks, cwd) {
            verdict.enabled = false;
            verdict.reason = Some(data_safety::disabled_text(b));
        }
    }
    verdict
}

enum RunEnd {
    Cancelled,
    Failed,
}

/// The opaque id of the source with trust key `key`: the same on every run.
pub(crate) fn source_id_of(key: &str) -> String {
    project_model::hash::content_hash(key.as_bytes())[..16].to_owned()
}

fn fresh_state(key: &str, origin: Origin) -> SourceState {
    SourceState {
        key: key.to_owned(),
        id: source_id_of(key),
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
        launch_folder: None,
        owner_sent: BTreeMap::new(),
    }
}

/// Bind an ephemeral loopback port and release it. The source binds the same
/// port a moment later; the window in between is the usual one every
/// "pick a free port" scheme has, and the identity check is what makes a
/// hijack of that window harmless (the token never reaches the hijacker).
fn free_loopback_port() -> Result<u16, String> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|e| format!("cannot pick a loopback port: {e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    drop(listener);
    Ok(port)
}

/// The attached token: the file's trimmed content, which must decode as the
/// 32-byte token. On Unix a file another user can read is refused outright
/// (that user could then impersonate the source to the app). Elsewhere the
/// permission model is not checked and the listing says so.
pub fn read_attached_token(path: &Path) -> Result<(String, Option<String>), String> {
    let meta = std::fs::metadata(path)
        .map_err(|e| format!("cannot read the token file {}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("{} is not a file", path.display()));
    }
    let warning = token_file_permission_check(path, &meta)?;
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read the token file {}: {e}", path.display()))?;
    let token = text.trim().to_owned();
    if identity::decode_token(&token).is_none() {
        return Err(format!(
            "{} does not hold a 64-character lowercase hex token",
            path.display()
        ));
    }
    Ok((token, warning))
}

#[cfg(unix)]
fn token_file_permission_check(
    path: &Path,
    meta: &std::fs::Metadata,
) -> Result<Option<String>, String> {
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
fn token_file_permission_check(
    path: &Path,
    _meta: &std::fs::Metadata,
) -> Result<Option<String>, String> {
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
    Launched {
        project_id: u32,
        project_path: String,
        command: String,
    },
    Attached {
        addr: String,
        token_file: String,
    },
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
    /// Whether its work item keys join pull requests that mention them.
    pub key_match: KeyMatch,
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
        Origin::Launched {
            project_id,
            project_path,
            command,
            ..
        } => (
            OriginDto::Launched {
                project_id: *project_id,
                project_path: project_path.display().to_string(),
                command: command.clone(),
            },
            command.clone(),
            None,
        ),
        Origin::Attached {
            addr,
            token_file,
            warning,
        } => (
            OriginDto::Attached {
                addr: addr.clone(),
                token_file: token_file.display().to_string(),
            },
            addr.clone(),
            warning.clone(),
        ),
    };
    SourceDto {
        id: state.id.clone(),
        label: state
            .manifest
            .as_ref()
            .map(|m| m.label.clone())
            .unwrap_or(fallback_label),
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
        prompt_policy: store
            .map(|s| s.prompt_policy(&state.key))
            .unwrap_or_default(),
        prompts_shown: store
            .map(|s| s.prompts_shown(&state.key))
            .unwrap_or_default(),
        jobs: state.jobs.clone(),
        key_match: store.map(|s| s.key_match(&state.key)).unwrap_or_default(),
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
pub fn list_sources(
    window: tauri::Window,
    sources: State<'_, Sources>,
) -> Result<SourcesListing, String> {
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
pub fn source_view(
    window: tauri::Window,
    sources: State<'_, Sources>,
    source_id: String,
    view: String,
) -> Result<SourceViewDto, String> {
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

/// Switch a source's key matching on or off and set its pattern (blank =
/// the default).
#[tauri::command]
pub fn set_source_key_match(
    window: tauri::Window,
    sources: State<'_, Sources>,
    source_id: String,
    enabled: bool,
    pattern: Option<String>,
) -> Result<SourcesListing, String> {
    main_window_only(&window)?;
    sources.set_key_match(&source_id, enabled, pattern.as_deref())?;
    Ok(sources.listing())
}

#[tauri::command]
pub fn attach_source(
    window: tauri::Window,
    sources: State<'_, Sources>,
    addr: String,
    token_file: String,
) -> Result<SourcesListing, String> {
    main_window_only(&window)?;
    sources.attach(&addr, Path::new(&token_file))?;
    Ok(sources.listing())
}

#[tauri::command]
pub fn detach_source(
    window: tauri::Window,
    sources: State<'_, Sources>,
    source_id: String,
) -> Result<SourcesListing, String> {
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
        assert_eq!(
            cache.get("/a").unwrap()["title"],
            "A2",
            "a refetch replaces the page"
        );
        assert_eq!(
            cache
                .pages
                .iter()
                .map(|(p, _)| p.as_str())
                .collect::<Vec<_>>(),
            ["/b", "/a"],
            "and makes it the most recent"
        );
        for i in 0..PANEL_CACHE_CAP {
            cache.put(&format!("/p{i}"), json!({}));
        }
        assert_eq!(cache.pages.len(), PANEL_CACHE_CAP);
        assert!(
            cache.get("/b").is_none() && cache.get("/a").is_none(),
            "the least recently fetched went first"
        );
        assert!(cache.get(&format!("/p{}", PANEL_CACHE_CAP - 1)).is_some());
        cache.clear();
        assert!(cache.get("/p0").is_none());
    }

    #[test]
    fn a_panel_page_is_addressed_only_while_a_view_names_it() {
        let mut state = fresh_state(
            "k",
            Origin::Attached {
                addr: "127.0.0.1:1".into(),
                token_file: PathBuf::from("t"),
                warning: None,
            },
        );
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
        assert!(
            Sources::cached_payload(&state, &view)
                .unwrap_err()
                .contains("open it first"),
            "named but not fetched yet"
        );
        state.panels.put(
            "/api/panel/validate",
            json!({"title": "Validate", "actions": []}),
        );
        assert_eq!(
            Sources::cached_payload(&state, &view).unwrap()["title"],
            "Validate"
        );
        // A cached page of a path no view names answers nothing.
        state
            .panels
            .put("/api/panel/deeper", json!({"title": "Deeper"}));
        assert!(Sources::cached_payload(&state, "panel:/api/panel/deeper")
            .unwrap_err()
            .contains("names no panel"));
        // The view stops naming the panel: its page no longer answers.
        state.views.get_mut("work").unwrap().payload = Some(json!({"items": []}));
        assert!(Sources::cached_payload(&state, &view)
            .unwrap_err()
            .contains("names no panel"));
    }

    #[test]
    fn launch_env_carries_exactly_the_two_variables() {
        let env = LaunchEnv {
            addr: "127.0.0.1:5000".into(),
            token: "ab".repeat(32),
        };
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
        assert!(read_attached_token(&path)
            .unwrap_err()
            .contains("64-character"));
        std::fs::write(&path, format!("{}\n", "0a".repeat(32))).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(read_attached_token(&path)
                .unwrap_err()
                .contains("readable by other users"));
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
        let a = fresh_state(
            "/p::feed",
            Origin::Attached {
                addr: "x".into(),
                token_file: "t".into(),
                warning: None,
            },
        );
        let b = fresh_state(
            "/p::feed",
            Origin::Attached {
                addr: "x".into(),
                token_file: "t".into(),
                warning: None,
            },
        );
        let c = fresh_state(
            "/p::other",
            Origin::Attached {
                addr: "x".into(),
                token_file: "t".into(),
                warning: None,
            },
        );
        assert_eq!(a.id, b.id);
        assert_ne!(a.id, c.id);
        assert_eq!(a.id.len(), 16);
        assert!(a.id.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    /// A launched source with an Environment view whose cached payload is
    /// `payload`, in `status`, trusted or not, its last poll failed or not.
    fn put_environment(
        sources: &Sources,
        project: &Path,
        payload: Option<Value>,
        status: SourceStatus,
        trusted: bool,
        poll_error: Option<&str>,
    ) -> String {
        let mut state = fresh_state(
            "stack",
            Origin::Launched {
                project_id: 1,
                project_path: project.to_path_buf(),
                command: "stack".into(),
                addr: "127.0.0.1:1".into(),
                token: "t".into(),
                term_id: None,
            },
        );
        state.status = status;
        state.trusted = trusted;
        state.views.insert(
            ENVIRONMENT_VIEW.into(),
            ViewState {
                endpoint: Endpoint::named_in_payload("/api/env").unwrap(),
                poll: Duration::from_secs(120),
                payload,
                as_of_ms: None,
                next_due_ms: 0,
                refreshing: false,
                failures: u32::from(poll_error.is_some()),
                last_error: poll_error.map(str::to_owned),
            },
        );
        let id = state.id.clone();
        sources.lock().sources.insert("stack".into(), state);
        id
    }

    #[test]
    fn a_block_stays_in_force_while_its_source_cannot_answer_and_clears_when_it_says_so() {
        use crate::delivery_hold::HoldGate;
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("stack");
        let inside = project.join(".work").join("ABC-1");
        std::fs::create_dir_all(&inside).unwrap();
        let payload = |state: &str, blocks: bool| {
            json!({ "components": [], "dataSafety": {
                "instance": "app/0", "state": state, "blocksAgents": blocks, "bound": null,
                "project": project.display().to_string(),
                "reasons": [{ "component": "db", "text": "db: 1 finding (card-number ×1)" }],
            }})
        };
        let sources = Sources::default();
        // Never sent a block: nothing is enforced.
        put_environment(&sources, &project, None, SourceStatus::Up, true, None);
        assert!(sources.data_safety_blocks().is_empty());
        put_environment(
            &sources,
            &project,
            Some(json!({ "components": [] })),
            SourceStatus::Up,
            true,
            None,
        );
        assert!(
            sources.data_safety_blocks().is_empty(),
            "an older source sends no block"
        );
        assert_eq!(sources.hold_reason(&inside), None);
        // Blocking, and answering.
        put_environment(
            &sources,
            &project,
            Some(payload("not-clean", true)),
            SourceStatus::Up,
            true,
            None,
        );
        let blocks = sources.data_safety_blocks();
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].current);
        assert_eq!(
            blocks[0].reason,
            "db: 1 finding (card-number ×1) — acknowledge in Environment"
        );
        assert!(sources
            .hold_reason(&inside)
            .unwrap()
            .starts_with("held until data safety on app/0 clears"));
        assert!(crate::data_safety::check_launch(&sources, &inside)
            .unwrap_err()
            .starts_with(crate::data_safety::REFUSAL_LEAD));
        assert!(
            crate::data_safety::check_launch(&sources, dir.path()).is_ok(),
            "outside the stack project"
        );
        // Down, untrusted, or failing its Environment poll: the last block stays, marked.
        for (status, trusted, error) in [
            (SourceStatus::Down, true, None),
            (SourceStatus::NeedsReview, false, None),
            (SourceStatus::Up, false, None),
            (SourceStatus::Up, true, Some("HTTP 500")),
        ] {
            put_environment(
                &sources,
                &project,
                Some(payload("not-clean", true)),
                status,
                trusted,
                error,
            );
            let blocks = sources.data_safety_blocks();
            assert_eq!(blocks.len(), 1, "{status:?} {trusted} {error:?}");
            assert!(!blocks[0].current);
            assert!(
                blocks[0].reason.starts_with("last known: db: 1 finding")
                    && blocks[0].reason.ends_with(", source unavailable"),
                "{}",
                blocks[0].reason
            );
            assert!(crate::data_safety::check_launch(&sources, &inside).is_err());
        }
        // A last block that did not block keeps nothing out.
        put_environment(
            &sources,
            &project,
            Some(payload("clean", false)),
            SourceStatus::Down,
            true,
            None,
        );
        assert!(sources.data_safety_blocks().is_empty());
        // An error blocks whatever it says, and recovery clears it.
        put_environment(
            &sources,
            &project,
            Some(payload("error", false)),
            SourceStatus::Up,
            true,
            None,
        );
        assert_eq!(sources.data_safety_blocks().len(), 1);
        put_environment(
            &sources,
            &project,
            Some(payload("acknowledged", false)),
            SourceStatus::Up,
            true,
            None,
        );
        assert!(sources.data_safety_blocks().is_empty());
        assert_eq!(sources.hold_reason(&inside), None);
        assert!(crate::data_safety::check_launch(&sources, &inside).is_ok());
    }

    #[test]
    fn a_block_without_a_project_covers_the_folder_of_the_project_that_launched_it() {
        let dir = tempfile::tempdir().unwrap();
        let sources = Sources::default();
        put_environment(
            &sources,
            dir.path(),
            Some(json!({ "dataSafety": { "instance": "app/0", "state": "error" } })),
            SourceStatus::Up,
            true,
            None,
        );
        let blocks = sources.data_safety_blocks();
        assert_eq!(blocks.len(), 1);
        assert!(crate::data_safety::covers(
            &blocks[0].covered,
            &dir.path().join("sub")
        ));
    }

    #[test]
    fn an_agent_action_inside_a_blocked_folder_is_disabled_with_the_reason() {
        let dir = tempfile::tempdir().unwrap();
        let sources = Sources::default();
        put_environment(
            &sources,
            dir.path(),
            Some(
                json!({ "dataSafety": { "instance": "app/0", "state": "not-clean", "blocksAgents": true, "reasons": [{ "component": "db", "text": "db: 1 finding (card-number ×1)" }] } }),
            ),
            SourceStatus::Up,
            true,
            None,
        );
        let blocks = sources.data_safety_blocks();
        let agent = actions::parse_action(&json!({ "kind": "agent", "id": "a", "label": "Agent here", "cwd": "/x", "prompt": "go" })).unwrap();
        let terminal = actions::parse_action(
            &json!({ "kind": "terminal", "id": "t", "label": "Terminal here", "cwd": "/x" }),
        )
        .unwrap();
        let ok = |cwd: &Path| ActionVerdict {
            enabled: true,
            reason: None,
            hidden: false,
            cwd: Some(cwd.to_path_buf()),
            tool: Some((1, "claude".into())),
        };
        let judged = data_safety_verdict(&agent, ok(dir.path()), &blocks);
        assert!(!judged.enabled);
        assert_eq!(
            judged.reason.as_deref(),
            Some("agents blocked — data safety on app/0: db: 1 finding (card-number ×1) — acknowledge in Environment")
        );
        assert!(
            data_safety_verdict(&terminal, ok(dir.path()), &blocks).enabled,
            "terminals are never blocked"
        );
        let elsewhere = tempfile::tempdir().unwrap();
        assert!(data_safety_verdict(&agent, ok(elsewhere.path()), &blocks).enabled);
    }

    #[test]
    fn the_actor_header_names_a_person_or_an_agent_in_printable_ascii() {
        assert_eq!(CallActor::Human("Alex".into()).header_value(), "human:Alex");
        assert_eq!(CallActor::Agent("12".into()).header_value(), "agent:12");
        assert_eq!(
            CallActor::Human("Jos\u{e9}\r\nX-Evil: 1".into()).header_value(),
            "human:Jos___X-Evil: 1"
        );
        let long = CallActor::Human("a".repeat(200)).header_value();
        assert_eq!(long.len(), ACTOR_HEADER_CAP);
        assert!(long.starts_with("human:aaa"));
        let me = CallActor::human();
        assert!(matches!(&me, CallActor::Human(n) if !n.is_empty()));
        assert!(me.header_value().starts_with("human:"));
    }

    /// A stack source's manifest: id `id`, serving the Environment view.
    fn stack_manifest(id: &str) -> Manifest {
        parse_manifest(&json!({
            "contract": 1, "source": {"id": id, "label": "Stack app", "version": "1"},
            "views": {"environment": {"list": "/api/env"}}
        }))
        .unwrap()
    }

    fn with_manifest(sources: &Sources, id: &str) {
        sources.lock().sources.get_mut("stack").unwrap().manifest = Some(stack_manifest(id));
    }

    /// Sources over a config folder, as the app starts them.
    fn started(config: &Path) -> Sources {
        let sources = Sources::default();
        sources.init_with(
            SourceStore::new(config.join(store::FILE_NAME)),
            Arc::new(NullEmitter),
        );
        sources
    }

    fn block_payload(project: &Path, state: &str, blocks: bool) -> Value {
        json!({ "components": [], "dataSafety": {
            "instance": "app/0", "state": state, "blocksAgents": blocks, "bound": null,
            "project": project.display().to_string(),
            "reasons": [{ "component": "db", "text": "db: 1 finding (card-number ×1)" }],
            "ackActionId": "data-safety-ack",
        }})
    }

    fn answer(sources: &Sources, payload: Value) {
        assert!(
            !sources.apply_poll(
                "stack",
                0,
                ENVIRONMENT_VIEW,
                Duration::from_secs(120),
                Ok(payload)
            ),
            "not cancelled"
        );
    }

    fn secret_payload(project: &Path, folder: &Path, state: &str, blocks: bool) -> Value {
        json!({ "components": [],
        "dataSafety": { "instance": "app/0", "state": "clean", "blocksAgents": false, "bound": null,
                        "project": project.display().to_string(), "reasons": [] },
        "secretScan": {
            "instance": "app/0", "state": state, "blocksAgents": blocks, "policy": "block-agents",
            "findings": [{ "rule": "github-pat", "file": ".work/ABC-1/src/api-1a2b3c4d/.env", "line": 1 }],
            "bound": { "source": "tracker", "key": "ABC-1", "folders": [folder.display().to_string()] },
            "project": project.display().to_string(),
            "reasons": [{ "component": null, "text": "api worktree: 1 possible secret (.work/ABC-1/src/api-1a2b3c4d/.env:1 (github-pat))" }],
            "ackActionId": "secret-scan-ack",
        }})
    }

    #[test]
    fn a_secret_scan_block_is_enforced_like_data_safety_and_kept_across_a_restart() {
        use crate::data_safety::BlockKind;
        use crate::delivery_hold::HoldGate;
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config");
        std::fs::create_dir_all(&config).unwrap();
        let project = dir.path().join("stack");
        let item = project.join(".work").join("ABC-1");
        let wt = item.join("src").join("api-1a2b3c4d");
        std::fs::create_dir_all(&wt).unwrap();

        let first = started(&config);
        put_environment(&first, &project, None, SourceStatus::Up, true, None);
        with_manifest(&first, "stack:app");
        answer(&first, secret_payload(&project, &item, "not-clean", true));
        let blocks = first.data_safety_blocks();
        assert_eq!(
            blocks.len(),
            1,
            "the clean data-safety block keeps nothing out"
        );
        assert_eq!(blocks[0].kind, BlockKind::SecretScan);
        assert!(blocks[0].current);
        // The one spawn path refuses, naming the check; deliveries are held.
        let refusal = crate::data_safety::check_launch(&first, &wt).unwrap_err();
        assert!(
            refusal.starts_with(crate::data_safety::REFUSAL_LEAD),
            "{refusal}"
        );
        assert!(
            refusal.contains("the secret scan on app/0") && refusal.contains(".env:1 (github-pat)"),
            "{refusal}"
        );
        assert!(first
            .hold_reason(&wt)
            .unwrap()
            .starts_with("held until the secret scan on app/0 clears"));
        assert!(
            crate::data_safety::check_launch(&first, &project).is_ok(),
            "the project itself is not the bound item"
        );
        let agent = actions::parse_action(&json!({ "kind": "agent", "id": "a", "label": "Agent here", "cwd": "/x", "prompt": "go" })).unwrap();
        let ok = ActionVerdict {
            enabled: true,
            reason: None,
            hidden: false,
            cwd: Some(wt.clone()),
            tool: Some((1, "claude".into())),
        };
        let judged = data_safety_verdict(&agent, ok, &blocks);
        assert!(!judged.enabled);
        assert!(judged
            .reason
            .unwrap()
            .starts_with("agents blocked — secret scan on app/0: api worktree"));
        // An unfinished scan the source says does not block (commits, build
        // contexts) keeps nothing out: unlike data safety, `error` alone is not a block.
        answer(&first, secret_payload(&project, &item, "error", false));
        assert!(first.data_safety_blocks().is_empty());
        answer(&first, secret_payload(&project, &item, "error", true));
        assert_eq!(first.data_safety_blocks().len(), 1);
        // Both checks blocking: one block each.
        let mut both = secret_payload(&project, &item, "not-clean", true);
        both["dataSafety"]["state"] = "not-clean".into();
        both["dataSafety"]["blocksAgents"] = true.into();
        answer(&first, both);
        let kinds: Vec<BlockKind> = first.data_safety_blocks().iter().map(|b| b.kind).collect();
        assert_eq!(kinds, [BlockKind::DataSafety, BlockKind::SecretScan]);
        answer(&first, secret_payload(&project, &item, "not-clean", true));
        let saved = std::fs::read_to_string(config.join(verdicts::FILE_NAME)).unwrap();
        assert!(
            saved.contains("\"secretScan\""),
            "kept in its own field: {saved}"
        );

        // Restart: the saved secret block is in force before any answer.
        let second = started(&config);
        let blocks = second.data_safety_blocks();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].kind, BlockKind::SecretScan);
        assert!(!blocks[0].current);
        assert!(
            blocks[0].reason.starts_with("last known: api worktree"),
            "{}",
            blocks[0].reason
        );
        assert!(crate::data_safety::check_launch(&second, &wt).is_err());
        // Its acknowledgement lifts it.
        put_environment(&second, &project, None, SourceStatus::Up, true, None);
        with_manifest(&second, "stack:app");
        answer(
            &second,
            secret_payload(&project, &item, "acknowledged", false),
        );
        assert!(second.data_safety_blocks().is_empty());
        assert!(crate::data_safety::check_launch(&second, &wt).is_ok());
        assert_eq!(second.hold_reason(&wt), None);
    }

    #[test]
    fn a_verdict_is_kept_across_a_restart_and_in_force_before_the_first_answer() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config");
        std::fs::create_dir_all(&config).unwrap();
        let project = dir.path().join("stack");
        let inside = project.join(".work").join("ABC-1");
        std::fs::create_dir_all(&inside).unwrap();

        // First run: the stack source answers that agents are blocked.
        let first = started(&config);
        let id = put_environment(&first, &project, None, SourceStatus::Up, true, None);
        with_manifest(&first, "stack:app");
        answer(&first, block_payload(&project, "not-clean", true));
        assert_eq!(first.data_safety_blocks().len(), 1);
        assert!(
            config.join(verdicts::FILE_NAME).is_file(),
            "kept in the config folder"
        );

        // Restart, before the source is even registered: the saved block
        // is in force, as the last known one.
        let second = started(&config);
        let blocks = second.data_safety_blocks();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].source_id, id);
        assert!(!blocks[0].current);
        assert!(
            blocks[0].reason.starts_with("last known: db: 1 finding"),
            "{}",
            blocks[0].reason
        );
        assert!(crate::data_safety::check_launch(&second, &inside).is_err());
        // Registered (launched, not trusted or answered yet): still in force.
        put_environment(&second, &project, None, SourceStatus::Starting, false, None);
        assert_eq!(second.data_safety_blocks().len(), 1);
        assert!(crate::data_safety::check_launch(&second, &inside).is_err());
        // Its first answer replaces the record: clean now, and allowed.
        second.lock().sources.get_mut("stack").unwrap().trusted = true;
        with_manifest(&second, "stack:app");
        answer(&second, block_payload(&project, "clean", false));
        assert!(second.data_safety_blocks().is_empty());
        assert!(crate::data_safety::check_launch(&second, &inside).is_ok());

        // Restart again: a saved clean verdict allows from the start, even
        // for a stack source that has not answered yet.
        let third = started(&config);
        assert!(third.data_safety_blocks().is_empty());
        put_environment(&third, &project, None, SourceStatus::Up, true, None);
        with_manifest(&third, "stack:app");
        assert!(third.data_safety_blocks().is_empty());
        // And it stays clean while it cannot answer (marked unavailable in
        // Environment, nothing enforced).
        third.lock().sources.get_mut("stack").unwrap().status = SourceStatus::Down;
        assert!(third.data_safety_blocks().is_empty());
    }

    #[test]
    fn a_stack_source_never_heard_from_blocks_its_folder_until_it_answers() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config");
        std::fs::create_dir_all(&config).unwrap();
        let project = dir.path().join("stack");
        let deep = project.join(".work").join("ABC-1");
        std::fs::create_dir_all(&deep).unwrap();
        let sources = started(&config);
        put_environment(&sources, &project, None, SourceStatus::Up, true, None);
        with_manifest(&sources, "stack:app");
        let blocks = sources.data_safety_blocks();
        assert_eq!(blocks.len(), 1);
        assert_eq!(
            (blocks[0].state.as_str(), blocks[0].instance.as_str()),
            (crate::data_safety::UNHEARD_STATE, "app")
        );
        assert!(!blocks[0].current);
        assert_eq!(
            blocks[0].reason,
            "Stack app has not said yet whether its data is safe — agents wait until it answers"
        );
        let refusal = crate::data_safety::check_launch(&sources, &deep).unwrap_err();
        assert!(
            refusal.starts_with(crate::data_safety::REFUSAL_LEAD),
            "{refusal}"
        );
        assert!(
            crate::data_safety::check_launch(&sources, dir.path()).is_ok(),
            "outside its folder"
        );
        // The launch folder, when known, is what is covered.
        let elsewhere = dir.path().join("other").join("stack-project");
        std::fs::create_dir_all(&elsewhere).unwrap();
        sources
            .lock()
            .sources
            .get_mut("stack")
            .unwrap()
            .launch_folder = Some(elsewhere.clone());
        assert!(crate::data_safety::check_launch(&sources, &elsewhere).is_err());
        assert!(crate::data_safety::check_launch(&sources, &deep).is_ok());
        sources
            .lock()
            .sources
            .get_mut("stack")
            .unwrap()
            .launch_folder = None;
        // An answer with no block at all lifts it, and is kept as such.
        answer(&sources, json!({ "components": [] }));
        assert!(sources.data_safety_blocks().is_empty());
        let restarted = started(&config);
        put_environment(&restarted, &project, None, SourceStatus::Up, true, None);
        with_manifest(&restarted, "stack:app");
        assert!(
            restarted.data_safety_blocks().is_empty(),
            "it answered before: no longer never heard from"
        );
    }

    #[test]
    fn only_a_trusted_stack_source_is_held_to_its_first_answer() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config");
        std::fs::create_dir_all(&config).unwrap();
        let project = dir.path().join("p");
        std::fs::create_dir_all(&project).unwrap();
        let sources = started(&config);
        // Not a stack source: never heard from, nothing enforced.
        put_environment(&sources, &project, None, SourceStatus::Up, true, None);
        with_manifest(&sources, "github");
        assert!(sources.data_safety_blocks().is_empty());
        // A stack source not trusted (yet): nothing either; its manifest
        // is not something the user agreed to.
        put_environment(
            &sources,
            &project,
            None,
            SourceStatus::NeedsReview,
            false,
            None,
        );
        with_manifest(&sources, "stack:app");
        assert!(sources.data_safety_blocks().is_empty());
        // One that serves no Environment view cannot answer, so is not held.
        put_environment(&sources, &project, None, SourceStatus::Up, true, None);
        sources.lock().sources.get_mut("stack").unwrap().manifest = Some(
            parse_manifest(&json!({"contract": 1, "source": {"id": "stack:app", "label": "S", "version": "1"}, "views": {"work": {"list": "/w"}}})).unwrap(),
        );
        assert!(sources.data_safety_blocks().is_empty());
        // Without a config folder (no store), the same rule still holds.
        let bare = Sources::default();
        put_environment(&bare, &project, None, SourceStatus::Up, true, None);
        with_manifest(&bare, "stack:app");
        assert_eq!(bare.data_safety_blocks().len(), 1);
    }

    #[test]
    fn a_source_detached_in_settings_takes_its_verdict_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config");
        std::fs::create_dir_all(&config).unwrap();
        let project = dir.path().join("p");
        std::fs::create_dir_all(&project).unwrap();
        let sources = started(&config);
        let key = SourceStore::attached_key("127.0.0.1:7000");
        let mut state = fresh_state(
            &key,
            Origin::Attached {
                addr: "127.0.0.1:7000".into(),
                token_file: dir.path().join("t"),
                warning: None,
            },
        );
        state.trusted = true;
        state.status = SourceStatus::Up;
        state.views.insert(
            ENVIRONMENT_VIEW.into(),
            ViewState {
                endpoint: Endpoint::named_in_payload("/api/env").unwrap(),
                poll: Duration::from_secs(120),
                payload: None,
                as_of_ms: None,
                next_due_ms: 0,
                refreshing: false,
                failures: 0,
                last_error: None,
            },
        );
        let id = state.id.clone();
        sources.lock().sources.insert(key.clone(), state);
        assert!(!sources.apply_poll(
            &key,
            0,
            ENVIRONMENT_VIEW,
            Duration::from_secs(120),
            Ok(block_payload(&project, "not-clean", true))
        ));
        assert_eq!(sources.data_safety_blocks().len(), 1);
        assert!(VerdictStore::beside(&config.join(store::FILE_NAME))
            .get(&id)
            .is_some());
        sources.detach(&id).unwrap();
        assert!(sources.data_safety_blocks().is_empty());
        assert!(VerdictStore::beside(&config.join(store::FILE_NAME))
            .get(&id)
            .is_none());
        assert!(
            started(&config).data_safety_blocks().is_empty(),
            "and it is not back after a restart"
        );
    }

    #[test]
    fn the_launch_folder_is_the_project_argument_or_the_working_folder() {
        let cwd = if cfg!(windows) {
            PathBuf::from("C:\\code\\x")
        } else {
            PathBuf::from("/srv/example")
        };
        let abs = if cfg!(windows) {
            "C:\\code\\stack"
        } else {
            "/srv/stack"
        };
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(launch_folder_of(&cwd, &args(&["serve"])), cwd);
        assert_eq!(
            launch_folder_of(&cwd, &args(&["serve", "--project", "stack"])),
            cwd.join("stack")
        );
        assert_eq!(
            launch_folder_of(&cwd, &args(&["serve", "--project", abs])),
            PathBuf::from(abs)
        );
        assert_eq!(
            launch_folder_of(&cwd, &args(&["serve", &format!("--project={abs}")])),
            PathBuf::from(abs)
        );
        // A command given as one shell string.
        assert_eq!(
            launch_folder_of(
                &cwd,
                &args(&["/C", "chappa-stack serve --project stack --slot 1"])
            ),
            cwd.join("stack")
        );
        // Not the flag: a longer word, or no value after it.
        assert_eq!(launch_folder_of(&cwd, &args(&["--projects", "x"])), cwd);
        assert_eq!(launch_folder_of(&cwd, &args(&["serve", "--project"])), cwd);
    }

    /// A source with a Work view whose payload is `work`, under `key`.
    fn put_work(
        sources: &Sources,
        key: &str,
        manifest: Value,
        work: Option<Value>,
        trusted: bool,
    ) -> String {
        let mut state = fresh_state(
            key,
            Origin::Attached {
                addr: "127.0.0.1:1".into(),
                token_file: PathBuf::from("t"),
                warning: None,
            },
        );
        state.trusted = trusted;
        state.status = SourceStatus::Up;
        state.manifest = Some(parse_manifest(&manifest).unwrap());
        state.views.insert(
            "work".into(),
            ViewState {
                endpoint: Endpoint::named_in_payload("/api/work").unwrap(),
                poll: Duration::from_secs(60),
                payload: work,
                as_of_ms: None,
                next_due_ms: 0,
                refreshing: false,
                failures: 0,
                last_error: None,
            },
        );
        let id = state.id.clone();
        sources.lock().sources.insert(key.into(), state);
        id
    }

    #[test]
    fn an_owner_poll_queues_changed_rows_for_trusted_overlays_that_ask_for_them() {
        let sources = Sources::default();
        // Shut down first: a sender started by a poll then gives up at once,
        // so the queue is read here rather than sent.
        sources.shutdown();
        let manifest = |id: &str, owner_updates: bool| {
            let mut m = json!({"contract": 1, "source": {"id": id, "label": id, "version": "1"}, "views": {"work": {"list": "/api/work"}}});
            if owner_updates {
                m["ownerUpdates"] = json!("/api/work/owner");
            }
            m
        };
        let overlay = json!({"items": [{"ref": {"source": "tracker", "key": "ABC-1"}}]});
        let asks = put_work(
            &sources,
            "a-asks",
            manifest("ws", true),
            Some(overlay.clone()),
            true,
        );
        put_work(
            &sources,
            "b-silent",
            manifest("other", false),
            Some(overlay.clone()),
            true,
        );
        put_work(
            &sources,
            "c-untrusted",
            manifest("third", true),
            Some(overlay.clone()),
            false,
        );
        put_work(&sources, "tracker", manifest("tracker", false), None, true);
        let row = |status: &str| json!({"items": [{"key": "ABC-1", "status": {"label": status}}]});
        let poll = |status: &str| {
            sources.apply_poll(
                "tracker",
                0,
                "work",
                Duration::from_secs(60),
                Ok(row(status)),
            )
        };
        assert!(!poll("To Do"));
        let queued = |sources: &Sources| sources.lock().owner_queue.len();
        assert_eq!(
            queued(&sources),
            1,
            "only the trusted overlay asking for them"
        );
        let batch = sources.lock().owner_queue.take();
        assert_eq!(batch[0].target, asks);
        assert_eq!(
            batch[0].body(),
            json!({"source": "tracker", "key": "ABC-1", "item": {"key": "ABC-1", "status": {"label": "To Do"}}})
        );
        // The same row on the next poll: nothing; a changed one: queued.
        assert!(!poll("To Do"));
        assert_eq!(queued(&sources), 0);
        assert!(!poll("Doing"));
        assert_eq!(queued(&sources), 1);
        // A failed poll queues nothing.
        let _ = sources.lock().owner_queue.take();
        assert!(!sources.apply_poll(
            "tracker",
            0,
            "work",
            Duration::from_secs(60),
            Err(SourceError::Unreachable("down".into()))
        ));
        assert_eq!(queued(&sources), 0);
        // An overlay re-trusted hears every row once more.
        sources
            .lock()
            .sources
            .get_mut("a-asks")
            .unwrap()
            .owner_sent
            .clear();
        assert!(!poll("Doing"));
        assert_eq!(queued(&sources), 1);
    }

    /// Launch the `source:` command `command` of `project` as projects.rs
    /// does (fresh address and token, the launch folder set); the worker is
    /// not started. Answers its trust key.
    fn launch(sources: &Sources, project: &Path, command: &str) -> String {
        sources.prepare_launch(1, project, command).unwrap();
        sources.set_launch_folder(project, command, project.to_path_buf());
        SourceStore::launched_key(project, command)
    }

    /// A manifest with id `id`; `environment` = it serves that view.
    fn manifest_of(id: &str, label: &str, environment: bool) -> Manifest {
        let views = if environment {
            json!({"environment": {"list": "/api/env"}})
        } else {
            json!({"work": {"list": "/w"}})
        };
        parse_manifest(&json!({"contract": 1, "source": {"id": id, "label": label, "version": "1"}, "views": views})).unwrap()
    }

    /// The manifest arrives as the worker takes it: the views it names, its
    /// trust summary, trusted when its hash is on record.
    fn manifest_arrives(sources: &Sources, key: &str, manifest: Manifest) {
        let mut inner = sources.lock();
        let hash = manifest::trust_hash(&manifest);
        let trusted = inner
            .store
            .as_ref()
            .is_some_and(|s| s.is_trusted(key, &hash));
        let state = inner.sources.get_mut(key).unwrap();
        for (name, v) in &manifest.views {
            state
                .views
                .entry(name.clone())
                .or_insert_with(|| ViewState {
                    endpoint: v.list.clone(),
                    poll: v.poll,
                    payload: None,
                    as_of_ms: None,
                    next_due_ms: 0,
                    refreshing: false,
                    failures: 0,
                    last_error: None,
                });
        }
        state.trust = Some(trust_summary(&manifest));
        state.manifest = Some(manifest);
        state.trusted = trusted;
        state.status = if trusted {
            SourceStatus::Starting
        } else {
            SourceStatus::NeedsReview
        };
        remember_kind(&mut inner, key);
    }

    /// The user trusts what the dialog shows.
    fn trust_shown(sources: &Sources, key: &str) {
        let (id, hash) = {
            let inner = sources.lock();
            let state = &inner.sources[key];
            (state.id.clone(), state.trust.as_ref().unwrap().hash.clone())
        };
        sources.trust_answer(&id, true, &hash).unwrap();
    }

    fn answer_as(sources: &Sources, key: &str, payload: Value) {
        let generation = sources.lock().sources[key].generation;
        assert!(
            !sources.apply_poll(
                key,
                generation,
                ENVIRONMENT_VIEW,
                Duration::from_secs(120),
                Ok(payload)
            ),
            "not cancelled"
        );
    }

    fn kept(config: &Path, key: &str) -> (bool, bool) {
        let store = VerdictStore::beside(&config.join(store::FILE_NAME));
        let id = source_id_of(key);
        (store.get(&id).is_some(), store.stack(&id).is_some())
    }

    /// A config folder, a stack project folder and a folder inside it.
    fn stack_dirs() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config");
        std::fs::create_dir_all(&config).unwrap();
        let project = dir.path().join("stack");
        let inside = project.join(".work").join("ABC-1");
        std::fs::create_dir_all(&inside).unwrap();
        (dir, config, project, inside)
    }

    fn undeclared_mark(config: &Path, key: &str) -> bool {
        VerdictStore::beside(&config.join(store::FILE_NAME))
            .undeclared(&source_id_of(key))
            .is_some()
    }

    #[test]
    fn a_source_no_longer_declared_keeps_its_block_until_the_new_content_is_trusted() {
        let (_dir, config, project, inside) = stack_dirs();
        let first = started(&config);
        let key = launch(&first, &project, "stack");
        manifest_arrives(&first, &key, manifest_of("stack:app", "Stack app", true));
        trust_shown(&first, &key);
        answer_as(&first, &key, block_payload(&project, "not-clean", true));
        assert!(crate::data_safety::check_launch(&first, &inside).is_err());

        // Crashing or failing to start keeps everything: it still blocks.
        first.process_gone(&project, "stack", "exited with code 1");
        assert!(crate::data_safety::check_launch(&first, &inside).is_err());
        first.prepare_launch(1, &project, "stack").unwrap();
        first.launch_failed(&project, "stack", "no such program");
        assert!(crate::data_safety::check_launch(&first, &inside).is_err());
        assert_eq!(kept(&config, &key), (true, true));

        // Its `source:` command is gone from chappa.yml, in content nobody
        // trusted yet: it leaves the listing, its block stays, marked.
        first.undeclare_launched(&project, "stack");
        assert!(first.listing().sources.is_empty(), "it leaves the listing");
        let blocks = first.data_safety_blocks();
        assert_eq!(blocks.len(), 1);
        assert!(
            blocks[0].reason.starts_with("last known: db: 1 finding")
                && blocks[0]
                    .reason
                    .ends_with(", source removed — trust the new chappa.yml to lift"),
            "{}",
            blocks[0].reason
        );
        assert!(crate::data_safety::check_launch(&first, &inside).is_err());
        assert_eq!(kept(&config, &key), (true, true));
        assert!(undeclared_mark(&config, &key));
        // Across a restart too.
        let restarted = started(&config);
        assert!(restarted.data_safety_blocks()[0]
            .reason
            .contains("source removed"));
        // Trust for another project's content changes nothing.
        restarted.content_trusted(&project.join("elsewhere"), &[]);
        assert!(crate::data_safety::check_launch(&restarted, &inside).is_err());
        // The new content is trusted: the record goes, the block lifts.
        restarted.content_trusted(&project, &[]);
        assert!(restarted.data_safety_blocks().is_empty());
        assert!(crate::data_safety::check_launch(&restarted, &inside).is_ok());
        assert_eq!(kept(&config, &key), (false, false));
        assert!(!undeclared_mark(&config, &key));
        assert!(
            started(&config).data_safety_blocks().is_empty(),
            "and it is not back after a restart"
        );
        // Its trust record stays: declared again, the same manifest is trusted.
        let again = started(&config);
        let key = launch(&again, &project, "stack");
        manifest_arrives(&again, &key, manifest_of("stack:app", "Stack app", true));
        assert!(again.lock().sources[&key].trusted);
    }

    #[test]
    fn a_source_declared_again_before_the_trust_answer_keeps_an_ordinary_record() {
        let (_dir, config, project, inside) = stack_dirs();
        let first = started(&config);
        let key = launch(&first, &project, "stack");
        manifest_arrives(&first, &key, manifest_of("stack:app", "Stack app", true));
        trust_shown(&first, &key);
        answer_as(&first, &key, block_payload(&project, "not-clean", true));
        // Removed, then the trusted content declares it again: the record
        // stays (it is that source's last answer), no longer marked removed.
        first.undeclare_launched(&project, "stack");
        first.content_trusted(&project, &["stack".to_owned()]);
        assert_eq!(kept(&config, &key), (true, true));
        assert!(!undeclared_mark(&config, &key));
        assert!(first.data_safety_blocks()[0]
            .reason
            .ends_with(", source unavailable"));
        // Removed, then launched again: the same.
        first.undeclare_launched(&project, "stack");
        assert!(undeclared_mark(&config, &key));
        launch(&first, &project, "stack");
        assert!(!undeclared_mark(&config, &key));
        assert!(crate::data_safety::check_launch(&first, &inside).is_err());
        // A source with nothing kept leaves no mark.
        first.undeclare_launched(&project, "fresh");
        assert!(!undeclared_mark(
            &config,
            &SourceStore::launched_key(&project, "fresh")
        ));
    }

    #[test]
    fn a_removed_project_drops_the_records_of_sources_not_running_this_run() {
        let (_dir, config, project, inside) = stack_dirs();
        let first = started(&config);
        let key = launch(&first, &project, "stack");
        manifest_arrives(&first, &key, manifest_of("stack:app", "Stack app", true));
        trust_shown(&first, &key);
        answer_as(&first, &key, block_payload(&project, "not-clean", true));
        // Restart: the source is not launched, its saved block is in force.
        let second = started(&config);
        assert!(crate::data_safety::check_launch(&second, &inside).is_err());
        // The project is removed: nothing of it is enforced any more, even
        // a source an untrusted edit had stopped declaring.
        second.undeclare_launched(&project, "stack");
        assert!(crate::data_safety::check_launch(&second, &inside).is_err());
        second.forget_project(&project);
        assert!(second.data_safety_blocks().is_empty());
        assert_eq!(kept(&config, &key), (false, false));
        // Another source's record is not touched.
        let third = started(&config);
        let other = launch(&third, &project, "other");
        manifest_arrives(&third, &other, manifest_of("stack:other", "Other", true));
        trust_shown(&third, &other);
        answer_as(&third, &other, block_payload(&project, "not-clean", true));
        third.forget_launched(&project, "stack");
        assert_eq!(kept(&config, &other), (true, true));
        assert_eq!(third.data_safety_blocks().len(), 1);
        // A running source of a removed project goes at once too.
        third.forget_launched(&project, "other");
        assert!(third.listing().sources.is_empty());
        assert!(third.data_safety_blocks().is_empty());
        assert_eq!(kept(&config, &other), (false, false));
    }

    #[test]
    fn a_source_remembered_as_a_stack_blocks_from_its_launch_until_its_first_answer() {
        let (_dir, config, project, inside) = stack_dirs();
        // First run: trusted as a stack, then gone before it ever answered.
        let first = started(&config);
        let key = launch(&first, &project, "stack");
        manifest_arrives(&first, &key, manifest_of("stack:app", "Stack app", true));
        assert_eq!(
            kept(&config, &key),
            (false, false),
            "not trusted yet: nothing remembered"
        );
        trust_shown(&first, &key);
        assert_eq!(kept(&config, &key), (false, true));
        first.process_gone(&project, "stack", "exited with code 1");

        // Next run: nothing is enforced before it is launched...
        let second = started(&config);
        assert!(second.data_safety_blocks().is_empty());
        // ...and from its launch, before any manifest, its folder is held.
        let key = launch(&second, &project, "stack");
        assert!(second.lock().sources[&key].manifest.is_none());
        let blocks = second.data_safety_blocks();
        assert_eq!(blocks.len(), 1);
        assert_eq!(
            (
                blocks[0].state.as_str(),
                blocks[0].instance.as_str(),
                blocks[0].source_label.as_str()
            ),
            (crate::data_safety::UNHEARD_STATE, "app", "Stack app")
        );
        assert_eq!(
            blocks[0].reason,
            crate::data_safety::unheard_reason("Stack app")
        );
        assert!(crate::data_safety::check_launch(&second, &inside).is_err());
        assert!(
            crate::data_safety::check_launch(&second, project.parent().unwrap()).is_ok(),
            "outside its folder"
        );
        // Its launch folder, when it names another, is what is covered.
        let elsewhere = project.parent().unwrap().join("served");
        std::fs::create_dir_all(&elsewhere).unwrap();
        second.set_launch_folder(&project, "stack", elsewhere.clone());
        assert!(crate::data_safety::check_launch(&second, &elsewhere).is_err());
        second.set_launch_folder(&project, "stack", project.clone());
        // A crash before the manifest keeps the hold.
        second.process_gone(&project, "stack", "exited with code 1");
        assert!(crate::data_safety::check_launch(&second, &inside).is_err());
        // Relaunched, the trusted manifest arrives: still held until it answers.
        let key = launch(&second, &project, "stack");
        manifest_arrives(&second, &key, manifest_of("stack:app", "Stack app", true));
        assert!(second.lock().sources[&key].trusted);
        assert!(crate::data_safety::check_launch(&second, &inside).is_err());
        // The first answer replaces the hold.
        answer_as(&second, &key, block_payload(&project, "clean", false));
        assert!(second.data_safety_blocks().is_empty());
        assert_eq!(kept(&config, &key), (true, true));

        // And next time the saved clean verdict allows from the launch on.
        let third = started(&config);
        launch(&third, &project, "stack");
        assert!(third.data_safety_blocks().is_empty());
    }

    #[test]
    fn a_non_stack_source_is_never_remembered_and_re_trust_keeps_the_flag_only_for_a_stack() {
        let (_dir, config, project, inside) = stack_dirs();
        // Not a stack (another id, or a stack id serving no Environment).
        let first = started(&config);
        let key = launch(&first, &project, "tracker");
        manifest_arrives(&first, &key, manifest_of("tracker", "Tracker", true));
        trust_shown(&first, &key);
        let plain = launch(&first, &project, "plain");
        manifest_arrives(&first, &plain, manifest_of("stack:plain", "Plain", false));
        trust_shown(&first, &plain);
        assert!(first.data_safety_blocks().is_empty());
        assert_eq!(kept(&config, &key), (false, false));
        assert_eq!(kept(&config, &plain), (false, false));
        let restarted = started(&config);
        launch(&restarted, &project, "tracker");
        launch(&restarted, &project, "plain");
        assert!(
            restarted.data_safety_blocks().is_empty(),
            "a non-stack source never blocks"
        );

        // A stack, remembered.
        let second = started(&config);
        let key = launch(&second, &project, "stack");
        manifest_arrives(&second, &key, manifest_of("stack:app", "Stack app", true));
        trust_shown(&second, &key);
        assert_eq!(kept(&config, &key), (false, true));
        // Re-trusted under a changed manifest that is still a stack: kept,
        // under its new name.
        let third = started(&config);
        let key = launch(&third, &project, "stack");
        manifest_arrives(&third, &key, manifest_of("stack:app2", "Stack two", true));
        assert!(
            !third.lock().sources[&key].trusted,
            "a changed hash asks again"
        );
        trust_shown(&third, &key);
        let remembered = VerdictStore::beside(&config.join(store::FILE_NAME))
            .stack(&source_id_of(&key))
            .cloned()
            .unwrap();
        assert_eq!(
            (
                remembered.manifest_id.as_str(),
                remembered.source_label.as_str()
            ),
            ("stack:app2", "Stack two")
        );

        // A changed manifest that is NOT a stack: until it is trusted the
        // flag holds (and so does the block); trusting it clears both.
        let fourth = started(&config);
        let key = launch(&fourth, &project, "stack");
        manifest_arrives(&fourth, &key, manifest_of("tracker", "Tracker", true));
        assert_eq!(
            fourth.lock().sources[&key].status,
            SourceStatus::NeedsReview
        );
        let blocks = fourth.data_safety_blocks();
        assert_eq!(blocks.len(), 1);
        assert_eq!(
            (blocks[0].instance.as_str(), blocks[0].source_label.as_str()),
            ("app2", "Stack two"),
            "named after what was trusted"
        );
        assert!(crate::data_safety::check_launch(&fourth, &inside).is_err());
        trust_shown(&fourth, &key);
        assert!(fourth.data_safety_blocks().is_empty());
        assert_eq!(kept(&config, &key), (false, false));
        let fifth = started(&config);
        launch(&fifth, &project, "stack");
        assert!(fifth.data_safety_blocks().is_empty());
    }
}
