//! The `process` driver: a component that is a project command run from a
//! worktree, and the environment every process of an instance gets.
//!
//! The stack never owns the process. `up` and `start` hand back a run block
//! (`{argv, cwd, env}`) that the host application starts as a pane; status
//! comes back the other way, as a [`ProcessStatus`] the caller supplies from
//! its own process table, and a probe judges the rest. The same shape serves
//! a compose component switched to `dev` mode: its container is removed and
//! its dev command runs here instead.
//!
//! The command itself is a name in the project file (`command: web-dev`).
//! Translating that name into argv is the project file's business, so it
//! comes through [`CommandLookup`]; this module only adds the instance's
//! variables.
//!
//! # Instance environment
//!
//! Every process, agent and terminal bound to an instance gets:
//!
//! | variable | value |
//! |---|---|
//! | `CHAPPA_STACK` | the stack name |
//! | `CHAPPA_STACK_SLOT` | the slot number |
//! | `CHAPPA_STACK_INSTANCE` | `<stack>-<slot>` |
//! | `CHAPPA_STACK_<COMPONENT>_URL` | each component's `outputs.url`, resolved on the host side |
//! | `CHAPPA_STACK_<COMPONENT>_<OUTPUT>` | its other outputs |
//! | `CHAPPA_STACK_<COMPONENT>_PORT` | its first host port (`_PORT_<n>` for the rest) |
//! | the stack's `env:` | exported as written, placeholders resolved |
//!
//! Without these, an agent in slot 2 would quietly test against slot 1.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::definition::{Mode, StackDef};
use crate::driver::{Actual, Health, ResolvedInstance};
use crate::jobs::RunBlock;
use crate::placeholders::{self, Context, PlaceholderError};

/// A project command, as the project file defines it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandSpec {
    pub argv: Vec<String>,
    /// Working directory relative to the project (or absolute); the
    /// component's repo when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub env: IndexMap<String, String>,
}

/// Resolves a command name from the project file.
pub trait CommandLookup {
    fn command(&self, name: &str) -> Option<CommandSpec>;
}

/// A fixed table of commands, for tests and for callers that preloaded them.
#[derive(Debug, Clone, Default)]
pub struct CommandTable(pub IndexMap<String, CommandSpec>);

impl CommandTable {
    pub fn with(mut self, name: &str, argv: &[&str]) -> Self {
        self.0.insert(
            name.to_string(),
            CommandSpec {
                argv: argv.iter().map(|s| s.to_string()).collect(),
                working_dir: None,
                env: IndexMap::new(),
            },
        );
        self
    }
}

