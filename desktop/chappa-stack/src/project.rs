//! The project a source serves: its folder, which stack and slot, and the
//! definition read fresh from disk.
//!
//! The definition is re-read on every request and by every job, so an edit
//! to `chappa.yml` (or `chappa.stack.yml`) shows up on the next poll with
//! no restart. Which file holds it is stack-core's rule: `stacks:` in
//! `chappa.yml`, or a `chappa.stack.yml` beside it, never both.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use indexmap::IndexMap;
use stack_core::definition::{
    validate, Diagnostic, Severity, StackDef, StacksDocument, ValidationContext,
};
use stack_core::git::KnownRepos;
use stack_core::intent::{instance_id, ActiveRecord};
use stack_core::provenance::{GitError, GitFacts};
use stack_core::runner::{SharedCheckoutGuard, StackRunner, Tools};
use stack_core::workspace::{load_allow_file, ItemRecord, ALLOW_FILE};

use crate::commands::ProjectCommands;
use crate::world::{Shared, World};

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
    /// The work item bound to this slot and its worktrees.
    pub binding: Binding,
    /// [`stack_core::trust::definition_trust_hash`] of what was read: what
    /// the source compares with the hash the person trusted before it runs
    /// anything. `None` when it could not be computed.
    pub definition_hash: Option<String>,
    /// The runners' git facts, checked (see [`CheckedGit`]); made on first
    /// use from the world the first runner is made with.
    checked_git: OnceLock<CheckedGit>,
}

/// [`GitFacts`] that ask git only about folders whose git folder is the
/// project's, a clone's, or a worktree's of a clone
/// ([`KnownRepos::check`]). A runner asks about the bound item's
/// worktrees, whose `.git` files anything working in them can rewrite.
pub struct CheckedGit {
    inner: Shared<dyn GitFacts + Send + Sync>,
    known: KnownRepos,
}

impl CheckedGit {
    fn check(&self, repo: &Path) -> Result<(), GitError> {
        self.known.check(repo).map_err(GitError::Refused)
    }
}

impl GitFacts for CheckedGit {
    fn head_commit(&self, repo: &Path) -> Result<String, GitError> {
        self.check(repo)?;
        self.inner.head_commit(repo)
    }
    fn current_branch(&self, repo: &Path) -> Result<Option<String>, GitError> {
        self.check(repo)?;
        self.inner.current_branch(repo)
    }
    fn count_between(&self, repo: &Path, from: &str, to: &str) -> Result<u32, GitError> {
        self.check(repo)?;
        self.inner.count_between(repo, from, to)
    }
    fn dirty_hash(&self, repo: &Path) -> Result<Option<String>, GitError> {
        self.check(repo)?;
        self.inner.dirty_hash(repo)
    }
    fn commit_exists(&self, repo: &Path, sha: &str) -> Result<bool, GitError> {
        self.check(repo)?;
        self.inner.commit_exists(repo, sha)
    }
}

/// The item a slot is bound to (`active.json`) and its worktree per clone:
/// what every runner of the slot builds and runs from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Binding {
    /// The item's folder name.
    pub item: Option<String>,
    pub worktrees: IndexMap<String, PathBuf>,
    /// The branch the item's record names per clone (what scripts get as
    /// `{branch:<repo>}`).
    pub branches: IndexMap<String, String>,
}

impl Binding {
    /// The binding of `item` (a folder under the work folder): its record's
    /// repos. A missing or unreadable record binds the item with no
    /// worktrees, so nothing builds from a shared clone in its name. A
    /// review workspace (`review:<name>`) binds its one worktree.
    pub fn of_item(work_dir: &Path, item: &str) -> Binding {
        if crate::reviews::is_review_binding(item) {
            return Binding {
                item: Some(item.to_string()),
                worktrees: crate::reviews::worktrees_of(work_dir, item),
                branches: IndexMap::new(),
            };
        }
        let dir = ItemRecord::dir(work_dir, item);
        let (worktrees, branches) = match ItemRecord::load(&dir) {
            Ok(Some(rec)) => (
                rec.repos
                    .keys()
                    .filter_map(|r| rec.worktree(&dir, r).map(|w| (r.clone(), w)))
                    .collect(),
                rec.repos
                    .iter()
                    .map(|(r, repo)| (r.clone(), repo.branch.clone()))
                    .collect(),
            ),
            _ => (IndexMap::new(), IndexMap::new()),
        };
        Binding {
            item: Some(item.to_string()),
            worktrees,
            branches,
        }
    }
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
        let definition_hash = stack_core::trust::definition_trust(dir)
            .ok()
            .flatten()
            .map(|t| t.hash);
        let mut loaded = Loaded {
            dir: dir.to_path_buf(),
            stack_name: name,
            stack: def,
            file,
            diagnostics,
            commands,
            slot,
            binding: Binding::default(),
            definition_hash,
            checked_git: OnceLock::new(),
        };
        // The work folder holds baselines, scratch and workspaces: it is
        // made git-ignored whenever the definition is read.
        let _ = stack_core::workdir::ensure(&loaded.work_dir());
        let active = ActiveRecord::load(&loaded.work_dir())
            .ok()
            .and_then(|a| a.get(slot).map(|s| s.item.clone()));
        loaded.bind_item(active.as_deref());
        Ok(loaded)
    }

    /// Bind (or unbind) this slot's runners to `item`, whatever
    /// `active.json` says: an activation job builds from the item it is
    /// activating before the file says so.
    pub fn bind_item(&mut self, item: Option<&str>) {
        self.binding = match item {
            Some(i) => Binding::of_item(&self.work_dir(), i),
            None => Binding::default(),
        };
    }

    /// The shared-checkout guard: the allow file in the stack project.
    pub fn guard(&self) -> SharedCheckoutGuard {
        SharedCheckoutGuard {
            allowed: load_allow_file(&self.dir),
            file: self.dir.join(ALLOW_FILE),
        }
    }

    pub fn runner<'a>(&'a self, world: &'a World, now_utc: &str) -> StackRunner<'a> {
        self.plain_runner(world, now_utc)
            .with_binding(self.binding.worktrees.clone(), Some(self.guard()))
            .with_item(self.binding.item.clone(), self.binding.branches.clone())
    }

    /// Where git may run for this stack: the project, its clones, and
    /// worktrees of those clones.
    pub fn known_repos(&self) -> KnownRepos {
        let clones: Vec<PathBuf> = self
            .stack
            .repos()
            .values()
            .map(|r| self.stack.repo_dir(&self.dir, &r.path))
            .collect();
        KnownRepos::new(&self.dir, &clones)
    }

    /// The runners' git facts: `world`'s, asked only about known folders.
    pub fn checked_git(&self, world: &World) -> &CheckedGit {
        self.checked_git.get_or_init(|| CheckedGit {
            inner: world.git.clone(),
            known: self.known_repos(),
        })
    }

    /// A runner with no binding and no guard.
    pub fn plain_runner<'a>(&'a self, world: &'a World, now_utc: &str) -> StackRunner<'a> {
        StackRunner::new(
            &self.stack_name,
            &self.stack,
            &self.dir,
            Tools {
                cli: &*world.cli,
                git: self.checked_git(world),
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
