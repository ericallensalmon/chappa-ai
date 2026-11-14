//! The project a source serves: its folder, which stack and slot, and the
//! definition read fresh from disk.
//!
//! The definition is re-read on every request and by every job, so an edit
//! to `chappa.yml` (or `chappa.stack.yml`) shows up on the next poll with
//! no restart. Which file holds it is stack-core's rule: `stacks:` in
//! `chappa.yml`, or a `chappa.stack.yml` beside it, never both.

use std::path::{Path, PathBuf};

use stack_core::definition::{
    validate, Diagnostic, Severity, StackDef, StacksDocument, ValidationContext,
};
use stack_core::intent::instance_id;
use stack_core::runner::{StackRunner, Tools};

use crate::commands::ProjectCommands;
use crate::world::World;

/// What the server was started for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectConfig {
    pub dir: PathBuf,
    /// The stack to serve; `None` = the project's only (or first) stack.
    pub stack: Option<String>,
    pub slot: u32,
    /// The prebuilt channel an `up` resolves `{channel}` with when the
    /// instance has none recorded yet.
    pub channel: String,
}

/// The channel used when nothing else names one.
pub const DEFAULT_CHANNEL: &str = "main";

impl ProjectConfig {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            stack: None,
            slot: 0,
            channel: DEFAULT_CHANNEL.to_string(),
        }
    }
}

/// The definition as loaded now.
pub struct Loaded {
    pub dir: PathBuf,
    pub stack_name: String,
    pub stack: StackDef,
    /// The file the definition came from.
    pub file: PathBuf,
    /// Validation findings, warnings included.
    pub diagnostics: Vec<Diagnostic>,
    pub commands: ProjectCommands,
    pub slot: u32,
}

impl Loaded {
    pub fn load(dir: &Path, stack: Option<&str>, slot: u32) -> Result<Loaded, String> {
        let (file, mut doc) = StacksDocument::load_from_project(dir)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| {
                format!(
                    "{} defines no stack (`stacks:` in chappa.yml or chappa.stack.yml)",
                    dir.display()
                )
            })?;
        let gitignore = std::fs::read_to_string(dir.join(".gitignore")).ok();
        let all = validate(
            &doc,
            &ValidationContext {
                gitignore: gitignore.as_deref(),
            },
        );
        let name = match stack {
            Some(s) => s.to_string(),
            None => doc
                .stacks
                .keys()
                .next()
                .cloned()
                .ok_or_else(|| format!("{} has an empty `stacks:`", file.display()))?,
        };
        let def = doc
            .stacks
            .shift_remove(&name)
            .ok_or_else(|| format!("no stack `{name}` in {}", file.display()))?;
        let prefix = format!("stacks.{name}");
        let diagnostics = all
            .into_iter()
            .filter(|d| d.path == prefix || d.path.starts_with(&format!("{prefix}.")))
            .collect();
        if slot >= def.slots.count.max(1) {
            return Err(format!(
                "stack `{name}` has {} slot(s); slot {slot} does not exist",
                def.slots.count.max(1)
            ));
        }
        let commands = ProjectCommands::load(dir)?;
        Ok(Loaded {
            dir: dir.to_path_buf(),
            stack_name: name,
            stack: def,
            file,
            diagnostics,
            commands,
            slot,
        })
    }

    pub fn runner<'a>(&'a self, world: &'a World, now_utc: &str) -> StackRunner<'a> {
        StackRunner::new(
            &self.stack_name,
            &self.stack,
            &self.dir,
            Tools {
                cli: &*world.cli,
                git: &*world.git,
                commands: &self.commands,
                gateways: &*world.gateways,
            },
            now_utc,
        )
    }

    pub fn work_dir(&self) -> PathBuf {
        if self.stack.work_dir.is_absolute() {
            self.stack.work_dir.clone()
        } else {
            self.dir.join(&self.stack.work_dir)
        }
    }

    pub fn instance_id(&self) -> String {
        instance_id(&self.stack_name, self.slot)
    }

    pub fn has_errors(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|d| d.severity == Severity::Error)
    }
}
