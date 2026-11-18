//! What status shows but must not wait for, refreshed off the request path.
//!
//! - **Channel digests** for the `newer prebuilt` badge: a registry round
//!   trip (`manifest inspect`) can take over ten seconds per image, so the
//!   Environment payload only reads [`DigestCache`]; a reference it does not
//!   know yet is queued and shows no badge until the refresher has asked.
//!   Entries are refreshed after [`DIGEST_TTL`].
//! - **Engine facts** (version, OS mode, memory) for the facts section and
//!   the reachability alerts: `docker info` alone can take seconds, so the
//!   same refresher asks every [`FACTS_TTL`].
//!
//! - **Script outputs** of script components: a status script may take
//!   seconds and a `plan` script a minute, so the Environment payload and
//!   the confirms read the last answer ([`ScriptOutputs`]); a run not seen
//!   before is queued and reads as "not answered yet". Status is asked
//!   again after [`SCRIPT_STATUS_TTL`], plans after [`SCRIPT_PLAN_TTL`],
//!   both right after a job; an answer nobody asked for in [`SCRIPT_IDLE`]
//!   is dropped.
//! - **Periodic work** registered with [`Background::add_periodic`]: the
//!   source's `every:` data-safety schedule looks here for due checks.
//!
//! **Two kinds of thread.** Digests, facts and periodic work run on one
//! refresher thread. Scripts run on their own fixed pool of
//! [`SCRIPT_WORKERS`] threads, so a status script near its time limit never
//! holds up a digest or a fact, and a stack with many script components
//! cannot use more than the pool:
//!
//! - **One run per instance.** A stack instance has one script in flight at
//!   most, status and plan sharing that slot: a team's scripts of one stack
//!   read (and often lock) the same state, so running two of them side by
//!   side races them against each other and doubles the load on one stack
//!   while others wait. The cost, a slow plan delaying its stack's status,
//!   is what the "status is slow" note says.
//! - **Skipped, not queued.** A periodic refresh whose instance is still
//!   busy when its turn comes skips that turn and waits for the next one; a
//!   queue would only replay stale turns back to back once the slow run
//!   ends. A first answer, and the answer wanted again after a job, is
//!   different: someone is waiting for it, so it waits for the slot.
//! - **Staggered turns.** Each run's periodic turns sit on its own grid,
//!   offset by a phase taken from a hash of the run (and a seed), so many
//!   stacks refreshed every ten seconds do not all start their scripts in
//!   the same second ([`phase`], [`next_turn`]).
//! - **Slow runs show.** A run in flight longer than the status interval
//!   marks the instance's rows with a "status is slow" note while they keep
//!   their last known state ([`ScriptOutputs::slow`]). Its own time limit
//!   still ends it (and every process it started).
//!
//! Status registers what it wants on every call ([`Background::want_digest`],
//! [`Background::want_engine`]); the threads pick it up within [`TICK`].
//! Nothing here blocks a reader for longer than a lock held around a map
//! lookup.

use indexmap::IndexMap;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::thread;
use std::time::{Duration, Instant};

use stack_core::checks::{engine_facts, EngineFacts};
use stack_core::definition::{ContainerOs, EngineRef};
use stack_core::images::resolve_digest;
use stack_core::runner::PrebuiltDigests;
use stack_core::script::{
    run_plan, run_status, PlanPreview, ScriptOutputs, ScriptReport, ScriptRun,
};

use crate::world::{RunningScripts, World};

/// How long a channel digest is trusted before it is asked again.
pub const DIGEST_TTL: Duration = Duration::from_secs(10 * 60);
/// How long engine facts are trusted before they are asked again.
pub const FACTS_TTL: Duration = Duration::from_secs(60);
/// How often the refresher looks for work.
pub const TICK: Duration = Duration::from_millis(500);
/// How long a script component's status report is shown before its
/// status script runs again.
pub const SCRIPT_STATUS_TTL: Duration = Duration::from_secs(10);
/// How long a plan script's preview is kept before it runs again.
pub const SCRIPT_PLAN_TTL: Duration = Duration::from_secs(60);
/// A script answer nobody asked for this long is forgotten.
pub const SCRIPT_IDLE: Duration = Duration::from_secs(10 * 60);
/// The threads status and plan scripts run on.
pub const SCRIPT_WORKERS: usize = 4;
/// How long shutdown waits for a script worker to finish its run before
/// leaving it behind. Shutdown stops every script in flight first, so a
/// worker normally ends well within it.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// How often each kind of answer is asked again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Intervals {
    pub digest: Duration,
    pub facts: Duration,
    pub status: Duration,
    pub plan: Duration,
}

