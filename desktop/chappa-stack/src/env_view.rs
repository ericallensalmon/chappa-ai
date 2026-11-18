//! The Environment payload: the runner's status turned into the view's
//! shapes.
//!
//! | field | from |
//! |---|---|
//! | `groups` | the definition's groups, in order, labelled, then the groups script components report (their `groups` order); a group of one script's rows carries that script's verbs |
//! | `actions` | the stack-wide verbs (bring up, start, stop, down, rebuild, clean up), and the verbs of a script component whose rows span several groups |
//! | `components[].state` | the nine states (desired state from the intent record vs what the engine or the process table reports) |
//! | `uptime` / `restarts` / `exitCode` | `inspect` for containers, the process table for processes |
//! | `source` | provenance: mode, ref (the image tag for prebuilt), commit, plus `dirty`, `buildId`, `image`, `stale` |
//! | `probe` | the component's probe rule, run while it runs |
//! | `links` | its `http(s)` outputs; the `url` output is the browser one |
//! | `hint` | why a red or amber state is what it is |
//! | `badges` | staleness (`newer prebuilt`, `behind 2` …), "no data-safety check" on an unscanned persistent component, the data-safety state of a checked one, the mode |
//! | `actions` | the component's verbs, its data-safety check, its `logs` terminal, its links |
//! | `alerts` | red components with their cause (`kind: state`: the view lists them in one short banner), engines that cannot be reached or run the wrong OS mode, relay failures, the hostname check, data that is not clean (`bad`) or not checked (`warn` under `unscanned: warn`) |
//! | `facts` | the stack (instance, slot, port block, bound item, channel, who reports a script's rows; `kind: meta`: it describes the runner, not the stack's health), engines and versions (rows toned `ok` / `bad`), the memory budget, relay listeners, which peers each gateway serves and how many it refused, the relay's Windows Firewall state (added by the server, with the remove-rule action), data safety (state, policy, last scan, marker, rules hash, acknowledgement, each component, with the check and acknowledge actions; its `status` is the one badge the header shows) |
//! | `freshness.notices` | how many reported links were left out, once for the whole view |
//! | `banners` | definition warnings or errors; the hosts-file fix; the relay's Windows Firewall findings (added by the server, see [`crate::relay_firewall`]); agents blocked by data safety (with the acknowledge action) |
//! | `dataSafety` | the instance's data-safety block the app enforces on (see [`crate::data_safety`]) |
//! | `secretScan` | the instance's secret-scan block, added by the server with its alerts, banner and facts section (see [`crate::secret_scan`]) |
//! | `job` | the instance's changing jobs, added by the server ([`decorate_jobs`]): `active` (pending or running: label, action, target, actor, times, `cancelling`, the log) and `last` (the newest that ended: its status `done` / `failed` / `cancelled` / `interrupted`, exit code, error line, the log); with an active job the `cancel-job` and `job-log` actions, a `running: <verb>` badge on the row it acts on and, for script stacks, a notice that status waits; a last job that did not end `done` is an alert (`bad`, `warn` when cancelled) with its `job-log` action |
//!
//! A script component shows as one row, or, when its status script reports
//! several components (a wrapped whole stack), as one row per reported
//! component (`<component>/<name>`) in the group it names (else the script
//! component's group, else one named after it). Its verbs are drawn once:
//! on the header of the one group its rows fill, else with the stack's
//! actions (see [`crate::actions::script_action_id`]); only a verb whose
//! argv names `{row}` is on each row. The script's facts become a section
//! of their own, its `warn` / `bad` alerts Environment alerts, its `info`
//! alerts rows of that section. Reported links pass the same rule as every
//! other link: only the stack's own hostname (loopback names when it has
//! none).
//!
//! `summary` is left out: the view counts the components itself, and a
//! summary is only worth sending when it knows more than the rows do.

use indexmap::IndexMap;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use project_model::link_hosts as link_host_rules;
use stack_core::checks::{format_mb, EngineFacts};
use stack_core::compose::{parse_utc, uptime_text};
use stack_core::definition::{ComponentKind, Mode, Severity};
use stack_core::driver::{ComponentState, ResolvedInstance};
use stack_core::hosts::HostsCheck;
use stack_core::placeholders;
use stack_core::probe::ProbeOutcome;
use stack_core::runner::{ComponentReport, Observation};
use stack_core::script::{self as script, AlertTone, ScriptVerb, ScriptView, ViewFrom};

use crate::actions::{logs_action, script_action_id, Offered};
use crate::data_safety::{self, InstanceSafety, SafetyState};
use crate::procs::ProcessTable;
use crate::project::Loaded;
use crate::relay::RelayState;

/// Everything the payload is built from, gathered by the server.
pub struct EnvInputs<'a> {
    pub loaded: &'a Loaded,
    pub inst: &'a ResolvedInstance,
    pub obs: &'a Observation,
    pub reports: &'a [ComponentReport],
    pub offered: &'a [Offered],
    pub channel: &'a str,
    /// Per container engine of the instance: facts once known.
    pub facts: IndexMap<String, Option<EngineFacts>>,
    pub hosts: Option<HostsCheck>,
    /// `None` when the stack configures no relay.
    pub relay: Option<RelayState>,
    pub processes: &'a ProcessTable,
    pub exe: &'a Path,
    pub safety: &'a InstanceSafety,
    pub now_utc: &'a str,
}

/// What a state says about a component, for alerts: "api is unhealthy".
fn state_phrase(state: ComponentState) -> &'static str {
    match state {
        ComponentState::Up => "is up",
        ComponentState::Starting => "is starting",
        ComponentState::Unhealthy => "is unhealthy",
        ComponentState::Crashloop => "is in a crash loop",
        ComponentState::StoppedProblem => "is deployed but not running",
        ComponentState::StoppedIdle => "is stopped",
        ComponentState::OneshotOk => "ran ok",
        ComponentState::OneshotFailed => "failed",
        ComponentState::NotDeployed => "is not deployed",
    }
}

