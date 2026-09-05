//! Per-terminal process stats — cpu / memory / subprocess count.
//!
//! One poller thread walks every live terminal's child tree on a 2s tick and
//! emits `term://stats` for the rail's `N%` readout and the terminal hint
//! bar's `N subprocesses`. Scope is those two readouts ONLY — chappa-ai does
//! not build a full activity-monitor view.
//!
//! Shape mirrors `term://activity`, the low-rate JSON-event
//! precedent: emit-only through the app-wide event name, rate limited, and
//! deliberately NOT pushed into the per-terminal debug event ring (it would
//! evict the title/notify/exited events the parity harness asserts
//! on).
//!
//! **Refresh policy — "never a full system scan per tick".** A tick does two
//! refreshes, and only ONE of them reads per-process counters:
//!  1. the *topology pass*: `ProcessesToUpdate::All` with
//!     `ProcessRefreshKind::nothing()` — the process list, parent links and
//!     start times, which is what "children-rediscovery" needs and is the
//!     cheap part of a `sysinfo` refresh (no cpu, memory, disk, user, exe,
//!     cmd, environ or cwd for anything);
//!  2. the *detail pass*: `ProcessesToUpdate::Some(&subtree_pids)` with
//!     cpu + memory — the expensive per-process counters, read for the few
//!     dozen pids under our own terminals and nothing else.
//!
//! **PID-reuse guard** (a 0.9.3 lesson — tree ops verify identity
//! against a fresh snapshot): a candidate child whose start time predates its
//! parent's is a recycled pid wearing a dead process's number. It and its
//! subtree are excluded. See [`walk_tree`].

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::json;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use tauri::{AppHandle, Emitter};
use term_core::status::ActivityLimiter;

use crate::registry::{JsonEvent, Registry, TermId};

/// Poll period. Also the rate limiter's window: one emit per terminal per
/// tick, at most.
pub const TICK: Duration = Duration::from_secs(2);

/// Material-change thresholds: >0.5% cpu, >1MB rss, any count
/// delta. Anything smaller is noise the rail must not repaint for.
const CPU_EPSILON: f32 = 0.5;
const MEM_EPSILON: u64 = 1024 * 1024;

/// One process row of a snapshot table — the pure input to the tree walk and
/// the aggregation, so both are testable against a hand-written table with no
/// `sysinfo` and no real processes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProcInfo {
    pub pid: u32,
    pub parent: Option<u32>,
    /// Seconds since the epoch, as `sysinfo` reports it. Only ORDER matters
    /// here (the PID-reuse guard compares child against parent).
    pub start_time: u64,
    /// `sysinfo`'s per-process cpu percentage: 100 = one core saturated, so
    /// this can exceed 100 on a multi-core box. [`normalize_cpu`] divides.
    pub cpu_pct: f32,
    pub mem_bytes: u64,
}

/// What one terminal's tree currently costs. The `term://stats` payload.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Sample {
    /// 0–100 across ALL cores (no "300%" readouts).
    pub cpu_pct: f32,
    pub mem_bytes: u64,
    /// Live processes under the shell, EXCLUDING the shell itself.
    pub subproc_count: usize,
}

impl Sample {
    pub const ZERO: Sample = Sample {
        cpu_pct: 0.0,
        mem_bytes: 0,
        subproc_count: 0,
    };

    fn is_zero(&self) -> bool {
        self.subproc_count == 0 && self.mem_bytes == 0 && self.cpu_pct == 0.0
    }
}

/// One terminal the poller should sample, as the registry sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatsTarget {
    pub id: TermId,
    /// The pty child pid — the tree root. `None` for a no-pty actor.
    pub pid: Option<u32>,
    /// Still `starting`/`running`. A terminal that left those states owes one
    /// final zero and then stops being sampled.
    pub alive: bool,
}