impl CommandLookup for CommandTable {
    fn command(&self, name: &str) -> Option<CommandSpec> {
        self.0.get(name).cloned()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProcessError {
    #[error("component `{component}` has no command to run in {mode} mode")]
    NoCommand { component: String, mode: Mode },
    #[error("command `{name}` of component `{component}` is not in the project file")]
    UnknownCommand { component: String, name: String },
    #[error("component `{component}`: {source}")]
    Placeholder {
        component: String,
        #[source]
        source: PlaceholderError,
    },
}

/// `API` for `api`, `MY_SVC` for `my-svc`.
pub fn env_name(component: &str) -> String {
    component
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// The instance variables (see the module docs). Outputs that cannot be
/// resolved yet (an `{image:x}` before `x` exists) are left out rather than
/// failing the whole environment.
pub fn instance_env(
    inst: &ResolvedInstance,
    stack: &StackDef,
    images: &IndexMap<String, String>,
) -> IndexMap<String, String> {
    let mut env = IndexMap::new();
    env.insert("CHAPPA_STACK".to_string(), inst.stack.clone());
    env.insert("CHAPPA_STACK_SLOT".to_string(), inst.slot.to_string());
    env.insert("CHAPPA_STACK_INSTANCE".to_string(), inst.id.clone());
    let mut ctx = inst.host_context();
    ctx.images = images.clone();
    for (name, c) in &inst.components {
        let prefix = format!("CHAPPA_STACK_{}", env_name(name));
        for (i, port) in c.ports.iter().enumerate() {
            let key = if i == 0 {
                format!("{prefix}_PORT")
            } else {
                format!("{prefix}_PORT_{i}")
            };
            env.insert(key, port.to_string());
        }
        for (k, template) in &c.outputs {
            if let Ok(v) = placeholders::resolve(template, &ctx) {
                env.insert(format!("{prefix}_{}", env_name(k)), v);
            }
        }
    }
    for (k, template) in &stack.env {
        if let Ok(v) = placeholders::resolve(template, &ctx) {
            env.insert(k.clone(), v);
        }
    }
    env
}

/// `KEY=VALUE` lines, for the `stack.env` file tools read.
pub fn env_file(env: &IndexMap<String, String>) -> String {
    let mut out = String::new();
    for (k, v) in env {
        out.push_str(k);
        out.push('=');
        out.push_str(v);
        out.push('\n');
    }
    out
}

/// Which component to start as a process, and how.
#[derive(Debug, Clone, Copy)]
pub struct RunRequest<'a> {
    pub component: &'a str,
    pub mode: Mode,
    /// The worktree the command runs from (the mode's `repo`, resolved by
    /// the caller). A command with its own `working_dir` runs there instead.
    pub repo: Option<&'a Path>,
    /// Run the watch command rather than the plain dev command.
    pub watch: bool,
}

/// The run block that starts a component as a process (`dev` mode, or a
/// plain `process` component).
pub fn run_block(
    inst: &ResolvedInstance,
    stack: &StackDef,
    req: RunRequest<'_>,
    lookup: &dyn CommandLookup,
    project_dir: &Path,
    images: &IndexMap<String, String>,
) -> Result<RunBlock, ProcessError> {
    let RunRequest {
        component,
        mode,
        repo,
        watch,
    } = req;
    let def = stack
        .components
        .get(component)
        .ok_or_else(|| ProcessError::NoCommand {
            component: component.to_string(),
            mode,
        })?;
    let name = if watch {
        def.watch_command()
    } else {
        def.dev_command()
    }
    .ok_or_else(|| ProcessError::NoCommand {
        component: component.to_string(),
        mode,
    })?;
    let spec = lookup
        .command(name)
        .ok_or_else(|| ProcessError::UnknownCommand {
            component: component.to_string(),
            name: name.to_string(),
        })?;
    let cwd = match (&spec.working_dir, repo) {
        (Some(wd), _) if wd.is_absolute() => wd.clone(),
        (Some(wd), _) => project_dir.join(wd),
        (None, Some(r)) => r.to_path_buf(),
        (None, None) => project_dir.to_path_buf(),
    };
    let mut env = instance_env(inst, stack, images);
    let mut ctx: Context = inst.host_context();
    ctx.images = images.clone();
    for (k, template) in &def.env {
        let v =
            placeholders::resolve(template, &ctx).map_err(|source| ProcessError::Placeholder {
                component: component.to_string(),
                source,
            })?;
        env.insert(k.clone(), v);
    }
    env.extend(spec.env.clone());
    let title = if watch {
        format!("{component}: watch ({name})")
    } else {
        format!("{component}: {name}")
    };
    Ok(RunBlock {
        argv: spec.argv,
        cwd,
        env,
        title: Some(title),
    })
}

/// What the host knows about a process it runs for a component.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessStatus {
    pub running: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_utc: Option<String>,
    #[serde(default)]
    pub restarts: u32,
}

/// A process the host has no record of does not exist for the stack.
pub fn actual_from_process(status: Option<&ProcessStatus>) -> Actual {
    match status {
        None => Actual::default(),
        Some(s) => Actual {
            exists: true,
            running: s.running,
            restarting: false,
            restarts: s.restarts,
            exit_code: if s.running { None } else { s.exit_code },
            health: Health::Unknown,
            probe_up: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::definition::StacksDocument;
    use crate::driver::resolve;
    use crate::intent::{ComponentIntent, Desired, IntentRecord};

    fn stack() -> StackDef {
        let text = r#"
stacks:
  app:
    slots: { count: 3, portBase: 14000, portStride: 100 }
    hostname: dev.example.test
    env: { APP_ENV: "slot-{slot}", DB_HOST: "{host}" }
    components:
      db:
        driver: compose
        file: s.yml
        modes: { prebuilt: { image: "registry.example/db" } }
        outputs: { url: "postgres://{hostname}:{port:db}/app", name: "app" }
        ports: 2
      api:
        driver: compose
        file: s.yml
        modes:
          prebuilt: { image: "registry.example/api" }
          dev: { repo: ../api, command: api-dev, watch: api-watch }
        outputs: { url: "http://{hostname}:{port:api}", image: "{image:api}" }
      web:
        driver: process
        command: web-dev
        modes: { dev: { repo: ../web } }
        env: { API_URL: "http://{host}:{port:api}", PORT: "{port:web}" }
"#;
        StacksDocument::parse(text)
            .unwrap()
            .stacks
            .shift_remove("app")
            .unwrap()
    }

    fn inst(slot: u32) -> ResolvedInstance {
        let mut intent = IntentRecord::new("app", slot, "2026-01-01T00:00:00Z");
        intent.components.insert(
            "web".into(),
            ComponentIntent {
                desired: Desired::Running,
                ..Default::default()
            },
        );
        resolve("app", &stack(), slot, Some(&intent)).unwrap()
    }

    #[test]
    fn env_names_upper_case_and_replace_punctuation() {
        assert_eq!(env_name("api"), "API");
        assert_eq!(env_name("my-svc.v2"), "MY_SVC_V2");
    }

    #[test]
    fn instance_env_carries_slot_ports_outputs_and_stack_env() {
        let i = inst(2);
        let env = instance_env(&i, &stack(), &IndexMap::new());
        assert_eq!(env["CHAPPA_STACK"], "app");
        assert_eq!(env["CHAPPA_STACK_SLOT"], "2");
        assert_eq!(env["CHAPPA_STACK_INSTANCE"], "app-2");
        assert_eq!(env["CHAPPA_STACK_DB_PORT"], "14200");
        assert_eq!(env["CHAPPA_STACK_DB_PORT_1"], "14201");
        assert_eq!(env["CHAPPA_STACK_API_PORT"], "14202");
        assert_eq!(env["CHAPPA_STACK_WEB_PORT"], "14203");
        assert_eq!(
            env["CHAPPA_STACK_DB_URL"],
            "postgres://dev.example.test:14200/app"
        );
        assert_eq!(env["CHAPPA_STACK_DB_NAME"], "app");
        assert_eq!(env["CHAPPA_STACK_API_URL"], "http://dev.example.test:14202");
        assert!(
            !env.contains_key("CHAPPA_STACK_API_IMAGE"),
            "an unresolvable output is left out, not failed"
        );
        assert_eq!(env["APP_ENV"], "slot-2");
        assert_eq!(env["DB_HOST"], "localhost", "host side of the stack env");
        let mut images = IndexMap::new();
        images.insert(
            "api".to_string(),
            "registry.example/api@sha256:aa".to_string(),
        );
        let env = instance_env(&i, &stack(), &images);
        assert_eq!(
            env["CHAPPA_STACK_API_IMAGE"],
            "registry.example/api@sha256:aa"
        );
        let file = env_file(&env);
        assert!(file.contains("CHAPPA_STACK_SLOT=2\n"));
        assert!(file.ends_with('\n'));
    }

    #[test]
    fn run_block_merges_instance_component_and_command_env_and_picks_the_cwd() {
        let i = inst(1);
        let st = stack();
        let table = CommandTable::default()
            .with("web-dev", &["npm", "run", "dev"])
            .with("api-dev", &["cargo", "run"])
            .with("api-watch", &["cargo", "watch", "-x", "run"]);
        let web = RunRequest {
            component: "web",
            mode: Mode::Dev,
            repo: Some(Path::new("/proj/../web")),
            watch: false,
        };
        let run = run_block(&i, &st, web, &table, Path::new("/proj"), &IndexMap::new()).unwrap();
        assert_eq!(run.argv, ["npm", "run", "dev"].map(String::from));
        assert_eq!(run.cwd, PathBuf::from("/proj/../web"));
        assert_eq!(run.env["API_URL"], "http://localhost:14102");
        assert_eq!(run.env["PORT"], "14103");
        assert_eq!(run.env["CHAPPA_STACK_SLOT"], "1");
        assert_eq!(run.title.as_deref(), Some("web: web-dev"));

        let watch = run_block(
            &i,
            &st,
            RunRequest {
                component: "api",
                mode: Mode::Dev,
                repo: Some(Path::new("/w/api")),
                watch: true,
            },
            &table,
            Path::new("/proj"),
            &IndexMap::new(),
        )
        .unwrap();
        assert_eq!(watch.argv[1], "watch");
        assert_eq!(watch.title.as_deref(), Some("api: watch (api-watch)"));

        let mut with_wd = table.clone();
        with_wd.0.get_mut("web-dev").unwrap().working_dir = Some(PathBuf::from("ui"));
        with_wd
            .0
            .get_mut("web-dev")
            .unwrap()
            .env
            .insert("NODE_ENV".into(), "development".into());
        let run = run_block(&i, &st, web, &with_wd, Path::new("/proj"), &IndexMap::new()).unwrap();
        assert_eq!(run.cwd, PathBuf::from("/proj").join("ui"));
        assert_eq!(run.env["NODE_ENV"], "development");
    }

    #[test]
    fn run_block_errors_name_the_missing_piece() {
        let i = inst(1);
        let st = stack();
        let req = |component| RunRequest {
            component,
            mode: Mode::Dev,
            repo: None,
            watch: false,
        };
        let err = run_block(
            &i,
            &st,
            req("web"),
            &CommandTable::default(),
            Path::new("/p"),
            &IndexMap::new(),
        )
        .unwrap_err();
        assert_eq!(
            err,
            ProcessError::UnknownCommand {
                component: "web".into(),
                name: "web-dev".into()
            }
        );
        let err = run_block(
            &i,
            &st,
            req("db"),
            &CommandTable::default(),
            Path::new("/p"),
            &IndexMap::new(),
        )
        .unwrap_err();
        assert_eq!(
            err,
            ProcessError::NoCommand {
                component: "db".into(),
                mode: Mode::Dev
            }
        );
    }

    #[test]
    fn process_status_maps_to_actual() {
        assert_eq!(actual_from_process(None), Actual::default());
        let running = actual_from_process(Some(&ProcessStatus {
            running: true,
            exit_code: Some(0),
            started_utc: None,
            restarts: 1,
        }));
        assert!(running.exists && running.running);
        assert_eq!(
            running.exit_code, None,
            "a stale exit code is dropped while running"
        );
        assert_eq!(running.restarts, 1);
        let exited = actual_from_process(Some(&ProcessStatus {
            running: false,
            exit_code: Some(137),
            started_utc: None,
            restarts: 0,
        }));
        assert!(exited.exists && !exited.running);
        assert_eq!(exited.exit_code, Some(137));
    }
}
