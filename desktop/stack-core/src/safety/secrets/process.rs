//! Running a scanner process with a deadline and bounded output.
//!
//! A scanner that hangs must not hang the gate that called it, and one that
//! prints without end must not exhaust memory. Output is read on helper
//! threads (so a full pipe never blocks the child), capped, and the child
//! is killed when the deadline passes.

use crate::safety::SafetyError;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// Most stderr kept for an error message.
const STDERR_CAP: usize = 64 * 1024;
/// How long to wait for the output pipes after the child has exited. A
/// grandchild that inherited them can keep them open indefinitely.
const PIPE_GRACE: Duration = Duration::from_secs(2);

pub(crate) struct Ran {
    pub status: Option<i32>,
    pub stdout: Vec<u8>,
    pub stdout_overflowed: bool,
    pub stderr: String,
}

fn read_capped(mut r: impl Read, cap: usize) -> (Vec<u8>, bool) {
    let mut kept = Vec::new();
    let mut buf = [0u8; 8192];
    let mut overflowed = false;
    loop {
        match r.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = cap.saturating_sub(kept.len());
                if n > room {
                    overflowed = true;
                }
                kept.extend_from_slice(&buf[..n.min(room)]);
            }
        }
    }
    (kept, overflowed)
}

/// Run `cmd` to completion or until `timeout`, keeping at most
/// `stdout_cap` bytes of stdout. `what` names the process in errors.
pub(crate) fn run(
    cmd: &mut Command,
    what: &str,
    timeout: Duration,
    stdout_cap: usize,
) -> Result<Ran, SafetyError> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::child_env::scrub(cmd);
    let mut child = cmd
        .spawn()
        .map_err(|e| SafetyError::Scanner(format!("could not start {what}: {e}")))?;

    let (out_tx, out_rx) = mpsc::channel();
    let stdout = child.stdout.take().expect("stdout is piped");
    thread::spawn(move || {
        let _ = out_tx.send(read_capped(stdout, stdout_cap));
    });
    let (err_tx, err_rx) = mpsc::channel();
    let stderr = child.stderr.take().expect("stderr is piped");
    thread::spawn(move || {
        let _ = err_tx.send(read_capped(stderr, STDERR_CAP));
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                return Err(SafetyError::Scanner(format!("waiting for {what}: {e}")));
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(SafetyError::Timeout {
                what: what.to_string(),
                seconds: timeout.as_secs(),
            });
        }
        thread::sleep(Duration::from_millis(15));
    };

    let (stdout, stdout_overflowed) = out_rx
        .recv_timeout(PIPE_GRACE)
        .map_err(|_| SafetyError::Scanner(format!("{what} exited but its output stayed open")))?;
    let (stderr, _) = err_rx.recv_timeout(PIPE_GRACE).unwrap_or_default();
    Ok(Ran {
        status: status.code(),
        stdout,
        stdout_overflowed,
        stderr: String::from_utf8_lossy(&stderr).trim().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scanner_never_sees_the_source_variables_or_gitleaks_overrides() {
        for name in crate::child_env::SCRUBBED {
            std::env::set_var(name, "planted-for-the-test");
        }
        let argv = crate::child_env::env_printer();
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]);
        let ran = run(&mut cmd, "env", Duration::from_secs(20), 1024 * 1024).unwrap();
        let text = String::from_utf8_lossy(&ran.stdout);
        assert_eq!(
            crate::child_env::names_a_scrubbed_variable(&text),
            None,
            "{text}"
        );
    }
}
