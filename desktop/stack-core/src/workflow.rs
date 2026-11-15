//! The local workflow of a work item: where it stands in the stack's own
//! status flow, who moved it there, the move waiting for a person's
//! approval, and the history. Kept in the item's `item.json`; the rules
//! come from the definition's `workflow:` block
//! ([`crate::definition::WorkflowDef`]).
//!
//! **Gates.** A state with gate `human` may be entered only by a person;
//! anyone else asking to move an item there leaves the move *pending*
//! until a person approves it or sends it back. A state with gate
//! `human-exit` may be entered by anyone and left only by a person; anyone
//! else is refused. Who is a person is the caller's word: an actor that
//! starts with `human:` (the app marks the calls its UI makes that way,
//! agents' calls are `agent:<id>`). A missing actor is not a person. This
//! is a guard against an agent moving its own work past a review, not
//! authentication.
//!
//! **Rounds.** An item's round in a state counts how often it entered that
//! state: review, back to development, review again is round 2 of review.
//!
//! **Trackers.** A tracker's status label maps (read-only) to a local
//! state; [`tracker_mismatch`] says when the two disagree, so the Work row
//! can offer the move. Nothing here ever writes to a tracker.
//!
//! Everything here is pure: no files, no clock, no network.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::definition::{Gate, WorkflowDef};

/// The actor prefix of a call a person made.
pub const HUMAN_PREFIX: &str = "human:";
/// The longest actor name kept; longer ones are cut.
pub const ACTOR_CAP: usize = 80;
/// The longest note kept.
pub const NOTE_CAP: usize = 2000;
/// The most history entries kept per item (the oldest go first).
pub const HISTORY_CAP: usize = 200;
/// The actor of a state chosen because the tracker's status maps to it.
pub const TRACKER_ACTOR: &str = "tracker";
/// The actor of a call that named none.
pub const NO_ACTOR: &str = "unknown";
/// Move action ids: `move:<state id>`. Ids are unique per row, so each
/// target state gets its own.
pub const MOVE_PREFIX: &str = "move:";
/// Approve the pending move (people only).
pub const APPROVE_ID: &str = "approve";
/// Decline the pending move; the item stays where it is (people only).
pub const SEND_BACK_ID: &str = "send-back";

/// The action id of a move to `to`.
pub fn move_id(to: &str) -> String {
    format!("{MOVE_PREFIX}{to}")
}

/// An actor name as kept: printable ASCII only (anything else becomes
/// `_`, as the app writes it), trimmed, at most [`ACTOR_CAP`] characters.
/// No name or an empty one is [`NO_ACTOR`].
pub fn clean_actor(raw: Option<&str>) -> String {
    let cleaned: String = raw
        .unwrap_or("")
        .chars()
        .map(|c| if (' '..='~').contains(&c) { c } else { '_' })
        .collect::<String>()
        .trim()
        .chars()
        .take(ACTOR_CAP)
        .collect();
    if cleaned.is_empty() {
        NO_ACTOR.to_string()
    } else {
        cleaned
    }
}

/// Whether `actor` (as kept) says a person made the call.
pub fn is_human(actor: &str) -> bool {
    actor
        .strip_prefix(HUMAN_PREFIX)
        .is_some_and(|name| !name.trim().is_empty())
}

/// A note as kept: control characters other than line breaks removed,
/// trimmed, capped. Empty is none.
pub fn clean_note(raw: Option<&str>) -> Option<String> {
    let kept: String = raw
        .unwrap_or("")
        .chars()
        .filter(|c| *c == '\n' || !c.is_control())
        .collect::<String>()
        .trim()
        .chars()
        .take(NOTE_CAP)
        .collect();
    (!kept.is_empty()).then_some(kept)
}

// ------------------------------------------------------------- the record

/// `item.json`'s `workflow` field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowRecord {
    /// The current state's id.
    pub state: String,
    /// When the item entered it.
    pub since_utc: String,
    /// Who moved it there.
    pub actor: String,
    /// How often the item has entered the current state (1 the first time).
    pub round: u32,
    /// Entries per state id, which is what [`Self::round`] is read from.
    #[serde(default)]
    pub visits: IndexMap<String, u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<PendingMove>,
    /// Oldest first.
    #[serde(default)]
    pub history: Vec<HistoryEntry>,
}

