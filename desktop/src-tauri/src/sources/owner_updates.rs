//! Owner updates: telling an overlay source when a row it adds to changed.
//!
//! A source that adds to another source's Work rows (a stack's workspaces
//! on a tracker's items) learns the tracker's side of an item from the row
//! the app sends with each call (`target.item`). A source that declares
//! `ownerUpdates` in its manifest is also told without waiting for a call:
//! after each Work poll of an owning source, every row that a TRUSTED
//! overlay source declaring it adds to, and whose content changed since it
//! was last sent there, is posted as `{source, key, item}` (the owner's
//! manifest id, the key, the row exactly as received).
//!
//! - Only rows whose content changed are sent (compared by a hash of the
//!   row as received, per overlay source and row); a source restarted or
//!   trusted again gets them all once more.
//! - Sends are debounced: what a poll queues waits [`DEBOUNCE`], a row
//!   queued twice meanwhile goes once with its latest content, and a batch
//!   goes out off the poll thread.
//! - A `2xx` is all the app looks at. Anything else is logged and dropped:
//!   the row is not sent again until it changes, so a failing source is
//!   never hammered in a loop.
//!
//! Everything here is pure, so the rules are tested without a socket.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::{json, Value};

use super::actions::{overlay_ref, owned_row};

/// How long a queued row waits for more before the batch is sent.
pub const DEBOUNCE: Duration = Duration::from_millis(500);

/// One overlay source as an owner poll sees it: whether it may be told
/// (trusted, declares `ownerUpdates`), its last Work payload, and the hash
/// of each row last sent to it (keyed by [`sent_key`]).
pub struct OverlaySide<'a> {
    /// The registry id.
    pub id: &'a str,
    pub accepts: bool,
    pub work: Option<&'a Value>,
    pub sent: &'a BTreeMap<String, String>,
}

/// A row to send.
#[derive(Debug, Clone, PartialEq)]
pub struct OwnerPush {
    /// The overlay source's registry id.
    pub target: String,
    /// The owning source's manifest id.
    pub owner: String,
    pub key: String,
    pub item: Value,
    pub hash: String,
}

impl OwnerPush {
    /// The body posted to the overlay source.
    pub fn body(&self) -> Value {
        json!({ "source": self.owner, "key": self.key, "item": self.item })
    }
}

/// How a row is filed in an overlay source's sent hashes.
pub fn sent_key(owner: &str, key: &str) -> String {
    format!("{owner}\u{1f}{key}")
}

/// The content hash a row is compared by.
pub fn row_hash(row: &Value) -> String {
    project_model::hash::content_hash(row.to_string().as_bytes())
}

/// The keys of `owner`'s items that an overlay's Work payload adds to, in
/// payload order, each once.
pub fn keys_added_to(work: &Value, owner: &str) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for item in work.get("items").and_then(Value::as_array).into_iter().flatten() {
        if let Some((source, key)) = overlay_ref(item) {
            if source == owner && !keys.iter().any(|k| k == key) {
                keys.push(key.to_owned());
            }
        }
    }
    keys
}