/// Every process in `table` reachable downward from `root`, root first.
///
/// Children are derived from the parent links of the SAME snapshot, so a
/// process that respawned between snapshots cannot smuggle itself in. The
/// PID-reuse guard drops any candidate that started before its parent did —
/// on a busy box a recycled pid is otherwise indistinguishable from a real
/// child, and it would show up as somebody else's cpu on our rail row.
/// A `seen` set doubles as the cycle guard (a self-parenting or looped pid
/// table must not hang the poller).
pub fn walk_tree(table: &HashMap<u32, ProcInfo>, root: u32) -> Vec<u32> {
    let Some(root_info) = table.get(&root) else {
        return Vec::new();
    };
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for info in table.values() {
        if let Some(parent) = info.parent {
            children.entry(parent).or_default().push(info.pid);
        }
    }
    let mut out = vec![root];
    let mut seen: HashSet<u32> = HashSet::from([root]);
    let mut queue: VecDeque<ProcInfo> = VecDeque::from([*root_info]);
    while let Some(parent) = queue.pop_front() {
        let Some(kids) = children.get(&parent.pid) else {
            continue;
        };
        let mut kids = kids.clone();
        // Deterministic order, so the walk's output is assertable.
        kids.sort_unstable();
        for pid in kids {
            let Some(info) = table.get(&pid) else {
                continue;
            };
            // PID reuse: a real child cannot predate its own parent. Only
            // EXCLUDE on proof — `sysinfo` reports start_time 0 for a process
            // it could not open (about 40% of a Windows process table is
            // unreadable at normal privilege), and dropping a genuine child on
            // a missing timestamp would silently under-report our own cpu.
            if info.start_time != 0 && parent.start_time != 0 && info.start_time < parent.start_time
            {
                continue;
            }
            if !seen.insert(pid) {
                continue;
            }
            out.push(pid);
            queue.push_back(*info);
        }
    }
    out
}

/// Sum a tree into one [`Sample`]. Pids missing from `table` died between the
/// topology pass and the detail pass and simply contribute nothing — the
/// subprocess count is the LIVE count, which is what the tooltip claims.
pub fn aggregate(table: &HashMap<u32, ProcInfo>, root: u32, pids: &[u32], cores: usize) -> Sample {
    let mut cpu = 0.0f32;
    let mut mem = 0u64;
    let mut subprocs = 0usize;
    for pid in pids {
        let Some(info) = table.get(pid) else { continue };
        cpu += info.cpu_pct;
        mem = mem.saturating_add(info.mem_bytes);
        if *pid != root {
            subprocs += 1;
        }
    }
    Sample {
        cpu_pct: normalize_cpu(cpu, cores),
        mem_bytes: mem,
        subproc_count: subprocs,
    }
}

/// `sysinfo` reports 100 per saturated core; the rail shows one 0–100 number
/// for the whole machine. Clamped at both ends: a sampling artefact must not
/// paint "104%".
pub fn normalize_cpu(total_pct: f32, cores: usize) -> f32 {
    (total_pct / cores.max(1) as f32).clamp(0.0, 100.0)
}

/// The emit gate: is `next` different enough from the last emitted sample to
/// be worth a repaint? Thresholds are the constants above.
pub fn material_change(prev: &Sample, next: &Sample) -> bool {
    prev.subproc_count != next.subproc_count
        || (next.cpu_pct - prev.cpu_pct).abs() > CPU_EPSILON
        || prev.mem_bytes.abs_diff(next.mem_bytes) > MEM_EPSILON
}

/// Per-terminal emit bookkeeping: the last emitted sample, the one-per-tick
/// limiter, and the event seq. Pure — the poller thread owns one of these and
/// the tests drive it with fake time.
#[derive(Default)]
pub struct StatsTracker {
    terms: HashMap<TermId, TermState>,
}

struct TermState {
    /// Last sample actually EMITTED (not merely observed) — the gate compares
    /// against what the frontend believes, never against a suppressed tick.
    last: Sample,
    limiter: ActivityLimiter,
    /// A non-zero sample is on screen, so this terminal owes a final zero.
    owes_zero: bool,
    /// Per-terminal monotonic event seq. `term://stats` is emitted OUTSIDE the
    /// pump, so it cannot share the pump's frame/event counter; this is its
    /// own stream position and exists only so the envelope stays the shape
    /// every other `term://*` event has.
    seq: u64,
}

impl Default for TermState {
    fn default() -> Self {
        Self {
            last: Sample::ZERO,
            limiter: ActivityLimiter::new(TICK),
            owes_zero: false,
            seq: 0,
        }
    }
}

