//! Running a source's actions: the glue between the source registry (which
//! holds the cached payloads, the trust records and the HTTP client) and
//! the rest of the app (agent spawns, terminals, the opener, the journal).
//!
//! Every action is looked up in the source's CACHED payload by view, key
//! and id — the webview names an action, never its contents — and judged
//! again here before anything runs, so a view cannot be talked into an
//! action the payload does not carry. An item action (one a source offers
//! on ANOTHER source's row) is looked up the same way in the declaring
//! source's trusted manifest, against the owner's cached row. Then, per
//! kind:
//!
//! - `link`: the trusted-host check (https, or plain http to this
//!   machine), then the OS opener;
//! - `panel`: fetched from the payload-named path and returned; the page
//!   is kept, so its own actions are looked up and run the same way under
//!   the view name `panel:<path>`;
//! - `endpoint`: `POST {actions}` with `confirmed: true` only when the
//!   dialog was accepted, and, for a row fed by more than one source, the
//!   owning source and its row in `target` (see `sources::CallTarget`); `200` is done, `202 {jobId}` is polled from a
//!   thread, `202 {jobId, run}` spawns the run's argv in a pane of the
//!   source's project and reports the exit back, `503` is "may or may not
//!   have happened" and is never retried;
//! - `agent`: the tool resolved by name, the folder checked against the cwd
//!   roots, the prompt shown first (per the source's prompt policy), then
//!   the ordinary agent spawn with `working_dir`, `env` and the source as
//!   the journal actor;
//! - `terminal`: the argv (or the default shell) in the checked folder.
//!
//! Launches and endpoint calls are journaled under actor `source:<id>`
//! (the manifest's `source.id`): on the launched row for agents and
//! terminals, on the source's own process row for endpoint calls.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use indexmap::IndexMap;
use serde::Serialize;
use serde_json::{json, Value};
use tauri::{AppHandle, State};
use term_core::actor::epoch_ms;
use term_core::pty::PtySpec;

use crate::agent_tools::AgentToolsState;
use crate::agents::{spawn_agent_with, SpawnAgentRequest, SpawnContext};
use crate::projects::Projects;
use crate::records::{Records, Scope};
use crate::registry::{
    actor_config, emit_created, AppCreatedBroadcast, CreatedEvent, ExitHandler, NullSink, RecordBinding, Registry, TermId,
};
use project_model::records::{new_record_id, JobRecord as ProjectJobRecord, RecordKind, Run, TailKind};
use crate::roots::{check_cwd, Root};
use crate::settings::SettingsState;
use crate::sources::actions::{action_verdict, resolve_tool, validate_env, NoteRule, SourceAction, VerdictContext};
use crate::sources::{iso_utc, main_window_only, ActionDto, CallTarget, JobRecord, PromptPolicy, SourceInfo, Sources, SourcesListing};

/// A run's pane dump in the exit report is capped here, with a marker.
pub const OUTPUT_CAP_BYTES: usize = 4 * 1024 * 1024;
pub const OUTPUT_TRUNCATED_MARKER: &str = "\n[output truncated: the pane dump exceeded 4 MiB]";
/// A polled job is asked about this often, and given up on after this long
/// without an answer (its record then reads `lost`).
pub const JOB_POLL_EVERY: Duration = Duration::from_secs(1);
pub const JOB_POLL_MAX_SILENCE: Duration = Duration::from_secs(120);
/// What a 503 means to the user. Never auto-retried.
pub const UNCERTAIN_MESSAGE: &str = "The action may or may not have happened; check the source before retrying.";

/// Everything the runner reaches for.
#[derive(Clone)]
pub struct ActionDeps {
    pub registry: Registry,
    pub tools: AgentToolsState,
    pub settings: SettingsState,
    pub projects: Projects,
    pub records: Records,
    pub sources: Sources,
    /// The app, for broadcasts. `None` in tests: rows are then created but
    /// not announced.
    pub app: Option<AppHandle>,
}

