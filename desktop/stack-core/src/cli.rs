//! Engines are command-line tools. This is the one door they are run through.
//!
//! `docker`, `podman` and their `compose` subcommands are invoked argv-only
//! (never through a shell), with an explicit timeout and bounded output, and
//! always through the [`EngineCli`] trait so every driver test can swap in a
//! [`FakeCli`] that records the argv it was given and answers with canned
//! stdout. Nothing above this module spawns a process itself.
//!
//! Quick calls only: `ps`, `inspect`, `version`, `manifest inspect` and the
//! like. Anything that runs long (`build`, `pull`, `up`, `logs -f`) is never
//! run here; the drivers hand such commands back as run blocks
//! ([`crate::jobs::RunBlock`]) for the host application to run in a pane.
//!
//! Standard error is captured but kept out of findings: a failure's
//! [`CliOutput::hint`] is its first line, trimmed and capped, because engine
//! error text can carry paths, registry names and tokens that must not be
//! copied into a status view or a log that outlives the call.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::fmt;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::definition::EngineRef;
use crate::driver::EngineKind;

/// Default deadline for a quick engine call.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// Most stdout kept from one call (a `ps` of a big host fits many times over).
pub const STDOUT_CAP: usize = 8 * 1024 * 1024;
/// Most stderr kept.
pub const STDERR_CAP: usize = 64 * 1024;
/// How much of a failure's first stderr line is shown.
pub const HINT_CAP: usize = 160;

/// One command to run: program, arguments, environment additions, working
/// directory and deadline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Invocation {
    pub program: String,
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub env: IndexMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

fn default_timeout_secs() -> u64 {
    DEFAULT_TIMEOUT.as_secs()
}

impl Invocation {
    pub fn new(program: &str, args: &[&str]) -> Self {
        Self {
            program: program.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            env: IndexMap::new(),
            cwd: None,
            timeout_secs: DEFAULT_TIMEOUT.as_secs(),
        }
    }

    /// From a full argv (`argv[0]` is the program).
    pub fn from_argv(argv: &[String]) -> Self {
        let (program, args) = argv
            .split_first()
            .map(|(p, a)| (p.clone(), a.to_vec()))
            .unwrap_or_default();
        Self {
            program,
            args,
            env: IndexMap::new(),
            cwd: None,
            timeout_secs: DEFAULT_TIMEOUT.as_secs(),
        }
    }

    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout_secs = timeout.as_secs().max(1);
        self
    }

    pub fn env(mut self, key: &str, value: &str) -> Self {
        self.env.insert(key.to_string(), value.to_string());
        self
    }

    /// The whole command line as words, for argv assertions and display.
    pub fn argv(&self) -> Vec<String> {
        let mut v = Vec::with_capacity(self.args.len() + 1);
        v.push(self.program.clone());
        v.extend(self.args.iter().cloned());
        v
    }
}

impl fmt::Display for Invocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.argv().join(" "))
    }
}

