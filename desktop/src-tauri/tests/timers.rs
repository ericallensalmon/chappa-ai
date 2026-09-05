//! Timer tests (## Tests, plus the 2026-08-29 review
//! fixes). The scheduler is pure over an injected clock and a fake liveness
//! probe, so every case below is deterministic: NO sleeps, no pty, no wall
//! clock. The persistence half runs against a tempdir `coordination.db`, and
//! the route half against the real control server on an ephemeral port.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use app_lib::agents::{gate_step, DeliveryPolicy, GateStep, IoView};
use app_lib::control_http::{self, ControlState, ACTOR_HEADER};
use app_lib::coordination::{Coordination, DB_FILE, SCHEMA_VERSION};
use app_lib::timers::{
    coalesced_with, is_idle, Action, Delivery, Deliverer, FakeClock, FireReason, Knobs, Liveness,
    OutputProbe, ProcessRow, RecentDelivery, Scheduler, Timer, TimerKind, TimerService, TimerStatus,
    TimerStore, INTERRUPTED,
};
use serde_json::{json, Value};

const TOKEN: &str = "t0ken-for-tests";
const IDLE: u64 = 120_000;
const CONFIRM: u64 = 5_000;

// ---- fakes -------------------------------------------------------------------

/// A liveness table the test writes by hand. `bytes` is the byte counter the
/// flicker check compares — a "byte arrived" is `bump`. Every process gets
/// the uuid `u<id>` unless `replace` re-spawns it under a new one.
/// `(liveness, name, project)`.
type ProbeRow = (Liveness, String, Option<u32>);

#[derive(Default)]
struct FakeProbe {
    rows: Mutex<BTreeMap<u32, ProbeRow>>,
}

fn uuid_of(id: u32) -> String {
    format!("u{id}")
}

impl FakeProbe {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Add a live process that last spoke at `last_output_at`.
    fn add(&self, id: u32, name: &str, last_output_at: u64) {
        self.add_with(id, name, last_output_at, &uuid_of(id), None);
    }

    /// Add a live process owned by a project.
    fn add_in(&self, id: u32, name: &str, last_output_at: u64, project: u32) {
        self.add_with(id, name, last_output_at, &uuid_of(id), Some(project));
    }

    /// The same numeric id, a DIFFERENT spawn (a relaunch reused the number).
    fn replace(&self, id: u32, name: &str, last_output_at: u64) {
        self.add_with(id, name, last_output_at, &format!("u{id}-reused"), None);
    }

    fn add_with(&self, id: u32, name: &str, last_output_at: u64, uuid: &str, project: Option<u32>) {
        self.rows.lock().unwrap().insert(
            id,
            (
                Liveness {
                    uuid: uuid.to_owned(),
                    child_alive: true,
                    last_output_at: Some(last_output_at),
                    output_bytes: 1,
                },
                name.to_owned(),
                project,
            ),
        );
    }

    /// A byte landed: the counter moves and the silence clock restarts.
    fn bump(&self, id: u32, at: u64) {
        if let Some((live, _, _)) = self.rows.lock().unwrap().get_mut(&id) {
            live.output_bytes += 1;
            live.last_output_at = Some(at);
        }
    }

    fn kill(&self, id: u32) {
        if let Some((live, _, _)) = self.rows.lock().unwrap().get_mut(&id) {
            live.child_alive = false;
        }
    }

    fn close(&self, id: u32) {
        self.rows.lock().unwrap().remove(&id);
    }
}

impl OutputProbe for FakeProbe {
    fn liveness(&self, id: u32) -> Option<Liveness> {
        self.rows.lock().unwrap().get(&id).map(|(l, _, _)| l.clone())
    }
    fn processes(&self) -> Vec<ProcessRow> {
        self.rows
            .lock()
            .unwrap()
            .iter()
            .map(|(id, (l, name, project))| ProcessRow {
                id: *id,
                name: name.clone(),
                uuid: l.uuid.clone(),
                project_id: *project,
            })
            .collect()
    }
}

/// Records every delivery; `gone` makes a target answer "process gone",
/// `unready` "not ready", and `hang` never returns (a delivery caught by a
/// crash / a wedged ready gate).
#[derive(Default)]
struct FakeDeliverer {
    sent: Mutex<Vec<(u32, String)>>,
    gone: Mutex<Vec<u32>>,
    unready: Mutex<Vec<u32>>,
    hang: AtomicBool,
    attempts: AtomicUsize,
}

impl FakeDeliverer {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
    fn sent(&self) -> Vec<(u32, String)> {
        self.sent.lock().unwrap().clone()
    }
    fn make_gone(&self, id: u32) {
        self.gone.lock().unwrap().push(id);
    }
    fn make_unready(&self, id: u32) {
        self.unready.lock().unwrap().push(id);
    }
    fn hang(&self) {
        self.hang.store(true, Ordering::SeqCst);
    }
    fn attempts(&self) -> usize {
        self.attempts.load(Ordering::SeqCst)
    }
}

impl Deliverer for FakeDeliverer {
    fn deliver(&self, process_id: u32, body: &str, timeout_ms: u64) -> Delivery {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        if self.hang.load(Ordering::SeqCst) {
            loop {
                std::thread::park();
            }
        }
        if self.gone.lock().unwrap().contains(&process_id) {
            return Delivery {
                delivered: false,
                detail: Some("process gone".into()),
                receipt: None,
            };
        }
        if self.unready.lock().unwrap().contains(&process_id) {
            return Delivery {
                delivered: false,
                detail: Some(format!("not ready within {timeout_ms} ms")),
                receipt: None,
            };
        }
        self.sent.lock().unwrap().push((process_id, body.to_owned()));
        Delivery {
            delivered: true,
            detail: None,
            receipt: Some(json!({"delivered": true, "had_output_within_ms": true})),
        }
    }
}

fn sched(now: u64) -> (Arc<FakeClock>, Arc<FakeProbe>, Scheduler<Arc<FakeClock>>) {
    let clock = FakeClock::new(now);
    (clock.clone(), FakeProbe::new(), Scheduler::new(clock))
}

fn idle_timer(kind: TimerKind, watch: Vec<u32>, max_wait: u64, now: u64) -> Timer {
    Timer::idle(
        kind,
        "7",
        7,
        "wake up".into(),
        watch,
        Default::default(),
        max_wait,
        IDLE,
        CONFIRM,
        true,
        now,
    )
}

// ---- pure scheduler ----------------------------------------------------------

/// A one-shot fires exactly at `delay_ms` — not before, and only once.
#[test]
fn one_shot_fires_at_delay_ms() {
    let (clock, probe, mut s) = sched(1_000);
    let id = s.insert(Timer::delay("7", 7, "go".into(), 5_000, 1_000));
    clock.set(5_999);
    assert!(s.tick(probe.as_ref()).is_empty(), "not due yet");
    clock.set(6_000);
    assert_eq!(
        s.tick(probe.as_ref()),
        vec![Action::Fire { id, reason: FireReason::Condition }]
    );
    assert_eq!(s.get(id).unwrap().status, TimerStatus::Fired);
    assert_eq!(s.get(id).unwrap().fired_count, 1);
    clock.set(60_000);
    assert!(s.tick(probe.as_ref()).is_empty(), "a fired one-shot never fires again");
}

