//! Shared fakes for the integration tests: a project in a temp folder, a
//! scripted engine, git, HTTP and process host, and a [`World`] made of
//! them.
#![allow(dead_code)]

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chappa_stack::identity::Token;
use chappa_stack::project::ProjectConfig;
use chappa_stack::server::Source;
use chappa_stack::world::{CliSteps, Locked, ProcessHost, SystemClock, ThreadSpawner, World};
use serde_json::Value;
use stack_core::checks::FakePorts;
use stack_core::cli::{CliRunner, FakeCli};
use stack_core::compose::FakeGateways;
use stack_core::jobs::RunBlock;
use stack_core::probe::FakeHttp;
use stack_core::provenance::{GitError, GitFacts};

pub const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";
pub const PG_DIGEST: &str =
    "sha256:1111111111111111111111111111111111111111111111111111111111111111";
pub const TOKEN: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

pub fn example_source() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../stack/examples/local")
}

pub fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            if entry.file_name() == ".work" {
                continue;
            }
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// The example stack copied into a temp folder.
pub struct Project {
    pub _tmp: tempfile::TempDir,
    pub dir: PathBuf,
}

impl Project {
    pub fn example() -> Project {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("local");
        copy_dir(&example_source(), &dir);
        Project { _tmp: tmp, dir }
    }

    /// A project from a `chappa.yml` text (plus compose files by name).
    pub fn with(yml: &str, files: &[(&str, &str)]) -> Project {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("proj");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("chappa.yml"), yml).unwrap();
        fs::write(dir.join(".gitignore"), ".work/\n.chappa/stack-state/\n").unwrap();
        for (name, text) in files {
            let p = dir.join(name);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, text).unwrap();
        }
        Project { _tmp: tmp, dir }
    }

    pub fn work(&self) -> PathBuf {
        self.dir.join(".work")
    }
}

#[derive(Default)]
pub struct FakeGit;

impl GitFacts for FakeGit {
    fn head_commit(&self, _: &Path) -> Result<String, GitError> {
        Ok(HEAD.into())
    }
    fn current_branch(&self, _: &Path) -> Result<Option<String>, GitError> {
        Ok(Some("main".into()))
    }
    fn count_between(&self, _: &Path, _: &str, _: &str) -> Result<u32, GitError> {
        Ok(0)
    }
    fn dirty_hash(&self, _: &Path) -> Result<Option<String>, GitError> {
        Ok(None)
    }
    fn commit_exists(&self, _: &Path, _: &str) -> Result<bool, GitError> {
        Ok(true)
    }
}

/// Processes that "run" until stopped: spawn hands out pids, `alive` says
/// whether a pid was spawned and not stopped or killed by the test.
#[derive(Default)]
pub struct FakeProcs {
    next: AtomicU32,
    pub alive: Mutex<BTreeSet<u32>>,
    pub spawned: Mutex<Vec<RunBlock>>,
    pub stopped: Mutex<Vec<u32>>,
}

impl FakeProcs {
    pub fn kill(&self, pid: u32) {
        self.alive.lock().unwrap().remove(&pid);
    }
    pub fn set_alive(&self, pid: u32) {
        self.alive.lock().unwrap().insert(pid);
    }
}

impl ProcessHost for FakeProcs {
    fn spawn(&self, run: &RunBlock, log: &Path) -> Result<u32, String> {
        let pid = 4000 + self.next.fetch_add(1, Ordering::SeqCst);
        fs::create_dir_all(log.parent().unwrap()).unwrap();
        fs::write(log, "listening\n").unwrap();
        self.alive.lock().unwrap().insert(pid);
        self.spawned.lock().unwrap().push(run.clone());
        Ok(pid)
    }
    fn alive(&self, pid: u32) -> bool {
        self.alive.lock().unwrap().contains(&pid)
    }
    fn stop(&self, pid: u32, _grace: Duration) -> Result<(), String> {
        self.alive.lock().unwrap().remove(&pid);
        self.stopped.lock().unwrap().push(pid);
        Ok(())
    }
}

