//! `restart_when_changed` file watcher (notify crate, 500ms
//! debounce → restart).
//!
//! Watches the project root recursively, filters events through the process's
//! glob patterns (matched RELATIVE to the project root), and emits one restart
//! signal per change-burst: matching events reset the debounce window, so a
//! save that touches several files (or fires several inotify events) restarts
//! the process exactly once.
//!
//! Invalid glob patterns are skipped at construction (ignored, never
//! fail the config).

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use globset::{Glob, GlobSet, GlobSetBuilder};
use log::warn;
use notify::{Event, EventKind, RecommendedWatcher, Watcher};
pub use notify::RecursiveMode;

/// Changes burst together; restart at most once per burst, 500ms after the
/// LAST matching event.
pub const WATCHER_DEBOUNCE: Duration = Duration::from_millis(500);

/// Pure glob matcher: does an event path match any of the process's patterns,
/// relative to the project root? Built once per process at spawn time.
#[derive(Debug, Clone)]
pub struct ChangeMatcher {
    root: PathBuf,
    set: GlobSet,
    patterns: Vec<String>,
    /// Restart semantics ([`new`](Self::new)): chappa-ai's own yml artifacts —
    /// the project file included — never count as a source change. The project
    /// file reload watcher ([`for_chappa_yml`](Self::for_chappa_yml)) exists to
    /// see exactly that file, so it turns this off (found live 2026-08-31: the
    /// exclusion silently ate every event the yml watcher was armed for, and
    /// external project-file edits never hot-reloaded).
    skip_own_yml: bool,
}

impl ChangeMatcher {
    pub fn new(root: PathBuf, patterns: &[String]) -> Self {
        let mut builder = GlobSetBuilder::new();
        let mut kept = Vec::new();
        for p in patterns {
            match Glob::new(p) {
                Ok(glob) => {
                    builder.add(glob);
                    kept.push(p.clone());
                }
                Err(_) => warn!("restart_when_changed: ignoring invalid glob `{p}`"),
            }
        }
        let set = builder.build().unwrap_or_default();
        Self {
            root,
            set,
            patterns: kept,
            skip_own_yml: true,
        }
    }

    /// The project-file reload watcher's matcher: matches `chappa.yml` in
    /// the project root and nothing else (the tmp/bak artifacts fail the glob,
    /// so the restart-side exclusion list is not needed — and its project-file
    /// entry must not apply here).
    pub fn for_chappa_yml(root: PathBuf) -> Self {
        let mut m = Self::new(root, &[crate::yml::CHAPPA_YML_NAME.to_owned()]);
        m.skip_own_yml = false;
        m
    }

    /// The patterns that compiled successfully (empty when none did).
    pub fn patterns(&self) -> &[String] {
        &self.patterns
    }

    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// True when `path` (absolute, from notify) matches a pattern relative to
    /// the project root.
    pub fn matches(&self, path: &Path) -> bool {
        // Chappa-ai's own write-back burst — the temp file,
        // the one-time backup, and the rename onto chappa.yml — must never
        // count as a source change. A `*`/`*.yml` glob would otherwise
        // restart every process on any edit of any OTHER process. chappa.yml
        // edits are handled by the reload diff, not by these watchers.
        if self.skip_own_yml
            && path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(is_own_yml_artifact)
        {
            return false;
        }
        match path.strip_prefix(&self.root) {
            Ok(rel) => self.set.is_match(rel),
            Err(_) => false,
        }
    }
}

/// The project-file names chappa-ai itself writes (`yml.rs::write` and
/// `writeback::BACKUP_NAME`): excluded from restart_when_changed matching.
fn is_own_yml_artifact(name: &str) -> bool {
    let own = crate::yml::CHAPPA_YML_NAME;
    name == own
        || name == format!("{own}.chappa-tmp").as_str()
        || name == format!("{own}.chappa-bak").as_str()
}

/// A running watcher. Dropping it stops the notify watcher (the debounce
/// thread exits when its channel closes). The restart signals arrive on the
/// `Receiver` returned by [`RestartWatcher::start`].
pub struct RestartWatcher {
    _watcher: Option<RecommendedWatcher>,
    /// Keeps the restart channel's sender alive for the no-patterns case
    /// (nothing to watch → the receiver simply never fires).
    _keep_alive: Option<Sender<()>>,
    handle: Option<JoinHandle<()>>,
}

impl RestartWatcher {
    /// Watch `root` recursively for changes matching `matcher`. Returns the
    /// watcher handle plus a channel that receives `()` once per debounced
    /// change-burst.
    pub fn start(root: PathBuf, matcher: ChangeMatcher) -> Result<(Self, Receiver<()>), String> {
        Self::start_with_mode(root, matcher, RecursiveMode::Recursive)
    }