/// What a call produced. `status` is `None` when the process was killed.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CliOutput {
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl CliOutput {
    pub fn ok(&self) -> bool {
        self.status == Some(0)
    }

    pub fn success(stdout: &str) -> Self {
        Self {
            status: Some(0),
            stdout: stdout.to_string(),
            stderr: String::new(),
        }
    }

    pub fn failure(status: i32, stderr: &str) -> Self {
        Self {
            status: Some(status),
            stdout: String::new(),
            stderr: stderr.to_string(),
        }
    }

    /// Parse stdout as JSON.
    pub fn json<T: for<'de> Deserialize<'de>>(&self) -> Result<T, CliError> {
        serde_json::from_str(&self.stdout).map_err(|e| CliError::BadOutput {
            what: "JSON".to_string(),
            message: e.to_string(),
        })
    }

    /// The first non-blank stderr line, trimmed and capped: enough to say
    /// "no such image" or "daemon not running" without copying the rest.
    pub fn hint(&self) -> String {
        let line = self
            .stderr
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("");
        let mut hint: String = line.chars().take(HINT_CAP).collect();
        if hint.len() < line.len() {
            hint.push('…');
        }
        hint
    }

    /// `exit N: <hint>` or `exit N` or `killed`.
    pub fn summary(&self) -> String {
        let hint = self.hint();
        match (self.status, hint.is_empty()) {
            (Some(s), true) => format!("exit {s}"),
            (Some(s), false) => format!("exit {s}: {hint}"),
            (None, true) => "killed".to_string(),
            (None, false) => format!("killed: {hint}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "error")]
pub enum CliError {
    /// The program could not start: most often it is not installed.
    #[error("could not run `{program}`: {message}")]
    Spawn { program: String, message: String },
    #[error("`{command}` did not finish within {seconds}s")]
    Timeout { command: String, seconds: u64 },
    #[error("`{command}` failed ({summary})")]
    Failed { command: String, summary: String },
    #[error("unexpected {what} output: {message}")]
    BadOutput { what: String, message: String },
}

/// Runs invocations. The real one spawns; the fake answers from a script.
pub trait EngineCli {
    fn run(&self, inv: &Invocation) -> Result<CliOutput, CliError>;

    /// Run and require exit 0.
    fn run_ok(&self, inv: &Invocation) -> Result<CliOutput, CliError> {
        let out = self.run(inv)?;
        if out.ok() {
            Ok(out)
        } else {
            Err(CliError::Failed {
                command: inv.to_string(),
                summary: out.summary(),
            })
        }
    }
}

// ------------------------------------------------------------- engine argv

/// `docker` or `podman`, with the remote selector when the engine is not
/// this machine's: `--context <name>` for docker, `--connection <name>` for
/// podman. Both are global flags, so they come before any subcommand.
pub fn engine_argv(engine: &EngineRef) -> Vec<String> {
    let mut v = vec![engine.kind.name().to_string()];
    if let Some(ctx) = &engine.context {
        v.push(
            match engine.kind {
                EngineKind::Podman => "--connection",
                _ => "--context",
            }
            .to_string(),
        );
        v.push(ctx.clone());
    }
    v
}

/// An engine subcommand: `docker [--context x] <args…>`.
pub fn engine_invocation(engine: &EngineRef, args: &[&str]) -> Invocation {
    let mut argv = engine_argv(engine);
    argv.extend(args.iter().map(|s| s.to_string()));
    Invocation::from_argv(&argv)
}

/// `docker compose` / `podman compose` for one project:
/// `<engine> compose -p <project> -f <file>… <args…>`.
pub fn compose_argv(
    engine: &EngineRef,
    project: &str,
    files: &[PathBuf],
    args: &[String],
) -> Vec<String> {
    let mut argv = engine_argv(engine);
    argv.push("compose".to_string());
    argv.push("-p".to_string());
    argv.push(project.to_string());
    for f in files {
        argv.push("-f".to_string());
        argv.push(f.to_string_lossy().into_owned());
    }
    argv.extend(args.iter().cloned());
    argv
}

// -------------------------------------------------------------- real runner

/// Spawns processes with a deadline and bounded output.
#[derive(Debug, Clone, Default)]
pub struct CliRunner;

fn read_capped(mut r: impl Read, cap: usize) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match r.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = cap.saturating_sub(kept.len());
                kept.extend_from_slice(&buf[..n.min(room)]);
            }
        }
    }
    kept
}

impl EngineCli for CliRunner {
    fn run(&self, inv: &Invocation) -> Result<CliOutput, CliError> {
        let mut cmd = Command::new(&inv.program);
        cmd.args(&inv.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in &inv.env {
            cmd.env(k, v);
        }
        if let Some(cwd) = &inv.cwd {
            cmd.current_dir(cwd);
        }
        let mut child = cmd.spawn().map_err(|e| CliError::Spawn {
            program: inv.program.clone(),
            message: e.to_string(),
        })?;
        let (out_tx, out_rx) = mpsc::channel();
        let stdout = child.stdout.take().expect("stdout is piped");
        thread::spawn(move || {
            let _ = out_tx.send(read_capped(stdout, STDOUT_CAP));
        });
        let (err_tx, err_rx) = mpsc::channel();
        let stderr = child.stderr.take().expect("stderr is piped");
        thread::spawn(move || {
            let _ = err_tx.send(read_capped(stderr, STDERR_CAP));
        });
        let timeout = Duration::from_secs(inv.timeout_secs);
        let deadline = Instant::now() + timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(e) => {
                    let _ = child.kill();
                    return Err(CliError::Spawn {
                        program: inv.program.clone(),
                        message: format!("waiting: {e}"),
                    });
                }
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(CliError::Timeout {
                    command: inv.to_string(),
                    seconds: inv.timeout_secs,
                });
            }
            thread::sleep(Duration::from_millis(15));
        };
        // A grandchild that inherited the pipes can keep them open; do not
        // wait on it forever.
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

// ---------------------------------------------------------------- fake cli

/// A scripted engine. Rules are matched against the argv (program included)
/// by words: a rule fires when every word it names appears in the argv, in
/// order (not necessarily adjacent). The most recently added matching rule
/// wins, so a test can set a general answer and then override one call.
/// Unmatched calls succeed with empty stdout. Every call is recorded.
#[derive(Debug, Default)]
pub struct FakeCli {
    pub calls: RefCell<Vec<Invocation>>,
    rules: Vec<(Vec<String>, Result<CliOutput, CliError>)>,
    /// Programs that are "not installed": spawning them fails.
    missing: Vec<String>,
}

impl FakeCli {
    pub fn new() -> Self {
        Self::default()
    }

