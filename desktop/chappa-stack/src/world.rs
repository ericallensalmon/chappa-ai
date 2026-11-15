//! Everything outside this process the runner talks to, as one bundle of
//! shareable trait objects: the engine CLI, git, HTTP probes, the gateway
//! resolver, the port probe, how a job's long steps run, how process
//! components are started and stopped, and how a headless job is spawned.
//!
//! The server answers requests on several threads and refreshes caches on
//! another, so every tool here is `Send + Sync`. Tests swap any of them for
//! a fake ([`Locked`] makes stack-core's single-threaded fakes shareable).

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
    /// The program a pane runs for `run-job` (this executable).
    pub exe: PathBuf,
    /// How long a freshly started process component must stay alive to
    /// count as started.
    pub settle: Duration,
    /// What time it is: data-safety scan times and the `every:` schedule.
    pub clock: Shared<dyn Clock + Send + Sync>,
    /// The pause between attempts of a data-safety hook run right after
    /// up, while a database may still be starting.
    pub hook_retry: Duration,
}

/// The default for [`World::settle`].
pub const PROCESS_SETTLE: Duration = Duration::from_millis(1500);
/// The default for [`World::hook_retry`].
pub const HOOK_RETRY: Duration = Duration::from_secs(3);

impl World {
    /// The real machine: engines, git and HTTP for real.
    pub fn real() -> World {
        let cli: Shared<dyn EngineCli + Send + Sync> = Arc::new(CliRunner);
        World {
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
            exe: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("chappa-stack")),
            settle: PROCESS_SETTLE,
            clock: Arc::new(SystemClock),
            hook_retry: HOOK_RETRY,
        }
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
        let mut cmd = command_for(&run.argv)?;
        cmd.current_dir(&run.cwd)
            .envs(&run.env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
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
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "timed out after {}; the command was stopped",
                    crate::timeouts::describe(limit)
                ));
            }
            thread::sleep(Duration::from_millis(50));
        };
        for r in readers {
            let _ = r.join();
        }
        while let Ok(line) = rx.try_recv() {
            out(&line);
        }
        Ok(status.code().unwrap_or(-1))
    }
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

/// A [`Command`] for argv. On Windows a bare program name is looked up on
/// `PATH` with the usual extensions, because `npm` or `npx` are `.cmd`
/// scripts that `CreateProcess` will not find by name; a script runs
/// through `cmd /C`, and then no argument may carry a character `cmd`
/// would interpret.
pub fn command_for(argv: &[String]) -> Result<Command, String> {
    let program = argv.first().ok_or("the command is empty")?;
    #[cfg(windows)]
    {
        if let Some(found) = find_on_path(program) {
            let ext = found
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            if ext == "cmd" || ext == "bat" {
                if let Some(bad) = argv[1..]
                    .iter()
                    .find(|a| a.chars().any(|c| "&|<>^%\"\r\n".contains(c)))
                {
                    return Err(format!(
                        "`{program}` is a batch script and argument `{bad}` has a character the command shell would interpret"
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
    let mut cmd = Command::new(program);
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
            .envs(&run.env)
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
    Command::new(program)
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
        cmd.arg("run-job")
            .arg(job_id)
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
}