/// What `source_run_action` answers.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ActionOutcome {
    /// `200`: the source did it.
    Done { message: Option<String> },
    /// `202 {jobId}`: started; the record is updated as it is polled.
    Job { job_id: String },
    /// `202 {jobId, run}`: started, and the job's CLI runs in this pane.
    Run { job_id: String, term_id: TermId },
    /// `503`: nobody knows.
    Uncertain { message: String },
    /// A link opened.
    Opened { url: String },
    /// A panel's page.
    Panel { payload: Value },
    /// An agent or terminal row was launched (`what` = agent | terminal).
    Launched { term_id: TermId, what: &'static str },
}

/// The journal actor for a source.
pub fn actor_for(info: &SourceInfo) -> String {
    format!("source:{}", info.manifest_id)
}

/// Cap a pane dump for the exit report. Cuts on a character boundary and
/// appends the marker when anything was dropped.
pub fn cap_output(lines: &[String], cap: usize) -> String {
    let mut text = lines.join("\n");
    if text.len() <= cap {
        return text;
    }
    let mut cut = cap;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text.truncate(cut);
    text.push_str(OUTPUT_TRUNCATED_MARKER);
    text
}

/// The roots an action of `info`'s source may use: its project's (every
/// open project's for an attached source) plus trusted source roots.
fn roots_for(deps: &ActionDeps, info: &SourceInfo) -> Vec<Root> {
    deps.projects.cwd_roots(info.project_id)
}

/// Run `action_id` of `key` in `view` of the source `source_id`.
pub fn run_action(
    deps: &ActionDeps,
    source_id: &str,
    view: &str,
    key: Option<&str>,
    action_id: &str,
    confirmed: bool,
    note: Option<&str>,
) -> Result<ActionOutcome, String> {
    let (action, info, target) = deps.sources.action_for_call(source_id, view, key, action_id)?;
    run_found(deps, source_id, view, key, action, info, target, confirmed, note)
}

/// Run the item action `action_id` that `source_id` offers on the `view`
/// row `key` of the source `owner`. The call goes to `source_id` and its
/// target names the owner and carries the owner's row.
#[allow(clippy::too_many_arguments)]
pub fn run_item_action(
    deps: &ActionDeps,
    source_id: &str,
    view: &str,
    owner: &str,
    key: &str,
    action_id: &str,
    confirmed: bool,
    note: Option<&str>,
) -> Result<ActionOutcome, String> {
    let (action, info, target) = deps.sources.item_action(source_id, view, owner, key, action_id)?;
    run_found(deps, source_id, view, Some(key), action, info, target, confirmed, note)
}