/// `loop` repeats at the interval and `fired_count` climbs; the schedule is
/// advanced from the FIRING instant, so a slow delivery cannot bunch firings.
#[test]
fn a_loop_repeats_at_its_interval() {
    let (clock, probe, mut s) = sched(0);
    let mut timer = Timer::delay("7", 7, "tick".into(), 2_000, 0);
    timer.repeat_every_ms = Some(2_000);
    let id = s.insert(timer);
    for n in 1..=3 {
        clock.set(2_000 * n);
        assert_eq!(s.tick(probe.as_ref()).len(), 1, "firing {n}");
        assert_eq!(s.get(id).unwrap().fired_count, n);
        assert_eq!(s.get(id).unwrap().status, TimerStatus::Pending);
    }
    s.cancel(id);
    clock.set(100_000);
    assert!(s.tick(probe.as_ref()).is_empty(), "no fourth firing after cancel");
    assert_eq!(s.get(id).unwrap().fired_count, 3);
}

/// Review fix: a repeating timer whose PINNED target is gone at fire time
/// expires (`process gone`) instead of firing into nothing forever — the
/// same path an idle timer takes when its watched process disappears.
#[test]
fn a_loop_whose_target_is_gone_expires_instead_of_firing_forever() {
    let (clock, probe, mut s) = sched(0);
    probe.add(7, "me", 0);
    let mut timer = Timer::delay("7", 7, "tick".into(), 2_000, 0).pinned("u7");
    timer.repeat_every_ms = Some(2_000);
    let id = s.insert(timer);
    clock.set(2_000);
    assert_eq!(s.tick(probe.as_ref()), vec![Action::Fire { id, reason: FireReason::Condition }]);
    probe.close(7);
    clock.set(4_000);
    assert_eq!(
        s.tick(probe.as_ref()),
        vec![Action::Expire { id, detail: "process gone" }]
    );
    assert_eq!(s.get(id).unwrap().status, TimerStatus::Expired);
    assert_eq!(s.get(id).unwrap().next_fire_at, None);
    clock.set(60_000);
    assert!(s.tick(probe.as_ref()).is_empty(), "expired for good");
    // A reused id under a DIFFERENT spawn is still "gone" (review fix).
    let mut again = Timer::delay("7", 7, "tick".into(), 1_000, 60_000).pinned("u7");
    again.repeat_every_ms = Some(1_000);
    let id2 = s.insert(again);
    probe.replace(7, "someone else's shell", 60_000);
    clock.set(61_000);
    assert_eq!(
        s.tick(probe.as_ref()),
        vec![Action::Expire { id: id2, detail: "process gone" }]
    );
}

/// `any` IGNORES processes that were already idle at schedule time (the
/// rule: it waits for a NEW transition) and fires only AFTER the confirm
/// window on a process that goes quiet later.
#[test]
fn idle_any_ignores_already_idle_processes_and_fires_after_confirm() {
    let (clock, probe, mut s) = sched(1_000_000);
    probe.add(1, "already-quiet", 0); // silent for 1000 s
    probe.add(2, "busy", 1_000_000);
    let mut timer = idle_timer(TimerKind::IdleAny, vec![1, 2], 600_000, 1_000_000);
    timer.ignored = [1].into_iter().collect();
    let id = s.insert(timer);
    assert_eq!(s.get(id).unwrap().waiting_on(), vec![2], "1 is ignored");

    // 1 stays quiet forever — it must NOT trigger anything.
    clock.set(1_100_000);
    assert!(s.tick(probe.as_ref()).is_empty(), "an already-idle process is not a transition");

    // 2 goes quiet: idle at +120 s, then CONFIRM before the body moves.
    clock.set(1_120_000);
    assert_eq!(
        s.tick(probe.as_ref()),
        vec![Action::Confirm { id, until: 1_125_000 }]
    );
    assert_eq!(s.get(id).unwrap().status, TimerStatus::Confirming);
    clock.set(1_124_999);
    assert!(s.tick(probe.as_ref()).is_empty(), "still confirming");
    clock.set(1_125_000);
    assert_eq!(
        s.tick(probe.as_ref()),
        vec![Action::Fire { id, reason: FireReason::Condition }]
    );
}

/// Review fix: `ignored` is not permanent. An already-idle process that
/// becomes BUSY (produces bytes) is watched again, and its next confirmed
/// idle is exactly the new transition `any` waits for.
#[test]
fn an_ignored_process_that_becomes_busy_is_watched_again() {
    let (clock, probe, mut s) = sched(1_000_000);
    probe.add(1, "already-quiet", 0);
    let mut timer = idle_timer(TimerKind::IdleAny, vec![1], 3_600_000, 1_000_000);
    timer.ignored = [1].into_iter().collect();
    let id = s.insert(timer);
    assert!(s.get(id).unwrap().waiting_on().is_empty());
    clock.set(1_100_000);
    assert!(s.tick(probe.as_ref()).is_empty(), "still idle from before: ignored");

    // It starts working: bytes at t=1 100 000.
    probe.bump(1, 1_100_000);
    clock.set(1_100_001);
    assert!(s.tick(probe.as_ref()).is_empty(), "busy: nothing to fire, but no longer ignored");
    assert_eq!(s.get(id).unwrap().waiting_on(), vec![1], "watched again");

    // Then it goes quiet for real: idle at +120 s → confirm → fire.
    clock.set(1_100_000 + IDLE);
    assert_eq!(
        s.tick(probe.as_ref()),
        vec![Action::Confirm { id, until: 1_100_000 + IDLE + CONFIRM }]
    );
    clock.set(1_100_000 + IDLE + CONFIRM);
    assert_eq!(
        s.tick(probe.as_ref()),
        vec![Action::Fire { id, reason: FireReason::Condition }]
    );
    assert_eq!(s.get(id).unwrap().fired_count, 1);
}

/// The timer-113 replay: idle 10 s → a byte → idle 130 s.
/// The flicker must NOT fire, and the real quiet afterwards fires ONCE.
#[test]
fn a_byte_in_the_confirm_window_rearms_the_timer() {
    let (clock, probe, mut s) = sched(0);
    probe.add(1, "worker", 0);
    let id = s.insert(idle_timer(TimerKind::IdleAny, vec![1], 600_000, 0));

    // Quiet for 120 s → confirming.
    clock.set(IDLE);
    assert_eq!(s.tick(probe.as_ref()), vec![Action::Confirm { id, until: IDLE + CONFIRM }]);
    // A byte lands 2 s into the confirm window: back to pending, still armed.
    clock.set(IDLE + 2_000);
    probe.bump(1, IDLE + 2_000);
    assert_eq!(s.tick(probe.as_ref()), vec![Action::Rearm { id, armed: true }]);
    assert_eq!(s.get(id).unwrap().status, TimerStatus::Pending);
    // Even past the ORIGINAL confirm deadline nothing fires — the silence
    // clock restarted with that byte.
    clock.set(IDLE + CONFIRM + 1);
    assert!(s.tick(probe.as_ref()).is_empty(), "the flicker must not fire the timer");
    // Now it really goes quiet: 120 s of silence, then the confirm window.
    let quiet_at = IDLE + 2_000;
    clock.set(quiet_at + IDLE);
    assert_eq!(s.tick(probe.as_ref()).len(), 1, "re-enters confirming");
    clock.set(quiet_at + IDLE + CONFIRM);
    assert_eq!(
        s.tick(probe.as_ref()),
        vec![Action::Fire { id, reason: FireReason::Condition }]
    );
    assert_eq!(s.get(id).unwrap().fired_count, 1, "fires exactly once");
}