impl Default for Intervals {
    fn default() -> Self {
        Intervals {
            digest: DIGEST_TTL,
            facts: FACTS_TTL,
            status: SCRIPT_STATUS_TTL,
            plan: SCRIPT_PLAN_TTL,
        }
    }
}

#[derive(Debug, Clone)]
enum ScriptAnswer {
    Status(ScriptReport),
    Plan(PlanPreview),
}

#[derive(Debug, Clone)]
struct ScriptEntry {
    run: ScriptRun,
    plan: bool,
    answer: Option<ScriptAnswer>,
    asked: Instant,
    /// When its next periodic turn is; `None` = wanted now (never answered,
    /// or asked again after a job).
    due: Option<Instant>,
    running: bool,
    /// Asked again while it was running: wanted now once it ends.
    reask: bool,
}

#[derive(Debug, Clone)]
struct DigestEntry {
    engine: EngineRef,
    os: Option<ContainerOs>,
    digest: Option<String>,
    fetched: Option<Instant>,
}

#[derive(Debug, Clone)]
struct FactsEntry {
    engine: EngineRef,
    facts: Option<EngineFacts>,
    fetched: Option<Instant>,
}

#[derive(Default)]
struct Caches {
    digests: HashMap<(String, String), DigestEntry>,
    facts: IndexMap<String, FactsEntry>,
    scripts: HashMap<String, ScriptEntry>,
    /// Instance slot → when its run in flight started.
    busy: HashMap<String, Instant>,
}

/// One script run handed to a worker: run it, then hand it back with
/// [`Background::finish_script`].
#[derive(Debug, Clone)]
pub struct ScriptJob {
    key: String,
    slot: String,
    pub run: ScriptRun,
    pub plan: bool,
}

/// Work the refresher does on every pass, after the caches. It must not
/// wait long: a slow task delays the next digest and facts refresh.
pub trait Periodic {
    fn tick(&self);
}

/// Whether status and plan scripts may run now: a source answers no while
/// its definition is not the one the person trusted, and while a job is
/// changing its instance.
pub trait ScriptGate {
    fn scripts_allowed(&self) -> bool;
}

/// The shared caches and the threads that fill them.
pub struct Background {
    caches: Mutex<Caches>,
    periodic: Mutex<Vec<Weak<dyn Periodic + Send + Sync>>>,
    wake: Condvar,
    stop: AtomicBool,
    intervals: Intervals,
    /// Where the turn grids start.
    epoch: Instant,
    seed: u64,
    /// The scripts the started workers have in flight, ended on shutdown.
    running: Mutex<Option<Arc<RunningScripts>>>,
    /// Asked before every script run; none = always allowed.
    gate: Mutex<Option<Weak<dyn ScriptGate + Send + Sync>>>,
}

/// The threads [`Background::start`] started.
pub struct Workers {
    refresher: thread::JoinHandle<()>,
    scripts: Vec<thread::JoinHandle<()>>,
}

impl Workers {
    /// Wait for the refresher, and for each script worker up to
    /// [`SHUTDOWN_GRACE`]; one still inside a script is left behind (the
    /// process is ending, and [`Background::shutdown`] stopped the script).
    pub fn join(self) -> thread::Result<()> {
        self.join_within(SHUTDOWN_GRACE)
    }

    /// [`Workers::join`] with another grace.
    pub fn join_within(self, grace: Duration) -> thread::Result<()> {
        let refreshed = self.refresher.join();
        let deadline = Instant::now() + grace;
        for h in self.scripts {
            while !h.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(20));
            }
            if h.is_finished() {
                h.join()?;
            }
        }
        refreshed
    }

    /// How many script workers are still running.
    pub fn script_workers_alive(&self) -> usize {
        self.scripts.iter().filter(|h| !h.is_finished()).count()
    }
}

