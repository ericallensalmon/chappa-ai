//! Item workspaces end to end, with real git against temporary repos: a
//! bare `origin` per repo, a shared clone of each under the stack's
//! `root:`, and the stack project beside them. Engines stay fakes.
//!
//! Start work from a tracker's row, Add repo (an existing branch on origin
//! or a new one from the base), Activate (the plan binds only the item's
//! repos; activating another item rebinds), drift after the branch moves,
//! Archive (refused while dirty or unpushed) and Restore, the overlay
//! payload (pinned as `tests/snapshots/work.json`; `UPDATE_SNAPSHOTS=1`
//! rewrites it), the manifest's `itemActions`, the shared-checkout guard,
//! and a Work list that never waits on a slow git.

mod common;

use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chappa_stack::records::{JobRecord, JobWork};
use chappa_stack::runjob::{run_job, EXIT_DONE, EXIT_REFUSED};
use chappa_stack::server::Source;
use chappa_stack::workspaces::WorkspaceOp;
use chappa_stack::world::ManualClock;
use common::*;
use stack_core::cli::{CliError, CliOutput, EngineCli, FakeCli, Invocation};
use stack_core::intent::ActiveRecord;
use stack_core::probe::FakeHttp;
use stack_core::provenance::GitCli;
use stack_core::workspace::{ItemRecord, ItemRef};

const YML: &str = r#"
name: ws
processes:
  web-dev:
    command: [npm, run, dev]
stacks:
  app:
    root: ../clones
    work:
      repos:
        api: { remote: "https://github.com/owner/api" }
        web: { remote: "https://github.com/owner/web" }
    groups:
      core: { engine: docker }
    components:
      db:
        group: core
        driver: compose
        file: stack/core.yml
        service: db
        modes: { prebuilt: { image: postgres, tag: "16" } }
      api:
        group: core
        driver: compose
        file: stack/core.yml
        service: api
        modes:
          prebuilt: { image: registry.example/api, tag: main }
          source: { repo: api }
        default: prebuilt
        depends_on: [db]
      web:
        group: core
        driver: process
        command: web-dev
        modes: { dev: { repo: web } }
        depends_on: [api]
"#;

const COMPOSE: &str = "services:\n  db:\n    image: postgres:16\n    expose: [\"5432\"]\n  api:\n    image: registry.example/api:main\n    expose: [\"8080\"]\n";
const NOW: &str = "2026-11-14T12:00:00Z";
const BRANCH: &str = "task/ABC-1-fix-login";