fn hint_for(r: &ComponentReport) -> Option<String> {
    let s = &r.status;
    let probe_reason = r.probe.as_ref().and_then(|p| match &p.outcome {
        ProbeOutcome::Down { reason, .. } => Some(format!("{}: {reason}", p.url)),
        ProbeOutcome::Unknown { reason } => Some(format!("{} not probed: {reason}", p.url)),
        ProbeOutcome::Up { .. } => None,
    });
    let hint = match s.state {
        ComponentState::Crashloop => Some(match s.restarts {
            Some(n) if n > 0 => format!("restarted {n} times; its logs say why"),
            _ => "restarting; its logs say why".to_string(),
        }),
        ComponentState::StoppedProblem => Some(match s.exit_code {
            Some(c) => format!(
                "wanted running, but it exited with code {c}; start it, or stop it through the stack"
            ),
            None => "wanted running, but it is not; start it, or stop it through the stack"
                .to_string(),
        }),
        ComponentState::Unhealthy => {
            Some(probe_reason.unwrap_or_else(|| "its healthcheck is failing".to_string()))
        }
        ComponentState::Starting => Some("waiting for its healthcheck".to_string()),
        ComponentState::OneshotFailed => s.exit_code.map(|c| format!("exited with code {c}")),
        _ => probe_reason,
    };
    match (hint, r.notes.is_empty()) {
        (Some(h), true) => Some(h),
        (Some(h), false) => Some(format!("{h} ({})", r.notes.join("; "))),
        (None, false) => Some(r.notes.join("; ")),
        (None, true) => None,
    }
}

fn source_cell(loaded: &Loaded, r: &ComponentReport) -> Value {
    let Some(p) = &r.status.provenance else {
        return Value::Null;
    };
    let git_ref = match p.mode {
        Mode::Prebuilt => {
            let repo = loaded
                .stack
                .components
                .get(&r.status.id)
                .and_then(|c| c.mode(Mode::Prebuilt))
                .and_then(|m| m.image.clone());
            match (repo, &p.build_id) {
                (Some(repo), Some(tag)) => Some(format!("{repo}:{tag}")),
                (Some(repo), None) => Some(repo),
                (None, tag) => tag.clone(),
            }
        }
        _ => p.git_ref.clone(),
    };
    let mut cell = json!({
        "mode": p.mode.as_str(),
        "ref": git_ref,
        "commit": p.commit,
    });
    if let Some(d) = &p.dirty {
        cell["dirty"] = d.clone().into();
    }
    if let Some(b) = &p.build_id {
        cell["buildId"] = b.clone().into();
    }
    if let Some(i) = &p.image {
        cell["image"] = i.clone().into();
    }
    if !r.status.stale.is_empty() {
        cell["stale"] = r
            .status
            .stale
            .iter()
            .map(|s| Value::from(s.to_string()))
            .collect();
    }
    cell
}

fn is_http(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

/// `host[:port]` of an http(s) URL.
fn authority(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let auth = rest.split(['/', '?', '#']).next()?;
    (!auth.is_empty() && !auth.contains('@')).then(|| auth.to_ascii_lowercase())
}

fn host_of(authority: &str) -> &str {
    match authority.rsplit_once(':') {
        Some((h, p)) if p.bytes().all(|b| b.is_ascii_digit()) => h,
        _ => authority,
    }
}

/// The host names the stack's links may point at: its hostname, else the
/// loopback names.
fn own_hosts(loaded: &Loaded) -> Vec<&str> {
    match &loaded.stack.hostname {
        Some(h) => vec![h.as_str()],
        None => vec!["localhost", "127.0.0.1"],
    }
}

/// Whether `url` is an http(s) link to one of the stack's own hosts.
fn is_own_link(loaded: &Loaded, url: &str) -> bool {
    is_http(url)
        && authority(url).is_some_and(|a| {
            own_hosts(loaded)
                .iter()
                .any(|h| h.eq_ignore_ascii_case(host_of(&a)))
        })
}

/// The hosts the stack's links point at: its hostname (or loopback when it
/// has none). Nothing else is declared, so the trust dialog lists only the
/// stack's own names.
///
/// Each own host is declared as `<host>:*` (any port), because a status
/// script reports services on ports the definition never names, and a
/// per-port list would also ask for trust again whenever an output moves.
/// A host the app would not accept a port wildcard on (a single-label
/// name) is declared per port instead, one entry per output in use.
pub fn link_hosts(loaded: &Loaded, inst: &ResolvedInstance) -> Vec<String> {
    let ctx = inst.host_context();
    let mut own = own_hosts(loaded);
    let mut out: BTreeSet<String> = BTreeSet::new();
    own.retain(|h| {
        let any = link_host_rules::check_any_port_host(h).is_ok();
        if any {
            out.insert(format!(
                "{}{}",
                h.to_ascii_lowercase(),
                link_host_rules::ANY_PORT
            ));
        }
        !any
    });
    for c in inst.components.values() {
        for template in c.outputs.values() {
            let Ok(url) = placeholders::resolve(template, &ctx) else {
                continue;
            };
            if !is_http(&url) {
                continue;
            }
            if let Some(a) = authority(&url) {
                if own.iter().any(|h| h.eq_ignore_ascii_case(host_of(&a))) {
                    out.insert(a);
                }
            }
        }
    }
    out.into_iter().collect()
}

fn links_of(r: &ComponentReport) -> Vec<Value> {
    let mut links: Vec<Value> = Vec::new();
    for (key, url) in &r.links {
        if !is_http(url) {
            continue;
        }
        let browser = key == "url";
        let label = if browser {
            "Open".to_string()
        } else {
            key.clone()
        };
        let mut l = json!({ "label": label, "url": url });
        if browser {
            l["browser"] = true.into();
        }
        links.push(l);
    }
    links
}

fn badge(text: &str, tone: &str, title: Option<String>) -> Value {
    let mut b = json!({ "text": text, "tone": tone });
    if let Some(t) = title {
        b["title"] = t.into();
    }
    b
}

fn process_uptime(processes: &ProcessTable, component: &str, now_utc: &str) -> Option<String> {
    let e = processes.processes.get(component)?;
    uptime_text(&e.started_utc, parse_utc(now_utc)?)
}

/// The rows of one report: one, or a script component's reported ones,
/// and how many reported links were left out.
fn component_rows(inp: &EnvInputs<'_>, r: &ComponentReport) -> (Vec<Value>, usize) {
    match &r.script {
        Some(view) => script_rows(inp, r, view),
        None => (vec![component(inp, r)], 0),
    }
}

/// The label a component is shown with: the definition's, else its id.
fn label_of(inp: &EnvInputs<'_>, id: &str) -> String {
    inp.loaded
        .stack
        .components
        .get(id)
        .and_then(|d| d.label.clone())
        .unwrap_or_else(|| id.to_string())
}

/// The group a reported row of a several-row script shows in: the one the
/// status names, else the script component's own, else one named after
/// the script component.
fn script_row_group(r: &ComponentReport, row: &stack_core::script::ScriptRow) -> String {
    row_group(&r.group, &r.status.id, row)
}

/// [`script_row_group`] from the script component's definition group and
/// id.
pub fn row_group(def_group: &str, component: &str, row: &stack_core::script::ScriptRow) -> String {
    row.group
        .clone()
        .or_else(|| (!def_group.is_empty()).then(|| def_group.to_string()))
        .unwrap_or_else(|| component.to_string())
}

fn kind_name(kind: ComponentKind) -> &'static str {
    match kind {
        ComponentKind::Oneshot => "oneshot",
        ComponentKind::Service => "service",
    }
}