/// A move into a `human` state that someone else asked for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingMove {
    pub from: String,
    pub to: String,
    pub by: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub at_utc: String,
}

/// One entry of the history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    pub at_utc: String,
    /// None for the entry that started the item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// None for a declined move (the item did not move).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    pub actor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Who asked for the move a person then approved or declined.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_by: Option<String>,
    /// The state a declined move would have entered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declined: Option<String>,
}

/// The state a new item starts in, and who chose it: the state the
/// tracker's status maps to (actor [`TRACKER_ACTOR`]) when it maps to one,
/// else the first state, chosen by `actor`. None when the workflow has no
/// states.
pub fn start(
    def: &WorkflowDef,
    source: &str,
    status_label: Option<&str>,
    actor: &str,
    now_utc: &str,
) -> Option<WorkflowRecord> {
    let (state, actor) = match status_label.and_then(|l| def.map_status(source, l)) {
        Some(mapped) => (mapped.to_string(), TRACKER_ACTOR.to_string()),
        None => (def.initial()?.id.clone(), actor.to_string()),
    };
    Some(WorkflowRecord {
        visits: IndexMap::from([(state.clone(), 1)]),
        history: vec![HistoryEntry {
            at_utc: now_utc.to_string(),
            from: None,
            to: Some(state.clone()),
            actor: actor.clone(),
            note: None,
            requested_by: None,
            declined: None,
        }],
        state,
        since_utc: now_utc.to_string(),
        actor,
        round: 1,
        pending: None,
    })
}

/// The states an item may move to from where it is. A state the
/// definition no longer has can only go back to the first state, so no
/// item is stranded by an edit of the workflow.
pub fn allowed_targets(def: &WorkflowDef, rec: &WorkflowRecord) -> Vec<String> {
    if def.state(&rec.state).is_none() {
        return def
            .initial()
            .map(|s| vec![s.id.clone()])
            .unwrap_or_default();
    }
    def.moves_from(&rec.state)
        .iter()
        .filter(|to| def.state(to).is_some())
        .cloned()
        .collect()
}

fn gate_of(def: &WorkflowDef, id: &str) -> Option<Gate> {
    def.state(id).and_then(|s| s.gate())
}

/// Whether leaving the current state needs a person.
pub fn exit_needs_human(def: &WorkflowDef, rec: &WorkflowRecord) -> bool {
    gate_of(def, &rec.state) == Some(Gate::HumanExit)
}

// ------------------------------------------------------------- the moves

/// What a call asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Move to this state. `from`, when given, is the state the caller saw
    /// the item in: a different current state refuses the call as stale.
    Move {
        to: String,
        from: Option<String>,
    },
    Approve,
    SendBack,
}

/// What happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Moved {
        from: String,
        to: String,
    },
    /// Waiting for a person: the move into a `human` state was asked for by
    /// someone else.
    Pending {
        from: String,
        to: String,
    },
    Approved {
        from: String,
        to: String,
        requested_by: String,
    },
    SentBack {
        to: String,
        requested_by: String,
    },
}

/// Why a call was refused; always a conflict with the workflow's rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// `not-offered`, `stale-plan`, `not-allowed`, `human-only`, `note-required`.
    pub kind: &'static str,
    pub message: String,
}

fn refuse(kind: &'static str, message: impl Into<String>) -> Refusal {
    Refusal {
        kind,
        message: message.into(),
    }
}

/// The caller of a move, as kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caller<'a> {
    /// [`clean_actor`]'s result.
    pub actor: &'a str,
    /// [`clean_note`]'s result.
    pub note: Option<&'a str>,
}

