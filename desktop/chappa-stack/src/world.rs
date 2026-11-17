//! Everything outside this process the runner talks to, as one bundle of
//! shareable trait objects: the engine CLI, git, HTTP probes, the gateway
//! resolver, the port probe, how a job's long steps run, how process
//! components are started and stopped, and how a headless job is spawned.
//!
//! The server answers requests on several threads and refreshes caches on
//! another, so every tool here is `Send + Sync`. Tests swap any of them for
//! a fake ([`Locked`] makes stack-core's single-threaded fakes shareable).

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use stack_core::checks::{PortProbe, StdPortProbe};
use stack_core::cli::{CliError, CliOutput, CliRunner, EngineCli, Invocation};
use stack_core::compose::{CliGatewayResolver, Gateway, GatewayResolver};
use stack_core::definition::{ContainerOs, EngineRef};
use stack_core::intent::format_utc;
use stack_core::jobs::RunBlock;
use stack_core::probe::{ProbeHttp, ProbeResponse, StdHttp};
use stack_core::provenance::{GitCli, GitError, GitFacts};
use stack_core::safety::secrets::config::ProjectScanConfig;

pub type Shared<T> = Arc<T>;

/// The tools a server, a job and the background refresher share.
#[derive(Clone)]
pub struct World {
    /// Quick engine questions (`ps`, `inspect`, `image inspect`).
    pub cli: Shared<dyn EngineCli + Send + Sync>,
    /// Registry questions (`manifest inspect`) asked off the status path.
    pub registry: Shared<dyn EngineCli + Send + Sync>,
    pub git: Shared<dyn GitFacts + Send + Sync>,
    /// `git` for item workspaces (worktrees, fetches, `ls-remote`), argv
    /// only, each call under its own time limit.
    pub git_cli: Shared<dyn EngineCli + Send + Sync>,
    pub http: Shared<dyn ProbeHttp + Send + Sync>,
    pub gateways: Shared<dyn GatewayResolver + Send + Sync>,
    pub ports: Shared<dyn PortProbe + Send + Sync>,
    /// Hosts-file text to check instead of the machine's (tests).
    pub hosts_text: Option<String>,
    /// How a job's engine commands run.
    pub steps: Shared<dyn StepExec + Send + Sync>,
    /// How process components are started, watched and stopped.
    pub procs: Shared<dyn ProcessHost + Send + Sync>,
    /// How a job is started when the caller cannot host a pane.
    pub spawner: Shared<dyn JobSpawner + Send + Sync>,
    /// The program a pane runs for `run-job`, `follow` and `follow-command`
    /// (`chappa-stack`; see [`runner_exe`]).
    pub exe: PathBuf,
    /// How long a freshly started process component must stay alive to
    /// count as started.
    pub settle: Duration,
    /// What time it is: data-safety scan times and the `every:` schedule.
    pub clock: Shared<dyn Clock + Send + Sync>,
    /// The pause between attempts of a data-safety hook run right after
    /// up, while a database may still be starting.
    pub hook_retry: Duration,
    /// How status and plan scripts run (the background pool's runs).
    pub scripts: Shared<dyn EngineCli + Send + Sync>,
    /// The scripts [`World::scripts`] has in flight, so shutdown can end
    /// them instead of leaving them to their own time limits.
    pub running_scripts: Shared<RunningScripts>,
    /// The secret scanner: gitleaks on `PATH` for real, a fake in tests.
    pub secrets: Shared<dyn crate::secret_scan::ScannerSource + Send + Sync>,
    /// The gitleaks config the app's Settings name, when they name one. The
    /// real scanner uses it, and every job is started with it
    /// ([`World::job_argv`]) so a job's scans use the same rules.
    pub gitleaks_config: Option<PathBuf>,
    /// The project's own gitleaks config (`chappa.yml` `secretScan.config`)
    /// with the hash the person trusted, as the app hands it over. Every
    /// scan re-reads and re-hashes the file and uses it only while it
    /// matches; otherwise the Settings config or the default, with a banner
    /// (see [`stack_core::safety::secrets::config`]). Jobs get it too.
    pub project_scan_config: ProjectScanConfig,
}

/// The argument that carries [`World::gitleaks_config`] on `serve` and
/// `run-job`.
pub const GITLEAKS_CONFIG_ARG: &str = "--gitleaks-config";
/// The arguments that carry [`World::project_scan_config`]: its path, and
/// its trusted hash (`none` while it is not trusted).
pub const PROJECT_GITLEAKS_CONFIG_ARG: &str = "--project-gitleaks-config";
pub const PROJECT_GITLEAKS_CONFIG_HASH_ARG: &str = "--project-gitleaks-config-hash";

/// The default for [`World::settle`].
pub const PROCESS_SETTLE: Duration = Duration::from_millis(1500);
/// The default for [`World::hook_retry`].
pub const HOOK_RETRY: Duration = Duration::from_secs(3);