#[allow(clippy::too_many_arguments)]
fn run_found(
    deps: &ActionDeps,
    source_id: &str,
    view: &str,
    key: Option<&str>,
    action: SourceAction,
    info: SourceInfo,
    target: CallTarget,
    confirmed: bool,
    note: Option<&str>,
) -> Result<ActionOutcome, String> {
    let tools = deps.tools.list();
    let roots = roots_for(deps, &info);
    let verdict = action_verdict(&action, &VerdictContext { tools: &tools, roots: &roots, link_hosts: &info.link_hosts });
    if !verdict.enabled {
        return Err(verdict.reason.unwrap_or_else(|| "the action is not available".into()));
    }
    let actor = actor_for(&info);
    match action {
        SourceAction::Link { url, .. } => {
            deps.sources.link_check(source_id, &url)?;
            tauri_plugin_opener::open_url(&url, None::<&str>).map_err(|e| format!("could not open {url}: {e}"))?;
            Ok(ActionOutcome::Opened { url })
        }
        SourceAction::Panel { endpoint, .. } => Ok(ActionOutcome::Panel { payload: deps.sources.panel(source_id, &endpoint)? }),
        SourceAction::Endpoint { id, label: action_label, mutates, confirm, params, note: rule } => {
            if mutates && confirm.is_none() {
                return Err("a mutating action without a confirm text is not offered".into());
            }
            if mutates && !confirmed {
                return Err(format!("`{id}` changes something and must be confirmed first"));
            }
            let note = note.map(str::trim).filter(|n| !n.is_empty());
            if rule == Some(NoteRule::Required) && note.is_none() {
                return Err(format!("`{id}` needs a note"));
            }
            let mut body = json!({ "id": id, "target": target.body(view, key) });
            if let Some(params) = params {
                body["params"] = params;
            }
            if let Some(note) = note {
                body["note"] = Value::String(note.to_owned());
            }
            // Set by the app, only after the dialog; a non-mutating action
            // needs none and sends none.
            if mutates && confirmed {
                body["confirmed"] = Value::Bool(true);
            }
            let reply = deps.sources.call_endpoint(source_id, &body)?;
            journal_endpoint(deps, &info, &actor, &format!("action `{id}` on {view}{} → {}", key.map(|k| format!("/{k}")).unwrap_or_default(), reply.status));
            match reply.status {
                200 => Ok(ActionOutcome::Done { message: reply.body["message"].as_str().map(str::to_owned) }),
                202 => {
                    let job_id = reply.body["jobId"]
                        .as_str()
                        .filter(|j| !j.trim().is_empty())
                        .ok_or("the source answered 202 without a jobId")?
                        .to_owned();
                    let run = reply.body.get("run").filter(|r| r.is_object()).cloned();
                    let record = JobRecord {
                        job_id: job_id.clone(),
                        action_id: id.clone(),
                        view: view.to_owned(),
                        key: key.map(str::to_owned),
                        started_utc: iso_utc(epoch_ms()),
                        state: "running".into(),
                        run: run.is_some(),
                        term_id: None,
                        exit_code: None,
                        ended_utc: None,
                        log: None,
                    };
                    deps.sources.job_started(source_id, record);
                    match run {
                        Some(run) => {
                            let origin = JobOrigin { action_id: &id, action_label: &action_label, view, key };
                            let term_id = spawn_job_run(deps, source_id, &info, &job_id, &origin, &run, &actor)?;
                            deps.sources.job_update(source_id, &job_id, |j| j.term_id = Some(term_id));
                            Ok(ActionOutcome::Run { job_id, term_id })
                        }
                        None => {
                            poll_job(deps.sources.clone(), source_id.to_owned(), job_id.clone());
                            Ok(ActionOutcome::Job { job_id })
                        }
                    }
                }
                503 => {
                    let detail = reply.body["message"].as_str().filter(|m| !m.is_empty());
                    Ok(ActionOutcome::Uncertain {
                        message: match detail {
                            Some(d) => format!("{UNCERTAIN_MESSAGE} ({d})"),
                            None => UNCERTAIN_MESSAGE.to_owned(),
                        },
                    })
                }
                _ => Err(reply.as_error().to_string()),
            }
        }
        SourceAction::Agent { id, label, cwd, prompt, tool, title, env } => {
            if !confirmed && deps.sources.prompt_needs_dialog(source_id, &id)? {
                return Err(format!("the prompt of `{label}` must be shown and accepted before it runs"));
            }
            let tool = resolve_tool(&tools, tool.as_deref())?;
            let cwd = check_cwd(&roots, &cwd).into_result()?;
            validate_env(&env)?;
            let req = SpawnAgentRequest {
                agent_tool_id: tool.id,
                project_id: info.project_id,
                name: Some(title.unwrap_or(label)),
                prompt: Some(prompt),
                working_dir: Some(cwd.display().to_string()),
                env: env.into_iter().collect::<IndexMap<String, String>>(),
                journal_actor: Some(actor.clone()),
                ..SpawnAgentRequest::default()
            };
            let ctx = SpawnContext {
                registry: &deps.registry,
                tools: &deps.tools,
                settings: &deps.settings,
                projects: &deps.projects,
                records: &deps.records,
                actor: None,
            };
            let resp = spawn_agent_with(ctx, req, Arc::new(NullSink), deps.app.clone())?;
            if confirmed {
                deps.sources.mark_prompt_shown(source_id, &id)?;
            }
            Ok(ActionOutcome::Launched { term_id: resp.process_id, what: "agent" })
        }
        SourceAction::Terminal { label, cwd, command, env, .. } => {
            let cwd = check_cwd(&roots, &cwd).into_result()?;
            validate_env(&env)?;
            let term_id = spawn_terminal_row(deps, &info, &label, &command, &cwd, &env, &actor, None, None)?;
            Ok(ActionOutcome::Launched { term_id, what: "terminal" })
        }
    }
}