fn script_rows(inp: &EnvInputs<'_>, r: &ComponentReport, view: &ScriptView) -> (Vec<Value>, usize) {
    let id = &r.status.id;
    let label = label_of(inp, id);
    let fingerprint = inp.obs.state.fingerprint();
    // A one-row script's verbs are its row's; a several-row script's are
    // drawn once elsewhere (see `environment`), only `{row}` verbs here.
    let verbs_of = |row_id: &str| -> Vec<Value> {
        inp.offered
            .iter()
            .filter(|o| {
                o.key.as_deref() == Some(row_id)
                    || (!view.multi && o.key.as_deref() == Some(id.as_str()))
            })
            .map(|o| o.to_json(&fingerprint))
            .collect()
    };
    let now = parse_utc(inp.now_utc);
    let badge_for = match view.from {
        ViewFrom::Status => None,
        ViewFrom::Assumed => Some(badge(
            "no status script",
            "muted",
            Some("The state is what the stack asked for and how the last script run went; nothing checks it.".into()),
        )),
        ViewFrom::Pending => Some(badge(
            "status not read yet",
            "muted",
            Some("Its status script runs in the background; the next look shows its answer.".into()),
        )),
        ViewFrom::Unreadable => Some(badge(
            "status unreadable",
            "bad",
            view.rows.first().and_then(|r| r.detail.clone()),
        )),
    };
    let mut out = Vec::new();
    let mut dropped_total = 0;
    for row in &view.rows {
        let running = matches!(
            row.state,
            ComponentState::Up
                | ComponentState::Starting
                | ComponentState::Unhealthy
                | ComponentState::Crashloop
        );
        // `since` gives a live uptime; a script that only knows the text
        // reports `uptime`. Neither shows while it is not running.
        let uptime = match (running, &row.since, now) {
            (true, Some(s), Some(n)) => uptime_text(s, n),
            _ => None,
        }
        .or_else(|| row.uptime.clone().filter(|_| running));
        let source = row.source.as_ref().map_or(
            Value::Null,
            |s| json!({ "mode": s.mode.as_str(), "ref": s.git_ref, "commit": s.commit }),
        );
        let group = if view.multi {
            script_row_group(r, row)
        } else {
            r.group.clone()
        };
        let mut c = json!({
            "id": row.id,
            "group": group,
            "label": if view.multi { row.name.clone() } else { label.clone() },
            "kind": kind_name(r.kind),
            "state": row.state.as_str(),
            "uptime": uptime,
            "restarts": row.restarts,
            "exitCode": row.exit_code,
            "source": source,
        });
        if let Some(p) = &row.probe {
            c["probe"] = json!({
                "url": p.url,
                "rule": serde_json::to_value(p.rule).unwrap_or(Value::Null),
                "up": p.up(),
                "status": p.status,
            });
        }
        let mut links: Vec<Value> = if view.multi { Vec::new() } else { links_of(r) };
        let mut link_actions: Vec<Value> = Vec::new();
        if !view.multi {
            for (key, url) in &r.links {
                if is_http(url) {
                    link_actions.push(json!({
                        "kind": "link",
                        "id": format!("open-{key}"),
                        "label": if key == "url" { format!("Open {label}") } else { format!("{label}: {key}") },
                        "url": url,
                    }));
                }
            }
        }
        let mut dropped = 0;
        for (i, l) in row.links.iter().enumerate() {
            if !is_own_link(inp.loaded, &l.url) {
                dropped += 1;
                continue;
            }
            links.push(json!({ "label": l.label, "url": l.url }));
            link_actions.push(json!({
                "kind": "link",
                "id": format!("open-reported-{i}"),
                "label": l.label,
                "url": l.url,
            }));
        }
        if !links.is_empty() {
            c["links"] = links.into();
        }
        dropped_total += dropped;
        if let Some(detail) = &row.detail {
            c["hint"] = detail.clone().into();
        }
        let mut badges: Vec<Value> = badge_for.iter().cloned().collect();
        if let Some(secs) = view.slow_secs {
            badges.push(badge(
                "status is slow",
                "warn",
                Some(format!("A script of this stack has been running for {secs} s, longer than a status refresh; this is the last known state.")),
            ));
        }
        if !badges.is_empty() {
            c["badges"] = badges.into();
        }
        let mut actions = verbs_of(&row.id);
        actions.extend(link_actions);
        if !actions.is_empty() {
            c["actions"] = actions.into();
        }
        out.push(c);
    }
    (out, dropped_total)
}

/// A section per script component whose status reports facts or `info`
/// alerts.
fn script_facts(inp: &EnvInputs<'_>) -> Vec<Value> {
    let mut out = Vec::new();
    for r in inp.reports {
        let Some(view) = &r.script else { continue };
        let mut rows: Vec<Value> = view
            .facts
            .iter()
            .map(|f| row(&f.label, f.value.clone()))
            .collect();
        rows.extend(
            view.alerts
                .iter()
                .filter(|a| a.tone == AlertTone::Info)
                .map(|a| row("Note", a.text.clone())),
        );
        if rows.is_empty() {
            continue;
        }
        let label = inp
            .loaded
            .stack
            .components
            .get(&r.status.id)
            .and_then(|d| d.label.clone())
            .unwrap_or_else(|| r.status.id.clone());
        out.push(json!({
            "id": format!("script-{}", r.status.id),
            "title": label,
            "rows": rows,
        }));
    }
    out
}

