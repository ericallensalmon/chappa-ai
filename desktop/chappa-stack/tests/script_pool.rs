//! The script pool: status and plan scripts run on a fixed set of worker
//! threads, apart from the digest and fact refresher. A fast script is not
//! held up by slow ones of another stack, one stack instance has one run
//! in flight at most, no more runs than workers ever run at once, a turn
//! that comes while its instance is busy is skipped rather than queued, a
//! run past the status interval shows as "status is slow" on its rows
//! while they keep their last state, engine facts arrive while a script
//! hangs, and shutdown does not wait on a hung script.

mod common;

use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chappa_stack::background::{next_turn, phase, Background, Intervals, SCRIPT_WORKERS};
use chappa_stack::identity::Token;
use chappa_stack::project::ProjectConfig;
use chappa_stack::server::Source;
use common::*;
use indexmap::IndexMap;
use stack_core::cli::{CliError, CliOutput, EngineCli, FakeCli, Invocation};
use stack_core::definition::EngineRef;
use stack_core::driver::EngineKind;
use stack_core::probe::FakeHttp;
use stack_core::script::{ScriptOutputs, ScriptRun, ScriptVerb};

const UP: &str = r#"{"components":[{"state":"up"}]}"#;

/// A status script per instance and component, as the pool sees it.
fn run(instance: &str, component: &str, script: &str) -> ScriptRun {
    let mut env = IndexMap::new();
    env.insert("CHAPPA_STACK_INSTANCE".to_string(), instance.to_string());
    ScriptRun {
        component: component.to_string(),
        verb: ScriptVerb::Status,
        argv: vec!["node".into(), script.into(), component.into()],
        cwd: PathBuf::from("/srv/example"),
        env,
        timeout_secs: 20,
    }
}

/// Scripts that take as long as their name says (`slow.js` / `hang.js` /
/// anything else = at once), counting how many run at once, overall and
/// per instance. Engine calls answer at once.
#[derive(Default)]
struct Timed {
    slow: Duration,
    hang: Duration,
    state: Mutex<Counts>,
}

#[derive(Default)]
struct Counts {
    now: usize,
    max: usize,
    per_instance: HashMap<String, usize>,
    max_per_instance: usize,
    started: Vec<String>,
}

impl Timed {
    fn new(slow: Duration, hang: Duration) -> Arc<Timed> {
        Arc::new(Timed {
            slow,
            hang,
            ..Timed::default()
        })
    }

    fn counts<T>(&self, f: impl FnOnce(&Counts) -> T) -> T {
        f(&self.state.lock().unwrap())
    }
}

impl EngineCli for Timed {
    fn run(&self, inv: &Invocation) -> Result<CliOutput, CliError> {
        if inv.program != "node" {
            return Ok(CliOutput::success(""));
        }
        let instance = inv
            .env
            .get("CHAPPA_STACK_INSTANCE")
            .cloned()
            .unwrap_or_default();
        let wait = match inv.args.first().map(String::as_str) {
            Some("slow.js") => self.slow,
            Some("hang.js") => self.hang,
            _ => Duration::ZERO,
        };
        {
            let mut c = self.state.lock().unwrap();
            c.now += 1;
            c.max = c.max.max(c.now);
            let n = c.per_instance.entry(instance.clone()).or_default();
            *n += 1;
            let n = *n;
            c.max_per_instance = c.max_per_instance.max(n);
            c.started.push(inv.args.last().cloned().unwrap_or_default());
        }
        std::thread::sleep(wait);
        let mut c = self.state.lock().unwrap();
        c.now -= 1;
        *c.per_instance.get_mut(&instance).unwrap() -= 1;
        Ok(CliOutput::success(UP))
    }
}

fn world_with(cli: Arc<Timed>) -> chappa_stack::world::World {
    let mut f = fakes(FakeCli::new(), FakeHttp::default());
    f.world.cli = cli;
    f.world
}