pub struct Fakes {
    pub cli: Arc<Locked<FakeCli>>,
    pub http: Arc<Locked<FakeHttp>>,
    pub procs: Arc<FakeProcs>,
    pub world: World,
}

/// A world of fakes around `cli` and `http`.
pub fn fakes(cli: FakeCli, http: FakeHttp) -> Fakes {
    let cli = Locked::new(cli);
    let http = Locked::new(http);
    let procs = Arc::new(FakeProcs::default());
    let world = World {
        cli: cli.clone(),
        registry: cli.clone(),
        git: Locked::new(FakeGit),
        git_cli: Arc::new(CliRunner),
        http: http.clone(),
        gateways: Locked::new(FakeGateways::default()),
        ports: Locked::new(FakePorts::default()),
        hosts_text: Some("127.0.0.1 localhost dev.example.test\n".into()),
        steps: Arc::new(CliSteps(cli.clone())),
        procs: procs.clone(),
        spawner: Arc::new(ThreadSpawner),
        exe: PathBuf::from("/opt/chappa/chappa-stack"),
        settle: Duration::ZERO,
        clock: Arc::new(SystemClock),
        hook_retry: Duration::ZERO,
    };
    Fakes {
        cli,
        http,
        procs,
        world,
    }
}

pub fn source(world: World, dir: &Path) -> Arc<Source> {
    Source::new(
        world,
        ProjectConfig::new(dir),
        Token::parse(TOKEN).unwrap(),
        "127.0.0.1:5000".into(),
    )
}

pub fn bearer() -> String {
    format!("Bearer {TOKEN}")
}

/// GET through the handler with the token.
pub fn get(src: &Source, path: &str) -> (u16, Value) {
    src.handle("GET", path, Some(&bearer()), false, "")
}

pub fn post(src: &Source, path: &str, body: &Value, runs_jobs: bool) -> (u16, Value) {
    src.handle("POST", path, Some(&bearer()), runs_jobs, &body.to_string())
}

// ------------------------------------------------------------ containers

/// One container as `compose ps` and `inspect` show it.
#[derive(Clone)]
pub struct Ctr {
    pub service: &'static str,
    pub state: &'static str,
    pub health: &'static str,
    pub exit: i32,
    pub restarts: u32,
}

impl Ctr {
    pub fn running(service: &'static str) -> Self {
        Ctr {
            service,
            state: "running",
            health: "",
            exit: 0,
            restarts: 0,
        }
    }
    pub fn exited(service: &'static str, exit: i32) -> Self {
        Ctr {
            state: "exited",
            exit,
            ..Ctr::running(service)
        }
    }
    pub fn health(mut self, h: &'static str) -> Self {
        self.health = h;
        self
    }
    pub fn restarts(mut self, n: u32) -> Self {
        self.restarts = n;
        self
    }
    pub fn state(mut self, s: &'static str) -> Self {
        self.state = s;
        self
    }
    pub fn name(&self, project: &str) -> String {
        format!("{project}-{}-1", self.service)
    }
    pub fn ps(&self, project: &str) -> Value {
        serde_json::json!({
            "Name": self.name(project),
            "Service": self.service,
            "State": self.state,
            "Health": self.health,
            "ExitCode": self.exit,
        })
    }
    pub fn inspect(&self, project: &str) -> Value {
        let health = if self.health.is_empty() {
            Value::Null
        } else {
            serde_json::json!({ "Status": self.health })
        };
        serde_json::json!({
            "Name": format!("/{}", self.name(project)),
            "RestartCount": self.restarts,
            "Image": "sha256:feedface",
            "State": {
                "Status": self.state,
                "Running": self.state == "running",
                "Restarting": self.state == "restarting",
                "ExitCode": self.exit,
                "StartedAt": "2026-11-13T10:00:00.5Z",
                "Health": health,
            },
            "Config": { "Image": "", "Labels": {} },
            "HostConfig": { "ExtraHosts": null },
        })
    }
}