    /// [`start`](Self::start) with an explicit recursion mode. The
    /// project-file watcher only cares about ONE file in the project root, so it
    /// watches non-recursively — a second recursive walk of a large repo per
    /// open project would be pure waste.
    pub fn start_with_mode(
        root: PathBuf,
        matcher: ChangeMatcher,
        mode: RecursiveMode,
    ) -> Result<(Self, Receiver<()>), String> {
        if matcher.is_empty() {
            // Nothing to watch — return a channel that never fires rather than
            // a watcher that would restart on every file in the tree.
            let (tx, rx) = channel();
            return Ok((
                Self {
                    _watcher: None,
                    _keep_alive: Some(tx),
                    handle: None,
                },
                rx,
            ));
        }

        let (ev_tx, ev_rx) = channel();
        let mut watcher = notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
            let _ = ev_tx.send(res);
        })
        .map_err(|e| e.to_string())?;
        watcher.watch(&root, mode).map_err(|e| e.to_string())?;

        let (out_tx, out_rx) = channel();
        let handle = thread::spawn(move || run_debounce(ev_rx, matcher, out_tx));
        Ok((
            Self {
                _watcher: Some(watcher),
                _keep_alive: None,
                handle: Some(handle),
            },
            out_rx,
        ))
    }
}

impl Drop for RestartWatcher {
    fn drop(&mut self) {
        // Drop the notify watcher BEFORE joining the debounce thread: the
        // watcher's closure owns the event-channel sender, so until it is
        // dropped the thread can never observe Disconnected and exit.
        self._watcher = None;
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Debounce loop: drain notify events, note the latest matching change time,
/// and emit once per burst 500ms after the last one.
fn run_debounce(
    ev_rx: Receiver<Result<Event, notify::Error>>,
    matcher: ChangeMatcher,
    out: Sender<()>,
) {
    let mut last_change: Option<Instant> = None;
    let mut sent = false;
    loop {
        // Drain whatever notify delivered since the last pass.
        let mut disconnected = false;
        loop {
            match ev_rx.try_recv() {
                Ok(Ok(event)) => {
                    let relevant = matches!(
                        event.kind,
                        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                    );
                    if relevant && event.paths.iter().any(|p| matcher.matches(p)) {
                        last_change = Some(Instant::now());
                        sent = false;
                    }
                }
                Ok(Err(_)) => { /* transient notify error: keep watching */ }
                Err(TryRecvError::Disconnected) => {
                    // The owning RestartWatcher dropped the notify watcher.
                    disconnected = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
            }
        }
        if disconnected {
            return;
        }
        if let Some(t) = last_change {
            if !sent && t.elapsed() >= WATCHER_DEBOUNCE {
                if out.send(()).is_err() {
                    return; // consumer dropped the receiver
                }
                sent = true;
            }
            // A consumed burst is forgotten once fully past the window, so a
            // LATER burst (beyond the debounce) restarts again.
            if sent && t.elapsed() >= WATCHER_DEBOUNCE {
                last_change = None;
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::fs;

    fn wait_for(signal: &Receiver<()>) -> bool {
        signal
            .recv_timeout(Duration::from_secs(5))
            .map(|_| true)
            .unwrap_or(false)
    }

    #[test]
    fn matcher_matches_relative_to_root_with_recursive_globs() {
        let root = tempfile::tempdir().unwrap();
        let m = ChangeMatcher::new(
            root.path().to_path_buf(),
            &["app/tasks/**/*.md".into(), "*.toml".into()],
        );
        assert!(m.matches(&root.path().join("app/tasks/09_x.md")));
        assert!(m.matches(&root.path().join("app/tasks/deep/9.md")));
        assert!(m.matches(&root.path().join("Cargo.toml")));
        assert!(!m.matches(&root.path().join("app/main.py")));
        assert!(!m.matches(&root.path().join("tasks/09_x.md")));
        // Paths outside the root never match.
        assert!(!m.matches(&tempfile::tempdir().unwrap().path().join("app/tasks/a.md")));
    }

    #[test]
    fn invalid_globs_are_skipped_not_fatal() {
        let root = tempfile::tempdir().unwrap();
        let m = ChangeMatcher::new(
            root.path().to_path_buf(),
            &["[".into(), "app/**/*.py".into()],
        );
        assert_eq!(m.patterns(), &["app/**/*.py"]);
        assert!(m.matches(&root.path().join("app/x.py")));
    }

    #[test]
    fn empty_patterns_never_restart() {
        let root = tempfile::tempdir().unwrap();
        let (_watcher, rx) = RestartWatcher::start(
            root.path().to_path_buf(),
            ChangeMatcher::new(root.path().to_path_buf(), &[]),
        )
        .unwrap();
        let file = root.path().join("anything");
        fs::write(&file, "x").unwrap();
        assert!(!wait_for(&rx), "no patterns → no restart signal, ever");
    }

    #[test]
    fn debounce_coalesces_a_burst_into_one_restart() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("src");
        fs::create_dir(&dir).unwrap();
        let matcher = ChangeMatcher::new(root.path().to_path_buf(), &["src/**/*.txt".into()]);
        let (_watcher, rx) = RestartWatcher::start(root.path().to_path_buf(), matcher).unwrap();

        let file = dir.join("watchme.txt");
        // A burst of writes (a save = several events) within the debounce
        // window must produce EXACTLY one restart signal.
        for i in 0..3 {
            fs::write(&file, format!("{i}")).unwrap();
            thread::sleep(Duration::from_millis(80));
        }
        assert!(wait_for(&rx), "the burst must trigger a restart");
        // No second signal for the SAME burst.
        assert!(
            !rx.recv_timeout(Duration::from_millis(600)).is_ok(),
            "one burst → exactly one restart"
        );

        // A LATER change (beyond the window) restarts again.
        fs::write(&file, "later").unwrap();
        assert!(wait_for(&rx), "a later change restarts again");
    }

    #[test]
    fn non_matching_changes_never_restart() {
        let root = tempfile::tempdir().unwrap();
        let matcher = ChangeMatcher::new(root.path().to_path_buf(), &["**/*.md".into()]);
        let (_watcher, rx) = RestartWatcher::start(root.path().to_path_buf(), matcher).unwrap();

        fs::write(root.path().join("main.py"), "print(1)").unwrap();
        thread::sleep(Duration::from_millis(700));
        assert!(
            !rx.try_recv().is_ok(),
            "a .py change must not restart a **/*.md watcher"
        );
    }

    /// The write-back burst never matches, even under `*`.
    #[test]
    fn own_yml_artifacts_never_match_restart_globs() {
        let root = PathBuf::from("/p");
        let m = ChangeMatcher::new(root.clone(), &["*".to_owned(), "*.yml".to_owned()]);
        let own = crate::yml::CHAPPA_YML_NAME;
        let tmp = format!("{own}.chappa-tmp");
        let bak = format!("{own}.chappa-bak");
        for artifact in [own, tmp.as_str(), bak.as_str()] {
            assert!(!m.matches(&root.join(artifact)), "{artifact} must not restart anything");
        }
        assert!(m.matches(&root.join("other.yml")));
        assert!(m.matches(&root.join("main.py")));
    }

    /// Live-found regression (2026-08-31): the project-file reload watcher's
    /// matcher was built with restart semantics, whose own-artifact exclusion
    /// list contains the file — so the watcher filtered out every event it
    /// existed to see and external edits never hot-reloaded.
    #[test]
    fn chappa_yml_matcher_sees_the_file_the_restart_exclusion_hides() {
        let root = PathBuf::from("/p");
        let m = ChangeMatcher::for_chappa_yml(root.clone());
        let own = crate::yml::CHAPPA_YML_NAME;
        assert!(m.matches(&root.join(own)), "the one file it exists for");
        // The write-back artifacts fail the glob — still no restart storms.
        assert!(!m.matches(&root.join(format!("{own}.chappa-tmp"))));
        assert!(!m.matches(&root.join(format!("{own}.chappa-bak"))));
        assert!(!m.matches(&root.join("other.yml")));
    }

    /// The composition the matcher-only tests missed: a REAL watcher armed
    /// the way `yml_watcher_for` arms it (for_chappa_yml + NonRecursive) must
    /// deliver a signal for an external project-file write.
    #[test]
    fn chappa_yml_watcher_fires_on_an_external_yml_write() {
        let root = tempfile::tempdir().unwrap();
        let own = crate::yml::CHAPPA_YML_NAME;
        fs::write(root.path().join(own), "processes: {}\n").unwrap();
        let matcher = ChangeMatcher::for_chappa_yml(root.path().to_path_buf());
        let (_watcher, rx) = RestartWatcher::start_with_mode(
            root.path().to_path_buf(),
            matcher,
            RecursiveMode::NonRecursive,
        )
        .unwrap();

        fs::write(root.path().join(own), "processes:\n  a:\n    command: x\n").unwrap();
        assert!(wait_for(&rx), "an external project-file edit must signal a reload");
    }
}