impl StatsTracker {
    /// Feed one tick's sample for `id`. `Some(sample, seq)` means emit;
    /// `None` means the change was immaterial, or a sample already went out
    /// inside this tick window.
    ///
    /// An unseen terminal starts from [`Sample::ZERO`], so an idle shell —
    /// which is what most terminals are, most of the time — never emits at
    /// all.
    pub fn observe(&mut self, id: TermId, sample: Sample, now_ms: u64) -> Option<(Sample, u64)> {
        let state = self.terms.entry(id).or_default();
        if !material_change(&state.last, &sample) {
            return None;
        }
        // Per-tick rate limit. Deliberately BEFORE `last` is updated: a
        // suppressed change stays pending and lands on the next tick rather
        // than being silently swallowed.
        if !state.limiter.due(now_ms) {
            return None;
        }
        state.last = sample;
        state.owes_zero = !sample.is_zero();
        state.seq += 1;
        Some((sample, state.seq))
    }

    /// The terminal exited: one final zero (so the rail's `N%` and the hint
    /// bar clear), then it stops being tracked. Bypasses the rate limiter —
    /// there is no next tick to defer to. `None` when nothing non-zero was
    /// ever shown for it.
    pub fn finish(&mut self, id: TermId) -> Option<(Sample, u64)> {
        let mut state = self.terms.remove(&id)?;
        if !state.owes_zero {
            return None;
        }
        state.seq += 1;
        Some((Sample::ZERO, state.seq))
    }

    /// Forget a terminal without emitting (its row is gone).
    pub fn forget(&mut self, id: TermId) {
        self.terms.remove(&id);
    }

    /// Terminals currently tracked (the poller drops states for rows the
    /// registry no longer has).
    pub fn tracked(&self) -> Vec<TermId> {
        self.terms.keys().copied().collect()
    }
}

/// Park/wake pair so the poller costs literally nothing while the registry is
/// empty — no wakeups, no `sysinfo` refresh, no timer. The registry signals it
/// when a terminal is created.
#[derive(Default)]
pub struct StatsWaker {
    pending: Mutex<bool>,
    cv: Condvar,
}

impl StatsWaker {
    /// A terminal appeared: release a parked poller (or arm the next park, if
    /// the poller has not reached it yet — the flag makes the handoff
    /// lossless).
    pub fn wake(&self) {
        if let Ok(mut pending) = self.pending.lock() {
            *pending = true;
            self.cv.notify_all();
        }
    }

    /// Block until [`StatsWaker::wake`] is called. Returns immediately when a
    /// wake is already pending.
    pub fn park(&self) {
        let Ok(mut pending) = self.pending.lock() else {
            return;
        };
        while !*pending {
            pending = match self.cv.wait(pending) {
                Ok(guard) => guard,
                Err(_) => return,
            };
        }
        *pending = false;
    }
}

/// Start the poller. One thread for the whole app; it parks whenever the
/// registry is empty and is woken by the next `create_terminal`.
pub fn spawn_poller(registry: Registry, app: AppHandle) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let waker = registry.stats_waker();
        let mut sys = System::new();
        let mut tracker = StatsTracker::default();
        // Logical cores — the denominator `sysinfo`'s per-process percentages
        // are measured against. Read once: it does not change at runtime, and
        // `sysinfo`'s own cpu list would need a cpu refresh to populate.
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        let started = Instant::now();
        loop {
            let targets = registry.stats_targets();
            if targets.is_empty() {
                // Nothing to sample: drop the bookkeeping and sleep for real.
                for id in tracker.tracked() {
                    tracker.forget(id);
                }
                waker.park();
                continue;
            }
            let now_ms = started.elapsed().as_millis() as u64;
            for (id, sample, seq) in poll_once(&mut sys, &mut tracker, &targets, cores, now_ms) {
                emit(&app, id, sample, seq);
            }
            std::thread::sleep(TICK);
        }
    })
}

