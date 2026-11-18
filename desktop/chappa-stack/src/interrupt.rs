//! Ctrl+C in a job's terminal: cancel the job, the same way the Cancel
//! action does, and say so in the terminal.
//!
//! `run-job` handles the interrupt itself instead of dying (a console
//! control handler for Ctrl+C and Ctrl+Break on Windows, `SIGINT`
//! elsewhere):
//!
//! 1. the first press writes the job's cancel request (`<id>.cancel`, what
//!    the Cancel action writes) and sets the job's [`Cancel`], so the step
//!    running then is stopped with every process it started, the rest are
//!    skipped, the job ends `cancelled` and its lease is let go once the
//!    tree is gone. A cancel already asked for by the Cancel action counts
//!    as this press;
//! 2. a press while that cancel is under way only says so, and opens a
//!    window of [`AGAIN_WITHIN`];
//! 3. a press inside that window ends the runner at once
//!    ([`EXIT_INTERRUPTED`]): its locks go with the process, and whoever
//!    looks next settles the job as `interrupted`. A press after the window
//!    closed counts as a second one again.
//!
//! Once the job's end is written, a press does nothing; a press before the
//! job was looked at (the runner still waiting for its lock) ends the
//! runner, as Ctrl+C always did.
//!
//! **Who sees the Ctrl+C.** A terminal delivers it to every process on its
//! console (Windows) or in its foreground process group (Unix), which
//! would include the step's own processes: they would stop half-way on
//! their own while the runner's cancel is still on its way. So every step
//! command, and every quick engine or git command the runner runs, starts
//! outside that reach: in a new console process group on Windows (which
//! starts with Ctrl+C ignored, inherited by everything it starts; the job
//! object still ends the tree), in its own process group on Unix. Ctrl+C
//! then reaches only the runner. On Windows a Ctrl+Break still reaches
//! every process on the console; the runner treats it as a Ctrl+C.
//!
//! On Unix a step outside the terminal's process group no longer gets the
//! terminal's hangup either, so `SIGHUP` (the pane closed) ends the step's
//! whole process group and then the runner, as closing the pane ends a
//! step's job object on Windows.

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::lease;
use crate::world::Cancel;

/// How long after a second press a third one ends the runner.
pub const AGAIN_WITHIN: Duration = Duration::from_secs(3);

/// The exit code of a runner ended by Ctrl+C while it was cancelling
/// (128 + `SIGINT`, as a shell reports it).
pub const EXIT_INTERRUPTED: i32 = 130;

/// What a press during a cancel says.
pub const ALREADY_CANCELLING: &str = "cancel already in progress — press Ctrl+C again within 3 s to stop the runner now (the stack may be left half-changed)";

/// What the press that ends the runner says.
pub const STOPPING_NOW: &str =
    "^C — stopping the runner now; the job will show as interrupted and the stack may be left half-changed";

/// What one press means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Press {
    /// Cancel the job.
    Cancel,
    /// A cancel is under way; say so.
    AlreadyCancelling,
    /// End the runner now.
    StopNow,
    /// The job has ended; nothing to do.
    Ignored,
}

/// The presses of one job, judged by the time each came at.
#[derive(Debug, Default)]
pub struct Presses {
    cancelling: bool,
    /// When the window for the press that ends the runner opened.
    armed: Option<Instant>,
    ended: bool,
}

impl Presses {
    /// One press at `now`; `cancel_requested` is whether the job was asked
    /// to stop some other way already.
    pub fn press(&mut self, now: Instant, cancel_requested: bool) -> Press {
        if self.ended {
            return Press::Ignored;
        }
        if !self.cancelling && !cancel_requested {
            self.cancelling = true;
            return Press::Cancel;
        }
        self.cancelling = true;
        match self.armed {
            Some(at) if now.saturating_duration_since(at) <= AGAIN_WITHIN => {
                self.ended = true;
                Press::StopNow
            }
            _ => {
                self.armed = Some(now);
                Press::AlreadyCancelling
            }
        }
    }

    /// The job's end is written: presses do nothing from now on.
    pub fn end(&mut self) {
        self.ended = true;
    }
}

/// The job a runner is running, as a press needs it.
pub struct Job {
    pub work_dir: PathBuf,
    pub id: String,
    pub label: String,
    pub cancel: Cancel,
    /// The job's log (a handle sharing the runner's), so the lines a press
    /// prints are in it too.
    pub log: Option<File>,
}

