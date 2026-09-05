//! Pure process-status tracking + activity rate limiter.
//!
//! Lives in term-core so `cargo test -p term-core` unit-tests the transition
//! rules, the 1/s activity limiter and the list_terminals snapshot headlessly;
//! the Tauri glue (src-tauri/src/registry.rs) calls these and stays thin.
//! Status vocabulary is fixed; the rail and the
//! process list both report it.

use std::time::Duration;

use serde::Serialize;

/// Process status vocabulary, shared by the rail and the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProcessStatus {
    Starting,
    Running,
    Stopped,
    Exited,
    Failed,
}

impl ProcessStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProcessStatus::Starting => "starting",
            ProcessStatus::Running => "running",
            ProcessStatus::Stopped => "stopped",
            ProcessStatus::Exited => "exited",
            ProcessStatus::Failed => "failed",
        }
    }
}

/// A registry-observed event that can move a process between statuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusEvent {
    /// The child produced its first frame — demonstrably running.
    FirstFrame,
    /// The child exited (pump-observed); `success`/`code` decide exited vs
    /// failed.
    Exited { code: Option<i32>, success: bool },
    /// The user closed the terminal.
    UserClosed,
}

/// Apply one observed event. The transitions: spawn = starting →
/// running on first frame, stopped when user-killed, exited on success,
/// failed on nonzero exit. Events after a terminal state are no-ops (a dead
/// process cannot revive).
pub fn transition(current: ProcessStatus, event: StatusEvent) -> ProcessStatus {
    match event {
        StatusEvent::FirstFrame => match current {
            ProcessStatus::Starting => ProcessStatus::Running,
            other => other,
        },
        StatusEvent::Exited { success, .. } => {
            if success {
                ProcessStatus::Exited
            } else {
                ProcessStatus::Failed
            }
        }
        StatusEvent::UserClosed => ProcessStatus::Stopped,
    }
}

/// One terminal's rail row / list_terminals snapshot row. Pure data (the
/// serialization is the Tauri glue's job); the rail hydrates from this.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TerminalSnapshot {
    pub id: u32,
    pub name: String,
    pub status: ProcessStatus,
    pub exit_code: Option<i32>,
    pub cols: u16,
    pub rows: u16,
    pub seq: u32,
}

impl TerminalSnapshot {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: u32,
        name: String,
        status: ProcessStatus,
        exit_code: Option<i32>,
        cols: u16,
        rows: u16,
        seq: u32,
    ) -> Self {
        Self {
            id,
            name,
            status,
            exit_code,
            cols,
            rows,
            seq,
        }
    }

    /// The wire/rail status string (the vocabulary).
    pub fn status_str(&self) -> &'static str {
        self.status.as_str()
    }
}

/// Rate limiter for low-rate rail events. Built at 1/s for the hidden-terminal
/// activity indicator (`term://activity`) and reused at the 2s tick by
/// the stats poller — the window is a constructor argument, so both
/// callers share one implementation. Pure over integer milliseconds so tests
/// fake time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActivityLimiter {
    interval_ms: u64,
    last_ms: Option<u64>,
}

impl ActivityLimiter {
    /// `interval` is the minimum gap between emits (1 s in the app).
    pub fn new(interval: Duration) -> Self {
        Self {
            interval_ms: interval.as_millis() as u64,
            last_ms: None,
        }
    }

    /// True when at least `interval` has passed since the last emit, and then
    /// records `now_ms` as the new emit time. The first call is always due.
    pub fn due(&mut self, now_ms: u64) -> bool {
        match self.last_ms {
            Some(last) if now_ms.saturating_sub(last) < self.interval_ms => false,
            _ => {
                self.last_ms = Some(now_ms);
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transitions() {
        use ProcessStatus::*;
        use StatusEvent::{Exited, FirstFrame, UserClosed};

        // spawn = starting → running on the first frame.
        assert_eq!(transition(Starting, FirstFrame), Running);
        // Already running: a later frame is a no-op.
        assert_eq!(transition(Running, FirstFrame), Running);
        // A dead process cannot revive.
        assert_eq!(transition(Exited, FirstFrame), Exited);
        assert_eq!(transition(Failed, FirstFrame), Failed);
        assert_eq!(transition(Stopped, FirstFrame), Stopped);

        // Exited-0 vs failed (nonzero / signal).
        assert_eq!(
            transition(Running, Exited { code: Some(0), success: true }),
            Exited
        );
        assert_eq!(
            transition(Running, Exited { code: Some(2), success: false }),
            Failed
        );
        assert_eq!(
            transition(Starting, Exited { code: None, success: false }),
            Failed
        );

        // User close → stopped.
        assert_eq!(transition(Running, UserClosed), Stopped);
        assert_eq!(transition(Starting, UserClosed), Stopped);
    }

    #[test]
    fn status_str_matches_the_vocabulary() {
        assert_eq!(ProcessStatus::Starting.as_str(), "starting");
        assert_eq!(ProcessStatus::Running.as_str(), "running");
        assert_eq!(ProcessStatus::Stopped.as_str(), "stopped");
        assert_eq!(ProcessStatus::Exited.as_str(), "exited");
        assert_eq!(ProcessStatus::Failed.as_str(), "failed");
    }

    #[test]
    fn activity_limiter_rate_limits() {
        let mut lim = ActivityLimiter::new(Duration::from_secs(1));
        assert!(lim.due(0), "first emit is due");
        assert!(!lim.due(500), "within 1s: suppressed");
        assert!(!lim.due(999), "just under 1s: suppressed");
        assert!(lim.due(1000), "1s later: due again");
        assert!(!lim.due(1500), "within the next second: suppressed");
        assert!(lim.due(2000), "2s: due again");
    }

    #[test]
    fn snapshot_carries_exit_code_and_status_str() {
        let failed = TerminalSnapshot::new(1, "shell".into(), ProcessStatus::Failed, Some(2), 80, 24, 7);
        assert_eq!(failed.status_str(), "failed");
        assert_eq!(failed.exit_code, Some(2));

        let ok = TerminalSnapshot::new(2, "vim".into(), ProcessStatus::Exited, Some(0), 80, 24, 9);
        assert_eq!(ok.status_str(), "exited");
        assert_eq!(ok.exit_code, Some(0));

        let stopped = TerminalSnapshot::new(3, "pwsh".into(), ProcessStatus::Stopped, None, 120, 30, 0);
        assert_eq!(stopped.status_str(), "stopped");
        assert_eq!(stopped.exit_code, None);
    }
}