/// Where a run's periodic turns sit within its interval: a stable offset in
/// `[0, interval)` from a hash of the seed and the run's key, so the turns
/// of many runs spread across the interval instead of falling together.
pub fn phase(seed: u64, key: &str, interval: Duration) -> Duration {
    let ms = interval.as_millis().max(1) as u64;
    let mut h = Sha256::new();
    h.update(seed.to_le_bytes());
    h.update(key.as_bytes());
    let d = h.finalize();
    let n = u64::from_le_bytes(d[..8].try_into().expect("eight bytes"));
    Duration::from_millis(n % ms)
}

/// The next turn on the grid `phase + k × interval` (times since the
/// epoch) that is at least half an interval after `after`, the end of the
/// last run: the period stays the interval on average, a run's turns stay
/// on its own phase, and a turn missed while it ran is not made up.
pub fn next_turn(phase: Duration, interval: Duration, after: Duration) -> Duration {
    let interval = interval.max(Duration::from_millis(1));
    let earliest = after + interval / 2;
    if earliest <= phase {
        return phase;
    }
    let since = (earliest - phase).as_nanos();
    let step = interval.as_nanos();
    let k = since.div_ceil(step);
    phase + Duration::from_nanos((k * step) as u64)
}

/// The slot a run occupies: its instance, or the run itself when it names
/// none.
fn slot_of(run: &ScriptRun) -> String {
    match run.instance() {
        Some(i) => format!("instance:{i}"),
        None => format!("run:{}", run.key()),
    }
}

impl Background {
    pub fn new() -> Arc<Background> {
        Self::with_intervals(Intervals::default(), 0)
    }

    pub fn with_ttls(digest_ttl: Duration, facts_ttl: Duration) -> Arc<Background> {
        Self::with_intervals(
            Intervals {
                digest: digest_ttl,
                facts: facts_ttl,
                ..Intervals::default()
            },
            0,
        )
    }

    /// Every interval chosen, and the seed the turn phases are hashed with.
    pub fn with_intervals(intervals: Intervals, seed: u64) -> Arc<Background> {
        Arc::new(Background {
            caches: Mutex::new(Caches::default()),
            periodic: Mutex::new(Vec::new()),
            wake: Condvar::new(),
            stop: AtomicBool::new(false),
            intervals,
            epoch: Instant::now(),
            seed,
            running: Mutex::new(None),
            gate: Mutex::new(None),
        })
    }

    /// Ask `gate` before every script run (held weakly).
    pub fn set_gate(&self, gate: Weak<dyn ScriptGate + Send + Sync>) {
        *self.gate.lock().unwrap_or_else(|p| p.into_inner()) = Some(gate);
    }

    fn scripts_allowed(&self) -> bool {
        let gate = self.gate.lock().unwrap_or_else(|p| p.into_inner()).clone();
        match gate {
            None => true,
            Some(w) => w.upgrade().is_none_or(|g| g.scripts_allowed()),
        }
    }