/// git for setting the repos up, isolated from the machine's settings and
/// with fixed dates, so every commit (and so every folder label) is the
/// same on every run.
fn git(dir: &Path, args: &[&str]) -> String {
    let empty = std::env::temp_dir().join("chappa-stack-test-empty-gitconfig");
    let _ = fs::write(&empty, "");
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.autocrlf=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", &empty)
        .env("GIT_AUTHOR_DATE", "2026-11-14T12:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-11-14T12:00:00Z")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn git_available() -> bool {
    let ok = Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !ok {
        eprintln!("skipped: git is not on PATH");
    }
    ok
}

fn commit(dir: &Path, file: &str, text: &str, message: &str) {
    fs::write(dir.join(file), text).unwrap();
    git(dir, &["add", file]);
    git(dir, &["commit", "-q", "-m", message]);
}

struct Ws {
    p: Project,
    f: Fakes,
    src: Arc<Source>,
    clock: Arc<ManualClock>,
    origins: PathBuf,
    clones: PathBuf,
}

impl Ws {
    fn clone_of(&self, repo: &str) -> PathBuf {
        self.clones.join(repo)
    }

    /// A commit pushed to origin's `branch` from a scratch clone, so the
    /// shared clone does not know it yet.
    fn push_branch(&self, repo: &str, branch: &str) {
        let scratch = self
            .origins
            .join(format!("scratch-{repo}-{}", branch.replace('/', "-")));
        git(
            &self.origins,
            &[
                "clone",
                "-q",
                self.origins.join(format!("{repo}.git")).to_str().unwrap(),
                scratch.to_str().unwrap(),
            ],
        );
        git(&scratch, &["checkout", "-q", "-b", branch]);
        commit(
            &scratch,
            "change.txt",
            &format!("{branch}\n"),
            "work on the branch",
        );
        git(&scratch, &["push", "-q", "origin", branch]);
    }

    fn refresh(&self) {
        self.src.work.invalidate();
        self.src.refresh_work();
    }

    fn work(&self) -> Value {
        get(&self.src, "/api/work").1
    }

    fn row(&self, key: &str) -> Value {
        self.work()["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["key"] == key)
            .cloned()
            .unwrap_or_else(|| panic!("no row {key}"))
    }

    fn run(&self, reply: &Value) -> (i32, String) {
        let id = reply["jobId"]
            .as_str()
            .unwrap_or_else(|| panic!("no job: {reply}"));
        let mut pane: Vec<u8> = Vec::new();
        let code = run_job(&self.f.world, &self.p.dir, id, &mut pane);
        (code, String::from_utf8(pane).unwrap())
    }
}

fn setup() -> Option<Ws> {
    if !git_available() {
        return None;
    }
    let p = Project::with(YML, &[("stack/core.yml", COMPOSE)]);
    let root = p.dir.parent().unwrap().to_path_buf();
    let origins = root.join("origins");
    let clones = root.join("clones");
    fs::create_dir_all(&origins).unwrap();
    fs::create_dir_all(&clones).unwrap();
    for repo in ["api", "web"] {
        let bare = origins.join(format!("{repo}.git"));
        git(&origins, &["init", "-q", "--bare", bare.to_str().unwrap()]);
        let clone = clones.join(repo);
        git(
            &clones,
            &[
                "clone",
                "-q",
                bare.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );
        commit(&clone, "README", &format!("{repo}\n"), "start");
        git(&clone, &["push", "-q", "origin", "main"]);
    }
    let cli = with_docker_project(
        docker_facts(FakeCli::new()),
        "app-0-core",
        &[Ctr::running("db"), Ctr::running("api")],
    )
    .on(
        &[
            "docker",
            "manifest",
            "inspect",
            "--verbose",
            "registry.example/api:main",
        ],
        &format!(r#"{{"Descriptor":{{"digest":"{PG_DIGEST}","platform":{{"os":"linux"}}}}}}"#),
    );
    let mut f = fakes(cli, FakeHttp::default());
    f.world.git = Arc::new(GitCli::default());
    let clock = ManualClock::at(NOW);
    f.world.clock = clock.clone();
    let src = source(f.world.clone(), &p.dir);
    let ws = Ws {
        p,
        f,
        src,
        clock,
        origins,
        clones,
    };
    // origin of api has the item's branch, with a commit main lacks.
    ws.push_branch("api", BRANCH);
    Some(ws)
}

fn start_body(key: &str, title: &str, prs: Value, status: Value, confirmed: bool) -> Value {
    json!({
        "id": "start-work",
        "target": {
            "view": "work", "key": key, "source": "tracker",
            "item": { "key": key, "title": title, "url": format!("https://tracker.example.com/{key}"),
                      "status": status, "prs": prs },
        },
        "params": {},
        "confirmed": confirmed,
    })
}

fn act(src: &Source, id: &str, key: Option<&str>, params: Value) -> (u16, Value) {
    post(
        src,
        "/api/actions",
        &json!({ "id": id, "target": { "view": "work", "key": key }, "params": params, "confirmed": true }),
        true,
    )
}

fn action<'a>(row: &'a Value, id: &str) -> Option<&'a Value> {
    row["actions"].as_array()?.iter().find(|a| a["id"] == id)
}

fn api_pr() -> Value {
    json!([
        { "forge": "github", "repo": "owner/api", "number": 7, "sourceBranch": BRANCH },
        { "forge": "github", "repo": "elsewhere/other", "number": 9, "sourceBranch": "x" }
    ])
}

fn doing() -> Value {
    json!({ "label": "In Progress", "category": "doing" })
}

/// ABC-1 started from its tracker row: api's worktree on the PR branch.
fn started(ws: &Ws) -> PathBuf {
    let (status, body) = post(
        &ws.src,
        "/api/actions",
        &start_body("ABC-1", "Fix login", api_pr(), doing(), true),
        true,
    );
    assert_eq!(status, 202, "{body}");
    let (code, log) = ws.run(&body);
    assert_eq!(code, EXIT_DONE, "{log}");
    ws.p.work().join("ABC-1")
}

#[test]
fn start_work_creates_the_folder_and_worktrees_for_pr_branches_only() {
    let Some(ws) = setup() else { return };
    // Unconfirmed and malformed calls create nothing.
    let (status, body) = post(
        &ws.src,
        "/api/actions",
        &start_body("ABC-1", "Fix login", api_pr(), doing(), false),
        true,
    );
    assert_eq!(
        (status, body["kind"].as_str()),
        (409, Some("confirmation-required"))
    );
    let mut bad = start_body("ABC-1", "x", api_pr(), doing(), true);
    bad["target"]["item"]["key"] = "ABC-2".into();
    assert_eq!(post(&ws.src, "/api/actions", &bad, true).0, 400);
    let mut bad = start_body("ABC-1", "x", api_pr(), doing(), true);
    bad["target"]["item"] = "ABC-1".into();
    assert_eq!(post(&ws.src, "/api/actions", &bad, true).0, 400);
    let bad = start_body("../..", "x", json!([]), doing(), true);
    let (status, body) = post(&ws.src, "/api/actions", &bad, true);
    assert_eq!(
        (status, body["kind"].as_str()),
        (400, Some("bad-item")),
        "{body}"
    );
    assert!(
        !ws.p.work().exists()
            || fs::read_dir(ws.p.work())
                .unwrap()
                .flatten()
                .all(|e| e.file_name() == ".gitignore")
    );

    let (status, body) = post(
        &ws.src,
        "/api/actions",
        &start_body("ABC-1", "Fix login", api_pr(), doing(), true),
        true,
    );
    assert_eq!(status, 202, "{body}");
    let dir = ws.p.work().join("ABC-1");
    let rec = ItemRecord::load(&dir)
        .unwrap()
        .expect("item.json is written before the job runs");
    assert_eq!(
        rec.item_ref,
        ItemRef {
            source: "tracker".into(),
            key: "ABC-1".into()
        }
    );
    assert_eq!(rec.title.as_deref(), Some("Fix login"));
    assert_eq!(
        rec.url.as_deref(),
        Some("https://tracker.example.com/ABC-1")
    );
    assert!(rec.repos.is_empty());
    let job = JobRecord::load(&ws.p.work(), body["jobId"].as_str().unwrap())
        .unwrap()
        .unwrap();
    match &job.work {
        JobWork::Workspace {
            op: WorkspaceOp::StartWork {
                checkouts, notes, ..
            },
        } => {
            assert_eq!(checkouts.len(), 1);
            assert_eq!(
                (checkouts[0].repo.as_str(), checkouts[0].branch.as_str()),
                ("api", BRANCH)
            );
            assert_eq!(notes, &["web: no PR of owner/web in this item; skipped"]);
        }
        other => panic!("{other:?}"),
    }
    let (code, log) = ws.run(&body);
    assert_eq!(code, EXIT_DONE, "{log}");
    assert!(
        log.contains("· web: no PR of owner/web in this item; skipped"),
        "{log}"
    );
    let rec = ItemRecord::load(&dir).unwrap().unwrap();
    let api = &rec.repos["api"];
    assert_eq!(api.branch, BRANCH);
    assert_eq!(api.base, "main");
    let head = git(
        &ws.clone_of("api"),
        &["rev-parse", &format!("refs/remotes/origin/{BRANCH}")],
    );
    assert_eq!(api.c8, head.trim()[..8]);
    assert_eq!(api.path, PathBuf::from(format!("src/api-{}", api.c8)));
    let wt = dir.join(&api.path);
    assert_eq!(
        git(&wt, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        BRANCH
    );
    // No branch was created anywhere: origin has exactly what it had.
    let heads = git(
        &ws.origins.join("api.git"),
        &["for-each-ref", "--format=%(refname)", "refs/heads"],
    );
    assert_eq!(
        heads.lines().collect::<Vec<_>>(),
        ["refs/heads/main", &format!("refs/heads/{BRANCH}")]
    );

    // Again: already started, no second job.
    let (status, body) = post(
        &ws.src,
        "/api/actions",
        &start_body("ABC-1", "Fix login", api_pr(), doing(), true),
        true,
    );
    assert_eq!(status, 200, "{body}");
    assert!(body["message"]
        .as_str()
        .unwrap()
        .contains("already started"));
    // Another tracker key that sanitises to the same folder is refused.
    let mut other = start_body("ABC/1", "x", json!([]), doing(), true);
    other["target"]["source"] = "tracker".into();
    let (status, body) = post(&ws.src, "/api/actions", &other, true);
    assert_eq!(
        (status, body["kind"].as_str()),
        (409, Some("conflict")),
        "{body}"
    );

    // An overlay call that carries the owner's row refreshes the cached
    // status, even when the call itself is refused.
    let (status, _) = post(
        &ws.src,
        "/api/actions",
        &json!({
            "id": "activate",
            "target": { "view": "work", "key": "ABC-1", "source": "tracker",
                        "item": { "key": "ABC-1", "status": { "label": "Done", "category": "done" } } },
            "params": { "fingerprint": "0" },
            "confirmed": true,
        }),
        true,
    );
    assert_eq!(status, 409);
    ws.refresh();
    let row = ws.row("ABC-1");
    assert_eq!(row["status"]["category"], "done");
    assert_eq!(
        row["title"], "Fix login",
        "fields the call left out are kept"
    );
    assert!(row["badges"]
        .as_array()
        .unwrap()
        .iter()
        .any(|b| b["text"] == "done in tracker — archive?"));
}

#[test]
fn add_repo_checks_out_an_existing_branch_or_creates_one_from_the_base() {
    let Some(ws) = setup() else { return };
    started(&ws);
    ws.refresh();
    let row = ws.row("ABC-1");
    let add = action(&row, "add-repo:web").expect("web can be added");
    assert_eq!(
        add["confirm"],
        format!("Create branch `{BRANCH}` from `main` in web")
    );
    assert_eq!(
        add["params"],
        json!({ "repo": "web", "expect": "new", "branch": BRANCH })
    );
    assert!(
        action(&row, "add-repo:api").is_none(),
        "api is already in the item"
    );

    // Origin gets the branch between the confirm and the run: the job
    // refuses rather than doing the other thing.
    let (status, body) = act(
        &ws.src,
        "add-repo:web",
        Some("ABC-1"),
        add["params"].clone(),
    );
    assert_eq!(status, 202, "{body}");
    ws.push_branch("web", BRANCH);
    let (code, log) = ws.run(&body);
    assert_eq!(code, EXIT_REFUSED, "{log}");
    assert!(log.contains("conflict (409)"), "{log}");
    assert!(!ItemRecord::load(&ws.p.work().join("ABC-1"))
        .unwrap()
        .unwrap()
        .repos
        .contains_key("web"));

    // Looked at again, the confirm says what will happen now.
    ws.refresh();
    let row = ws.row("ABC-1");
    let add = action(&row, "add-repo:web").unwrap();
    assert_eq!(
        add["confirm"],
        format!("Check out existing branch `{BRANCH}` in web")
    );
    // The stale confirm is refused at once.
    let (status, body) = act(
        &ws.src,
        "add-repo:web",
        Some("ABC-1"),
        json!({ "repo": "web", "expect": "new", "branch": BRANCH }),
    );
    assert_eq!(
        (status, body["kind"].as_str()),
        (409, Some("stale-plan")),
        "{body}"
    );
    let (status, body) = act(
        &ws.src,
        "add-repo:web",
        Some("ABC-1"),
        add["params"].clone(),
    );
    assert_eq!(status, 202, "{body}");
    let (code, log) = ws.run(&body);
    assert_eq!(code, EXIT_DONE, "{log}");
    let rec = ItemRecord::load(&ws.p.work().join("ABC-1"))
        .unwrap()
        .unwrap();
    let wt = ws.p.work().join("ABC-1").join(&rec.repos["web"].path);
    // (The machine's own line-ending setting applies to a checkout.)
    assert_eq!(
        fs::read_to_string(wt.join("change.txt"))
            .unwrap()
            .trim_end(),
        BRANCH
    );

    // A new branch from the base: a second item whose branch origin lacks.
    let (_, body) = post(
        &ws.src,
        "/api/actions",
        &start_body("ABC-2", "Other thing", json!([]), doing(), true),
        true,
    );
    ws.run(&body);
    ws.refresh();
    let row = ws.row("ABC-2");
    let add = action(&row, "add-repo:api").unwrap();
    assert_eq!(
        add["confirm"],
        "Create branch `task/ABC-2-other-thing` from `main` in api"
    );
    let (_, body) = act(
        &ws.src,
        "add-repo:api",
        Some("ABC-2"),
        add["params"].clone(),
    );
    let (code, log) = ws.run(&body);
    assert_eq!(code, EXIT_DONE, "{log}");
    let rec = ItemRecord::load(&ws.p.work().join("ABC-2"))
        .unwrap()
        .unwrap();
    let wt = ws.p.work().join("ABC-2").join(&rec.repos["api"].path);
    assert_eq!(
        git(&wt, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        "task/ABC-2-other-thing"
    );
    let main = git(
        &ws.clone_of("api"),
        &["rev-parse", "refs/remotes/origin/main"],
    );
    assert_eq!(git(&wt, &["rev-parse", "HEAD"]), main);
    assert_eq!(rec.repos["api"].c8, main.trim()[..8]);
}

#[test]
fn activate_binds_only_the_items_repos_and_activating_another_rebinds() {
    let Some(ws) = setup() else { return };
    let dir = started(&ws);
    ws.refresh();
    let row = ws.row("ABC-1");
    assert_eq!(row["active"], false);
    let activate = action(&row, "activate").expect("activate is offered");
    assert_eq!(activate["label"], "Activate in slot 0");
    let confirm = activate["confirm"].as_str().unwrap();
    assert!(
        confirm.starts_with(&format!(
            "Bind slot 0 to ABC-1: build api from {BRANCH}; others unchanged. "
        )),
        "{confirm}"
    );
    assert!(confirm.contains("build api"), "{confirm}");
    assert!(
        confirm.contains("recreate api (mode prebuilt → source)"),
        "{confirm}"
    );
    assert!(!confirm.contains("db"), "db has no repo: {confirm}");
    assert!(
        !confirm.contains("web"),
        "the item has no web worktree: {confirm}"
    );

    // A stale fingerprint is refused; the shown one runs.
    let (status, body) = act(
        &ws.src,
        "activate",
        Some("ABC-1"),
        json!({ "fingerprint": "0" }),
    );
    assert_eq!((status, body["kind"].as_str()), (409, Some("stale-plan")));
    let (status, body) = act(
        &ws.src,
        "activate",
        Some("ABC-1"),
        activate["params"].clone(),
    );
    assert_eq!(status, 202, "{body}");
    let (code, log) = ws.run(&body);
    assert_eq!(code, EXIT_DONE, "{log}");
    assert!(log.contains("api: already on"), "{log}");
    let active = ActiveRecord::load(&ws.p.work()).unwrap();
    assert_eq!(active.get(0).unwrap().item, "ABC-1");
    let intent: Value = serde_json::from_str(
        &fs::read_to_string(ws.p.work().join("instances/app-0.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(intent["item"], "ABC-1");
    assert_eq!(intent["components"]["api"]["mode"], "source");
    assert_eq!(intent["components"]["api"]["ref"], BRANCH);
    assert!(intent["components"]
        .get("db")
        .and_then(|d| d.get("mode"))
        .is_none());
    // The build ran in the item's worktree.
    let wt = dir.join(&ItemRecord::load(&dir).unwrap().unwrap().repos["api"].path);
    let builds = ws.f.cli.with(|c| c.calls_matching(&["docker", "build"]));
    assert_eq!(
        builds.len(),
        0,
        "the content key existed (fake), so the build was reused"
    );
    let cache: Value = serde_json::from_str(
        &fs::read_to_string(ws.p.work().join("instances/app-0/provenance.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        cache["components"]["api"]["commit"].as_str().unwrap(),
        git(&wt, &["rev-parse", "HEAD"]).trim()
    );

    ws.refresh();
    let row = ws.row("ABC-1");
    assert_eq!(row["active"], true);
    assert!(action(&row, "activate").is_none());
    assert!(row["badges"]
        .as_array()
        .unwrap()
        .iter()
        .any(|b| b["text"] == "active in slot 0"));

    // Another item with only web: api goes back to prebuilt, web runs from it.
    let (_, body) = post(
        &ws.src,
        "/api/actions",
        &start_body("ABC-2", "Other thing", json!([]), doing(), true),
        true,
    );
    ws.run(&body);
    ws.refresh();
    let add = action(&ws.row("ABC-2"), "add-repo:web").unwrap().clone();
    let (_, body) = act(
        &ws.src,
        "add-repo:web",
        Some("ABC-2"),
        add["params"].clone(),
    );
    ws.run(&body);
    ws.refresh();
    let row = ws.row("ABC-2");
    let activate = action(&row, "activate").unwrap();
    let confirm = activate["confirm"].as_str().unwrap();
    assert!(
        confirm.starts_with("Saves the stack state of ABC-1 first (if that fails, nothing else runs). Bind slot 0 to ABC-2: run web from task/ABC-2-other-thing, api back to prebuilt; others unchanged. "),
        "{confirm}"
    );
    assert!(
        confirm.contains("recreate api (mode source → prebuilt)"),
        "{confirm}"
    );
    let (_, body) = act(
        &ws.src,
        "activate",
        Some("ABC-2"),
        activate["params"].clone(),
    );
    let (code, log) = ws.run(&body);
    assert_eq!(code, EXIT_DONE, "{log}");
    assert_eq!(
        ActiveRecord::load(&ws.p.work())
            .unwrap()
            .get(0)
            .unwrap()
            .item,
        "ABC-2"
    );
    // The slot left ABC-1: its stack state was saved first.
    let saved = stack_core::states::list_states(&dir);
    assert_eq!(saved.len(), 1, "{log}");
    assert_eq!(
        saved[0].reason,
        stack_core::states::SaveReason::BeforeRebind
    );
    assert_eq!(
        saved[0].components["api"].mode,
        Some(stack_core::definition::Mode::Source)
    );
    ws.refresh();
    assert_eq!(ws.row("ABC-1")["active"], false);
    assert_eq!(ws.row("ABC-2")["active"], true);
}

#[test]
fn drift_shows_once_the_branch_moves_and_keep_as_built_dismisses_it() {
    let Some(ws) = setup() else { return };
    let dir = started(&ws);
    ws.refresh();
    let activate = action(&ws.row("ABC-1"), "activate").unwrap().clone();
    let (_, body) = act(
        &ws.src,
        "activate",
        Some("ABC-1"),
        activate["params"].clone(),
    );
    let (code, log) = ws.run(&body);
    assert_eq!(code, EXIT_DONE, "{log}");
    ws.refresh();
    assert_eq!(
        ws.row("ABC-1")["banners"],
        json!([]),
        "built from HEAD: no drift"
    );

    let wt = dir.join(&ItemRecord::load(&dir).unwrap().unwrap().repos["api"].path);
    let built = git(&wt, &["rev-parse", "HEAD"]).trim().to_string();
    commit(&wt, "more.txt", "more\n", "more work");
    let head = git(&wt, &["rev-parse", "HEAD"]).trim().to_string();
    // The cached facts are still fresh: nothing yet, and nothing waited.
    ws.src.refresh_work();
    assert_eq!(ws.row("ABC-1")["banners"], json!([]));
    ws.clock.advance(Duration::from_secs(31));
    ws.src.refresh_work();
    // The banner is the item's own; the view has none of its own.
    let work = ws.work();
    assert_eq!(work["banners"], json!([]));
    let banner = &work["items"][0]["banners"][0];
    assert_eq!(banner["id"], "drift-ABC-1");
    assert_eq!(banner["tone"], "warn");
    let line = format!(
        "api was built from {}; {BRANCH} is now at {}",
        &built[..8],
        &head[..8]
    );
    assert_eq!(banner["lines"], json!([line]));
    let rebuild = banner["actions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == "rebuild-changed:ABC-1")
        .unwrap();
    assert_eq!(rebuild["mutates"], true);
    let confirm = rebuild["confirm"].as_str().unwrap();
    assert!(
        confirm.starts_with(&format!(
            "Rebuild api from {BRANCH} at {} (built from {}). ",
            &head[..8],
            &built[..8]
        )),
        "{confirm}"
    );
    assert!(confirm.contains("recreate api (rebuilt)"), "{confirm}");
    let keep = banner["actions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == "keep-as-built:ABC-1")
        .unwrap();
    assert_eq!(keep["mutates"], false);
    let row = ws.row("ABC-1");
    assert!(row["badges"]
        .as_array()
        .unwrap()
        .iter()
        .any(|b| b["text"] == "built from older commits"));
    // The Environment view says it too.
    let (_, env) = get(&ws.src, "/api/environment");
    let alert = env["alerts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["componentId"] == "api")
        .unwrap();
    assert_eq!(alert["severity"], "warn");
    assert_eq!(alert["text"], format!("{line} (ABC-1)"));

    // Rebuild changed is a planned job.
    let (status, body) = post(
        &ws.src,
        "/api/actions",
        &json!({ "id": "rebuild-changed:ABC-1", "target": { "view": "work", "key": null }, "params": rebuild["params"], "confirmed": true }),
        true,
    );
    assert_eq!(status, 202, "{body}");
    // Keep as built: no confirm, done at once, gone until HEAD moves again.
    let (status, body) = post(
        &ws.src,
        "/api/actions",
        &json!({ "id": "keep-as-built:ABC-1", "target": { "view": "work", "key": null }, "params": keep["params"] }),
        true,
    );
    assert_eq!(status, 200, "{body}");
    ws.refresh();
    assert_eq!(ws.row("ABC-1")["banners"], json!([]));
    commit(&wt, "even-more.txt", "x\n", "even more");
    ws.refresh();
    assert_eq!(ws.row("ABC-1")["banners"][0]["id"], "drift-ABC-1");
}

/// git that takes seconds per call, like one on a slow disk or network.
struct SlowGit(Duration);

impl EngineCli for SlowGit {
    fn run(&self, _: &Invocation) -> Result<CliOutput, CliError> {
        std::thread::sleep(self.0);
        Ok(CliOutput::success(""))
    }
}

#[test]
fn the_work_list_never_waits_on_git() {
    let p = Project::with(YML, &[("stack/core.yml", COMPOSE)]);
    let mut f = fakes(
        with_docker_project(docker_facts(FakeCli::new()), "app-0-core", &[]),
        FakeHttp::default(),
    );
    f.world.git_cli = Arc::new(SlowGit(Duration::from_secs(3)));
    let src = source(f.world.clone(), &p.dir);
    let dir = p.work().join("ABC-1");
    ItemRecord::new(
        ItemRef {
            source: "tracker".into(),
            key: "ABC-1".into(),
        },
        "ABC-1",
        NOW,
    )
    .save(&dir)
    .unwrap();
    let refresher = {
        let src = src.clone();
        std::thread::spawn(move || src.refresh_work())
    };
    let t = Instant::now();
    let (status, work) = get(&src, "/api/work");
    assert!(
        t.elapsed() < Duration::from_secs(2),
        "the list took {:?}",
        t.elapsed()
    );
    assert_eq!(status, 200);
    assert_eq!(work["items"][0]["badges"][0]["text"], "checking git…");
    let t = Instant::now();
    let _ = get(&src, "/api/environment");
    assert!(
        t.elapsed() < Duration::from_secs(2),
        "status took {:?}",
        t.elapsed()
    );
    refresher.join().unwrap();
}

#[test]
fn archive_is_refused_while_dirty_or_unpushed_and_restore_brings_it_back() {
    let Some(ws) = setup() else { return };
    let dir = started(&ws);
    let rec = ItemRecord::load(&dir).unwrap().unwrap();
    let wt = dir.join(&rec.repos["api"].path);
    fs::write(wt.join("scratch.txt"), "wip\n").unwrap();
    let (status, body) = act(&ws.src, "archive", Some("ABC-1"), json!({}));
    assert_eq!(
        (status, body["kind"].as_str()),
        (409, Some("dirty")),
        "{body}"
    );
    assert_eq!(
        body["message"],
        "not archived, commit and push first: api: 1 uncommitted change"
    );
    git(&wt, &["add", "scratch.txt"]);
    git(&wt, &["commit", "-q", "-m", "wip"]);
    let (status, body) = act(&ws.src, "archive", Some("ABC-1"), json!({}));
    assert_eq!(status, 409);
    assert_eq!(
        body["message"],
        "not archived, commit and push first: api: 1 unpushed commit"
    );
    git(&wt, &["push", "-q", "origin", BRANCH]);

    // Make it the active item first: archiving clears that.
    let mut active = ActiveRecord::load(&ws.p.work()).unwrap();
    active.set(0, "ABC-1", "app-0", NOW);
    active.save(&ws.p.work()).unwrap();
    ws.refresh();
    let archive = action(&ws.row("ABC-1"), "archive").unwrap().clone();
    // It is the item the slot runs: its stack state is saved first, and
    // the twin goes ahead without saving.
    assert!(action(&ws.row("ABC-1"), "archive-no-save").is_some());
    assert_eq!(
        archive["confirm"],
        "Saves the stack state of ABC-1 first (if that fails, nothing else runs). Archive ABC-1: remove the worktrees of api (refused if any has uncommitted or unpushed work) and move the folder to _archive/ABC-1."
    );
    let (status, body) = act(&ws.src, "archive", Some("ABC-1"), json!({}));
    assert_eq!(status, 202, "{body}");
    let (code, log) = ws.run(&body);
    assert_eq!(code, EXIT_DONE, "{log}");
    assert!(!dir.exists());
    assert!(!wt.exists());
    let archived = ws.p.work().join("_archive/ABC-1");
    assert!(ItemRecord::load(&archived)
        .unwrap()
        .unwrap()
        .archived_utc
        .is_some());
    // The automatic save went with the item into the archive.
    let saved = stack_core::states::list_states(&archived);
    assert_eq!(saved.len(), 1, "{log}");
    assert_eq!(
        saved[0].reason,
        stack_core::states::SaveReason::BeforeArchive
    );
    assert!(ActiveRecord::load(&ws.p.work()).unwrap().get(0).is_none());
    let list = git(&ws.clone_of("api"), &["worktree", "list", "--porcelain"]);
    assert_eq!(list.matches("worktree ").count(), 1, "{list}");

    // Listed only in the Archived panel.
    ws.refresh();
    let work = ws.work();
    assert!(work["items"].as_array().unwrap().is_empty());
    let banner = work["banners"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["id"] == "archived")
        .unwrap();
    assert_eq!(banner["title"], "1 archived workspace");
    assert_eq!(
        banner["actions"][0],
        json!({ "kind": "panel", "id": "archived", "label": "Archived", "endpoint": "/api/work/archived" })
    );
    let (_, panel) = get(&ws.src, "/api/work/archived");
    let restore = &panel["sections"][0]["actions"][0];
    assert_eq!(restore["id"], "restore:ABC-1");
    assert_eq!(
        restore["confirm"],
        format!("Restore ABC-1: recreate the worktrees of api (`{BRANCH}`) and move it back to the Work list.")
    );
    let (status, body) = post(
        &ws.src,
        "/api/actions",
        &json!({ "id": "restore:ABC-1", "target": { "view": "panel:/api/work/archived", "key": null }, "params": restore["params"], "confirmed": true }),
        true,
    );
    assert_eq!(status, 202, "{body}");
    let (code, log) = ws.run(&body);
    assert_eq!(code, EXIT_DONE, "{log}");
    assert_eq!(
        git(&wt, &["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
        BRANCH
    );
    assert!(wt.join("scratch.txt").exists(), "the pushed commit is back");
    assert!(ItemRecord::load(&dir)
        .unwrap()
        .unwrap()
        .archived_utc
        .is_none());
    ws.refresh();
    assert_eq!(ws.work()["items"].as_array().unwrap().len(), 1);
}

#[test]
fn the_overlay_payload() {
    let Some(ws) = setup() else { return };
    started(&ws);
    ws.refresh();
    let add = action(&ws.row("ABC-1"), "add-repo:web").unwrap().clone();
    let (_, body) = act(
        &ws.src,
        "add-repo:web",
        Some("ABC-1"),
        add["params"].clone(),
    );
    assert_eq!(ws.run(&body).0, EXIT_DONE);
    ws.refresh();
    let activate = action(&ws.row("ABC-1"), "activate").unwrap().clone();
    let (_, body) = act(
        &ws.src,
        "activate",
        Some("ABC-1"),
        activate["params"].clone(),
    );
    assert_eq!(ws.run(&body).0, EXIT_DONE);
    let (_, body) = post(
        &ws.src,
        "/api/actions",
        &start_body("ABC-2", "Other thing", json!([]), doing(), true),
        true,
    );
    ws.run(&body);
    let done = json!({ "label": "Done", "category": "done" });
    let (_, body) = post(
        &ws.src,
        "/api/actions",
        &start_body("ABC-3", "Finished thing", json!([]), done, true),
        true,
    );
    ws.run(&body);
    // One uncommitted file in ABC-1's web worktree, and a commit in its
    // api worktree after api was built (drift, on ABC-1's own entry).
    let rec = ItemRecord::load(&ws.p.work().join("ABC-1"))
        .unwrap()
        .unwrap();
    commit(
        &ws.p.work().join("ABC-1").join(&rec.repos["api"].path),
        "more.txt",
        "more
",
        "more work",
    );
    fs::write(
        ws.p.work()
            .join("ABC-1")
            .join(&rec.repos["web"].path)
            .join("notes.txt"),
        "x\n",
    )
    .unwrap();
    ws.refresh();

    let mut work = ws.work();
    work["freshness"]["asOf"] = "<now>".into();
    mask_fingerprints(&mut work);
    scrub(&mut work, &ws.p.dir);
    let snap = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots/work.json");
    let text = serde_json::to_string_pretty(&work).unwrap() + "\n";
    if std::env::var("UPDATE_SNAPSHOTS").as_deref() == Ok("1") {
        fs::write(&snap, &text).unwrap();
    }
    let want = fs::read_to_string(&snap)
        .expect("snapshot missing; run with UPDATE_SNAPSHOTS=1")
        .replace("\r\n", "\n");
    assert_eq!(
        text, want,
        "the payload moved; rerun with UPDATE_SNAPSHOTS=1 and review the diff"
    );

    let items = work["items"].as_array().unwrap();
    assert_eq!(items.len(), 3);
    let one = &items[0];
    assert_eq!(one["ref"], json!({ "source": "tracker", "key": "ABC-1" }));
    assert_eq!(one["active"], true);
    assert_eq!(
        one["branches"],
        json!([
            { "forge": "github", "repo": "owner/api", "branch": BRANCH },
            { "forge": "github", "repo": "owner/web", "branch": BRANCH }
        ])
    );
    assert_eq!(one["folder"], "<project>/.work/ABC-1");
    assert_eq!(one["banners"][0]["id"], "drift-ABC-1");
    assert_eq!(work["banners"], json!([]), "no view-level drift banner");
    assert!(
        one.get("localStatus").is_none(),
        "no workflow, no status block"
    );
    assert!(items[2]["badges"]
        .as_array()
        .unwrap()
        .iter()
        .any(|b| b["text"] == "done in tracker — archive?"));
}

fn mask_fingerprints(v: &mut Value) {
    match v {
        Value::Object(o) => {
            if o.contains_key("fingerprint") {
                o.insert("fingerprint".into(), "<fingerprint>".into());
            }
            o.values_mut().for_each(mask_fingerprints);
        }
        Value::Array(a) => a.iter_mut().for_each(mask_fingerprints),
        _ => {}
    }
}

#[test]
fn the_manifest_offers_start_work_on_every_work_row_or_the_listed_sources() {
    let p = Project::with(YML, &[("stack/core.yml", COMPOSE)]);
    let f = fakes(FakeCli::new(), FakeHttp::default());
    let src = source(f.world.clone(), &p.dir);
    let (_, m) = get(&src, "/chappa/v1/manifest");
    assert_eq!(
        m,
        json!({
            "contract": 1,
            "source": { "id": "stack:app", "label": "Stack app", "version": env!("CARGO_PKG_VERSION") },
            "views": {
                "environment": { "list": "/api/environment", "pollSeconds": 15 },
                "work": { "list": "/api/work", "pollSeconds": 30 },
            },
            "itemActions": [{
                "kind": "endpoint",
                "id": "start-work",
                "label": "Start work",
                "mutates": true,
                "confirm": "Create a workspace for {key} — {title}",
                "on": { "view": "work", "sources": ["*"] },
            }, {
                "kind": "endpoint",
                "id": "review-env",
                "label": "Review with environment",
                "mutates": true,
                "confirm": "Check out {key} at its head into a review worktree",
                "on": { "view": "prs", "sources": ["*"] },
            }, {
                "kind": "endpoint",
                "id": "review-env-up",
                "label": "Review with environment in slot 0",
                "mutates": true,
                "confirm": "Check out {key} at its head into a review worktree and bring up slot 0 from it",
                "on": { "view": "prs", "sources": ["*"] },
            }],
            "ownerUpdates": "/api/work/owner",
            "actions": "/api/actions",
            "jobs": "/api/jobs/{id}",
            "linkHosts": ["127.0.0.1:*", "localhost:*"],
        })
    );
    let narrowed = YML.replace("    work:\n", "    work:\n      sources: [tracker]\n");
    let p = Project::with(&narrowed, &[("stack/core.yml", COMPOSE)]);
    let src = source(f.world.clone(), &p.dir);
    let (_, m) = get(&src, "/chappa/v1/manifest");
    assert_eq!(m["itemActions"][0]["on"]["sources"], json!(["tracker"]));
    // Rows of another source are refused.
    let mut body = start_body("ABC-1", "x", json!([]), doing(), true);
    body["target"]["source"] = "other".into();
    let (status, reply) = post(&src, "/api/actions", &body, true);
    assert_eq!((status, reply["kind"].as_str()), (409, Some("not-offered")));
}

#[test]
fn building_from_a_shared_clone_is_refused_unless_it_is_allowed() {
    let p = Project::with(YML, &[("stack/core.yml", COMPOSE)]);
    let f = fakes(
        with_docker_project(
            docker_facts(FakeCli::new()),
            "app-0-core",
            &[Ctr::running("db"), Ctr::running("api")],
        ),
        FakeHttp::default(),
    );
    let src = source(f.world.clone(), &p.dir);
    let (_, env) = get(&src, "/api/environment");
    let to_source = env["components"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "api")
        .unwrap()["actions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == "mode-source")
        .unwrap()
        .clone();
    let call = json!({ "id": "mode-source", "target": { "view": "environment", "key": "api" }, "params": to_source["params"], "confirmed": true });
    let (status, body) = post(&src, "/api/actions", &call, true);
    assert_eq!(
        (status, body["kind"].as_str()),
        (409, Some("shared-checkout")),
        "{body}"
    );
    let message = body["message"].as_str().unwrap();
    assert!(message.contains("shared clone `api`"), "{message}");
    assert!(message.contains("shared-checkouts-allow.txt"), "{message}");
    fs::write(
        p.dir.join("shared-checkouts-allow.txt"),
        "# demo builds\napi\n",
    )
    .unwrap();
    let (status, body) = post(&src, "/api/actions", &call, true);
    assert_eq!(status, 202, "{body}");
}
