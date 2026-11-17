//! Output shown with the instances' generated secrets hidden: `follow`
//! (a process component's log file) and `follow-command` (an engine's
//! `logs -f`), the two programs behind a component's `logs` terminal.
//!
//! A component's output can print what its env holds (a connection string
//! at startup), and a generated password is hidden there like everywhere
//! else. Output is passed on a line at a time, each line with every value
//! of [`stack_core::instance_secrets::existing`] shown as `••••`; the values
//! are read again for every chunk, so one generated after the terminal
//! opened is hidden too. A line still open when the writer pauses is held
//! for one more read and then passed on as it stands, so a prompt without a
//! newline still shows.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use stack_core::instance_secrets::existing;

/// Turns raw output into what is shown: complete lines, hidden.
pub struct LineHider {
    work_dir: PathBuf,
    pending: Vec<u8>,
    /// The open line was already held back once.
    held: bool,
}

impl LineHider {
    pub fn new(work_dir: &Path) -> Self {
        LineHider {
            work_dir: work_dir.to_path_buf(),
            pending: Vec::new(),
            held: false,
        }
    }

    fn hide(&self, bytes: &[u8]) -> Vec<u8> {
        let secrets = existing(&self.work_dir);
        if secrets.is_empty() {
            return bytes.to_vec();
        }
        let text = String::from_utf8_lossy(bytes);
        if secrets.found_in(&text) {
            secrets.hide(&text).into_bytes()
        } else {
            bytes.to_vec()
        }
    }

    /// `chunk` read; what can be shown now (every complete line).
    pub fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.pending.extend_from_slice(chunk);
        self.held = false;
        match self.pending.iter().rposition(|b| *b == b'\n') {
            Some(i) => {
                let rest = self.pending.split_off(i + 1);
                let lines = std::mem::replace(&mut self.pending, rest);
                self.hide(&lines)
            }
            None => Vec::new(),
        }
    }

    /// Nothing new was read: the open line goes out on the second time in
    /// a row.
    pub fn idle(&mut self) -> Vec<u8> {
        if self.pending.is_empty() {
            return Vec::new();
        }
        if !self.held {
            self.held = true;
            return Vec::new();
        }
        let open = std::mem::take(&mut self.pending);
        self.held = false;
        self.hide(&open)
    }

    /// The end of the output: whatever is left.
    pub fn finish(&mut self) -> Vec<u8> {
        let open = std::mem::take(&mut self.pending);
        if open.is_empty() {
            Vec::new()
        } else {
            self.hide(&open)
        }
    }
}

/// Run `argv` with its stdout and stderr passed to `out` a line at a time,
/// hidden. The exit code is the command's (1 when it cannot start; 1 also
/// when `out` goes away, after which the command is stopped).
pub fn follow_command(argv: &[String], work_dir: &Path, out: Arc<Mutex<dyn Write + Send>>) -> i32 {
    let Some((program, args)) = argv.split_first() else {
        return 2;
    };
    let mut cmd = Command::new(program);
    stack_core::child_env::scrub(&mut cmd);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let mut o = out.lock().unwrap_or_else(|p| p.into_inner());
            let _ = writeln!(o, "chappa-stack: cannot start {program}: {e}");
            return 1;
        }
    };
    let broken = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let pump = |stream: Box<dyn Read + Send>| {
        let out = out.clone();
        let broken = broken.clone();
        let work_dir = work_dir.to_path_buf();
        std::thread::spawn(move || {
            let hider = LineHider::new(&work_dir);
            let mut reader = BufReader::new(stream);
            let mut line = Vec::new();
            loop {
                line.clear();
                match reader.read_until(b'\n', &mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let shown = hider.hide(&line);
                        let mut o = out.lock().unwrap_or_else(|p| p.into_inner());
                        if o.write_all(&shown).and_then(|_| o.flush()).is_err() {
                            broken.store(true, std::sync::atomic::Ordering::Relaxed);
                            break;
                        }
                    }
                }
            }
        })
    };
    let stdout = child.stdout.take().map(|s| pump(Box::new(s)));
    let stderr = child.stderr.take().map(|s| pump(Box::new(s)));
    let status = loop {
        if broken.load(std::sync::atomic::Ordering::Relaxed) {
            let _ = child.kill();
        }
        match child.try_wait() {
            Ok(Some(s)) => break s.code().unwrap_or(1),
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(100)),
            Err(_) => break 1,
        }
    };
    for t in [stdout, stderr].into_iter().flatten() {
        let _ = t.join();
    }
    if broken.load(std::sync::atomic::Ordering::Relaxed) {
        1
    } else {
        status
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stack_core::instance_secrets::load_or_create;

    #[test]
    fn complete_lines_go_out_hidden_and_an_open_line_after_one_idle_read() {
        let tmp = tempfile::tempdir().unwrap();
        let names = ["db".to_string()].into();
        let v = load_or_create(tmp.path(), "app-0", &names)
            .unwrap()
            .get("db")
            .unwrap()
            .to_owned();
        let mut h = LineHider::new(tmp.path());
        // A value split over two reads is still hidden: the line waits.
        let (a, b) = v.split_at(10);
        assert!(h
            .push(format!("url=postgres://app:{a}").as_bytes())
            .is_empty());
        let out = h.push(format!("{b}@db/app\nnext ").as_bytes());
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "url=postgres://app:••••@db/app\n"
        );
        assert!(h.idle().is_empty(), "held once");
        assert_eq!(h.idle(), b"next ");
        assert!(h.finish().is_empty());
        h.push(format!("tail {v}").as_bytes());
        assert_eq!(String::from_utf8(h.finish()).unwrap(), "tail ••••");
    }
}