    fn caches(&self) -> MutexGuard<'_, Caches> {
        self.caches.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Where the turn grids start (for a caller that drives the scheduler
    /// with its own clock).
    pub fn epoch(&self) -> Instant {
        self.epoch
    }

    /// Start the refresher and the script workers; they end after
    /// [`Background::shutdown`].
    pub fn start(self: &Arc<Self>, world: World) -> Workers {
        *self.running.lock().unwrap_or_else(|p| p.into_inner()) =
            Some(world.running_scripts.clone());
        let me = self.clone();
        let w = world.clone();
        let refresher = thread::spawn(move || me.run(&w));
        let scripts = (0..SCRIPT_WORKERS)
            .map(|_| {
                let me = self.clone();
                let w = world.clone();
                thread::spawn(move || me.work(&w))
            })
            .collect();
        Workers { refresher, scripts }
    }

    /// Stop the threads, and end every status or plan script still
    /// running (with what it started) rather than leave it to its own time
    /// limit after this process is gone.
    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.wake.notify_all();
        let running = self
            .running
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        if let Some(r) = running {
            let killed = r.kill_all();
            if !killed.is_empty() {
                eprintln!(
                    "chappa-stack: stopped {} script(s) still running at shutdown",
                    killed.len()
                );
            }
        }
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    fn run(&self, world: &World) {
        while !self.stopped() {
            self.refresh_caches(world);
            let guard = self.caches();
            if self.stopped() {
                break;
            }
            let _ = self.wake.wait_timeout(guard, TICK);
        }
    }

    /// One script worker: take the next due run, run it, hand it back.
    fn work(&self, world: &World) {
        while !self.stopped() {
            match self.next_script(Instant::now()) {
                Some(job) => {
                    let answer = Self::answer(world, &job);
                    self.finish_script(job, answer, Instant::now());
                }
                None => {
                    let guard = self.caches();
                    if self.stopped() {
                        break;
                    }
                    let _ = self.wake.wait_timeout(guard, TICK);
                }
            }
        }
    }

    fn answer(world: &World, job: &ScriptJob) -> ScriptAnswer {
        if job.plan {
            ScriptAnswer::Plan(run_plan(&*world.scripts, &job.run))
        } else {
            ScriptAnswer::Status(run_status(&*world.scripts, &job.run))
        }
    }

    fn interval_of(&self, plan: bool) -> Duration {
        if plan {
            self.intervals.plan
        } else {
            self.intervals.status
        }
    }

    /// The script run due at `now` whose instance is free, marked running;
    /// `None` when there is nothing to do. A periodic turn that is due while
    /// its instance is busy is skipped to its next turn here.
    pub fn next_script(&self, now: Instant) -> Option<ScriptJob> {
        let job = self.pick_script(now)?;
        if self.scripts_allowed() {
            return Some(job);
        }
        // Not allowed now: hand it back untouched, to be asked again.
        let mut c = self.caches();
        c.busy.remove(&job.slot);
        if let Some(e) = c.scripts.get_mut(&job.key) {
            e.running = false;
        }
        None
    }

    fn pick_script(&self, now: Instant) -> Option<ScriptJob> {
        let mut c = self.caches();
        c.scripts
            .retain(|_, e| e.running || now.saturating_duration_since(e.asked) < SCRIPT_IDLE);
        let since = |t: Instant| t.saturating_duration_since(self.epoch);
        let mut skips: Vec<(String, Instant)> = Vec::new();
        let mut best: Option<(bool, Instant, String)> = None;
        for (key, e) in &c.scripts {
            if e.running {
                continue;
            }
            let wanted_now = e.due.is_none();
            if e.due.is_some_and(|d| d > now) {
                continue;
            }
            if c.busy.contains_key(&slot_of(&e.run)) {
                if !wanted_now {
                    let interval = self.interval_of(e.plan);
                    let ph = phase(self.seed, key, interval);
                    let next = self.epoch + next_turn(ph, interval, since(now));
                    skips.push((key.clone(), next));
                }
                continue;
            }
            // Wanted-now first (oldest ask first), then the earliest turn.
            let order = (!wanted_now, e.due.unwrap_or(e.asked), key.clone());
            if best.as_ref().is_none_or(|b| order < *b) {
                best = Some(order);
            }
        }
        for (key, next) in skips {
            if let Some(e) = c.scripts.get_mut(&key) {
                e.due = Some(next);
            }
        }
        let (_, _, key) = best?;
        let e = c.scripts.get_mut(&key)?;
        e.running = true;
        let job = ScriptJob {
            slot: slot_of(&e.run),
            key,
            run: e.run.clone(),
            plan: e.plan,
        };
        c.busy.insert(job.slot.clone(), now);
        Some(job)
    }

    /// Store what `job` answered (ending at `now`), free its instance and
    /// set its next turn.
    fn finish_script(&self, job: ScriptJob, answer: ScriptAnswer, now: Instant) {
        let mut c = self.caches();
        c.busy.remove(&job.slot);
        let interval = self.interval_of(job.plan);
        let ph = phase(self.seed, &job.key, interval);
        let next = self.epoch + next_turn(ph, interval, now.saturating_duration_since(self.epoch));
        if let Some(e) = c.scripts.get_mut(&job.key) {
            e.answer = Some(answer);
            e.running = false;
            e.due = if std::mem::take(&mut e.reask) {
                None
            } else {
                Some(next)
            };
        }
        drop(c);
        self.wake.notify_all();
    }

    /// Hand back `job` with the status report or plan preview it produced
    /// (for a caller that runs scripts itself).
    pub fn finish_status(&self, job: ScriptJob, report: ScriptReport, now: Instant) {
        self.finish_script(job, ScriptAnswer::Status(report), now);
    }

    pub fn finish_plan(&self, job: ScriptJob, preview: PlanPreview, now: Instant) {
        self.finish_script(job, ScriptAnswer::Plan(preview), now);
    }

    /// One pass on the calling thread: every script due now (one instance
    /// after another), then digests, facts and periodic work. What a host
    /// without the threads, and a test, calls.
    pub fn refresh_once(&self, world: &World) {
        while let Some(job) = self.next_script(Instant::now()) {
            if self.stopped() {
                return;
            }
            let answer = Self::answer(world, &job);
            self.finish_script(job, answer, Instant::now());
        }
        self.refresh_caches(world);
    }

    /// Digests, facts and periodic work: ask for every entry that is
    /// missing or past its TTL, with no lock held.
    fn refresh_caches(&self, world: &World) {
        let now = Instant::now();
        let (digests, engines) = {
            let c = self.caches();
            let digests: Vec<((String, String), DigestEntry)> = c
                .digests
                .iter()
                .filter(|(_, e)| e.fetched.is_none_or(|t| now - t >= self.intervals.digest))
                .map(|(k, e)| (k.clone(), e.clone()))
                .collect();
            let engines: Vec<FactsEntry> = c
                .facts
                .values()
                .filter(|e| e.fetched.is_none_or(|t| now - t >= self.intervals.facts))
                .cloned()
                .collect();
            (digests, engines)
        };
        for (key, entry) in digests {
            if self.stopped() {
                return;
            }
            let digest =
                resolve_digest(&*world.registry, &entry.engine, &key.1, entry.os).unwrap_or(None);
            if let Some(e) = self.caches().digests.get_mut(&key) {
                e.digest = digest;
                e.fetched = Some(Instant::now());
            }
        }
        for entry in engines {
            if self.stopped() {
                return;
            }
            let facts = engine_facts(&*world.cli, &entry.engine);
            if let Some(e) = self.caches().facts.get_mut(&entry.engine.name) {
                e.facts = Some(facts);
                e.fetched = Some(Instant::now());
            }
        }
        let tasks: Vec<Arc<dyn Periodic + Send + Sync>> = {
            let mut list = self.periodic.lock().unwrap_or_else(|p| p.into_inner());
            list.retain(|w| w.strong_count() > 0);
            list.iter().filter_map(Weak::upgrade).collect()
        };
        for t in tasks {
            if self.stopped() {
                return;
            }
            t.tick();
        }
    }

    /// Run `task` on every pass while it lives (held weakly, so a dropped
    /// source stops being ticked).
    pub fn add_periodic(&self, task: Weak<dyn Periodic + Send + Sync>) {
        self.periodic
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(task);
    }

    /// Queue a channel reference for the refresher (no-op when known).
    pub fn want_digest(&self, engine: &EngineRef, reference: &str, os: Option<ContainerOs>) {
        let mut c = self.caches();
        let key = (engine.name.clone(), reference.to_string());
        if let std::collections::hash_map::Entry::Vacant(slot) = c.digests.entry(key) {
            slot.insert(DigestEntry {
                engine: engine.clone(),
                os,
                digest: None,
                fetched: None,
            });
            drop(c);
            self.wake.notify_all();
        }
    }

    pub fn want_engine(&self, engine: &EngineRef) {
        let mut c = self.caches();
        if !c.facts.contains_key(&engine.name) {
            c.facts.insert(
                engine.name.clone(),
                FactsEntry {
                    engine: engine.clone(),
                    facts: None,
                    fetched: None,
                },
            );
            drop(c);
            self.wake.notify_all();
        }
    }

    /// The facts known for an engine; `None` until first asked.
    pub fn engine_facts(&self, engine: &str) -> Option<EngineFacts> {
        self.caches()
            .facts
            .get(engine)
            .and_then(|e| e.facts.clone())
    }

    /// Forget the facts so the next pass asks again (after a job, which
    /// may have started or stopped an engine's containers). Script
    /// components' status reports and plan previews are asked again too;
    /// the last answer is shown until the new one comes.
    pub fn invalidate_facts(&self) {
        let mut c = self.caches();
        for e in c.facts.values_mut() {
            e.fetched = None;
        }
        for e in c.scripts.values_mut() {
            if e.running {
                e.reask = true;
            } else {
                e.due = None;
            }
        }
        drop(c);
        self.wake.notify_all();
    }

    /// The last answer for `run`, queueing it when it is new.
    fn script_answer(&self, run: &ScriptRun, plan: bool) -> Option<ScriptAnswer> {
        let key = format!("{}:{}", if plan { "plan" } else { "status" }, run.key());
        let mut c = self.caches();
        match c.scripts.get_mut(&key) {
            Some(e) => {
                e.asked = Instant::now();
                e.answer.clone()
            }
            None => {
                c.scripts.insert(
                    key,
                    ScriptEntry {
                        run: run.clone(),
                        plan,
                        answer: None,
                        asked: Instant::now(),
                        due: None,
                        running: false,
                        reask: false,
                    },
                );
                drop(c);
                self.wake.notify_all();
                None
            }
        }
    }

    /// [`ScriptOutputs::slow`] at `now`.
    pub fn slow_at(&self, run: &ScriptRun, now: Instant) -> Option<Duration> {
        let started = *self.caches().busy.get(&slot_of(run))?;
        let took = now.saturating_duration_since(started);
        (took > self.intervals.status).then_some(took)
    }
}

impl ScriptOutputs for Background {
    /// Never waits: the last report, or `None` (and queued) when new.
    fn status(&self, run: &ScriptRun) -> Option<ScriptReport> {
        match self.script_answer(run, false)? {
            ScriptAnswer::Status(r) => Some(r),
            ScriptAnswer::Plan(_) => None,
        }
    }