fn component(inp: &EnvInputs<'_>, r: &ComponentReport) -> Value {
    let id = &r.status.id;
    let def = inp.loaded.stack.components.get(id);
    let label = def
        .and_then(|d| d.label.clone())
        .unwrap_or_else(|| id.clone());
    let running = inp.obs.actual.get(id).is_some_and(|a| a.running);
    let uptime = r.status.uptime.clone().or_else(|| {
        running
            .then(|| process_uptime(inp.processes, id, inp.now_utc))
            .flatten()
    });
    let mut c = json!({
        "id": id,
        "group": r.group,
        "label": label,
        "kind": kind_name(r.kind),
        "state": r.status.state.as_str(),
        "uptime": uptime,
        "restarts": r.status.restarts,
        "exitCode": r.status.exit_code,
        "source": source_cell(inp.loaded, r),
    });
    if let Some(p) = &r.probe {
        if let Some(up) = p.up() {
            c["probe"] = json!({
                "url": p.url,
                "rule": serde_json::to_value(p.rule).unwrap_or(Value::Null),
                "up": up,
                "status": p.status(),
            });
        }
    }
    let links = links_of(r);
    if !links.is_empty() {
        c["links"] = links.clone().into();
    }
    if let Some(h) = hint_for(r) {
        c["hint"] = h.into();
    }
    let mut badges: Vec<Value> = r
        .status
        .stale
        .iter()
        .map(|s| {
            let fix = match s.action() {
                stack_core::provenance::StaleAction::Pull => "Rebuild pulls the newer image",
                stack_core::provenance::StaleAction::Rebuild => "Rebuild builds it again",
            };
            badge(&s.badge(), "warn", Some(format!("{s}. {fix}.")))
        })
        .collect();
    if let Some(d) = def {
        if d.persistence.is_some() && !data_safety::has_check(d) {
            badges.push(badge(
                "no data-safety check",
                "warn",
                Some("This component keeps data and nothing checks that data is scrubbed; it counts as sensitive.".into()),
            ));
        }
    }
    if let Some(cs) = inp.safety.component(id).filter(|c| c.has_check) {
        let verdict = cs.entry.as_ref().and_then(|e| e.verdict.as_ref());
        match cs.state {
            SafetyState::NotClean => badges.push(badge(
                "data not clean",
                "bad",
                verdict.map(|v| data_safety::problems(v, true).join("; ")),
            )),
            SafetyState::Error => {
                badges.push(badge("data-safety check failed", "bad", cs.error.clone()))
            }
            SafetyState::Acknowledged => badges.push(badge(
                "data acknowledged",
                "warn",
                cs.entry
                    .as_ref()
                    .and_then(|e| e.ack.as_ref())
                    .map(|a| format!("Acknowledged by {} at {}: {}", a.who, a.at_utc, a.why)),
            )),
            SafetyState::Unscanned => badges.push(badge(
                "data not checked yet",
                "warn",
                Some("Its data-safety check has not run since it started.".into()),
            )),
            SafetyState::Clean => {}
        }
    }
    if !badges.is_empty() {
        c["badges"] = badges.into();
    }
    let mut actions: Vec<Value> = inp
        .offered
        .iter()
        .filter(|o| o.key.as_deref() == Some(id.as_str()))
        .map(|o| o.to_json(&inp.obs.state.fingerprint()))
        .collect();
    if inp.safety.checkable.iter().any(|c| c == id) {
        actions.push(data_safety::check_action(Some(id)));
    }
    let process_log: Option<PathBuf> = inp.processes.processes.get(id).map(|e| e.log.clone());
    if let Some(l) = logs_action(
        inp.inst,
        inp.obs,
        id,
        &inp.loaded.dir,
        process_log.as_ref(),
        inp.exe,
        &inp.loaded.work_dir(),
    ) {
        actions.push(l);
    }
    for (key, url) in &r.links {
        if is_http(url) {
            actions.push(json!({
                "kind": "link",
                "id": format!("open-{key}"),
                "label": if key == "url" { format!("Open {label}") } else { format!("{label}: {key}") },
                "url": url,
            }));
        }
    }
    if !actions.is_empty() {
        c["actions"] = actions.into();
    }
    c
}

fn row(label: &str, value: impl Into<String>) -> Value {
    json!({ "label": label, "value": value.into() })
}

/// A fact row with the tone the compact facts line colours it by.
fn toned_row(label: &str, value: impl Into<String>, tone: &str) -> Value {
    json!({ "label": label, "value": value.into(), "tone": tone })
}

/// `1 component`, `56 components`.
fn plural(n: usize, one: &str) -> String {
    if n == 1 {
        format!("{n} {one}")
    } else {
        format!("{n} {one}s")
    }
}

fn facts(inp: &EnvInputs<'_>) -> Vec<Value> {
    let loaded = inp.loaded;
    let stack = &loaded.stack;
    let mut sections = Vec::new();

    let mut rows = vec![row("Instance", inp.inst.id.clone())];
    let block_start =
        u32::from(stack.slots.port_base) + inp.inst.slot * u32::from(stack.slots.port_stride);
    let ports = inp.inst.ports();
    let in_use = match (ports.iter().next(), ports.iter().last()) {
        (Some(a), Some(b)) if a == b => format!("{a}"),
        (Some(a), Some(b)) => format!("{a}–{b}"),
        _ => "none".to_string(),
    };
    rows.push(row(
        "Slot",
        format!(
            "{} of {} · ports {in_use} (block {block_start}–{})",
            inp.inst.slot,
            stack.slots.count.max(1),
            block_start + u32::from(stack.slots.port_stride).max(1) - 1
        ),
    ));
    if let Some(h) = &stack.hostname {
        rows.push(row("Hostname", h.clone()));
    }
    rows.push(row(
        "Bound item",
        inp.obs.intent.item.clone().unwrap_or_else(|| "none".into()),
    ));
    rows.push(row("Channel", inp.channel.to_string()));
    let file = loaded
        .file
        .strip_prefix(&loaded.dir)
        .unwrap_or(&loaded.file)
        .display()
        .to_string();
    rows.push(row("Definition", file));
    // These describe the runner (where and as what it runs), not how the
    // stack is doing: the view shows them apart from the facts.
    sections.push(json!({
        "id": "stack",
        "kind": "meta",
        "title": format!("Stack {} · slot {}", loaded.stack_name, inp.inst.slot),
        "rows": rows,
    }));

    let memory = inp.inst.memory_by_engine();
    let mut engine_rows = Vec::new();
    for (name, f) in &inp.facts {
        let tone = match f {
            None => None,
            Some(f) if !f.reachable => Some("bad"),
            Some(_) => Some("ok"),
        };
        let value = match f {
            None => "checking…".to_string(),
            Some(f) if !f.reachable => format!(
                "not reachable ({})",
                f.reason.clone().unwrap_or_else(|| "no reason given".into())
            ),
            Some(f) => {
                let mut parts = Vec::new();
                if let Some(v) = &f.version {
                    parts.push(format!("v{v}"));
                }
                if let Some(os) = f.os {
                    parts.push(format!("{os} containers"));
                }
                match (f.mem_free_mb, f.mem_total_mb) {
                    (Some(free), Some(total)) => {
                        parts.push(format!("{} free of {}", format_mb(free), format_mb(total)))
                    }
                    (None, Some(total)) => parts.push(format!("{} total", format_mb(total))),
                    _ => {}
                }
                if parts.is_empty() {
                    "reachable".to_string()
                } else {
                    parts.join(" · ")
                }
            }
        };
        engine_rows.push(match tone {
            Some(t) => toned_row(name, value, t),
            None => row(name, value),
        });
    }
    for (name, needed) in &memory {
        if *needed == 0 {
            continue;
        }
        let budget = inp
            .facts
            .get(name)
            .cloned()
            .flatten()
            .and_then(|f| f.mem_free_mb.or(f.mem_total_mb));
        let value = match budget {
            Some(b) => format!(
                "needs {} of {}",
                format_mb(u64::from(*needed)),
                format_mb(b)
            ),
            None => format!("needs {}", format_mb(u64::from(*needed))),
        };
        let label = format!("Memory on {name}");
        engine_rows.push(match budget {
            Some(b) if u64::from(*needed) > b => toned_row(&label, value, "warn"),
            _ => row(&label, value),
        });
    }
    if !engine_rows.is_empty() {
        sections.push(json!({ "id": "engines", "title": "Engines", "rows": engine_rows }));
    }

    if let Some(relay) = &inp.relay {
        sections.push(relay_section(relay, "none (nothing is running)"));
    }

    sections.extend(script_facts(inp));
    if inp.safety.configured {
        sections.push(data_safety_facts(inp.safety));
    }
    sections
}

