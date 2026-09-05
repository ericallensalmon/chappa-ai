//! Project-process status mapping.
//!
//! A spawned process is `starting`; it becomes `running` on its first PTY
//! output or after staying alive 500 ms; a user stop → `stopped`; exit 0 →
//! `exited`; exit ≠ 0 or a spawn error → `failed`. Lifecycle idempotency
//! (measured): `stop` on a non-running
//! process is a no-op success, never an error; `restart` on a stopped process
//! is just a start.
//!
//! The status *vocabulary* lives in `term-core::status::ProcessStatus` (shared
//! with the terminal registry, serialized lowercase); this module owns the
//! project-process RULES, including the 500ms-alive tick that the registry's
//! simpler first-frame transition does not model. Pure over a fake-able
//! millisecond clock so tests need no sleeps.

use term_core::status::ProcessStatus;

/// A starting process with no output yet is Running once it has been alive
/// this long (measured behavior).
pub const RUNNING_AFTER_MS: u64 = 500;

/// An observed event that can move a project process between statuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessEvent {
    /// A spawn attempt was made; the process is now expected to produce output
    /// or stay alive (status → starting).
    Spawned,
    /// First PTY output arrived (→ running).
    Output,
    /// The user asked to stop the process (→ stopped; no-op otherwise).
    Stop,
    /// The child exited; `success` (exit 0) decides exited vs failed.
    Exited { success: bool },
    /// The spawn failed outright (command not found, exec error) (→ failed).
    SpawnFailed,
}

/// Per-process lifecycle tracker. `now_ms` is injected so tests fake the
/// 500ms-alive clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lifecycle {
    status: ProcessStatus,
    spawn_ms: Option<u64>,
}

impl Default for Lifecycle {
    fn default() -> Self {
        Self::new()
    }
}

impl Lifecycle {
    pub fn new() -> Self {
        Self {
            status: ProcessStatus::Stopped,
            spawn_ms: None,
        }
    }

    pub fn status(&self) -> ProcessStatus {
        self.status
    }

    /// Feed one event. Returns the resulting status.
    pub fn on(&mut self, event: ProcessEvent, now_ms: u64) -> ProcessStatus {
        self.status = match event {
            ProcessEvent::Spawned => match self.status {
                // Already live: a restart request must stop-then-start, so a
                // duplicate spawn is a no-op here (the glue serializes it).
                ProcessStatus::Starting | ProcessStatus::Running => self.status,
                // Dead-but-restartable (Stopped/Exited/Failed) → starting.
                _ => {
                    self.spawn_ms = Some(now_ms);
                    ProcessStatus::Starting
                }
            },
            ProcessEvent::Output => {
                if self.status == ProcessStatus::Starting {
                    ProcessStatus::Running
                } else {
                    self.status
                }
            }
            ProcessEvent::Stop => match self.status {
                ProcessStatus::Starting | ProcessStatus::Running => ProcessStatus::Stopped,
                // Stop on a non-running process = no-op success (measured).
                other => other,
            },
            ProcessEvent::Exited { success } => {
                if success {
                    ProcessStatus::Exited
                } else {
                    ProcessStatus::Failed
                }
            }
            ProcessEvent::SpawnFailed => ProcessStatus::Failed,
        };
        self.status
    }

    /// The 500ms-alive rule, evaluated by the pump's tick: a starting process
    /// that has been alive ≥ 500ms with no output is Running.
    pub fn tick(&mut self, now_ms: u64) -> ProcessStatus {
        if self.status == ProcessStatus::Starting {
            if let Some(spawn) = self.spawn_ms {
                if now_ms.saturating_sub(spawn) >= RUNNING_AFTER_MS {
                    self.status = ProcessStatus::Running;
                }
            }
        }
        self.status
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run() -> Lifecycle {
        let mut l = Lifecycle::new();
        assert_eq!(l.on(ProcessEvent::Spawned, 0), ProcessStatus::Starting);
        l
    }

    #[test]
    fn spawn_to_starting_output_to_running() {
        let mut l = run();
        assert_eq!(l.on(ProcessEvent::Output, 1), ProcessStatus::Running);
    }

    #[test]
    fn five_hundred_ms_alive_rule_without_output() {
        let mut l = run();
        assert_eq!(
            l.tick(499),
            ProcessStatus::Starting,
            "499ms alive: still starting"
        );
        assert_eq!(l.tick(500), ProcessStatus::Running, "500ms alive: running");
        // Later ticks stay running.
        assert_eq!(l.tick(90_000), ProcessStatus::Running);
    }

    #[test]
    fn output_beats_the_timer_and_is_idempotent() {
        let mut l = run();
        assert_eq!(l.on(ProcessEvent::Output, 100), ProcessStatus::Running);
        assert_eq!(l.on(ProcessEvent::Output, 101), ProcessStatus::Running);
    }

    #[test]
    fn exit_zero_is_exited_nonzero_is_failed() {
        let mut ok = run();
        assert_eq!(
            ok.on(ProcessEvent::Exited { success: true }, 10),
            ProcessStatus::Exited
        );
        let mut bad = run();
        assert_eq!(
            bad.on(ProcessEvent::Exited { success: false }, 10),
            ProcessStatus::Failed
        );
        // Spawn error → failed.
        let mut err = Lifecycle::new();
        assert_eq!(err.on(ProcessEvent::SpawnFailed, 0), ProcessStatus::Failed);
    }

    #[test]
    fn user_stop_and_idempotent_stop() {
        let mut l = run();
        assert_eq!(l.on(ProcessEvent::Stop, 5), ProcessStatus::Stopped);
        // Stop on a non-running process is a no-op success (measured).
        assert_eq!(l.on(ProcessEvent::Stop, 6), ProcessStatus::Stopped);
        let mut l2 = Lifecycle::new();
        assert_eq!(l2.on(ProcessEvent::Stop, 0), ProcessStatus::Stopped);
    }

    #[test]
    fn restart_on_stopped_is_a_start() {
        let mut l = run();
        l.on(ProcessEvent::Stop, 5);
        // Restart = start: spawn again from Stopped → Starting.
        assert_eq!(l.on(ProcessEvent::Spawned, 6), ProcessStatus::Starting);
        // A process that exited can also be restarted (crash → user start).
        l.on(ProcessEvent::Output, 7);
        l.on(ProcessEvent::Exited { success: false }, 8);
        assert_eq!(l.status(), ProcessStatus::Failed);
        assert_eq!(l.on(ProcessEvent::Spawned, 9), ProcessStatus::Starting);
    }

    #[test]
    fn duplicate_spawn_while_live_is_a_noop() {
        let mut l = run();
        assert_eq!(l.on(ProcessEvent::Spawned, 1), ProcessStatus::Starting);
    }

    #[test]
    fn exited_process_cannot_revive_from_output() {
        let mut l = run();
        l.on(ProcessEvent::Exited { success: true }, 3);
        assert_eq!(l.on(ProcessEvent::Output, 4), ProcessStatus::Exited);
        assert_eq!(l.tick(1000), ProcessStatus::Exited);
    }

    #[test]
    fn timer_restarts_on_each_spawn() {
        let mut l = run();
        l.tick(500); // → running
        l.on(ProcessEvent::Stop, 600);
        l.on(ProcessEvent::Spawned, 1000); // fresh spawn
        assert_eq!(l.tick(1499), ProcessStatus::Starting, "new 500ms window");
        assert_eq!(l.tick(1500), ProcessStatus::Running);
    }
}
