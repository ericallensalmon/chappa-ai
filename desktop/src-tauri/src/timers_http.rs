//! The HTTP shape of the timer surface: one handler per `ROUTES`
//! row under `/timers…` in `control_http.rs`, in the shape the coordination routes
//! established — thin handlers, the shared `Args` view over query/body, and
//! the typed `CoordError` → status/JSON mapping. Every rule lives in
//! `timers.rs`.
//!
//! Two argument conventions are deliberate, because an orchestrator writing
//! against this surface should not have to look them up:
//! - `processes` accepts process IDs **or** names (resolved against the live
//!   registry at schedule time);
//! - the delivery target defaults to the CALLER's own process — the
//!   `X-Chappa-Actor` header, which `chappa-ai-mcp` fills from its
//!   `CHAPPA_AI_PROCESS_ID`;
//! - `project_id` defaults like every other coordination tool's: the
//!   explicit argument, else the route's `{project}`, else the project the
//!   actor's own process belongs to (resolved in `timers.rs`).

use serde_json::Value;

use crate::control_http::{ControlState, Params, Resp};
use crate::coordination::CoordError;
use crate::coordination_http::{record, run, Args};
use crate::timers::{Lifecycle, TimerKind};

/// The delivery target: an explicit `delivery_process_id`, else the caller's
/// own process id (the actor header). A caller with neither — a bare curl
/// with no `X-Chappa-Actor` — must say where the body goes; a timer that
/// fires "somewhere" is the exact incident this rule exists to prevent.
fn delivery_target(args: &Args, params: &Params) -> Result<u32, CoordError> {
    if let Some(explicit) = args.i64("delivery_process_id")? {
        return u32::try_from(explicit)
            .map_err(|_| CoordError::Invalid("`delivery_process_id` must be a process id".into()));
    }
    params.actor.parse::<u32>().map_err(|_| {
        CoordError::Invalid(
            "`delivery_process_id` is required when the caller is not itself a chappa-ai process \
             (a timer must know which process its body reaches)"
                .into(),
        )
    })
}

/// A non-negative millisecond count (`Args::usize` already refuses negatives).
fn optional_ms(args: &Args, key: &str) -> Result<Option<u64>, CoordError> {
    Ok(args.usize(key)?.map(|v| v as u64))
}

fn required_ms(args: &Args, key: &str) -> Result<u64, CoordError> {
    optional_ms(args, key)?.ok_or_else(|| CoordError::Invalid(format!("`{key}` is required")))
}

/// The explicit `project_id`, else the route's `{project}`; the service
/// falls back to the actor's own project after that.
fn project_scope(args: &Args, params: &Params) -> Result<Option<i64>, CoordError> {
    Ok(args.i64("project_id")?.or(params.project.map(i64::from)))
}