    fn plan(&self, run: &ScriptRun) -> Option<PlanPreview> {
        match self.script_answer(run, true)? {
            ScriptAnswer::Plan(p) => Some(p),
            ScriptAnswer::Status(_) => None,
        }
    }

    fn slow(&self, run: &ScriptRun) -> Option<Duration> {
        self.slow_at(run, Instant::now())
    }
}

impl PrebuiltDigests for Background {
    /// Never waits: the cached digest, or `None` (and queued) when unknown.
    fn current_digest(
        &self,
        engine: &EngineRef,
        reference: &str,
        os: Option<ContainerOs>,
    ) -> Option<String> {
        let known = self
            .caches()
            .digests
            .get(&(engine.name.clone(), reference.to_string()))
            .map(|e| e.digest.clone());
        match known {
            Some(d) => d,
            None => {
                self.want_digest(engine, reference, os);
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phases_are_stable_per_seed_and_spread_across_the_interval() {
        let ttl = Duration::from_secs(10);
        let keys: Vec<String> = (0..40).map(|i| format!("status:run-{i}")).collect();
        let phases: Vec<Duration> = keys.iter().map(|k| phase(7, k, ttl)).collect();
        assert_eq!(
            phases,
            keys.iter().map(|k| phase(7, k, ttl)).collect::<Vec<_>>(),
            "the same seed gives the same phases"
        );
        assert!(phases.iter().all(|p| *p < ttl));
        // Forty runs land in at least six of the ten one-second buckets,
        // and no second holds more than a third of them.
        let mut buckets = [0usize; 10];
        for p in &phases {
            buckets[p.as_secs() as usize] += 1;
        }
        assert!(
            buckets.iter().filter(|b| **b > 0).count() >= 6,
            "{buckets:?}"
        );
        assert!(buckets.iter().all(|b| *b <= 40 / 3), "{buckets:?}");
        assert_ne!(
            phases,
            keys.iter().map(|k| phase(8, k, ttl)).collect::<Vec<_>>(),
            "another seed moves them"
        );
    }

    #[test]
    fn the_next_turn_stays_on_the_grid_and_never_makes_up_a_missed_one() {
        let s = Duration::from_secs;
        let ms = Duration::from_millis;
        let ttl = s(10);
        // Before the first grid point: the phase itself.
        assert_eq!(next_turn(s(7), ttl, s(0)), s(7));
        // A quick run that ended just after its turn at 17 s: the next is 27 s.
        assert_eq!(next_turn(s(7), ttl, s(17) + ms(20)), s(27));
        // A run that took 25 s from its turn at 17 s: the turns at 27 s and
        // 37 s are gone, the next is 47 s (at least half an interval on).
        assert_eq!(next_turn(s(7), ttl, s(42)), s(47));
        assert_eq!(next_turn(s(7), ttl, s(43)), s(57));
        // Exactly on the boundary counts.
        assert_eq!(next_turn(s(7), ttl, s(22)), s(27));
    }
}