/// `rearm: false` gets ONE chance at the idle condition: a flicker disarms
/// the watch (the timer keeps only its hard deadline) instead of re-arming.
#[test]
fn rearm_false_disarms_after_a_flicker_but_keeps_the_deadline() {
    let (clock, probe, mut s) = sched(0);
    probe.add(1, "worker", 0);
    let mut timer = idle_timer(TimerKind::IdleAny, vec![1], 600_000, 0);
    timer.rearm = false;
    let id = s.insert(timer);
    clock.set(IDLE);
    assert_eq!(s.tick(probe.as_ref()).len(), 1, "confirming");
    clock.set(IDLE + 1_000);
    probe.bump(1, IDLE + 1_000);
    assert_eq!(s.tick(probe.as_ref()), vec![Action::Rearm { id, armed: false }]);
    // Quiet again — but it is disarmed, so the condition no longer fires it.
    clock.set(IDLE * 4);
    assert!(s.tick(probe.as_ref()).is_empty(), "disarmed: no second chance");
    // The hard deadline still holds.
    clock.set(600_000);
    assert_eq!(
        s.tick(probe.as_ref()),
        vec![Action::Fire { id, reason: FireReason::Deadline }]
    );
}

/// `all` with everything already idle answers `already_satisfied` and creates
/// NO timer (asserted through the service, where that decision lives).
#[test]
fn idle_all_with_everything_idle_is_already_satisfied() {
    let (service, clock, probe, _deliver, _dir) = service(1_000_000);
    probe.add(1, "a", 0);
    probe.add(2, "b", 0);
    probe.add(7, "me", 1_000_000);
    let reply = service
        .fire_when_idle(
            TimerKind::IdleAll,
            "7",
            None,
            7,
            "go".into(),
            &["a".into(), "2".into()],
            600_000,
            None,
            None,
            None,
            None,
        )
        .unwrap();
    assert_eq!(reply["status"], "already_satisfied");
    assert_eq!(reply["timer_id"], Value::Null);
    assert_eq!(reply["already_idle"], json!([1, 2]));
    assert_eq!(reply["waiting_on"], json!([]));
    assert_eq!(service.list("7", true, true, None, None, None).unwrap()["total"], 0);
    clock.advance(1);
}

/// `all` waits for the LAST watched process and confirms like `any` does.
#[test]
fn idle_all_waits_for_every_process() {
    let (clock, probe, mut s) = sched(0);
    probe.add(1, "a", 0);
    probe.add(2, "b", 0);
    let id = s.insert(idle_timer(TimerKind::IdleAll, vec![1, 2], 900_000, 0));
    clock.set(IDLE);
    probe.bump(2, IDLE); // b is still talking
    assert!(s.tick(probe.as_ref()).is_empty(), "b is not idle");
    clock.set(IDLE * 2 + 2_000);
    assert_eq!(s.tick(probe.as_ref()).len(), 1, "both quiet → confirming");
    clock.set(IDLE * 2 + 2_000 + CONFIRM);
    assert_eq!(
        s.tick(probe.as_ref()),
        vec![Action::Fire { id, reason: FireReason::Condition }]
    );
}

/// The deadline is HARD: it fires even from `confirming`, with its own reason.
#[test]
fn the_deadline_fires_with_reason_deadline() {
    let (clock, probe, mut s) = sched(0);
    probe.add(1, "a", 0);
    let id = s.insert(idle_timer(TimerKind::IdleAny, vec![1], IDLE + 1_000, 0));
    clock.set(IDLE);
    assert_eq!(s.tick(probe.as_ref()).len(), 1, "confirming");
    clock.set(IDLE + 1_000);
    assert_eq!(
        s.tick(probe.as_ref()),
        vec![Action::Fire { id, reason: FireReason::Deadline }]
    );
    assert_eq!(s.get(id).unwrap().status, TimerStatus::Fired);
}

/// Pause STOPS THE CLOCK; resume continues from the remaining delay.
#[test]
fn pause_stops_the_clock_and_resume_continues_from_the_remainder() {
    let (clock, probe, mut s) = sched(0);
    let id = s.insert(Timer::delay("7", 7, "go".into(), 10_000, 0));
    clock.set(4_000);
    s.pause(id);
    assert_eq!(s.get(id).unwrap().remaining_ms, Some(6_000));
    clock.set(1_000_000);
    assert!(s.tick(probe.as_ref()).is_empty(), "a paused timer never comes due");
    s.resume(id);
    assert_eq!(s.get(id).unwrap().next_fire_at, Some(1_006_000));
    clock.set(1_005_999);
    assert!(s.tick(probe.as_ref()).is_empty());
    clock.set(1_006_000);
    assert_eq!(s.tick(probe.as_ref()).len(), 1);
}

/// Cancelling a CONFIRMING timer never fires it.
#[test]
fn cancelling_a_confirming_timer_never_fires() {
    let (clock, probe, mut s) = sched(0);
    probe.add(1, "a", 0);
    let id = s.insert(idle_timer(TimerKind::IdleAny, vec![1], 600_000, 0));
    clock.set(IDLE);
    assert_eq!(s.tick(probe.as_ref()).len(), 1, "confirming");
    s.cancel(id);
    assert_eq!(s.get(id).unwrap().status, TimerStatus::Cancelled);
    clock.set(600_001);
    assert!(s.tick(probe.as_ref()).is_empty(), "not even at the deadline");
    assert_eq!(s.get(id).unwrap().fired_count, 0);
}

/// Every watched process gone = nothing to wait for: the timer EXPIRES with
/// an audit row rather than claiming a process went quiet (host check 2).
#[test]
fn a_watched_process_going_away_expires_the_timer() {
    let (clock, probe, mut s) = sched(0);
    probe.add(1, "b", 0);
    let id = s.insert(idle_timer(TimerKind::IdleAny, vec![1], 600_000, 0));
    clock.set(1_000);
    probe.kill(1);
    assert_eq!(
        s.tick(probe.as_ref()),
        vec![Action::Expire { id, detail: "process gone" }]
    );
    assert_eq!(s.get(id).unwrap().status, TimerStatus::Expired);
    // A CLOSED process (gone from the registry entirely) is the same story.
    let id2 = s.insert(idle_timer(TimerKind::IdleAny, vec![1], 600_000, 1_000));
    probe.close(1);
    clock.set(2_000);
    assert_eq!(
        s.tick(probe.as_ref()),
        vec![Action::Expire { id: id2, detail: "process gone" }]
    );
    // And so is a watched id whose spawn changed under it (pinned watch).
    let mut id3_timer = idle_timer(TimerKind::IdleAny, vec![1], 600_000, 2_000);
    id3_timer.watch_uuids = vec!["u1".into()];
    let id3 = s.insert(id3_timer);
    probe.replace(1, "b again", 2_000);
    clock.set(3_000);
    assert_eq!(
        s.tick(probe.as_ref()),
        vec![Action::Expire { id: id3, detail: "process gone" }]
    );
}

/// The idle rule reads the byte stream and nothing else.
#[test]
fn idle_is_derived_from_the_byte_stream_only() {
    let booting = Liveness { uuid: "u".into(), child_alive: true, last_output_at: None, output_bytes: 0 };
    assert!(!is_idle(&booting, u64::MAX, IDLE), "no output yet = booting, not idle");
    let quiet = Liveness { uuid: "u".into(), child_alive: true, last_output_at: Some(0), output_bytes: 9 };
    assert!(!is_idle(&quiet, IDLE - 1, IDLE));
    assert!(is_idle(&quiet, IDLE, IDLE));
    assert!(!is_idle(&Liveness { child_alive: false, ..quiet }, IDLE, IDLE));
}

/// Dedupe is a pure window test over the delivery log: delivered rows inside
/// the window, in-flight rows regardless of age.
#[test]
fn dedupe_coalesces_an_identical_body_inside_the_window_only() {
    let log = vec![RecentDelivery { firing_id: 30, timer_id: 3, process_id: 7, body: "go".into(), at: 10_000, in_flight: false }];
    assert_eq!(coalesced_with(&log, 9, false, 7, "go", 14_999, 5_000), Some(30));
    assert_eq!(coalesced_with(&log, 9, false, 7, "go", 15_000, 5_000), None, "past the window");
    assert_eq!(coalesced_with(&log, 9, false, 8, "go", 11_000, 5_000), None, "other process");
    assert_eq!(coalesced_with(&log, 9, false, 7, "go!", 11_000, 5_000), None, "other body");
    let inflight = vec![RecentDelivery { firing_id: 31, timer_id: 3, process_id: 7, body: "go".into(), at: 10_000, in_flight: true }];
    assert_eq!(coalesced_with(&inflight, 9, false, 7, "go", 99_000, 5_000), Some(31), "in flight: any age");
    assert_eq!(coalesced_with(&inflight, 3, true, 7, "go", 99_000, 5_000), Some(31), "a loop behind its own in-flight firing");
}