    /// Answer `stdout` with exit 0 to calls containing `words` in order.
    pub fn on(mut self, words: &[&str], stdout: &str) -> Self {
        self.rules.push((
            words.iter().map(|s| s.to_string()).collect(),
            Ok(CliOutput::success(stdout)),
        ));
        self
    }

    /// Make calls containing `words` run out of time, as the real runner
    /// reports a command it had to kill.
    pub fn timing_out(mut self, words: &[&str]) -> Self {
        let command = words.join(" ");
        self.rules.push((
            words.iter().map(|s| s.to_string()).collect(),
            Err(CliError::Timeout {
                command,
                seconds: 0,
            }),
        ));
        self
    }

    /// Fail calls containing `words` with `status` and `stderr`.
    pub fn failing(mut self, words: &[&str], status: i32, stderr: &str) -> Self {
        self.rules.push((
            words.iter().map(|s| s.to_string()).collect(),
            Ok(CliOutput::failure(status, stderr)),
        ));
        self
    }

    /// Make `program` unspawnable.
    pub fn without(mut self, program: &str) -> Self {
        self.missing.push(program.to_string());
        self
    }

    /// Add a rule after construction.
    pub fn add(&mut self, words: &[&str], out: CliOutput) {
        self.rules
            .push((words.iter().map(|s| s.to_string()).collect(), Ok(out)));
    }

    /// Every recorded argv, one line each (space-joined), for assertions.
    pub fn lines(&self) -> Vec<String> {
        self.calls.borrow().iter().map(|i| i.to_string()).collect()
    }

    /// The recorded invocations containing `words` in order.
    pub fn calls_matching(&self, words: &[&str]) -> Vec<Invocation> {
        self.calls
            .borrow()
            .iter()
            .filter(|i| matches_in_order(&i.argv(), words))
            .cloned()
            .collect()
    }

