//! An item's local status on its Work overlay: the `localStatus` block, the
//! move actions it names, the banner when the tracker's status means
//! another state, and accepting the moves. The rules themselves are
//! [`stack_core::workflow`]'s; this module is how the source serves them.
//!
//! | action | does |
//! |---|---|
//! | `move:<state>` | moves the item (`params: {from, to}`); refused when the workflow has no such move from where it is, when it leaves a `human-exit` state and the caller is not a person, or when the move needs a note and has none. Into a `human` state from anyone but a person, it waits for approval instead |
//! | `approve` | a person makes the waiting move (`params: {to}`) |
//! | `send-back` | a person declines it, with a note; the item stays where it is |
//!
//! Who called is the `X-Chappa-Actor` header, kept as the app writes it.
//! The tracker's status comes from the owner row the app sends with every
//! overlay call (`target.item`) and from Start work; the stack never asks
//! the tracker and never writes to it.

use serde_json::{json, Value};
use std::path::Path;

use stack_core::definition::WorkflowDef;
use stack_core::workflow::{
    self, clean_actor, clean_note, local_status, move_id, tracker_mismatch, Caller, Outcome,
    Request, WorkflowRecord, APPROVE_ID, MOVE_PREFIX, NO_ACTOR, SEND_BACK_ID,
};
use stack_core::workspace::ItemRecord;

use crate::project::Loaded;
use crate::workspaces::{endpoint, Accepted};

/// The banner id of a tracker / local status mismatch.
pub const MISMATCH_BANNER: &str = "tracker-status";

/// Whether `id` is one of the workflow's actions.
pub fn handles(id: &str) -> bool {
    id.starts_with(MOVE_PREFIX) || id == APPROVE_ID || id == SEND_BACK_ID || id == "move"
}

/// Where `rec` stands. An item made before the stack had a workflow has no
/// record yet: it stands where it would have started, since it was created.
pub fn current(def: &WorkflowDef, rec: &ItemRecord) -> Option<WorkflowRecord> {
    rec.workflow.clone().or_else(|| {
        workflow::start(
            def,
            &rec.item_ref.source,
            rec.status.as_ref().map(|s| s.label.as_str()),
            NO_ACTOR,
            &rec.created_utc,
        )
    })
}

/// The record a new item starts with, or none without a workflow.
pub fn start_record(
    loaded: &Loaded,
    rec: &ItemRecord,
    actor: Option<&str>,
    now_utc: &str,
) -> Option<WorkflowRecord> {
    let def = loaded.stack.workflow.as_ref()?;
    workflow::start(
        def,
        &rec.item_ref.source,
        rec.status.as_ref().map(|s| s.label.as_str()),
        &clean_actor(actor),
        now_utc,
    )
}

fn move_action(
    def: &WorkflowDef,
    key: &str,
    wrec: &WorkflowRecord,
    to: &str,
    label: &str,
) -> Value {
    let mut a = endpoint(
        &move_id(to),
        label,
        true,
        Some(format!(
            "Move {key} from {} to {}",
            def.label_of(&wrec.state),
            def.label_of(to)
        )),
        json!({ "from": wrec.state, "to": to }),
    );
    a["note"] = json!(if def.needs_note(&wrec.state, to) {
        "required"
    } else {
        "optional"
    });
    a
}

/// What the overlay of `rec` gets from the workflow.
#[derive(Debug, Default)]
pub struct OverlayParts {
    pub local_status: Option<Value>,
    pub actions: Vec<Value>,
    pub banners: Vec<Value>,
}

/// The `localStatus` block, its actions, and the mismatch banner. Nothing
/// at all when the stack has no workflow.
pub fn overlay_parts(loaded: &Loaded, rec: &ItemRecord) -> OverlayParts {
    let Some(def) = loaded.stack.workflow.as_ref() else {
        return OverlayParts::default();
    };
    let Some(wrec) = current(def, rec) else {
        return OverlayParts::default();
    };
    let key = rec.item_ref.key.as_str();
    let mut actions = Vec::new();
    if let Some(p) = &wrec.pending {
        let mut approve = endpoint(
            APPROVE_ID,
            &format!("Approve: move to {}", def.label_of(&p.to)),
            true,
            Some(format!(
                "Approve moving {key} from {} to {} (asked by {})",
                def.label_of(&wrec.state),
                def.label_of(&p.to),
                p.by
            )),
            json!({ "to": p.to }),
        );
        approve["note"] = json!("optional");
        let mut back = endpoint(
            SEND_BACK_ID,
            "Send back",
            true,
            Some(format!(
                "Send back the move of {key} to {} (asked by {}); it stays {}",
                def.label_of(&p.to),
                p.by,
                def.label_of(&wrec.state)
            )),
            json!({ "to": p.to }),
        );
        back["note"] = json!("required");
        actions.push(approve);
        actions.push(back);
    }
    for to in workflow::allowed_targets(def, &wrec) {
        actions.push(move_action(
            def,
            key,
            &wrec,
            &to,
            &workflow::move_label(def, &to),
        ));
    }
    let mut banners = Vec::new();
    if let Some(status) = &rec.status {
        if let Some(m) = tracker_mismatch(def, &rec.item_ref.source, &status.label, &wrec) {
            let mut banner = json!({
                "id": MISMATCH_BANNER,
                "tone": "warn",
                "title": format!("Tracker: {} · here: {}", m.tracker, m.here),
                "lines": [format!("The tracker's status means {} here.", m.mapped_label)],
                "actions": [],
            });
            if m.can_move {
                banner["actions"] = json!([move_action(
                    def,
                    key,
                    &wrec,
                    &m.mapped,
                    &format!("Move here to {}", m.mapped_label)
                )]);
            } else {
                banner["lines"].as_array_mut().unwrap().push(json!(format!(
                    "The workflow has no move from {} to {}.",
                    m.here, m.mapped_label
                )));
            }
            banners.push(banner);
        }
    }
    OverlayParts {
        local_status: Some(serde_json::to_value(local_status(def, &wrec)).expect("serializes")),
        actions,
        banners,
    }
}

