//! PtySession over portable-pty: the small spawn/wait/resize surface the
//! actor consumes. Kept deliberately narrow — no terminal parsing
//! lives here, and the module is platform-agnostic: portable-pty picks
//! openpty on Linux and ConPTY on Windows, so the actor never needs cfg
//! noise for spawn/kill/resize.
//!
//! Ownership rules that matter:
//! - `Drop` kills and reaps the child, so a dropped session never leaves a
//!   running process or a zombie behind (verified in tests/pty.rs).
//! - The PTY writer is shared behind a mutex so actor input and
//!   `Event::PtyWrite` loopback can interleave from any thread.
//! - The reader is handed out exactly once (`take_reader`), matching the
//!   single-reader actor design.

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

pub type Result<T> = io::Result<T>;

/// Everything needed to spawn one pty child. `command` is the program
/// only; `args` are argv[1..].
#[derive(Debug, Clone, Default)]
pub struct PtySpec {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    /// Added to the inherited environment. Applied last, so it overrides
    /// both inherited variables and the TERM/COLORTERM defaults.
    pub env: Vec<(String, String)>,
    pub cols: u16,
    pub rows: u16,
}

/// Process exit information observed from the pty child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitStatus {
    /// Exit code, or `None` when terminated by a signal.
    pub code: Option<i32>,
    pub success: bool,
}

impl ExitStatus {
    fn from_portable(status: portable_pty::ExitStatus) -> Self {
        let code = match status.signal() {
            Some(_) => None,
            None => Some(status.exit_code() as i32),
        };
        Self {
            code,
            success: status.success(),
        }
    }
}

/// A live pty child. Holds the master end, the child handle and a shared
/// writer; the reader is pre-cloned at spawn and surrendered once via
/// [`PtySession::take_reader`].
pub struct PtySession {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    reader: Option<Box<dyn Read + Send>>,
}

impl PtySession {
    /// Spawns a child into a fresh pty. Always injects TERM=xterm-256color
    /// and COLORTERM=truecolor; `spec.env` entries win over both those and
    /// the inherited environment. Fails loudly (Err carrying the OS
    /// message) rather than returning a half-open session.
    pub fn spawn(spec: &PtySpec) -> Result<Self> {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: spec.rows,
                cols: spec.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(to_io_err)?;

        let mut cmd = CommandBuilder::new(&spec.command);
        cmd.args(&spec.args);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        for (key, value) in &spec.env {
            cmd.env(key, value);
        }
        if let Some(cwd) = &spec.cwd {
            cmd.cwd(cwd);
        }

        let child = pair.slave.spawn_command(cmd).map_err(to_io_err)?;
        drop(pair.slave);

        let reader = pair.master.try_clone_reader().map_err(to_io_err)?;
        let writer = pair.master.take_writer().map_err(to_io_err)?;

        Ok(Self {
            master: pair.master,
            child,
            writer: Arc::new(Mutex::new(writer)),
            reader: Some(reader),
        })
    }

    /// Blocking reader for the actor thread. Returns an error on a second
    /// call — the actor is the only consumer of the output stream.
    pub fn take_reader(&mut self) -> Result<Box<dyn Read + Send>> {
        self.reader
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "pty reader already taken"))
    }

    /// A writer usable from any thread; all returned handles share the one
    /// pty writer behind a mutex. Dropping every handle closes the writer
    /// (EOF to the child on Windows).
    pub fn writer(&self) -> Result<Box<dyn Write + Send>> {
        Ok(Box::new(SharedWriter {
            inner: self.writer.clone(),
        }))
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(to_io_err)
    }

    /// OS process id of the pty child, or `None` when the platform did not
    /// hand one back. The stats poller walks the child tree from this
    /// pid; nothing else in term-core reads it (the actor drives the child
    /// through the handle, never by pid).
    pub fn process_id(&self) -> Option<u32> {
        self.child.process_id()
    }

    /// Non-blocking: `Some(status)` once the child has exited.
    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
        self.child
            .try_wait()
            .map(|s| s.map(ExitStatus::from_portable))
    }

    /// Blocks until the child exits. Use from a dedicated watcher thread.
    pub fn wait(&mut self) -> Result<ExitStatus> {
        self.child.wait().map(ExitStatus::from_portable)
    }

    /// Hard-kill the child (process tree on Windows via ConPTY; on unix the
    /// child is a session leader, so portable-pty's SIGHUP-then-SIGKILL
    /// takes the session down). Follow with `wait` to reap.
    pub fn kill(&mut self) -> Result<()> {
        self.child.kill()
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        // Already exited and reaped: nothing to do.
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            return;
        }
        // Kill the child, then wait to reap it so no zombie is left behind.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A `Write` handle that shares the pty writer across threads.
struct SharedWriter {
    inner: Arc<Mutex<Box<dyn Write + Send>>>,
}

impl Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.lock().unwrap().write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.lock().unwrap().flush()
    }
}

/// portable-pty reports errors as anyhow; carry the message (including any
/// underlying OS error text) into a plain io::Error. This stays generic so
/// the crate never has to depend on anyhow directly.
fn to_io_err<E: std::fmt::Display>(err: E) -> io::Error {
    io::Error::new(io::ErrorKind::Other, format!("{err:#}"))
}