/// Apply `req` to `rec` under `def`'s rules. `key` names the item in
/// messages. On a refusal `rec` is unchanged.
pub fn apply(
    def: &WorkflowDef,
    rec: &mut WorkflowRecord,
    key: &str,
    req: &Request,
    caller: &Caller<'_>,
    now_utc: &str,
) -> Result<Outcome, Refusal> {
    let human = is_human(caller.actor);
    let here = def.label_of(&rec.state).to_string();
    match req {
        Request::Move { to, from } => {
            let Some(target) = def.state(to) else {
                return Err(refuse(
                    "not-offered",
                    format!("the workflow has no state `{to}`"),
                ));
            };
            if from.as_deref().is_some_and(|f| f != rec.state) {
                return Err(refuse(
                    "stale-plan",
                    format!(
                        "{key} is {here} now, not {}",
                        def.label_of(from.as_deref().unwrap_or(""))
                    ),
                ));
            }
            let allowed = allowed_targets(def, rec);
            if !allowed.iter().any(|t| t == to) {
                let list: Vec<&str> = allowed.iter().map(|t| def.label_of(t)).collect();
                return Err(refuse(
                    "not-allowed",
                    if list.is_empty() {
                        format!(
                            "{key} cannot move from {here}; the workflow has no moves out of it"
                        )
                    } else {
                        format!(
                            "{key} cannot move from {here} to {}; from {here} it can move to {}",
                            target.label,
                            list.join(", ")
                        )
                    },
                ));
            }
            if exit_needs_human(def, rec) && !human {
                return Err(refuse(
                    "human-only",
                    format!(
                        "only a person may move {key} out of {here} (the call came from {})",
                        caller.actor
                    ),
                ));
            }
            if def.needs_note(&rec.state, to) && caller.note.is_none() {
                return Err(refuse(
                    "note-required",
                    format!(
                        "moving {key} from {here} to {} needs a note saying why",
                        target.label
                    ),
                ));
            }
            let from = rec.state.clone();
            if target.gate() == Some(Gate::Human) && !human {
                rec.pending = Some(PendingMove {
                    from: from.clone(),
                    to: to.clone(),
                    by: caller.actor.to_string(),
                    note: caller.note.map(str::to_string),
                    at_utc: now_utc.to_string(),
                });
                return Ok(Outcome::Pending {
                    from,
                    to: to.clone(),
                });
            }
            enter(rec, to, caller.actor, caller.note, None, now_utc);
            Ok(Outcome::Moved {
                from,
                to: to.clone(),
            })
        }
        Request::Approve | Request::SendBack => {
            let Some(pending) = rec.pending.clone() else {
                return Err(refuse(
                    "not-offered",
                    format!("no move of {key} is waiting for approval"),
                ));
            };
            if !human {
                return Err(refuse(
                    "human-only",
                    format!(
                        "only a person may approve or send back a move of {key} (the call came from {})",
                        caller.actor
                    ),
                ));
            }
            if *req == Request::SendBack {
                if caller.note.is_none() {
                    return Err(refuse(
                        "note-required",
                        format!("sending back the move of {key} needs a note saying why"),
                    ));
                }
                rec.pending = None;
                push(
                    rec,
                    HistoryEntry {
                        at_utc: now_utc.to_string(),
                        from: None,
                        to: None,
                        actor: caller.actor.to_string(),
                        note: caller.note.map(str::to_string),
                        requested_by: Some(pending.by.clone()),
                        declined: Some(pending.to.clone()),
                    },
                );
                return Ok(Outcome::SentBack {
                    to: pending.to,
                    requested_by: pending.by,
                });
            }
            // The definition may have changed since it was asked.
            if pending.from != rec.state || !allowed_targets(def, rec).contains(&pending.to) {
                return Err(refuse(
                    "stale-plan",
                    format!(
                        "the workflow no longer has the move of {key} from {here} to {}; send it back",
                        def.label_of(&pending.to)
                    ),
                ));
            }
            let note = caller.note.map(str::to_string).or(pending.note.clone());
            enter(
                rec,
                &pending.to,
                caller.actor,
                note.as_deref(),
                Some(&pending.by),
                now_utc,
            );
            Ok(Outcome::Approved {
                from: pending.from,
                to: pending.to,
                requested_by: pending.by,
            })
        }
    }
}