/// What one Work poll of the source whose manifest id is `owner` (payload
/// `owner_work`) must send: each row of it that an overlay that may be told
/// adds to, when it is not what was last sent there. A row the owner does
/// not list (any more) is not sent.
pub fn pushes_after_poll(owner: &str, owner_work: &Value, overlays: &[OverlaySide<'_>]) -> Vec<OwnerPush> {
    let mut out = Vec::new();
    for side in overlays.iter().filter(|s| s.accepts) {
        let Some(work) = side.work else { continue };
        for key in keys_added_to(work, owner) {
            let Some(row) = owned_row(owner_work, "work", &key) else { continue };
            let hash = row_hash(row);
            if side.sent.get(&sent_key(owner, &key)) == Some(&hash) {
                continue;
            }
            out.push(OwnerPush { target: side.id.to_owned(), owner: owner.to_owned(), key, item: row.clone(), hash });
        }
    }
    out
}

/// The rows waiting for the next send, one per (overlay source, owner,
/// key): a row queued again before the send replaces its older content.
#[derive(Debug, Default)]
pub struct PushQueue {
    pending: BTreeMap<(String, String, String), OwnerPush>,
    /// A sender is waiting out [`DEBOUNCE`] and will take the queue.
    scheduled: bool,
}

impl PushQueue {
    /// Queue `push`. Answers whether a sender must be started (none is
    /// waiting yet).
    pub fn add(&mut self, push: OwnerPush) -> bool {
        self.pending.insert((push.target.clone(), push.owner.clone(), push.key.clone()), push);
        let start = !self.scheduled;
        self.scheduled = true;
        start
    }

    /// Everything queued, for the sender; the next add starts a new one.
    pub fn take(&mut self) -> Vec<OwnerPush> {
        self.scheduled = false;
        std::mem::take(&mut self.pending).into_values().collect()
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner_work(status: &str) -> Value {
        json!({ "items": [
            { "key": "ABC-1", "title": "Fix login", "status": { "label": status } },
            { "key": "ABC-2", "title": "Other" },
            { "ref": { "source": "elsewhere", "key": "ABC-3" }, "key": "ABC-3" },
        ]})
    }

    fn overlay_work() -> Value {
        json!({ "items": [
            { "ref": { "source": "tracker", "key": "ABC-1" }, "folder": "/srv/example/abc-1" },
            { "ref": { "source": "tracker", "key": "ABC-1" } },
            { "ref": { "source": "tracker", "key": "ABC-9" } },
            { "ref": { "source": "other", "key": "ABC-2" } },
            { "ref": { "source": "tracker", "key": "ABC-3" } },
        ]})
    }

    #[test]
    fn only_rows_an_overlay_adds_to_that_changed_are_sent_and_only_where_they_may_be() {
        let overlay = overlay_work();
        assert_eq!(keys_added_to(&overlay, "tracker"), ["ABC-1", "ABC-9", "ABC-3"]);
        let none = BTreeMap::new();
        let first = pushes_after_poll(
            "tracker",
            &owner_work("In Progress"),
            &[OverlaySide { id: "ws", accepts: true, work: Some(&overlay), sent: &none }],
        );
        // ABC-1 once (two overlay entries name it); ABC-9 is not listed by
        // the owner; ABC-3 is an overlay itself in the owner's payload.
        assert_eq!(first.len(), 1);
        assert_eq!((first[0].target.as_str(), first[0].owner.as_str(), first[0].key.as_str()), ("ws", "tracker", "ABC-1"));
        assert_eq!(first[0].item["status"]["label"], "In Progress");
        assert_eq!(first[0].body(), json!({ "source": "tracker", "key": "ABC-1", "item": owner_work("In Progress")["items"][0] }));

        // Sent once: the same row again is not sent; a change is.
        let mut sent = BTreeMap::new();
        sent.insert(sent_key("tracker", "ABC-1"), first[0].hash.clone());
        let again = pushes_after_poll("tracker", &owner_work("In Progress"), &[OverlaySide { id: "ws", accepts: true, work: Some(&overlay), sent: &sent }]);
        assert!(again.is_empty());
        let moved = pushes_after_poll("tracker", &owner_work("Code Review"), &[OverlaySide { id: "ws", accepts: true, work: Some(&overlay), sent: &sent }]);
        assert_eq!(moved.len(), 1);
        assert_ne!(moved[0].hash, first[0].hash);

        // Not to a source that may not be told (untrusted, or no
        // `ownerUpdates`), nor one with no Work payload yet.
        for side in [
            OverlaySide { id: "ws", accepts: false, work: Some(&overlay), sent: &none },
            OverlaySide { id: "ws", accepts: true, work: None, sent: &none },
        ] {
            assert!(pushes_after_poll("tracker", &owner_work("Done"), &[side]).is_empty());
        }
        // Another owner's poll sends what the overlay adds to of ITS rows.
        let other = pushes_after_poll("other", &owner_work("Done"), &[OverlaySide { id: "ws", accepts: true, work: Some(&overlay), sent: &none }]);
        assert_eq!(other.iter().map(|p| (p.owner.as_str(), p.key.as_str())).collect::<Vec<_>>(), [("other", "ABC-2")]);
        assert!(pushes_after_poll("nobody", &owner_work("Done"), &[OverlaySide { id: "ws", accepts: true, work: Some(&overlay), sent: &none }]).is_empty());
    }

    #[test]
    fn a_row_queued_twice_before_the_send_goes_once_with_its_latest_content() {
        let push = |key: &str, status: &str| OwnerPush {
            target: "ws".into(),
            owner: "tracker".into(),
            key: key.into(),
            item: json!({ "key": key, "status": { "label": status } }),
            hash: status.into(),
        };
        let mut q = PushQueue::default();
        assert!(q.add(push("ABC-1", "To Do")), "the first add starts a sender");
        assert!(!q.add(push("ABC-1", "Doing")), "one is waiting already");
        assert!(!q.add(push("ABC-2", "To Do")));
        assert_eq!(q.len(), 2);
        let batch = q.take();
        assert_eq!(batch.len(), 2);
        assert_eq!(batch.iter().find(|p| p.key == "ABC-1").unwrap().item["status"]["label"], "Doing");
        assert!(q.is_empty());
        assert!(q.add(push("ABC-1", "Done")), "after a send, the next add starts another");
    }
}
