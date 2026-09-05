//! Coordination surface II: timers that re-validate before they
//! fire. The field-notes incidents are the requirements: timers 98/103/113
//! fired on idle FLICKERS (a 10 s blip that turned real later), and timer 116
//! fired "somewhere" with no way to check whether its body was ever seen.
//!
//! Three pieces, deliberately separated so the interesting half is testable
//! without a clock, a pty or a database:
//!
//! 1. [`Scheduler`] — PURE. Owns the timer table in memory and decides what
//!    should happen, over an injected [`Clock`] and an [`OutputProbe`]
//!    (byte-stream liveness per process: `child_alive`,
//!    `last_output_at`, `output_bytes`, plus the process UUID). `tick`
//!    returns [`Action`]s and applies the state transitions; it never
//!    delivers anything, never touches SQLite and never sleeps.
//! 2. [`TimerStore`] — persistence in `coordination.db` (`timers` +
//!    `timer_firings`, schema version 3), sharing the ONE connection mutex
//!    `coordination.rs` owns.
//! 3. [`TimerService`] — the shell. One scheduler thread parked on a condvar
//!    (woken by set/cancel/pause/resume and by a 1 s tick while idle timers
//!    exist) plus one delivery thread, so a wedged agent's 30 s ready-gate
//!    wait can never stall the scheduler.
//!
//! Semantics that are fixed, not choices:
//! - **Targets are pinned by UUID** (review fix). Registry ids restart at 1
//!   on every launch, so a persisted timer stores `(id, uuid)` for its
//!   delivery target and every watched process. A target whose uuid no
//!   longer matches the live entry under that id is GONE — the body is never
//!   typed into whatever shell reused the number.
//! - **Idle comes from the BYTE STREAM only**: idle ⇔ `child_alive` and
//!   `now - last_output_at >= idle_ms`. Never from render diffs. A process
//!   that has produced no output at all is NOT idle (it is booting).
//! - **Fire-time re-validation**: a met idle condition enters
//!   `confirming` for `confirm_ms`; bytes in that window re-arm the timer
//!   (`rearm: true`, the default) instead of firing it.
//! - **Every firing is auditable**: pending AND fired timers are
//!   listable for a retention window with the delivery receipt, and a body
//!   that could not be delivered is `delivered: false` with the reason —
//!   never silently dropped.
//! - **Persist before deliver** (review fix): the fired/advanced timer row
//!   and an IN-FLIGHT audit row are written in one transaction BEFORE the
//!   body is queued; the receipt updates that row afterwards. A crash in
//!   between leaves a row a restart closes as "interrupted" — never a second
//!   delivery.
//! - **Duplicate suppression** : the same body to the same
//!   process within `timer_dedupe_ms` — delivered OR still in flight — is
//!   coalesced (`coalesced_with` = the other FIRING's id), and a repeating
//!   timer never queues a second firing while its previous one is in flight.
//! - **Delivery is the path with the quiet-only policy** (review fix
//!   3): ready gate → ONE atomic write of text + `\r` → receipt, waiting for
//!   GENUINE quiet up to `timer_delivery_timeout_ms`. A timer body is a fresh
//!   user turn, never typed into an agent mid-turn.
//!
//! Lock discipline (results): the scheduler mutex is taken for
//! short, pure stretches only; nothing under it calls a self-locking method,
//! nothing under it unwraps, and delivery/persistence happen after the guard
//! is dropped. Waits are condvar parks, never sleep-polls.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Value};

use crate::coordination::{
    self, CoordError, Coordination, Result, DEFAULT_LIST_LIMIT, MAX_LIST_LIMIT, USER_ACTOR,
};
use crate::registry::{Registry, TermId};
use crate::settings::SettingsState;

/// How often the scheduler wakes while at least one idle timer is watching.
pub const IDLE_TICK: Duration = Duration::from_millis(1_000);
/// How long a `reason: "missed"` firing waits for its target (by uuid) to be
/// live again before it is recorded `process gone` (review fix).
pub const DEFAULT_MISSED_GRACE_MS: u64 = 120_000;
/// The audit detail written to an in-flight firing a restart found open.
pub const INTERRUPTED: &str = "interrupted by a restart before the receipt was recorded";
pub const PROCESS_GONE: &str = "process gone";

// ---- clock -------------------------------------------------------------------

/// Wall clock, injected so every scheduler test is deterministic.
pub trait Clock: Send + Sync + 'static {
    fn now_ms(&self) -> u64;
}

/// Unix epoch milliseconds — the coordination store's reading, so timer rows
/// and scratchpad rows agree about "now".
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        coordination::now_ms().max(0) as u64
    }
}

/// A clock the tests drive by hand. Shared (`Arc`) so a test can advance the
/// same instant the scheduler reads.
#[derive(Debug, Default)]
pub struct FakeClock(AtomicU64);

impl FakeClock {
    pub fn new(start_ms: u64) -> Arc<Self> {
        Arc::new(Self(AtomicU64::new(start_ms)))
    }

    pub fn advance(&self, ms: u64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }

    pub fn set(&self, ms: u64) {
        self.0.store(ms, Ordering::SeqCst);
    }
}

impl Clock for FakeClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// `?Sized` so both `Arc<FakeClock>` (tests) and `Arc<dyn Clock>` (the
/// service's erased clock) are themselves clocks.
impl<T: Clock + ?Sized> Clock for Arc<T> {
    fn now_ms(&self) -> u64 {
        (**self).now_ms()
    }
}

// ---- liveness probe ----------------------------------------------------------

/// The liveness fields for one process — the ONLY signal idle
/// detection is allowed to use — plus the process UUID that pins a target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Liveness {
    /// The stable per-spawn identity (`ControlSnapshot.uuid`).
    pub uuid: String,
    pub child_alive: bool,
    /// Unix ms of the last pty byte; `None` while the child is still booting.
    pub last_output_at: Option<u64>,
    pub output_bytes: u64,
}

/// One live process for name resolution (the ids-or-names rule, scoped
/// by project since the review).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessRow {
    pub id: u32,
    pub name: String,
    pub uuid: String,
    pub project_id: Option<u32>,
}

/// A snapshot source for the scheduler. The real one reads the registry;
/// tests use a fake table.
pub trait OutputProbe: Send + Sync {
    /// `None` = there is no such process (closed, or never existed).
    fn liveness(&self, id: u32) -> Option<Liveness>;

    /// Many ids in one read (the registry does it under ONE lock). Missing
    /// ids are absent from the map.
    fn liveness_many(&self, ids: &[u32]) -> BTreeMap<u32, Liveness> {
        ids.iter()
            .filter_map(|id| self.liveness(*id).map(|l| (*id, l)))
            .collect()
    }

    /// Every live process — `processes` accepts ids OR names, resolved
    /// against this.
    fn processes(&self) -> Vec<ProcessRow>;
}

/// The registry-backed probe.
pub struct RegistryProbe(pub Registry);

impl OutputProbe for RegistryProbe {
    fn liveness(&self, id: u32) -> Option<Liveness> {
        self.liveness_many(&[id]).remove(&id)
    }

    fn liveness_many(&self, ids: &[u32]) -> BTreeMap<u32, Liveness> {
        self.0
            .liveness_many(ids)
            .into_iter()
            .map(|(id, row)| {
                (
                    id,
                    Liveness {
                        uuid: row.uuid,
                        child_alive: row.child_alive,
                        last_output_at: row.last_output_at,
                        output_bytes: row.output_bytes,
                    },
                )
            })
            .collect()
    }

    fn processes(&self) -> Vec<ProcessRow> {
        self.0
            .list_control()
            .into_iter()
            .map(|row| ProcessRow {
                id: row.base.id,
                name: row.base.name.clone(),
                uuid: row.uuid.clone(),
                project_id: row.project_id,
            })
            .collect()
    }
}

/// Idle ⇔ the child is alive AND its byte stream has been silent for at
/// least `idle_ms`. A process that has produced NO output is not idle: it is
/// booting, and firing a wake-up at a booting TUI is exactly the boot race.
pub fn is_idle(live: &Liveness, now_ms: u64, idle_ms: u64) -> bool {
    live.child_alive
        && live
            .last_output_at
            .is_some_and(|t| now_ms.saturating_sub(t) >= idle_ms)
}

/// The live row for `id` IF it is still the spawn the timer pinned. `None`
/// when the id is gone or (when pinned) belongs to a different spawn now —
/// a reused number is not the process the timer was set against.
fn pinned<'a>(live: &'a BTreeMap<u32, Liveness>, id: u32, uuid: Option<&str>) -> Option<&'a Liveness> {
    live.get(&id)
        .filter(|l| uuid.map_or(true, |u| l.uuid == u))
}

// ---- timer model -------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TimerKind {
    /// `timer_set`: a delay, optionally repeating.
    Delay,
    /// `timer_fire_when_idle_any`.
    IdleAny,
    /// `timer_fire_when_idle_all`.
    IdleAll,
}

impl TimerKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TimerKind::Delay => "delay",
            TimerKind::IdleAny => "idle_any",
            TimerKind::IdleAll => "idle_all",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "idle_any" => TimerKind::IdleAny,
            "idle_all" => TimerKind::IdleAll,
            _ => TimerKind::Delay,
        }
    }

    pub fn is_idle(self) -> bool {
        matches!(self, TimerKind::IdleAny | TimerKind::IdleAll)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TimerStatus {
    Pending,
    /// The condition looks met; re-checking after `confirm_ms`.
    Confirming,
    Paused,
    Fired,
    Cancelled,
    /// Nothing left to wait for (every watched process is gone, or the
    /// delivery target is gone at fire time).
    Expired,
}

impl TimerStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TimerStatus::Pending => "pending",
            TimerStatus::Confirming => "confirming",
            TimerStatus::Paused => "paused",
            TimerStatus::Fired => "fired",
            TimerStatus::Cancelled => "cancelled",
            TimerStatus::Expired => "expired",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "confirming" => TimerStatus::Confirming,
            "paused" => TimerStatus::Paused,
            "fired" => TimerStatus::Fired,
            "cancelled" => TimerStatus::Cancelled,
            "expired" => TimerStatus::Expired,
            _ => TimerStatus::Pending,
        }
    }

    /// Terminal states are kept for the audit window and never ticked again.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TimerStatus::Fired | TimerStatus::Cancelled | TimerStatus::Expired
        )
    }
}

