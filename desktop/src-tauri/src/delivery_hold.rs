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

use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use term_core::actor::epoch_ms;

/// Decides whether deliveries to an agent working in `cwd` must wait, and
/// says why. The app's gate is the data-safety check; tests use their own.
pub trait HoldGate: Send + Sync {
    fn hold_reason(&self, cwd: &Path) -> Option<String>;
}

/// One held delivery: what it says (for the journal and the marker), when
/// it was queued, and the call that makes it.
pub struct Held {
    pub text: String,
    pub queued_ms: u64,
    run: Box<dyn FnOnce() + Send>,
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
        Some(format!("held behind {} earlier message(s) for this agent", q.items.len().max(1)))
    }

    /// Queue `run` for pane `id`. Answers how many deliveries are now held
    /// for it.
    pub fn hold(&self, id: u32, text: String, run: Box<dyn FnOnce() + Send>) -> usize {
        let mut inner = self.lock();
        let q = inner.panes.entry(id).or_default();
        q.items.push_back(Held { text, queued_ms: epoch_ms(), run });
        q.items.len()
    }

    /// How many deliveries are held per pane (panes with none are left out).
    pub fn counts(&self) -> Vec<(u32, usize)> {
        self.lock().panes.iter().filter(|(_, q)| !q.items.is_empty()).map(|(id, q)| (*id, q.items.len())).collect()
    }

    /// What is held for pane `id`, oldest first: `(text, queued at)`.
    pub fn held_texts(&self, id: u32) -> Vec<(String, u64)> {
        self.lock()
            .panes
            .get(&id)
            .map(|q| q.items.iter().map(|h| (h.text.clone(), h.queued_ms)).collect())
            .unwrap_or_default()
    }

    /// Deliver what the gate no longer holds, in order, on the calling
    /// thread. `cwd_of` answers a pane's working folder, `None` when the
    /// pane is gone (its queue is dropped). A pane whose gate closes again
    /// half way keeps the rest for the next release. Answers how many
    /// deliveries were made.
    pub fn release(&self, cwd_of: &dyn Fn(u32) -> Option<Option<std::path::PathBuf>>) -> usize {
        let gate = self.gate();
        let ids: Vec<u32> = self.lock().panes.iter().filter(|(_, q)| !q.releasing).map(|(id, _)| *id).collect();
        let mut delivered = 0;
        for id in ids {
            let Some(cwd) = cwd_of(id) else {
                if let Some(q) = self.lock().panes.remove(&id) {
                    log::info!("[holds] pane {id} is gone; {} held message(s) dropped", q.items.len());
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
                    let Some(q) = inner.panes.get_mut(&id) else { break };
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
        assert_eq!(holds.reason_for(1, None), None, "a pane with no folder is never held");
        let out = Arc::new(Mutex::new(Vec::<&str>::new()));
        for text in ["one", "two", "three"] {
            let out = out.clone();
            holds.hold(1, text.into(), Box::new(move || out.lock().unwrap().push(text)));
        }
        assert_eq!(holds.counts(), vec![(1, 3)]);
        let cwd_of = |_: u32| Some(Some(cwd.clone()));
        assert_eq!(holds.release(&cwd_of), 0, "still blocked: nothing goes");
        assert!(out.lock().unwrap().is_empty());
        gate.0.store(false, Ordering::SeqCst);
        // Queued ones make a new delivery wait behind them, gate open or not.
        assert!(holds.reason_for(1, Some(&cwd)).unwrap().contains("behind 3"));
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
        holds.hold(1, "closes the gate".into(), Box::new(move || g.0.store(true, Ordering::SeqCst)));
        holds.hold(1, "waits".into(), Box::new(|| {}));
        holds.hold(2, "orphan".into(), Box::new(|| panic!("a gone pane gets nothing")));
        let cwd_of = |id: u32| (id == 1).then(|| Some(PathBuf::from("/srv/example")));
        assert_eq!(holds.release(&cwd_of), 1);
        assert_eq!(holds.held_texts(1).iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>(), ["waits"]);
        assert_eq!(holds.counts(), vec![(1, 1)], "pane 2 is gone, its queue with it");
        gate.0.store(false, Ordering::SeqCst);
        assert_eq!(holds.release(&cwd_of), 1);
        assert!(holds.counts().is_empty());
    }
}
