//! What applying a plan hands back: things to run, and files to write.
//!
//! A verb never runs a long command itself. `build`, `pull`, `up`, `logs -f`
//! and a dev server all take time and produce output worth watching, so the
//! driver returns each as a [`RunBlock`]: the exact argv, working directory,
//! environment and a title. The host application spawns it in a pane (or a
//! headless runner spawns it with a log file) and reports the exit code
//! back. The quick parts of a plan (writing the per-instance override file,
//! the intent record, the provenance cache) are [`LocalStep`]s the caller
//! performs in process, in order, between the runs.
//!
//! The same [`RunBlock`] shape serves `terminal` actions: a failed check's
//! fixing command (`docker login …`), a component's `logs -f`, the elevated
//! hosts-file edit. None of them is executed here.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::script::ScriptVerb;

/// A command for the host to run: argv (no shell), where, with what added
/// to the environment, under what title.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunBlock {
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub env: IndexMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

impl RunBlock {
    pub fn new(argv: Vec<String>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            argv,
            cwd: cwd.into(),
            env: IndexMap::new(),
            title: None,
        }
    }

    pub fn titled(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    pub fn with_env(mut self, env: IndexMap<String, String>) -> Self {
        self.env.extend(env);
        self
    }

    /// The command line as one string, for summaries only (never for running).
    pub fn command_line(&self) -> String {
        self.argv.join(" ")
    }
}

/// A quick, in-process step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum LocalStep {
    /// Write `contents` to `path` atomically (temp file + rename), creating
    /// parent folders. `what` says which file this is, for the step list.
    WriteFile {
        path: PathBuf,
        contents: String,
        what: String,
    },
    /// Nothing to do, with the reason (an image that was reused, a checkout
    /// already on its ref). Kept in the step list so the user sees why a
    /// planned step produced no command.
    Note { text: String },
    /// A record in the app-private gate storage ([`crate::gate_store`]) of
    /// the project in `project`, project scope: written with the store this
    /// process installed, owner-only, replaced atomically.
    WriteGate {
        project: PathBuf,
        name: String,
        value: serde_json::Value,
        what: String,
    },
}

impl LocalStep {
    /// Do it. Notes do nothing.
    pub fn perform(&self) -> io::Result<()> {
        match self {
            LocalStep::WriteFile { path, contents, .. } => write_atomic(path, contents),
            LocalStep::Note { .. } => Ok(()),
            LocalStep::WriteGate {
                project,
                name,
                value,
                ..
            } => crate::gate_store::GateStore::installed()
                .and_then(|store| {
                    store
                        .project(project)
                        .write(crate::gate_store::Scope::Project, name, value)
                })
                .map_err(io::Error::other),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            LocalStep::WriteFile { what, path, .. } => format!("write {what} ({})", path.display()),
            LocalStep::Note { text } => text.clone(),
            LocalStep::WriteGate { what, .. } => format!("write {what} (app storage)"),
        }
    }
}

/// One step of an applied plan, in execution order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum JobStep {
    /// A command the host runs as a job and waits for.
    Run { run: RunBlock, component: String },
    /// An in-process step.
    Local { step: LocalStep },
    /// A `process` (or dev-mode) component the host must stop: the stack
    /// never owns that pane, so it asks.
    StopProcess {
        component: String,
        grace_seconds: u32,
    },
    /// One of a script component's scripts: run like [`JobStep::Run`],
    /// but under its own limit, killed with its whole process tree when
    /// it runs out of time, and with its outcome recorded for the
    /// component's state.
    Script {
        run: RunBlock,
        component: String,
        verb: ScriptVerb,
        timeout_seconds: u64,
    },
}

impl JobStep {
    pub fn run(component: &str, run: RunBlock) -> Self {
        JobStep::Run {
            run,
            component: component.to_string(),
        }
    }

    pub fn write(what: &str, path: PathBuf, contents: String) -> Self {
        JobStep::Local {
            step: LocalStep::WriteFile {
                path,
                contents,
                what: what.to_string(),
            },
        }
    }

    pub fn write_gate(what: &str, project: &Path, name: &str, value: serde_json::Value) -> Self {
        JobStep::Local {
            step: LocalStep::WriteGate {
                project: project.to_path_buf(),
                name: name.to_string(),
                value,
                what: what.to_string(),
            },
        }
    }

    pub fn note(text: impl Into<String>) -> Self {
        JobStep::Local {
            step: LocalStep::Note { text: text.into() },
        }
    }

    pub fn as_run(&self) -> Option<&RunBlock> {
        match self {
            JobStep::Run { run, .. } | JobStep::Script { run, .. } => Some(run),
            _ => None,
        }
    }

    /// One line for the confirm dialog.
    pub fn describe(&self) -> String {
        match self {
            JobStep::Run { run, component } => format!(
                "{component}: {}",
                run.title.clone().unwrap_or_else(|| run.command_line())
            ),
            JobStep::Local { step } => step.describe(),
            JobStep::StopProcess {
                component,
                grace_seconds,
            } => format!("{component}: stop process ({grace_seconds}s)"),
            JobStep::Script {
                component, verb, ..
            } => format!("{component}: {verb} script"),
        }
    }
}

/// Write via a sibling temp file and rename, so a crash mid-write leaves
/// the previous file, never a truncated one.
pub fn write_atomic(path: &Path, contents: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, contents.as_bytes())?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_file_step_creates_parents_and_replaces_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a").join("b").join("f.yml");
        let step = JobStep::write("override", path.clone(), "x: 1\n".into());
        if let JobStep::Local { step } = &step {
            step.perform().unwrap();
            assert_eq!(fs::read_to_string(&path).unwrap(), "x: 1\n");
            assert!(step.describe().starts_with("write override ("));
        } else {
            panic!("not a local step");
        }
        write_atomic(&path, "x: 2\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "x: 2\n");
        assert!(!dir.path().join("a").join("b").join("f.yml.tmp").exists());
    }

    #[test]
    fn describe_prefers_the_title() {
        let run = RunBlock::new(vec!["docker".into(), "build".into()], ".").titled("build api");
        assert_eq!(JobStep::run("api", run).describe(), "api: build api");
        let bare = RunBlock::new(vec!["docker".into(), "pull".into(), "x".into()], ".");
        assert_eq!(JobStep::run("api", bare).describe(), "api: docker pull x");
        assert_eq!(
            JobStep::StopProcess {
                component: "web".into(),
                grace_seconds: 5
            }
            .describe(),
            "web: stop process (5s)"
        );
        let json = serde_json::to_value(JobStep::note("api:1234abcd reused")).unwrap();
        assert_eq!(json["kind"], "local");
        assert_eq!(json["step"]["kind"], "note");
    }
}