#[test]
fn a_fast_script_does_not_wait_behind_slow_ones_and_an_instance_runs_one_at_a_time() {
    let cli = Timed::new(Duration::from_millis(1500), Duration::ZERO);
    let world = world_with(cli.clone());
    let bg = Background::new();
    // Four slow status scripts of one stack, then a fast one of another.
    let slow: Vec<ScriptRun> = ["a", "b", "c", "d"]
        .iter()
        .map(|c| run("app-0", c, "slow.js"))
        .collect();
    let fast = run("web-0", "w", "fast.js");
    for r in &slow {
        assert!(bg.status(r).is_none());
    }
    assert!(bg.status(&fast).is_none());
    let t = Instant::now();
    let workers = bg.start(world);
    assert!(eventually(|| bg.status(&fast).is_some()));
    assert!(
        t.elapsed() < Duration::from_millis(1000),
        "the fast script waited {:?}",
        t.elapsed()
    );
    // The slow ones finish one after another, never two of app-0 at once.
    assert!(eventually(|| slow
        .iter()
        .filter(|r| bg.status(r).is_some())
        .count()
        >= 2));
    assert_eq!(cli.counts(|c| c.max_per_instance), 1);
    bg.shutdown();
    workers.join().unwrap();
}

#[test]
fn no_more_scripts_run_at_once_than_there_are_workers() {
    let cli = Timed::new(Duration::from_millis(600), Duration::ZERO);
    let world = world_with(cli.clone());
    let bg = Background::new();
    let runs: Vec<ScriptRun> = (0..SCRIPT_WORKERS + 2)
        .map(|i| run(&format!("app-{i}"), "app", "slow.js"))
        .collect();
    for r in &runs {
        bg.status(r);
    }
    let workers = bg.start(world);
    assert!(eventually(|| runs.iter().all(|r| bg.status(r).is_some())));
    assert_eq!(cli.counts(|c| c.max), SCRIPT_WORKERS);
    bg.shutdown();
    workers.join().unwrap();
}

/// The scheduler driven with its own clock: a periodic turn that falls
/// while its instance is busy is skipped to its next turn, and when the
/// busy run ends it is not made up; a first answer, by contrast, waits for
/// the slot.
#[test]
fn a_turn_that_comes_while_its_instance_is_busy_is_skipped_not_queued() {
    let bg = Background::new();
    let t0 = bg.epoch();
    let at = |d: Duration| t0 + d;
    let s = Duration::from_secs;
    let a = run("app-0", "a", "status.js");
    let b = run("app-0", "b", "status.js");
    bg.status(&a);
    bg.status(&b);
    // Both wanted now: one runs, the other waits for the slot.
    let first = bg.next_script(at(s(1))).unwrap();
    assert!(bg.next_script(at(s(1))).is_none(), "one run per instance");
    let report =
        || stack_core::script::run_status(&FakeCli::new().on(&["node", "status.js"], UP), &a);
    let other = if first.run == a { b.clone() } else { a.clone() };
    bg.finish_status(first, report(), at(s(1)));
    let second = bg
        .next_script(at(s(1)))
        .expect("the waiting first answer runs next");
    assert_eq!(second.run, other);
    bg.finish_status(second, report(), at(s(1)));
    // Their next turns are on their own grids.
    let ttl = Intervals::default().status;
    let due = |r: &ScriptRun| {
        let key = format!("status:{}", r.key());
        next_turn(phase(0, &key, ttl), ttl, s(1))
    };
    let (da, db) = (due(&a), due(&b));
    assert!(
        da >= s(6) && db >= s(6) && da < s(16) && db < s(16),
        "{da:?} {db:?}"
    );
    // Nothing is due before then.
    assert!(bg.next_script(at(s(5))).is_none());
    // At the later of the two turns both are due: one runs, the other's
    // turn is skipped while its instance is busy.
    let later = da.max(db);
    let running = bg.next_script(at(later)).unwrap();
    let skipped = if running.run == a {
        b.clone()
    } else {
        a.clone()
    };
    assert!(bg.next_script(at(later)).is_none());
    // The busy run ends at once; the skipped turn is not made up.
    bg.finish_status(running, report(), at(later));
    assert!(
        bg.next_script(at(later)).is_none(),
        "a skipped periodic turn is not queued behind the run that blocked it"
    );
    // It runs at its next turn, once.
    let key = format!("status:{}", skipped.key());
    let next = next_turn(phase(0, &key, ttl), ttl, later);
    // (The other run's own turn may come first; the skipped one does not.)
    let before = at(next) - Duration::from_millis(1);
    if let Some(j) = bg.next_script(before) {
        assert_ne!(j.run, skipped, "the skipped run came back before its turn");
        bg.finish_status(j, report(), before);
    }
    let job = bg.next_script(at(next)).expect("its next turn");
    assert_eq!(job.run, skipped);
    bg.finish_status(job, report(), at(next));
    assert!(bg.next_script(at(next)).is_none());
}