/// An endpoint call's journal line lands on the source's own process row
/// (a launched source); an attached source has no row, so the log gets it.
fn journal_endpoint(deps: &ActionDeps, info: &SourceInfo, actor: &str, text: &str) {
    match info.term_id {
        Some(term_id) => deps.registry.record_input_as(term_id, 0, text.to_owned(), false, Some(actor), Some("endpoint")),
        None => log::info!("[sources] {actor}: {text}"),
    }
}

/// The durable record a terminal row runs for: a job's pane is a `job`
/// record, bound before the spawn so even an instant exit lands on it.
struct RowRecord {
    binding: RecordBinding,
    kind: RecordKind,
}

/// A terminal row in the source's project running `argv` (empty = the
/// default shell) in `cwd`, journaled under the source. `on_exit` is the
/// job runner's exit hook and `record` its job record when the row is a
/// job's run.
#[allow(clippy::too_many_arguments)]
fn spawn_terminal_row(
    deps: &ActionDeps,
    info: &SourceInfo,
    name: &str,
    argv: &[String],
    cwd: &std::path::Path,
    env: &BTreeMap<String, String>,
    actor: &str,
    on_exit: Option<ExitHandler>,
    record: Option<RowRecord>,
) -> Result<TermId, String> {
    let settings = deps.settings.snapshot();
    let spec = if argv.is_empty() {
        crate::commands::to_pty_spec(
            crate::commands::PtySpecDto {
                command: String::new(),
                args: None,
                cwd: Some(cwd.display().to_string()),
                env: Some(env.iter().map(|(k, v)| (k.clone(), v.clone())).collect()),
                cols: None,
                rows: None,
                profile: None,
            },
            &settings,
        )
    } else {
        PtySpec {
            command: argv[0].clone(),
            args: argv[1..].to_vec(),
            cwd: Some(cwd.to_path_buf()),
            env: env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            cols: 80,
            rows: 24,
        }
    };
    let cfg = actor_config(spec.clone(), None, Some(&settings));
    let (binding, record_kind) = match record {
        Some(r) => (Some(r.binding), Some(r.kind)),
        None => (None, None),
    };
    let id = deps
        .registry
        .create_terminal_with_exit(cfg, Arc::new(NullSink), deps.app.clone(), name.to_owned(), on_exit, binding.clone())?;
    if let Some(project_id) = info.project_id {
        deps.registry.set_project_shell(id, project_id);
    }
    let launch = format!(
        "launch {} {} in {}",
        spec.command,
        spec.args.iter().map(|a| project_model::words::quote_word(a)).collect::<Vec<_>>().join(" "),
        crate::roots::display(cwd)
    );
    deps.registry.record_input_as(id, 0, launch, false, Some(actor), Some("launch"));
    if let Some(app) = &deps.app {
        emit_created(
            &AppCreatedBroadcast(app.clone()),
            CreatedEvent {
                term_id: id,
                name: name.to_owned(),
                kind: "terminal",
                project_id: info.project_id,
                agent_tool_id: None,
                parent_process_id: None,
                record_id: binding.map(|b| b.record_id),
                last_run: deps.registry.run_head(id),
                record_kind,
            },
        );
    }
    Ok(id)
}

