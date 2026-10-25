//! Windows runs a `.cmd`/`.bat` program through cmd.exe, which re-parses
//! the whole command line with its own metacharacters. portable-pty quotes
//! arguments for the C runtime, not for cmd.exe, so an argument such as
//! `x"&type nul>marker&"` would start a second command. These tests drive
//! real ConPTY spawns of a throwaway batch file and prove such arguments
//! are refused before anything runs, while ordinary arguments still pass.
#![cfg(windows)]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use term_core::pty::{PtySession, PtySpec};

const TIMEOUT: Duration = Duration::from_secs(10);

fn scratch_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "term-core-batch-{tag}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A harmless batch file that only echoes its argument string.
fn write_probe(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, "@echo off\r\necho ARGS:%*\r\n").unwrap();
    path
}

fn spec(command: &str, args: &[&str], cwd: &Path) -> PtySpec {
    PtySpec {
        command: command.into(),
        args: args.iter().map(|a| a.to_string()).collect(),
        cwd: Some(cwd.to_path_buf()),
        cols: 200,
        rows: 24,
        ..PtySpec::default()
    }
}

fn reader_collect(mut reader: Box<dyn Read + Send>) -> Arc<Mutex<String>> {
    let buf = Arc::new(Mutex::new(String::new()));
    let buf2 = buf.clone();
    std::thread::spawn(move || {
        let mut tmp = [0u8; 4096];
        loop {
            match reader.read(&mut tmp) {
                Ok(0) | Err(_) => break,
                Ok(n) => buf2
                    .lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&tmp[..n])),
            }
        }
    });
    buf
}

/// Drains the pty and polls for exit (ConPTY holds a child whose output
/// nobody reads), killing it at the timeout. Returns everything printed.
fn run_to_exit(mut session: PtySession) -> String {
    let buf = reader_collect(session.take_reader().unwrap());
    // ConPTY opens with a cursor-position query and holds the child until
    // the terminal answers it.
    let mut writer = session.writer().unwrap();
    let mut answered = false;
    let start = Instant::now();
    while start.elapsed() < TIMEOUT {
        if !answered && buf.lock().unwrap().contains("[6n") {
            let _ = writer.write_all(b"[1;1R");
            let _ = writer.flush();
            answered = true;
        }
        if let Ok(Some(_)) = session.try_wait() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(writer);
    drop(session);
    std::thread::sleep(Duration::from_millis(200));
    let text = buf.lock().unwrap().clone();
    text
}

#[test]
fn injected_command_in_a_batch_argument_is_refused_and_never_runs() {
    let dir = scratch_dir("inject");
    let probe = write_probe(&dir, "probe.cmd");
    let marker = dir.join("marker.txt");

    let result = PtySession::spawn(&spec(
        probe.to_str().unwrap(),
        &["ok", "x\"&type nul>marker.txt&\""],
        &dir,
    ));

    let err = match result {
        Ok(session) => {
            // Let an unsafe spawn finish so the failure message says whether
            // the injected command actually ran.
            let text = run_to_exit(session);
            panic!(
                "batch spawn with a cmd.exe metacharacter was allowed; injected command ran: {}; output: {text:?}",
                marker.exists()
            );
        }
        Err(err) => err,
    };
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    let msg = err.to_string();
    assert!(
        msg.contains("argument 2"),
        "error should name the index: {msg}"
    );
    std::thread::sleep(Duration::from_millis(300));
    assert!(!marker.exists(), "injected command must not run");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn batch_found_through_path_without_an_extension_is_also_guarded() {
    let dir = scratch_dir("pathext");
    write_probe(&dir, "probe.cmd");
    let mut spec = spec("probe", &["a|b"], &dir);
    spec.env
        .push(("PATH".into(), dir.to_string_lossy().into_owned()));
    spec.env
        .push(("PATHEXT".into(), ".COM;.EXE;.BAT;.CMD".into()));

    let err = PtySession::spawn(&spec)
        .err()
        .expect("spawn must be refused");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn plain_arguments_with_spaces_still_reach_a_batch_file() {
    let dir = scratch_dir("plain");
    let probe = write_probe(&dir, "probe.bat");

    let session = PtySession::spawn(&spec(
        probe.to_str().unwrap(),
        &["hello world", "--flag=a:b/c\\d.e", "x,y;z"],
        &dir,
    ))
    .expect("plain arguments must be allowed");
    let text = run_to_exit(session);
    assert!(text.contains("--flag=a:b/c\\d.e"), "got: {text:?}");
    assert!(text.contains("\"hello world\""), "got: {text:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