const ONE: &str = "stacks:\n  app:\n    components:\n      app:\n        driver: script\n        up: [node, up.js]\n        status: [node, status.js]\n";

fn badge_texts(row: &Value) -> Vec<String> {
    row["badges"]
        .as_array()
        .map(|b| {
            b.iter()
                .map(|x| x["text"].as_str().unwrap().to_string())
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn a_run_longer_than_the_status_interval_marks_the_rows_slow_and_keeps_their_state() {
    let p = Project::with(ONE, &[]);
    let f = fakes(
        FakeCli::new().on(&["node", "status.js"], UP),
        FakeHttp::default(),
    );
    let bg = Background::with_intervals(
        Intervals {
            status: Duration::from_millis(200),
            ..Intervals::default()
        },
        0,
    );
    let src = Source::with_background(
        f.world.clone(),
        ProjectConfig::new(&p.dir),
        Token::parse(TOKEN).unwrap(),
        "127.0.0.1:5000".into(),
        bg.clone(),
    );
    let env = || get(&src, "/api/environment").1;
    env();
    bg.refresh_once(&f.world);
    let e = env();
    assert_eq!(e["components"][0]["state"], "up");
    assert!(badge_texts(&e["components"][0]).is_empty());
    // Asked again, and the run takes its time (held here, past the interval).
    bg.invalidate_facts();
    let job = bg.next_script(Instant::now()).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let e = env();
    let row = &e["components"][0];
    assert_eq!(row["state"], "up", "the last known state stays");
    assert_eq!(badge_texts(row), ["status is slow"]);
    let alerts: Vec<&str> = e["alerts"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["text"].as_str())
        .collect();
    assert!(
        alerts.iter().any(|a| a.starts_with("app: status is slow")),
        "{alerts:?}"
    );
    // Once it ends the note goes.
    let report =
        stack_core::script::run_status(&FakeCli::new().on(&["node", "status.js"], UP), &job.run);
    bg.finish_status(job, report, Instant::now());
    let e = env();
    assert!(badge_texts(&e["components"][0]).is_empty());
    assert!(!e["alerts"]
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a["text"].as_str().unwrap_or("").contains("status is slow")));
}

#[test]
fn engine_facts_arrive_while_a_script_hangs_and_shutdown_does_not_wait_for_it() {
    let cli = Timed::new(Duration::ZERO, Duration::from_secs(4));
    let world = world_with(cli.clone());
    let bg = Background::new();
    let hung = run("app-0", "app", "hang.js");
    bg.status(&hung);
    let workers = bg.start(world);
    assert!(
        eventually(|| cli.counts(|c| c.now) == 1),
        "the script started"
    );
    let docker = EngineRef {
        name: "docker".into(),
        kind: EngineKind::Docker,
        context: None,
    };
    let t = Instant::now();
    bg.want_engine(&docker);
    assert!(eventually(|| bg.engine_facts("docker").is_some()));
    assert!(
        t.elapsed() < Duration::from_millis(1500),
        "{:?}",
        t.elapsed()
    );
    assert!(bg.status(&hung).is_none(), "the script is still running");
    let t = Instant::now();
    bg.shutdown();
    workers.join_within(Duration::from_millis(300)).unwrap();
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
}

#[test]
fn stack_wide_first_turns_spread_across_the_interval() {
    // Twenty stacks refreshed every ten seconds: after their first answers
    // at the same moment, their next turns fall in different seconds.
    let ttl = Intervals::default().status;
    let mut seconds: Vec<u64> = (0..20)
        .map(|i| {
            let key = format!(
                "status:{}",
                run(&format!("app-{i}"), "app", "status.js").key()
            );
            next_turn(phase(0, &key, ttl), ttl, Duration::from_secs(100)).as_secs()
        })
        .collect();
    seconds.sort();
    seconds.dedup();
    assert!(seconds.len() >= 6, "{seconds:?}");
}
