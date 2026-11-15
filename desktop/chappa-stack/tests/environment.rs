//! The Environment payload from fake engines: one component per state, so a
//! single answer holds all nine, and every cell the view draws is filled
//! at least once. The payload is pinned as a snapshot
//! (`tests/snapshots/environment.json`; `UPDATE_SNAPSHOTS=1` rewrites it).

mod common;

use indexmap::IndexMap;
use serde_json::Value;
use std::path::Path;
use std::time::{Duration, Instant};

use chappa_stack::background::Background;
use chappa_stack::identity::Token;
use chappa_stack::procs::{ProcessEntry, ProcessTable};
use chappa_stack::project::ProjectConfig;
use chappa_stack::server::Source;
use common::*;
use stack_core::cli::{CliError, CliOutput, EngineCli, FakeCli, Invocation};
use stack_core::definition::Mode;
use stack_core::images::ProvenanceCache;
use stack_core::intent::{ComponentIntent, Desired, IntentRecord};
use stack_core::probe::FakeHttp;
use stack_core::provenance::Provenance;

const NEW_DIGEST: &str = "sha256:2222222222222222222222222222222222222222222222222222222222222222";

const YML: &str = r#"name: nine
processes:
  web-dev:
    command: [npm, run, dev]
stacks:
  env:
    slots: { count: 2, portBase: 15000, portStride: 50 }
    hostname: dev.example.test
    groups:
      app: { engine: docker, label: Application }
      data: { engine: docker, label: Data }
      jobs: { engine: docker, label: Jobs }
    components:
      api:
        group: app
        driver: compose
        file: stack/app.yml
        service: api
        modes: { prebuilt: { image: registry.example/api, tag: "{channel}" } }
        probe: { url: "http://{hostname}:{port:api}/health", rule: health }
        outputs: { url: "http://{hostname}:{port:api}", docs: "http://{hostname}:{port:api}/docs" }
      web:
        group: app
        driver: process
        command: web-dev
        modes: { dev: { repo: ../web } }
        probe: { url: "http://{hostname}:{port:web}/", rule: web }
        outputs: { url: "http://{hostname}:{port:web}" }
      search:
        group: app
        driver: compose
        file: stack/app.yml
        service: search
        modes: { prebuilt: { image: registry.example/search, tag: "1" } }
        probe: { url: "http://{hostname}:{port:search}/health", rule: health }
      worker:
        group: app
        driver: compose
        file: stack/app.yml
        service: worker
        ports: 0
        modes: { prebuilt: { image: registry.example/worker, tag: "1" } }
      cache:
        group: data
        driver: compose
        file: stack/data.yml
        service: cache
        modes: { prebuilt: { image: registry.example/cache, tag: "7" } }
      db:
        group: data
        driver: compose
        file: stack/data.yml
        service: db
        modes: { prebuilt: { image: postgres, tag: "16" } }
        persistence: { volume: db-data }
        outputs: { url: "postgres://{hostname}:{port:db}/app" }
      mailer:
        group: app
        driver: compose
        file: stack/app.yml
        service: mailer
        ports: 0
        modes: { prebuilt: { image: registry.example/mailer, tag: "1" } }
      migrate:
        group: jobs
        driver: compose
        file: stack/jobs.yml
        service: migrate
        kind: oneshot
        ports: 0
        modes: { prebuilt: { image: postgres, tag: "16" } }
      seed:
        group: jobs
        driver: compose
        file: stack/jobs.yml
        service: seed
        kind: oneshot
        ports: 0
        modes: { prebuilt: { image: postgres, tag: "16" } }
      docs:
        group: app
        driver: compose
        file: stack/app.yml
        service: docs
        normallyOff: true
        modes: { prebuilt: { image: registry.example/docs, tag: "1" } }
"#;

fn files() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "stack/app.yml",
            "services:\n  api: { image: x, expose: [\"8080\"] }\n  search: { image: x }\n  worker: { image: x }\n  mailer: { image: x }\n  docs: { image: x }\n",
        ),
        (
            "stack/data.yml",
            "services:\n  cache: { image: x }\n  db: { image: x, expose: [\"5432\"] }\n",
        ),
        (
            "stack/jobs.yml",
            "services:\n  migrate: { image: x }\n  seed: { image: x }\n",
        ),
    ]
}

