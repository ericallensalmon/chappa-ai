//! Secret scans on the stack side: opening an item scans its worktrees
//! (an ignored `.env` is found) and the commits they carry (a token
//! committed and then deleted is found), a source build's context is
//! scanned before it builds (`block-build` fails the job first, `warn`
//! builds), env files and the instance env are scanned by name, the
//! rescan reads only the files that changed, the `secretScan` block in
//! every state (a failed worktree scan blocks agents whatever the policy, a
//! failed scan of anything agents do not read only warns), and the
//! acknowledgement writes the project's one baseline.
//!
//! The fake scanner flags every line holding the word `PLANTED`; the
//! real-gitleaks tests plant a token assembled at run time and skip, with a
//! note, when gitleaks or git is not on PATH.

mod common;

use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chappa_stack::project::Loaded;
use chappa_stack::runjob::{run_job, EXIT_DONE, EXIT_FAILED};
use chappa_stack::secret_scan::{self, FixedScanner, GitleaksOnPath, ScanStore, ScannerSource};
use chappa_stack::server::Source;
use chappa_stack::world::ManualClock;
use common::*;
use stack_core::cli::FakeCli;
use stack_core::probe::FakeHttp;
use stack_core::safety::secrets::Baseline;
use stack_core::safety::{Finding, Location, SafetyError, SecretScanner, Severity};

const NOW: &str = "2026-11-15T12:00:00Z";
const WT: &str = ".work/ABC-1/src/api-1a2b3c4d";

fn yml(policy: &str) -> String {
    format!(
        r#"
name: sec
stacks:
  app:
    secrets: {{ onFinding: {policy}, excludes: [fixtures/] }}
    env:
      TOKEN: "PLANTED-in-the-env"
      PLAIN: "nothing"
    groups:
      core: {{ engine: docker }}
    components:
      api:
        group: core
        driver: compose
        file: stack/core.yml
        service: api
        modes:
          prebuilt: {{ image: registry.example/api, tag: main }}
          source: {{ repo: api }}
        default: source
"#
    )
}

const COMPOSE: &str = "services:\n  api:\n    image: registry.example/api:main\n    env_file: api.env\n    expose: [\"8080\"]\n";

/// Flags each line with `PLANTED`; can be told to fail; records what it read.
#[derive(Default)]
struct Planted {
    fail_files: Mutex<bool>,
    fail_commits: Mutex<bool>,
    read: Mutex<Vec<String>>,
}

impl Planted {
    fn failing_files(&self, on: bool) {
        *self.fail_files.lock().unwrap() = on;
    }
    fn failing_commits(&self, on: bool) {
        *self.fail_commits.lock().unwrap() = on;
    }
    fn take_read(&self) -> Vec<String> {
        let mut v = std::mem::take(&mut *self.read.lock().unwrap());
        v.sort();
        v
    }
}

impl SecretScanner for Planted {
    fn scan_files(&self, root: &Path, files: &[PathBuf]) -> Result<Vec<Finding>, SafetyError> {
        if *self.fail_files.lock().unwrap() {
            return Err(SafetyError::Timeout {
                what: "fake scanner".into(),
                seconds: 60,
            });
        }
        let mut out = Vec::new();
        for f in files {
            self.read
                .lock()
                .unwrap()
                .push(f.to_string_lossy().replace('\\', "/"));
            let text = fs::read_to_string(root.join(f)).unwrap_or_default();
            for (i, line) in text.lines().enumerate() {
                if line.contains("PLANTED") {
                    out.push(Finding {
                        rule: "planted".into(),
                        location: Location::Source {
                            file: f.clone(),
                            line: Some(i as u32 + 1),
                            commit: None,
                        },
                        count: 1,
                        severity: Severity::Bad,
                    });
                }
            }
        }
        Ok(out)
    }

    fn scan_commits(&self, _repo: &Path, _range: &str) -> Result<Vec<Finding>, SafetyError> {
        if *self.fail_commits.lock().unwrap() {
            return Err(SafetyError::Scanner("the fake scanner is down".into()));
        }
        Ok(Vec::new())
    }
}

struct Sec {
    p: Project,
    f: Fakes,
    src: Arc<Source>,
    clock: Arc<ManualClock>,
    scanner: Arc<Planted>,
}

impl Sec {
    fn loaded(&self) -> Loaded {
        Loaded::load(&self.p.dir, None, 0).unwrap()
    }
    fn wt(&self) -> PathBuf {
        self.p.dir.join(WT)
    }
    fn block(&self) -> Value {
        let (status, env) = get(&self.src, "/api/environment");
        assert_eq!(status, 200, "{env}");
        env["secretScan"].clone()
    }
    fn env(&self) -> Value {
        get(&self.src, "/api/environment").1
    }
    fn open_item(&self) -> Vec<String> {
        let mut log = Vec::new();
        secret_scan::scan_item(&self.f.world, &self.loaded(), "ABC-1", &mut |l| {
            log.push(l.to_string())
        });
        log
    }
}

