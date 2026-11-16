//! Agent CLI settings files git would commit, watched over a project's
//! life rather than only at an agent's launch.
//!
//! An agent CLI writes approved commands (secrets included) into a local
//! settings file whenever a person answers "don't ask again", so the file
//! can appear or change at any time an agent runs, in the project folder or
//! in any worktree of the project's stack. The check
//! ([`stack_core::safety::agent_settings::check`], a `git check-ignore` per
//! file that exists) therefore runs when a project opens, when an agent
//! launches, when an agent exits, and each time across the project folder,
//! the agent's folder and every worktree the project's stacks keep.
//!
//! What it finds is kept in `<app config>/agent-settings-warnings.json`
//! until it is resolved (the file is ignored, removed, or no longer
//! there), so the warning is still listed after the app restarts. A file
//! is warned about once: a check that finds it again with the same
//! exposure says nothing new. A check over a folder drops the warnings of
//! that folder's files it no longer finds exposed.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use stack_core::safety::agent_settings::{self, AgentSettingsFile};

/// The file the warnings are kept in, in the app's config folder.
pub const STORE_FILE: &str = "agent-settings-warnings.json";

/// The event the webview turns into a notification (for `files`, the ones
/// new to this check) and its Needs you rows (`all`, every standing
/// warning): `{term_id, projectRoot, files: [...], all: [{projectRoot,
/// file}]}`.
pub const EVENT: &str = "agent://agent-settings";

/// One standing warning: the file and the project it was found for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Warning {
    pub project_root: PathBuf,
    pub file: AgentSettingsFile,
}

/// What one check changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Update {
    /// Files not warned about before (or whose exposure changed): the ones
    /// to notify about.
    pub new: Vec<AgentSettingsFile>,
    /// Whether the standing list changed at all (new, or resolved).
    pub changed: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Stored {
    #[serde(default)]
    warnings: Vec<Warning>,
}

#[derive(Debug, Default)]
struct State {
    /// `None` keeps the warnings in memory only (tests, headless services).
    path: Option<PathBuf>,
    warnings: Vec<Warning>,
}

/// The standing warnings, shared by every trigger. Cheap to clone.
#[derive(Debug, Clone, Default)]
pub struct AgentSettingsWarnings {
    state: Arc<Mutex<State>>,
}