/// Intent and provenance as a brought-up instance has them: everything
/// wanted running but `cache`... (all but docs, and `mailer` stopped on
/// request so it reads idle while exited).
fn seed_records(work: &Path) {
    let now = "2026-11-13T12:00:00Z";
    let mut intent = IntentRecord::new("env", 0, now);
    for c in [
        "api", "web", "search", "worker", "cache", "db", "migrate", "seed", "mailer",
    ] {
        intent.components.insert(
            c.into(),
            ComponentIntent {
                desired: Desired::Running,
                channel: (c == "api").then(|| "main".to_string()),
                ..Default::default()
            },
        );
    }
    intent.components["cache"].desired = Desired::Stopped;
    intent.item = Some("ABC-1".into());
    intent.save(work).unwrap();
    let mut cache = ProvenanceCache::default();
    let mut api = Provenance::new(Mode::Prebuilt);
    api.build_id = Some("main".into());
    api.image = Some(PG_DIGEST.into());
    api.commit = Some(HEAD.into());
    api.git_ref = Some("main".into());
    cache.remember("api", api, now);
    let mut db = Provenance::new(Mode::Prebuilt);
    db.build_id = Some("16".into());
    db.image = Some(PG_DIGEST.into());
    cache.remember("db", db, now);
    let mut web = Provenance::new(Mode::Dev);
    web.commit = Some(HEAD.into());
    web.git_ref = Some("main".into());
    cache.remember("web", web, now);
    stack_core::jobs::write_atomic(&ProvenanceCache::path(work, "env-0"), &cache.to_json())
        .unwrap();
}