/// The `run` block of a 202 reply, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunBlock {
    pub argv: Vec<String>,
    pub cwd: String,
    pub env: BTreeMap<String, String>,
    pub title: Option<String>,
}

pub fn parse_run_block(run: &Value) -> Result<RunBlock, String> {
    let argv: Vec<String> = run["argv"]
        .as_array()
        .ok_or("the job's `run.argv` must be a list")?
        .iter()
        .map(|w| w.as_str().map(str::to_owned).ok_or_else(|| "the job's `run.argv` must hold strings".to_owned()))
        .collect::<Result<_, _>>()?;
    if argv.first().map_or(true, |w| w.trim().is_empty()) {
        return Err("the job's `run.argv` names no program".into());
    }
    let cwd = run["cwd"].as_str().map(str::trim).filter(|c| !c.is_empty()).ok_or("the job's `run.cwd` must be a folder")?.to_owned();
    let mut env = BTreeMap::new();
    match run.get("env") {
        None | Some(Value::Null) => {}
        Some(Value::Object(map)) => {
            for (k, v) in map {
                env.insert(k.clone(), v.as_str().ok_or_else(|| format!("the job's env `{k}` must be a string"))?.to_owned());
            }
        }
        Some(_) => return Err("the job's `run.env` must be an object of strings".into()),
    }
    validate_env(&env)?;
    Ok(RunBlock { argv, cwd, env, title: run["title"].as_str().map(str::trim).filter(|t| !t.is_empty()).map(str::to_owned) })
}

/// Which action of which view started a job (what its record says).
struct JobOrigin<'a> {
    action_id: &'a str,
    action_label: &'a str,
    view: &'a str,
    key: Option<&'a str>,
}

/// Spawn a job's `run` in a pane of the source's project; on exit the
/// pane's text (capped) goes back to the source as the job's exit report.
///
/// The pane is a durable `job` record (in the source's project, or the
/// workspace for an attached source): its run — start, end, exit code, the
/// automatic tail, a summary the job posts — outlives the pane, so the
/// dashboard and the record card show it like any other run. The record
/// keeps the source's job id; the source's own job log stays the source's.
/// A record the store refuses only costs the history: the job still runs.
#[allow(clippy::too_many_arguments)]
fn spawn_job_run(
    deps: &ActionDeps,
    source_id: &str,
    info: &SourceInfo,
    job_id: &str,
    origin: &JobOrigin<'_>,
    run: &Value,
    actor: &str,
) -> Result<TermId, String> {
    let run = parse_run_block(run)?;
    let roots = roots_for(deps, info);
    let cwd = check_cwd(&roots, &run.cwd).into_result()?;
    let title = run.title.clone().unwrap_or_else(|| format!("{} job {job_id}", info.label));
    let record = job_record(deps, source_id, info, job_id, origin, &title, &run.argv, &cwd);
    let (sources, registry, source_id, job_id_owned) = (deps.sources.clone(), deps.registry.clone(), source_id.to_owned(), job_id.to_owned());
    let on_exit: ExitHandler = Arc::new(move |term_id, exit_code, _closed| {
        let (sources, registry, source_id, job_id) = (sources.clone(), registry.clone(), source_id.clone(), job_id_owned.clone());
        // Off the pump thread: a POST to the source must not stall the
        // terminal's event stream.
        std::thread::spawn(move || report_job_exit(&sources, &registry, &source_id, &job_id, term_id, exit_code));
    });
    let record_id = record.as_ref().map(|r| r.binding.record_id.clone());
    let run_uuid = record.as_ref().map(|r| r.binding.run_uuid.clone());
    match spawn_terminal_row(deps, info, &title, &run.argv, &cwd, &run.env, actor, Some(on_exit), record) {
        Ok(id) => Ok(id),
        Err(e) => {
            // The run began in the store but no terminal ever ran it.
            if let (Some(record_id), Some(run_uuid)) = (record_id, run_uuid) {
                use crate::registry::RunRecorder;
                deps.records.run_stopped(&record_id, &run_uuid, epoch_ms());
            }
            Err(e)
        }
    }
}