/// A stack project with item ABC-1 active in slot 0 and one worktree of
/// api, a git repository on its branch with `main` as the base. `git`
/// decides whether the worktree is a real repository (for commit scans).
fn setup(policy: &str, git: bool) -> Option<Sec> {
    if git && !has_git() {
        eprintln!("skipped: git is not on PATH");
        return None;
    }
    let p = Project::with(
        &yml(policy),
        &[
            ("stack/core.yml", COMPOSE),
            ("stack/api.env", "API_KEY=PLANTED-in-a-file\nOTHER=1\n"),
            ("api/Dockerfile", "FROM scratch\n"),
            ("api/main.txt", "nothing here\n"),
            ("shared-checkouts-allow.txt", "api\n"),
        ],
    );
    let wt = p.dir.join(WT);
    fs::create_dir_all(&wt).unwrap();
    fs::write(wt.join("README"), "api\n").unwrap();
    if git {
        test_git(&wt, &["init", "-q", "-b", "main"]);
        fs::write(wt.join(".gitignore"), ".env\n").unwrap();
        commit_file(&wt, "README", "api\n", "start");
        test_git(&wt, &["checkout", "-q", "-b", "task/ABC-1-fix"]);
    }
    let work = p.work();
    fs::write(
        work.join("ABC-1").join("item.json"),
        json!({
            "ref": { "source": "tracker", "key": "ABC-1" },
            "key": "ABC-1",
            "repos": { "api": { "branch": "task/ABC-1-fix", "path": "src/api-1a2b3c4d", "c8": "1a2b3c4d", "base": "main", "createdUtc": NOW } },
            "createdUtc": NOW,
            "updatedUtc": NOW,
        })
        .to_string(),
    )
    .unwrap();
    fs::write(
        work.join("active.json"),
        json!({ "slots": { "0": { "item": "ABC-1", "instance": "app-0", "sinceUtc": NOW } } })
            .to_string(),
    )
    .unwrap();
    // No image of api yet, so an up builds it.
    let cli = with_docker_project(docker_facts(FakeCli::new()), "app-0-core", &[]).failing(
        &["docker", "image", "inspect"],
        1,
        "Error: No such image",
    );
    let mut f = fakes(cli, FakeHttp::default());
    let clock = ManualClock::at(NOW);
    f.world.clock = clock.clone();
    let scanner = Arc::new(Planted::default());
    f.world.secrets = Arc::new(FixedScanner(scanner.clone()));
    let src = source(f.world.clone(), &p.dir);
    Some(Sec {
        p,
        f,
        src,
        clock,
        scanner,
    })
}

fn files_of(block: &Value) -> Vec<String> {
    let mut v: Vec<String> = block["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            format!(
                "{}:{} ({})",
                f["file"].as_str().unwrap(),
                f["line"],
                f["rule"].as_str().unwrap()
            )
        })
        .collect();
    v.sort();
    v
}

/// The findings in the worktree only.
fn tree_files(block: &Value) -> Vec<String> {
    files_of(block)
        .into_iter()
        .filter(|f| f.starts_with(WT))
        .collect()
}

fn ack_action(env: &Value) -> Value {
    env["banners"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["id"] == "secret-scan")
        .and_then(|b| b["actions"][0].as_object().cloned())
        .map(Value::Object)
        .unwrap_or_else(|| panic!("no secret-scan banner with an action: {env}"))
}

fn ack(
    src: &Source,
    action: &Value,
    note: &str,
    confirmed: bool,
    actor: Option<&str>,
) -> (u16, Value) {
    let body = json!({
        "id": secret_scan::ACK_ACTION_ID,
        "target": { "view": "environment", "key": null },
        "params": action["params"],
        "confirmed": confirmed,
        "note": note,
    });
    let caller = chappa_stack::server::Caller {
        runs_jobs: true,
        actor: actor.map(str::to_string),
    };
    src.handle_as(
        "POST",
        "/api/actions",
        Some(&bearer()),
        &caller,
        &body.to_string(),
    )
}