fn enter(
    rec: &mut WorkflowRecord,
    to: &str,
    actor: &str,
    note: Option<&str>,
    requested_by: Option<&str>,
    now_utc: &str,
) {
    let from = std::mem::replace(&mut rec.state, to.to_string());
    let visits = rec.visits.entry(to.to_string()).or_insert(0);
    *visits += 1;
    rec.round = *visits;
    rec.since_utc = now_utc.to_string();
    rec.actor = actor.to_string();
    rec.pending = None;
    push(
        rec,
        HistoryEntry {
            at_utc: now_utc.to_string(),
            from: Some(from),
            to: Some(to.to_string()),
            actor: actor.to_string(),
            note: note.map(str::to_string),
            requested_by: requested_by.map(str::to_string),
            declined: None,
        },
    );
}

fn push(rec: &mut WorkflowRecord, entry: HistoryEntry) {
    rec.history.push(entry);
    let extra = rec.history.len().saturating_sub(HISTORY_CAP);
    rec.history.drain(..extra);
}

// -------------------------------------------------------------- the view

/// The Work row's `localStatus`: what the app's status block shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalStatus {
    pub id: String,
    pub label: String,
    pub since: Option<String>,
    pub actor: Option<String>,
    pub round: Option<u32>,
    /// The current state's gate, as the definition says it.
    pub gate: Option<&'static str>,
    pub pending_approval: Option<PendingView>,
    pub allowed: Vec<AllowedMove>,
    /// Oldest first.
    pub history: Vec<HistoryView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingView {
    /// The target state's label.
    pub to: String,
    pub to_id: String,
    pub by: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// One move on offer. `id` is the endpoint action that makes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AllowedMove {
    pub id: String,
    pub label: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub human_only: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub require_note: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryView {
    pub at: String,
    /// Labels, for reading; the ids beside them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_id: Option<String>,
    pub actor: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// The label of the button that moves an item to `to`.
pub fn move_label(def: &WorkflowDef, to: &str) -> String {
    format!("Move to {}", def.label_of(to))
}

/// What the app shows for `rec`. The allowed list is the same for every
/// viewer (the view is not asked per caller): moves only a person may make
/// say so, and a person's call is what the action checks.
pub fn local_status(def: &WorkflowDef, rec: &WorkflowRecord) -> LocalStatus {
    let mut allowed = Vec::new();
    if let Some(p) = &rec.pending {
        allowed.push(AllowedMove {
            id: APPROVE_ID.to_string(),
            label: format!("Approve: move to {}", def.label_of(&p.to)),
            human_only: true,
            require_note: false,
        });
        allowed.push(AllowedMove {
            id: SEND_BACK_ID.to_string(),
            label: "Send back".to_string(),
            human_only: true,
            require_note: true,
        });
    }
    let leave_human = exit_needs_human(def, rec);
    for to in allowed_targets(def, rec) {
        allowed.push(AllowedMove {
            id: move_id(&to),
            label: move_label(def, &to),
            human_only: leave_human,
            require_note: def.needs_note(&rec.state, &to),
        });
    }
    let history = rec
        .history
        .iter()
        .map(|h| {
            let declined = h.declined.as_deref().map(|d| {
                let by = h
                    .requested_by
                    .as_deref()
                    .map(|b| format!(" asked by {b}"))
                    .unwrap_or_default();
                format!("sent back the move to {}{by}", def.label_of(d))
            });
            let note = match (declined, &h.note) {
                (Some(d), Some(n)) => Some(format!("{d}: {n}")),
                (Some(d), None) => Some(d),
                (None, Some(n)) => Some(match &h.requested_by {
                    Some(b) => format!("{n} (asked by {b})"),
                    None => n.clone(),
                }),
                (None, None) => h.requested_by.as_ref().map(|b| format!("asked by {b}")),
            };
            HistoryView {
                at: h.at_utc.clone(),
                from: h.from.as_deref().map(|f| def.label_of(f).to_string()),
                to: h.to.as_deref().map(|t| def.label_of(t).to_string()),
                from_id: h.from.clone(),
                to_id: h.to.clone(),
                actor: h.actor.clone(),
                note,
            }
        })
        .collect();
    LocalStatus {
        id: rec.state.clone(),
        label: match def.state(&rec.state) {
            Some(s) => s.label.clone(),
            None => format!("{} (not in the workflow)", rec.state),
        },
        since: Some(rec.since_utc.clone()),
        actor: Some(rec.actor.clone()),
        round: Some(rec.round),
        gate: gate_of(def, &rec.state).map(Gate::as_str),
        pending_approval: rec.pending.as_ref().map(|p| PendingView {
            to: def.label_of(&p.to).to_string(),
            to_id: p.to.clone(),
            by: p.by.clone(),
            note: p.note.clone(),
        }),
        allowed,
        history,
    }
}

/// The tracker says one thing, the workflow another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mismatch {
    /// The tracker's status label.
    pub tracker: String,
    /// The local state's label.
    pub here: String,
    /// The state the tracker's label maps to.
    pub mapped: String,
    pub mapped_label: String,
    /// Whether the workflow has that move from where the item is.
    pub can_move: bool,
}

/// When the tracker's status (`label`, from source `source`) maps to a
/// state other than the item's: what to tell the user. An unmapped label
/// says nothing.
pub fn tracker_mismatch(
    def: &WorkflowDef,
    source: &str,
    label: &str,
    rec: &WorkflowRecord,
) -> Option<Mismatch> {
    let mapped = def.map_status(source, label)?;
    if mapped == rec.state {
        return None;
    }
    Some(Mismatch {
        tracker: label.to_string(),
        here: def.label_of(&rec.state).to_string(),
        mapped: mapped.to_string(),
        mapped_label: def.label_of(mapped).to_string(),
        can_move: allowed_targets(def, rec).iter().any(|t| t == mapped),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::definition::StacksDocument;

    const WF: &str = r#"
stacks:
  app:
    workflow:
      states:
        - { id: todo,   label: To do }
        - { id: dev,    label: In development }
        - { id: review, label: In review, gate: human-exit }
        - { id: done,   label: Done,      gate: human }
      moves: { todo: [dev], dev: [review], review: [dev, done], done: [dev] }
      requireNote: [ "review->dev" ]
      trackers:
        tracker:
          map: { "To Do": todo, "In Progress": dev, "Code Review": review, "Done": done }
        "*":
          map: { "Doing": dev, "Code Review": todo }
"#;

    const T0: &str = "2026-11-14T12:00:00Z";
    const T1: &str = "2026-11-14T13:00:00Z";
    const HUMAN: &str = "human:alex";
    const AGENT: &str = "agent:a1";

    fn def() -> WorkflowDef {
        StacksDocument::parse(WF).unwrap().stacks["app"]
            .workflow
            .clone()
            .unwrap()
    }

    fn mv(to: &str) -> Request {
        Request::Move {
            to: to.into(),
            from: None,
        }
    }

    fn by<'a>(actor: &'a str, note: Option<&'a str>) -> Caller<'a> {
        Caller { actor, note }
    }

    fn at(def: &WorkflowDef, state: &str) -> WorkflowRecord {
        let mut r = start(def, "tracker", None, HUMAN, T0).unwrap();
        r.state = state.into();
        r.visits = IndexMap::from([(state.to_string(), 1)]);
        r
    }

    #[test]
    fn actors_are_kept_as_the_app_writes_them_and_only_human_prefixes_count() {
        assert_eq!(clean_actor(Some("human:alex")), "human:alex");
        assert_eq!(clean_actor(Some("human:\u{e9}lan\r\n")), "human:_lan__");
        assert_eq!(clean_actor(Some("   ")), NO_ACTOR);
        assert_eq!(clean_actor(None), NO_ACTOR);
        assert_eq!(clean_actor(Some(&"x".repeat(200))).len(), ACTOR_CAP);
        assert!(is_human("human:alex"));
        for not in [
            "agent:a1",
            "human:",
            "human: ",
            "Human:alex",
            NO_ACTOR,
            "source:x",
        ] {
            assert!(!is_human(not), "{not}");
        }
        assert_eq!(
            clean_note(Some("  why\u{7}\nbecause ")).as_deref(),
            Some("why\nbecause")
        );
        assert_eq!(clean_note(Some(" \t ")), None);
    }

    #[test]
    fn a_new_item_starts_in_the_first_state_or_where_the_tracker_maps_it() {
        let d = def();
        let r = start(&d, "tracker", Some("Brand new"), HUMAN, T0).unwrap();
        assert_eq!(
            (r.state.as_str(), r.actor.as_str(), r.round),
            ("todo", HUMAN, 1)
        );
        let r = start(&d, "tracker", Some("Code Review"), HUMAN, T0).unwrap();
        assert_eq!(
            (r.state.as_str(), r.actor.as_str()),
            ("review", TRACKER_ACTOR)
        );
        assert_eq!(r.history.len(), 1);
        assert_eq!(r.history[0].to.as_deref(), Some("review"));
        // The owner's own map first, then "*".
        let r = start(&d, "other", Some("Code Review"), HUMAN, T0).unwrap();
        assert_eq!(r.state, "todo");
        let r = start(&d, "tracker", Some("Doing"), HUMAN, T0).unwrap();
        assert_eq!(r.state, "dev");
        assert!(start(&WorkflowDef::default(), "tracker", None, HUMAN, T0).is_none());
    }

    #[test]
    fn moves_follow_the_moves_list_and_need_their_notes() {
        let d = def();
        let mut r = start(&d, "tracker", None, AGENT, T0).unwrap();
        let err = apply(&d, &mut r, "ABC-1", &mv("review"), &by(AGENT, None), T1).unwrap_err();
        assert_eq!(err.kind, "not-allowed");
        assert_eq!(
            err.message,
            "ABC-1 cannot move from To do to In review; from To do it can move to In development"
        );
        assert_eq!(r.state, "todo", "a refusal changes nothing");
        let err = apply(&d, &mut r, "ABC-1", &mv("nope"), &by(AGENT, None), T1).unwrap_err();
        assert_eq!(err.kind, "not-offered");
        let stale = Request::Move {
            to: "dev".into(),
            from: Some("review".into()),
        };
        assert_eq!(
            apply(&d, &mut r, "ABC-1", &stale, &by(AGENT, None), T1)
                .unwrap_err()
                .kind,
            "stale-plan"
        );
        assert_eq!(
            apply(&d, &mut r, "ABC-1", &mv("dev"), &by(AGENT, None), T1),
            Ok(Outcome::Moved {
                from: "todo".into(),
                to: "dev".into()
            })
        );
        assert_eq!((r.since_utc.as_str(), r.actor.as_str()), (T1, AGENT));
        apply(&d, &mut r, "ABC-1", &mv("review"), &by(AGENT, None), T1).unwrap();
        // review -> dev needs a note, and only a person may leave review.
        let err = apply(&d, &mut r, "ABC-1", &mv("dev"), &by(HUMAN, None), T1).unwrap_err();
        assert_eq!(err.kind, "note-required");
        apply(
            &d,
            &mut r,
            "ABC-1",
            &mv("dev"),
            &by(HUMAN, Some("tests fail")),
            T1,
        )
        .unwrap();
        let last = r.history.last().unwrap();
        assert_eq!(
            (
                last.from.as_deref(),
                last.to.as_deref(),
                last.actor.as_str(),
                last.note.as_deref()
            ),
            (Some("review"), Some("dev"), HUMAN, Some("tests fail"))
        );
    }

    #[test]
    fn only_a_person_leaves_a_human_exit_state() {
        let d = def();
        let mut r = at(&d, "review");
        for actor in [AGENT, NO_ACTOR, "source:x"] {
            let err = apply(&d, &mut r, "ABC-1", &mv("done"), &by(actor, None), T1).unwrap_err();
            assert_eq!(err.kind, "human-only", "{actor}");
            assert!(err.message.contains(actor), "{}", err.message);
        }
        assert_eq!(r.state, "review");
        assert!(r.pending.is_none());
        // Entering it is open to anyone.
        let mut r = at(&d, "dev");
        apply(&d, &mut r, "ABC-1", &mv("review"), &by(AGENT, None), T1).unwrap();
        assert_eq!(r.state, "review");
    }

    #[test]
    fn an_agent_entering_a_human_state_waits_for_a_person() {
        // review is human-exit, so an agent could not even ask; open it so
        // the only gate on the way into done is done's own.
        let mut open = def();
        open.states[2].gate = None;
        let mut r = at(&open, "review");
        assert_eq!(
            apply(
                &open,
                &mut r,
                "ABC-1",
                &mv("done"),
                &by(AGENT, Some("all green")),
                T1
            ),
            Ok(Outcome::Pending {
                from: "review".into(),
                to: "done".into()
            })
        );
        assert_eq!(r.state, "review");
        let p = r.pending.clone().unwrap();
        assert_eq!(
            (p.to.as_str(), p.by.as_str(), p.note.as_deref()),
            ("done", AGENT, Some("all green"))
        );

        // An agent can neither approve nor send back.
        for req in [Request::Approve, Request::SendBack] {
            let err = apply(&open, &mut r, "ABC-1", &req, &by(AGENT, Some("x")), T1).unwrap_err();
            assert_eq!(err.kind, "human-only");
        }
        // Send back needs a note, keeps the state, records the decline.
        let mut back = r.clone();
        assert_eq!(
            apply(
                &open,
                &mut back,
                "ABC-1",
                &Request::SendBack,
                &by(HUMAN, None),
                T1
            )
            .unwrap_err()
            .kind,
            "note-required"
        );
        apply(
            &open,
            &mut back,
            "ABC-1",
            &Request::SendBack,
            &by(HUMAN, Some("not yet")),
            T1,
        )
        .unwrap();
        assert_eq!(
            (back.state.as_str(), back.pending.is_none()),
            ("review", true)
        );
        let last = back.history.last().unwrap();
        assert_eq!(
            (
                last.declined.as_deref(),
                last.requested_by.as_deref(),
                last.to.as_deref()
            ),
            (Some("done"), Some(AGENT), None)
        );
        assert_eq!(
            apply(
                &open,
                &mut back,
                "ABC-1",
                &Request::Approve,
                &by(HUMAN, None),
                T1
            )
            .unwrap_err()
            .kind,
            "not-offered"
        );
        // Approve makes the move as the person, keeping the request's note.
        assert_eq!(
            apply(
                &open,
                &mut r,
                "ABC-1",
                &Request::Approve,
                &by(HUMAN, None),
                T1
            ),
            Ok(Outcome::Approved {
                from: "review".into(),
                to: "done".into(),
                requested_by: AGENT.into()
            })
        );
        assert_eq!(
            (r.state.as_str(), r.actor.as_str(), r.pending.is_none()),
            ("done", HUMAN, true)
        );
        let last = r.history.last().unwrap();
        assert_eq!(
            (last.note.as_deref(), last.requested_by.as_deref()),
            (Some("all green"), Some(AGENT))
        );

        // A person enters a human state directly.
        let mut r = at(&open, "review");
        apply(&open, &mut r, "ABC-1", &mv("done"), &by(HUMAN, None), T1).unwrap();
        assert_eq!(r.state, "done");
    }

    #[test]
    fn rounds_count_entries_into_the_state() {
        let d = def();
        let mut r = at(&d, "dev");
        let rounds: Vec<(String, u32)> = [
            ("review", None),
            ("dev", Some("again")),
            ("review", None),
            ("dev", Some("still")),
            ("review", None),
        ]
        .into_iter()
        .map(|(to, note)| {
            apply(&d, &mut r, "ABC-1", &mv(to), &by(HUMAN, note), T1).unwrap();
            (r.state.clone(), r.round)
        })
        .collect();
        assert_eq!(
            rounds,
            [
                ("review", 1),
                ("dev", 2),
                ("review", 2),
                ("dev", 3),
                ("review", 3)
            ]
            .map(|(s, n)| (s.to_string(), n))
        );
    }

    #[test]
    fn the_history_keeps_the_newest_entries() {
        let d = def();
        let mut r = at(&d, "dev");
        for _ in 0..HISTORY_CAP {
            apply(&d, &mut r, "ABC-1", &mv("review"), &by(HUMAN, None), T1).unwrap();
            apply(&d, &mut r, "ABC-1", &mv("dev"), &by(HUMAN, Some("n")), T1).unwrap();
        }
        assert_eq!(r.history.len(), HISTORY_CAP);
        assert_eq!(r.history.last().unwrap().to.as_deref(), Some("dev"));
        assert_eq!(r.round as usize, HISTORY_CAP + 1, "rounds survive the cap");
    }

    #[test]
    fn the_status_block_per_gate() {
        let d = def();
        let allowed = |r: &WorkflowRecord| -> Vec<(String, bool, bool)> {
            local_status(&d, r)
                .allowed
                .into_iter()
                .map(|a| (a.id, a.human_only, a.require_note))
                .collect()
        };
        let o = |id: &str, h: bool, n: bool| (id.to_string(), h, n);
        // No gate: anyone may make each move.
        let dev = at(&d, "dev");
        let ls = local_status(&d, &dev);
        assert_eq!((ls.gate, ls.label.as_str()), (None, "In development"));
        assert_eq!(allowed(&dev), [o("move:review", false, false)]);
        // human-exit: every move out is a person's.
        let review = at(&d, "review");
        assert_eq!(local_status(&d, &review).gate, Some("human-exit"));
        assert_eq!(
            allowed(&review),
            [o("move:dev", true, true), o("move:done", true, false)]
        );
        // human: entering was a person's; leaving is open.
        let done = at(&d, "done");
        assert_eq!(local_status(&d, &done).gate, Some("human"));
        assert_eq!(allowed(&done), [o("move:dev", false, false)]);
        // Waiting for approval: approve and send back first, people only.
        let mut open = d.clone();
        open.states[2].gate = None;
        let mut waiting = at(&open, "review");
        apply(
            &open,
            &mut waiting,
            "ABC-1",
            &mv("done"),
            &by(AGENT, Some("ready")),
            T1,
        )
        .unwrap();
        let ls = local_status(&open, &waiting);
        assert_eq!(
            serde_json::to_value(&ls).unwrap(),
            serde_json::json!({
                "id": "review", "label": "In review", "since": T0, "actor": HUMAN, "round": 1,
                "gate": null,
                "pendingApproval": { "to": "Done", "toId": "done", "by": AGENT, "note": "ready" },
                "allowed": [
                    { "id": "approve", "label": "Approve: move to Done", "humanOnly": true },
                    { "id": "send-back", "label": "Send back", "humanOnly": true, "requireNote": true },
                    { "id": "move:dev", "label": "Move to In development", "requireNote": true },
                    { "id": "move:done", "label": "Move to Done" }
                ],
                "history": [{ "at": T0, "to": "To do", "toId": "todo", "actor": HUMAN }]
            })
        );
        // A state the definition dropped can only go back to the start.
        let gone = at(&d, "qa");
        let ls = local_status(&d, &gone);
        assert_eq!(ls.label, "qa (not in the workflow)");
        assert_eq!(allowed(&gone), [o("move:todo", false, false)]);
    }

    #[test]
    fn the_tracker_mismatch_uses_the_owner_map_then_any() {
        let d = def();
        let r = at(&d, "dev");
        let m = tracker_mismatch(&d, "tracker", "Code Review", &r).unwrap();
        assert_eq!(
            (
                m.tracker.as_str(),
                m.here.as_str(),
                m.mapped.as_str(),
                m.mapped_label.as_str(),
                m.can_move
            ),
            ("Code Review", "In development", "review", "In review", true)
        );
        // Another source falls back to "*", where Code Review means todo.
        let m = tracker_mismatch(&d, "other", "Code Review", &r).unwrap();
        assert_eq!((m.mapped.as_str(), m.can_move), ("todo", false));
        assert_eq!(
            tracker_mismatch(&d, "tracker", "In Progress", &r),
            None,
            "a match"
        );
        assert_eq!(
            tracker_mismatch(&d, "tracker", "Blocked", &r),
            None,
            "unmapped"
        );
        assert_eq!(
            tracker_mismatch(&d, "tracker", "Doing", &r),
            None,
            "* maps it to dev too"
        );
    }
}
