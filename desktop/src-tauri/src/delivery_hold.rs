//! Deliveries held back from an agent until it is safe to make them.
//!
//! Text that reaches an agent's pane from something other than the person
//! at the keyboard (a `send_input` over the control surface or MCP, a timer
//! firing) is how an agent is told to keep working. When the agent works in
//! a folder next to data that may be real (see [`crate::data_safety`]),
//! such deliveries are held here: queued, not dropped and not sent, until
//! the gate lets them through, and then delivered in the order they came.
//! A delivery that arrives while earlier ones for the same pane are still
//! queued or being released queues behind them, so the order never changes.
//!
//! What the person types is never held: keystrokes and pastes go straight
//! to the pty through their own commands and never pass through here.
//!
//! The queue is in memory. A pane that closes drops what was held for it
//! (there is nothing left to deliver to), and so does an app restart; a
//! held timer firing then reads as interrupted, like any firing a restart
//! cuts short.
//!
//! Each held delivery carries what a list of them needs: an id that stays
//! the same while it is held (and is never reused in a run), who sent it,
//! when, and its first line ([`HeldEntry`], from [`DeliveryHolds::list`]).
//! [`DeliveryHolds::discard`] takes one out by id without delivering it.
//! Nothing shows the list yet: the app shows a count per agent; these are
//! the parts a "held messages" view with a discard button is built on.

use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use serde::Serialize;
use term_core::actor::epoch_ms;

/// The longest first line a [`HeldEntry`] carries.
pub const FIRST_LINE_CAP: usize = 120;

/// Decides whether deliveries to an agent working in `cwd` must wait, and
/// says why. The app's gate is the data-safety check; tests use their own.
pub trait HoldGate: Send + Sync {
    fn hold_reason(&self, cwd: &Path) -> Option<String>;
}

/// One held delivery: its id, who sent it, what it says (for the journal
/// and the marker), when it was queued, and the call that makes it.
pub struct Held {
    pub id: u64,
    pub from: String,
    pub text: String,
    pub queued_ms: u64,
    run: Box<dyn FnOnce() + Send>,
}

/// A held delivery as a list shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HeldEntry {
    /// Stable while it is held; never reused in this run.
    pub id: u64,
    /// The pane it waits for.
    pub pane: u32,
    /// Who sent it: the control-surface actor (`user`, or an agent's
    /// process id), or `timer <id>`.
    pub from: String,
    pub queued_ms: u64,
    /// Its first non-empty line, trimmed, at most [`FIRST_LINE_CAP`]
    /// characters (an ellipsis marks a cut).
    pub first_line: String,
}

/// The first non-empty line of `text`, trimmed and capped.
pub fn first_line(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    if line.chars().count() <= FIRST_LINE_CAP {
        line.to_owned()
    } else {
        let mut cut: String = line.chars().take(FIRST_LINE_CAP - 1).collect();
        cut.push('…');
        cut
    }
}

impl Held {
    fn entry(&self, pane: u32) -> HeldEntry {
        HeldEntry {
            id: self.id,
            pane,
            from: self.from.clone(),
            queued_ms: self.queued_ms,
            first_line: first_line(&self.text),
        }
    }
}

#[derive(Default)]
struct PaneQueue {
    items: VecDeque<Held>,
    /// A release is delivering this queue now: new deliveries still queue,
    /// so none overtakes one taken off the front a moment ago.
    releasing: bool,
}

#[derive(Default)]
struct Inner {
    gate: Option<Arc<dyn HoldGate>>,
    panes: BTreeMap<u32, PaneQueue>,
    /// The id the next held delivery gets.
    next_id: u64,
}

/// The held deliveries of every pane. Cheap to clone; the registry holds one.
#[derive(Clone, Default)]
pub struct DeliveryHolds {
    inner: Arc<Mutex<Inner>>,
}

impl DeliveryHolds {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn set_gate(&self, gate: Arc<dyn HoldGate>) {
        self.lock().gate = Some(gate);
    }

    fn gate(&self) -> Option<Arc<dyn HoldGate>> {
        self.lock().gate.clone()
    }

    /// Why a delivery to pane `id` (working in `cwd`) must wait now, or
    /// `None` to deliver it: the gate's reason, else, while earlier held
    /// deliveries for the pane are still queued, that it waits behind them.
    pub fn reason_for(&self, id: u32, cwd: Option<&Path>) -> Option<String> {
        if let (Some(gate), Some(dir)) = (self.gate(), cwd) {
            if let Some(why) = gate.hold_reason(dir) {
                return Some(why);
            }
        }
        let inner = self.lock();
        let q = inner.panes.get(&id)?;
        Some(format!(
            "held behind {} earlier message(s) for this agent",
            q.items.len().max(1)
        ))
    }