/// Review fix: the timer delivery policy waits for GENUINE quiet. A
/// streaming target is never written to on the spawn prompt's
/// ready-by-timeout fallback, and when quiet never comes the answer is
/// `not-ready` at the cap — decided over a fake clock, no pty.
#[test]
fn a_timer_body_waits_for_genuine_quiet_and_gives_up_at_the_cap() {
    let streaming = |now: u64| IoView { has_output: true, last_output_ms: Some(now - 10), child_alive: true };
    let first = Some(0);
    let timer = DeliveryPolicy::quiet_only(750, Duration::from_millis(30_000));
    let spawn = DeliveryPolicy::spawn_prompt(750, 5_000);
    // 6 s of continuous output: the spawn prompt goes in by timeout, a timer
    // body keeps waiting.
    assert_eq!(gate_step(streaming(6_000), 6_000, first, spawn, Duration::from_millis(6_000)), Some(GateStep::Write { by_timeout: true }));
    assert_eq!(gate_step(streaming(6_000), 6_000, first, timer, Duration::from_millis(6_000)), None, "still mid-turn");
    assert_eq!(gate_step(streaming(29_000), 29_000, first, timer, Duration::from_millis(29_000)), None);
    // Quiet for 750 ms: write, and not "by timeout".
    let quiet = IoView { has_output: true, last_output_ms: Some(20_000), child_alive: true };
    assert_eq!(gate_step(quiet, 20_750, first, timer, Duration::from_millis(20_750)), Some(GateStep::Write { by_timeout: false }));
    assert_eq!(gate_step(quiet, 20_749, first, timer, Duration::from_millis(20_749)), None);
    // Quiet never comes: `not-ready` exactly at the cap.
    assert_eq!(gate_step(streaming(30_000), 30_000, first, timer, Duration::from_millis(30_000)), Some(GateStep::GiveUp("not-ready")));
    // A dead child is `exited` before anything else.
    let dead = IoView { has_output: true, last_output_ms: Some(0), child_alive: false };
    assert_eq!(gate_step(dead, 1_000, first, timer, Duration::from_millis(1_000)), Some(GateStep::GiveUp("exited")));
}

// ---- the service (fake clock + fake probe + fake deliverer + real db) --------

fn knobs() -> Knobs {
    Knobs {
        idle_threshold_ms: IDLE,
        confirm_ms: CONFIRM,
        delivery_timeout_ms: 1_000,
        dedupe_ms: 5_000,
        retention_ms: 24 * 3_600_000,
        missed_grace_ms: 10_000,
    }
}

fn service(now: u64) -> (
    TimerService,
    Arc<FakeClock>,
    Arc<FakeProbe>,
    Arc<FakeDeliverer>,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().unwrap();
    let (service, clock, probe, deliverer) = service_over(dir.path(), now);
    (service, clock, probe, deliverer, dir)
}

/// A service over an existing (or fresh) db file — the "restart" half of the
/// persistence tests.
fn service_over(dir: &std::path::Path, now: u64) -> (TimerService, Arc<FakeClock>, Arc<FakeProbe>, Arc<FakeDeliverer>) {
    let coord = Coordination::default();
    coord.open(dir.join(DB_FILE)).unwrap();
    let clock = FakeClock::new(now);
    let probe = FakeProbe::new();
    let deliverer = FakeDeliverer::new();
    let service = TimerService::with_parts(
        clock.clone(),
        probe.clone(),
        deliverer.clone(),
        TimerStore::new(coord),
        Arc::new(knobs),
    );
    (service, clock, probe, deliverer)
}

/// The delivery thread is async: wait for the audit row to settle.
fn wait_for<F: Fn() -> bool>(f: F) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("condition never became true");
}