/// One entry of `processes`, in every accepted form: a bare id, a name,
/// or the explicit `{process_id}` / `{process_name}` object. All three
/// collapse to a token that `timers::resolve_processes` looks up against the
/// live registry (a numeric token is tried as an id first, then as a name).
fn process_token(value: &Value) -> Result<String, CoordError> {
    match value {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Object(map) => map
            .get("process_id")
            .and_then(Value::as_i64)
            .map(|id| id.to_string())
            .or_else(|| {
                map.get("process_name")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .ok_or_else(|| {
                CoordError::Invalid(
                    "a `processes` object needs `process_id` or `process_name`".into(),
                )
            }),
        _ => Err(CoordError::Invalid(
            "`processes` items must be a process id, a name, or {process_id | process_name}".into(),
        )),
    }
}

fn process_tokens(args: &Args) -> Result<Vec<String>, CoordError> {
    match args.get("processes") {
        Some(Value::Array(items)) => items.iter().map(process_token).collect(),
        // The query-string transport carries a comma list.
        None | Some(Value::String(_)) => args.strings("processes"),
        Some(other) => process_token(other).map(|t| vec![t]),
    }
}

/// `POST /timers` — `timer_set`.
pub fn timer_set(state: &ControlState, params: &Params, query: &str, body: &str) -> Resp {
    run(query, body, |a| {
        let delay_ms = required_ms(a, "delay_ms")?;
        let text = a.req_str("body")?;
        // `loop: true` repeats every delay_ms; `repeat_every_ms` overrides
        // the interval (and implies repetition on its own).
        let repeat = match optional_ms(a, "repeat_every_ms")? {
            Some(ms) if ms > 0 => Some(ms),
            _ if a.bool("loop")? == Some(true) => Some(delay_ms),
            _ => None,
        };
        state.timers.set(
            &params.actor,
            project_scope(a, params)?,
            delivery_target(a, params)?,
            text,
            delay_ms,
            repeat,
            a.str("name").map(str::to_owned),
        )
    })
}

fn fire_when_idle(kind: TimerKind, state: &ControlState, params: &Params, query: &str, body: &str) -> Resp {
    run(query, body, |a| {
        let processes = process_tokens(a)?;
        let max_wait_ms = required_ms(a, "max_wait_ms")?;
        let text = a.req_str("body")?;
        state.timers.fire_when_idle(
            kind,
            &params.actor,
            project_scope(a, params)?,
            delivery_target(a, params)?,
            text,
            &processes,
            max_wait_ms,
            optional_ms(a, "idle_ms")?,
            optional_ms(a, "confirm_ms")?,
            a.bool("rearm")?,
            a.str("name").map(str::to_owned),
        )
    })
}

pub fn timer_fire_when_idle_any(state: &ControlState, params: &Params, query: &str, body: &str) -> Resp {
    fire_when_idle(TimerKind::IdleAny, state, params, query, body)
}

pub fn timer_fire_when_idle_all(state: &ControlState, params: &Params, query: &str, body: &str) -> Resp {
    fire_when_idle(TimerKind::IdleAll, state, params, query, body)
}

/// `GET /timers` — `timer_list`.
pub fn timer_list(state: &ControlState, params: &Params, query: &str, body: &str) -> Resp {
    run(query, body, |a| {
        state.timers.list(
            &params.actor,
            a.bool("include_fired")?.unwrap_or(false),
            a.bool("all")?.unwrap_or(false),
            a.usize("limit")?,
            a.usize("offset")?,
            project_scope(a, params)?,
        )
    })
}

fn lifecycle(op: Lifecycle, state: &ControlState, params: &Params, query: &str, body: &str) -> Resp {
    run(query, body, |_: &Args| {
        state.timers.lifecycle(&params.actor, record(params)?, op)
    })
}

pub fn timer_cancel(state: &ControlState, params: &Params, query: &str, body: &str) -> Resp {
    lifecycle(Lifecycle::Cancel, state, params, query, body)
}

pub fn timer_pause(state: &ControlState, params: &Params, query: &str, body: &str) -> Resp {
    lifecycle(Lifecycle::Pause, state, params, query, body)
}

pub fn timer_resume(state: &ControlState, params: &Params, query: &str, body: &str) -> Resp {
    lifecycle(Lifecycle::Resume, state, params, query, body)
}

/// Shared by the doc emitter: the one place the timer surface's prose lives.
pub const DOC: &str = "Timers wake an agent with a body injected VERBATIM as a fresh user turn, through the agent \
delivery path (ready gate → ONE atomic write of text + `\\r` → receipt) under the QUIET-ONLY policy: a \
timer body waits for genuine quiet up to `timer_delivery_timeout_ms` and is never typed into an agent \
mid-turn on the spawn prompt's ready-by-timeout fallback. They persist in `coordination.db` \
(`timers` / `timer_firings`, schema version 3), so an app restart does not lose a pending wake-up.\n\n\
**Targets are pinned by process UUID.** Registry ids restart at 1 every launch, so a timer stores \
`(id, uuid)` for its delivery target (which must be LIVE when the timer is set) and for every watched \
process. A target whose uuid no longer matches is GONE: a delay timer (repeating or not) whose target \
is gone at fire time `expired`s with `{delivered: false, error: \"process gone\"}`, and nothing is ever \
typed into a shell that reused the number.\n\n\
**Idle comes from the BYTE STREAM only**: a process is idle when `child_alive` and \
`now - last_output_at >= idle_ms` (default `idle_threshold_ms`, 120 000 — the \"trust idle only past \
120 s\" rule made server-side). Never from render diffs; a process that has produced no output at all \
is booting, not idle.\n\n\
**Fire-time re-validation**: a met idle condition does NOT fire. The timer enters `confirming` \
and is re-checked after `confirm_ms` (default 5 000); bytes in that window put it back to `pending` \
and, with `rearm: true` (the default), it keeps waiting. `max_wait_ms` is a hard deadline that fires \
the body with `reason: \"deadline\"` regardless.\n\n\
**Every firing is auditable**: `timer_list(include_fired: true)` returns fired timers for \
`timer_retention_hours` (default 24) with `delivery: {firing_id, process_id, status: in_flight | \
delivered | failed | coalesced, delivered: true | false | null, receipt, reason, error, coalesced_with, \
at}`. The audit row is written IN FLIGHT before the body is queued and finished with the receipt; a body \
that could not be delivered is `delivered: false` with the reason in `error` (`process gone`, \
`not ready within …`, `interrupted by a restart …`) — never silently dropped. The list is pending first, \
newest first, paged by `limit` + `offset` with the unpaged `total`.\n\n\
**Duplicate suppression**: an identical body to the same process within `timer_dedupe_ms` \
(default 5 000) — delivered, OR still in flight — is coalesced: `status: \"coalesced\", delivered: null, \
coalesced_with: <firing id>` instead of re-typed. A repeating timer never queues a second body while its \
previous firing is still in flight, and never coalesces against its own COMPLETED firing.\n\n\
On startup, PENDING absolute-time timers whose fire time passed while the app was down are advanced \
ONCE and a `reason: \"missed\"` firing is recorded in flight (a repeating one then continues on a fresh \
interval — never a burst of catch-up firings). It is delivered only if the SAME spawn (by uuid) is live \
within `timer_missed_grace_ms` (120 000); otherwise the row closes as `{delivered: false, error: \
\"process gone\", reason: \"missed\"}` without typing anywhere — after a relaunch that is the normal \
outcome, and the audit row is what an orchestrator sees. Idle timers resume watching. An idle timer \
whose every watched process is gone stops with `status: \"expired\"` and an audit row \
`{delivered: false, error: \"process gone\"}` rather than claiming a process went quiet.\n";