/// Why a body was delivered (or why a timer stopped).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FireReason {
    /// The timer's own condition: a delay elapsed, or a CONFIRMED idle.
    Condition,
    /// `max_wait_ms` ran out.
    Deadline,
    /// The app was down when an absolute fire time passed.
    Missed,
    /// Recorded on an `Expire` — nothing was delivered.
    Expired,
}

impl FireReason {
    pub fn as_str(self) -> &'static str {
        match self {
            FireReason::Condition => "condition",
            FireReason::Deadline => "deadline",
            FireReason::Missed => "missed",
            FireReason::Expired => "expired",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "deadline" => FireReason::Deadline,
            "missed" => FireReason::Missed,
            "expired" => FireReason::Expired,
            _ => FireReason::Condition,
        }
    }
}

/// A `reason: "missed"` firing recorded at restore and waiting for its target
/// (review fix): the in-flight audit row and the end of the grace window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MissedFiring {
    /// The `timer_firings` row id (0 in the pure tests).
    pub row: i64,
    pub until: u64,
}

/// One timer. Everything the scheduler needs and everything the db stores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timer {
    /// 0 until the store (or [`Scheduler::insert`]) assigns one.
    pub id: i64,
    pub name: Option<String>,
    pub kind: TimerKind,
    /// The actor that created it (`X-Chappa-Actor`, else `user`).
    pub owner: String,
    pub project_id: Option<i64>,
    /// A timer delivers to exactly ONE process…
    pub delivery_process_id: u32,
    /// …pinned by its uuid. `None` only for rows written before schema 3 —
    /// the service never delivers an unpinned body (it could reach a reused
    /// id); the pure scheduler skips the check for them.
    pub delivery_uuid: Option<String>,
    pub body: String,
    /// Watched processes (idle timers)…
    pub watch: Vec<u32>,
    /// …and their uuids, parallel to `watch` (shorter = unpinned tail).
    pub watch_uuids: Vec<String>,
    /// Processes that were ALREADY idle at schedule time — `any` ignores
    /// them until they become busy again (review fix), then a later
    /// confirmed idle counts as the NEW transition the rule wants.
    pub ignored: BTreeSet<u32>,
    pub delay_ms: u64,
    pub repeat_every_ms: Option<u64>,
    /// Absolute unix ms of the next firing (delay timers).
    pub next_fire_at: Option<u64>,
    /// Absolute unix ms of the hard deadline (idle timers).
    pub deadline_at: Option<u64>,
    pub idle_ms: u64,
    pub confirm_ms: u64,
    /// Whether a flicker re-arms the timer.
    pub rearm: bool,
    /// Still allowed to enter `confirming`. A `rearm: false` timer clears
    /// this after its one chance.
    pub armed: bool,
    pub status: TimerStatus,
    pub fired_count: u64,
    /// Remaining delay captured by `timer_pause` (the clock stops).
    pub remaining_ms: Option<u64>,
    pub created_at: u64,
    pub updated_at: u64,
    /// Byte counters captured when `confirming` was entered — a change in
    /// ANY of them is the flicker.
    pub confirm_baseline: BTreeMap<u32, u64>,
    /// When the confirm window closes.
    pub confirm_until: Option<u64>,
    /// Runtime only: a missed firing waiting for its target.
    pub missed: Option<MissedFiring>,
}

impl Timer {
    /// A `timer_set` timer.
    pub fn delay(owner: &str, delivery_process_id: u32, body: String, delay_ms: u64, now: u64) -> Self {
        Self {
            id: 0,
            name: None,
            kind: TimerKind::Delay,
            owner: owner.to_owned(),
            project_id: None,
            delivery_process_id,
            delivery_uuid: None,
            body,
            watch: Vec::new(),
            watch_uuids: Vec::new(),
            ignored: BTreeSet::new(),
            delay_ms,
            repeat_every_ms: None,
            next_fire_at: Some(now.saturating_add(delay_ms)),
            deadline_at: None,
            idle_ms: 0,
            confirm_ms: 0,
            rearm: false,
            armed: true,
            status: TimerStatus::Pending,
            fired_count: 0,
            remaining_ms: None,
            created_at: now,
            updated_at: now,
            confirm_baseline: BTreeMap::new(),
            confirm_until: None,
            missed: None,
        }
    }

    /// A `timer_fire_when_idle_*` timer.
    #[allow(clippy::too_many_arguments)]
    pub fn idle(
        kind: TimerKind,
        owner: &str,
        delivery_process_id: u32,
        body: String,
        watch: Vec<u32>,
        ignored: BTreeSet<u32>,
        max_wait_ms: u64,
        idle_ms: u64,
        confirm_ms: u64,
        rearm: bool,
        now: u64,
    ) -> Self {
        Self {
            kind,
            watch,
            ignored,
            delay_ms: 0,
            next_fire_at: None,
            deadline_at: Some(now.saturating_add(max_wait_ms)),
            idle_ms,
            confirm_ms,
            rearm,
            ..Self::delay(owner, delivery_process_id, body, 0, now)
        }
    }

    /// Pin the delivery target to a spawn (builder form for the tests).
    pub fn pinned(mut self, uuid: &str) -> Self {
        self.delivery_uuid = Some(uuid.to_owned());
        self
    }

    /// The processes an idle timer is actually waiting on: everything watched
    /// minus (for `any`) the ones ignored since schedule time.
    pub fn waiting_on(&self) -> Vec<u32> {
        self.watch
            .iter()
            .copied()
            .filter(|id| !(self.kind == TimerKind::IdleAny && self.ignored.contains(id)))
            .collect()
    }

    /// The pinned uuid of watched process `id` (`None` = unpinned).
    fn watch_uuid(&self, id: u32) -> Option<&str> {
        self.watch
            .iter()
            .position(|w| *w == id)
            .and_then(|i| self.watch_uuids.get(i))
            .map(String::as_str)
    }

    /// Every process id this timer reads on a tick.
    fn probe_ids(&self) -> impl Iterator<Item = u32> + '_ {
        std::iter::once(self.delivery_process_id).chain(self.watch.iter().copied())
    }

    /// The next instant this timer needs the scheduler's attention.
    fn wake_at(&self) -> Option<u64> {
        if let Some(missed) = self.missed {
            return Some(missed.until);
        }
        match self.status {
            TimerStatus::Pending => match self.kind {
                TimerKind::Delay => self.next_fire_at,
                _ => self.deadline_at,
            },
            TimerStatus::Confirming => match (self.confirm_until, self.deadline_at) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            },
            _ => None,
        }
    }
}

/// What the shell must do. The scheduler has ALREADY applied the matching
/// state transition — an `Action` is a side effect to perform (deliver,
/// persist, audit), never a decision to re-make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Deliver the body and record a firing (`reason: "missed"` = the firing
    /// recorded at restore whose target is now live, or whose grace ran out).
    Fire { id: i64, reason: FireReason },
    /// Entered `confirming` — nothing is delivered yet.
    Confirm { id: i64, until: u64 },
    /// A byte landed in the confirm window. `armed` = the timer went back to
    /// watching (`rearm: true`); `armed: false` = it keeps only its deadline.
    Rearm { id: i64, armed: bool },
    /// Nothing left to wait for — audit row, no delivery.
    Expire { id: i64, detail: &'static str },
}

impl Action {
    pub fn id(&self) -> i64 {
        match self {
            Action::Fire { id, .. }
            | Action::Confirm { id, .. }
            | Action::Rearm { id, .. }
            | Action::Expire { id, .. } => *id,
        }
    }
}

// ---- the pure scheduler ------------------------------------------------------

/// The timer table plus the decision rules. No I/O, no sleeps, no clock of
/// its own — `C` supplies `now`, the probe supplies liveness.
pub struct Scheduler<C: Clock> {
    clock: C,
    timers: BTreeMap<i64, Timer>,
    next_id: i64,
}

impl<C: Clock> Scheduler<C> {
    pub fn new(clock: C) -> Self {
        Self {
            clock,
            timers: BTreeMap::new(),
            next_id: 1,
        }
    }

    pub fn now(&self) -> u64 {
        self.clock.now_ms()
    }

    /// Add a timer, assigning an id when it has none (the store assigns real
    /// ids; the pure tests let the scheduler do it).
    pub fn insert(&mut self, mut timer: Timer) -> i64 {
        if timer.id == 0 {
            timer.id = self.next_id;
        }
        self.next_id = self.next_id.max(timer.id + 1);
        let id = timer.id;
        self.timers.insert(id, timer);
        id
    }

    pub fn get(&self, id: i64) -> Option<&Timer> {
        self.timers.get(&id)
    }

    pub fn rows(&self) -> impl Iterator<Item = &Timer> {
        self.timers.values()
    }

    /// True while any non-terminal idle timer is watching, or a missed
    /// firing is waiting for its target — the 1 s tick.
    pub fn has_live_idle_timers(&self) -> bool {
        self.timers.values().any(|t| {
            t.missed.is_some()
                || (t.kind.is_idle() && matches!(t.status, TimerStatus::Pending | TimerStatus::Confirming))
        })
    }

    /// How long the scheduler thread may park. `None` = until woken.
    pub fn plan_wait(&self) -> Option<Duration> {
        let now = self.now();
        let soonest = self.timers.values().filter_map(Timer::wake_at).min();
        let absolute = soonest.map(|at| Duration::from_millis(at.saturating_sub(now)));
        match (self.has_live_idle_timers(), absolute) {
            (true, Some(d)) => Some(d.min(IDLE_TICK)),
            (true, None) => Some(IDLE_TICK),
            (false, d) => d,
        }
    }

    // -- lifecycle by id ------------------------------------------------------

    pub fn cancel(&mut self, id: i64) -> Option<&Timer> {
        let now = self.now();
        let timer = self.timers.get_mut(&id)?;
        if !timer.status.is_terminal() {
            timer.status = TimerStatus::Cancelled;
            timer.confirm_until = None;
            timer.confirm_baseline.clear();
            timer.updated_at = now;
        }
        self.timers.get(&id)
    }