/// One tick: refresh, walk, aggregate, gate. Returns the emits owed.
fn poll_once(
    sys: &mut System,
    tracker: &mut StatsTracker,
    targets: &[StatsTarget],
    cores: usize,
    now_ms: u64,
) -> Vec<(TermId, Sample, u64)> {
    let mut out = Vec::new();

    // Terminals the registry dropped: forget them silently (no row to clear).
    let live: HashSet<TermId> = targets.iter().map(|t| t.id).collect();
    for id in tracker.tracked() {
        if !live.contains(&id) {
            tracker.forget(id);
        }
    }

    // Pass 1 — topology only (see the module doc: this is NOT a full scan;
    // `nothing()` reads no per-process counters at all).
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());
    let topology = snapshot(sys);

    let mut trees: Vec<(TermId, u32, Vec<u32>)> = Vec::new();
    let mut wanted: Vec<Pid> = Vec::new();
    for target in targets {
        if !target.alive {
            if let Some((sample, seq)) = tracker.finish(target.id) {
                out.push((target.id, sample, seq));
            }
            continue;
        }
        let Some(root) = target.pid else { continue };
        let tree = walk_tree(&topology, root);
        wanted.extend(tree.iter().map(|pid| Pid::from_u32(*pid)));
        trees.push((target.id, root, tree));
    }
    if wanted.is_empty() {
        return out;
    }

    // Pass 2 — the only counter read of the tick, over our pids alone.
    wanted.sort_unstable();
    wanted.dedup();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&wanted),
        true,
        ProcessRefreshKind::nothing().with_cpu().with_memory(),
    );
    // Only our own pids: the topology table's other few hundred rows carry no
    // counters this tick and nothing reads them again.
    let detail = snapshot_of(sys, &wanted);

    for (id, root, tree) in trees {
        let sample = aggregate(&detail, root, &tree, cores);
        if let Some((sample, seq)) = tracker.observe(id, sample, now_ms) {
            out.push((id, sample, seq));
        }
    }
    out
}

/// `sysinfo`'s whole process map as the pure table the tree walk reads.
fn snapshot(sys: &System) -> HashMap<u32, ProcInfo> {
    sys.processes()
        .iter()
        .map(|(pid, proc_)| (pid.as_u32(), row(*pid, proc_)))
        .collect()
}

/// The same table for a named subset — the aggregation only ever looks at our
/// own subtree, and a dead pid simply drops out.
fn snapshot_of(sys: &System, pids: &[Pid]) -> HashMap<u32, ProcInfo> {
    pids.iter()
        .filter_map(|pid| {
            sys.process(*pid)
                .map(|proc_| (pid.as_u32(), row(*pid, proc_)))
        })
        .collect()
}

fn row(pid: Pid, proc_: &sysinfo::Process) -> ProcInfo {
    ProcInfo {
        pid: pid.as_u32(),
        parent: proc_.parent().map(|p| p.as_u32()),
        start_time: proc_.start_time(),
        cpu_pct: proc_.cpu_usage(),
        mem_bytes: proc_.memory(),
    }
}

/// Emit-only, exactly like `term://activity`: the app-wide event name, never
/// the per-terminal debug ring.
///
/// The payload carries the same `term_id` key every other `term://*` event
/// does, so the frontend decodes it with the one `TerminalEventDto` shape
/// instead of a special case.
fn emit(app: &AppHandle, term_id: TermId, sample: Sample, seq: u64) {
    let event = JsonEvent {
        event: "term://stats",
        term_id,
        seq,
        data: [
            ("cpu_pct".to_owned(), json!(round1(sample.cpu_pct))),
            ("mem_bytes".to_owned(), json!(sample.mem_bytes)),
            ("subproc_count".to_owned(), json!(sample.subproc_count)),
        ]
        .into_iter()
        .collect(),
    };
    let _ = app.emit(event.event, &event);
}

