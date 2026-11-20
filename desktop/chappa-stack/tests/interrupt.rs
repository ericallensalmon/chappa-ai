//! Ctrl+C in a job's terminal, delivered to the real `chappa-stack run-job`
//! as a terminal delivers it: on Unix `SIGINT` to the runner's whole
//! process group (the terminal's foreground group), on Windows a real
//! `CTRL_C_EVENT` to every process on the runner's console. The runner
//! cancels the job the way the Cancel action does: the step's process tree
//! is gone, the step's own processes never saw the Ctrl+C, the record says
//! `cancelled`, the lease is free, the Environment's last job reads
//! cancelled, and the terminal says what happened.

mod common;

use serde_json::{json, Value};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use chappa_stack::lease;
use chappa_stack::records::{JobRecord, JobStatus};
use chappa_stack::runjob::EXIT_CANCELLED;
use common::*;
use stack_core::cli::FakeCli;
use stack_core::probe::FakeHttp;

/// The environment variable that turns the helper below into the sender
/// of a console Ctrl+C (Windows).
#[cfg(windows)]
const CTRL_C_CONSOLE_OF: &str = "CHAPPA_TEST_CTRL_C_CONSOLE_OF";

fn has_node() -> bool {
    Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// A script that would leave `got-int` behind if a Ctrl+C reached it,
/// starts a child that writes `beat` every 200 ms, and runs until stopped.
const SLOW: &str = r#""use strict";
const { spawn } = require("child_process");
const fs = require("fs");
const path = require("path");
const dir = process.cwd();
process.on("SIGINT", () => fs.writeFileSync(path.join(dir, "got-int"), "step"));
const beat = path.join(dir, "beat");
spawn(process.execPath, ["-e", `process.on("SIGINT", () => require("fs").writeFileSync(${JSON.stringify(path.join(dir, "got-int"))}, "child")); setInterval(() => require("fs").writeFileSync(${JSON.stringify(beat)}, String(Date.now())), 200)`], { stdio: "ignore" });
console.log("working...");
setInterval(() => {}, 1000);
"#;

fn wait_until(what: &str, within: Duration, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + within;
    while !f() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Whether `beat` stops changing (the step's child is gone).
fn heartbeat_stops(dir: &Path) -> bool {
    let beat = dir.join("beat");
    std::thread::sleep(Duration::from_millis(800));
    let a = std::fs::read_to_string(&beat).unwrap_or_default();
    std::thread::sleep(Duration::from_millis(1200));
    let b = std::fs::read_to_string(&beat).unwrap_or_default();
    a == b
}

/// `run-job <id>` as a pane starts it, on a console (Windows) or in a
/// process group (Unix) of its own, so the Ctrl+C sent below reaches what
/// a terminal's would and nothing else.
fn start_runner(dir: &Path, id: &str) -> std::process::Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chappa-stack"));
    cmd.args(["run-job", id, "--project"])
        .arg(dir)
        .arg(chappa_stack::world::STATE_DIR_ARG)
        .arg(stack_core::gate_store::install_for_tests())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // A console of its own, without a window.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.spawn().unwrap()
}

/// Press Ctrl+C in the runner's terminal.
#[cfg(unix)]
fn press_ctrl_c(runner: u32) {
    let ok = Command::new("kill")
        .args(["-INT", "--", &format!("-{runner}")])
        .status()
        .unwrap()
        .success();
    assert!(ok, "kill -INT failed");
}

/// Press Ctrl+C on the runner's console: this test binary, started again
/// as the helper below, attaches to that console and sends the event to
/// every process on it.
#[cfg(windows)]
fn press_ctrl_c(runner: u32) {
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "a_ctrl_c_sender_when_asked_to_be_one",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CTRL_C_CONSOLE_OF, runner.to_string())
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("sent ctrl-c"),
        "{text}{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Not a test of its own: with [`CTRL_C_CONSOLE_OF`] set, it sends Ctrl+C to
/// the console of that process (what the integration test above uses).
#[cfg(windows)]
#[test]
fn a_ctrl_c_sender_when_asked_to_be_one() {
    use windows_sys::Win32::System::Console::{
        AttachConsole, FreeConsole, GenerateConsoleCtrlEvent, SetConsoleCtrlHandler, CTRL_C_EVENT,
    };
    let Ok(pid) = std::env::var(CTRL_C_CONSOLE_OF) else {
        return;
    };
    let pid: u32 = pid.parse().unwrap();
    // SAFETY: plain console calls on this process.
    unsafe {
        FreeConsole();
        assert_ne!(AttachConsole(pid), 0, "cannot attach to {pid}'s console");
        // The event reaches this process too; it is not for it.
        SetConsoleCtrlHandler(None, 1);
        assert_ne!(GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0), 0);
        FreeConsole();
    }
    println!("sent ctrl-c");
}

#[test]
fn ctrl_c_in_a_jobs_terminal_cancels_it_like_the_cancel_action() {
    if !has_node() {
        eprintln!("skipping the Ctrl+C job: node is not on PATH");
        return;
    }
    let p = Project::with(
        "stacks:\n  app:\n    components:\n      slow:\n        driver: script\n        up: [node, scripts/slow.js]\n        timeouts: { up: 2m }\n",
        &[("scripts/slow.js", SLOW)],
    );
    let f = fakes(FakeCli::new(), FakeHttp::default());
    let src = source(f.world.clone(), &p.dir);
    let (_, env) = get(&src, "/api/environment");
    let row = env["components"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "slow")
        .unwrap()
        .clone();
    let up = row["actions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == "up")
        .unwrap()
        .clone();
    let (status, reply) = post(
        &src,
        "/api/actions",
        &json!({ "id": "up", "target": { "view": "environment", "key": "slow" }, "params": up["params"], "confirmed": true }),
        true,
    );
    assert_eq!(status, 202, "{reply}");
    let id = reply["jobId"].as_str().unwrap().to_string();

    let mut runner = start_runner(&p.dir, &id);
    let mut stdout = runner.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout.read_to_string(&mut s);
        s
    });
    wait_until(
        "the step's child never started",
        Duration::from_secs(30),
        || p.dir.join("beat").exists(),
    );
    let t = Instant::now();
    press_ctrl_c(runner.id());
    let code = loop {
        if let Some(s) = runner.try_wait().unwrap() {
            break s.code();
        }
        assert!(
            t.elapsed() < Duration::from_secs(40),
            "the runner did not end"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    let pane = reader.join().unwrap();
    let mut stderr = String::new();
    let _ = runner.stderr.take().unwrap().read_to_string(&mut stderr);
    assert_eq!(code, Some(EXIT_CANCELLED), "{pane}{stderr}");

    let rec = JobRecord::load(&p.work(), &id).unwrap().unwrap();
    let result: Value =
        serde_json::from_str(&std::fs::read_to_string(&rec.result).unwrap()).unwrap();
    let n = result["steps"].as_array().unwrap().len();
    assert!(
        pane.contains(&format!(
            "\r^C — cancelling \"{}\": stopping step ",
            rec.label
        )),
        "{pane}"
    );
    assert!(
        pane.contains(&format!(
            "/{n} (slow: up script) and everything it started…"
        )),
        "{pane}"
    );
    assert!(pane.contains("✗ cancelled after "), "{pane}");
    assert!(pane.contains(" stopped, "), "{pane}");
    assert!(!pane.contains("cancel already in progress"), "{pane}");
    let log = std::fs::read_to_string(&rec.log).unwrap();
    assert!(log.contains("^C — cancelling"), "{log}");

    assert_eq!(rec.status, JobStatus::Cancelled);
    assert_eq!(rec.exit_code, Some(EXIT_CANCELLED));
    assert!(
        rec.error.as_deref().unwrap().starts_with("cancelled"),
        "{:?}",
        rec.error
    );
    // The lease is free, its holder gone, the cancel request cleared.
    assert_eq!(
        lease::held(&lease::instance_lock_path(&p.work(), &rec.instance)),
        Some(false)
    );
    assert!(lease::read_holder(&p.work(), &rec.instance).is_none());
    assert!(lease::cancel_requested(&p.work(), &id).is_none());
    // The whole tree is gone, and none of it saw the Ctrl+C.
    assert!(heartbeat_stops(&p.dir), "the step's child still runs");
    assert!(
        !p.dir.join("got-int").exists(),
        "the Ctrl+C reached the step: {}",
        std::fs::read_to_string(p.dir.join("got-int")).unwrap_or_default()
    );
    let (_, env) = get(&src, "/api/environment");
    assert_eq!(env["job"]["active"], Value::Null, "{}", env["job"]);
    assert_eq!(env["job"]["last"]["status"], "cancelled", "{}", env["job"]);
}