struct Active {
    job: Job,
    /// The step running now: its number, how many there are, what it is.
    step: Option<(usize, usize, String)>,
}

/// Turns presses into a cancel, a notice or the end of the runner, for the
/// one job this process runs.
pub struct Interrupts {
    presses: Mutex<Presses>,
    active: Mutex<Option<Active>>,
    pane: Mutex<Box<dyn Write + Send>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl Interrupts {
    /// Presses whose lines go to `pane` (the job's terminal).
    pub fn new(pane: Box<dyn Write + Send>) -> Interrupts {
        Interrupts {
            presses: Mutex::new(Presses::default()),
            active: Mutex::new(None),
            pane: Mutex::new(pane),
        }
    }

    /// The runner holds `job` and will run it.
    pub fn begin(&self, job: Job) {
        *lock(&self.active) = Some(Active { job, step: None });
    }

    /// Step `at` of `of` (`what`) runs now; `None` between steps.
    pub fn step(&self, now: Option<(usize, usize, &str)>) {
        if let Some(a) = lock(&self.active).as_mut() {
            a.step = now.map(|(at, of, what)| (at, of, what.to_string()));
        }
    }

    /// The job's end is written.
    pub fn end(&self) {
        lock(&self.presses).end();
        *lock(&self.active) = None;
    }

    /// One press at `now`: what it meant, with its line printed and, for
    /// the first, the cancel asked for. Ending the runner is the caller's.
    pub fn interrupt(&self, now: Instant) -> Press {
        let mut presses = lock(&self.presses);
        let mut active = lock(&self.active);
        let Some(a) = active.as_mut() else {
            // Not begun: nothing was looked at yet, so stop as Ctrl+C
            // always stopped a program. Ended: the job is over.
            return if presses.ended {
                Press::Ignored
            } else {
                Press::StopNow
            };
        };
        let requested =
            a.job.cancel.is_set() || lease::cancel_requested(&a.job.work_dir, &a.job.id).is_some();
        let press = presses.press(now, requested);
        match press {
            Press::Cancel => {
                let what = match &a.step {
                    Some((at, of, what)) => {
                        format!("stopping step {at}/{of} ({what}) and everything it started…")
                    }
                    None => "stopping before the next step…".to_string(),
                };
                let line = format!("^C — cancelling \"{}\": {what}", a.job.label);
                self.say(&mut a.job, &line);
                let _ = lease::request_cancel(
                    &a.job.work_dir,
                    &a.job.id,
                    None,
                    &stack_core::intent::utc_now(),
                );
                a.job.cancel.set();
            }
            Press::AlreadyCancelling => self.say(&mut a.job, ALREADY_CANCELLING),
            Press::StopNow => self.say(&mut a.job, STOPPING_NOW),
            Press::Ignored => {}
        }
        press
    }

    /// A line in the pane and the log, with the instance's generated values
    /// hidden like every other job line. The pane's line starts by going
    /// back to the start of the line, over the `^C` a terminal may echo.
    fn say(&self, job: &mut Job, text: &str) {
        let shown = stack_core::instance_secrets::existing(&job.work_dir).hide(text);
        let mut pane = lock(&self.pane);
        let _ = pane.write_all(format!("\r{shown}\n").as_bytes());
        let _ = pane.flush();
        if let Some(f) = &mut job.log {
            let _ = f.write_all(format!("{shown}\n").as_bytes());
        }
    }
}

static INTERRUPTS: OnceLock<Interrupts> = OnceLock::new();

/// Handle Ctrl+C in this process from now on, its lines going to stdout
/// (the job's terminal). `None` when the handler could not be installed
/// (Ctrl+C then ends the runner as before).
pub fn install() -> Option<&'static Interrupts> {
    let ints = INTERRUPTS.get_or_init(|| Interrupts::new(Box::new(std::io::stdout())));
    os::install().then_some(ints)
}

/// A press, from the system's handler.
fn deliver() {
    if let Some(ints) = INTERRUPTS.get() {
        if ints.interrupt(Instant::now()) == Press::StopNow {
            std::process::exit(EXIT_INTERRUPTED);
        }
    }
}

/// The step command running now, on Unix: its process group is what a
/// hangup ends.
#[cfg(unix)]
static STEP_GROUP: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// A step command that leads its own process group runs while this is
/// held.
pub struct StepGroup(());

