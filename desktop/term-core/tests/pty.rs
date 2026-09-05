//! Integration tests for PtySession. These run
//! against real ptys on Linux; spawn/kill/resize are exercised exactly as
//! the actor will drive them. They shell out to `sh`, `stty` and `sleep`,
//! so the whole file is compiled out on Windows (ConPTY is covered by the
//! actor tests and the app itself).
#![cfg(unix)]

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use term_core::pty::{ExitStatus, PtySession, PtySpec};

const TIMEOUT: Duration = Duration::from_secs(5);

/// Reads the pty on a background thread into a growing string buffer so
/// tests can wait on substrings without blocking the main thread.
fn reader_collect(mut reader: Box<dyn Read + Send>) -> Arc<Mutex<String>> {
    let buf = Arc::new(Mutex::new(String::new()));
    let buf2 = buf.clone();
    std::thread::spawn(move || {
        let mut tmp = [0u8; 4096];
        loop {
            match reader.read(&mut tmp) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let text = String::from_utf8_lossy(&tmp[..n]);
                    buf2.lock().unwrap().push_str(&text);
                }
            }
        }
    });
    buf
}

/// Polls the collector buffer until it contains `needle` or times out.
fn wait_for(buf: &Mutex<String>, needle: &str) -> String {
    let start = Instant::now();
    loop {
        let text = buf.lock().unwrap().clone();
        if text.contains(needle) {
            return text;
        }
        assert!(
            start.elapsed() < TIMEOUT,
            "timed out waiting for {needle:?}; got so far: {text:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn sh(args: Vec<&str>) -> PtySpec {
    PtySpec {
        command: "sh".into(),
        args: args.into_iter().map(String::from).collect(),
        cols: 80,
        rows: 24,
        ..PtySpec::default()
    }
}

#[test]
fn echo_roundtrip() {
    let mut session = PtySession::spawn(&sh(vec!["-c", "cat"])).unwrap();
    let buf = reader_collect(session.take_reader().unwrap());

    let mut writer = session.writer().unwrap();
    writer.write_all(b"hello\n").unwrap();
    writer.flush().unwrap();

    let out = wait_for(&buf, "hello");
    assert!(out.contains("hello"), "roundtrip failed: {out:?}");
}

#[test]
fn env_defaults_and_override() {
    // TERM and COLORTERM are injected by default.
    let mut session = PtySession::spawn(&sh(vec!["-c", "printf %s \"$TERM\""])).unwrap();
    let out = wait_for(
        &reader_collect(session.take_reader().unwrap()),
        "xterm-256color",
    );
    assert_eq!(out.trim(), "xterm-256color", "got: {out:?}");

    let mut session = PtySession::spawn(&sh(vec!["-c", "printf %s \"$COLORTERM\""])).unwrap();
    let out = wait_for(&reader_collect(session.take_reader().unwrap()), "truecolor");
    assert_eq!(out.trim(), "truecolor", "got: {out:?}");

    // Spec env overrides the injected default.
    let mut spec = sh(vec!["-c", "printf %s \"$TERM\""]);
    spec.env = vec![("TERM".into(), "xterm-custom".into())];
    let mut session = PtySession::spawn(&spec).unwrap();
    let out = wait_for(
        &reader_collect(session.take_reader().unwrap()),
        "xterm-custom",
    );
    assert_eq!(out.trim(), "xterm-custom", "got: {out:?}");

    // Spec env overrides an inherited variable (HOME comes from the parent
    // env). An explicit existing cwd keeps portable-pty's PATH search
    // working even though HOME no longer points anywhere real.
    let mut spec = sh(vec!["-c", "printf %s \"$HOME\""]);
    spec.cwd = Some("/tmp".into());
    spec.env = vec![("HOME".into(), "/tmp/fakehome".into())];
    let mut session = PtySession::spawn(&spec).unwrap();
    let out = wait_for(
        &reader_collect(session.take_reader().unwrap()),
        "/tmp/fakehome",
    );
    assert_eq!(out.trim(), "/tmp/fakehome", "got: {out:?}");
}

#[test]
fn resize_initial_size() {
    let mut session = PtySession::spawn(&PtySpec {
        command: "sh".into(),
        args: vec!["-c".into(), "stty size".into()],
        cols: 100,
        rows: 40,
        ..PtySpec::default()
    })
    .unwrap();
    let out = wait_for(&reader_collect(session.take_reader().unwrap()), "40 100");
    assert!(out.contains("40 100"), "got: {out:?}");
}

#[test]
fn resize_live_session() {
    let mut session = PtySession::spawn(&sh(vec![])).unwrap();
    let buf = reader_collect(session.take_reader().unwrap());
    let mut writer = session.writer().unwrap();

    writer.write_all(b"stty size\n").unwrap();
    writer.flush().unwrap();
    let out = wait_for(&buf, "24 80");
    assert!(out.contains("24 80"), "initial size wrong: {out:?}");

    session.resize(100, 60).unwrap();
    writer.write_all(b"stty size\n").unwrap();
    writer.flush().unwrap();
    let out = wait_for(&buf, "60 100");
    assert!(out.contains("60 100"), "resize didn't take: {out:?}");
}

#[test]
fn exit_code() {
    let mut session = PtySession::spawn(&sh(vec!["-c", "exit 3"])).unwrap();
    let status: ExitStatus = session.wait().unwrap();
    assert_eq!(status.code, Some(3));
    assert!(!status.success);
}

#[test]
fn kill_then_wait_returns_promptly() {
    let mut session = PtySession::spawn(&PtySpec {
        command: "sleep".into(),
        args: vec!["100".into()],
        cols: 80,
        rows: 24,
        ..PtySpec::default()
    })
    .unwrap();

    let start = Instant::now();
    session.kill().unwrap();
    let status = session.wait().unwrap();
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "wait() did not return promptly after kill"
    );
    assert!(!status.success);
}

#[test]
fn drop_reaps_child() {
    // Echo our own pid, then exec into a long sleep (same pid).
    let mut session = PtySession::spawn(&sh(vec!["-c", "echo $$; exec sleep 100"])).unwrap();
    let buf = reader_collect(session.take_reader().unwrap());

    let pid: u32 = {
        let start = Instant::now();
        loop {
            let text = buf.lock().unwrap().clone();
            if let Some(pid) = text
                .lines()
                .next()
                .and_then(|line| line.trim().parse::<u32>().ok())
            {
                break pid;
            }
            assert!(
                start.elapsed() < TIMEOUT,
                "child never echoed its pid; got: {text:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    };

    drop(buf);
    drop(session);

    // A reaped process leaves no /proc entry (a zombie would still show a
    // `stat` file with state Z).
    let stat = std::path::Path::new("/proc")
        .join(pid.to_string())
        .join("stat");
    let mut gone = false;
    for _ in 0..50 {
        if !stat.exists() {
            gone = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(gone, "pid {pid} still present after dropping the session");
}

#[test]
fn spawn_failure_is_loud() {
    let err = match PtySession::spawn(&PtySpec {
        command: "/no/such/binary".into(),
        cols: 80,
        rows: 24,
        ..PtySpec::default()
    }) {
        Ok(_) => panic!("expected spawn to fail"),
        Err(e) => e,
    };
    let msg = err.to_string();
    assert!(
        msg.contains("No such file"),
        "expected the OS error message, got: {msg:?}"
    );
}