    /// Pause STOPS THE CLOCK: the remaining delay (and the remaining deadline)
    /// are captured and restored by `resume`, so a paused timer does not
    /// silently come due while it is paused.
    pub fn pause(&mut self, id: i64) -> Option<&Timer> {
        let now = self.now();
        let timer = self.timers.get_mut(&id)?;
        if matches!(timer.status, TimerStatus::Pending | TimerStatus::Confirming) {
            let target = match timer.kind {
                TimerKind::Delay => timer.next_fire_at,
                _ => timer.deadline_at,
            };
            timer.remaining_ms = Some(target.map(|t| t.saturating_sub(now)).unwrap_or(0));
            timer.status = TimerStatus::Paused;
            timer.confirm_until = None;
            timer.confirm_baseline.clear();
            timer.updated_at = now;
        }
        self.timers.get(&id)
    }

    pub fn resume(&mut self, id: i64) -> Option<&Timer> {
        let now = self.now();
        let timer = self.timers.get_mut(&id)?;
        if timer.status == TimerStatus::Paused {
            let remaining = timer.remaining_ms.take().unwrap_or(0);
            match timer.kind {
                TimerKind::Delay => timer.next_fire_at = Some(now.saturating_add(remaining)),
                _ => timer.deadline_at = Some(now.saturating_add(remaining)),
            }
            timer.status = TimerStatus::Pending;
            timer.updated_at = now;
        }
        self.timers.get(&id)
    }

    /// Startup recovery: a PENDING absolute-time timer whose fire time passed
    /// while the app was down is advanced ONCE (a repeating one continues on
    /// a fresh interval from now — never a burst of catch-up firings) and
    /// its `reason: "missed"` firing is parked until the target is live again
    /// or `grace_ms` runs out (the tick decides). Returns the ids touched so
    /// the shell persists them BEFORE anything is delivered. Idle timers
    /// just resume watching — their deadline is re-checked by the tick.
    pub fn recover_missed(&mut self, grace_ms: u64) -> Vec<i64> {
        let now = self.now();
        let mut touched = Vec::new();
        for timer in self.timers.values_mut() {
            if timer.status != TimerStatus::Pending || timer.kind != TimerKind::Delay {
                continue;
            }
            if !timer.next_fire_at.is_some_and(|at| at <= now) {
                continue;
            }
            fire(timer, now);
            timer.missed = Some(MissedFiring {
                row: 0,
                until: now.saturating_add(grace_ms),
            });
            touched.push(timer.id);
        }
        touched
    }

    /// The shell recorded the in-flight audit row for a missed firing.
    pub fn set_missed_row(&mut self, id: i64, row: i64) {
        if let Some(missed) = self.timers.get_mut(&id).and_then(|t| t.missed.as_mut()) {
            missed.row = row;
        }
    }

    /// The shell dispatched the missed firing; forget it.
    pub fn take_missed(&mut self, id: i64) -> Option<MissedFiring> {
        self.timers.get_mut(&id).and_then(|t| t.missed.take())
    }

    /// Drop terminal timers (and, with them, their firing rows) older than
    /// the retention window. Returns the ids removed.
    pub fn prune(&mut self, retention_ms: u64) -> Vec<i64> {
        let cutoff = self.now().saturating_sub(retention_ms);
        let doomed: Vec<i64> = self
            .timers
            .values()
            .filter(|t| t.status.is_terminal() && t.missed.is_none() && t.updated_at < cutoff)
            .map(|t| t.id)
            .collect();
        for id in &doomed {
            self.timers.remove(id);
        }
        doomed
    }

    // -- the tick -------------------------------------------------------------

    /// Evaluate every live timer once and apply the transitions.
    ///
    /// A `Fire` is applied EAGERLY (fired_count incremented, a repeat's
    /// schedule advanced, a one-shot marked fired) so that a delivery which
    /// takes 30 s cannot cause a second firing. Liveness for every process
    /// any timer reads is fetched ONCE per tick.
    pub fn tick(&mut self, probe: &dyn OutputProbe) -> Vec<Action> {
        let now = self.clock.now_ms();
        let ids: Vec<u32> = {
            let mut set = BTreeSet::new();
            for t in self.timers.values() {
                if t.missed.is_some() || matches!(t.status, TimerStatus::Pending | TimerStatus::Confirming) {
                    set.extend(t.probe_ids());
                }
            }
            set.into_iter().collect()
        };
        let live = probe.liveness_many(&ids);
        let mut actions = Vec::new();
        for timer in self.timers.values_mut() {
            // A missed firing waits for ITS spawn (by uuid) to be live, or
            // for the grace window to close; the shell then records
            // `process gone` without typing anywhere.
            if let Some(missed) = timer.missed {
                let target = pinned(&live, timer.delivery_process_id, timer.delivery_uuid.as_deref());
                if target.is_some_and(|l| l.child_alive) || now >= missed.until {
                    actions.push(Action::Fire {
                        id: timer.id,
                        reason: FireReason::Missed,
                    });
                }
                continue;
            }
            match timer.status {
                TimerStatus::Pending | TimerStatus::Confirming => {}
                _ => continue,
            }
            // A PINNED delivery target that is gone at
            // fire time expires the timer (a repeating one included) — there
            // is no one left to deliver to, ever.
            let target_gone = timer.delivery_uuid.is_some()
                && !pinned(&live, timer.delivery_process_id, timer.delivery_uuid.as_deref())
                    .is_some_and(|l| l.child_alive);
            if timer.kind == TimerKind::Delay {
                if timer.next_fire_at.is_some_and(|at| now >= at) {
                    if target_gone {
                        expire(timer, now);
                        actions.push(Action::Expire {
                            id: timer.id,
                            detail: PROCESS_GONE,
                        });
                    } else {
                        fire(timer, now);
                        actions.push(Action::Fire {
                            id: timer.id,
                            reason: FireReason::Condition,
                        });
                    }
                }
                continue;
            }
            // ---- idle timers ----
            if target_gone {
                expire(timer, now);
                actions.push(Action::Expire {
                    id: timer.id,
                    detail: PROCESS_GONE,
                });
                continue;
            }
            // The hard deadline wins over everything, confirming included.
            if timer.deadline_at.is_some_and(|at| now >= at) {
                fire(timer, now);
                actions.push(Action::Fire {
                    id: timer.id,
                    reason: FireReason::Deadline,
                });
                continue;
            }
            // Review fix: an ignored (already-idle-at-schedule) process
            // that produced bytes since is BUSY again — its next confirmed
            // idle is the new transition `any` waits for.
            if timer.kind == TimerKind::IdleAny && !timer.ignored.is_empty() {
                let busy_again: Vec<u32> = timer
                    .ignored
                    .iter()
                    .copied()
                    .filter(|id| {
                        pinned(&live, *id, timer.watch_uuid(*id))
                            .is_some_and(|l| l.child_alive && !is_idle(l, now, timer.idle_ms))
                    })
                    .collect();
                for id in busy_again {
                    timer.ignored.remove(&id);
                }
            }
            let watched = timer.waiting_on();
            let rows: Vec<(u32, Option<&Liveness>)> = watched
                .iter()
                .map(|id| (*id, pinned(&live, *id, timer.watch_uuid(*id))))
                .collect();
            // Nothing left to observe: every watched process is gone. Firing
            // "B went idle" at that point is a lie, so the timer expires with
            // an audit row instead (host check 2).
            if !rows.is_empty() && rows.iter().all(|(_, l)| !l.is_some_and(|l| l.child_alive)) {
                expire(timer, now);
                actions.push(Action::Expire {
                    id: timer.id,
                    detail: PROCESS_GONE,
                });
                continue;
            }
            match timer.status {
                TimerStatus::Confirming => {
                    // Did ANY watched process produce bytes since the
                    // confirm window opened? That is the flicker.
                    let flickered = rows.iter().any(|(id, l)| {
                        l.is_some_and(|l| {
                            timer
                                .confirm_baseline
                                .get(id)
                                .is_some_and(|base| l.output_bytes != *base)
                        })
                    });
                    let still = condition_met(timer.kind, &rows, now, timer.idle_ms);
                    if flickered || !still {
                        timer.status = TimerStatus::Pending;
                        timer.confirm_until = None;
                        timer.confirm_baseline.clear();
                        timer.updated_at = now;
                        if !timer.rearm {
                            timer.armed = false;
                        }
                        actions.push(Action::Rearm {
                            id: timer.id,
                            armed: timer.armed,
                        });
                    } else if timer.confirm_until.is_some_and(|until| now >= until) {
                        fire(timer, now);
                        actions.push(Action::Fire {
                            id: timer.id,
                            reason: FireReason::Condition,
                        });
                    }
                }
                _ => {
                    if timer.armed && condition_met(timer.kind, &rows, now, timer.idle_ms) {
                        timer.status = TimerStatus::Confirming;
                        timer.confirm_until = Some(now.saturating_add(timer.confirm_ms));
                        timer.confirm_baseline = rows
                            .iter()
                            .filter_map(|(id, l)| l.map(|l| (*id, l.output_bytes)))
                            .collect();
                        timer.updated_at = now;
                        let until = timer.confirm_until.unwrap_or(now);
                        actions.push(Action::Confirm { id: timer.id, until });
                    }
                }
            }
        }
        actions
    }
}

/// `any`: at least one watched process idle. `all`: every watched process
/// idle (a process that has gone away counts as satisfied for `all` — there
/// is nothing left to wait for from it).
fn condition_met(kind: TimerKind, rows: &[(u32, Option<&Liveness>)], now: u64, idle_ms: u64) -> bool {
    match kind {
        TimerKind::IdleAny => rows
            .iter()
            .any(|(_, l)| l.is_some_and(|l| is_idle(l, now, idle_ms))),
        // (`map_or(true, …)`, not `is_none_or`: the workspace MSRV is 1.77.)
        TimerKind::IdleAll => {
            !rows.is_empty()
                && rows
                    .iter()
                    .all(|(_, l)| l.map_or(true, |l| !l.child_alive || is_idle(l, now, idle_ms)))
        }
        TimerKind::Delay => false,
    }
}

