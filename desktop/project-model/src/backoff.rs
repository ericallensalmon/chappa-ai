//! Auto-restart backoff (1s → 2s → 4s → … → 30s cap).
//!
//! A crashed `auto_restart` process restarts with an exponentially growing
//! delay so a crash-looping command doesn't hot-spin the machine.
//! Pure over an attempt counter so tests can assert the exact sequence.

use std::time::Duration;

/// First restart delay.
pub const BACKOFF_START: Duration = Duration::from_secs(1);
/// Longest delay we will ever wait between restarts.
pub const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Doubling backoff, capped at [`BACKOFF_MAX`]. `delay()` is pure — call
/// [`RestartBackoff::record_failure`] after each failed spawn and
/// [`RestartBackoff::reset`] after a healthy run (or a user-initiated start).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestartBackoff {
    attempts: u32,
}

impl Default for RestartBackoff {
    fn default() -> Self {
        Self::new()
    }
}

impl RestartBackoff {
    pub fn new() -> Self {
        Self { attempts: 0 }
    }

    /// The delay to wait before the NEXT restart. attempt 0 → 1s, 1 → 2s,
    /// 2 → 4s, 3 → 8s, 4 → 16s, 5 → 30s (1<<5 = 32s capped), and it stays at
    /// the 30s cap from then on.
    pub fn delay(&self) -> Duration {
        let secs = 1u64 << self.attempts.min(5);
        Duration::from_secs(secs.min(BACKOFF_MAX.as_secs()))
    }

    /// The number of consecutive failures so far (also the next exponent).
    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    /// Record one failed spawn → the next delay doubles.
    pub fn record_failure(&mut self) {
        self.attempts = self.attempts.saturating_add(1);
    }

    /// Reset after a successful, sustained run (or a user-initiated start).
    pub fn reset(&mut self) {
        self.attempts = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_sequence_doubles_then_caps_at_30s() {
        let mut b = RestartBackoff::new();
        assert_eq!(b.delay(), Duration::from_secs(1));
        b.record_failure();
        assert_eq!(b.delay(), Duration::from_secs(2));
        b.record_failure();
        assert_eq!(b.delay(), Duration::from_secs(4));
        b.record_failure();
        assert_eq!(b.delay(), Duration::from_secs(8));
        b.record_failure();
        assert_eq!(b.delay(), Duration::from_secs(16));
        b.record_failure();
        // 1<<5 = 32s, capped at 30s — and it stays there forever.
        assert_eq!(b.delay(), Duration::from_secs(30));
        b.record_failure();
        assert_eq!(b.delay(), Duration::from_secs(30));
    }

    #[test]
    fn reset_returns_to_the_first_rung() {
        let mut b = RestartBackoff::new();
        for _ in 0..7 {
            b.record_failure();
        }
        assert_eq!(b.delay(), Duration::from_secs(30));
        b.reset();
        assert_eq!(b.delay(), Duration::from_secs(1));
        assert_eq!(b.attempts(), 0);
    }

    #[test]
    fn never_overflows_attempts() {
        let mut b = RestartBackoff::new();
        for _ in 0..10_000 {
            b.record_failure();
        }
        assert_eq!(b.delay(), Duration::from_secs(30));
    }
}