/// The program's own file name.
const RUNNER_NAME: &str = "chappa-stack";
/// The `chappa-stack` program that panes and detached jobs run (`run-job`,
/// `follow`, `follow-command`): this process when it is `chappa-stack` (the
/// app launches the runner as its own process, the usual case), else a
/// `chappa-stack` program in the same folder as the running one (the runner
/// embedded in another host shipped beside it), else the bare name, found on
/// `PATH` by whatever starts it. Never another program: a command built for
/// a pane must not start a test harness or an embedding host in its place.
/// No folder above is searched, so a `chappa-stack` planted higher up the
/// tree is never what runs. (Tests set [`World::exe`] themselves.)
pub fn runner_exe(current: Option<&Path>) -> PathBuf {
    let file = format!("{RUNNER_NAME}{}", std::env::consts::EXE_SUFFIX);
    let Some(current) = current else {
        return PathBuf::from(RUNNER_NAME);
    };
    let is_runner = current
        .file_stem()
        .is_some_and(|s| s.to_string_lossy().eq_ignore_ascii_case(RUNNER_NAME));
    if is_runner {
        return current.to_path_buf();
    }
    current
        .parent()
        .map(|dir| dir.join(&file))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from(RUNNER_NAME))
}

impl World {
    /// The real machine: engines, git and HTTP for real.
    pub fn real() -> World {
        let cli: Shared<dyn EngineCli + Send + Sync> = Arc::new(CliRunner);
        let running_scripts = Arc::new(RunningScripts::default());
        World {
            scripts: Arc::new(ScriptCli {
                running: running_scripts.clone(),
            }),
            running_scripts,
            cli: cli.clone(),
            registry: cli.clone(),
            git: Arc::new(GitCli::default()),
            git_cli: Arc::new(CliRunner),
            http: Arc::new(StdHttp),
            gateways: Arc::new(CachedGateways::new(CliGateways(cli))),
            ports: Arc::new(StdPortProbe),
            hosts_text: None,
            steps: Arc::new(ChildSteps),
            procs: Arc::new(OsProcesses),
            spawner: Arc::new(DetachedSpawner),
            exe: runner_exe(std::env::current_exe().ok().as_deref()),
            settle: PROCESS_SETTLE,
            clock: Arc::new(SystemClock),
            hook_retry: HOOK_RETRY,
            secrets: Arc::new(crate::secret_scan::GitleaksOnPath::default()),
            gitleaks_config: None,
            project_scan_config: ProjectScanConfig::None,
        }
    }

    /// The real world scanning with `config` (the app's Settings value) in
    /// place of the built-in default.
    pub fn with_gitleaks_config(mut self, config: Option<PathBuf>) -> World {
        self.gitleaks_config = config;
        self.secrets = Arc::new(crate::secret_scan::GitleaksOnPath {
            config: self.gitleaks_config.clone(),
            project: self.project_scan_config.clone(),
        });
        self
    }

    /// The real world scanning with the project's own config first, while
    /// it is the trusted one (see [`World::project_scan_config`]).
    pub fn with_project_scan_config(mut self, project: ProjectScanConfig) -> World {
        self.project_scan_config = project;
        self.secrets = Arc::new(crate::secret_scan::GitleaksOnPath {
            config: self.gitleaks_config.clone(),
            project: self.project_scan_config.clone(),
        });
        self
    }

    /// `run-job <id>` as this executable runs it, carrying the gitleaks
    /// configs on, so a job scans with the rules the source was started with.
    pub fn job_argv(&self, job_id: &str) -> Vec<String> {
        let mut argv = vec![
            self.exe.display().to_string(),
            "run-job".to_string(),
            job_id.to_string(),
        ];
        if let Some(c) = &self.gitleaks_config {
            argv.push(GITLEAKS_CONFIG_ARG.to_string());
            argv.push(c.display().to_string());
        }
        if let Some((path, hash)) = self.project_scan_config.args() {
            argv.push(PROJECT_GITLEAKS_CONFIG_ARG.to_string());
            argv.push(path.display().to_string());
            argv.push(PROJECT_GITLEAKS_CONFIG_HASH_ARG.to_string());
            argv.push(hash.to_string());
        }
        argv
    }
}

/// The gateway rule through a shared CLI.
pub struct CliGateways(pub Shared<dyn EngineCli + Send + Sync>);

impl GatewayResolver for CliGateways {
    fn gateway(&self, engine: &EngineRef, os: Option<ContainerOs>) -> Result<Gateway, String> {
        CliGatewayResolver { cli: &*self.0 }.gateway(engine, os)
    }
}

/// How long a resolved gateway is reused. Asking costs an engine call
/// (`docker info`, `network inspect`, `machine ssh`) that can take seconds,
/// and a gateway only moves when an engine restarts; a job runs in its own
/// process and so always asks afresh.
pub const GATEWAY_TTL: Duration = Duration::from_secs(60);
/// A failed lookup is retried sooner.
pub const GATEWAY_FAILURE_TTL: Duration = Duration::from_secs(10);

type GatewayAnswer = (Instant, Result<Gateway, String>);