fn short_hash(h: &str) -> &str {
    &h[..h.len().min(12)]
}

fn state_words(s: SafetyState) -> &'static str {
    match s {
        SafetyState::Clean => "clean",
        SafetyState::NotClean => "not clean",
        SafetyState::Acknowledged => "acknowledged",
        SafetyState::Unscanned => "not checked",
        SafetyState::Error => "cannot be judged",
    }
}

fn data_safety_facts(s: &InstanceSafety) -> Value {
    let wire = s.wire();
    let mut rows = vec![row("State", state_words(s.state))];
    rows.push(row(
        "Policy",
        format!(
            "on findings: {} · unscanned: {}",
            s.on_finding, s.unscanned_policy
        ),
    ));
    rows.push(row(
        "Last scan",
        s.last_scan().unwrap_or("never").to_string(),
    ));
    let marker = &wire["marker"];
    if marker.is_object() {
        let mut parts = vec![if marker["scrubbed"] == true {
            "scrubbed".to_string()
        } else {
            "not attested".to_string()
        }];
        if let Some(v) = marker["version"].as_str() {
            parts.push(format!("version {v}"));
        }
        if let Some(at) = marker["at"].as_str() {
            parts.push(format!("at {at}"));
        }
        rows.push(row("Marker", parts.join(" · ")));
    }
    let rules = match (&s.current_rules, s.rules_hash()) {
        (None, _) => "none configured".to_string(),
        (Some(Err(e)), _) => format!("cannot be read: {e}"),
        (Some(Ok(now)), Some(used)) if &used != now => format!(
            "hash {} (changed since the last scan, which used {})",
            short_hash(now),
            short_hash(&used)
        ),
        (Some(Ok(now)), _) => format!("hash {}", short_hash(now)),
    };
    rows.push(row("Rules", rules));
    if let Some(a) = wire["ack"].as_object() {
        rows.push(row(
            "Acknowledged",
            format!(
                "by {} at {}: {}",
                a["by"].as_str().unwrap_or_default(),
                a["atUtc"].as_str().unwrap_or_default(),
                a["note"].as_str().unwrap_or_default()
            ),
        ));
    }
    for c in &s.components {
        let detail = match (c.state, c.entry.as_ref().and_then(|e| e.verdict.as_ref())) {
            (SafetyState::Error, _) => c.error.clone(),
            (SafetyState::Unscanned, _) if !c.has_check => {
                Some("keeps data; no check declared".to_string())
            }
            (SafetyState::Unscanned, _) => Some("not checked since it started".to_string()),
            (_, Some(v)) => Some(data_safety::verdict_line(v)),
            _ => None,
        };
        let value = match detail {
            Some(d) if !d.starts_with(state_words(c.state)) => {
                format!("{} · {d}", state_words(c.state))
            }
            Some(d) => d,
            None => state_words(c.state).to_string(),
        };
        rows.push(row(&c.id, value));
    }
    let mut actions = Vec::new();
    if !s.checkable.is_empty() {
        actions.push(data_safety::check_action(None));
    }
    if let Some(a) = s.ack_action() {
        actions.push(a);
    }
    let tone = match s.state {
        SafetyState::Clean | SafetyState::Acknowledged => "ok",
        SafetyState::Unscanned if !s.blocks_agents() => "warn",
        _ => "bad",
    };
    // One badge in the view's header; the rows are its details.
    let mut section = json!({
        "id": "data-safety",
        "title": "Data safety",
        "status": { "label": "Data", "text": state_words(s.state), "tone": tone },
        "rows": rows,
    });
    if !actions.is_empty() {
        section["actions"] = actions.into();
    }
    section
}

/// The Relay facts section: per engine and gateway, the ports listened on
/// (each forwarded to the same port on loopback), what the peer check
/// serves and refused, engines that need no relay, and the problems.
/// `idle` is the row shown when nothing listens.
pub fn relay_section(relay: &RelayState, idle: &str) -> Value {
    let mut rows: Vec<Value> = Vec::new();
    let mut seen: Vec<(String, std::net::IpAddr)> = Vec::new();
    for w in &relay.listening {
        let key = (w.engine.clone(), w.bind.ip());
        if seen.contains(&key) {
            continue;
        }
        let ports: Vec<u16> = relay
            .listening
            .iter()
            .filter(|x| x.engine == key.0 && x.bind.ip() == key.1)
            .map(|x| x.bind.port())
            .collect();
        let runs = crate::relay_firewall::normalize(
            ports
                .iter()
                .map(|p| crate::relay_firewall::PortRange { lo: *p, hi: *p }),
        );
        let target = if relay
            .listening
            .iter()
            .filter(|x| x.engine == key.0 && x.bind.ip() == key.1)
            .all(|x| x.target.ip() == w.target.ip() && x.target.port() == x.bind.port())
        {
            format!("{} (same ports)", w.target.ip())
        } else {
            "loopback".to_string()
        };
        let mut value = format!(
            "{} port{} {} → {target}",
            key.1,
            if ports.len() == 1 { "" } else { "s" },
            crate::relay_firewall::ranges_text(&runs)
        );
        if let Some(g) = relay
            .guards
            .iter()
            .find(|g| g.engine == key.0 && g.gateway == key.1)
        {
            value = format!("{value} · {}", g.summary());
        }
        let mut r = row(&key.0, value);
        if relay
            .guards
            .iter()
            .any(|g| g.engine == key.0 && g.gateway == key.1 && g.refused > 0)
        {
            r["badges"] = json!([badge("refused", "warn", None)]);
        }
        rows.push(r);
        seen.push(key);
    }
    for (engine, why) in &relay.not_needed {
        rows.push(row(engine, why.clone()));
    }
    for p in &relay.problems {
        rows.push(json!({
            "label": p.engine,
            "value": p.text,
            "badges": [badge("problem", "bad", None)],
        }));
    }
    if rows.is_empty() {
        rows.push(row("Listeners", idle));
    }
    json!({ "id": "relay", "title": "Relay", "rows": rows })
}