fn row_of(service: &TimerService, id: i64) -> Value {
    let rows = service.list("7", true, true, None, None, None).unwrap();
    rows["timers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == json!(id))
        .expect("timer row")
        .clone()
}

fn last_delivery(service: &TimerService, id: i64) -> Value {
    row_of(service, id)["delivery"].clone()
}

/// A delivery failure that is NOT "gone" (the target never became ready) is
/// RECORDED (`delivered: false` + the reason), and a `loop` timer keeps its
/// schedule regardless. A target the DELIVERER finds gone is recorded too.
#[test]
fn a_delivery_failure_is_recorded_and_a_loop_keeps_its_schedule() {
    let (service, clock, probe, deliverer, _dir) = service(0);
    probe.add(7, "me", 0);
    deliverer.make_unready(7);
    let reply = service
        .set("7", None, 7, "go".into(), 2_000, Some(2_000), Some("beat".into()))
        .unwrap();
    let id = reply["timer_id"].as_i64().unwrap();
    clock.set(2_000);
    service.tick_now();
    wait_for(|| last_delivery(&service, id)["delivered"] == json!(false));
    let delivery = last_delivery(&service, id);
    assert_eq!(delivery["status"], "failed");
    assert_eq!(delivery["error"], "not ready within 1000 ms");
    assert_eq!(delivery["reason"], "condition");
    let row = row_of(&service, id);
    assert_eq!(row["status"], "pending", "a loop keeps its schedule after a failure");
    assert_eq!(row["next_fire_at"], json!(4_000));
    assert_eq!(row["fired_count"], 1);
    // The deliverer itself finding the child gone (it died between the tick
    // and the write) is recorded the same way.
    deliverer.make_gone(7);
    clock.set(4_000);
    service.tick_now();
    wait_for(|| last_delivery(&service, id)["error"] == json!("process gone"));
    assert_eq!(row_of(&service, id)["fired_count"], 2);
}

/// The same body to the same process inside `dedupe_ms` is coalesced —
/// `status: coalesced, delivered: null, coalesced_with: <the other FIRING>` —
/// and NOT re-typed. The same body after the window is delivered normally.
#[test]
fn an_identical_body_is_coalesced_inside_the_window_and_not_after() {
    let (service, clock, probe, deliverer, _dir) = service(0);
    probe.add(7, "me", 0);
    let first = service.set("7", None, 7, "same".into(), 1_000, None, None).unwrap()["timer_id"]
        .as_i64()
        .unwrap();
    let second = service.set("7", None, 7, "same".into(), 2_000, None, None).unwrap()["timer_id"]
        .as_i64()
        .unwrap();
    let third = service.set("7", None, 7, "same".into(), 30_000, None, None).unwrap()["timer_id"]
        .as_i64()
        .unwrap();
    clock.set(1_000);
    service.tick_now();
    wait_for(|| last_delivery(&service, first)["delivered"] == json!(true));
    let first_firing = last_delivery(&service, first)["firing_id"].clone();
    clock.set(2_000);
    service.tick_now();
    let coalesced = last_delivery(&service, second);
    assert_eq!(coalesced["status"], "coalesced");
    assert_eq!(coalesced["delivered"], Value::Null, "THIS firing typed nothing");
    assert_eq!(coalesced["coalesced_with"], first_firing, "names the FIRING, not the timer");
    assert!(coalesced["error"].as_str().unwrap().contains("identical body"));
    clock.set(30_000);
    service.tick_now();
    wait_for(|| last_delivery(&service, third)["status"] == json!("delivered"));
    assert_eq!(last_delivery(&service, third)["coalesced_with"], Value::Null);
    // Exactly TWO writes reached the process, not three.
    assert_eq!(
        deliverer.sent(),
        vec![(7, "same".to_owned()), (7, "same".to_owned())]
    );
}

/// Review fix: dedupe sees IN-FLIGHT firings, not only delivered ones. A
/// `loop` whose previous firing is still waiting at the ready gate does not
/// queue a second body behind it (the mpsc replay), and another timer's
/// identical body is coalesced with the in-flight firing too.
#[test]
fn a_loop_does_not_queue_behind_its_own_in_flight_firing() {
    let (service, clock, probe, deliverer, _dir) = service(0);
    probe.add(7, "me", 0);
    deliverer.hang();
    let beat = service
        .set("7", None, 7, "beat".into(), 1_000, Some(1_000), None)
        .unwrap()["timer_id"]
        .as_i64()
        .unwrap();
    let other = service.set("7", None, 7, "beat".into(), 1_500, None, None).unwrap()["timer_id"]
        .as_i64()
        .unwrap();
    clock.set(1_000);
    service.tick_now();
    wait_for(|| deliverer.attempts() == 1);
    let inflight = last_delivery(&service, beat);
    assert_eq!(inflight["status"], "in_flight");
    assert_eq!(inflight["delivered"], Value::Null);
    let inflight_id = inflight["firing_id"].clone();
    // Second firing of the loop while the first is stuck: coalesced.
    clock.set(2_000);
    service.tick_now();
    let second = last_delivery(&service, beat);
    assert_eq!(second["status"], "coalesced");
    assert_eq!(second["coalesced_with"], inflight_id);
    assert_eq!(row_of(&service, beat)["fired_count"], 2, "the schedule still advanced");
    // The other timer's identical body (due at 1 500, seen on this tick):
    // coalesced with the in-flight firing as well.
    let other_delivery = last_delivery(&service, other);
    assert_eq!(other_delivery["status"], "coalesced");
    assert_eq!(other_delivery["coalesced_with"], inflight_id);
    assert_eq!(deliverer.attempts(), 1, "ONE write attempt, ever");
}

/// A `timer_list` row carries the audit fields, and
/// `include_fired` is what surfaces a fired timer at all.
#[test]
fn timer_list_is_the_audit_log() {
    let (service, clock, probe, _deliverer, _dir) = service(0);
    probe.add(7, "me", 0);
    let id = service.set("7", None, 7, "go".into(), 1_000, None, Some("wake".into())).unwrap()
        ["timer_id"]
        .as_i64()
        .unwrap();
    clock.set(1_000);
    service.tick_now();
    wait_for(|| last_delivery(&service, id)["delivered"] == json!(true));
    let pending_only = service.list("7", false, false, None, None, None).unwrap();
    assert_eq!(pending_only["total"], 0, "a fired timer is hidden by default");
    let rows = service.list("7", true, false, None, None, None).unwrap();
    let row = &rows["timers"][0];
    assert_eq!(row["id"], json!(id));
    assert_eq!(row["name"], "wake");
    assert_eq!(row["kind"], "delay");
    assert_eq!(row["status"], "fired");
    assert_eq!(row["fired_count"], 1);
    assert_eq!(row["delivery_uuid"], "u7");
    assert_eq!(row["delivery"]["process_id"], 7);
    assert_eq!(row["delivery"]["status"], "delivered");
    assert_eq!(row["delivery"]["delivered"], true);
    assert_eq!(row["delivery"]["reason"], "condition");
    assert_eq!(row["delivery"]["at"], json!(1_000));
    assert_eq!(row["delivery"]["receipt"]["had_output_within_ms"], true);
    // Owner scoping: another actor sees nothing until `all`.
    assert_eq!(service.list("99", true, false, None, None, None).unwrap()["total"], 0);
    assert_eq!(service.list("99", true, true, None, None, None).unwrap()["total"], 1);
}

/// Review fix: the list is pending first, newest first, paged by
/// `limit` + `offset` with the unpaged `total` — a `limit` never cuts the
/// newest rows.
#[test]
fn timer_list_is_newest_first_with_pending_ahead_and_pages_by_offset() {
    let (service, clock, probe, _deliverer, _dir) = service(0);
    probe.add(7, "me", 0);
    let mut ids = Vec::new();
    for n in 1..=5u64 {
        clock.set(n * 100);
        let delay = if n <= 2 { 1_000 } else { 100_000 };
        ids.push(service.set("7", None, 7, format!("t{n}"), delay, None, None).unwrap()["timer_id"].as_i64().unwrap());
    }
    clock.set(2_000);
    service.tick_now(); // t1 and t2 fire
    wait_for(|| last_delivery(&service, ids[1])["delivered"] == json!(true));
    let page = service.list("7", true, false, Some(2), None, None).unwrap();
    assert_eq!(page["total"], 5);
    assert_eq!(page["limit"], 2);
    assert_eq!(page["offset"], 0);
    let bodies = |page: &Value| -> Vec<String> {
        page["timers"].as_array().unwrap().iter().map(|t| t["body"].as_str().unwrap().to_owned()).collect()
    };
    assert_eq!(bodies(&page), vec!["t5", "t4"], "the newest pending first");
    let next = service.list("7", true, false, Some(2), Some(2), None).unwrap();
    assert_eq!(bodies(&next), vec!["t3", "t2"], "then the last pending, then the newest fired");
    let last = service.list("7", true, false, Some(2), Some(4), None).unwrap();
    assert_eq!(bodies(&last), vec!["t1"]);
    assert_eq!(last["total"], 5);
    let pending = service.list("7", false, false, None, None, None).unwrap();
    assert_eq!(bodies(&pending), vec!["t5", "t4", "t3"]);
}

/// Retention prunes fired rows past the window (and their firing rows with
/// them); a still-pending timer is never pruned, but its OLD firing rows are.
#[test]
fn retention_prunes_fired_rows_past_the_window() {
    let dir = tempfile::tempdir().unwrap();
    let coord = Coordination::default();
    coord.open(dir.path().join(DB_FILE)).unwrap();
    let clock = FakeClock::new(0);
    let store = TimerStore::new(coord);
    let probe = FakeProbe::new();
    probe.add(7, "me", 0);
    let service = TimerService::with_parts(
        clock.clone(),
        probe,
        FakeDeliverer::new(),
        store.clone(),
        Arc::new(|| Knobs {
            retention_ms: 3_600_000,
            ..Knobs::default()
        }),
    );
    let fired = service.set("7", None, 7, "go".into(), 1_000, None, None).unwrap()["timer_id"]
        .as_i64()
        .unwrap();
    let beat = service.set("7", None, 7, "beat".into(), 1_000, Some(10_000_000), None).unwrap()["timer_id"]
        .as_i64()
        .unwrap();
    service.set("7", None, 7, "later".into(), 10_000_000, None, None).unwrap();
    clock.set(1_000);
    service.tick_now();
    wait_for(|| last_delivery(&service, fired)["delivered"] == json!(true));
    wait_for(|| last_delivery(&service, beat)["delivered"] == json!(true));
    clock.set(3_600_999);
    service.prune_now();
    assert_eq!(service.list("7", true, true, None, None, None).unwrap()["total"], 3, "inside the window");
    clock.set(3_602_001);
    service.prune_now();
    let rows = service.list("7", true, true, None, None, None).unwrap();
    assert_eq!(rows["total"], 2, "the fired row is gone, the pending ones stay");
    assert!(
        store.last_firings(&[fired]).unwrap().is_empty(),
        "its firing rows went with it"
    );
    assert!(
        store.load_all().unwrap().iter().all(|t| t.id != fired),
        "and so did the db row"
    );
    assert!(
        store.last_firings(&[beat]).unwrap().is_empty(),
        "the live loop's OLD firing row was pruned by age"
    );
}

/// Persistence + startup recovery when the SAME spawn is still alive: a
/// pending timer survives a restart; an OVERDUE one fires exactly once with
/// `reason: "missed"`; a repeating overdue one continues on a fresh interval
/// instead of bursting.
#[test]
fn missed_absolute_timers_fire_once_on_restart() {
    let dir = tempfile::tempdir().unwrap();
    let ids = {
        let (service, _clock, probe, _deliverer) = service_over(dir.path(), 0);
        probe.add(7, "me", 0);
        let coord = Coordination::default();
        coord.open(dir.path().join(DB_FILE)).unwrap();
        assert_eq!(coord.schema_version().unwrap(), SCHEMA_VERSION);
        let overdue = service.set("7", None, 7, "overdue".into(), 60_000, None, None).unwrap()
            ["timer_id"]
            .as_i64()
            .unwrap();
        let looping = service
            .set("7", None, 7, "beat".into(), 60_000, Some(60_000), None)
            .unwrap()["timer_id"]
            .as_i64()
            .unwrap();
        let future = service
            .set("7", None, 7, "much later".into(), 6_000_000, None, None)
            .unwrap()["timer_id"]
            .as_i64()
            .unwrap();
        (overdue, looping, future)
    };
    // Restart: a NEW service over the same file, 10 minutes later, with the
    // same spawn (uuid u7) still under id 7.
    let (service, clock, probe, deliverer) = service_over(dir.path(), 600_000);
    probe.add(7, "me", 600_000);
    service.restore();
    assert!(deliverer.sent().is_empty(), "restore itself delivers nothing");
    assert_eq!(last_delivery(&service, ids.0)["status"], "in_flight", "recorded BEFORE delivery");
    service.tick_now();
    wait_for(|| deliverer.sent().len() >= 2);
    assert_eq!(
        deliverer.sent(),
        vec![(7, "overdue".to_owned()), (7, "beat".to_owned())]
    );
    wait_for(|| last_delivery(&service, ids.1)["status"] == json!("delivered"));
    assert_eq!(row_of(&service, ids.0)["status"], "fired");
    assert_eq!(last_delivery(&service, ids.0)["reason"], "missed");
    assert_eq!(last_delivery(&service, ids.0)["delivered"], true);
    assert_eq!(row_of(&service, ids.0)["fired_count"], 1);
    assert_eq!(row_of(&service, ids.1)["status"], "pending", "a loop continues");
    assert_eq!(row_of(&service, ids.1)["next_fire_at"], json!(660_000), "on a FRESH interval");
    assert_eq!(row_of(&service, ids.2)["status"], "pending", "a future timer is untouched");
    assert_eq!(row_of(&service, ids.2)["next_fire_at"], json!(6_000_000));
    // And nothing fires twice on a second restore-free tick.
    clock.set(600_001);
    service.tick_now();
    assert_eq!(deliverer.sent().len(), 2);
}

/// After a relaunch the registry restarts at id 1, so a
/// persisted target's NUMBER may now be someone else's shell. A missed timer
/// whose spawn (by uuid) is not live is recorded `delivered: false, error:
/// "process gone", reason: "missed"` WITHOUT typing anywhere; a pending idle
/// timer watching a stale spawn expires the same way.
#[test]
fn a_stale_target_after_a_relaunch_is_process_gone_never_a_reused_id() {
    let dir = tempfile::tempdir().unwrap();
    let (overdue, watching) = {
        let (service, _clock, probe, _deliverer) = service_over(dir.path(), 0);
        probe.add(7, "me", 0);
        probe.add(3, "worker", 0);
        let overdue = service.set("7", None, 7, "overdue".into(), 60_000, None, None).unwrap()["timer_id"].as_i64().unwrap();
        let watching = service
            .fire_when_idle(TimerKind::IdleAny, "7", None, 7, "B is quiet".into(), &["worker".into()], 3_600_000, None, None, None, None)
            .unwrap()["timer_id"]
            .as_i64()
            .unwrap();
        (overdue, watching)
    };
    // Relaunch: ids 7 and 3 both exist again — as OTHER spawns.
    let (service, clock, probe, deliverer) = service_over(dir.path(), 600_000);
    probe.replace(7, "unrelated shell", 600_000);
    probe.replace(3, "another worker", 0);
    service.restore();
    clock.set(600_000 + knobs().missed_grace_ms);
    service.tick_now();
    let missed = last_delivery(&service, overdue);
    assert_eq!(missed["status"], "failed");
    assert_eq!(missed["delivered"], false);
    assert_eq!(missed["error"], "process gone");
    assert_eq!(missed["reason"], "missed");
    assert_eq!(row_of(&service, overdue)["status"], "fired");
    let expired = last_delivery(&service, watching);
    assert_eq!(expired["error"], "process gone");
    assert_eq!(expired["reason"], "expired");
    assert_eq!(row_of(&service, watching)["status"], "expired");
    assert!(deliverer.sent().is_empty(), "nothing was typed into the reused ids");
    assert_eq!(deliverer.attempts(), 0);
}

/// Review fix: a missed firing waits (in flight) for its OWN spawn for the
/// grace window, delivers when that spawn is live again, and closes as
/// `process gone` when the grace runs out.
#[test]
fn a_missed_firing_waits_for_its_spawn_within_the_grace_window() {
    let dir = tempfile::tempdir().unwrap();
    let (early, late) = {
        let (service, _clock, probe, _deliverer) = service_over(dir.path(), 0);
        probe.add(7, "me", 0);
        let early = service.set("7", None, 7, "early".into(), 1_000, None, None).unwrap()["timer_id"].as_i64().unwrap();
        let late = service.set("7", None, 7, "late".into(), 2_000, None, None).unwrap()["timer_id"].as_i64().unwrap();
        (early, late)
    };
    let (service, clock, probe, deliverer) = service_over(dir.path(), 100_000);
    service.restore(); // nothing is live yet
    service.tick_now();
    assert_eq!(last_delivery(&service, early)["status"], "in_flight", "waiting for its spawn");
    assert!(deliverer.sent().is_empty());
    // The spawn comes back 3 s later (same uuid): both bodies go out, once.
    clock.set(103_000);
    probe.add(7, "me", 103_000);
    service.tick_now();
    wait_for(|| deliverer.sent().len() == 2);
    assert_eq!(deliverer.sent(), vec![(7, "early".to_owned()), (7, "late".to_owned())]);
    wait_for(|| last_delivery(&service, late)["status"] == json!("delivered"));
    assert_eq!(last_delivery(&service, early)["reason"], "missed");
    clock.set(200_000);
    service.tick_now();
    assert_eq!(deliverer.sent().len(), 2, "no second firing");
    assert_eq!(row_of(&service, early)["fired_count"], 1);
}

/// Review fix: the fired/advanced timer row and the in-flight audit row
/// are persisted BEFORE the delivery is attempted, so a crash mid-delivery
/// (simulated: the deliverer never returns, the process "restarts" over the
/// same file) does not re-fire the timer — the interrupted row is closed
/// honestly instead.
#[test]
fn a_crash_after_the_persisted_firing_does_not_refire_on_restart() {
    let dir = tempfile::tempdir().unwrap();
    let (one_shot, beat) = {
        let (service, clock, probe, deliverer) = service_over(dir.path(), 0);
        probe.add(7, "me", 0);
        deliverer.hang();
        let one_shot = service.set("7", None, 7, "once".into(), 1_000, None, None).unwrap()["timer_id"].as_i64().unwrap();
        let beat = service.set("7", None, 7, "beat".into(), 1_000, Some(1_000), None).unwrap()["timer_id"].as_i64().unwrap();
        clock.set(1_000);
        service.tick_now();
        wait_for(|| deliverer.attempts() == 1);
        assert_eq!(last_delivery(&service, one_shot)["status"], "in_flight");
        (one_shot, beat)
        // …and the "crash": the service (with its stuck delivery) is dropped.
    };
    let (service, clock, probe, deliverer) = service_over(dir.path(), 1_500);
    probe.add(7, "me", 1_500);
    service.restore();
    service.tick_now();
    assert_eq!(row_of(&service, one_shot)["status"], "fired", "the persisted transition stands");
    let closed = last_delivery(&service, one_shot);
    assert_eq!(closed["status"], "failed");
    assert_eq!(closed["delivered"], false);
    assert_eq!(closed["error"], INTERRUPTED);
    assert_eq!(row_of(&service, beat)["next_fire_at"], json!(2_000), "the loop's advanced schedule stands");
    assert_eq!(row_of(&service, beat)["fired_count"], 1);
    assert_eq!(deliverer.attempts(), 0, "nothing re-fired at restore");
    // The loop simply continues from its persisted schedule.
    clock.set(2_000);
    service.tick_now();
    wait_for(|| deliverer.sent() == vec![(7, "beat".to_owned())]);
}

/// Names resolve as documented (`processes` = ids or names), and an unknown one
/// is an error naming what IS live — never a silently empty watch set.
#[test]
fn processes_accept_ids_or_names() {
    let (service, _clock, probe, _deliverer, _dir) = service(1_000_000);
    probe.add(7, "me", 1_000_000);
    probe.add(4, "worker · build", 1_000_000);
    let reply = service
        .fire_when_idle(
            TimerKind::IdleAny,
            "7",
            None,
            7,
            "go".into(),
            &["worker · build".into()],
            600_000,
            None,
            None,
            None,
            None,
        )
        .unwrap();
    assert_eq!(reply["waiting_on"], json!([4]));
    assert_eq!(reply["status"], "scheduled");
    assert_eq!(reply["already_idle"], json!([]));
    let by_id = service
        .fire_when_idle(
            TimerKind::IdleAny, "7", None, 7, "go".into(), &["4".into()], 600_000, None, None, None, None,
        )
        .unwrap();
    assert_eq!(by_id["waiting_on"], json!([4]));
    let err = service
        .fire_when_idle(
            TimerKind::IdleAny, "7", None, 7, "go".into(), &["nope".into()], 600_000, None, None, None, None,
        )
        .unwrap_err();
    assert!(err.to_string().contains("no process `nope`"), "{err}");
    assert!(err.to_string().contains("worker · build"), "names what IS live: {err}");
    // A delivery target that is not live is refused up front (timer 116).
    let err = service.set("7", None, 99, "go".into(), 1_000, None, None).unwrap_err();
    assert!(err.to_string().contains("no live process 99"), "{err}");
}

/// A name resolves within the caller's project first;
/// the same name in two projects with no scope is an error naming both; and
/// `project_id` defaults to the actor's own project.
#[test]
fn names_resolve_within_the_project_scope_first() {
    let (service, _clock, probe, _deliverer, _dir) = service(1_000_000);
    probe.add_in(3, "server", 1_000_000, 1);
    probe.add_in(7, "server", 1_000_000, 2);
    probe.add_in(8, "orchestrator", 1_000_000, 2);
    probe.add(9, "loose", 1_000_000);
    // Explicit scope: project 2's server.
    let reply = service
        .fire_when_idle(TimerKind::IdleAny, "8", Some(2), 8, "go".into(), &["server".into()], 600_000, None, None, None, None)
        .unwrap();
    assert_eq!(reply["waiting_on"], json!([7]));
    assert_eq!(reply["project_id"], 2);
    // No scope, but the actor (8) belongs to project 2: same answer, and the
    // timer is recorded in project 2.
    let reply = service
        .fire_when_idle(TimerKind::IdleAny, "8", None, 8, "go".into(), &["server".into()], 600_000, None, None, None, None)
        .unwrap();
    assert_eq!(reply["waiting_on"], json!([7]));
    assert_eq!(reply["project_id"], 2);
    // A scope with no match falls back to every project — one candidate.
    let reply = service
        .fire_when_idle(TimerKind::IdleAny, "8", Some(5), 8, "go".into(), &["loose".into()], 600_000, None, None, None, None)
        .unwrap();
    assert_eq!(reply["waiting_on"], json!([9]));
    // An unscoped caller (no project) hits the ambiguity and is told.
    let err = service
        .fire_when_idle(TimerKind::IdleAny, "9", None, 9, "go".into(), &["server".into()], 600_000, None, None, None, None)
        .unwrap_err();
    assert!(err.to_string().contains("ambiguous"), "{err}");
    assert!(err.to_string().contains("3 (project 1)") && err.to_string().contains("7 (project 2)"), "{err}");
    // The numeric id is never ambiguous.
    let reply = service
        .fire_when_idle(TimerKind::IdleAny, "9", None, 9, "go".into(), &["3".into()], 600_000, None, None, None, None)
        .unwrap();
    assert_eq!(reply["waiting_on"], json!([3]));
    assert_eq!(reply["project_id"], Value::Null, "a loose actor has no project to default to");
    // timer_set defaults its project the same way, and the list filter sees it.
    let set = service.set("3", None, 3, "go".into(), 60_000, None, None).unwrap();
    assert_eq!(set["project_id"], 1);
    assert_eq!(service.list("3", false, false, None, None, Some(1)).unwrap()["total"], 1);
    assert_eq!(service.list("3", false, false, None, None, Some(2)).unwrap()["total"], 0);
}

/// Lifecycle is owner-scoped; `user` (the orchestrator seat) may act on any
/// timer, and another agent may not.
#[test]
fn lifecycle_is_owner_scoped() {
    let (service, _clock, probe, _deliverer, _dir) = service(0);
    probe.add(7, "me", 0);
    let id = service.set("7", None, 7, "go".into(), 60_000, None, None).unwrap()["timer_id"]
        .as_i64()
        .unwrap();
    let err = service
        .lifecycle("9", id, app_lib::timers::Lifecycle::Cancel)
        .unwrap_err();
    assert!(err.to_string().contains("belongs to 7"), "{err}");
    let paused = service
        .lifecycle("user", id, app_lib::timers::Lifecycle::Pause)
        .unwrap();
    assert_eq!(paused["paused"], true);
    assert_eq!(paused["status"], "paused");
    let resumed = service
        .lifecycle("7", id, app_lib::timers::Lifecycle::Resume)
        .unwrap();
    assert_eq!(resumed["resumed"], true);
    let cancelled = service
        .lifecycle("7", id, app_lib::timers::Lifecycle::Cancel)
        .unwrap();
    assert_eq!(cancelled["cancelled"], true);
    assert_eq!(cancelled["status"], "cancelled");
}

// ---- routes ------------------------------------------------------------------

fn http(addr: &str, method: &str, path: &str, actor: Option<&str>, body: &str) -> (u16, Value) {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let actor = actor.map(|a| format!("{ACTOR_HEADER}: {a}\r\n")).unwrap_or_default();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {TOKEN}\r\n{actor}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8_lossy(&raw);
    let status: u16 = text.split_whitespace().nth(1).and_then(|s| s.parse().ok()).expect("status");
    let payload = text.split("\r\n\r\n").nth(1).unwrap_or("");
    (status, serde_json::from_str(payload).unwrap_or(Value::Null))
}

/// The routes carry the actor through as owner AND as the default delivery
/// target, the list is owner-scoped, and a caller with neither an explicit
/// target nor an identity is refused rather than firing "somewhere".
#[test]
fn timer_routes_default_the_delivery_target_to_the_caller() {
    let (service, clock, probe, deliverer, _dir) = service(0);
    probe.add_in(12, "caller", 0, 6);
    probe.add(3, "other", 0);
    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind");
    let addr = server.server_addr().to_string();
    let state = ControlState { timers: service.clone(), ..ControlState::default() };
    std::thread::spawn(move || control_http::serve(server, state, TOKEN.to_owned()));

    let (status, reply) = http(&addr, "POST", "/timers", Some("12"), r#"{"delay_ms": 1000, "body": "wake"}"#);
    assert_eq!(status, 200, "{reply}");
    let id = reply["timer_id"].as_i64().unwrap();
    assert_eq!(reply["delivery_process_id"], 12, "defaults to the caller");
    assert_eq!(reply["delivery_uuid"], "u12", "pinned to the caller's spawn");
    assert_eq!(reply["owner"], "12");
    assert_eq!(reply["project_id"], 6, "review fix: the actor's project by default");

    // No identity and no explicit target: refused with the reason.
    let (status, reply) = http(&addr, "POST", "/timers", None, r#"{"delay_ms": 1000, "body": "wake"}"#);
    assert_eq!(status, 400);
    assert!(reply["message"].as_str().unwrap().contains("delivery_process_id"), "{reply}");

    // An explicit target (and an explicit project) win.
    let (_, explicit) = http(&addr, "POST", "/timers", Some("12"), r#"{"delay_ms": 5000, "body": "other", "delivery_process_id": 3, "project_id": 2}"#);
    assert_eq!(explicit["delivery_process_id"], 3);
    assert_eq!(explicit["project_id"], 2);
    // A target that is not live is a 404 naming what is.
    let (status, missing) = http(&addr, "POST", "/timers", Some("12"), r#"{"delay_ms": 5000, "body": "x", "delivery_process_id": 44}"#);
    assert_eq!(status, 404, "{missing}");
    assert!(missing["message"].as_str().unwrap().contains("12:caller (project 6)"), "{missing}");

    clock.set(1_000);
    service.tick_now();
    wait_for(|| !deliverer.sent().is_empty());
    let (status, listed) = http(&addr, "GET", "/timers?include_fired=true", Some("12"), "");
    assert_eq!(status, 200);
    assert_eq!(listed["total"], 2, "both of this actor's timers");
    assert_eq!(listed["offset"], 0);
    let (_, other) = http(&addr, "GET", "/timers?include_fired=true", Some("99"), "");
    assert_eq!(other["total"], 0, "owner-scoped");
    let (_, everyones) = http(&addr, "GET", "/timers?include_fired=true&all=true", Some("99"), "");
    assert_eq!(everyones["total"], 2);
    let (_, paged) = http(&addr, "GET", "/timers?include_fired=true&limit=1&offset=1", Some("12"), "");
    assert_eq!(paged["timers"].as_array().unwrap().len(), 1);
    assert_eq!(paged["total"], 2);
    let (_, scoped) = http(&addr, "GET", "/timers?include_fired=true&project_id=2", Some("12"), "");
    assert_eq!(scoped["total"], 1, "the project filter");

    // Lifecycle by id.
    let (status, cancelled) = http(&addr, "POST", &format!("/timers/{id}/cancel"), Some("12"), "");
    assert_eq!(status, 200);
    assert_eq!(cancelled["timer_id"], json!(id));
    let (status, missing) = http(&addr, "POST", "/timers/9999/cancel", Some("12"), "");
    assert_eq!(status, 404, "{missing}");
}

/// The idle routes answer the documented scheduling fields verbatim.
#[test]
fn idle_routes_answer_already_idle_waiting_on_and_status() {
    let (service, _clock, probe, _deliverer, _dir) = service(1_000_000);
    probe.add(12, "caller", 1_000_000);
    probe.add(1, "quiet", 0);
    probe.add(2, "busy", 1_000_000);
    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind");
    let addr = server.server_addr().to_string();
    let state = ControlState { timers: service, ..ControlState::default() };
    std::thread::spawn(move || control_http::serve(server, state, TOKEN.to_owned()));

    let (status, any) = http(
        &addr,
        "POST",
        "/timers/idle_any",
        Some("12"),
        r#"{"processes": ["quiet", 2], "max_wait_ms": 600000, "body": "go"}"#,
    );
    assert_eq!(status, 200, "{any}");
    assert_eq!(any["status"], "scheduled");
    assert_eq!(any["already_idle"], json!([1]));
    assert_eq!(any["waiting_on"], json!([2]), "`any` ignores the already-idle one");
    assert!(any["note"].as_str().unwrap().contains("NEW idle transition"));

    let (status, all) = http(
        &addr,
        "POST",
        "/timers/idle_all",
        Some("12"),
        r#"{"processes": ["quiet"], "max_wait_ms": 600000, "body": "go"}"#,
    );
    assert_eq!(status, 200, "{all}");
    assert_eq!(all["status"], "already_satisfied");
    assert_eq!(all["timer_id"], Value::Null);

    let (status, bad) = http(
        &addr,
        "POST",
        "/timers/idle_any",
        Some("12"),
        r#"{"processes": ["ghost"], "max_wait_ms": 1000, "body": "go"}"#,
    );
    assert_eq!(status, 404, "{bad}");
    assert_eq!(bad["error"], "not_found");
}

/// A `ControlState` with no scheduler answers a clear 500 instead of
/// pretending a timer was scheduled.
#[test]
fn an_inert_service_refuses_loudly() {
    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind");
    let addr = server.server_addr().to_string();
    std::thread::spawn(move || control_http::serve(server, ControlState::default(), TOKEN.to_owned()));
    let (status, reply) = http(&addr, "POST", "/timers", Some("12"), r#"{"delay_ms": 1, "body": "x"}"#);
    assert_eq!(status, 500);
    assert!(reply["message"].as_str().unwrap().contains("not running"), "{reply}");
}

/// The scheduler thread wakes on its own (the condvar park, not a sleep-poll):
/// a real-clock service with a 50 ms timer delivers without anyone ticking it.
#[test]
fn the_scheduler_thread_fires_without_being_ticked() {
    let dir = tempfile::tempdir().unwrap();
    let coord = Coordination::default();
    coord.open(dir.path().join(DB_FILE)).unwrap();
    let deliverer = FakeDeliverer::new();
    let probe = FakeProbe::new();
    probe.add(7, "me", 1);
    let service = TimerService::with_parts(
        Arc::new(app_lib::timers::SystemClock),
        probe,
        deliverer.clone(),
        TimerStore::new(coord),
        Arc::new(Knobs::default),
    );
    service.spawn_scheduler();
    service.set("7", None, 7, "wake".into(), 50, None, None).unwrap();
    wait_for(|| deliverer.sent() == vec![(7, "wake".to_owned())]);
}

/// `plan_wait` is the 1 s tick while idle timers exist and the exact delay
/// otherwise — the scheduler never spins.
#[test]
fn plan_wait_is_the_idle_tick_or_the_exact_delay() {
    let (clock, _probe, mut s) = sched(0);
    assert_eq!(s.plan_wait(), None, "nothing to do: park until woken");
    s.insert(Timer::delay("7", 7, "go".into(), 90_000, 0));
    assert_eq!(s.plan_wait(), Some(Duration::from_millis(90_000)));
    let id = s.insert(idle_timer(TimerKind::IdleAny, vec![1], 600_000, 0));
    assert_eq!(s.plan_wait(), Some(Duration::from_millis(1_000)), "1 s tick while watching");
    s.cancel(id);
    clock.set(1_000);
    assert_eq!(s.plan_wait(), Some(Duration::from_millis(89_000)));
}
