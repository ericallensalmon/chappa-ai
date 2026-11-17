//! The Environment payload: the runner's status turned into the view's
//! shapes.
//!
//! | field | from |
//! |---|---|
//! | `groups` | the definition's groups, in order, labelled |
//! | `components[].state` | the nine states (desired state from the intent record vs what the engine or the process table reports) |
//! | `uptime` / `restarts` / `exitCode` | `inspect` for containers, the process table for processes |
//! | `source` | provenance: mode, ref (the image tag for prebuilt), commit, plus `dirty`, `buildId`, `image`, `stale` |
//! | `probe` | the component's probe rule, run while it runs |
//! | `links` | its `http(s)` outputs; the `url` output is the browser one |
//! | `hint` | why a red or amber state is what it is |
//! | `badges` | staleness (`newer prebuilt`, `behind 2` …), "no data-safety check" on an unscanned persistent component, the data-safety state of a checked one, the mode |
//! | `actions` | the component's verbs, its data-safety check, its `logs` terminal, its links |
//! | `alerts` | red components with their cause, engines that cannot be reached or run the wrong OS mode, relay failures, the hostname check, data that is not clean (`bad`) or not checked (`warn` under `unscanned: warn`) |
//! | `facts` | the stack (instance, slot, port block, bound item, channel, with the stack-wide actions), engines and versions, the memory budget, relay listeners, data safety (state, policy, last scan, marker, rules hash, acknowledgement, each component, with the check and acknowledge actions) |
//! | `banners` | definition warnings or errors; the hosts-file fix; agents blocked by data safety (with the acknowledge action) |
//! | `dataSafety` | the instance's data-safety block the app enforces on (see [`crate::data_safety`]) |
//! | `secretScan` | the instance's secret-scan block, added by the server with its alerts, banner and facts section (see [`crate::secret_scan`]) |
//!
//! A script component shows as one row, or, when its status script reports
//! several components (a wrapped whole stack), as one row per reported
//! component (`<component>/<name>`) in its group with the script
//! component's verbs on each. The script's facts become a section of their
//! own, its `warn` / `bad` alerts Environment alerts, its `info` alerts rows
//! of that section. Reported links pass the same rule as every other link:
//! only the stack's own hostname (loopback names when it has none).
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
use stack_core::script::{AlertTone, ScriptView, ViewFrom};

use crate::actions::{logs_action, Offered};
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

/// The rows of one report: one, or a script component's reported ones.
fn component_rows(inp: &EnvInputs<'_>, r: &ComponentReport) -> Vec<Value> {
    match &r.script {
        Some(view) => script_rows(inp, r, view),
        None => vec![component(inp, r)],
    }
}

fn kind_name(kind: ComponentKind) -> &'static str {
    match kind {
        ComponentKind::Oneshot => "oneshot",
        ComponentKind::Service => "service",
    }
}

fn script_rows(inp: &EnvInputs<'_>, r: &ComponentReport, view: &ScriptView) -> Vec<Value> {
    let id = &r.status.id;
    let label = inp
        .loaded
        .stack
        .components
        .get(id)
        .and_then(|d| d.label.clone())
        .unwrap_or_else(|| id.clone());
    let fingerprint = inp.obs.state.fingerprint();
    let verbs: Vec<Value> = inp
        .offered
        .iter()
        .filter(|o| o.key.as_deref() == Some(id.as_str()))
        .map(|o| o.to_json(&fingerprint))
        .collect();
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
    for row in &view.rows {
        let running = matches!(
            row.state,
            ComponentState::Up
                | ComponentState::Starting
                | ComponentState::Unhealthy
                | ComponentState::Crashloop
        );
        let uptime = match (running, &row.since, now) {
            (true, Some(s), Some(n)) => uptime_text(s, n),
            _ => None,
        };
        let mut c = json!({
            "id": row.id,
            "group": r.group,
            "label": if view.multi { row.name.clone() } else { label.clone() },
            "kind": kind_name(r.kind),
            "state": row.state.as_str(),
            "uptime": uptime,
            "restarts": Value::Null,
            "exitCode": row.exit_code,
            "source": Value::Null,
        });
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
        let mut hint: Vec<String> = row.detail.iter().cloned().collect();
        if view.multi {
            hint.push(format!(
                "reported by {label}; its actions run {label}'s scripts"
            ));
        }
        if dropped > 0 {
            hint.push(format!(
                "{dropped} reported link(s) to a host other than the stack's own left out"
            ));
        }
        if !hint.is_empty() {
            c["hint"] = hint.join("; ").into();
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
        let mut actions = verbs.clone();
        actions.extend(link_actions);
        if !actions.is_empty() {
            c["actions"] = actions.into();
        }
        out.push(c);
    }
    out
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
    let fingerprint = inp.obs.state.fingerprint();
    let stack_actions: Vec<Value> = inp
        .offered
        .iter()
        .filter(|o| o.key.is_none())
        .map(|o| o.to_json(&fingerprint))
        .collect();
    sections.push(json!({
        "id": "stack",
        "title": format!("Stack {} · slot {}", loaded.stack_name, inp.inst.slot),
        "rows": rows,
        "actions": stack_actions,
    }));

    let memory = inp.inst.memory_by_engine();
    let mut engine_rows = Vec::new();
    for (name, f) in &inp.facts {
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
        engine_rows.push(row(name, value));
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
        engine_rows.push(row(&format!("Memory on {name}"), value));
    }
    if !engine_rows.is_empty() {
        sections.push(json!({ "id": "engines", "title": "Engines", "rows": engine_rows }));
    }

    if let Some(relay) = &inp.relay {
        let mut rows: Vec<Value> = relay
            .listening
            .iter()
            .map(|w| row(&w.engine, format!("{} → {}", w.bind, w.target)))
            .collect();
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
            rows.push(row("Listeners", "none (nothing is running)"));
        }
        sections.push(json!({ "id": "relay", "title": "Relay", "rows": rows }));
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
    let mut section = json!({ "id": "data-safety", "title": "Data safety", "rows": rows });
    if !actions.is_empty() {
        section["actions"] = actions.into();
    }
    section
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
        out.push(json!({ "text": text, "severity": "bad", "componentId": id }));
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
        for p in &relay.problems {
            out.push(json!({ "text": p.text, "severity": "bad" }));
        }
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

/// The whole `GET` answer.
pub fn environment(inp: &EnvInputs<'_>, errors: &[(String, String)]) -> Value {
    let stack = &inp.loaded.stack;
    let mut groups: Vec<Value> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let used: Vec<String> = inp
        .inst
        .components
        .values()
        .map(|c| c.group.clone())
        .collect();
    for (id, g) in &stack.groups {
        if seen.insert(id.clone()) {
            groups
                .push(json!({ "id": id, "label": g.label.clone().unwrap_or_else(|| capital(id)) }));
        }
    }
    for id in used {
        if seen.insert(id.clone()) {
            groups.push(json!({ "id": id, "label": capital(&id) }));
        }
    }
    let components: Vec<Value> = inp
        .reports
        .iter()
        .flat_map(|r| component_rows(inp, r))
        .collect();
    let alerts = alerts(inp, &components);
    json!({
        "freshness": {
            "asOf": inp.now_utc,
            "errors": errors.iter().map(|(s, m)| json!({"source": s, "message": m})).collect::<Vec<_>>(),
        },
        "groups": groups,
        "components": components,
        "facts": facts(inp),
        "alerts": alerts,
        "banners": banners(inp),
        "dataSafety": data_safety_block(inp),
    })
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