/// Write the job's record and its first (only) run before the pane exists.
#[allow(clippy::too_many_arguments)]
fn job_record(
    deps: &ActionDeps,
    source_id: &str,
    info: &SourceInfo,
    job_id: &str,
    origin: &JobOrigin<'_>,
    title: &str,
    argv: &[String],
    cwd: &std::path::Path,
) -> Option<RowRecord> {
    let scope = info.project_id.map_or(Scope::Workspace, Scope::Project);
    let made = (|| -> Result<RowRecord, String> {
        let record = ProjectJobRecord {
            record_id: new_record_id()?,
            name: title.to_owned(),
            source_id: source_id.to_owned(),
            source_label: info.label.clone(),
            action_id: origin.action_id.to_owned(),
            action_label: origin.action_label.to_owned(),
            view: origin.view.to_owned(),
            key: origin.key.map(str::to_owned),
            job_id: job_id.to_owned(),
            argv: argv.to_vec(),
            cwd: Some(crate::roots::display(cwd)),
            last_run: None,
        };
        let record_id = record.record_id.clone();
        deps.records.add_job_record(scope, record)?;
        let run_uuid = crate::control_http::random_hex(16).map_err(|e| format!("cannot generate a run uuid: {e}"))?;
        deps.records.begin_run(scope, RecordKind::Job, &record_id, &Run::begin(run_uuid.clone(), epoch_ms(), TailKind::Text))?;
        Ok(RowRecord { binding: RecordBinding { record_id, run_uuid }, kind: RecordKind::Job })
    })();
    match made {
        Ok(r) => Some(r),
        Err(e) => {
            log::warn!("[sources] job {job_id} of {}: its pane leaves no record: {e}", info.label);
            None
        }
    }
}

/// `POST {jobs}/{id}` with `{exitCode, endedUtc, output}` once the run's
/// pane exited, and the job record's end.
fn report_job_exit(sources: &Sources, registry: &Registry, source_id: &str, job_id: &str, term_id: TermId, exit_code: Option<i32>) {
    let output = registry.handle(term_id).map(|h| cap_output(&h.dump_text(10_000), OUTPUT_CAP_BYTES)).unwrap_or_default();
    let code = exit_code.unwrap_or(-1);
    let ended = iso_utc(epoch_ms());
    let report = json!({ "exitCode": code, "endedUtc": ended, "output": output });
    let state = if code == 0 { "done" } else { "failed" };
    let note = match sources.job_report(source_id, job_id, &report) {
        Ok(reply) if (200..300).contains(&reply.status) => None,
        Ok(reply) => Some(format!("the exit report was refused: {}", reply.as_error())),
        Err(e) => Some(format!("the exit report did not reach the source: {e}")),
    };
    if let Some(n) = &note {
        log::warn!("[sources] job {job_id}: {n}");
    }
    sources.job_update(source_id, job_id, |j| {
        j.state = state.into();
        j.exit_code = Some(code);
        j.ended_utc = Some(ended.clone());
        j.log = note;
    });
}