/// A [`GatewayResolver`] that remembers answers per engine for a while, so
/// a status poll does not ask every engine for its gateway each time.
pub struct CachedGateways<G> {
    inner: G,
    answers: Mutex<std::collections::HashMap<String, GatewayAnswer>>,
}

impl<G> CachedGateways<G> {
    pub fn new(inner: G) -> Self {
        Self {
            inner,
            answers: Mutex::new(std::collections::HashMap::new()),
        }
    }
}

impl<G: GatewayResolver> GatewayResolver for CachedGateways<G> {
    fn gateway(&self, engine: &EngineRef, os: Option<ContainerOs>) -> Result<Gateway, String> {
        let now = Instant::now();
        if let Some((at, answer)) = self
            .answers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&engine.name)
        {
            let ttl = if answer.is_ok() {
                GATEWAY_TTL
            } else {
                GATEWAY_FAILURE_TTL
            };
            if now.duration_since(*at) < ttl {
                return answer.clone();
            }
        }
        let answer = self.inner.gateway(engine, os);
        self.answers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(engine.name.clone(), (now, answer.clone()));
        answer
    }
}

/// A single-threaded tool behind a mutex, so stack-core's fakes (which
/// record calls in a `RefCell`) can be shared across threads.
pub struct Locked<T>(pub Mutex<T>);

impl<T> Locked<T> {
    pub fn new(inner: T) -> Arc<Self> {
        Arc::new(Locked(Mutex::new(inner)))
    }

    pub fn with<R>(&self, f: impl FnOnce(&T) -> R) -> R {
        f(&self.0.lock().unwrap_or_else(|p| p.into_inner()))
    }
}

impl<T: EngineCli> EngineCli for Locked<T> {
    fn run(&self, inv: &Invocation) -> Result<CliOutput, CliError> {
        self.with(|c| c.run(inv))
    }
}

impl<T: ProbeHttp> ProbeHttp for Locked<T> {
    fn get(&self, url: &str, timeout: Duration) -> ProbeResponse {
        self.with(|h| h.get(url, timeout))
    }
}

impl<T: GatewayResolver> GatewayResolver for Locked<T> {
    fn gateway(&self, engine: &EngineRef, os: Option<ContainerOs>) -> Result<Gateway, String> {
        self.with(|g| g.gateway(engine, os))
    }
}

impl<T: PortProbe> PortProbe for Locked<T> {
    fn is_free(&self, port: u16) -> bool {
        self.with(|p| p.is_free(port))
    }
    fn release_grace(&self) -> Duration {
        self.with(|p| p.release_grace())
    }
    fn wait(&self, step: Duration) {
        self.with(|p| p.wait(step))
    }
}

impl<T: GitFacts> GitFacts for Locked<T> {
    fn head_commit(&self, repo: &Path) -> Result<String, GitError> {
        self.with(|g| g.head_commit(repo))
    }
    fn current_branch(&self, repo: &Path) -> Result<Option<String>, GitError> {
        self.with(|g| g.current_branch(repo))
    }
    fn count_between(&self, repo: &Path, from: &str, to: &str) -> Result<u32, GitError> {
        self.with(|g| g.count_between(repo, from, to))
    }
    fn dirty_hash(&self, repo: &Path) -> Result<Option<String>, GitError> {
        self.with(|g| g.dirty_hash(repo))
    }
    fn commit_exists(&self, repo: &Path, sha: &str) -> Result<bool, GitError> {
        self.with(|g| g.commit_exists(repo, sha))
    }
}

// ----------------------------------------------------------------- clock

/// The time, as UTC text (`2026-11-14T12:00:00Z`) and as epoch seconds.
pub trait Clock {
    fn now_secs(&self) -> u64;
    fn now_utc(&self) -> String {
        format_utc(self.now_secs())
    }
}

/// The machine's clock.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_secs(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }
}

/// A clock that moves only when told to (tests of the schedule).
pub struct ManualClock(pub std::sync::atomic::AtomicU64);

impl ManualClock {
    pub fn at(utc: &str) -> Arc<ManualClock> {
        Arc::new(ManualClock(std::sync::atomic::AtomicU64::new(
            stack_core::compose::parse_utc(utc).unwrap_or(0),
        )))
    }

