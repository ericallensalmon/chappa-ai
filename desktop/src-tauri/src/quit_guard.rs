//! Quitting while a source job runs in a pane of this app.
//!
//! A job's pane is its runner: quitting kills it part-way, and the stack is
//! left half-changed. So closing the main window, or any other request to
//! exit, is held while such a job runs and the webview is asked (event
//! [`QUIT_EVENT`] with the jobs); "Quit anyway" calls [`confirm_quit`],
//! which lets the next exit through. Jobs a source runs on its own
//! (headless) outlive the app and are not asked about. A job the app did
//! end this way is settled as `interrupted` by its source the next time it
//! looks.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde::Serialize;
use serde_json::Value;
use tauri::{AppHandle, Emitter, Manager};

use crate::sources::{main_window_only, Sources, MAIN_WINDOW_LABEL};

/// What the webview hears when a quit is held.
pub const QUIT_EVENT: &str = "app://quit-blocked";

/// Set once the person chose to quit anyway.
#[derive(Clone, Default)]
pub struct QuitGate(Arc<AtomicBool>);

impl QuitGate {
    pub fn allowed(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    pub fn allow(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// One job a quit would end.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunningJob {
    pub label: String,
    pub started_utc: String,
}

/// The question to ask before quitting, or `None` when nothing would be
/// lost (no job running in a pane, or the person already said quit).
pub fn quit_question(allowed: bool, jobs: Vec<(String, String)>) -> Option<Value> {
    if allowed || jobs.is_empty() {
        return None;
    }
    let jobs: Vec<RunningJob> = jobs
        .into_iter()
        .map(|(label, started_utc)| RunningJob { label, started_utc })
        .collect();
    Some(serde_json::json!({ "jobs": jobs }))
}

/// Hold a quit when it would end a running job: `true` when held (the
/// webview was asked).
pub fn hold_quit(app: &AppHandle) -> bool {
    let allowed = app.state::<QuitGate>().allowed();
    let jobs = app.state::<Sources>().running_pane_jobs();
    let Some(question) = quit_question(allowed, jobs) else {
        return false;
    };
    match app.get_webview_window(MAIN_WINDOW_LABEL) {
        Some(w) => {
            let _ = w.show();
            let _ = w.set_focus();
            w.emit(QUIT_EVENT, question).is_ok()
        }
        // Nowhere to ask: let it go.
        None => false,
    }
}

/// "Quit anyway": let the exit through and exit.
#[tauri::command]
pub fn confirm_quit(window: tauri::Window, app: AppHandle) -> Result<(), String> {
    main_window_only(&window)?;
    app.state::<QuitGate>().allow();
    app.exit(0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quit_is_asked_about_only_while_a_pane_job_runs_and_until_confirmed() {
        assert_eq!(quit_question(false, vec![]), None);
        let jobs = vec![(
            "app: bring up".to_string(),
            "2026-11-14T12:00:00Z".to_string(),
        )];
        let q = quit_question(false, jobs.clone()).unwrap();
        assert_eq!(
            q,
            serde_json::json!({"jobs": [{"label": "app: bring up", "startedUtc": "2026-11-14T12:00:00Z"}]})
        );
        assert_eq!(quit_question(true, jobs), None);
    }
}