/// The alerts of the relay's problems.
pub fn relay_alerts(relay: &RelayState) -> Vec<Value> {
    relay
        .problems
        .iter()
        .map(|p| json!({ "text": p.text, "severity": "bad" }))
        .collect()
}

/// One alert per component whose data is not clean, cannot be judged, or
/// is not checked.
fn data_safety_alerts(s: &InstanceSafety) -> Vec<Value> {
    let mut out = Vec::new();
    for c in &s.components {
        let Some((text, severity)) = component_problem(c) else {
            continue;
        };
        let effect = data_safety::policy_effect(c.policy)
            .map(|e| format!(" — {e}"))
            .unwrap_or_default();
        out.push(
            json!({ "text": format!("{text}{effect}"), "severity": severity, "componentId": c.id }),
        );
    }
    out
}

/// What one component's data safety has to say, without the policy's
/// effect, and how loud: `None` for a clean or acknowledged component.
fn component_problem(c: &data_safety::ComponentSafety) -> Option<(String, &'static str)> {
    match c.state {
        SafetyState::NotClean => {
            let why = c
                .entry
                .as_ref()
                .and_then(|e| e.verdict.as_ref())
                .map(|v| data_safety::problems(v, false).join("; "))
                .unwrap_or_else(|| "not clean".into());
            Some((format!("{}: {why}", c.id), "bad"))
        }
        SafetyState::Error => Some((
            format!(
                "{}: the data-safety check could not be judged ({})",
                c.id,
                c.error.as_deref().unwrap_or("no verdict")
            ),
            "bad",
        )),
        SafetyState::Unscanned => {
            let why = if c.has_check {
                "has a data-safety check that has not run since it started"
            } else {
                "keeps data and has no data-safety check"
            };
            let severity = if c
                .policy
                .is_some_and(stack_core::safety::Policy::blocks_agents)
            {
                "bad"
            } else {
                "warn"
            };
            Some((format!("{} {why}", c.id), severity))
        }
        SafetyState::Clean | SafetyState::Acknowledged => None,
    }
}

/// The `dataSafety` block: the instance's verdict plus where it applies
/// (`bound`, `project`) and one reason line per component, so the app can
/// keep agents out of exactly those folders and say why.
fn data_safety_block(inp: &EnvInputs<'_>) -> Value {
    let mut block = inp.safety.wire();
    block["bound"] = data_safety::bound(inp.loaded);
    block["project"] = data_safety::project_dir(&inp.loaded.dir);
    block["reasons"] = inp
        .safety
        .components
        .iter()
        .filter_map(|c| {
            component_problem(c).map(|(text, _)| json!({ "component": c.id, "text": text }))
        })
        .collect();
    block
}

fn alerts(inp: &EnvInputs<'_>, components: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    for c in components {
        let Some(state) = c["state"]
            .as_str()
            .and_then(|s| ComponentState::ALL.into_iter().find(|x| x.as_str() == s))
        else {
            continue;
        };
        if !state.is_problem() {
            continue;
        }
        let id = c["id"].as_str().unwrap_or_default();
        let label = c["label"].as_str().unwrap_or(id);
        let text = match c["hint"].as_str() {
            Some(h) => format!("{label} {}: {h}", state_phrase(state)),
            None => format!("{label} {}", state_phrase(state)),
        };
        // `kind: state` says the alert only restates the row's state: the
        // view lists those rows in one short banner instead.
        out.push(json!({ "text": text, "severity": "bad", "componentId": id, "kind": "state" }));
    }
    for r in inp.reports {
        let Some(view) = &r.script else { continue };
        for a in &view.alerts {
            let severity = match a.tone {
                AlertTone::Bad => "bad",
                AlertTone::Warn => "warn",
                AlertTone::Info => continue,
            };
            out.push(json!({ "text": a.text, "severity": severity, "componentId": r.status.id }));
        }
    }
    for (name, f) in &inp.facts {
        let Some(f) = f else { continue };
        if !f.reachable {
            out.push(json!({
                "text": format!("{name} is not reachable: {}", f.reason.clone().unwrap_or_default()),
                "severity": "bad",
            }));
        } else if let (Some(needed), Some(actual)) = (inp.loaded.stack.os_for_engine(name), f.os) {
            if needed != actual {
                out.push(json!({
                    "text": format!("{name} runs {actual} containers; its group needs {needed} containers"),
                    "severity": "bad",
                }));
            }
        }
    }
    if let Some(relay) = &inp.relay {
        out.extend(relay_alerts(relay));
    }
    if let Some(h) = &inp.hosts {
        if !h.ok() {
            out.push(json!({ "text": h.reason(), "severity": "warn" }));
        }
    }
    // An engine already reported unreachable explains its own failed
    // questions; repeating each one would bury the cause.
    let down: Vec<&String> = inp
        .facts
        .iter()
        .filter(|(_, f)| f.as_ref().is_some_and(|f| !f.reachable))
        .map(|(name, _)| name)
        .collect();
    for note in &inp.obs.notes {
        if down.iter().any(|e| note.starts_with(&format!("{e}:"))) {
            continue;
        }
        out.push(json!({ "text": note, "severity": "warn" }));
    }
    out.extend(data_safety_alerts(inp.safety));
    out
}