#[test]
fn opening_an_item_finds_an_ignored_env_file_in_its_worktree() {
    let Some(s) = setup("block-agents", true) else {
        return;
    };
    fs::write(s.wt().join(".env"), "TOKEN=PLANTED\n").unwrap();
    fs::create_dir_all(s.wt().join("fixtures")).unwrap();
    fs::write(
        s.wt().join("fixtures").join("sample.txt"),
        "PLANTED but excluded\n",
    )
    .unwrap();
    assert_eq!(
        s.block()["state"],
        "unscanned",
        "a bound worktree not scanned yet"
    );
    assert_eq!(s.block()["blocksAgents"], false);
    let log = s.open_item();
    assert!(
        log.iter()
            .any(|l| l.contains("api worktree") && l.contains(".env:1 (planted)")),
        "{log:?}"
    );
    let b = s.block();
    assert_eq!(b["state"], "not-clean", "{b}");
    assert_eq!(b["policy"], "block-agents");
    assert_eq!(b["blocksAgents"], true);
    assert_eq!(
        files_of(&b),
        [format!("{WT}/.env:1 (planted)")],
        "excludes are left out"
    );
    assert_eq!(b["bound"]["key"], "ABC-1");
    assert_eq!(b["ackActionId"], "secret-scan-ack");
    assert_eq!(
        b["reasons"],
        json!([{ "component": null, "text": format!("api worktree: 1 possible secret ({WT}/.env:1 (planted))") }])
    );
    let env = s.env();
    let alert = env["alerts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["text"].as_str().unwrap_or("").starts_with("secret scan:"))
        .cloned()
        .unwrap();
    assert_eq!(alert["severity"], "bad");
    assert!(alert["text"]
        .as_str()
        .unwrap()
        .ends_with("— agents blocked"));
    let action = ack_action(&env);
    assert_eq!(action["note"], "required");
    assert!(action["confirm"]
        .as_str()
        .unwrap()
        .contains(&format!("{WT}/.env:1 (planted)")));
    let facts = env["facts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == "secret-scan")
        .cloned()
        .unwrap();
    assert!(facts["rows"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["label"] == "api worktree" && r["value"] == "1 possible secret"));
    // The stored results hold places only.
    let stored = fs::read_to_string(secret_scan::store_path(&s.p.work(), "app-0")).unwrap();
    assert!(
        !stored.contains("TOKEN=") && stored.contains(".env"),
        "{stored}"
    );
}

#[test]
fn a_finding_under_warn_shows_but_blocks_nothing() {
    let Some(s) = setup("warn", true) else { return };
    fs::write(s.wt().join(".env"), "TOKEN=PLANTED\n").unwrap();
    s.open_item();
    let b = s.block();
    assert_eq!(
        (
            b["state"].as_str(),
            b["policy"].as_str(),
            b["blocksAgents"].as_bool()
        ),
        (Some("not-clean"), Some("warn"), Some(false))
    );
    let env = s.env();
    assert!(env["alerts"].as_array().unwrap().iter().any(
        |a| a["severity"] == "warn" && a["text"].as_str().unwrap().starts_with("secret scan:")
    ));
    assert!(!env["banners"]
        .as_array()
        .unwrap()
        .iter()
        .any(|b| b["id"] == "secret-scan"));
}

#[test]
fn a_worktree_scan_that_does_not_finish_blocks_agents_whatever_the_policy() {
    let Some(s) = setup("warn", true) else { return };
    s.scanner.failing_files(true);
    s.open_item();
    let b = s.block();
    assert_eq!(b["state"], "error", "{b}");
    assert_eq!(
        b["blocksAgents"], true,
        "agents would read files nobody checked"
    );
    assert!(
        b["reasons"][0]["text"]
            .as_str()
            .unwrap()
            .contains("did not finish within 60 s"),
        "{b}"
    );
    // The acknowledgement lets agents in until the next scan succeeds.
    let action = ack_action(&s.env());
    assert!(
        action["confirm"]
            .as_str()
            .unwrap()
            .contains("without a complete scan"),
        "{action}"
    );
    let (status, body) = ack(
        &s.src,
        &action,
        "the scanner is being replaced",
        true,
        Some("human:alex"),
    );
    assert_eq!(status, 200, "{body}");
    let b = s.block();
    assert_eq!(
        (b["state"].as_str(), b["blocksAgents"].as_bool()),
        (Some("acknowledged"), Some(false)),
        "{b}"
    );
    assert_eq!(b["ack"]["by"], "human:alex");
    // A successful scan ends it; a later failure blocks again.
    s.scanner.failing_files(false);
    s.open_item();
    assert_eq!(s.block()["state"], "clean");
    s.scanner.failing_files(true);
    s.open_item();
    assert_eq!(
        s.block()["blocksAgents"],
        true,
        "the old acknowledgement does not carry over"
    );
}

#[test]
fn a_commit_scan_that_does_not_finish_only_warns() {
    let Some(s) = setup("block-agents", true) else {
        return;
    };
    s.scanner.failing_commits(true);
    s.open_item();
    let b = s.block();
    assert_eq!(b["state"], "error", "{b}");
    assert_eq!(
        b["blocksAgents"], false,
        "agents do not read commits from the worktree"
    );
    let env = s.env();
    let alert = env["alerts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| {
            a["text"]
                .as_str()
                .unwrap()
                .contains("commits ahead of main")
        })
        .cloned()
        .unwrap();
    assert_eq!(alert["severity"], "warn", "{alert}");
}

#[test]
fn no_scanner_blocks_a_bound_worktree_when_a_policy_is_declared_and_says_unscanned_otherwise() {
    let Some(s) = setup("block-agents", false) else {
        return;
    };
    let mut world = s.f.world.clone();
    world.secrets = Arc::new(secret_scan::NoScanner);
    let src = source(world.clone(), &s.p.dir);
    let b = get(&src, "/api/environment").1["secretScan"].clone();
    assert_eq!(
        (b["state"].as_str(), b["blocksAgents"].as_bool()),
        (Some("error"), Some(true)),
        "{b}"
    );
    assert!(b["reasons"][0]["text"]
        .as_str()
        .unwrap()
        .contains("install gitleaks"));
    // Without `secrets:` and without a scanner, nothing is scanned or said.
    let text = fs::read_to_string(s.p.dir.join("chappa.yml")).unwrap();
    fs::write(
        s.p.dir.join("chappa.yml"),
        text.replace(
            "    secrets: { onFinding: block-agents, excludes: [fixtures/] }\n",
            "",
        ),
    )
    .unwrap();
    let b = get(&src, "/api/environment").1["secretScan"].clone();
    assert_eq!(
        (
            b["state"].as_str(),
            b["blocksAgents"].as_bool(),
            b["policy"].is_null()
        ),
        (Some("unscanned"), Some(false), true),
        "{b}"
    );
}

#[test]
fn acknowledging_writes_the_projects_baseline_with_project_relative_fingerprints() {
    let Some(s) = setup("block-agents", true) else {
        return;
    };
    fs::write(s.wt().join(".env"), "TOKEN=PLANTED\n").unwrap();
    s.open_item();
    let env = s.env();
    let action = ack_action(&env);
    assert_eq!(
        ack(&s.src, &action, "fixture", false, None).1["kind"],
        "confirmation-required"
    );
    assert_eq!(
        ack(&s.src, &action, "  ", true, None).1["kind"],
        "note-required"
    );
    let mut stale = action.clone();
    stale["params"]["fingerprint"] = "0000".into();
    assert_eq!(
        ack(&s.src, &stale, "fixture", true, None).1["kind"],
        "stale-plan"
    );
    let (status, body) = ack(&s.src, &action, "a test fixture", true, Some("human:alex"));
    assert_eq!(status, 200, "{body}");
    let b = s.block();
    assert_eq!(
        (b["state"].as_str(), b["blocksAgents"].as_bool()),
        (Some("acknowledged"), Some(false)),
        "{b}"
    );
    assert_eq!(
        (b["ack"]["by"].as_str(), b["ack"]["note"].as_str()),
        (Some("human:alex"), Some("a test fixture"))
    );
    // The one baseline, keyed the way the app's pre-agent scan keys it.
    let raw = fs::read_to_string(s.p.dir.join(".work").join("secrets-baseline.json")).unwrap();
    assert!(raw.contains(&format!("\"{WT}/.env:planted:1\"")), "{raw}");
    assert!(!raw.contains("TOKEN="));
    assert_eq!(
        ack(&s.src, &action, "again", true, None).1["kind"],
        "not-offered"
    );
}

#[test]
fn an_entry_written_by_the_app_gate_silences_the_same_finding_here() {
    let Some(s) = setup("block-agents", true) else {
        return;
    };
    fs::write(s.wt().join(".env"), "TOKEN=PLANTED\n").unwrap();
    s.open_item();
    assert_eq!(s.block()["blocksAgents"], true);
    // What the app writes when the finding is acknowledged at agent launch
    // in the worktree: the file named from the project's folder.
    let work = s.p.dir.join(".work");
    let mut b = Baseline::load(&work).unwrap();
    let from_app = Finding {
        rule: "planted".into(),
        location: Location::Source {
            file: PathBuf::from(format!("{WT}/.env")),
            line: Some(1),
            commit: None,
        },
        count: 1,
        severity: Severity::Bad,
    };
    b.acknowledge(
        &[from_app],
        "user",
        "acknowledged from the launch dialog",
        NOW,
    );
    b.save(&work).unwrap();
    let block = s.block();
    assert_eq!(
        (block["state"].as_str(), block["blocksAgents"].as_bool()),
        (Some("acknowledged"), Some(false)),
        "{block}"
    );
}

#[test]
fn the_rescan_reads_only_changed_files_and_a_full_one_comes_every_ten_minutes() {
    let Some(s) = setup("block-agents", false) else {
        return;
    };
    for n in ["a.txt", "b.txt", "c.txt"] {
        fs::write(s.wt().join(n), "plain\n").unwrap();
    }
    fs::write(s.wt().join("d.txt"), "PLANTED\n").unwrap();
    assert!(s.src.secret_scan_pass(), "the first pass scans everything");
    let first = s.scanner.take_read();
    assert!(
        first.contains(&"a.txt".to_string()) && first.contains(&"d.txt".to_string()),
        "{first:?}"
    );
    assert_eq!(tree_files(&s.block()), [format!("{WT}/d.txt:1 (planted)")]);
    // Nothing changed: the tree is not read again.
    s.scanner.take_read();
    s.src.secret_scan_pass();
    let again: Vec<String> = s
        .scanner
        .take_read()
        .into_iter()
        .filter(|f| f.ends_with(".txt"))
        .collect();
    assert!(
        again.is_empty(),
        "nothing in the tree was read again: {again:?}"
    );
    // One file changed, one added, one deleted: only the first two are read.
    fs::write(s.wt().join("b.txt"), "now PLANTED here, and longer\n").unwrap();
    fs::write(s.wt().join("e.txt"), "new\n").unwrap();
    fs::remove_file(s.wt().join("d.txt")).unwrap();
    s.src.secret_scan_pass();
    let read: Vec<String> = s
        .scanner
        .take_read()
        .into_iter()
        .filter(|f| f.ends_with(".txt"))
        .collect();
    assert_eq!(read, ["b.txt", "e.txt"]);
    assert_eq!(
        tree_files(&s.block()),
        [format!("{WT}/b.txt:1 (planted)")],
        "the deleted file's finding went"
    );
    // Ten minutes on, a full pass reads everything again.
    s.clock.advance(Duration::from_secs(601));
    s.src.secret_scan_pass();
    let all: Vec<String> = s
        .scanner
        .take_read()
        .into_iter()
        .filter(|f| f.ends_with(".txt"))
        .collect();
    assert_eq!(all, ["a.txt", "b.txt", "c.txt", "e.txt"]);
}

#[test]
fn env_files_and_the_instance_env_are_scanned_by_name_never_by_value() {
    let Some(s) = setup("block-agents", false) else {
        return;
    };
    assert!(s.src.secret_scan_pass());
    let b = s.block();
    let files = files_of(&b);
    assert!(
        files.contains(&"stack/api.env:1 (planted)".to_string()),
        "{files:?}"
    );
    let env_findings: Vec<&Value> = b["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["file"].as_str().unwrap().starts_with("instance env"))
        .collect();
    assert_eq!(env_findings.len(), 1, "{b}");
    assert_eq!(env_findings[0]["file"], "instance env: TOKEN");
    assert!(env_findings[0].get("line").is_none());
    // The scratch file is gone, and nothing kept holds a value.
    let leftovers: Vec<String> = fs::read_dir(s.p.dir.join(".work"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("secret-scan-env"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    let stored = fs::read_to_string(secret_scan::store_path(&s.p.work(), "app-0")).unwrap();
    assert!(!stored.contains("PLANTED-in"), "{stored}");
    assert!(!s.env().to_string().contains("PLANTED-in"));
}

fn up(s: &Sec) -> (i32, String, Vec<String>) {
    let env = s.env();
    let action = env["actions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == "up")
        .cloned()
        .unwrap_or_else(|| panic!("no up: {env}"));
    let (status, body) = post(
        &s.src,
        "/api/actions",
        &json!({ "id": "up", "target": { "view": "environment", "key": null }, "params": action["params"], "confirmed": true }),
        true,
    );
    assert_eq!(status, 202, "{body}");
    let mut pane: Vec<u8> = Vec::new();
    let code = run_job(
        &s.f.world,
        &s.p.dir,
        body["jobId"].as_str().unwrap(),
        &mut pane,
    );
    let calls: Vec<String> =
        s.f.cli
            .with(|c| c.calls.borrow().iter().map(|i| i.to_string()).collect());
    (code, String::from_utf8(pane).unwrap(), calls)
}

#[test]
fn block_build_fails_the_build_job_before_building_and_warn_builds() {
    for (policy, expect) in [("block-build", EXIT_FAILED), ("warn", EXIT_DONE)] {
        let Some(s) = setup(policy, false) else {
            return;
        };
        // Unbind the item: api builds from the shared clone in the project.
        fs::remove_file(s.p.work().join("active.json")).unwrap();
        fs::write(s.p.dir.join("api").join(".env"), "TOKEN=PLANTED\n").unwrap();
        let (code, log, calls) = up(&s);
        assert!(
            log.contains("api build context") && log.contains("api/.env:1 (planted)"),
            "{policy}: {log}"
        );
        let built = calls.iter().any(|c| c.contains(" build "));
        assert_eq!(code, expect, "{policy}: {log}");
        if policy == "block-build" {
            assert!(
                log.contains("not built: possible secrets would go into the image"),
                "{log}"
            );
            assert!(!built, "nothing was built: {calls:?}");
        } else {
            assert!(built, "warn builds: {calls:?}");
        }
        let b = s.block();
        assert!(
            files_of(&b).contains(&"api/.env:1 (planted)".to_string()),
            "{b}"
        );
    }
}

#[test]
fn a_build_context_that_cannot_be_scanned_fails_the_build_only_under_block_build() {
    for (policy, expect) in [("block-build", EXIT_FAILED), ("block-agents", EXIT_DONE)] {
        let Some(s) = setup(policy, false) else {
            return;
        };
        fs::remove_file(s.p.work().join("active.json")).unwrap();
        s.scanner.failing_files(true);
        let (code, log, _) = up(&s);
        assert_eq!(code, expect, "{policy}: {log}");
        let b = s.block();
        assert_eq!(b["state"], "error");
        assert_eq!(
            b["blocksAgents"], false,
            "agents do not read build contexts: {b}"
        );
    }
}

#[test]
fn real_gitleaks_finds_an_ignored_env_and_a_token_committed_then_deleted() {
    let Some(scanner) = GitleaksOnPath::default().scanner(&std::env::temp_dir()) else {
        eprintln!("skipped: gitleaks is not on PATH");
        return;
    };
    drop(scanner);
    let Some(mut s) = setup("block-agents", true) else {
        return;
    };
    s.f.world.secrets = Arc::new(GitleaksOnPath::default());
    s.src = source(s.f.world.clone(), &s.p.dir);
    // Assembled at run time so no source file holds a token-shaped string.
    let token = format!("ghp_{}", "A1b2C3d4".repeat(5));
    fs::write(s.wt().join(".env"), format!("GITHUB_TOKEN={token}\n")).unwrap();
    commit_file(
        &s.wt(),
        "cfg.txt",
        &format!("token = {token}\n"),
        "add a config",
    );
    fs::remove_file(s.wt().join("cfg.txt")).unwrap();
    test_git(&s.wt(), &["commit", "-q", "-am", "remove it again"]);
    let log = s.open_item();
    let b = s.block();
    assert_eq!(b["blocksAgents"], true, "{b} {log:?}");
    let findings = b["findings"].as_array().unwrap();
    assert!(
        findings
            .iter()
            .any(|f| f["file"] == format!("{WT}/.env") && f.get("commit").is_none()),
        "{b}"
    );
    assert!(
        findings
            .iter()
            .any(|f| f["file"] == format!("{WT}/cfg.txt") && f["commit"].is_string()),
        "{b}"
    );
    assert!(!b.to_string().contains(&token));
    let stored = fs::read_to_string(secret_scan::store_path(&s.p.work(), "app-0")).unwrap();
    assert!(!stored.contains(&token));
}

/// A rule only the person's own gitleaks config has: a house marker.
const HOUSE_CONFIG: &str = r#"title = "house rules"

[extend]
useDefault = true

[[rules]]
id = "house-marker"
description = "a house deploy marker"
regex = '''HOUSEMARK-[0-9A-Z]{12}'''
"#;

/// An allow-everything config someone left in the scanned tree.
const ALLOW_ALL: &str = "title = \"allow all\"\n[allowlist]\npaths = ['''.*''']\n";

fn rules_of(block: &Value) -> Vec<String> {
    block["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|f| f["rule"].as_str().map(str::to_string))
        .collect()
}

#[test]
fn real_gitleaks_scans_with_the_settings_config_and_never_one_from_the_tree() {
    if GitleaksOnPath::default()
        .scanner(&std::env::temp_dir())
        .is_none()
    {
        eprintln!("skipped: gitleaks is not on PATH");
        return;
    }
    let Some(mut s) = setup("block-agents", true) else {
        return;
    };
    let config_dir = tempfile::tempdir().unwrap();
    let config = config_dir.path().join("house.toml");
    fs::write(&config, HOUSE_CONFIG).unwrap();
    // Assembled at run time so no source line holds the marker.
    let marker = format!("HOUSEMARK-{}", "7Q4W9X2K8M3N");
    fs::write(s.wt().join("deploy.txt"), format!("marker {marker}\n")).unwrap();
    // A config and an ignore file planted in the tree that would hide it.
    fs::write(s.wt().join(".gitleaks.toml"), ALLOW_ALL).unwrap();
    fs::write(
        s.wt().join(".gitleaksignore"),
        "deploy.txt:house-marker:1\n",
    )
    .unwrap();

    // The built-in default (what the app uses with nothing set): no such rule.
    s.f.world = s.f.world.clone().with_gitleaks_config(None);
    s.src = source(s.f.world.clone(), &s.p.dir);
    s.open_item();
    let b = s.block();
    assert!(!rules_of(&b).contains(&"house-marker".to_string()), "{b}");

    // The Settings config, as `serve --gitleaks-config` hands it over.
    s.f.world = s.f.world.clone().with_gitleaks_config(Some(config.clone()));
    s.src = source(s.f.world.clone(), &s.p.dir);
    let log = s.open_item();
    let b = s.block();
    assert!(
        b["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["rule"] == "house-marker" && f["file"] == format!("{WT}/deploy.txt")),
        "{b} {log:?}"
    );
    assert_eq!(b["blocksAgents"], true, "{b}");
    assert!(!b.to_string().contains(&marker));
    // Every job the source starts carries the same config on.
    let argv = s.f.world.job_argv("job-1");
    assert_eq!(
        argv[argv.len() - 2..],
        [
            chappa_stack::world::GITLEAKS_CONFIG_ARG.to_string(),
            config.display().to_string()
        ]
    );
}

/// The scan of the instance env reports a database password written into
/// the definition and lets one the runner generated (`{secret:db}`) pass:
/// the generated value is scanned as its placeholder. The same value
/// written out in full is still reported, so it is the placeholder, not
/// the scanner's taste, that lets it pass. Real gitleaks, the app's rules.
#[test]
fn real_gitleaks_reports_a_written_password_and_not_a_generated_one() {
    let Some(scanner) = GitleaksOnPath::default().scanner(&std::env::temp_dir()) else {
        eprintln!("skipped: gitleaks is not on PATH");
        return;
    };
    // Built at run time so no source line holds a URL with a password.
    let written = [
        "postgres://app:",
        &"Wr1tten9".repeat(3),
        "@localhost:5432/app",
    ]
    .concat();
    let yml = format!(
        r#"
name: gen
stacks:
  app:
    hostname: dev.example.test
    env:
      GENERATED_URL: "postgres://app:{{secret:db}}@{{hostname}}:{{port:db}}/app"
      WRITTEN_URL: "{written}"
    groups:
      core: {{ engine: docker }}
    components:
      db:
        group: core
        driver: compose
        file: stack/core.yml
        service: db
        modes:
          prebuilt: {{ image: postgres, tag: "16" }}
        ports: 1
        env:
          POSTGRES_PASSWORD: "{{secret:db}}"
"#
    );
    let p = Project::with(
        &yml,
        &[(
            "stack/core.yml",
            "services:\n  db:\n    image: postgres:16\n    expose: [\"5432\"]\n",
        )],
    );
    let f = fakes(FakeCli::new(), FakeHttp::default());
    let loaded = Loaded::load(&p.dir, None, 0).unwrap();
    let runner = loaded.runner(&f.world, NOW);
    let (inst, _) = runner.instance(0).unwrap();
    let generated = fs::read_to_string(p.work().join("instances/app-0/secrets/db")).unwrap();
    assert_eq!(inst.secrets.get("db"), Some(generated.as_str()));

    let env = secret_scan::instance_env(&loaded, &inst);
    assert!(
        env.values().all(|v| !v.contains(&generated)),
        "the scanned env never holds the generated value: {env:?}"
    );
    assert_eq!(env["POSTGRES_PASSWORD (db)"], "{secret:db}");
    let entry = secret_scan::scan_env(&*scanner, &p.dir, &p.work(), &[], &env, NOW);
    assert!(entry.error.is_none(), "{:?}", entry.error);
    let flagged = |entry: &secret_scan::TargetEntry| -> Vec<String> {
        entry
            .findings
            .iter()
            .filter_map(|f| match &f.location {
                Location::Source { file, .. } => Some(file.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect()
    };
    assert_eq!(flagged(&entry), ["instance env: WRITTEN_URL"], "{entry:?}");

    // The generated value written out in full is a password like any other.
    let mut unmasked = env.clone();
    unmasked.insert(
        "SPELLED_OUT_URL".into(),
        ["postgres://app:", &generated, "@localhost:5432/app"].concat(),
    );
    let entry = secret_scan::scan_env(&*scanner, &p.dir, &p.work(), &[], &unmasked, NOW);
    assert!(
        flagged(&entry).contains(&"instance env: SPELLED_OUT_URL".to_string()),
        "{entry:?}"
    );
}

#[test]
fn a_store_that_cannot_be_read_keeps_agents_out_of_the_bound_worktree() {
    let Some(s) = setup("warn", false) else {
        return;
    };
    let path = secret_scan::store_path(&s.p.work(), "app-0");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "{ not json").unwrap();
    let b = s.block();
    assert_eq!(
        (b["state"].as_str(), b["blocksAgents"].as_bool()),
        (Some("error"), Some(true)),
        "{b}"
    );
    assert!(ScanStore::load(&s.p.work(), "app-0").is_err());
}

/// Every `mutates: true` action anywhere in a payload.
fn mutating_actions(v: &Value, out: &mut Vec<Value>) {
    match v {
        Value::Object(map) => {
            if let Some(Value::Array(actions)) = map.get("actions") {
                out.extend(actions.iter().filter(|a| a["mutates"] == true).cloned());
            }
            for child in map.values() {
                mutating_actions(child, out);
            }
        }
        Value::Array(items) => {
            for child in items {
                mutating_actions(child, out);
            }
        }
        _ => {}
    }
}

#[test]
fn while_the_definition_is_untrusted_acknowledgements_are_refused_like_every_action() {
    use chappa_stack::server::{
        DefinitionPin, UNTRUSTED_DISABLED, UNTRUSTED_KIND, UNTRUSTED_REFUSAL,
    };
    let Some(s) = setup("block-agents", false) else {
        return;
    };
    fs::write(s.wt().join(".env"), "TOKEN=PLANTED\n").unwrap();
    s.open_item();
    let action = ack_action(&s.env());
    assert!(action.get("disabled").is_none(), "{action}");

    // Nothing trusted (or the definition changed since): read-only.
    s.src.set_definition_pin(DefinitionPin::Nothing);
    let env = s.env();
    // The read-only view still says what the scan found.
    assert_eq!(env["secretScan"]["blocksAgents"], true, "{env}");
    assert_eq!(env["banners"][0]["id"], UNTRUSTED_KIND);
    let mut mutating = Vec::new();
    mutating_actions(&env, &mut mutating);
    assert!(
        mutating
            .iter()
            .any(|a| a["id"] == secret_scan::ACK_ACTION_ID),
        "{env}"
    );
    for a in &mutating {
        assert_eq!(a["disabled"], UNTRUSTED_DISABLED, "{a}");
    }
    // The secret-scan acknowledgement is refused, nothing is written.
    let (code, body) = ack(&s.src, &action, "a note", true, Some("human:alex"));
    assert_eq!((code, body["kind"].as_str()), (409, Some(UNTRUSTED_KIND)));
    assert_eq!(body["message"], UNTRUSTED_REFUSAL);
    assert!(UNTRUSTED_REFUSAL.contains("trust it again"));
    assert!(!Baseline::path(&s.p.dir.join(".work")).exists());
    // The data-safety acknowledgement and the other changes too.
    for (path, body) in [
        (
            "/api/actions",
            json!({"id": "data-safety-ack", "target": {"view": "environment", "key": null}, "confirmed": true, "note": "a note"}),
        ),
        (
            "/api/actions",
            json!({"id": "data-safety-check", "target": {"view": "environment", "key": "api"}, "confirmed": true}),
        ),
        ("/api/work/owner", json!({"source": "tracker", "items": []})),
    ] {
        let (code, reply) = post(&s.src, path, &body, true);
        assert_eq!(
            (code, reply["kind"].as_str()),
            (409, Some(UNTRUSTED_KIND)),
            "{path} {body}: {reply}"
        );
    }
    // The other read-only views keep answering.
    assert_eq!(get(&s.src, "/api/work").0, 200);
    assert_eq!(get(&s.src, "/chappa/v1/manifest").0, 200);
}

/// A value the runner generated for an instance is a finding of its own
/// (`generated-secret`) in a tracked file of a bound worktree and in the
/// commits ahead of the base (one committed and deleted again included),
/// by file and line and never by value; an untracked file holding it is
/// not one (the work folder is where it lives). Every reply shows it as
/// `••••`, here in a finding's file name.
#[test]
fn a_generated_value_in_a_commit_or_tracked_file_is_flagged_and_every_reply_hides_it() {
    let Some(s) = setup("block-agents", true) else {
        return;
    };
    let names = ["db".to_string()].into();
    let value = stack_core::instance_secrets::load_or_create(&s.p.work(), "app-0", &names)
        .unwrap()
        .get("db")
        .unwrap()
        .to_owned();
    commit_file(
        &s.wt(),
        "db.txt",
        &format!("url=postgres://app:{value}@db/app\n"),
        "add the url",
    );
    fs::remove_file(s.wt().join("db.txt")).unwrap();
    test_git(&s.wt(), &["commit", "-q", "-am", "remove it again"]);
    commit_file(
        &s.wt(),
        "notes.txt",
        &format!("a\npassword {value}\n"),
        "notes",
    );
    fs::write(s.wt().join("scratch.txt"), format!("{value}\n")).unwrap();
    // A file whose name holds the value: its finding names it in the view.
    fs::write(s.wt().join(format!("{value}.txt")), "PLANTED\n").unwrap();
    let log = s.open_item();
    let (_, env) = get(&s.src, "/api/environment");
    let b = env["secretScan"].clone();
    let generated: Vec<String> = b["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["rule"] == stack_core::safety::secrets::generated::GENERATED_RULE)
        .map(|f| {
            format!(
                "{}:{}{}",
                f["file"].as_str().unwrap(),
                f["line"],
                if f["commit"].is_string() {
                    " @commit"
                } else {
                    ""
                }
            )
        })
        .collect();
    assert!(
        generated.contains(&format!("{WT}/notes.txt:2")),
        "{generated:?} {b}"
    );
    assert!(
        generated.contains(&format!("{WT}/notes.txt:2 @commit")),
        "{generated:?} {b}"
    );
    assert!(
        generated.contains(&format!("{WT}/db.txt:1 @commit")),
        "{generated:?} {b}"
    );
    assert!(
        !generated.iter().any(|g| g.contains("scratch.txt")),
        "an untracked file is where such a value may live: {generated:?}"
    );
    assert_eq!(b["blocksAgents"], true, "{b}");
    let planted = files_of(&b)
        .into_iter()
        .find(|f| f.contains("(planted)"))
        .unwrap_or_else(|| panic!("{b}"));
    assert_eq!(planted, format!("{WT}/••••.txt:1 (planted)"));
    let shown = env.to_string();
    assert!(!shown.contains(&value), "{shown}");
    assert!(
        log.iter()
            .all(|l| !l.contains(&format!("password {value}"))),
        "{log:?}"
    );
    let stored = fs::read_to_string(secret_scan::store_path(&s.p.work(), "app-0")).unwrap();
    assert!(
        !stored.contains(&format!("password {value}")),
        "the store names places, never contents"
    );
}

/// The project's own gitleaks config, trusted at its hash: its allowlist
/// lets one known public test value through. Edited after it was trusted,
/// the runner's hash check falls back to the built-in rules (the value is
/// found again) and the Environment view asks to review it.
#[test]
fn real_gitleaks_uses_a_trusted_project_config_and_falls_back_when_it_changes() {
    if GitleaksOnPath::default()
        .scanner(&std::env::temp_dir())
        .is_none()
    {
        eprintln!("skipped: gitleaks is not on PATH");
        return;
    }
    let Some(mut s) = setup("block-agents", true) else {
        return;
    };
    // A token-shaped public test value, assembled at run time.
    let public = format!("ghp_{}", "PubL1cTe5tVa1ue".repeat(3));
    let token = &public[..40];
    fs::create_dir_all(s.wt().join("samples")).unwrap();
    fs::write(
        s.wt().join("samples/public.txt"),
        format!("sample = {token}\n"),
    )
    .unwrap();
    let config_path = s.p.dir.join("tools").join("gitleaks.toml");
    fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    let config = format!(
        "[extend]\nuseDefault = true\n\n[allowlist]\ndescription = \"a public test value\"\nregexes = ['''^{token}$''']\n"
    );
    fs::write(&config_path, &config).unwrap();
    let hash = stack_core::safety::secrets::config::content_hash(config.as_bytes());
    let pinned = stack_core::safety::secrets::config::ProjectScanConfig::Trusted {
        path: config_path.clone(),
        hash: hash.clone(),
    };
    let is_found = |b: &Value| {
        b["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["file"] == format!("{WT}/samples/public.txt"))
    };
    let review = |env: &Value| {
        env["banners"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["id"] == secret_scan::CONFIG_REVIEW_BANNER)
            .cloned()
    };

    // Without the project config, the built-in rules find it.
    s.f.world = s.f.world.clone().with_gitleaks_config(None);
    s.src = source(s.f.world.clone(), &s.p.dir);
    s.open_item();
    assert!(is_found(&s.block()), "{}", s.block());

    // Trusted: its allowlist lets the known value through.
    s.f.world = s.f.world.clone().with_project_scan_config(pinned.clone());
    s.src = source(s.f.world.clone(), &s.p.dir);
    s.open_item();
    let env = s.env();
    assert!(!is_found(&env["secretScan"]), "{env}");
    assert_eq!(review(&env), None, "{env}");
    let argv = s.f.world.job_argv("job-1");
    assert_eq!(
        argv[argv.len() - 4..],
        [
            chappa_stack::world::PROJECT_GITLEAKS_CONFIG_ARG.to_string(),
            config_path.display().to_string(),
            chappa_stack::world::PROJECT_GITLEAKS_CONFIG_HASH_ARG.to_string(),
            hash.clone(),
        ]
    );

    // Edited since: the hash no longer matches, the built-in rules apply
    // again, and the view asks to review the config.
    fs::write(&config_path, format!("{config}# edited\n")).unwrap();
    s.open_item();
    let env = s.env();
    assert!(is_found(&env["secretScan"]), "{env}");
    let banner = review(&env).unwrap_or_else(|| panic!("{env}"));
    let text = banner.to_string();
    assert!(text.contains("changed since it was trusted"), "{text}");
    assert!(text.contains("the built-in rules"), "{text}");
    assert!(!env.to_string().contains(token));

    // Not trusted at all: never used, the same banner.
    let unpinned = stack_core::safety::secrets::config::ProjectScanConfig::from_args(
        Some(config_path.clone()),
        Some("none"),
    )
    .unwrap();
    s.f.world = s.f.world.clone().with_project_scan_config(unpinned);
    s.src = source(s.f.world.clone(), &s.p.dir);
    s.open_item();
    let env = s.env();
    assert!(is_found(&env["secretScan"]), "{env}");
    assert!(review(&env).unwrap().to_string().contains("is not trusted"));
}