    /// Queue `run` for pane `id`, sent by `from`. Answers how many
    /// deliveries are now held for it.
    pub fn hold(
        &self,
        id: u32,
        from: String,
        text: String,
        run: Box<dyn FnOnce() + Send>,
    ) -> usize {
        let mut inner = self.lock();
        inner.next_id += 1;
        let held = Held {
            id: inner.next_id,
            from,
            text,
            queued_ms: epoch_ms(),
            run,
        };
        let q = inner.panes.entry(id).or_default();
        q.items.push_back(held);
        q.items.len()
    }

    /// How many deliveries are held per pane (panes with none are left out).
    pub fn counts(&self) -> Vec<(u32, usize)> {
        self.lock()
            .panes
            .iter()
            .filter(|(_, q)| !q.items.is_empty())
            .map(|(id, q)| (*id, q.items.len()))
            .collect()
    }

    /// What is held, oldest first per pane: for pane `pane`, or for every
    /// pane (in pane order) when `None`.
    pub fn list(&self, pane: Option<u32>) -> Vec<HeldEntry> {
        self.lock()
            .panes
            .iter()
            .filter(|(id, _)| pane.is_none_or(|p| p == **id))
            .flat_map(|(id, q)| q.items.iter().map(|h| h.entry(*id)))
            .collect()
    }

    /// Take the held delivery `entry_id` out without making it. Answers
    /// what it was, `None` when nothing with that id is held (it was
    /// delivered, discarded already, or its pane went).
    pub fn discard(&self, entry_id: u64) -> Option<HeldEntry> {
        let mut inner = self.lock();
        let (pane, at) = inner.panes.iter().find_map(|(pane, q)| {
            q.items
                .iter()
                .position(|h| h.id == entry_id)
                .map(|at| (*pane, at))
        })?;
        let q = inner.panes.get_mut(&pane)?;
        let held = q.items.remove(at)?;
        if q.items.is_empty() && !q.releasing {
            inner.panes.remove(&pane);
        }
        let entry = held.entry(pane);
        log::info!(
            "[holds] held message {} for pane {pane} from {} discarded",
            entry.id,
            entry.from
        );
        Some(entry)
    }