/// Poll `GET jobs/{id}` until `done` / `failed`, updating the record; a
/// source silent for [`JOB_POLL_MAX_SILENCE`] leaves the job `lost`.
fn poll_job(sources: Sources, source_id: String, job_id: String) {
    let thread_name = format!("source job {job_id}");
    std::thread::Builder::new()
        .name(thread_name.clone())
        .spawn(move || {
            let mut last_answer = Instant::now();
            loop {
                if sources.is_shutting_down() {
                    return;
                }
                std::thread::sleep(JOB_POLL_EVERY);
                match sources.job_get(&source_id, &job_id) {
                    Ok(status) => {
                        last_answer = Instant::now();
                        let state = status["state"].as_str().unwrap_or("").to_owned();
                        let log = status["log"].as_str().map(str::to_owned);
                        if state == "done" || state == "failed" {
                            sources.job_update(&source_id, &job_id, |j| {
                                j.state = state.clone();
                                j.log = log.clone();
                                j.ended_utc = Some(iso_utc(epoch_ms()));
                            });
                            return;
                        }
                    }
                    Err(e) => {
                        if last_answer.elapsed() >= JOB_POLL_MAX_SILENCE {
                            sources.job_update(&source_id, &job_id, |j| {
                                j.state = "lost".into();
                                j.log = Some(format!("the source stopped answering about this job: {e}"));
                                j.ended_utc = Some(iso_utc(epoch_ms()));
                            });
                            return;
                        }
                    }
                }
            }
        })
        .map(|_| ())
        .unwrap_or_else(|e| log::error!("[sources] cannot start the {thread_name} poller: {e}"));
}

fn deps_of(
    registry: &State<'_, Registry>,
    tools: &State<'_, AgentToolsState>,
    settings: &State<'_, SettingsState>,
    projects: &State<'_, Projects>,
    records: &State<'_, Records>,
    sources: &State<'_, Sources>,
    app: AppHandle,
) -> ActionDeps {
    ActionDeps {
        registry: registry.inner().clone(),
        tools: tools.inner().clone(),
        settings: settings.inner().clone(),
        projects: projects.inner().clone(),
        records: records.inner().clone(),
        sources: sources.inner().clone(),
        app: Some(app),
    }
}

// ---- Tauri commands (main window only) ---------------------------------------------

/// The judged actions under `key` in a source's cached `view`, or on a
/// fetched panel page (`panel:<path>`). With `owner`: the item actions the
/// source offers on the row `key` of that other source.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub fn source_actions(
    window: tauri::Window,
    sources: State<'_, Sources>,
    tools: State<'_, AgentToolsState>,
    projects: State<'_, Projects>,
    source_id: String,
    view: String,
    key: Option<String>,
    owner: Option<String>,
) -> Result<Vec<ActionDto>, String> {
    main_window_only(&window)?;
    let info = sources.info(&source_id)?;
    let roots = projects.cwd_roots(info.project_id);
    match (owner, key) {
        (Some(owner), Some(key)) => sources.item_actions(&source_id, &view, &owner, &key, &tools.list(), &roots),
        (Some(_), None) => Err("an item action needs the row's key".into()),
        (None, key) => sources.actions(&source_id, &view, key.as_deref(), &tools.list(), &roots),
    }
}

/// Run one action. Async: it may call the source, spawn a process and
/// wait on an agent's ready gate.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn source_run_action(
    window: tauri::Window,
    app: AppHandle,
    registry: State<'_, Registry>,
    tools: State<'_, AgentToolsState>,
    settings: State<'_, SettingsState>,
    projects: State<'_, Projects>,
    records: State<'_, Records>,
    sources: State<'_, Sources>,
    source_id: String,
    view: String,
    key: Option<String>,
    action_id: String,
    confirmed: bool,
    note: Option<String>,
    owner: Option<String>,
) -> Result<ActionOutcome, String> {
    main_window_only(&window)?;
    let deps = deps_of(&registry, &tools, &settings, &projects, &records, &sources, app);
    tauri::async_runtime::spawn_blocking(move || match (owner, key) {
        (Some(owner), Some(key)) => run_item_action(&deps, &source_id, &view, &owner, &key, &action_id, confirmed, note.as_deref()),
        (Some(_), None) => Err("an item action needs the row's key".into()),
        (None, key) => run_action(&deps, &source_id, &view, key.as_deref(), &action_id, confirmed, note.as_deref()),
    })
    .await
    .map_err(|e| e.to_string())?
}