    pub fn clear_calls(&self) {
        self.calls.borrow_mut().clear();
    }
}

/// True when every word of `words` occurs in `argv`, in that order.
pub fn matches_in_order<S: AsRef<str>>(argv: &[String], words: &[S]) -> bool {
    let mut pos = 0;
    for w in words {
        match argv[pos..].iter().position(|a| a == w.as_ref()) {
            Some(i) => pos += i + 1,
            None => return false,
        }
    }
    true
}

impl EngineCli for FakeCli {
    fn run(&self, inv: &Invocation) -> Result<CliOutput, CliError> {
        self.calls.borrow_mut().push(inv.clone());
        if self.missing.contains(&inv.program) {
            return Err(CliError::Spawn {
                program: inv.program.clone(),
                message: "program not found".to_string(),
            });
        }
        let argv = inv.argv();
        for (words, out) in self.rules.iter().rev() {
            if matches_in_order(&argv, words) {
                return out.clone();
            }
        }
        Ok(CliOutput::success(""))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn docker_remote() -> EngineRef {
        EngineRef {
            name: "build-host".into(),
            kind: EngineKind::Docker,
            context: Some("build-host".into()),
        }
    }

    #[test]
    fn engine_argv_puts_the_remote_selector_before_the_subcommand() {
        let d = engine_invocation(&docker_remote(), &["version", "--format", "json"]);
        assert_eq!(
            d.argv(),
            [
                "docker",
                "--context",
                "build-host",
                "version",
                "--format",
                "json"
            ]
        );
        let p = EngineRef {
            name: "podman".into(),
            kind: EngineKind::Podman,
            context: Some("vm".into()),
        };
        assert_eq!(
            engine_argv(&p),
            ["podman", "--connection", "vm"].map(String::from)
        );
        let local = EngineRef {
            name: "docker".into(),
            kind: EngineKind::Docker,
            context: None,
        };
        assert_eq!(engine_argv(&local), ["docker"].map(String::from));
    }

    #[test]
    fn compose_argv_names_project_then_every_file_then_the_verb() {
        let argv = compose_argv(
            &docker_remote(),
            "app-1-core",
            &[PathBuf::from("stack/core.yml"), PathBuf::from("ov.yml")],
            &["up".to_string(), "-d".to_string()],
        );
        assert_eq!(
            argv,
            [
                "docker",
                "--context",
                "build-host",
                "compose",
                "-p",
                "app-1-core",
                "-f",
                "stack/core.yml",
                "-f",
                "ov.yml",
                "up",
                "-d"
            ]
            .map(String::from)
        );
    }

    #[test]
    fn hint_is_first_nonblank_stderr_line_capped() {
        let out = CliOutput::failure(1, "\n\n  Error: no such image: x  \nsecond line\n");
        assert_eq!(out.hint(), "Error: no such image: x");
        assert_eq!(out.summary(), "exit 1: Error: no such image: x");
        let long = CliOutput::failure(2, &"a".repeat(HINT_CAP + 10));
        assert_eq!(long.hint().chars().count(), HINT_CAP + 1);
        assert!(long.hint().ends_with('…'));
        assert_eq!(CliOutput::failure(3, "").summary(), "exit 3");
        let killed = CliOutput {
            status: None,
            stdout: String::new(),
            stderr: String::new(),
        };
        assert_eq!(killed.summary(), "killed");
    }

    #[test]
    fn fake_matches_words_in_order_latest_rule_wins_and_records() {
        let fake = FakeCli::new()
            .on(&["docker", "compose", "ps"], "[]")
            .on(
                &["docker", "compose", "-p", "app-1-core", "ps"],
                "[{\"Name\":\"x\"}]",
            )
            .failing(&["podman", "version"], 125, "cannot connect\nmore");
        let generic = fake
            .run(&Invocation::new(
                "docker",
                &["compose", "-p", "app-1-apis", "ps", "-a"],
            ))
            .unwrap();
        assert_eq!(generic.stdout, "[]");
        let specific = fake
            .run(&Invocation::new(
                "docker",
                &["compose", "-p", "app-1-core", "ps"],
            ))
            .unwrap();
        assert_eq!(specific.stdout, "[{\"Name\":\"x\"}]");
        // out of order does not match
        assert!(!matches_in_order(
            &["docker", "ps", "compose"].map(String::from),
            &["compose", "ps"]
        ));
        let err = fake
            .run_ok(&Invocation::new("podman", &["version"]))
            .unwrap_err();
        assert_eq!(
            err,
            CliError::Failed {
                command: "podman version".into(),
                summary: "exit 125: cannot connect".into(),
            }
        );
        assert_eq!(fake.calls.borrow().len(), 3);
        assert_eq!(fake.calls_matching(&["compose", "ps"]).len(), 2);
        let missing = FakeCli::new().without("podman");
        assert!(matches!(
            missing.run(&Invocation::new("podman", &["version"])),
            Err(CliError::Spawn { .. })
        ));
    }

    #[test]
    fn real_runner_captures_output_status_and_timeouts() {
        // A program that surely exists on both platforms and prints something.
        let (program, args): (&str, &[&str]) = if cfg!(windows) {
            ("cmd", &["/C", "echo hello&& exit 3"])
        } else {
            ("sh", &["-c", "echo hello; exit 3"])
        };
        let out = CliRunner.run(&Invocation::new(program, args)).unwrap();
        assert_eq!(out.status, Some(3));
        assert_eq!(out.stdout.trim(), "hello");
        let missing = CliRunner.run(&Invocation::new("definitely-not-a-program-xyz", &[]));
        assert!(matches!(missing, Err(CliError::Spawn { .. })));
        let (program, args): (&str, &[&str]) = if cfg!(windows) {
            ("cmd", &["/C", "ping -n 10 127.0.0.1 > nul"])
        } else {
            ("sh", &["-c", "sleep 10"])
        };
        let started = Instant::now();
        let slow = CliRunner.run(&Invocation::new(program, args).timeout(Duration::from_secs(1)));
        assert!(matches!(slow, Err(CliError::Timeout { seconds: 1, .. })));
        assert!(started.elapsed() < Duration::from_secs(8));
    }
}