/// Apply a firing to the timer itself (the delivery is the shell's job).
fn fire(timer: &mut Timer, now: u64) {
    timer.fired_count += 1;
    timer.updated_at = now;
    timer.confirm_until = None;
    timer.confirm_baseline.clear();
    match timer.repeat_every_ms {
        Some(interval) if interval > 0 && timer.kind == TimerKind::Delay => {
            timer.status = TimerStatus::Pending;
            timer.next_fire_at = Some(now.saturating_add(interval));
        }
        _ => {
            timer.status = TimerStatus::Fired;
            timer.next_fire_at = None;
        }
    }
}

fn expire(timer: &mut Timer, now: u64) {
    timer.status = TimerStatus::Expired;
    timer.confirm_until = None;
    timer.confirm_baseline.clear();
    timer.next_fire_at = None;
    timer.updated_at = now;
}

// ---- dedupe (pure) -----------------------------------------------------------

/// One firing the duplicate check looks at: delivered recently, or still
/// in flight (queued, or waiting at the ready gate).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentDelivery {
    pub firing_id: i64,
    pub timer_id: i64,
    pub process_id: u32,
    pub body: String,
    pub at: u64,
    pub in_flight: bool,
}

/// The FIRING whose identical body reached (or is on its way to) the same
/// process, if any — the queued-duplicate re-delivery bug, refused
/// server-side. Rules:
/// - an in-flight firing of the SAME timer always coalesces (a repeating
///   timer never queues a second body behind one still waiting at the gate);
/// - with `dedupe_ms > 0`, an in-flight firing with the same body, or one
///   DELIVERED less than `dedupe_ms` ago, coalesces too — except a repeating
///   timer's own completed firing (a 2 s `loop` under a 5 s window must not
///   deliver once and go silent).
pub fn coalesced_with(
    recent: &[RecentDelivery],
    timer_id: i64,
    repeating: bool,
    process_id: u32,
    body: &str,
    now: u64,
    dedupe_ms: u64,
) -> Option<i64> {
    recent
        .iter()
        .filter(|r| r.process_id == process_id)
        .filter(|r| {
            if r.in_flight {
                r.timer_id == timer_id || (dedupe_ms > 0 && r.body == body)
            } else {
                dedupe_ms > 0
                    && r.body == body
                    && now.saturating_sub(r.at) < dedupe_ms
                    && !(repeating && r.timer_id == timer_id)
            }
        })
        .max_by_key(|r| (r.at, r.firing_id))
        .map(|r| r.firing_id)
}

// ---- persistence -------------------------------------------------------------

/// The state of one audit row. Stored in `timer_firings.delivered` as
/// 1 / 0 / 2 / 3 (the column predates the two extra states and keeps its
/// NOT NULL).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryState {
    /// Persisted BEFORE the body is queued; the receipt turns it into
    /// `Delivered` or `Failed`. A restart closes any it finds as `Failed`
    /// with [`INTERRUPTED`].
    InFlight,
    Delivered,
    Failed,
    /// Not written: an identical body was delivered / is in flight
    /// (`coalesced_with` names that firing).
    Coalesced,
}

impl DeliveryState {
    fn as_i64(self) -> i64 {
        match self {
            DeliveryState::Failed => 0,
            DeliveryState::Delivered => 1,
            DeliveryState::InFlight => 2,
            DeliveryState::Coalesced => 3,
        }
    }

    fn from_i64(v: i64) -> Self {
        match v {
            1 => DeliveryState::Delivered,
            2 => DeliveryState::InFlight,
            3 => DeliveryState::Coalesced,
            _ => DeliveryState::Failed,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            DeliveryState::InFlight => "in_flight",
            DeliveryState::Delivered => "delivered",
            DeliveryState::Failed => "failed",
            DeliveryState::Coalesced => "coalesced",
        }
    }

    /// The wire `delivered` field: `true` / `false` / `null` (in flight or
    /// coalesced — nothing was typed by THIS firing).
    fn delivered(self) -> Option<bool> {
        match self {
            DeliveryState::Delivered => Some(true),
            DeliveryState::Failed => Some(false),
            DeliveryState::InFlight | DeliveryState::Coalesced => None,
        }
    }
}

/// One row of `timer_firings` — the audit log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Firing {
    pub id: i64,
    pub timer_id: i64,
    pub at: u64,
    pub reason: FireReason,
    pub process_id: u32,
    pub state: DeliveryState,
    /// Why it was not delivered ("process gone", "not ready", …), or the
    /// expiry detail. `None` on a clean delivery.
    pub detail: Option<String>,
    /// The FIRING this one was coalesced with.
    pub coalesced_with: Option<i64>,
    /// The receipt (`seq_before`/`seq_after`/`had_output_within_ms`).
    pub receipt: Option<Value>,
}

impl Firing {
    fn new(timer: &Timer, at: u64, reason: FireReason, state: DeliveryState, detail: Option<String>) -> Self {
        Self {
            id: 0,
            timer_id: timer.id,
            at,
            reason,
            process_id: timer.delivery_process_id,
            state,
            detail,
            coalesced_with: None,
            receipt: None,
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "firing_id": self.id,
            "process_id": self.process_id,
            "status": self.state.as_str(),
            "delivered": self.state.delivered(),
            "receipt": self.receipt,
            "reason": self.reason.as_str(),
            "error": self.detail,
            "coalesced_with": self.coalesced_with,
            "at": self.at,
        })
    }
}

/// Timers in `coordination.db`. Shares the ONE connection mutex the
/// coordination store owns, so the whole file still has a single writer.
#[derive(Clone, Default)]
pub struct TimerStore {
    coord: Coordination,
}

fn to_json_text<T: Serialize>(v: &T) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "[]".into())
}

fn from_json_text<T: serde::de::DeserializeOwned + Default>(text: &str) -> T {
    serde_json::from_str(text).unwrap_or_default()
}

fn placeholders(n: usize) -> String {
    (1..=n).map(|i| format!("?{i}")).collect::<Vec<_>>().join(",")
}

/// INSERT (id 0) or UPDATE inside an open transaction. Returns the row id.
fn write_timer(tx: &rusqlite::Transaction<'_>, timer: &Timer) -> Result<i64> {
    let watch = to_json_text(&timer.watch);
    let watch_uuids = to_json_text(&timer.watch_uuids);
    let ignored = to_json_text(&timer.ignored.iter().collect::<Vec<_>>());
    let kind = timer.kind.as_str();
    let status = timer.status.as_str();
    let params = rusqlite::params![
        timer.id, timer.name, kind, timer.owner, timer.project_id, timer.delivery_process_id, timer.body,
        watch, ignored, timer.delay_ms as i64,
        timer.repeat_every_ms.map(|v| v as i64), timer.next_fire_at.map(|v| v as i64),
        timer.deadline_at.map(|v| v as i64), timer.idle_ms as i64, timer.confirm_ms as i64,
        timer.rearm as i64, timer.armed as i64, status, timer.fired_count as i64,
        timer.remaining_ms.map(|v| v as i64), timer.created_at as i64, timer.updated_at as i64,
        timer.delivery_uuid, watch_uuids,
    ];
    if timer.id == 0 {
        tx.execute(
            "INSERT INTO timers (name, kind, owner, project_id, delivery_process_id, body, watch, ignored,
                delay_ms, repeat_every_ms, next_fire_at, deadline_at, idle_ms, confirm_ms, rearm, armed,
                status, fired_count, remaining_ms, created_at, updated_at, delivery_uuid, watch_uuids)
             VALUES (?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24)",
            params,
        )?;
        Ok(tx.last_insert_rowid())
    } else {
        tx.execute(
            "UPDATE timers SET name=?2, kind=?3, owner=?4, project_id=?5, delivery_process_id=?6, body=?7,
                watch=?8, ignored=?9, delay_ms=?10, repeat_every_ms=?11, next_fire_at=?12, deadline_at=?13,
                idle_ms=?14, confirm_ms=?15, rearm=?16, armed=?17, status=?18, fired_count=?19,
                remaining_ms=?20, updated_at=?22, delivery_uuid=?23, watch_uuids=?24 WHERE id=?1",
            params,
        )?;
        Ok(timer.id)
    }
}

fn insert_firing(tx: &rusqlite::Transaction<'_>, firing: &Firing) -> Result<i64> {
    let receipt = firing.receipt.as_ref().map(Value::to_string);
    tx.execute(
        "INSERT INTO timer_firings (timer_id, at, reason, process_id, delivered, detail, coalesced_with, receipt)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        rusqlite::params![
            firing.timer_id, firing.at as i64, firing.reason.as_str(), firing.process_id as i64,
            firing.state.as_i64(), firing.detail, firing.coalesced_with, receipt,
        ],
    )?;
    Ok(tx.last_insert_rowid())
}

impl TimerStore {
    pub fn new(coord: Coordination) -> Self {
        Self { coord }
    }

    /// INSERT (id 0) or UPDATE. Returns the row id.
    pub fn save(&self, timer: &Timer) -> Result<i64> {
        self.coord.with_tx(|tx| write_timer(tx, timer))
    }

    /// Review fix: the fired/advanced timer row AND its in-flight audit row
    /// in ONE transaction, before any delivery is attempted. Returns the
    /// firing row id the receipt will update.
    pub fn begin_firing(&self, timer: &Timer, firing: &Firing) -> Result<i64> {
        self.coord.with_tx(|tx| {
            write_timer(tx, timer)?;
            insert_firing(tx, firing)
        })
    }

