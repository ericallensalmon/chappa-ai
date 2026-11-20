//! PR review workspaces end to end, with real git against temporary
//! repos: a github PR (fetched by its pull ref) and an azure-devops one
//! (fetched by its source branch) checked out under `.work/reviews/`, the
//! panel and the Environment section, bringing a slot up from a review,
//! removal (refused while dirty) and the removal a merged PR triggers.

mod common;

use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chappa_stack::records::{JobRecord, JobWork};
use chappa_stack::runjob::{run_job, EXIT_DONE};
use chappa_stack::server::Source;
use chappa_stack::world::ManualClock;
use common::*;
use stack_core::cli::FakeCli;
use stack_core::intent::ActiveRecord;
use stack_core::probe::FakeHttp;
use stack_core::provenance::GitCli;

const NOW: &str = "2026-11-14T12:00:00Z";

const YML: &str = r#"
name: reviews
processes:
  web-dev:
    command: [npm, run, dev]
stacks:
  app:
    root: ../clones
    work:
      repos:
        api: { remote: "https://github.com/owner/api" }
        web: { remote: "https://dev.azure.com/org/project/_git/web" }
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

struct Rv {
    p: Project,
    f: Fakes,
    src: Arc<Source>,
    /// The commit `refs/pull/12/head` of api points at.
    pr12: String,
    /// The commit `feature/x` of web points at.
    pr7: String,
}

/// Push a commit to `refspec` of `bare` from a scratch clone; its sha.
fn push_change(bare: &Path, scratch: &Path, file: &str, refspec: &str) -> String {
    test_git(
        bare.parent().unwrap(),
        &[
            "clone",
            "-q",
            bare.to_str().unwrap(),
            scratch.to_str().unwrap(),
        ],
    );
    commit_file(scratch, file, "a change under review\n", "change");
    test_git(
        scratch,
        &["push", "-q", "origin", &format!("HEAD:{refspec}")],
    );
    test_git(scratch, &["rev-parse", "HEAD"]).trim().to_string()
}

fn setup() -> Option<Rv> {
    if !has_git() {
        return None;
    }
    let p = Project::with(YML, &[("stack/core.yml", COMPOSE)]);
    let (api_bare, _) = origin_and_clone(&p.dir, "api");
    let (web_bare, _) = origin_and_clone(&p.dir, "web");
    let root = p.dir.parent().unwrap().to_path_buf();
    let pr12 = push_change(
        &api_bare,
        &root.join("scratch-api"),
        "fix.txt",
        "refs/pull/12/head",
    );
    let pr7 = push_change(
        &web_bare,
        &root.join("scratch-web"),
        "page.txt",
        "refs/heads/feature/x",
    );
    let cli = with_docker_project(
        docker_facts(FakeCli::new()),
        "app-0-core",
        &[Ctr::running("db"), Ctr::running("api")],
    );
    let mut f = fakes(cli, FakeHttp::default());
    f.world.git = Arc::new(GitCli::default());
    f.world.clock = ManualClock::at(NOW);
    let src = source(f.world.clone(), &p.dir);
    Some(Rv {
        p,
        f,
        src,
        pr12,
        pr7,
    })
}

fn pr(forge: &str, repo: &str, number: Value, branch: &str, state: &str, role: &str) -> Value {
    json!({
        "forge": forge, "repo": repo, "number": number, "title": "Fix it",
        "url": format!("https://example.com/{repo}/pull/1"),
        "sourceBranch": branch, "targetBranch": "main", "state": state, "role": role,
        "author": { "name": "Someone" }, "bucket": "vote", "reasons": [],
    })
}

fn review_call(id: &str, item: Value, confirmed: bool) -> Value {
    json!({
        "id": id,
        "target": { "view": "prs", "key": format!("{}:{}:{}", item["forge"].as_str().unwrap(), item["repo"].as_str().unwrap(), item["number"]), "source": "forge", "item": item },
        "params": {},
        "confirmed": confirmed,
    })
}

impl Rv {
    fn run(&self, reply: &Value) -> (i32, String) {
        let id = reply["jobId"]
            .as_str()
            .unwrap_or_else(|| panic!("no job: {reply}"));
        let mut pane = Vec::new();
        let code = run_job(&self.f.world, &self.p.dir, id, &mut pane);
        (code, String::from_utf8(pane).unwrap())
    }

    fn reviews(&self) -> PathBuf {
        self.p.work().join("reviews")
    }

    fn create(&self, item: Value) -> (i32, String) {
        let (status, reply) = post(
            &self.src,
            "/api/actions",
            &review_call("review-env", item, true),
            true,
        );
        assert_eq!(status, 202, "{reply}");
        self.run(&reply)
    }
}

fn head(dir: &Path) -> String {
    test_git(dir, &["rev-parse", "HEAD"]).trim().to_string()
}