/// One job's record (as last polled or reported).
#[tauri::command]
pub fn source_job(window: tauri::Window, sources: State<'_, Sources>, source_id: String, job_id: String) -> Result<JobRecord, String> {
    main_window_only(&window)?;
    sources.job(&source_id, &job_id).ok_or_else(|| format!("source {source_id} has no job {job_id}"))
}

/// A panel's page from a payload-named path. Re-call while it is open.
#[tauri::command]
pub async fn source_panel(window: tauri::Window, sources: State<'_, Sources>, source_id: String, path: String) -> Result<Value, String> {
    main_window_only(&window)?;
    let sources = sources.inner().clone();
    tauri::async_runtime::spawn_blocking(move || sources.panel(&source_id, &path))
        .await
        .map_err(|e| e.to_string())?
}

/// Open a link a trusted source handed over, after the host check (https,
/// or plain http to this machine). Async: a plain-http name is looked up
/// first, which must not hold the UI thread.
#[tauri::command]
pub async fn open_source_link(window: tauri::Window, sources: State<'_, Sources>, source_id: String, url: String) -> Result<(), String> {
    main_window_only(&window)?;
    let sources = sources.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        sources.link_check(&source_id, &url)?;
        tauri_plugin_opener::open_url(&url, None::<&str>).map_err(|e| format!("could not open {url}: {e}"))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// `first_time` | `always`: when a source's agent prompts are shown.
#[tauri::command]
pub fn set_source_prompt_policy(
    window: tauri::Window,
    sources: State<'_, Sources>,
    source_id: String,
    policy: String,
) -> Result<SourcesListing, String> {
    main_window_only(&window)?;
    let policy = PromptPolicy::parse(&policy).ok_or_else(|| format!("`{policy}` is not a prompt policy (first_time | always)"))?;
    sources.set_prompt_policy(&source_id, policy)?;
    Ok(sources.listing())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_is_capped_on_a_char_boundary_with_a_marker() {
        let lines = vec!["abc".to_owned(), "déf".to_owned()];
        assert_eq!(cap_output(&lines, 100), "abc\ndéf");
        // The cap falls inside the two-byte `é`: the cut backs off to `d`.
        let capped = cap_output(&lines, 5);
        assert_eq!(capped, format!("abc\nd{OUTPUT_TRUNCATED_MARKER}"));
        assert_eq!(cap_output(&[], 10), "");
    }

    #[test]
    fn run_blocks_are_checked_field_by_field() {
        let ok = parse_run_block(&json!({"argv": ["node", "-e", "1"], "cwd": "/srv/x", "env": {"CHAPPA_STACK_SLOT": "1"}, "title": "Seed"})).unwrap();
        assert_eq!(ok.argv, ["node", "-e", "1"]);
        assert_eq!(ok.title.as_deref(), Some("Seed"));
        assert_eq!(ok.env["CHAPPA_STACK_SLOT"], "1");
        assert!(parse_run_block(&json!({"argv": [], "cwd": "/x"})).unwrap_err().contains("no program"));
        assert!(parse_run_block(&json!({"argv": "node x", "cwd": "/x"})).unwrap_err().contains("list"));
        assert!(parse_run_block(&json!({"argv": ["node"]})).unwrap_err().contains("cwd"));
        assert!(parse_run_block(&json!({"argv": ["node"], "cwd": "/x", "env": {"bad name": "1"}})).unwrap_err().contains("env name"));
    }

    #[test]
    fn the_actor_names_the_manifest_id() {
        let info = SourceInfo {
            id: "0123".into(),
            manifest_id: "fixture".into(),
            label: "Fixture".into(),
            key: "k".into(),
            project_id: Some(1),
            project_path: Some(std::path::PathBuf::from("/p")),
            term_id: None,
            link_hosts: vec![],
        };
        assert_eq!(actor_for(&info), "source:fixture");
    }
}