    /// Every stored timer, oldest id first.
    pub fn load_all(&self) -> Result<Vec<Timer>> {
        self.coord.with_tx(|tx| {
            let mut stmt = tx.prepare(
                "SELECT id, name, kind, owner, project_id, delivery_process_id, body, watch, ignored, delay_ms,
                        repeat_every_ms, next_fire_at, deadline_at, idle_ms, confirm_ms, rearm, armed, status,
                        fired_count, remaining_ms, created_at, updated_at, delivery_uuid, watch_uuids
                 FROM timers ORDER BY id",
            )?;
            let rows = stmt.query_map([], |r| {
                let watch: String = r.get(7)?;
                let ignored: String = r.get(8)?;
                let kind: String = r.get(2)?;
                let status: String = r.get(17)?;
                let watch_uuids: String = r.get(23)?;
                Ok(Timer {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    kind: TimerKind::parse(&kind),
                    owner: r.get(3)?,
                    project_id: r.get(4)?,
                    delivery_process_id: r.get::<_, i64>(5)? as u32,
                    delivery_uuid: r.get(22)?,
                    body: r.get(6)?,
                    watch: from_json_text(&watch),
                    watch_uuids: from_json_text(&watch_uuids),
                    ignored: from_json_text::<Vec<u32>>(&ignored).into_iter().collect(),
                    delay_ms: r.get::<_, i64>(9)? as u64,
                    repeat_every_ms: r.get::<_, Option<i64>>(10)?.map(|v| v as u64),
                    next_fire_at: r.get::<_, Option<i64>>(11)?.map(|v| v as u64),
                    deadline_at: r.get::<_, Option<i64>>(12)?.map(|v| v as u64),
                    idle_ms: r.get::<_, i64>(13)? as u64,
                    confirm_ms: r.get::<_, i64>(14)? as u64,
                    rearm: r.get::<_, i64>(15)? != 0,
                    armed: r.get::<_, i64>(16)? != 0,
                    // `confirming` is a RUNTIME state: a restart re-observes
                    // the condition from scratch rather than trusting a
                    // confirm window that spans a shutdown.
                    status: match TimerStatus::parse(&status) {
                        TimerStatus::Confirming => TimerStatus::Pending,
                        other => other,
                    },
                    fired_count: r.get::<_, i64>(18)? as u64,
                    remaining_ms: r.get::<_, Option<i64>>(19)?.map(|v| v as u64),
                    created_at: r.get::<_, i64>(20)? as u64,
                    updated_at: r.get::<_, i64>(21)? as u64,
                    confirm_baseline: BTreeMap::new(),
                    confirm_until: None,
                    missed: None,
                })
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
        })
    }

    pub fn delete(&self, ids: &[i64]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        self.coord.with_tx(|tx| {
            for id in ids {
                tx.execute("DELETE FROM timer_firings WHERE timer_id = ?1", [id])?;
                tx.execute("DELETE FROM timers WHERE id = ?1", [id])?;
            }
            Ok(())
        })
    }

    /// Age-based prune of the audit rows themselves (a long-lived `loop`
    /// timer never leaves the table, but its firings must not grow forever).
    /// In-flight rows are never pruned.
    pub fn prune_firings(&self, cutoff: u64) -> Result<usize> {
        self.coord.with_tx(|tx| {
            Ok(tx.execute(
                "DELETE FROM timer_firings WHERE at < ?1 AND delivered != ?2",
                rusqlite::params![cutoff as i64, DeliveryState::InFlight.as_i64()],
            )?)
        })
    }

    /// Append an audit row; returns its id.
    pub fn record_firing(&self, firing: &Firing) -> Result<i64> {
        self.coord.with_tx(|tx| insert_firing(tx, firing))
    }

    /// The receipt (or the coalescing decision) for an in-flight row.
    pub fn finish_firing(
        &self,
        row: i64,
        state: DeliveryState,
        detail: Option<&str>,
        receipt: Option<&Value>,
        coalesced_with: Option<i64>,
    ) -> Result<()> {
        let receipt = receipt.map(Value::to_string);
        self.coord.with_tx(|tx| {
            tx.execute(
                "UPDATE timer_firings SET delivered=?2, detail=?3, receipt=?4, coalesced_with=?5 WHERE id=?1",
                rusqlite::params![row, state.as_i64(), detail, receipt, coalesced_with],
            )?;
            Ok(())
        })
    }

    /// Review fix: a restart closes every in-flight row it finds — the
    /// delivery may or may not have happened, and the audit must say so
    /// rather than leave a row that looks pending forever.
    pub fn close_interrupted(&self) -> Result<usize> {
        self.coord.with_tx(|tx| {
            Ok(tx.execute(
                "UPDATE timer_firings SET delivered=?1, detail=?2 WHERE delivered=?3",
                rusqlite::params![
                    DeliveryState::Failed.as_i64(),
                    INTERRUPTED,
                    DeliveryState::InFlight.as_i64()
                ],
            )?)
        })
    }

    /// The LAST firing of each listed timer (`timer_list`'s `delivery`).
    pub fn last_firings(&self, ids: &[i64]) -> Result<BTreeMap<i64, Firing>> {
        if ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        self.coord.with_tx(|tx| {
            let sql = format!(
                "SELECT id, timer_id, at, reason, process_id, delivered, detail, coalesced_with, receipt
                 FROM timer_firings WHERE timer_id IN ({}) ORDER BY timer_id, at, id",
                placeholders(ids.len())
            );
            let mut stmt = tx.prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(ids.iter()), |r| {
                let reason: String = r.get(3)?;
                let receipt: Option<String> = r.get(8)?;
                Ok(Firing {
                    id: r.get(0)?,
                    timer_id: r.get(1)?,
                    at: r.get::<_, i64>(2)? as u64,
                    reason: FireReason::parse(&reason),
                    process_id: r.get::<_, i64>(4)? as u32,
                    state: DeliveryState::from_i64(r.get(5)?),
                    detail: r.get(6)?,
                    coalesced_with: r.get(7)?,
                    receipt: receipt.and_then(|t| serde_json::from_str(&t).ok()),
                })
            })?;
            let mut out: BTreeMap<i64, Firing> = BTreeMap::new();
            for row in rows {
                let firing = row?;
                out.insert(firing.timer_id, firing);
            }
            Ok(out)
        })
    }

    /// The dedupe view (review fix): bodies DELIVERED since `since`, plus
    /// every firing still IN FLIGHT (any age — it is about to be typed),
    /// excluding `except` (the caller's own fresh row).
    pub fn recent_deliveries(&self, since: u64, except: i64) -> Result<Vec<RecentDelivery>> {
        self.coord.with_tx(|tx| {
            let mut stmt = tx.prepare(
                "SELECT f.id, f.timer_id, f.process_id, t.body, f.at, f.delivered FROM timer_firings f
                 JOIN timers t ON t.id = f.timer_id
                 WHERE f.id != ?2 AND f.coalesced_with IS NULL
                   AND ((f.delivered = 1 AND f.at >= ?1) OR f.delivered = 2)",
            )?;
            let rows = stmt.query_map(rusqlite::params![since as i64, except], |r| {
                Ok(RecentDelivery {
                    firing_id: r.get(0)?,
                    timer_id: r.get(1)?,
                    process_id: r.get::<_, i64>(2)? as u32,
                    body: r.get(3)?,
                    at: r.get::<_, i64>(4)? as u64,
                    in_flight: DeliveryState::from_i64(r.get(5)?) == DeliveryState::InFlight,
                })
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
        })
    }
}

// ---- delivery ----------------------------------------------------------------

/// The outcome of writing a body into a process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    pub delivered: bool,
    /// `None` on success.
    pub detail: Option<String>,
    pub receipt: Option<Value>,
}

/// How a timer body reaches its process. The real one is the prompt
/// path (ready gate → one atomic write of text + `\r` → receipt); tests
/// substitute a recorder.
pub trait Deliverer: Send + Sync {
    fn deliver(&self, process_id: u32, body: &str, timeout_ms: u64) -> Delivery;
}

/// The registry-backed deliverer: the SAME code path a queued spawn prompt
/// takes, under the QUIET-ONLY policy (review fix): a timer body waits for
/// genuine quiet up to `timer_delivery_timeout_ms` and is never typed into
/// an agent mid-turn on the spawn prompt's ready-by-timeout fallback.
pub struct PromptDeliverer {
    pub registry: Registry,
    pub settings: SettingsState,
}

impl Deliverer for PromptDeliverer {
    fn deliver(&self, process_id: u32, body: &str, timeout_ms: u64) -> Delivery {
        if self.registry.handle(process_id as TermId).is_none() {
            return Delivery {
                delivered: false,
                detail: Some(PROCESS_GONE.into()),
                receipt: None,
            };
        }
        let settings = self.settings.snapshot();
        let receipt = crate::agents::deliver_prompt_when_ready(
            &self.registry,
            process_id as TermId,
            body,
            crate::agents::DeliveryPolicy::quiet_only(
                u64::from(settings.agent_ready_quiet_ms),
                Duration::from_millis(timeout_ms),
            ),
        );
        let detail = match receipt.reason.as_deref() {
            Some("exited") => Some(PROCESS_GONE.to_owned()),
            Some("not-ready") => Some(format!("not ready within {timeout_ms} ms")),
            _ => None,
        };
        Delivery {
            delivered: receipt.delivered,
            detail: if receipt.delivered { None } else { detail },
            receipt: serde_json::to_value(&receipt).ok(),
        }
    }
}

// ---- the shell ---------------------------------------------------------------

/// A lossless park/wake pair (the `stats::StatsWaker` shape, with a timeout):
/// a wake that arrives before the park is remembered, so a `timer_set` can
/// never leave the scheduler asleep past its own deadline.
#[derive(Default)]
struct Waker {
    pending: Mutex<bool>,
    cv: Condvar,
}

impl Waker {
    fn wake(&self) {
        if let Ok(mut pending) = self.pending.lock() {
            *pending = true;
            self.cv.notify_all();
        }
    }

    /// Park until woken, or for `timeout` (`None` = until woken). A spurious
    /// wake-up re-checks `pending` and keeps waiting for the remainder.
    fn park(&self, timeout: Option<Duration>) {
        let Ok(mut pending) = self.pending.lock() else {
            return;
        };
        let deadline = timeout.map(|d| Instant::now() + d);
        while !*pending {
            match deadline {
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        break;
                    }
                    let Ok((guard, _)) = self.cv.wait_timeout(pending, deadline - now) else {
                        return;
                    };
                    pending = guard;
                }
                None => {
                    let Ok(guard) = self.cv.wait(pending) else {
                        return;
                    };
                    pending = guard;
                }
            }
        }
        *pending = false;
    }
}