fn banners(inp: &EnvInputs<'_>) -> Vec<Value> {
    let mut out = Vec::new();
    let errors: Vec<String> = inp
        .loaded
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .map(|d| d.to_string())
        .collect();
    let warnings: Vec<String> = inp
        .loaded
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Warning)
        .map(|d| d.to_string())
        .collect();
    if !errors.is_empty() {
        out.push(json!({
            "id": "definition-errors",
            "tone": "bad",
            "title": format!("The stack definition has {} error(s); nothing is offered until they are fixed", errors.len()),
            "lines": errors,
        }));
    }
    if !warnings.is_empty() {
        out.push(json!({
            "id": "definition-warnings",
            "tone": "warn",
            "title": format!("The stack definition has {} warning(s)", warnings.len()),
            "lines": warnings,
        }));
    }
    if let Some(h) = &inp.hosts {
        if !h.ok() {
            let mut b = json!({
                "id": "hostname",
                "tone": "warn",
                "title": h.reason(),
                "lines": h.line_to_add.iter().map(|l| format!("hosts file line: {l}")).collect::<Vec<_>>(),
            });
            if let Some(fix) = &h.fix {
                b["actions"] = json!([{
                    "kind": "terminal",
                    "id": "hosts-fix",
                    "label": "Add it (asks for elevation)",
                    "cwd": inp.loaded.dir.display().to_string(),
                    "command": fix.argv,
                }]);
            }
            out.push(b);
        }
    }
    let s = inp.safety;
    if s.blocks_agents() {
        let lines: Vec<String> = data_safety_alerts(s)
            .iter()
            .filter_map(|a| a["text"].as_str().map(str::to_string))
            .collect();
        let mut b = json!({
            "id": "data-safety",
            "tone": "bad",
            "title": format!(
                "Agents are blocked on {}: its data is not known to be scrubbed",
                s.instance
            ),
            "lines": lines,
        });
        if let Some(a) = s.ack_action() {
            b["actions"] = json!([a]);
        }
        out.push(b);
    }
    out
}

/// The stack's own verbs that a script component standing for the whole
/// stack makes redundant: with it as the only component, "bring up" is its
/// `up` script, "stop" its `stop`, and so on.
const STACK_VERBS: [&str; 5] = ["up", "start", "stop", "down", "rebuild"];

/// The whole `GET` answer.
pub fn environment(inp: &EnvInputs<'_>, errors: &[(String, String)]) -> Value {
    let stack = &inp.loaded.stack;
    let fingerprint = inp.obs.state.fingerprint();
    // Groups in display order: the definition's, then each report's as it
    // comes (a script's in the order its status lists them).
    let mut groups: IndexMap<String, Value> = IndexMap::new();
    fn add_group(groups: &mut IndexMap<String, Value>, id: &str, label: String) {
        groups
            .entry(id.to_string())
            .or_insert_with(|| json!({ "id": id, "label": label }));
    }
    for (id, g) in &stack.groups {
        add_group(
            &mut groups,
            id,
            g.label.clone().unwrap_or_else(|| capital(id)),
        );
    }
    let mut components: Vec<Value> = Vec::new();
    let mut dropped = 0;
    let mut notices: Vec<String> = Vec::new();
    // Who reports a script's rows and runs their actions: runner metadata,
    // said once with the stack's own.
    let mut provenance: Vec<Value> = Vec::new();
    let mut root_actions: Vec<Value> = Vec::new();
    let mut group_actions: IndexMap<String, Vec<Value>> = IndexMap::new();
    let sole = inp.inst.components.len() == 1;
    let mut sole_script_at_root = false;
    for r in inp.reports {
        let (rows, left_out) = component_rows(inp, r);
        dropped += left_out;
        match &r.script {
            Some(view) if view.multi => {
                let id = &r.status.id;
                let label = label_of(inp, id);
                for g in &view.groups {
                    add_group(&mut groups, &g.id, g.label.clone());
                }
                let mut used: Vec<String> = Vec::new();
                for row in &view.rows {
                    let g = script_row_group(r, row);
                    if !used.contains(&g) {
                        used.push(g);
                    }
                }
                for g in &used {
                    let shown = if g == id { label.clone() } else { capital(g) };
                    add_group(&mut groups, g, shown);
                }
                provenance.push(row(
                    "Scripts",
                    format!(
                        "{label} reports {}; their actions run {label}'s scripts",
                        plural(view.rows.len(), "component")
                    ),
                ));
                let picks_rows = stack
                    .components
                    .get(id)
                    .is_some_and(|d| script::takes_rows(d, ScriptVerb::Up));
                let verbs: Vec<Value> = inp
                    .offered
                    .iter()
                    .filter(|o| o.key.as_deref() == Some(id.as_str()))
                    .map(|o| {
                        let mut v = o.to_json(&fingerprint);
                        v["id"] = script_action_id(id, &o.id).into();
                        if o.id == "up" && picks_rows && view.from == ViewFrom::Status {
                            v["inputs"] =
                                crate::actions::rows_inputs(view, &|row| script_row_group(r, row));
                        }
                        v
                    })
                    .collect();
                // The script's verbs go on its group when its rows fill
                // exactly one group that holds nothing else.
                let own_group = match used.as_slice() {
                    [g] if !sole => {
                        let others = inp.reports.iter().any(|o| {
                            o.status.id != *id
                                && match &o.script {
                                    Some(v) if v.multi => {
                                        v.rows.iter().any(|row| script_row_group(o, row) == *g)
                                    }
                                    _ => o.group == *g,
                                }
                        });
                        (!others).then(|| g.clone())
                    }
                    _ => None,
                };
                match own_group {
                    Some(g) => group_actions.entry(g).or_default().extend(verbs),
                    None => {
                        sole_script_at_root |= sole;
                        root_actions.extend(verbs);
                    }
                }
            }
            _ => add_group(&mut groups, &r.group, capital(&r.group)),
        }
        components.extend(rows);
    }
    let stack_actions: Vec<Value> = inp
        .offered
        .iter()
        .filter(|o| o.key.is_none())
        .filter(|o| !(sole_script_at_root && STACK_VERBS.contains(&o.id.as_str())))
        .map(|o| {
            let mut v = o.to_json(&fingerprint);
            // Bring up picks what comes up when there is anything to pick.
            if o.id == "up" && (stack.components.len() > 1 || !stack.selections.is_empty()) {
                v["inputs"] = crate::actions::up_inputs(stack);
            }
            v
        })
        .collect();
    let mut actions = stack_actions;
    actions.extend(root_actions);
    if dropped > 0 {
        notices.push(format!(
            "{} to a host other than the stack's own {} left out",
            plural(dropped, "reported link"),
            if dropped == 1 { "was" } else { "were" }
        ));
    }
    let groups: Vec<Value> = groups
        .into_iter()
        .map(|(id, mut g)| {
            if let Some(a) = group_actions.shift_remove(&id) {
                g["actions"] = a.into();
            }
            g
        })
        .collect();
    let alerts = alerts(inp, &components);
    let mut freshness = json!({
        "asOf": inp.now_utc,
        "errors": errors.iter().map(|(s, m)| json!({"source": s, "message": m})).collect::<Vec<_>>(),
    });
    if !notices.is_empty() {
        freshness["notices"] = notices.into();
    }
    let mut facts = facts(inp);
    if let Some(rows) = facts
        .iter_mut()
        .find(|s| s["kind"] == "meta")
        .and_then(|s| s["rows"].as_array_mut())
    {
        rows.extend(provenance);
    }
    json!({
        "freshness": freshness,
        "groups": groups,
        "components": components,
        "actions": actions,
        "facts": facts,
        "alerts": alerts,
        "banners": banners(inp),
        "dataSafety": data_safety_block(inp),
    })
}