    pub fn advance(&self, by: Duration) {
        self.0
            .fetch_add(by.as_secs(), std::sync::atomic::Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_secs(&self) -> u64 {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

// ------------------------------------------------------------- step exec

/// Runs one engine command of a job, sending its output, line by line, to
/// `out`, and returns its exit code. Errors are a command that could not
/// start or ran out of time.
pub trait StepExec {
    fn run(
        &self,
        run: &RunBlock,
        limit: Duration,
        out: &mut dyn FnMut(&str),
    ) -> Result<i32, String>;

    /// [`StepExec::run`] for a command that may start children of its own
    /// (a project's script): when it runs out of time, its whole process
    /// tree is stopped, not just the first process.
    fn run_tree(
        &self,
        run: &RunBlock,
        limit: Duration,
        out: &mut dyn FnMut(&str),
    ) -> Result<i32, String> {
        self.run(run, limit, out)
    }
}

/// The real thing: a child process with both pipes streamed as they come.
pub struct ChildSteps;

impl StepExec for ChildSteps {
    fn run(
        &self,
        run: &RunBlock,
        limit: Duration,
        out: &mut dyn FnMut(&str),
    ) -> Result<i32, String> {
        run_child(run, limit, out, false)
    }

    fn run_tree(
        &self,
        run: &RunBlock,
        limit: Duration,
        out: &mut dyn FnMut(&str),
    ) -> Result<i32, String> {
        run_child(run, limit, out, true)
    }
}

/// The status and plan scripts in flight, by process id.
#[derive(Debug, Default)]
pub struct RunningScripts(Mutex<HashSet<u32>>);

impl RunningScripts {
    fn add(&self, pid: u32) {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).insert(pid);
    }

    fn remove(&self, pid: u32) {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&pid);
    }

    /// The scripts running now.
    pub fn pids(&self) -> Vec<u32> {
        let mut v: Vec<u32> = self
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .copied()
            .collect();
        v.sort_unstable();
        v
    }

    /// Stop every script in flight and everything each one started (on
    /// shutdown). The ids stopped.
    pub fn kill_all(&self) -> Vec<u32> {
        let pids = self.pids();
        for pid in &pids {
            kill_tree(*pid);
        }
        pids
    }
}

/// Runs status and plan scripts: like stack-core's `CliRunner` (argv only,
/// captured, capped, under the invocation's time limit), but each script
/// leads its own process group on Unix, is listed in [`RunningScripts`]
/// while it runs, and is stopped with every process it started when it runs
/// out of time or the source shuts down.
pub struct ScriptCli {
    pub running: Arc<RunningScripts>,
}

/// Most stdout and stderr kept from one script run.
const SCRIPT_STDOUT_CAP: usize = 8 * 1024 * 1024;
const SCRIPT_STDERR_CAP: usize = 1024 * 1024;

fn read_capped(mut pipe: impl Read, cap: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    while let Ok(n) = pipe.read(&mut buf) {
        if n == 0 {
            break;
        }
        if out.len() < cap {
            let room = cap - out.len();
            out.extend_from_slice(&buf[..n.min(room)]);
        }
    }
    out
}

impl EngineCli for ScriptCli {
    fn run(&self, inv: &Invocation) -> Result<CliOutput, CliError> {
        let mut cmd = Command::new(&inv.program);
        cmd.args(&inv.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        stack_core::child_env::scrub(&mut cmd);
        for (k, v) in &inv.env {
            if !stack_core::child_env::SCRUBBED.contains(&k.as_str()) {
                cmd.env(k, v);
            }
        }
        if let Some(cwd) = &inv.cwd {
            cmd.current_dir(cwd);
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let mut child = cmd.spawn().map_err(|e| CliError::Spawn {
            program: inv.program.clone(),
            message: e.to_string(),
        })?;
        let pid = child.id();
        self.running.add(pid);
        let (out_tx, out_rx) = std::sync::mpsc::channel();
        let stdout = child.stdout.take().expect("stdout is piped");
        thread::spawn(move || {
            let _ = out_tx.send(read_capped(stdout, SCRIPT_STDOUT_CAP));
        });
        let (err_tx, err_rx) = std::sync::mpsc::channel();
        let stderr = child.stderr.take().expect("stderr is piped");
        thread::spawn(move || {
            let _ = err_tx.send(read_capped(stderr, SCRIPT_STDERR_CAP));
        });
        let deadline = Instant::now() + Duration::from_secs(inv.timeout_secs);
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(e) => {
                    kill_tree(pid);
                    let _ = child.kill();
                    self.running.remove(pid);
                    return Err(CliError::Spawn {
                        program: inv.program.clone(),
                        message: format!("waiting: {e}"),
                    });
                }
            }
            if Instant::now() >= deadline {
                kill_tree(pid);
                let _ = child.kill();
                let _ = child.wait();
                self.running.remove(pid);
                return Err(CliError::Timeout {
                    command: inv.to_string(),
                    seconds: inv.timeout_secs,
                });
            }
            thread::sleep(Duration::from_millis(15));
        };
        self.running.remove(pid);
        let grace = Duration::from_secs(2);
        let stdout = out_rx.recv_timeout(grace).unwrap_or_default();
        let stderr = err_rx.recv_timeout(grace).unwrap_or_default();
        Ok(CliOutput {
            status: status.code(),
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        })
    }
}

/// Stop `pid` and everything it started: `taskkill /T` on Windows, the
/// process group (which the child leads) elsewhere.
fn kill_tree(pid: u32) {
    #[cfg(windows)]
    let mut cmd = {
        let mut c = Command::new("taskkill");
        c.args(["/T", "/F", "/PID", &pid.to_string()]);
        c
    };
    #[cfg(not(windows))]
    let mut cmd = {
        let mut c = Command::new("kill");
        c.args(["-KILL", &format!("-{pid}")]);
        c
    };
    let _ = stack_core::child_env::scrub(&mut cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Run one command with both pipes streamed to `out`, under `limit`. With
/// `tree`, the command leads its own process group (Unix) and a timeout
/// stops all of its descendants.
fn run_child(
    run: &RunBlock,
    limit: Duration,
    out: &mut dyn FnMut(&str),
    tree: bool,
) -> Result<i32, String> {
    let mut cmd = command_in(&run.argv, Some(&run.cwd))?;
    cmd.current_dir(&run.cwd)
        .envs(child_env(&run.env))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    if tree {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("cannot start `{}`: {e}", run.argv[0]))?;
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let readers: Vec<_> = [
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .flatten()
    .map(|pipe| {
        let tx = tx.clone();
        thread::spawn(move || {
            let mut reader = BufReader::new(pipe);
            let mut buf = Vec::new();
            while let Ok(n) = reader.read_until(b'\n', &mut buf) {
                if n == 0 {
                    break;
                }
                let line = String::from_utf8_lossy(&buf);
                let _ = tx.send(line.trim_end_matches(['\r', '\n']).to_string());
                buf.clear();
            }
        })
    })
    .collect();
    drop(tx);
    let deadline = Instant::now() + limit;
    let status = loop {
        while let Ok(line) = rx.try_recv() {
            out(&line);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => return Err(format!("cannot wait for `{}`: {e}", run.argv[0])),
        }
        if Instant::now() >= deadline {
            if tree {
                kill_tree(child.id());
            }
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "timed out after {}; {} was stopped",
                crate::timeouts::describe(limit),
                if tree {
                    "it and every process it started"
                } else {
                    "the command"
                }
            ));
        }
        thread::sleep(Duration::from_millis(50));
    };
    // A process the command left running may still hold its pipes:
    // read what is there, and stop waiting after a short grace.
    let grace = Instant::now() + Duration::from_secs(2);
    while readers.iter().any(|r| !r.is_finished()) && Instant::now() < grace {
        while let Ok(line) = rx.try_recv() {
            out(&line);
        }
        thread::sleep(Duration::from_millis(20));
    }
    while let Ok(line) = rx.try_recv() {
        out(&line);
    }
    Ok(status.code().unwrap_or(-1))
}

/// Engine commands through an [`EngineCli`] (captured, not streamed): what
/// tests run jobs against, with a scripted fake.
pub struct CliSteps(pub Shared<dyn EngineCli + Send + Sync>);

impl StepExec for CliSteps {
    fn run(
        &self,
        run: &RunBlock,
        limit: Duration,
        out: &mut dyn FnMut(&str),
    ) -> Result<i32, String> {
        let mut inv = Invocation::from_argv(&run.argv)
            .cwd(&run.cwd)
            .timeout(limit);
        for (k, v) in &run.env {
            inv = inv.env(k, v);
        }
        let output = self.0.run(&inv).map_err(|e| e.to_string())?;
        for line in output.stdout.lines().chain(output.stderr.lines()) {
            out(line);
        }
        Ok(output.status.unwrap_or(-1))
    }
}

/// A run block's env without the variables no child may get (the
/// source's token and address; see [`stack_core::child_env`]).
fn child_env(env: &indexmap::IndexMap<String, String>) -> Vec<(&String, &String)> {
    env.iter()
        .filter(|(k, _)| !stack_core::child_env::SCRUBBED.contains(&k.as_str()))
        .collect()
}

/// A [`Command`] for argv. On Windows a bare program name is looked up on
/// `PATH` with the usual extensions, because `npm` or `npx` are `.cmd`
/// scripts that `CreateProcess` will not find by name; a script runs
/// through `cmd /C`, and then no argument may carry a character `cmd`
/// would interpret (the same rule a pane's batch spawn uses,
/// [`term_core::pty::batch_unsafe_char`]). A PowerShell file is refused: it
/// only runs through an interpreter the argv names. The command never
/// passes the source's token or address on.
pub fn command_for(argv: &[String]) -> Result<Command, String> {
    command_in(argv, None)
}

/// [`command_for`] for a command that runs in `cwd`: a relative program
/// path with a folder in it is taken from there.
pub fn command_in(argv: &[String], cwd: Option<&Path>) -> Result<Command, String> {
    let mut cmd = command_unscrubbed(argv, cwd)?;
    stack_core::child_env::scrub(&mut cmd);
    Ok(cmd)
}

fn command_unscrubbed(argv: &[String], cwd: Option<&Path>) -> Result<Command, String> {
    let program = argv.first().ok_or("the command is empty")?;
    if stack_core::script::needs_interpreter(argv) {
        return Err(format!(
            "`{program}` is a PowerShell script, which does not run on its own; name the interpreter first, as in [powershell, -NoProfile, -File, {program}]"
        ));
    }
    let has_folder = program.contains('/') || program.contains('\\');
    let program_path = match cwd {
        Some(dir) if has_folder && Path::new(program).is_relative() => dir.join(program),
        _ => PathBuf::from(program),
    };
    #[cfg(windows)]
    {
        let batch = program_path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat"));
        let found = find_on_path(program).or_else(|| batch.then(|| program_path.clone()));
        if let Some(found) = found {
            let ext = found
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            if ext == "cmd" || ext == "bat" {
                if let Some((bad, c)) = argv[1..]
                    .iter()
                    .find_map(|a| term_core::pty::batch_unsafe_char(a).map(|c| (a, c)))
                {
                    return Err(format!(
                        "`{program}` is a batch script and argument `{}` has {c:?}, a character the command shell would interpret",
                        stack_core::credentials::mask_text(bad).escape_debug()
                    ));
                }
                let mut cmd = Command::new("cmd");
                cmd.arg("/C").arg(&found).args(&argv[1..]);
                return Ok(cmd);
            }
            let mut cmd = Command::new(found);
            cmd.args(&argv[1..]);
            return Ok(cmd);
        }
    }
    let mut cmd = Command::new(program_path);
    cmd.args(&argv[1..]);
    Ok(cmd)
}

#[cfg(windows)]
fn find_on_path(program: &str) -> Option<PathBuf> {
    let p = Path::new(program);
    if p.extension().is_some() || p.components().count() > 1 {
        return None;
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for ext in ["exe", "com", "cmd", "bat"] {
            let candidate = dir.join(format!("{program}.{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

// -------------------------------------------------------------- processes

/// Process components: the dev servers and `process`-driver commands a
/// job starts. They outlive the job that started them, so they run
/// detached with their output in a log file, and the stack keeps their
/// pids in its process table.
pub trait ProcessHost {
    /// Start `run` detached, output appended to `log`; the pid.
    fn spawn(&self, run: &RunBlock, log: &Path) -> Result<u32, String>;
    fn alive(&self, pid: u32) -> bool;
    /// Ask the process tree to end, then force it after `grace`.
    fn stop(&self, pid: u32, grace: Duration) -> Result<(), String>;
}

pub struct OsProcesses;

impl ProcessHost for OsProcesses {
    fn spawn(&self, run: &RunBlock, log: &Path) -> Result<u32, String> {
        if let Some(parent) = log.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .map_err(|e| format!("cannot open {}: {e}", log.display()))?;
        let err = file.try_clone().map_err(|e| e.to_string())?;
        let mut cmd = command_for(&run.argv)?;
        cmd.current_dir(&run.cwd)
            .envs(child_env(&run.env))
            .stdin(Stdio::null())
            .stdout(file)
            .stderr(err);
        detach(&mut cmd);
        let child =
            spawn_detached(cmd).map_err(|e| format!("cannot start `{}`: {e}", run.argv[0]))?;
        Ok(child.id())
    }

    fn alive(&self, pid: u32) -> bool {
        pid_alive(pid)
    }

    fn stop(&self, pid: u32, grace: Duration) -> Result<(), String> {
        stop_tree(pid, grace)
    }
}

/// Start a job or a process so that it survives this process and the
/// pane that started it.
#[cfg(windows)]
fn detach(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB);
}

#[cfg(unix)]
fn detach(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    // Its own process group, so a stop can signal the whole tree.
    cmd.process_group(0);
}

#[cfg(windows)]
fn spawn_detached(mut cmd: Command) -> io::Result<Child> {
    use std::os::windows::process::CommandExt;
    match cmd.spawn() {
        Ok(c) => Ok(c),
        // A job object that forbids breaking away refuses the flag; the
        // process then stays in that job, which is the best on offer.
        Err(e) if e.raw_os_error() == Some(5) => {
            const DETACHED_PROCESS: u32 = 0x0000_0008;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
            cmd.spawn()
        }
        Err(e) => Err(e),
    }
}

#[cfg(unix)]
fn spawn_detached(mut cmd: Command) -> io::Result<Child> {
    cmd.spawn()
}

fn quiet(program: &str, args: &[&str]) -> Option<std::process::Output> {
    stack_core::child_env::scrub(&mut Command::new(program))
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
}

#[cfg(windows)]
pub fn pid_alive(pid: u32) -> bool {
    quiet(
        "tasklist",
        &["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"],
    )
    .is_some_and(|o| String::from_utf8_lossy(&o.stdout).contains(&format!("\"{pid}\"")))
}

#[cfg(unix)]
pub fn pid_alive(pid: u32) -> bool {
    quiet("kill", &["-0", &pid.to_string()]).is_some_and(|o| o.status.success())
}

#[cfg(windows)]
fn stop_tree(pid: u32, grace: Duration) -> Result<(), String> {
    let id = pid.to_string();
    let _ = quiet("taskkill", &["/PID", &id, "/T"]);
    if wait_gone(pid, grace) {
        return Ok(());
    }
    let _ = quiet("taskkill", &["/PID", &id, "/T", "/F"]);
    if wait_gone(pid, Duration::from_secs(5)) {
        Ok(())
    } else {
        Err(format!("process {pid} did not end"))
    }
}

#[cfg(unix)]
fn stop_tree(pid: u32, grace: Duration) -> Result<(), String> {
    let group = format!("-{pid}");
    let _ = quiet("kill", &["-TERM", "--", &group]);
    if wait_gone(pid, grace) {
        return Ok(());
    }
    let _ = quiet("kill", &["-KILL", "--", &group]);
    if wait_gone(pid, Duration::from_secs(5)) {
        Ok(())
    } else {
        Err(format!("process {pid} did not end"))
    }
}

fn wait_gone(pid: u32, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    loop {
        if !pid_alive(pid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(200));
    }
}

// --------------------------------------------------------------- spawner

/// Starts `run-job <id>` for a caller that cannot host a pane.
pub trait JobSpawner {
    fn spawn(&self, world: &World, project_dir: &Path, job_id: &str) -> Result<(), String>;
}

/// The real one: this executable, detached, output to nowhere (the job
/// writes its own log file).
pub struct DetachedSpawner;

impl JobSpawner for DetachedSpawner {
    fn spawn(&self, world: &World, project_dir: &Path, job_id: &str) -> Result<(), String> {
        let mut cmd = Command::new(&world.exe);
        stack_core::child_env::scrub(&mut cmd);
        cmd.args(&world.job_argv(job_id)[1..])
            .current_dir(project_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        detach(&mut cmd);
        spawn_detached(cmd)
            .map(|_| ())
            .map_err(|e| format!("cannot start the job runner: {e}"))
    }
}

/// Runs the job on a thread of this process with the same tools: what
/// tests use for the headless path.
pub struct ThreadSpawner;

impl JobSpawner for ThreadSpawner {
    fn spawn(&self, world: &World, project_dir: &Path, job_id: &str) -> Result<(), String> {
        let (world, dir, id) = (world.clone(), project_dir.to_path_buf(), job_id.to_string());
        thread::spawn(move || {
            let mut sink = io::sink();
            crate::runjob::run_job(&world, &dir, &id, &mut sink)
        });
        Ok(())
    }
}

/// Append-only text file opener used for logs.
pub fn open_log(path: &Path, truncate: bool) -> io::Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut o = OpenOptions::new();
    o.create(true);
    if truncate {
        o.write(true).truncate(true);
    } else {
        o.append(true);
    }
    o.open(path)
}

/// Write one line, ignoring a closed pane.
pub fn line(out: &mut dyn Write, text: &str) {
    let _ = writeln!(out, "{text}");
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_runner_program_is_chappa_stack_never_the_host_running_it() {
        let tmp = tempfile::tempdir().unwrap();
        let file = format!("chappa-stack{}", std::env::consts::EXE_SUFFIX);
        let bin = tmp.path().join("bin");
        let deps = bin.join("deps");
        std::fs::create_dir_all(&deps).unwrap();
        let host = |dir: &Path| dir.join(format!("host-1a2b{}", std::env::consts::EXE_SUFFIX));
        // Itself, when it is the runner (whether or not the file is there).
        let own = bin.join(&file);
        assert_eq!(runner_exe(Some(&own)), own);
        // Another program with nothing beside it: the bare name, never itself.
        assert_eq!(runner_exe(Some(&host(&bin))), PathBuf::from("chappa-stack"));
        // A runner in the same folder is the one.
        std::fs::write(&own, "").unwrap();
        assert_eq!(runner_exe(Some(&host(&bin))), own);
        // One a folder above is never searched for: it could be planted.
        assert_eq!(
            runner_exe(Some(&host(&deps))),
            PathBuf::from("chappa-stack")
        );
        // A folder named like the program is not the program.
        std::fs::create_dir_all(deps.join(&file)).unwrap();
        assert_eq!(
            runner_exe(Some(&host(&deps))),
            PathBuf::from("chappa-stack")
        );
        assert_eq!(runner_exe(None), PathBuf::from("chappa-stack"));
    }
    use stack_core::driver::EngineKind;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct Counting(AtomicU32);

    impl GatewayResolver for Counting {
        fn gateway(&self, _: &EngineRef, _: Option<ContainerOs>) -> Result<Gateway, String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Gateway::literal("172.20.0.1"))
        }
    }

    #[test]
    fn gateways_are_asked_once_per_engine_within_the_ttl() {
        let cached = CachedGateways::new(Counting(AtomicU32::new(0)));
        let docker = EngineRef {
            name: "docker".into(),
            kind: EngineKind::Docker,
            context: None,
        };
        let podman = EngineRef {
            name: "podman".into(),
            kind: EngineKind::Podman,
            context: None,
        };
        for _ in 0..3 {
            assert_eq!(cached.gateway(&docker, None).unwrap().address, "172.20.0.1");
        }
        cached.gateway(&podman, None).unwrap();
        assert_eq!(cached.inner.0.load(Ordering::SeqCst), 2);
    }

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|s| s.to_string()).collect()
    }

    fn args_of(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn a_powershell_file_only_runs_through_the_interpreter_the_argv_names() {
        let e = command_for(&argv(&["scripts/up.ps1", "-Slot", "0"])).unwrap_err();
        assert!(
            e.contains("[powershell, -NoProfile, -File, scripts/up.ps1]"),
            "{e}"
        );
        let cmd = command_in(
            &argv(&[
                "powershell",
                "-NoProfile",
                "-File",
                "scripts/up.ps1",
                "-Slot",
                "0",
            ]),
            Some(Path::new("/srv/example")),
        )
        .unwrap();
        // The interpreter is the program (found on PATH where it exists);
        // the script is one argument, passed as written.
        let program = cmd.get_program().to_string_lossy().to_ascii_lowercase();
        assert!(
            program == "powershell" || program.ends_with("powershell.exe"),
            "{program}"
        );
        assert_eq!(
            args_of(&cmd),
            ["-NoProfile", "-File", "scripts/up.ps1", "-Slot", "0"]
        );
    }

    #[test]
    fn a_relative_program_with_a_folder_is_taken_from_the_working_directory() {
        let cmd = command_in(&argv(&["tools/up", "x"]), Some(Path::new("/srv/example"))).unwrap();
        assert_eq!(
            PathBuf::from(cmd.get_program()),
            Path::new("/srv/example").join("tools/up")
        );
        let bare = command_in(&argv(&["node", "up.js"]), Some(Path::new("/srv/example"))).unwrap();
        assert_ne!(
            PathBuf::from(bare.get_program()),
            Path::new("/srv/example").join("node")
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_batch_file_named_by_path_runs_through_the_guarded_command_shell() {
        let cmd = command_in(
            &argv(&["scripts\\stop.cmd", "app-0", "two words"]),
            Some(Path::new("C:\\code\\x")),
        )
        .unwrap();
        assert_eq!(cmd.get_program().to_string_lossy(), "cmd");
        assert_eq!(
            args_of(&cmd),
            ["/C", "C:\\code\\x\\scripts\\stop.cmd", "app-0", "two words"]
        );
        let e = command_in(
            &argv(&["scripts\\stop.bat", "a&b"]),
            Some(Path::new("C:\\code\\x")),
        )
        .unwrap_err();
        assert!(
            e.contains("character the command shell would interpret"),
            "{e}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_batch_file_refuses_delayed_expansion_and_control_characters_like_a_pane() {
        for bad in ["!VAR!", "a\u{7}b", "tab\u{1b}x", "line\nbreak", "100%"] {
            let e = command_in(
                &argv(&["scripts\\stop.cmd", bad]),
                Some(Path::new("C:\\code\\x")),
            )
            .unwrap_err();
            assert!(
                e.contains("character the command shell would interpret"),
                "{bad:?}: {e}"
            );
        }
        command_in(
            &argv(&["scripts\\stop.cmd", "with\ttab", "Bash(git:*)"]),
            Some(Path::new("C:\\code\\x")),
        )
        .unwrap();
    }

    fn plant_source_variables() {
        for name in stack_core::child_env::SCRUBBED {
            std::env::set_var(name, "planted-for-the-test");
        }
    }

    fn run_block(dir: &Path) -> RunBlock {
        let mut env = indexmap::IndexMap::new();
        env.insert("KEPT_FOR_THE_CHILD".to_string(), "1".to_string());
        env.insert(
            stack_core::child_env::SOURCE_TOKEN_ENV.to_string(),
            "named-by-a-run-block".to_string(),
        );
        RunBlock {
            argv: stack_core::child_env::env_printer(),
            cwd: dir.to_path_buf(),
            env,
            title: None,
        }
    }

    fn sees_nothing_scrubbed(text: &str) {
        assert_eq!(
            stack_core::child_env::names_a_scrubbed_variable(text),
            None,
            "{text}"
        );
        assert!(text.contains("KEPT_FOR_THE_CHILD=1"), "{text}");
    }

    #[test]
    fn job_steps_and_status_scripts_never_see_the_source_variables() {
        plant_source_variables();
        let dir = tempfile::tempdir().unwrap();
        let run = run_block(dir.path());
        for tree in [false, true] {
            let mut lines = Vec::new();
            run_child(
                &run,
                Duration::from_secs(20),
                &mut |l| lines.push(l.to_string()),
                tree,
            )
            .unwrap();
            sees_nothing_scrubbed(&lines.join("\n"));
        }
        let scripts = ScriptCli {
            running: Arc::new(RunningScripts::default()),
        };
        let mut inv = Invocation::from_argv(&run.argv).cwd(dir.path());
        for (k, v) in &run.env {
            inv = inv.env(k, v);
        }
        let out = scripts.run(&inv).unwrap();
        sees_nothing_scrubbed(&out.stdout);
    }

    #[test]
    fn process_components_never_see_the_source_variables() {
        plant_source_variables();
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("logs/env.log");
        let pid = OsProcesses.spawn(&run_block(dir.path()), &log).unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut text = String::new();
        while Instant::now() < deadline {
            text = std::fs::read_to_string(&log).unwrap_or_default();
            if text.contains("KEPT_FOR_THE_CHILD") && !pid_alive(pid) {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        sees_nothing_scrubbed(&text);
    }
}