/// What the delivery thread is handed.
struct Job {
    timer_id: i64,
    firing_row: i64,
    process_id: u32,
    body: String,
    timeout_ms: u64,
}

/// The managed timer state: the scheduler, its store, and the two threads.
/// `Default` builds an INERT service (no threads, no store) so `ControlState`
/// and the parity harness work without a Tauri runtime; every route on an
/// inert service answers a clear 500.
#[derive(Clone)]
pub struct TimerService {
    inner: Arc<ServiceInner>,
}

struct ServiceInner {
    sched: Mutex<Scheduler<Arc<dyn Clock>>>,
    store: TimerStore,
    probe: Arc<dyn OutputProbe>,
    waker: Arc<Waker>,
    jobs: Mutex<Option<mpsc::Sender<Job>>>,
    /// Knobs, read fresh on every use (the settings pane is live).
    knobs: Arc<dyn Fn() -> Knobs + Send + Sync>,
    running: bool,
}

/// The settings, snapshotted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Knobs {
    pub idle_threshold_ms: u64,
    pub confirm_ms: u64,
    pub delivery_timeout_ms: u64,
    pub dedupe_ms: u64,
    pub retention_ms: u64,
    /// Review fix — not a settings-pane knob (yet): [`DEFAULT_MISSED_GRACE_MS`].
    pub missed_grace_ms: u64,
}

impl Default for Knobs {
    fn default() -> Self {
        Self {
            idle_threshold_ms: 120_000,
            confirm_ms: 5_000,
            delivery_timeout_ms: 30_000,
            dedupe_ms: 5_000,
            retention_ms: 24 * 3_600_000,
            missed_grace_ms: DEFAULT_MISSED_GRACE_MS,
        }
    }
}

impl Knobs {
    pub fn of(settings: &project_model::settings::Settings) -> Self {
        Self {
            idle_threshold_ms: u64::from(settings.idle_threshold_ms),
            confirm_ms: u64::from(settings.timer_confirm_ms),
            delivery_timeout_ms: u64::from(settings.timer_delivery_timeout_ms),
            dedupe_ms: u64::from(settings.timer_dedupe_ms),
            retention_ms: u64::from(settings.timer_retention_hours) * 3_600_000,
            missed_grace_ms: DEFAULT_MISSED_GRACE_MS,
        }
    }
}

/// A probe with nothing in it (the inert default service).
struct NoProcesses;

impl OutputProbe for NoProcesses {
    fn liveness(&self, _: u32) -> Option<Liveness> {
        None
    }
    fn processes(&self) -> Vec<ProcessRow> {
        Vec::new()
    }
}

impl Default for TimerService {
    fn default() -> Self {
        Self {
            inner: Arc::new(ServiceInner {
                sched: Mutex::new(Scheduler::new(Arc::new(SystemClock) as Arc<dyn Clock>)),
                store: TimerStore::default(),
                probe: Arc::new(NoProcesses),
                waker: Arc::new(Waker::default()),
                jobs: Mutex::new(None),
                knobs: Arc::new(Knobs::default),
                running: false,
            }),
        }
    }
}

/// Errors the routes map: everything is a [`CoordError`] so the HTTP shape
/// matches the exactly.
fn not_running() -> CoordError {
    CoordError::Internal("the timer scheduler is not running in this instance".into())
}

