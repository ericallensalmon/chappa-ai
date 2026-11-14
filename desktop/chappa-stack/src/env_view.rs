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
//! | `badges` | staleness (`newer prebuilt`, `behind 2` …), "no data-safety check" on an unscanned persistent component, the mode |
//! | `actions` | the component's verbs, its `logs` terminal, its links |
//! | `alerts` | red components with their cause, engines that cannot be reached or run the wrong OS mode, relay failures, the hostname check |
//! | `facts` | the stack (instance, slot, port block, bound item, channel, with the stack-wide actions), engines and versions, the memory budget, relay listeners, the data-safety rules hash |
//! | `banners` | definition warnings or errors; the hosts-file fix |
//!
//! `summary` is left out: the view counts the components itself, and a
//! summary is only worth sending when it knows more than the rows do.

use indexmap::IndexMap;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use stack_core::checks::{format_mb, EngineFacts};
use stack_core::compose::{parse_utc, uptime_text};
use stack_core::definition::{ComponentKind, Mode, Severity};
use stack_core::driver::{ComponentState, ResolvedInstance};
use stack_core::hosts::HostsCheck;
use stack_core::placeholders;
use stack_core::probe::ProbeOutcome;
use stack_core::runner::{ComponentReport, Observation};

use crate::actions::{logs_action, Offered};
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
    pub rules_hash: Option<String>,
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

/// The hosts the stack's links point at: its hostname (or loopback when it
/// has none), with each port in use. Nothing else is declared, so the
/// trust dialog lists only the stack's own names.
pub fn link_hosts(loaded: &Loaded, inst: &ResolvedInstance) -> Vec<String> {
    let ctx = inst.host_context();
    let own: Vec<&str> = match &loaded.stack.hostname {
        Some(h) => vec![h.as_str()],
        None => vec!["localhost", "127.0.0.1"],
    };
    let mut out: BTreeSet<String> = BTreeSet::new();
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
        "kind": match r.kind { ComponentKind::Oneshot => "oneshot", ComponentKind::Service => "service" },
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
        if d.persistence.is_some() && d.data_safety.is_none() {
            badges.push(badge(
                "no data-safety check",
                "warn",
                Some("This component keeps data and nothing checks that data is scrubbed; it counts as sensitive.".into()),
            ));
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
    let process_log: Option<PathBuf> = inp.processes.processes.get(id).map(|e| e.log.clone());
    if let Some(l) = logs_action(
        inp.inst,
        inp.obs,
        id,
        &inp.loaded.dir,
        process_log.as_ref(),
        inp.exe,
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

    if let Some(hash) = &inp.rules_hash {
        sections.push(json!({
            "id": "data-safety",
            "title": "Data safety",
            "rows": [row("Rules", format!("hash {}", &hash[..hash.len().min(16)]))],
        }));
    }
    sections
}

fn alerts(inp: &EnvInputs<'_>, components: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    for (r, c) in inp.reports.iter().zip(components) {
        if !r.status.state.is_problem() {
            continue;
        }
        let label = c["label"].as_str().unwrap_or(&r.status.id);
        let text = match c["hint"].as_str() {
            Some(h) => format!("{label} {}: {h}", state_phrase(r.status.state)),
            None => format!("{label} {}", state_phrase(r.status.state)),
        };
        out.push(json!({ "text": text, "severity": "bad", "componentId": r.status.id }));
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
    let components: Vec<Value> = inp.reports.iter().map(|r| component(inp, r)).collect();
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
    })
}

/// A payload for a project whose definition cannot be loaded: no rows,
/// one banner saying why.
pub fn unavailable(reason: &str, now_utc: &str) -> Value {
    json!({
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