    /// Deliver what the gate no longer holds, in order, on the calling
    /// thread. `cwd_of` answers a pane's working folder, `None` when the
    /// pane is gone (its queue is dropped). A pane whose gate closes again
    /// half way keeps the rest for the next release. Answers how many
    /// deliveries were made.
    pub fn release(&self, cwd_of: &dyn Fn(u32) -> Option<Option<std::path::PathBuf>>) -> usize {
        let gate = self.gate();
        let ids: Vec<u32> = self
            .lock()
            .panes
            .iter()
            .filter(|(_, q)| !q.releasing)
            .map(|(id, _)| *id)
            .collect();
        let mut delivered = 0;
        for id in ids {
            let Some(cwd) = cwd_of(id) else {
                if let Some(q) = self.lock().panes.remove(&id) {
                    log::info!(
                        "[holds] pane {id} is gone; {} held message(s) dropped",
                        q.items.len()
                    );
                }
                continue;
            };
            let blocked = |gate: &Option<Arc<dyn HoldGate>>| match (gate, cwd.as_deref()) {
                (Some(g), Some(dir)) => g.hold_reason(dir).is_some(),
                _ => false,
            };
            if blocked(&gate) {
                continue;
            }
            match self.lock().panes.get_mut(&id) {
                Some(q) if !q.releasing => q.releasing = true,
                _ => continue,
            }
            loop {
                let next = {
                    let mut inner = self.lock();
                    let Some(q) = inner.panes.get_mut(&id) else {
                        break;
                    };
                    match q.items.pop_front() {
                        Some(h) => h,
                        None => {
                            inner.panes.remove(&id);
                            break;
                        }
                    }
                };
                (next.run)();
                delivered += 1;
                if blocked(&self.gate()) {
                    if let Some(q) = self.lock().panes.get_mut(&id) {
                        q.releasing = false;
                    }
                    break;
                }
            }
        }
        delivered
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Switch(AtomicBool);
    impl HoldGate for Switch {
        fn hold_reason(&self, _: &Path) -> Option<String> {
            self.0.load(Ordering::SeqCst).then(|| "blocked".to_owned())
        }
    }

    #[test]
    fn held_deliveries_wait_for_the_gate_and_go_out_in_order() {
        let holds = DeliveryHolds::default();
        let gate = Arc::new(Switch(AtomicBool::new(true)));
        holds.set_gate(gate.clone());
        let cwd = PathBuf::from("/srv/example");
        assert_eq!(holds.reason_for(1, Some(&cwd)).as_deref(), Some("blocked"));
        assert_eq!(
            holds.reason_for(1, None),
            None,
            "a pane with no folder is never held"
        );
        let out = Arc::new(Mutex::new(Vec::<&str>::new()));
        for text in ["one", "two", "three"] {
            let out = out.clone();
            holds.hold(
                1,
                "user".into(),
                text.into(),
                Box::new(move || out.lock().unwrap().push(text)),
            );
        }
        assert_eq!(holds.counts(), vec![(1, 3)]);
        let cwd_of = |_: u32| Some(Some(cwd.clone()));
        assert_eq!(holds.release(&cwd_of), 0, "still blocked: nothing goes");
        assert!(out.lock().unwrap().is_empty());
        gate.0.store(false, Ordering::SeqCst);
        // Queued ones make a new delivery wait behind them, gate open or not.
        assert!(holds
            .reason_for(1, Some(&cwd))
            .unwrap()
            .contains("behind 3"));
        assert_eq!(holds.release(&cwd_of), 3);
        assert_eq!(*out.lock().unwrap(), ["one", "two", "three"]);
        assert!(holds.counts().is_empty());
        assert_eq!(holds.reason_for(1, Some(&cwd)), None);
    }

    #[test]
    fn a_gate_that_closes_mid_release_keeps_the_rest_and_a_gone_pane_drops_its_queue() {
        let holds = DeliveryHolds::default();
        let gate = Arc::new(Switch(AtomicBool::new(false)));
        holds.set_gate(gate.clone());
        let g = gate.clone();
        holds.hold(
            1,
            "user".into(),
            "closes the gate".into(),
            Box::new(move || g.0.store(true, Ordering::SeqCst)),
        );
        holds.hold(1, "user".into(), "waits".into(), Box::new(|| {}));
        holds.hold(
            2,
            "user".into(),
            "orphan".into(),
            Box::new(|| panic!("a gone pane gets nothing")),
        );
        let cwd_of = |id: u32| (id == 1).then(|| Some(PathBuf::from("/srv/example")));
        assert_eq!(holds.release(&cwd_of), 1);
        assert_eq!(
            holds
                .list(Some(1))
                .iter()
                .map(|e| e.first_line.as_str())
                .collect::<Vec<_>>(),
            ["waits"]
        );
        assert_eq!(
            holds.counts(),
            vec![(1, 1)],
            "pane 2 is gone, its queue with it"
        );
        gate.0.store(false, Ordering::SeqCst);
        assert_eq!(holds.release(&cwd_of), 1);
        assert!(holds.counts().is_empty());
    }

    #[test]
    fn held_deliveries_are_listed_with_stable_ids_and_one_can_be_discarded() {
        let holds = DeliveryHolds::default();
        holds.set_gate(Arc::new(Switch(AtomicBool::new(true))));
        let out = Arc::new(Mutex::new(Vec::<&str>::new()));
        let push = |text: &'static str| {
            let out = out.clone();
            Box::new(move || out.lock().unwrap().push(text)) as Box<dyn FnOnce() + Send>
        };
        holds.hold(
            1,
            "agent:7".into(),
            "\n  first: please rerun the tests  \nsecond line".into(),
            push("a"),
        );
        holds.hold(2, "timer 4".into(), "check the build".into(), push("b"));
        holds.hold(1, "user".into(), "x".repeat(300), push("c"));
        let all = holds.list(None);
        assert_eq!(
            all.iter()
                .map(|e| (e.id, e.pane, e.from.as_str()))
                .collect::<Vec<_>>(),
            [(1, 1, "agent:7"), (3, 1, "user"), (2, 2, "timer 4")]
        );
        assert_eq!(all[0].first_line, "first: please rerun the tests");
        assert_eq!(all[1].first_line.chars().count(), FIRST_LINE_CAP);
        assert!(all[1].first_line.ends_with('…'));
        assert!(all.iter().all(|e| e.queued_ms > 0));
        assert_eq!(holds.list(Some(2)).len(), 1);
        assert_eq!(first_line("   \n\t"), "");

        // Discard the middle one of pane 1: the others keep their ids and order.
        let gone = holds.discard(3).expect("held");
        assert_eq!((gone.pane, gone.from.as_str()), (1, "user"));
        assert_eq!(holds.discard(3), None, "an id is discarded once");
        assert_eq!(holds.discard(99), None);
        assert_eq!(
            holds.list(None).iter().map(|e| e.id).collect::<Vec<_>>(),
            [1, 2]
        );
        // The last one of a pane takes the pane's queue with it.
        holds.discard(2).unwrap();
        assert_eq!(holds.counts(), vec![(1, 1)]);
        // Ids are never reused.
        holds.hold(2, "user".into(), "later".into(), push("d"));
        assert_eq!(holds.list(Some(2))[0].id, 4);

        holds.set_gate(Arc::new(Switch(AtomicBool::new(false))));
        let cwd_of = |_: u32| Some(Some(PathBuf::from("/srv/example")));
        assert_eq!(holds.release(&cwd_of), 2);
        assert_eq!(
            *out.lock().unwrap(),
            ["a", "d"],
            "the discarded ones are never made"
        );
    }
}
