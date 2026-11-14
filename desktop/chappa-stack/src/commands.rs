//! Project commands (`chappa.yml` `processes:`) as the process driver's
//! [`CommandLookup`].
//!
//! A component with `command: web-dev` names a project command; this is
//! where that name becomes argv. The project file is read with the same
//! parser the app uses, so a command means exactly what it means there:
//! a list runs its first word with the rest as arguments, a string runs
//! through the platform shell (`cmd /C` on Windows, `sh -c` elsewhere),
//! `working_dir` must stay inside the project, and `env` is added.

use indexmap::IndexMap;
use std::path::Path;

use project_model::yml::{CommandLine, ProjectYml};
use stack_core::process::{CommandLookup, CommandSpec};

/// The project's commands, read once.
#[derive(Debug, Clone, Default)]
pub struct ProjectCommands {
    commands: IndexMap<String, CommandSpec>,
    /// Commands that could not be used, with the reason (a `working_dir`
    /// outside the project, a blank command).
    pub problems: Vec<String>,
}

impl ProjectCommands {
    /// Read `chappa.yml` in `project_dir`. A project without one has no
    /// commands; a file that does not parse is an error.
    pub fn load(project_dir: &Path) -> Result<ProjectCommands, String> {
        if !project_dir.join("chappa.yml").exists() {
            return Ok(ProjectCommands::default());
        }
        let yml = ProjectYml::load_result(project_dir)?;
        Ok(Self::from_yml(&yml, project_dir))
    }

    pub fn from_yml(yml: &ProjectYml, project_dir: &Path) -> ProjectCommands {
        let mut out = ProjectCommands::default();
        for (name, def) in &yml.processes {
            if def.command.is_blank() {
                out.problems.push(format!("command `{name}` is blank"));
                continue;
            }
            let working_dir = match def.resolve_working_dir(project_dir) {
                Ok(dir) => dir,
                Err(e) => {
                    out.problems.push(format!("command `{name}`: {e}"));
                    continue;
                }
            };
            out.commands.insert(
                name.clone(),
                CommandSpec {
                    argv: argv_of(&def.command),
                    working_dir,
                    env: def.env.clone(),
                },
            );
        }
        out
    }

    pub fn names(&self) -> impl Iterator<Item = &String> {
        self.commands.keys()
    }
}

impl CommandLookup for ProjectCommands {
    fn command(&self, name: &str) -> Option<CommandSpec> {
        self.commands.get(name).cloned()
    }
}

/// argv for a command line: the list as is, a string through the shell.
pub fn argv_of(command: &CommandLine) -> Vec<String> {
    match command {
        CommandLine::Argv(words) => words.clone(),
        CommandLine::Shell(line) => {
            if cfg!(windows) {
                vec!["cmd".into(), "/C".into(), line.clone()]
            } else {
                vec!["sh".into(), "-c".into(), line.clone()]
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_stay_argv_and_strings_go_through_the_shell() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("web")).unwrap();
        let text = r#"
processes:
  web-dev:
    command: [npm, run, dev]
    working_dir: web
    env: { NODE_ENV: development }
  build:
    command: "make all"
  escape:
    command: [ls]
    working_dir: ../elsewhere
"#;
        let yml = ProjectYml::parse(text).unwrap();
        let c = ProjectCommands::from_yml(&yml, dir.path());
        let web = c.command("web-dev").unwrap();
        assert_eq!(web.argv, ["npm", "run", "dev"]);
        assert!(web.working_dir.unwrap().ends_with("web"));
        assert_eq!(web.env["NODE_ENV"], "development");
        let build = c.command("build").unwrap();
        assert_eq!(build.argv.last().unwrap(), "make all");
        assert_eq!(build.argv.len(), 3);
        assert!(c.command("escape").is_none());
        assert_eq!(c.problems.len(), 1, "{:?}", c.problems);
        assert!(c.command("nope").is_none());
    }
}