impl AgentSettingsWarnings {
    /// Warnings kept in `path`, starting from what it holds (a missing or
    /// unreadable file starts empty: the next checks fill it again).
    pub fn at(path: PathBuf) -> Self {
        let warnings = fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<Stored>(&t).ok())
            .map(|s| s.warnings)
            .unwrap_or_default();
        Self {
            state: Arc::new(Mutex::new(State {
                path: Some(path),
                warnings,
            })),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Every standing warning, oldest first.
    pub fn all(&self) -> Vec<Warning> {
        self.lock().warnings.clone()
    }

    /// Run the check for `project_root` over `folders` and fold the result
    /// into the standing warnings: a file found for the first time (or
    /// with another exposure) is new; a warned-about file in one of these
    /// folders that is not found exposed any more is resolved.
    pub fn observe(&self, project_root: &Path, folders: &[PathBuf]) -> Update {
        let refs: Vec<&Path> = folders.iter().map(PathBuf::as_path).collect();
        let found = agent_settings::check(project_root, &refs);
        let checked: Vec<PathBuf> = folders
            .iter()
            .flat_map(|f| agent_settings::candidates(f))
            .collect();
        let mut state = self.lock();
        let mut update = Update::default();
        let before = state.warnings.len();
        state.warnings.retain(|w| {
            !checked.contains(&w.file.path) || found.iter().any(|f| f.path == w.file.path)
        });
        update.changed |= state.warnings.len() != before;
        for f in found {
            match state.warnings.iter_mut().find(|w| w.file.path == f.path) {
                Some(w) if w.file == f => {}
                Some(w) => {
                    w.file = f.clone();
                    update.new.push(f);
                }
                None => {
                    state.warnings.push(Warning {
                        project_root: project_root.to_path_buf(),
                        file: f.clone(),
                    });
                    update.new.push(f);
                }
            }
        }
        update.changed |= !update.new.is_empty();
        if update.changed {
            save(&state);
        }
        update
    }

    /// Check every standing warning again and drop the resolved ones
    /// (ignored now, removed, or gone): at startup, and after "Add to
    /// .gitignore". Whether anything was dropped or changed.
    pub fn revalidate(&self) -> bool {
        let standing = self.all();
        let mut now: Vec<Warning> = Vec::new();
        for w in &standing {
            if let Some(file) = agent_settings::check_file(&w.project_root, &w.file.path) {
                now.push(Warning {
                    project_root: w.project_root.clone(),
                    file,
                });
            }
        }
        let mut state = self.lock();
        // Anything a concurrent check added meanwhile stays.
        let added: Vec<Warning> = state
            .warnings
            .iter()
            .filter(|w| !standing.iter().any(|s| s.file.path == w.file.path))
            .cloned()
            .collect();
        now.extend(added);
        let changed = state.warnings != now;
        if changed {
            state.warnings = now;
            save(&state);
        }
        changed
    }
}

fn save(state: &State) {
    let Some(path) = &state.path else {
        return;
    };
    let text = serde_json::to_string_pretty(&Stored {
        warnings: state.warnings.clone(),
    })
    .expect("warnings serialize");
    if let Err(e) = stack_core::jobs::write_atomic(path, &text) {
        log::warn!("[agent-settings] {} not written: {e}", path.display());
    }
}

/// The folders one check covers: the project, the extra folders (the
/// agent's), and every worktree of every stack the project's runners serve.
/// Each once, in that order.
pub fn folders_for(project_root: &Path, extra: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = vec![project_root.to_path_buf()];
    let mut push = |p: PathBuf| {
        if !out.contains(&p) {
            out.push(p);
        }
    };
    for e in extra {
        push(e.clone());
    }
    if let Ok(yml) = project_model::yml::ProjectYml::load_result(project_root) {
        for s in crate::stack_trust::stack_sources(project_root, &yml.processes) {
            for wt in agent_settings::stack_worktrees(&s.folder) {
                push(wt);
            }
        }
    }
    out
}

/// The event payload for one check (or a revalidation, with no `new`).
pub fn payload(
    term_id: Option<u32>,
    project_root: &Path,
    new: &[AgentSettingsFile],
    all: &[Warning],
) -> serde_json::Value {
    let dto = |f: &AgentSettingsFile| {
        serde_json::json!({
            "path": f.path.display().to_string(),
            "ignoreLine": f.ignore_line,
            "exposure": f.exposure,
            "message": f.message(),
        })
    };
    serde_json::json!({
        "term_id": term_id,
        "projectRoot": project_root.display().to_string(),
        "files": new.iter().map(dto).collect::<Vec<_>>(),
        "all": all
            .iter()
            .map(|w| serde_json::json!({
                "projectRoot": w.project_root.display().to_string(),
                "file": dto(&w.file),
            }))
            .collect::<Vec<_>>(),
    })
}

/// Log each new warning and tell the webview, when anything changed.
pub fn emit(
    app: Option<&tauri::AppHandle>,
    warnings: &AgentSettingsWarnings,
    term_id: Option<u32>,
    project_root: &Path,
    update: &Update,
) {
    for f in &update.new {
        log::warn!("[agent-settings] {}", f.message());
    }
    if !update.changed {
        return;
    }
    if let Some(app) = app {
        use tauri::Emitter;
        let _ = app.emit(
            EVENT,
            payload(term_id, project_root, &update.new, &warnings.all()),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    }

    /// A repository with no global or system git config (a per-user global
    /// ignore rule would hide the file).
    fn repo(tmp: &Path) -> PathBuf {
        std::env::set_var("GIT_CONFIG_GLOBAL", tmp.join("no-global-config"));
        std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
        std::env::set_var("XDG_CONFIG_HOME", tmp.join("no-xdg"));
        let root = tmp.join("project");
        fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q"]);
        agent_settings::plain(&root)
    }

    fn settings_file(folder: &Path) -> PathBuf {
        let p = folder.join(".claude").join("settings.local.json");
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, "{}\n").unwrap();
        p
    }

    #[test]
    fn a_file_is_warned_about_once_kept_across_a_restart_and_dropped_once_resolved() {
        if !git_available() {
            eprintln!("git not on PATH; skipping");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(tmp.path());
        let store = tmp.path().join("config").join(STORE_FILE);
        let w = AgentSettingsWarnings::at(store.clone());
        assert_eq!(
            w.observe(&root, std::slice::from_ref(&root)),
            Update::default()
        );
        settings_file(&root);
        // Found: new, and kept on disk.
        let first = w.observe(&root, &[root.clone(), root.join("."), root.clone()]);
        assert_eq!(first.new.len(), 1, "{first:?}");
        assert!(first.changed);
        // Found again from another trigger: nothing new to say.
        let again = w.observe(&root, std::slice::from_ref(&root));
        assert_eq!(again, Update::default());
        assert_eq!(w.all().len(), 1);
        // A restart reads the same warning back, and its revalidation keeps it.
        let restarted = AgentSettingsWarnings::at(store.clone());
        assert_eq!(restarted.all(), w.all());
        assert!(!restarted.revalidate());
        assert_eq!(restarted.all().len(), 1);
        // Resolved (ignored): the revalidation after the action drops it,
        // and so does the file the next run reads.
        agent_settings::add_to_gitignore(&root, "/.claude/settings.local.json").unwrap();
        assert!(restarted.revalidate());
        assert!(restarted.all().is_empty());
        assert!(AgentSettingsWarnings::at(store).all().is_empty());
    }

    #[test]
    fn a_check_drops_a_resolved_file_of_its_folders_only() {
        if !git_available() {
            eprintln!("git not on PATH; skipping");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = repo(tmp.path());
        let wt = root.join("wt");
        let in_root = settings_file(&root);
        settings_file(&wt);
        let w = AgentSettingsWarnings::default();
        assert_eq!(w.observe(&root, &[root.clone(), wt.clone()]).new.len(), 2);
        fs::remove_file(&in_root).unwrap();
        // A check of the worktree alone leaves the project's warning be.
        assert_eq!(
            w.observe(&root, std::slice::from_ref(&wt)),
            Update::default()
        );
        assert_eq!(w.all().len(), 2);
        // A check of the project drops it.
        let u = w.observe(&root, std::slice::from_ref(&root));
        assert!(u.changed && u.new.is_empty(), "{u:?}");
        assert_eq!(w.all().len(), 1);
    }

    #[test]
    fn the_folders_cover_the_project_the_agent_and_every_stack_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("project");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("chappa.yml"),
            "processes:\n  stack:\n    command: [chappa-stack, serve]\n    source: {}\nstacks:\n  app:\n    components: {}\n",
        )
        .unwrap();
        let review = root.join(".work").join("reviews").join("github-app-3");
        fs::create_dir_all(&review).unwrap();
        let agent = root.join("elsewhere");
        let folders = folders_for(&root, &[agent.clone(), root.clone()]);
        assert_eq!(folders, vec![root.clone(), agent, review]);
    }

    #[test]
    fn the_event_lists_the_new_files_and_every_standing_warning() {
        let file = AgentSettingsFile {
            path: PathBuf::from("/srv/example/.claude/settings.local.json"),
            ignore_line: Some("/.claude/settings.local.json".into()),
            exposure: agent_settings::Exposure::NotIgnored,
        };
        let all = vec![Warning {
            project_root: PathBuf::from("/srv/example"),
            file: file.clone(),
        }];
        let p = payload(None, Path::new("/srv/example"), &[], &all);
        assert_eq!(p["term_id"], serde_json::Value::Null);
        assert_eq!(p["files"].as_array().unwrap().len(), 0);
        assert_eq!(p["all"][0]["file"]["exposure"], "not-ignored");
        assert!(p["all"][0]["file"]["message"]
            .as_str()
            .unwrap()
            .contains("is not git-ignored"));
        let p = payload(Some(7), Path::new("/srv/example"), &[file], &all);
        assert_eq!(p["term_id"], 7);
        assert_eq!(p["files"][0]["ignoreLine"], "/.claude/settings.local.json");
    }
}