impl StepGroup {
    pub fn started(pid: u32) -> StepGroup {
        #[cfg(unix)]
        STEP_GROUP.store(pid as i32, std::sync::atomic::Ordering::SeqCst);
        #[cfg(not(unix))]
        let _ = pid;
        StepGroup(())
    }
}

impl Drop for StepGroup {
    fn drop(&mut self) {
        #[cfg(unix)]
        STEP_GROUP.store(0, std::sync::atomic::Ordering::SeqCst);
    }
}

#[cfg(windows)]
mod os {
    use windows_sys::core::BOOL;
    use windows_sys::Win32::System::Console::{
        SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_C_EVENT,
    };

    unsafe extern "system" fn on_console_event(kind: u32) -> BOOL {
        if kind == CTRL_C_EVENT || kind == CTRL_BREAK_EVENT {
            super::deliver();
            1
        } else {
            // Closing the console: the default (end the process; the
            // step's job object ends its tree).
            0
        }
    }

    pub fn install() -> bool {
        // SAFETY: plain calls; the routine is a `'static` function. The
        // first undoes an ignore-Ctrl+C this process may have inherited.
        unsafe {
            SetConsoleCtrlHandler(None, 0);
            SetConsoleCtrlHandler(Some(on_console_event), 1) != 0
        }
    }
}

#[cfg(unix)]
mod os {
    use std::sync::atomic::{AtomicI32, Ordering};

    /// The write end of the pipe the signal handler wakes the reader with.
    static WAKE: AtomicI32 = AtomicI32::new(-1);

    extern "C" fn on_signal(sig: libc::c_int) {
        let fd = WAKE.load(Ordering::SeqCst);
        if fd >= 0 {
            let b = sig as u8;
            // SAFETY: write(2) is async-signal-safe; one byte from a local.
            unsafe { libc::write(fd, &b as *const u8 as *const libc::c_void, 1) };
        }
    }