/// `docker compose -p <project> ps` / `docker inspect` answering with
/// `ctrs`, added to `cli`.
pub fn with_docker_project(cli: FakeCli, project: &str, ctrs: &[Ctr]) -> FakeCli {
    let ps: Vec<String> = ctrs.iter().map(|c| c.ps(project).to_string()).collect();
    let inspect: Vec<Value> = ctrs.iter().map(|c| c.inspect(project)).collect();
    cli.on(&["docker", "compose", "-p", project, "ps"], &ps.join("\n"))
        .on(
            &["docker", "inspect", "--type", "container"],
            &Value::from(inspect).to_string(),
        )
}

/// A docker engine that answers `version` and `info`.
pub fn docker_facts(cli: FakeCli) -> FakeCli {
    cli.on(
        &["docker", "version", "--format", "json"],
        r#"{"Client":{"Version":"29.0.0"},"Server":{"Version":"29.0.0"}}"#,
    )
    .on(
        &["docker", "info", "--format", "json"],
        r#"{"OSType":"linux","ServerVersion":"29.0.0","MemTotal":17179869184,"ServerErrors":[]}"#,
    )
}

/// Replace the project folder (and backslashes) in every string, so a
/// payload compares the same on every machine.
pub fn scrub(v: &mut Value, dir: &Path) {
    let d = dir.display().to_string();
    let d_fwd = d.replace('\\', "/");
    match v {
        Value::String(s) => {
            let t = s.replace(&d, "<project>").replace('\\', "/");
            *s = t.replace(&d_fwd, "<project>");
        }
        Value::Array(a) => a.iter_mut().for_each(|x| scrub(x, dir)),
        Value::Object(o) => o.values_mut().for_each(|x| scrub(x, dir)),
        _ => {}
    }
}

/// Wait (up to 10 s) until `f` holds.
pub fn eventually(mut f: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

// ------------------------------------------------------------------- git

/// git for setting test repos up, isolated from the machine's settings and
/// with fixed dates, so commits come out the same on every run.
pub fn test_git(dir: &Path, args: &[&str]) -> String {
    let empty = std::env::temp_dir().join("chappa-stack-test-empty-gitconfig");
    let _ = fs::write(&empty, "");
    let out = std::process::Command::new("git")
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

/// Whether git is on PATH (tests that need it skip with a line otherwise).
pub fn has_git() -> bool {
    let ok = std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !ok {
        eprintln!("skipped: git is not on PATH");
    }
    ok
}

/// Write `file` and commit it.
pub fn commit_file(dir: &Path, file: &str, text: &str, message: &str) {
    fs::write(dir.join(file), text).unwrap();
    test_git(dir, &["add", file]);
    test_git(dir, &["commit", "-q", "-m", message]);
}

/// A bare `origins/<repo>.git` and its shared clone `clones/<repo>` beside
/// `beside` (the stack project), with one commit on `main` pushed.
pub fn origin_and_clone(beside: &Path, repo: &str) -> (PathBuf, PathBuf) {
    let root = beside.parent().unwrap();
    let origins = root.join("origins");
    let clones = root.join("clones");
    fs::create_dir_all(&origins).unwrap();
    fs::create_dir_all(&clones).unwrap();
    let bare = origins.join(format!("{repo}.git"));
    test_git(&origins, &["init", "-q", "--bare", bare.to_str().unwrap()]);
    let clone = clones.join(repo);
    test_git(
        &clones,
        &[
            "clone",
            "-q",
            bare.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    commit_file(&clone, "README", &format!("{repo}\n"), "start");
    test_git(&clone, &["push", "-q", "origin", "main"]);
    (bare, clone)
}
