//! The ONE bounded, seq-cursored event ring: the
//! registry's per-terminal `JsonEvent` ring (debug `/events`) and the json
//! agent's `AgentEvent` ring (`get_agent_events`) are the same structure —
//! keep the last `cap` items, answer `since(seq)`, and keep `seq` counting
//! past evictions so a cursor stays valid.

use std::collections::VecDeque;

/// An item with a per-ring monotonic sequence number.
pub trait Sequenced {
    fn seq(&self) -> u64;
}

#[derive(Debug)]
pub struct EventRing<T> {
    buf: VecDeque<T>,
    cap: usize,
    /// The seq the next pushed item is expected to carry (1-based).
    next_seq: u64,
}

impl<T: Sequenced + Clone> EventRing<T> {
    pub fn new(cap: usize) -> Self {
        Self {
            buf: VecDeque::with_capacity(cap.min(64)),
            cap,
            next_seq: 1,
        }
    }

    /// The seq to stamp on the next item (callers that sequence their own
    /// items read this; callers whose items arrive pre-sequenced ignore it).
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Append, evicting the oldest past `cap`. The counter follows the
    /// item's seq so `last_seq` is right either way.
    pub fn push(&mut self, item: T) {
        self.next_seq = self.next_seq.max(item.seq() + 1);
        if self.buf.len() >= self.cap {
            self.buf.pop_front();
        }
        self.buf.push_back(item);
    }

    pub fn since(&self, seq: u64) -> Vec<T> {
        self.buf.iter().filter(|e| e.seq() > seq).cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// The last seq handed out — the cursor "now".
    pub fn last_seq(&self) -> u64 {
        self.next_seq - 1
    }
}