    pub fn install() -> bool {
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: `fds` has room for the two descriptors.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return false;
        }
        for fd in fds {
            // SAFETY: descriptors just created; no child may inherit them.
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        }
        WAKE.store(fds[1], Ordering::SeqCst);
        let read = fds[0];
        let reader = std::thread::Builder::new()
            .name("interrupts".into())
            .spawn(move || loop {
                let mut b = 0u8;
                // SAFETY: reads one byte into a local.
                let n = unsafe { libc::read(read, &mut b as *mut u8 as *mut libc::c_void, 1) };
                if n == 1 {
                    match b as libc::c_int {
                        libc::SIGINT => super::deliver(),
                        libc::SIGHUP => hang_up(),
                        _ => {}
                    }
                } else if n < 0
                    && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
                {
                    continue;
                } else {
                    return;
                }
            });
        if reader.is_err() {
            return false;
        }
        for sig in [libc::SIGINT, libc::SIGHUP] {
            // SAFETY: a zeroed sigaction with our handler and an empty mask.
            unsafe {
                let mut sa: libc::sigaction = std::mem::zeroed();
                sa.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as usize;
                sa.sa_flags = libc::SA_RESTART;
                libc::sigemptyset(&mut sa.sa_mask);
                if libc::sigaction(sig, &sa, std::ptr::null_mut()) != 0 {
                    return false;
                }
            }
        }
        true
    }

    /// The pane went away: end the running step's process group, then the
    /// runner (its locks go with it; the job settles as `interrupted`).
    fn hang_up() {
        let group = super::STEP_GROUP.load(Ordering::SeqCst);
        if group > 0 {
            // SAFETY: a signal to a process group this runner started.
            unsafe { libc::kill(-group, libc::SIGKILL) };
        }
        std::process::exit(128 + libc::SIGHUP);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn the_first_press_cancels_a_second_warns_and_a_third_within_the_window_stops() {
        let t0 = Instant::now();
        let mut p = Presses::default();
        assert_eq!(p.press(t0, false), Press::Cancel);
        assert_eq!(
            p.press(t0 + Duration::from_millis(500), false),
            Press::AlreadyCancelling
        );
        assert_eq!(p.press(t0 + Duration::from_secs(3), false), Press::StopNow);
        // Stopped: nothing after that means anything.
        assert_eq!(p.press(t0 + Duration::from_secs(4), false), Press::Ignored);
    }

    #[test]
    fn the_window_opens_at_the_second_press_and_its_edge_still_counts() {
        let t0 = Instant::now();
        let mut p = Presses::default();
        assert_eq!(p.press(t0, false), Press::Cancel);
        // Long after the first: still only the warning, which opens it.
        let second = t0 + Duration::from_secs(60);
        assert_eq!(p.press(second, false), Press::AlreadyCancelling);
        assert_eq!(p.press(second + AGAIN_WITHIN, false), Press::StopNow);
    }

    #[test]
    fn a_press_after_the_window_closed_warns_again_and_opens_a_new_one() {
        let t0 = Instant::now();
        let mut p = Presses::default();
        p.press(t0, false);
        let second = t0 + Duration::from_secs(1);
        assert_eq!(p.press(second, false), Press::AlreadyCancelling);
        let late = second + AGAIN_WITHIN + Duration::from_millis(1);
        assert_eq!(p.press(late, false), Press::AlreadyCancelling);
        assert_eq!(
            p.press(late + Duration::from_secs(2), false),
            Press::StopNow
        );
    }

    #[test]
    fn a_cancel_asked_for_elsewhere_makes_the_first_press_the_warning() {
        let t0 = Instant::now();
        let mut p = Presses::default();
        assert_eq!(p.press(t0, true), Press::AlreadyCancelling);
        assert_eq!(p.press(t0 + Duration::from_secs(1), true), Press::StopNow);
    }

    #[test]
    fn presses_after_the_end_do_nothing() {
        let t0 = Instant::now();
        let mut p = Presses::default();
        p.end();
        assert_eq!(p.press(t0, false), Press::Ignored);
        assert_eq!(p.press(t0, false), Press::Ignored);
    }

    /// A pane that the test can read while the interrupts hold it.
    #[derive(Clone, Default)]
    struct Pane(Arc<Mutex<Vec<u8>>>);

    impl Write for Pane {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Pane {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    #[test]
    fn a_press_asks_for_the_cancel_the_action_asks_for_and_says_what_it_stops() {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join(".work");
        let log = tmp.path().join("job.log");
        let pane = Pane::default();
        let ints = Interrupts::new(Box::new(pane.clone()));
        let t0 = Instant::now();
        // Before the job is looked at, Ctrl+C ends the runner as it always did.
        assert_eq!(ints.interrupt(t0), Press::StopNow);
        let cancel = Cancel::new();
        ints.begin(Job {
            work_dir: work.clone(),
            id: "j1".into(),
            label: "local: bring up".into(),
            cancel: cancel.clone(),
            log: Some(File::create(&log).unwrap()),
        });
        ints.step(Some((2, 3, "app: up script")));
        assert_eq!(ints.interrupt(t0), Press::Cancel);
        assert!(cancel.is_set());
        let req = lease::cancel_requested(&work, "j1").expect("the cancel request");
        assert_eq!(lease::cancelled_text(Some(&req)), "cancelled");
        assert_eq!(
            ints.interrupt(t0 + Duration::from_secs(1)),
            Press::AlreadyCancelling
        );
        assert_eq!(ints.interrupt(t0 + Duration::from_secs(2)), Press::StopNow);
        let first = "^C — cancelling \"local: bring up\": stopping step 2/3 (app: up script) and everything it started…";
        assert_eq!(
            pane.text(),
            format!("\r{first}\n\r{ALREADY_CANCELLING}\n\r{STOPPING_NOW}\n")
        );
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            format!("{first}\n{ALREADY_CANCELLING}\n{STOPPING_NOW}\n")
        );
    }

    #[test]
    fn between_steps_a_press_says_nothing_more_starts_and_after_the_end_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let pane = Pane::default();
        let ints = Interrupts::new(Box::new(pane.clone()));
        ints.begin(Job {
            work_dir: tmp.path().to_path_buf(),
            id: "j2".into(),
            label: "local: tear down".into(),
            cancel: Cancel::new(),
            log: None,
        });
        ints.step(Some((1, 2, "x")));
        ints.step(None);
        assert_eq!(ints.interrupt(Instant::now()), Press::Cancel);
        assert_eq!(
            pane.text(),
            "\r^C — cancelling \"local: tear down\": stopping before the next step…\n"
        );
        ints.end();
        assert_eq!(ints.interrupt(Instant::now()), Press::Ignored);
        assert_eq!(ints.interrupt(Instant::now()), Press::Ignored);
    }
}