/// One decimal on the wire: the rail rounds to a whole percent and the
/// tooltip shows one decimal, so more precision is only float noise in the
/// event log.
fn round1(v: f32) -> f32 {
    (v * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn proc_(
        pid: u32,
        parent: Option<u32>,
        start_time: u64,
        cpu_pct: f32,
        mem_mb: u64,
    ) -> ProcInfo {
        ProcInfo {
            pid,
            parent,
            start_time,
            cpu_pct,
            mem_bytes: mem_mb * 1024 * 1024,
        }
    }

    fn table(rows: &[ProcInfo]) -> HashMap<u32, ProcInfo> {
        rows.iter().map(|p| (p.pid, *p)).collect()
    }

    #[test]
    fn tree_walk_reaches_depth_three_and_excludes_a_recycled_pid() {
        // 100 pwsh ─ 200 cargo ─ 300 rustc ─ 400 ld   (depth 3)
        //          └ 201 git
        // 900 is an unrelated process; 999 CLAIMS 300 as its parent but
        // started before the whole tree existed — a recycled pid.
        let t = table(&[
            proc_(1, None, 5, 0.0, 0),
            proc_(100, Some(1), 10, 1.0, 10),
            proc_(200, Some(100), 20, 2.0, 20),
            proc_(201, Some(100), 21, 3.0, 30),
            proc_(300, Some(200), 30, 4.0, 40),
            proc_(400, Some(300), 40, 5.0, 50),
            proc_(900, Some(1), 11, 99.0, 900),
            proc_(999, Some(300), 7, 88.0, 800),
        ]);

        let tree = walk_tree(&t, 100);
        assert_eq!(tree, vec![100, 200, 201, 300, 400]);
        assert!(!tree.contains(&999), "recycled pid must be excluded");
        assert!(!tree.contains(&900), "unrelated process must be excluded");

        // The impostor's children go with it — exclusion is subtree-wide.
        let mut t2 = t.clone();
        t2.insert(1000, proc_(1000, Some(999), 8, 77.0, 700));
        assert_eq!(walk_tree(&t2, 100), vec![100, 200, 201, 300, 400]);

        // An unknown root yields nothing rather than a bogus empty tree root.
        assert!(walk_tree(&t, 12345).is_empty());
    }

    #[test]
    fn an_unreadable_start_time_keeps_a_genuine_child() {
        // sysinfo reports start_time 0 for a process it could not open. The
        // guard needs PROOF of reuse — a missing timestamp is not proof, and
        // excluding on it would under-report our own tree's cpu.
        let t = table(&[
            proc_(100, Some(1), 10, 1.0, 10),
            proc_(200, Some(100), 0, 5.0, 20),
            proc_(300, Some(200), 30, 5.0, 20),
        ]);
        assert_eq!(walk_tree(&t, 100), vec![100, 200, 300]);

        // An unreadable PARENT is the same story.
        let t = table(&[
            proc_(100, Some(1), 0, 1.0, 10),
            proc_(200, Some(100), 5, 5.0, 20),
        ]);
        assert_eq!(walk_tree(&t, 100), vec![100, 200]);
    }

    #[test]
    fn tree_walk_survives_a_parent_cycle() {
        // A pid table that loops must terminate, not hang the poller.
        let t = table(&[
            proc_(10, Some(11), 1, 1.0, 1),
            proc_(11, Some(10), 1, 1.0, 1),
        ]);
        assert_eq!(walk_tree(&t, 10), vec![10, 11]);
    }

    #[test]
    fn aggregate_sums_the_tree_and_counts_subprocesses_only() {
        let t = table(&[
            proc_(100, Some(1), 10, 10.0, 10),
            proc_(200, Some(100), 20, 30.0, 20),
            proc_(300, Some(200), 30, 40.0, 30),
        ]);
        let tree = walk_tree(&t, 100);
        // 80% total over 4 cores = 20% of the machine.
        let s = aggregate(&t, 100, &tree, 4);
        assert_eq!(s.cpu_pct, 20.0);
        assert_eq!(s.mem_bytes, 60 * 1024 * 1024);
        assert_eq!(s.subproc_count, 2, "the shell itself is not a subprocess");

        // A child that died between the two passes contributes nothing and is
        // not counted as live.
        let mut gone = t.clone();
        gone.remove(&300);
        let s = aggregate(&gone, 100, &tree, 4);
        assert_eq!(s.cpu_pct, 10.0);
        assert_eq!(s.subproc_count, 1);
    }

    #[test]
    fn cpu_is_normalized_across_cores_and_clamped() {
        // No "300%" readouts.
        assert_eq!(normalize_cpu(300.0, 4), 75.0);
        assert_eq!(normalize_cpu(400.0, 4), 100.0);
        assert_eq!(normalize_cpu(400.0, 8), 50.0);
        // One saturated core on a 16-thread box is 6.25%, not 100%.
        assert_eq!(normalize_cpu(100.0, 16), 6.25);
        // Sampling artefacts clamp instead of painting >100.
        assert_eq!(normalize_cpu(900.0, 4), 100.0);
        assert_eq!(normalize_cpu(-3.0, 4), 0.0);
        // A bogus core count must not divide by zero.
        assert_eq!(normalize_cpu(50.0, 0), 50.0);
    }

    #[test]
    fn change_threshold_gates_immaterial_movement() {
        let base = Sample {
            cpu_pct: 10.0,
            mem_bytes: 100 * 1024 * 1024,
            subproc_count: 2,
        };
        assert!(!material_change(&base, &base));

        // cpu: >0.5 points.
        let mut small = base;
        small.cpu_pct = 10.5;
        assert!(!material_change(&base, &small));
        small.cpu_pct = 10.6;
        assert!(material_change(&base, &small));
        small.cpu_pct = 9.4;
        assert!(material_change(&base, &small), "drops count too");

        // memory: >1MB, in both directions.
        let mut mem = base;
        mem.mem_bytes = base.mem_bytes + MEM_EPSILON;
        assert!(!material_change(&base, &mem));
        mem.mem_bytes = base.mem_bytes + MEM_EPSILON + 1;
        assert!(material_change(&base, &mem));
        mem.mem_bytes = base.mem_bytes - MEM_EPSILON - 1;
        assert!(material_change(&base, &mem));

        // subprocess count: ANY delta is material.
        let mut count = base;
        count.subproc_count = 3;
        assert!(material_change(&base, &count));
    }

    #[test]
    fn tracker_emits_at_most_once_per_tick_and_never_for_an_idle_shell() {
        let mut tracker = StatsTracker::default();
        let busy = Sample {
            cpu_pct: 12.0,
            mem_bytes: 50 * 1024 * 1024,
            subproc_count: 1,
        };

        // An idle shell (all zeros) never emits: an unseen terminal starts
        // from ZERO, so there is nothing material to report.
        assert!(tracker.observe(1, Sample::ZERO, 0).is_none());

        let (sample, seq) = tracker.observe(1, busy, 0).expect("first change emits");
        assert_eq!(sample, busy);
        assert_eq!(seq, 1);

        // Same tick window: a second material change is held back…
        let busier = Sample {
            cpu_pct: 40.0,
            ..busy
        };
        assert!(tracker.observe(1, busier, 1_500).is_none());
        // …and lands on the next tick, undiminished (the suppressed sample was
        // never adopted as `last`).
        let (sample, seq) = tracker.observe(1, busier, 2_000).expect("next tick emits");
        assert_eq!(sample.cpu_pct, 40.0);
        assert_eq!(seq, 2);

        // Immaterial drift is free, even when the tick allows an emit.
        let drift = Sample {
            cpu_pct: 40.3,
            ..busier
        };
        assert!(tracker.observe(1, drift, 4_000).is_none());

        // Terminals are independent: id 2's first change is its own.
        assert!(tracker.observe(2, busy, 2_000).is_some());
    }

    #[test]
    fn exit_emits_exactly_one_final_zero() {
        let mut tracker = StatsTracker::default();
        let busy = Sample {
            cpu_pct: 12.0,
            mem_bytes: 50 * 1024 * 1024,
            subproc_count: 1,
        };
        tracker.observe(1, busy, 0).expect("emitted");

        let (final_, seq) = tracker.finish(1).expect("a non-zero reading owes a zero");
        assert_eq!(final_, Sample::ZERO);
        assert_eq!(seq, 2);
        // Then it stops: no second zero, ever.
        assert!(tracker.finish(1).is_none());

        // A terminal that never emitted owes nothing on exit.
        assert!(tracker.observe(2, Sample::ZERO, 0).is_none());
        assert!(tracker.finish(2).is_none());
    }

    #[test]
    fn waker_hands_off_losslessly() {
        let waker = Arc::new(StatsWaker::default());
        // A wake that arrives BEFORE the park is not lost — park returns at
        // once (otherwise a terminal created in that window would leave the
        // poller asleep forever).
        waker.wake();
        waker.park();

        // And a wake from another thread releases a parked poller.
        let other = waker.clone();
        let handle = std::thread::spawn(move || other.park());
        // The flag makes this race-free whichever thread wins.
        waker.wake();
        handle.join().expect("parked thread woke");
    }
}