fn live_names(probe: &dyn OutputProbe) -> String {
    probe
        .processes()
        .iter()
        .map(|p| match p.project_id {
            Some(pid) => format!("{}:{} (project {pid})", p.id, p.name),
            None => format!("{}:{}", p.id, p.name),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

impl TimerService {
    /// Build a service over an explicit clock/probe/deliverer and START its
    /// two threads. Tests call this with a fake clock and a fake probe; the
    /// app calls [`TimerService::start`].
    pub fn with_parts(
        clock: Arc<dyn Clock>,
        probe: Arc<dyn OutputProbe>,
        deliverer: Arc<dyn Deliverer>,
        store: TimerStore,
        knobs: Arc<dyn Fn() -> Knobs + Send + Sync>,
    ) -> Self {
        let (tx, rx) = mpsc::channel::<Job>();
        let service = Self {
            inner: Arc::new(ServiceInner {
                sched: Mutex::new(Scheduler::new(clock)),
                store,
                probe,
                waker: Arc::new(Waker::default()),
                jobs: Mutex::new(Some(tx)),
                knobs,
                running: true,
            }),
        };
        // The delivery thread. One thread, one queue: a firing whose target
        // never becomes ready blocks the NEXT delivery (bounded by
        // delivery_timeout_ms) but can never stall the scheduler.
        let deliver_store = service.inner.store.clone();
        std::thread::spawn(move || {
            while let Ok(job) = rx.recv() {
                let out = deliverer.deliver(job.process_id, &job.body, job.timeout_ms);
                let state = if out.delivered {
                    DeliveryState::Delivered
                } else {
                    DeliveryState::Failed
                };
                if let Err(err) = deliver_store.finish_firing(
                    job.firing_row,
                    state,
                    out.detail.as_deref(),
                    out.receipt.as_ref(),
                    None,
                ) {
                    log::warn!("[timers] timer {} firing not recorded: {err}", job.timer_id);
                }
            }
        });
        service
    }

    /// Start the scheduler thread. Separate from [`TimerService::with_parts`]
    /// on purpose: [`TimerService::restore`] MUST run first (otherwise the
    /// first tick could fire an overdue timer with `reason: "condition"`
    /// before the missed-timer pass ever sees it), and the tests drive the
    /// scheduler by hand with [`TimerService::tick_now`].
    pub fn spawn_scheduler(&self) {
        let ticker = self.clone();
        std::thread::spawn(move || ticker.run());
    }

    /// The app's wiring: system clock, registry probe, delivery,
    /// `coordination.db` persistence, live settings.
    pub fn start(registry: Registry, settings: SettingsState, coordination: Coordination) -> Self {
        let knob_settings = settings.clone();
        let service = Self::with_parts(
            Arc::new(SystemClock),
            Arc::new(RegistryProbe(registry.clone())),
            Arc::new(PromptDeliverer {
                registry,
                settings: settings.clone(),
            }),
            TimerStore::new(coordination),
            Arc::new(move || Knobs::of(&knob_settings.snapshot())),
        );
        service.restore();
        service.spawn_scheduler();
        service
    }

    fn knobs(&self) -> Knobs {
        (self.inner.knobs)()
    }

    /// Load persisted timers, close the in-flight firings a crash left open,
    /// and record a `reason: "missed"` firing (in flight, persisted first)
    /// for every absolute timer whose time passed while the app was down.
    /// Nothing is delivered here: the tick delivers when the target's spawn
    /// is live, or records `process gone` when the grace window closes.
    pub fn restore(&self) {
        let timers = match self.inner.store.load_all() {
            Ok(t) => t,
            Err(err) => {
                log::error!("[timers] cannot load persisted timers: {err}");
                return;
            }
        };
        match self.inner.store.close_interrupted() {
            Ok(n) if n > 0 => log::warn!("[timers] {n} in-flight firing(s) interrupted by the last shutdown"),
            Ok(_) => {}
            Err(err) => log::warn!("[timers] cannot close interrupted firings: {err}"),
        }
        let grace = self.knobs().missed_grace_ms;
        let (missed, now) = {
            let Ok(mut sched) = self.inner.sched.lock() else {
                return;
            };
            for timer in timers {
                sched.insert(timer);
            }
            (sched.recover_missed(grace), sched.now())
        };
        for id in missed {
            let snapshot = {
                let Ok(sched) = self.inner.sched.lock() else { return };
                sched.get(id).cloned()
            };
            let Some(timer) = snapshot else { continue };
            let firing = Firing::new(&timer, now, FireReason::Missed, DeliveryState::InFlight, None);
            match self.inner.store.begin_firing(&timer, &firing) {
                Ok(row) => {
                    if let Ok(mut sched) = self.inner.sched.lock() {
                        sched.set_missed_row(id, row);
                    }
                }
                Err(err) => log::warn!("[timers] missed firing of timer {id} not recorded: {err}"),
            }
        }
        self.inner.waker.wake();
    }

    /// The scheduler thread body: tick, execute, park.
    fn run(&self) {
        loop {
            let (actions, wait, doomed, cutoff) = {
                let Ok(mut sched) = self.inner.sched.lock() else {
                    return;
                };
                let actions = sched.tick(self.inner.probe.as_ref());
                let retention = self.knobs().retention_ms;
                let doomed = sched.prune(retention);
                (actions, sched.plan_wait(), doomed, sched.now().saturating_sub(retention))
            };
            if !doomed.is_empty() {
                if let Err(err) = self.inner.store.delete(&doomed) {
                    log::warn!("[timers] retention prune failed: {err}");
                }
                if let Err(err) = self.inner.store.prune_firings(cutoff) {
                    log::warn!("[timers] firing prune failed: {err}");
                }
            }
            self.execute(actions);
            self.inner.waker.park(wait);
        }
    }

    /// Perform the side effects of one tick: persist the new timer state
    /// where it matters (a firing, an expiry, a disarm — never the transient
    /// confirm/re-arm hops), record audit rows, queue deliveries. Runs with
    /// NO scheduler lock held.
    fn execute(&self, actions: Vec<Action>) {
        if actions.is_empty() {
            return;
        }
        let knobs = self.knobs();
        for action in actions {
            let id = action.id();
            // Copy the row out under a short guard; everything below is I/O.
            let snapshot = {
                let Ok(sched) = self.inner.sched.lock() else { return };
                sched.get(id).cloned()
            };
            let Some(timer) = snapshot else { continue };
            match action {
                Action::Confirm { .. } | Action::Rearm { armed: true, .. } => {}
                Action::Rearm { armed: false, .. } => {
                    if let Err(err) = self.inner.store.save(&timer) {
                        log::warn!("[timers] timer {id} not persisted: {err}");
                    }
                }
                Action::Expire { detail, .. } => {
                    let firing = Firing::new(
                        &timer,
                        timer.updated_at,
                        FireReason::Expired,
                        DeliveryState::Failed,
                        Some(detail.to_owned()),
                    );
                    if let Err(err) = self.inner.store.begin_firing(&timer, &firing) {
                        log::warn!("[timers] timer {id} expiry not recorded: {err}");
                    }
                }
                Action::Fire { reason, .. } => self.dispatch(&timer, reason, knobs),
            }
        }
    }

    /// Persist (timer row + in-flight audit row, one transaction), then
    /// validate the target, then dedupe, then queue the delivery. The order
    /// is the review-fix-8 guarantee: nothing is typed that the db does not
    /// already know about.
    fn dispatch(&self, timer: &Timer, reason: FireReason, knobs: Knobs) {
        let now = {
            let Ok(sched) = self.inner.sched.lock() else { return };
            sched.now()
        };
        let missed = if reason == FireReason::Missed {
            self.inner.sched.lock().ok().and_then(|mut s| s.take_missed(timer.id))
        } else {
            None
        };
        let row = match missed.map(|m| m.row).filter(|row| *row > 0) {
            Some(row) => row,
            None => {
                let firing = Firing::new(timer, now, reason, DeliveryState::InFlight, None);
                match self.inner.store.begin_firing(timer, &firing) {
                    Ok(row) => row,
                    Err(err) => {
                        log::warn!("[timers] timer {} firing not recorded; NOT delivered: {err}", timer.id);
                        return;
                    }
                }
            }
        };
        let finish = |state: DeliveryState, detail: &str, coalesced: Option<i64>| {
            if let Err(err) = self.inner.store.finish_firing(row, state, Some(detail), None, coalesced) {
                log::warn!("[timers] timer {} firing not recorded: {err}", timer.id);
            }
        };
        // Review fix: the body goes to the SPAWN the timer was pinned to,
        // or nowhere. An unpinned row (pre-schema-3) is never delivered.
        let target_live = timer.delivery_uuid.as_deref().is_some_and(|uuid| {
            self.inner
                .probe
                .liveness(timer.delivery_process_id)
                .is_some_and(|l| l.child_alive && l.uuid == uuid)
        });
        if !target_live {
            finish(DeliveryState::Failed, PROCESS_GONE, None);
            return;
        }
        // Review fix: dedupe against delivered AND in-flight bodies.
        let recent = self
            .inner
            .store
            .recent_deliveries(now.saturating_sub(knobs.dedupe_ms), row)
            .unwrap_or_default();
        if let Some(other) = coalesced_with(
            &recent,
            timer.id,
            timer.repeat_every_ms.is_some(),
            timer.delivery_process_id,
            &timer.body,
            now,
            knobs.dedupe_ms,
        ) {
            finish(
                DeliveryState::Coalesced,
                &format!("coalesced with firing {other} (identical body to the same process)"),
                Some(other),
            );
            return;
        }
        let job = Job {
            timer_id: timer.id,
            firing_row: row,
            process_id: timer.delivery_process_id,
            body: timer.body.clone(),
            timeout_ms: knobs.delivery_timeout_ms,
        };
        let sent = self
            .inner
            .jobs
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|tx| tx.send(job)));
        if !matches!(sent, Some(Ok(()))) {
            finish(DeliveryState::Failed, "the delivery thread is not running", None);
        }
    }

    // -- the API the routes call ---------------------------------------------

    /// The project an actor's own process belongs to (review fix: the
    /// default scope when a call names none).
    fn project_of_actor(&self, actor: &str) -> Option<i64> {
        let pid = actor.parse::<u32>().ok()?;
        self.inner
            .probe
            .processes()
            .into_iter()
            .find(|p| p.id == pid)
            .and_then(|p| p.project_id)
            .map(i64::from)
    }

    /// The delivery target must be LIVE at schedule time, and is pinned to
    /// its spawn — a timer to a process that does not exist is timer 116.
    fn pin_target(&self, delivery_process_id: u32) -> Result<String> {
        self.inner
            .probe
            .liveness(delivery_process_id)
            .filter(|l| l.child_alive)
            .map(|l| l.uuid)
            .ok_or_else(|| {
                CoordError::NotFound(format!(
                    "no live process {delivery_process_id} to deliver to (live: [{}])",
                    live_names(self.inner.probe.as_ref())
                ))
            })
    }

    /// `timer_set`.
    #[allow(clippy::too_many_arguments)]
    pub fn set(
        &self,
        owner: &str,
        project_id: Option<i64>,
        delivery_process_id: u32,
        body: String,
        delay_ms: u64,
        repeat_every_ms: Option<u64>,
        name: Option<String>,
    ) -> Result<Value> {
        if !self.inner.running {
            return Err(not_running());
        }
        if body.is_empty() {
            return Err(CoordError::Invalid("`body` must not be empty".into()));
        }
        let uuid = self.pin_target(delivery_process_id)?;
        let now = self.lock()?.now();
        // `loop: true` = repeat every delay_ms; `repeat_every_ms` overrides
        // the interval. The route resolves which of the two applies.
        let mut timer = Timer::delay(owner, delivery_process_id, body, delay_ms, now).pinned(&uuid);
        timer.project_id = project_id.or_else(|| self.project_of_actor(owner));
        timer.repeat_every_ms = repeat_every_ms;
        timer.name = name;
        timer.id = self.inner.store.save(&timer)?;
        let row = {
            let mut sched = self.lock()?;
            sched.insert(timer.clone());
            timer_json(&timer, None)
        };
        self.inner.waker.wake();
        Ok(row)
    }

    /// `timer_fire_when_idle_any` / `_all`.
    #[allow(clippy::too_many_arguments)]
    pub fn fire_when_idle(
        &self,
        kind: TimerKind,
        owner: &str,
        project_id: Option<i64>,
        delivery_process_id: u32,
        body: String,
        processes: &[String],
        max_wait_ms: u64,
        idle_ms: Option<u64>,
        confirm_ms: Option<u64>,
        rearm: Option<bool>,
        name: Option<String>,
    ) -> Result<Value> {
        if !self.inner.running {
            return Err(not_running());
        }
        if body.is_empty() {
            return Err(CoordError::Invalid("`body` must not be empty".into()));
        }
        if processes.is_empty() {
            return Err(CoordError::Invalid("`processes` must not be empty".into()));
        }
        let knobs = self.knobs();
        let idle_ms = idle_ms.unwrap_or(knobs.idle_threshold_ms);
        let confirm_ms = confirm_ms.unwrap_or(knobs.confirm_ms);
        let project_id = project_id.or_else(|| self.project_of_actor(owner));
        let uuid = self.pin_target(delivery_process_id)?;
        // The rule: ids OR names — names within the caller's project first.
        let watched = resolve_processes(self.inner.probe.as_ref(), processes, project_id)?;
        let watch: Vec<u32> = watched.iter().map(|p| p.id).collect();
        let watch_uuids: Vec<String> = watched.iter().map(|p| p.uuid.clone()).collect();
        let now = self.lock()?.now();
        let live = self.inner.probe.liveness_many(&watch);
        let already_idle: Vec<u32> = watch
            .iter()
            .copied()
            .filter(|id| live.get(id).is_some_and(|l| is_idle(l, now, idle_ms)))
            .collect();
        // `all` counts already-idle processes as satisfied: when EVERYTHING
        // is idle there is nothing to wait for and no timer is created.
        if kind == TimerKind::IdleAll && already_idle.len() == watch.len() {
            return Ok(json!({
                "timer_id": Value::Null,
                "status": "already_satisfied",
                "already_idle": already_idle,
                "waiting_on": [],
                "delivery_process_id": delivery_process_id,
                "project_id": project_id,
                "note": "every watched process was already idle; no timer was created and no body was delivered",
            }));
        }
        let ignored: BTreeSet<u32> = if kind == TimerKind::IdleAny {
            already_idle.iter().copied().collect()
        } else {
            BTreeSet::new()
        };
        let mut timer = Timer::idle(
            kind,
            owner,
            delivery_process_id,
            body,
            watch,
            ignored,
            max_wait_ms,
            idle_ms,
            confirm_ms,
            rearm.unwrap_or(true),
            now,
        )
        .pinned(&uuid);
        timer.watch_uuids = watch_uuids;
        timer.project_id = project_id;
        timer.name = name;
        timer.id = self.inner.store.save(&timer)?;
        let waiting_on = timer.waiting_on();
        {
            let mut sched = self.lock()?;
            sched.insert(timer.clone());
        }
        self.inner.waker.wake();
        let note = if kind == TimerKind::IdleAny && !already_idle.is_empty() {
            if waiting_on.is_empty() {
                "every watched process was ALREADY idle; `any` waits for a NEW idle transition (a process that becomes busy and then goes quiet again), else this timer fires at its deadline"
            } else {
                "processes already idle at schedule time are ignored until they become busy again; `any` waits for a NEW idle transition"
            }
        } else {
            "idle is derived from the pty byte stream only, and a met condition is re-checked after confirm_ms before the body is delivered"
        };
        let mut row = timer_json(&timer, None);
        row["status"] = json!("scheduled");
        row["already_idle"] = json!(already_idle);
        row["waiting_on"] = json!(waiting_on);
        row["note"] = json!(note);
        Ok(row)
    }

    /// `timer_list`. Owner-scoped unless `all` (the orchestrator seat);
    /// pending timers first, newest first within each half; paged by
    /// `limit` + `offset` with the unpaged `total`.
    #[allow(clippy::too_many_arguments)]
    pub fn list(
        &self,
        owner: &str,
        include_fired: bool,
        all: bool,
        limit: Option<usize>,
        offset: Option<usize>,
        project_id: Option<i64>,
    ) -> Result<Value> {
        if !self.inner.running {
            return Err(not_running());
        }
        let limit = limit.unwrap_or(DEFAULT_LIST_LIMIT).min(MAX_LIST_LIMIT);
        let offset = offset.unwrap_or(0);
        let mut rows: Vec<Timer> = {
            let sched = self.lock()?;
            sched
                .rows()
                .filter(|t| all || t.owner == owner)
                .filter(|t| project_id.is_none() || t.project_id == project_id)
                .filter(|t| include_fired || !t.status.is_terminal())
                .cloned()
                .collect()
        };
        rows.sort_by(|a, b| {
            a.status
                .is_terminal()
                .cmp(&b.status.is_terminal())
                .then(b.created_at.cmp(&a.created_at))
                .then(b.id.cmp(&a.id))
        });
        let total = rows.len();
        let page: Vec<Timer> = rows.into_iter().skip(offset).take(limit).collect();
        let ids: Vec<i64> = page.iter().map(|t| t.id).collect();
        let firings = self.inner.store.last_firings(&ids).unwrap_or_default();
        let timers: Vec<Value> = page
            .iter()
            .map(|t| timer_json(t, firings.get(&t.id)))
            .collect();
        Ok(json!({"timers": timers, "total": total, "limit": limit, "offset": offset}))
    }

    /// Cancel / pause / resume, owner-scoped. `user` (a bare curl, or an
    /// agent with no `CHAPPA_AI_PROCESS_ID`) is the orchestrator seat and may
    /// act on any timer.
    pub fn lifecycle(&self, owner: &str, id: i64, op: Lifecycle) -> Result<Value> {
        if !self.inner.running {
            return Err(not_running());
        }
        let timer = {
            let mut sched = self.lock()?;
            match sched.get(id) {
                None => return Err(CoordError::NotFound(format!("no such timer: {id}"))),
                Some(t) if t.owner != owner && owner != USER_ACTOR => {
                    return Err(CoordError::NotFound(format!(
                        "no such timer for this actor: {id} (it belongs to {})",
                        t.owner
                    )))
                }
                Some(_) => {}
            }
            match op {
                Lifecycle::Cancel => sched.cancel(id),
                Lifecycle::Pause => sched.pause(id),
                Lifecycle::Resume => sched.resume(id),
            }
            .cloned()
        };
        let Some(timer) = timer else {
            return Err(CoordError::NotFound(format!("no such timer: {id}")));
        };
        self.inner.store.save(&timer)?;
        self.inner.waker.wake();
        let done = match op {
            Lifecycle::Cancel => timer.status == TimerStatus::Cancelled,
            Lifecycle::Pause => timer.status == TimerStatus::Paused,
            Lifecycle::Resume => timer.status == TimerStatus::Pending,
        };
        let mut row = json!({
            "timer_id": timer.id,
            "project_id": timer.project_id,
            "status": timer.status.as_str(),
        });
        row[op.field()] = json!(done);
        Ok(row)
    }

    /// Tests: run one tick synchronously (the scheduler thread does the same
    /// thing on its own schedule).
    pub fn tick_now(&self) {
        let actions = {
            let Ok(mut sched) = self.inner.sched.lock() else { return };
            sched.tick(self.inner.probe.as_ref())
        };
        self.execute(actions);
    }

    /// Tests: the pruning half of a tick.
    pub fn prune_now(&self) {
        let retention = self.knobs().retention_ms;
        let (doomed, cutoff) = {
            let Ok(mut sched) = self.inner.sched.lock() else { return };
            (sched.prune(retention), sched.now().saturating_sub(retention))
        };
        let _ = self.inner.store.delete(&doomed);
        let _ = self.inner.store.prune_firings(cutoff);
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Scheduler<Arc<dyn Clock>>>> {
        self.inner
            .sched
            .lock()
            .map_err(|_| CoordError::Internal("timer scheduler poisoned".into()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifecycle {
    Cancel,
    Pause,
    Resume,
}

impl Lifecycle {
    fn field(self) -> &'static str {
        match self {
            Lifecycle::Cancel => "cancelled",
            Lifecycle::Pause => "paused",
            Lifecycle::Resume => "resumed",
        }
    }
}

/// The rule: `processes` accepts ids or names. A name is resolved within
/// `project_id` first (review fix); with no scope, or no match in scope,
/// every project is searched and a name that matches in MORE than one is an
/// error naming the candidates — never a silent pick. A token that matches
/// nothing is an error naming what IS there — silently watching an empty
/// set is how a wake-up disappears.
pub fn resolve_processes(
    probe: &dyn OutputProbe,
    wanted: &[String],
    project_id: Option<i64>,
) -> Result<Vec<ProcessRow>> {
    let live = probe.processes();
    let scope = project_id.and_then(|p| u32::try_from(p).ok());
    let mut out: Vec<ProcessRow> = Vec::new();
    for token in wanted {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        let by_id = token
            .parse::<u32>()
            .ok()
            .and_then(|id| live.iter().find(|p| p.id == id));
        let resolved = match by_id {
            Some(p) => p,
            None => {
                let mut named: Vec<&ProcessRow> = live.iter().filter(|p| p.name == token).collect();
                if named.is_empty() {
                    named = live.iter().filter(|p| p.name.eq_ignore_ascii_case(token)).collect();
                }
                if scope.is_some() {
                    let in_scope: Vec<&ProcessRow> =
                        named.iter().copied().filter(|p| p.project_id == scope).collect();
                    if !in_scope.is_empty() {
                        named = in_scope;
                    }
                }
                match named.as_slice() {
                    [one] => *one,
                    [] => {
                        return Err(CoordError::NotFound(format!(
                            "no process `{token}` (ids or names accepted; live: [{}])",
                            live_names(probe)
                        )))
                    }
                    many => {
                        let candidates: Vec<String> = many
                            .iter()
                            .map(|p| match p.project_id {
                                Some(pid) => format!("{} (project {pid})", p.id),
                                None => format!("{} (no project)", p.id),
                            })
                            .collect();
                        return Err(CoordError::Invalid(format!(
                            "`{token}` is ambiguous across projects: [{}] — pass project_id or the process id",
                            candidates.join(", ")
                        )));
                    }
                }
            }
        };
        if !out.iter().any(|p| p.id == resolved.id) {
            out.push(resolved.clone());
        }
    }
    if out.is_empty() {
        return Err(CoordError::Invalid(
            "`processes` resolved to nothing".into(),
        ));
    }
    Ok(out)
}

/// The `timer_list` row shape, also used by the scheduling replies so a
/// caller sees the same keys everywhere.
pub fn timer_json(timer: &Timer, last: Option<&Firing>) -> Value {
    json!({
        "id": timer.id,
        "timer_id": timer.id,
        "name": timer.name,
        "kind": timer.kind.as_str(),
        "status": timer.status.as_str(),
        "owner": timer.owner,
        "project_id": timer.project_id,
        "body": timer.body,
        "delivery_process_id": timer.delivery_process_id,
        "delivery_uuid": timer.delivery_uuid,
        "delivery": last.map(Firing::to_json),
        "fired_count": timer.fired_count,
        "next_fire_at": timer.next_fire_at,
        "deadline_at": timer.deadline_at,
        "repeat_every_ms": timer.repeat_every_ms,
        "delay_ms": timer.delay_ms,
        "watch": timer.watch,
        "waiting_on": timer.waiting_on(),
        "idle_ms": timer.idle_ms,
        "confirm_ms": timer.confirm_ms,
        "rearm": timer.rearm,
        "created_at": timer.created_at,
        "updated_at": timer.updated_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live(uuid: &str, last: Option<u64>, alive: bool) -> Liveness {
        Liveness {
            uuid: uuid.into(),
            child_alive: alive,
            last_output_at: last,
            output_bytes: 10,
        }
    }

    #[test]
    fn idle_needs_output_first() {
        let booting = Liveness {
            uuid: "u".into(),
            child_alive: true,
            last_output_at: None,
            output_bytes: 0,
        };
        assert!(!is_idle(&booting, 1_000_000, 1_000), "booting is not idle");
        let quiet = live("u", Some(1_000), true);
        assert!(!is_idle(&quiet, 1_500, 1_000));
        assert!(is_idle(&quiet, 2_000, 1_000));
        let dead = live("u", Some(1_000), false);
        assert!(!is_idle(&dead, 9_999, 1_000), "a dead child is not idle");
    }

    #[test]
    fn a_pinned_lookup_refuses_a_reused_id() {
        let mut table = BTreeMap::new();
        table.insert(7, live("spawn-b", Some(0), true));
        assert!(pinned(&table, 7, Some("spawn-a")).is_none(), "7 belongs to another spawn now");
        assert!(pinned(&table, 7, Some("spawn-b")).is_some());
        assert!(pinned(&table, 7, None).is_some(), "unpinned = any spawn under that id");
        assert!(pinned(&table, 8, None).is_none());
    }

    fn recent(firing_id: i64, timer_id: i64, body: &str, at: u64, in_flight: bool) -> RecentDelivery {
        RecentDelivery {
            firing_id,
            timer_id,
            process_id: 1,
            body: body.into(),
            at,
            in_flight,
        }
    }

    #[test]
    fn dedupe_window_is_exclusive_at_the_edge() {
        let log = vec![recent(40, 4, "go", 1_000, false)];
        assert_eq!(coalesced_with(&log, 9, false, 1, "go", 4_000, 5_000), Some(40));
        assert_eq!(coalesced_with(&log, 9, false, 1, "go", 6_000, 5_000), None);
        assert_eq!(coalesced_with(&log, 9, false, 2, "go", 4_000, 5_000), None);
        assert_eq!(coalesced_with(&log, 9, false, 1, "other", 4_000, 5_000), None);
        assert_eq!(coalesced_with(&log, 9, false, 1, "go", 1_001, 0), None, "0 disables");
        // A repeating timer's own COMPLETED firing never coalesces it…
        assert_eq!(coalesced_with(&log, 4, true, 1, "go", 2_000, 5_000), None);
        // …but its own IN-FLIGHT firing always does, window or not.
        let inflight = vec![recent(41, 4, "go", 1_000, true)];
        assert_eq!(coalesced_with(&inflight, 4, true, 1, "go", 60_000, 0), Some(41));
        // An in-flight identical body from another timer coalesces too.
        assert_eq!(coalesced_with(&inflight, 9, false, 1, "go", 60_000, 5_000), Some(41));
        assert_eq!(coalesced_with(&inflight, 9, false, 1, "go", 60_000, 0), None, "0 disables the body rule");
    }
}