/// What [`decorate_jobs`] needs besides the jobs.
pub struct JobsContext<'a> {
    /// The `chappa-stack` program the `job-log` terminal runs.
    pub exe: &'a Path,
    /// Where that terminal opens.
    pub project_dir: &'a Path,
    /// The stack has script components whose status scripts wait while a
    /// change runs (the view says so).
    pub scripts: bool,
}

fn job_json(rec: &crate::records::JobRecord) -> Value {
    let mut j = json!({
        "id": rec.id,
        "label": rec.label,
        "action": rec.action,
        "status": rec.status.as_str(),
        "createdUtc": rec.created_utc,
        "headless": rec.headless,
        "log": rec.log.display().to_string(),
    });
    for (k, v) in [
        ("target", rec.target.clone()),
        ("actor", rec.actor.clone()),
        ("startedUtc", rec.started_utc.clone()),
        ("endedUtc", rec.ended_utc.clone()),
        ("error", rec.error.clone()),
    ] {
        if let Some(v) = v {
            j[k] = v.into();
        }
    }
    if let Some(c) = rec.exit_code {
        j["exitCode"] = c.into();
    }
    j
}

fn log_action(rec: &crate::records::JobRecord, label: &str, cx: &JobsContext<'_>) -> Value {
    json!({
        "kind": "terminal",
        "id": crate::server::JOB_LOG_ACTION_ID,
        "label": label,
        "cwd": cx.project_dir.display().to_string(),
        "command": [cx.exe.display().to_string(), "follow", rec.log.display().to_string()],
    })
}

/// Add the instance's changing jobs to an Environment payload `v` (see the
/// table above). The busy actions are disabled by the server before this.
pub fn decorate_jobs(v: &mut Value, jobs: &crate::lease::JobsView, cx: &JobsContext<'_>) {
    let mut block = json!({ "active": null, "last": null });
    if let Some(a) = &jobs.active {
        let since = a.started_utc.as_deref().unwrap_or(&a.created_utc);
        let mut j = job_json(a);
        j["cancelling"] = jobs.cancelling.into();
        block["active"] = j;
        let mut cancel = json!({
            "kind": "endpoint",
            "id": crate::server::CANCEL_ACTION_ID,
            "label": "Cancel",
            "mutates": true,
            "confirm": format!(
                "Cancel {}? The step running now is stopped with every process it started, and the steps after it do not run. The stack may be left part-way; look at it before changing it again.",
                a.label
            ),
            "params": { "jobId": a.id },
        });
        if jobs.cancelling {
            cancel["disabled"] =
                "already cancelling; it ends once its current step has stopped".into();
        }
        if let Some(list) = v.get_mut("actions").and_then(Value::as_array_mut) {
            list.push(cancel);
            list.push(log_action(a, "Job log", cx));
        } else {
            v["actions"] = json!([cancel, log_action(a, "Job log", cx)]);
        }
        if let Some(target) = &a.target {
            let badge = badge(
                &format!("running: {}", a.action),
                "info",
                Some(crate::lease::busy_text(&a.label, since)),
            );
            if let Some(rows) = v.get_mut("components").and_then(Value::as_array_mut) {
                for c in rows.iter_mut().filter(|c| c["id"] == target.as_str()) {
                    match c.get_mut("badges").and_then(Value::as_array_mut) {
                        Some(b) => b.push(badge.clone()),
                        None => c["badges"] = json!([badge.clone()]),
                    }
                }
            }
        }
        if cx.scripts {
            let note = format!(
                "status scripts wait while {} runs; rows show their last answer",
                a.label
            );
            match v
                .pointer_mut("/freshness/notices")
                .and_then(Value::as_array_mut)
            {
                Some(n) => n.push(note.into()),
                None => v["freshness"]["notices"] = json!([note]),
            }
        }
    }
    if let Some(l) = &jobs.last {
        block["last"] = job_json(l);
        if l.status != crate::records::JobStatus::Done {
            let when = l
                .ended_utc
                .as_deref()
                .map(crate::lease::clock_of)
                .unwrap_or_default();
            let mut text = format!("{} {} at {when}", l.label, l.status.as_str());
            if let Some(c) = l.exit_code {
                text.push_str(&format!(" (exit code {c})"));
            }
            if let Some(e) = &l.error {
                text.push_str(&format!(": {e}"));
            }
            let severity = if l.status == crate::records::JobStatus::Cancelled {
                "warn"
            } else {
                "bad"
            };
            if let Some(alerts) = v.get_mut("alerts").and_then(Value::as_array_mut) {
                alerts.push(json!({ "text": text, "severity": severity, "kind": "job" }));
            }
        }
        if jobs.active.is_none() {
            let action = log_action(l, "Last job's log", cx);
            match v.get_mut("actions").and_then(Value::as_array_mut) {
                Some(list) => list.push(action),
                None => v["actions"] = json!([action]),
            }
        }
    }
    v["job"] = block;
}

/// A payload for a project whose definition cannot be loaded: no rows,
/// one banner saying why, and a data-safety block that says it cannot
/// tell (`instance` is `<stack>/<slot>` as configured).
pub fn unavailable(reason: &str, now_utc: &str, instance: &str, project: &Path) -> Value {
    json!({
        "dataSafety": {
            "instance": instance,
            "state": SafetyState::Error.as_str(),
            "policy": null,
            "blocksAgents": false,
            "lastScanUtc": null,
            "rulesHash": null,
            "marker": null,
            "findings": [],
            "unscanned": [],
            "ack": null,
            "ackActionId": data_safety::ACK_ACTION_ID,
            "bound": null,
            "project": data_safety::project_dir(project),
            "reasons": [{ "component": null, "text": format!("the stack cannot be read: {reason}") }],
        },
        "secretScan": crate::secret_scan::unavailable_block(instance, project, reason),
        "freshness": { "asOf": now_utc, "errors": [{ "source": "definition", "message": reason }] },
        "groups": [],
        "components": [],
        "banners": [{ "id": "definition-errors", "tone": "bad", "title": "The stack cannot be read", "lines": [reason] }],
        "alerts": [{ "text": reason, "severity": "bad" }],
    })
}

fn capital(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}