fn find<'a>(list: &'a Value, id: &str) -> Option<&'a Value> {
    list.as_array()?.iter().find(|a| a["id"] == id)
}

#[test]
fn a_github_pr_is_checked_out_at_its_pull_head_and_shown_in_the_panel() {
    let Some(rv) = setup() else { return };
    let item = pr(
        "github",
        "owner/api",
        json!("12"),
        "feature/y",
        "open",
        "reviewing",
    );
    // Unconfirmed: nothing happens.
    let (status, body) = post(
        &rv.src,
        "/api/actions",
        &review_call("review-env", item.clone(), false),
        true,
    );
    assert_eq!(
        (status, body["kind"].as_str()),
        (409, Some("confirmation-required"))
    );
    // What follows the checkout is one of the offered choices.
    let mut call = review_call("review-env", item.clone(), true);
    call["params"] = json!({ "then": "sideways" });
    let (status, body) = post(&rv.src, "/api/actions", &call, true);
    assert_eq!((status, body["kind"].as_str()), (400, Some("bad-input")));
    let (code, pane) = rv.create(item.clone());
    assert_eq!(code, EXIT_DONE, "{pane}");
    let wt = rv.reviews().join("github-owner-api-12");
    assert_eq!(head(&wt), rv.pr12);
    assert!(
        pane.contains(&format!("✓ checked out owner/api#12 at {}", &rv.pr12[..8])),
        "{pane}"
    );
    let rec: Value = serde_json::from_str(
        &fs::read_to_string(rv.reviews().join("github-owner-api-12.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(rec["clone"], "api");
    assert_eq!(rec["head"], rv.pr12.as_str());
    assert_eq!(rec["fetched"], "refs/remotes/origin/pr/12");
    // The record sits beside the worktree, not in it.
    assert!(!wt.join("github-owner-api-12.json").exists());
    assert!(rv.reviews().join("github-owner-api-12.json").is_file());
    // Again: already there.
    let (code, pane) = rv.create(item);
    assert_eq!(code, EXIT_DONE, "{pane}");
    assert!(pane.contains("· already at"), "{pane}");

    let (status, panel) = get(&rv.src, "/api/work/reviews");
    assert_eq!(status, 200);
    assert_eq!(panel["title"], "Review workspaces");
    let s = &panel["sections"][0];
    assert_eq!(s["id"], "github-owner-api-12");
    assert_eq!(s["title"], "owner/api#12 — Fix it");
    assert_eq!(
        s["rows"][0]["value"],
        format!("{} · refs/remotes/origin/pr/12", &rv.pr12[..8])
    );
    assert_eq!(
        s["rows"][0]["links"][0]["url"],
        "https://example.com/owner/api/pull/1"
    );
    let terminal = find(&s["actions"], "review-terminal:github-owner-api-12").unwrap();
    assert_eq!(terminal["kind"], "terminal");
    assert_eq!(Path::new(terminal["cwd"].as_str().unwrap()), wt);
    let agent = find(&s["actions"], "review-agent:github-owner-api-12").unwrap();
    assert_eq!(agent["kind"], "agent");
    let prompt = agent["prompt"].as_str().unwrap();
    assert!(
        prompt.starts_with("Review the pull request named below."),
        "{prompt}"
    );
    assert!(
        prompt.ends_with(&format!(
            "\n{}\nowner/api#12 — Fix it\n{}",
            chappa_stack::reviews::PR_TEXT_OPEN,
            chappa_stack::reviews::PR_TEXT_CLOSE
        )),
        "{prompt}"
    );
    let up = find(&s["actions"], "review-up:github-owner-api-12").unwrap();
    assert!(
        up["confirm"].as_str().unwrap().starts_with(&format!(
            "Bring up slot 0 from the review worktree of owner/api#12 at {}: build api from it; others unchanged.",
            &rv.pr12[..8]
        )),
        "{up}"
    );
    assert!(find(&s["actions"], "review-remove:github-owner-api-12").is_some());

    // The Environment names it and opens the panel.
    let env = get(&rv.src, "/api/environment").1;
    let sec = env["facts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "reviews")
        .unwrap();
    assert_eq!(sec["rows"][0]["label"], "owner/api#12");
    assert_eq!(sec["actions"][0]["endpoint"], "/api/work/reviews");

    // Bring the slot up from it: a plan bound to the review worktree.
    let (status, reply) = post(
        &rv.src,
        "/api/actions",
        &json!({ "id": "review-up:github-owner-api-12", "target": { "view": "panel:/api/work/reviews", "key": null },
                 "params": up["params"], "confirmed": true }),
        true,
    );
    assert_eq!(status, 202, "{reply}");
    let rec = JobRecord::load(&rv.p.work(), reply["jobId"].as_str().unwrap())
        .unwrap()
        .unwrap();
    assert!(matches!(rec.work, JobWork::Plan { save: None, .. }));
    let (code, pane) = rv.run(&reply);
    assert_eq!(code, EXIT_DONE, "{pane}");
    let active = ActiveRecord::load(&gate(&rv.p.dir)).unwrap();
    assert_eq!(active.get(0).unwrap().item, "review:github-owner-api-12");
    let cache: Value = serde_json::from_str(
        &fs::read_to_string(rv.p.work().join("instances/app-0/provenance.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(cache["components"]["api"]["mode"], "source");
    assert_eq!(cache["components"]["api"]["commit"], rv.pr12.as_str());
    // Bound to it, the panel no longer offers bringing it up again.
    let (_, panel) = get(&rv.src, "/api/work/reviews");
    assert!(find(
        &panel["sections"][0]["actions"],
        "review-up:github-owner-api-12"
    )
    .is_none());
    assert_eq!(
        panel["sections"][0]["rows"][3]["value"],
        "slot 0 runs from it"
    );
}

#[test]
fn an_azure_devops_pr_is_checked_out_from_its_source_branch() {
    let Some(rv) = setup() else { return };
    let item = pr(
        "azure-devops",
        "org/project/web",
        json!(7),
        "refs/heads/feature/x",
        "open",
        "reviewing",
    );
    let (code, pane) = rv.create(item);
    assert_eq!(code, EXIT_DONE, "{pane}");
    let wt = rv.reviews().join("azure-devops-org-project-web-7");
    assert_eq!(head(&wt), rv.pr7);
    let rec: Value = serde_json::from_str(
        &fs::read_to_string(rv.reviews().join("azure-devops-org-project-web-7.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(rec["clone"], "web");
    assert_eq!(rec["fetched"], "refs/remotes/origin/feature/x");
    // A PR of a repo the stack has no clone of is refused.
    let other = pr(
        "github",
        "elsewhere/other",
        json!(3),
        "x",
        "open",
        "reviewing",
    );
    let (status, body) = post(
        &rv.src,
        "/api/actions",
        &review_call("review-env", other, true),
        true,
    );
    assert_eq!(
        (status, body["kind"].as_str()),
        (409, Some("not-offered")),
        "{body}"
    );
    // A malformed number never reaches a path or git.
    let bad = pr(
        "github",
        "owner/api",
        json!("12/../../x"),
        "x",
        "open",
        "reviewing",
    );
    let (status, _) = post(
        &rv.src,
        "/api/actions",
        &review_call("review-env", bad, true),
        true,
    );
    assert_eq!(status, 400);
}

#[test]
fn a_review_is_removed_by_hand_when_clean_and_when_its_pr_is_merged() {
    let Some(rv) = setup() else { return };
    let item = pr(
        "github",
        "owner/api",
        json!("12"),
        "feature/y",
        "open",
        "reviewing",
    );
    let (code, pane) = rv.create(item.clone());
    assert_eq!(code, EXIT_DONE, "{pane}");
    let wt = rv.reviews().join("github-owner-api-12");
    let remove = |src: &Source| {
        post(
            src,
            "/api/actions",
            &json!({ "id": "review-remove:github-owner-api-12", "target": { "view": "panel:/api/work/reviews", "key": null },
                     "params": { "review": "github-owner-api-12" }, "confirmed": true }),
            true,
        )
    };
    fs::write(wt.join("notes.txt"), "work in progress\n").unwrap();
    let (status, body) = remove(&rv.src);
    assert_eq!(
        (status, body["kind"].as_str()),
        (409, Some("dirty")),
        "{body}"
    );
    fs::remove_file(wt.join("notes.txt")).unwrap();
    let (status, reply) = remove(&rv.src);
    assert_eq!(status, 202, "{reply}");
    let (code, pane) = rv.run(&reply);
    assert_eq!(code, EXIT_DONE, "{pane}");
    assert!(!wt.exists());
    assert!(!rv.reviews().join("github-owner-api-12.json").exists());
    let (_, panel) = get(&rv.src, "/api/work/reviews");
    assert_eq!(panel["sections"], json!([]));

    // Made again; then the PR arrives merged: the clean workspace goes.
    rv.create(item);
    assert!(wt.exists());
    let merged = pr(
        "github",
        "owner/api",
        json!("12"),
        "feature/y",
        "merged",
        "reviewing",
    );
    let (status, body) = post(
        &rv.src,
        "/api/actions",
        &review_call("review-env", merged.clone(), true),
        true,
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["message"],
        "owner/api#12 is merged; its review workspace was removed"
    );
    assert!(!wt.exists());
    let (status, body) = post(
        &rv.src,
        "/api/actions",
        &review_call("review-env", merged, true),
        true,
    );
    assert_eq!(
        (status, body["kind"].as_str()),
        (409, Some("not-offered")),
        "{body}"
    );
}