fn refused(kind: &'static str, message: impl Into<String>) -> Accepted {
    Accepted::Refused(409, kind, message.into())
}

/// Accept (or refuse) a workflow action on `rec`, made by `actor` (the
/// `X-Chappa-Actor` header) with `note`. A move made is saved in
/// `item.json` at once.
#[allow(clippy::too_many_arguments)]
pub fn accept(
    loaded: &Loaded,
    dir: &Path,
    rec: &mut ItemRecord,
    id: &str,
    params: &Value,
    note: Option<&str>,
    actor: Option<&str>,
    confirmed: bool,
    now_utc: &str,
) -> Accepted {
    let Some(def) = loaded.stack.workflow.as_ref() else {
        return refused("not-offered", "this stack has no workflow");
    };
    let Some(mut wrec) = current(def, rec) else {
        return refused("not-offered", "this stack's workflow has no states");
    };
    let req = if let Some(to) = id.strip_prefix(MOVE_PREFIX) {
        Request::Move {
            to: to.to_string(),
            from: params
                .get("from")
                .and_then(Value::as_str)
                .map(str::to_string),
        }
    } else if id == "move" {
        let Some(to) = params.get("to").and_then(Value::as_str) else {
            return refused("not-offered", "a move names its state in `params.to`");
        };
        Request::Move {
            to: to.to_string(),
            from: params
                .get("from")
                .and_then(Value::as_str)
                .map(str::to_string),
        }
    } else if id == APPROVE_ID {
        Request::Approve
    } else {
        Request::SendBack
    };
    if !confirmed {
        return refused(
            "confirmation-required",
            "a mutating action must be confirmed",
        );
    }
    // Approve / send back name the move they were shown: a newer request
    // in between makes them stale.
    if matches!(req, Request::Approve | Request::SendBack) {
        if let (Some(p), Some(shown)) = (&wrec.pending, params.get("to").and_then(Value::as_str)) {
            if p.to != shown {
                return refused(
                    "stale-plan",
                    format!(
                        "the move waiting for approval is now to {} (asked by {})",
                        def.label_of(&p.to),
                        p.by
                    ),
                );
            }
        }
    }
    let actor = clean_actor(actor);
    let note = clean_note(note);
    let key = rec.item_ref.key.clone();
    let caller = Caller {
        actor: &actor,
        note: note.as_deref(),
    };
    let outcome = workflow::apply(def, &mut wrec, &key, &req, &caller, now_utc);
    if outcome.is_ok() {
        rec.workflow = Some(wrec.clone());
        rec.updated_utc = now_utc.to_string();
        if let Err(e) = rec.save(dir) {
            return Accepted::Refused(500, "workspace", e);
        }
    }
    let label = |id: &str| def.label_of(id).to_string();
    match outcome {
        Err(r) => refused(r.kind, r.message),
        Ok(Outcome::Moved { from, to }) => {
            Accepted::Done(format!("{key}: {} → {}", label(&from), label(&to)))
        }
        Ok(Outcome::Pending { to, .. }) => Accepted::Done(format!(
            "{key}: the move to {} waits for a person's approval",
            label(&to)
        )),
        Ok(Outcome::Approved {
            from,
            to,
            requested_by,
        }) => Accepted::Done(format!(
            "{key}: {} → {} (approved; asked by {requested_by})",
            label(&from),
            label(&to)
        )),
        Ok(Outcome::SentBack { to, requested_by }) => Accepted::Done(format!(
            "{key}: sent back the move to {} asked by {requested_by}; it stays {}",
            label(&to),
            def.label_of(&wrec.state)
        )),
    }
}