fn engines() -> FakeCli {
    let cli = docker_facts(FakeCli::new());
    let app = vec![
        Ctr::running("api"),
        Ctr::running("search"),
        Ctr::running("worker").state("restarting").restarts(4),
        Ctr::exited("mailer", 137),
    ];
    let data = vec![
        Ctr::exited("cache", 0),
        Ctr::running("db").health("starting"),
    ];
    let jobs = vec![Ctr::exited("migrate", 0), Ctr::exited("seed", 2)];
    let mut all_inspect: Vec<Value> = Vec::new();
    for (p, cs) in [
        ("env-0-app", &app),
        ("env-0-data", &data),
        ("env-0-jobs", &jobs),
    ] {
        all_inspect.extend(cs.iter().map(|c| c.inspect(p)));
    }
    let mut cli = cli;
    for (p, cs) in [
        ("env-0-app", &app),
        ("env-0-data", &data),
        ("env-0-jobs", &jobs),
    ] {
        let ps: Vec<String> = cs.iter().map(|c| c.ps(p).to_string()).collect();
        cli = cli.on(&["docker", "compose", "-p", p, "ps"], &ps.join("\n"));
    }
    cli.on(
        &["docker", "inspect", "--type", "container"],
        &Value::from(all_inspect).to_string(),
    )
    .on(
        &[
            "docker",
            "manifest",
            "inspect",
            "--verbose",
            "registry.example/api:main",
        ],
        &format!(r#"{{"Descriptor":{{"digest":"{NEW_DIGEST}","platform":{{"os":"linux"}}}}}}"#),
    )
}

fn http() -> FakeHttp {
    FakeHttp::default()
        .status("http://dev.example.test:15000/health", 200)
        .status("http://dev.example.test:15002/health", 503)
        .status("http://dev.example.test:15001/", 302)
}

fn mask(v: &mut Value) {
    v["freshness"]["asOf"] = "<now>".into();
    v["dataSafety"]["project"] = "<project>".into();
    for c in v["components"].as_array_mut().unwrap() {
        if !c["uptime"].is_null() {
            c["uptime"] = "<uptime>".into();
        }
    }
}

#[test]
fn one_answer_holds_all_nine_states_and_every_cell() {
    let p = Project::with(YML, &files());
    seed_records(&p.work());
    let f = fakes(engines(), http());
    // web runs as a process the stack started.
    let mut table = ProcessTable::default();
    table.processes.insert(
        "web".into(),
        ProcessEntry {
            pid: 4321,
            argv: vec!["npm".into(), "run".into(), "dev".into()],
            cwd: p.dir.join("../web"),
            log: chappa_stack::procs::log_path(&p.work(), "env-0", "web"),
            started_utc: "2026-11-13T11:00:00Z".into(),
            stopped_utc: None,
        },
    );
    table.save(&p.work(), "env-0").unwrap();
    f.procs.set_alive(4321);
    let bg = Background::new();
    let src = Source::with_background(
        f.world.clone(),
        ProjectConfig::new(&p.dir),
        Token::parse(TOKEN).unwrap(),
        "127.0.0.1:5000".into(),
        bg.clone(),
    );
    // First answer: the registry and the engine facts are not known yet.
    let (status, first) = get(&src, "/api/environment");
    assert_eq!(status, 200);
    let api = &first["components"][0];
    assert!(
        api.get("badges").is_none(),
        "no staleness badge before the refresher ran"
    );
    // Let the refresher ask once, then answer again.
    bg.refresh_once(&f.world);
    let (_, mut env) = get(&src, "/api/environment");

    let states: IndexMap<String, String> = env["components"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["id"].as_str().unwrap().to_string(),
                c["state"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    let expected = [
        ("api", "up"),
        ("web", "up"),
        ("search", "unhealthy"),
        ("worker", "crashloop"),
        ("cache", "stopped-idle"),
        ("db", "starting"),
        ("mailer", "stopped-problem"),
        ("migrate", "oneshot-ok"),
        ("seed", "oneshot-failed"),
        ("docs", "not-deployed"),
    ];
    for (id, state) in expected {
        assert_eq!(states[id], state, "{id}");
    }
    let by_id = |id: &str| {
        env["components"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["id"] == id)
            .unwrap()
            .clone()
    };
    let api = by_id("api");
    assert_eq!(api["group"], "app");
    assert_eq!(api["kind"], "service");
    assert!(api["uptime"].as_str().is_some());
    assert_eq!(api["restarts"], 0);
    assert_eq!(api["source"]["mode"], "prebuilt");
    assert_eq!(api["source"]["ref"], "registry.example/api:main");
    assert_eq!(api["source"]["commit"], HEAD);
    assert_eq!(api["source"]["buildId"], "main");
    assert_eq!(api["source"]["image"], PG_DIGEST);
    assert_eq!(api["probe"]["rule"], "health");
    assert_eq!(api["probe"]["up"], true);
    assert_eq!(api["probe"]["status"], 200);
    assert_eq!(api["links"][0]["browser"], true);
    assert_eq!(api["links"][0]["url"], "http://dev.example.test:15000");
    assert_eq!(api["links"][1]["label"], "docs");
    assert_eq!(api["badges"][0]["text"], "newer prebuilt");
    let ids: Vec<&str> = api["actions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        [
            "stop",
            "restart",
            "rebuild",
            "logs",
            "open-url",
            "open-docs"
        ]
    );
    let logs = api["actions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == "logs")
        .unwrap();
    assert_eq!(logs["kind"], "terminal");
    assert_eq!(
        logs["command"],
        serde_json::json!(["docker", "logs", "-f", "env-0-app-api-1"])
    );

    let web = by_id("web");
    assert_eq!(web["source"]["mode"], "dev");
    assert_eq!(web["source"]["ref"], "main");
    assert!(
        web["uptime"].as_str().is_some(),
        "a process has an uptime too"
    );
    let web_logs = web["actions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == "logs")
        .unwrap();
    assert_eq!(web_logs["command"][1], "follow");

    let search = by_id("search");
    assert_eq!(search["probe"]["up"], false);
    assert_eq!(search["probe"]["status"], 503);
    assert!(search["hint"].as_str().unwrap().contains("health"));
    assert_eq!(by_id("worker")["restarts"], 4);
    assert!(by_id("worker")["hint"]
        .as_str()
        .unwrap()
        .contains("restarted 4 times"));
    let mailer = by_id("mailer");
    assert_eq!(mailer["exitCode"], 137);
    assert!(mailer["hint"].as_str().unwrap().contains("wanted running"));
    assert_eq!(by_id("seed")["exitCode"], 2);
    assert_eq!(by_id("migrate")["kind"], "oneshot");
    let db = by_id("db");
    assert_eq!(db["badges"][0]["text"], "no data-safety check");
    assert!(db.get("links").is_none(), "a postgres:// URL is not a link");
    assert_eq!(by_id("docs")["source"], Value::Null);

    // groups in definition order, labelled
    let groups: Vec<&str> = env["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["label"].as_str().unwrap())
        .collect();
    assert_eq!(groups, ["Application", "Data", "Jobs"]);
    // alerts: every red component, with its cause, scoped to it
    let alerted: Vec<&str> = env["alerts"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["componentId"].as_str())
        .collect();
    // (and db, which keeps data with no data-safety check: a warning)
    assert_eq!(alerted, ["search", "worker", "mailer", "seed", "db"]);
    let db_alert = env["alerts"].as_array().unwrap().last().unwrap();
    assert_eq!(db_alert["severity"], "warn");
    assert_eq!(
        db_alert["text"],
        "db keeps data and has no data-safety check"
    );
    assert_eq!(env["dataSafety"]["state"], "unscanned");
    assert_eq!(env["dataSafety"]["policy"], "warn");
    assert_eq!(env["dataSafety"]["blocksAgents"], false);
    assert_eq!(env["dataSafety"]["unscanned"], serde_json::json!(["db"]));
    // facts: the stack (with its actions), engines, memory
    let stack = &env["facts"][0];
    assert_eq!(stack["id"], "stack");
    let rows: Vec<String> = stack["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            format!(
                "{}={}",
                r["label"].as_str().unwrap(),
                r["value"].as_str().unwrap()
            )
        })
        .collect();
    assert!(rows.contains(&"Instance=env-0".to_string()), "{rows:?}");
    assert!(rows.contains(&"Bound item=ABC-1".to_string()), "{rows:?}");
    assert!(
        rows.iter()
            .any(|r| r.starts_with("Slot=0 of 2 · ports 15000–15005 (block 15000–15049)")),
        "{rows:?}"
    );
    let stack_ids: Vec<&str> = stack["actions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        stack_ids,
        [
            "up",
            "start",
            "stop",
            "down",
            "down-volumes",
            "rebuild",
            "gc"
        ]
    );
    assert_eq!(
        env["facts"][1]["rows"][0]["value"],
        "v29.0.0 · linux containers · 16.0 GB total"
    );
    // banners: the definition's one warning (db has no data-safety check)
    assert_eq!(env["banners"][0]["id"], "definition-warnings");

    mask(&mut env);
    scrub(&mut env, &p.dir);
    let snap = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots/environment.json");
    let text = serde_json::to_string_pretty(&env).unwrap() + "\n";
    if std::env::var("UPDATE_SNAPSHOTS").as_deref() == Ok("1") {
        std::fs::create_dir_all(snap.parent().unwrap()).unwrap();
        std::fs::write(&snap, &text).unwrap();
    }
    let want = std::fs::read_to_string(&snap)
        .expect("snapshot missing; run with UPDATE_SNAPSHOTS=1")
        .replace("\r\n", "\n");
    assert_eq!(
        text, want,
        "the payload moved; rerun with UPDATE_SNAPSHOTS=1 and review the diff"
    );
}

/// An engine whose registry questions take seconds, like a real one. The
/// wait happens before the shared fake is locked, so nothing else waits
/// with it.
struct SlowRegistry {
    inner: std::sync::Arc<chappa_stack::world::Locked<FakeCli>>,
    delay: Duration,
}

impl EngineCli for SlowRegistry {
    fn run(&self, inv: &Invocation) -> Result<CliOutput, CliError> {
        if inv.args.iter().any(|a| a == "manifest") {
            std::thread::sleep(self.delay);
        }
        self.inner.run(inv)
    }
}

#[test]
fn status_never_waits_for_a_slow_registry() {
    let p = Project::with(YML, &files());
    seed_records(&p.work());
    let mut f = fakes(engines(), http());
    f.world.registry = std::sync::Arc::new(SlowRegistry {
        inner: chappa_stack::world::Locked::new(engines()),
        delay: Duration::from_secs(3),
    });
    let bg = Background::with_ttls(Duration::from_secs(600), Duration::from_secs(60));
    let refresher = bg.start(f.world.clone());
    let src = Source::with_background(
        f.world.clone(),
        ProjectConfig::new(&p.dir),
        Token::parse(TOKEN).unwrap(),
        "127.0.0.1:5000".into(),
        bg.clone(),
    );
    let t = Instant::now();
    let (_, env) = get(&src, "/api/environment");
    let first = t.elapsed();
    assert!(first < Duration::from_secs(2), "status took {first:?}");
    assert!(env["components"][0].get("badges").is_none());
    // A second call while the refresher is still waiting on the registry
    // returns just as fast.
    let t = Instant::now();
    let _ = get(&src, "/api/environment");
    assert!(t.elapsed() < Duration::from_secs(2));
    // Once the slow answer is in, the badge shows.
    assert!(eventually(|| {
        let (_, env) = get(&src, "/api/environment");
        env["components"][0]["badges"][0]["text"] == "newer prebuilt"
    }));
    bg.shutdown();
    refresher.join().unwrap();
}
