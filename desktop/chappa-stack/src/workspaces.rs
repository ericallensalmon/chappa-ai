//! Item workspaces as a Work overlay: the stack adds to the tracker's row,
//! it never serves a competing one.
//!
//! A tracker source owns a work item's row. This source adds, per item it
//! has a workspace for, an *overlay* entry (`ref: {source, key}`) with the
//! local side: the folder, whether the item is active in this slot, its
//! branches in the PR rows' own terms, badges, a Repos section and the
//! actions below. Until the app merges overlays into their owner's rows it
//! shows each as its own row; the overlay's `key` is the owner's key and
//! it carries the cached `title` / `url`, so such a row reads well.
//!
//! | action | where | does |
//! |---|---|---|
//! | `start-work` | manifest `itemActions`, on other sources' Work rows | creates `.work/<KEY>/item.json` from the row it was sent, then (a job) a worktree for every repo the item already has a PR branch in; never creates a branch |
//! | `add-repo:<repo>` | overlay, per definition repo the item lacks | a job: checks out the naming pattern's branch when origin has it, else creates it from the repo's base; the confirm says which, and the job refuses if origin changed in between |
//! | `activate` | overlay, when not active in this slot | binds the slot to the item: a planned job that builds or runs each component whose repo the item has from its worktree |
//! | `terminal:<repo>` | overlay, per worktree | a terminal in the worktree |
//! | `switch-branch:<repo>` | overlay, while that worktree's HEAD is detached | a terminal running `git switch <branch>` in it (the worktree also carries a `warn` badge until HEAD is on a branch) |
//! | `archive` | overlay | refused while a worktree is dirty or has unpushed commits (listed); else a job removes the worktrees and moves the folder to `_archive/` |
//! | `restore:<KEY>` | the Archived panel | a job that recreates the worktrees at their branches |
//! | `rebuild-changed:<KEY>` / `keep-as-built:<KEY>` | the drift banner on the item | rebuild what the moved branches feed / dismiss until HEAD moves again |
//! | `move:<state>` / `approve` / `send-back` | overlay (and the tracker-status banner), with a `workflow:` | the item's local status, see [`crate::local_status`] |
//! | `set-seeds` | overlay, when the stack defines state seeds | which state seeds the item asks for, see [`crate::seeds`] |
//! | `save-state` / `resume:<state>` | overlay (the Saved states section) | the item's saved stack states, see [`crate::states`] |
//! | `activate-no-save` / `archive-no-save` / `archive-keep-data` | overlay | activate saves the item the slot leaves first, archive saves the item itself first when it is the one the slot runs; these twins skip the save, or keep the host copies of sensitive states' snapshots |
//!
//! Every git question the Work list shows (HEADs, dirty and unpushed
//! counts, ahead / behind, whether origin has a branch, each clone's
//! remote) is asked on the background thread and cached ([`WorkCache`]);
//! the list and the Environment view only read the cache, so a slow git
//! never slows a poll. Actions that change something ask git again,
//! live, before they are accepted and when their job runs.
//!
//! git runs through [`crate::world::World::git_cli`], argv only, never a
//! shell, each call under a time limit and with terminal prompts off.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use stack_core::cli::{EngineCli, Invocation};
use stack_core::definition::{Mode, ANY_SOURCE};
use stack_core::images::ProvenanceCache;
use stack_core::intent::ActiveRecord;
use stack_core::planner::{BindTarget, Change, Plan};
use stack_core::runner::{Observation, StackRunner};
use stack_core::workspace::{
    c8, check_path_length, classify_remote, fill_branch, find_item, is_valid_branch, item_dir_name,
    list_archived, list_items, override_hosts, slug, ItemRecord, ItemRef, ItemRepo, ItemStatus,
    LongPaths, RepoIdentity, ARCHIVE_DIR, SRC_DIR,
};

use crate::actions::{confirm_text, mode_now, Offered};
use crate::local_status;
use crate::project::{Binding, Loaded};
use crate::records::JobWork;
use crate::world::World;

pub const WORK_PATH: &str = "/api/work";
pub const ARCHIVED_PATH: &str = "/api/work/archived";
/// Where the app pushes the owner rows this source adds to (the manifest's
/// `ownerUpdates`).
pub const OWNER_PATH: &str = "/api/work/owner";
/// How often the app should poll the Work view, in seconds.
pub const WORK_POLL_SECONDS: u64 = 30;
pub const START_WORK_ID: &str = "start-work";
pub const ACTIVATE_ID: &str = "activate";
pub const ARCHIVE_ID: &str = "archive";
/// Action ids are unique per row (the app finds an action by id), so the
/// per-repo and per-item ones carry their subject after the colon.
pub const ADD_REPO_PREFIX: &str = "add-repo:";
pub const TERMINAL_PREFIX: &str = "terminal:";
pub const RESTORE_PREFIX: &str = "restore:";
pub const REBUILD_PREFIX: &str = "rebuild-changed:";
pub const KEEP_PREFIX: &str = "keep-as-built:";
/// A terminal that puts a detached worktree back on the item's branch.
pub const SWITCH_PREFIX: &str = "switch-branch:";
/// How long a worktree's facts are trusted.
pub const FACTS_TTL_SECS: u64 = 30;
/// How long "origin has / lacks this branch" is trusted.
pub const REMOTE_TTL_SECS: u64 = 120;
/// How long a clone's remote URL is trusted.
pub const IDENTITY_TTL_SECS: u64 = 300;
/// Quick local git questions.
pub const LOCAL_GIT: Duration = Duration::from_secs(15);
/// `ls-remote` against origin.
pub const LS_REMOTE: Duration = Duration::from_secs(30);
/// `fetch` of one branch.
pub const FETCH: Duration = Duration::from_secs(120);
/// The longest title and status label kept from a tracker's row.
const TITLE_CAP: usize = 300;
const LABEL_CAP: usize = 80;
const KEY_CAP: usize = 200;
const URL_CAP: usize = 2048;
const PRS_CAP: usize = 100;

// ------------------------------------------------------------------- git

/// git through an [`EngineCli`]: `git -C <dir> …`, argv only, with
/// `GIT_TERMINAL_PROMPT=0` so a missing credential fails instead of
/// waiting for a prompt nobody sees.
pub struct Git<'a> {
    pub cli: &'a dyn EngineCli,
}

impl<'a> Git<'a> {
    pub fn new(cli: &'a dyn EngineCli) -> Self {
        Self { cli }
    }

    fn inv(&self, dir: &Path, args: &[&str], limit: Duration) -> Invocation {
        let mut argv = vec![
            "git".to_string(),
            "-C".to_string(),
            dir.display().to_string(),
        ];
        argv.extend(args.iter().map(|a| a.to_string()));
        Invocation::from_argv(&argv)
            .timeout(limit)
            .env("GIT_TERMINAL_PROMPT", "0")
    }

    fn run(&self, dir: &Path, args: &[&str], limit: Duration) -> Result<String, String> {
        match self.cli.run(&self.inv(dir, args, limit)) {
            Ok(o) if o.ok() => Ok(o.stdout),
            Ok(o) => Err(format!("`git {}` failed ({})", args.join(" "), o.summary())),
            Err(e) => Err(e.to_string()),
        }
    }

    pub fn remote_url(&self, clone: &Path) -> Result<String, String> {
        self.run(clone, &["remote", "get-url", "origin"], LOCAL_GIT)
            .map(|s| s.trim().to_string())
    }

    /// The commit origin's `branch` points at, `None` when origin has no
    /// such branch.
    pub fn remote_branch(&self, clone: &Path, branch: &str) -> Result<Option<String>, String> {
        let r = format!("refs/heads/{branch}");
        let out = self.run(clone, &["ls-remote", "--heads", "origin", &r], LS_REMOTE)?;
        Ok(out.lines().find_map(|l| {
            let (sha, name) = l.split_once('\t')?;
            (name.trim() == r).then(|| sha.trim().to_string())
        }))
    }

    pub fn local_branch_exists(&self, clone: &Path, branch: &str) -> bool {
        self.run(
            clone,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ],
            LOCAL_GIT,
        )
        .is_ok()
    }

    /// Fetch origin's `branch` into `refs/remotes/origin/<branch>` and
    /// return its commit.
    pub fn fetch_branch(&self, clone: &Path, branch: &str) -> Result<String, String> {
        let spec = format!("+refs/heads/{branch}:refs/remotes/origin/{branch}");
        self.run(clone, &["fetch", "--no-tags", "origin", &spec], FETCH)?;
        self.rev_parse(clone, &format!("refs/remotes/origin/{branch}"))
    }

    pub fn rev_parse(&self, dir: &Path, rev: &str) -> Result<String, String> {
        self.run(
            dir,
            &["rev-parse", "--verify", &format!("{rev}^{{commit}}")],
            LOCAL_GIT,
        )
        .map(|s| s.trim().to_string())
    }

    /// A worktree of an existing branch: the local one when the clone has
    /// it, else a new local branch tracking origin's.
    pub fn worktree_add_existing(
        &self,
        clone: &Path,
        path: &Path,
        branch: &str,
    ) -> Result<(), String> {
        let p = path.display().to_string();
        if self.local_branch_exists(clone, branch) {
            self.run(clone, &["worktree", "add", &p, branch], LOCAL_GIT * 4)
        } else {
            let upstream = format!("refs/remotes/origin/{branch}");
            self.run(
                clone,
                &["worktree", "add", "--track", "-b", branch, &p, &upstream],
                LOCAL_GIT * 4,
            )
        }
        .map(|_| ())
    }

    /// A worktree on a new local branch started at `start`, with no upstream.
    pub fn worktree_add_new(
        &self,
        clone: &Path,
        path: &Path,
        branch: &str,
        start: &str,
    ) -> Result<(), String> {
        let p = path.display().to_string();
        self.run(
            clone,
            &["worktree", "add", "--no-track", "-b", branch, &p, start],
            LOCAL_GIT * 4,
        )
        .map(|_| ())
    }

    pub fn worktree_remove(&self, clone: &Path, path: &Path) -> Result<(), String> {
        let p = path.display().to_string();
        self.run(clone, &["worktree", "remove", &p], LOCAL_GIT * 4)
            .map(|_| ())
    }

    /// A worktree at `commit` with no branch (a review checkout).
    pub fn worktree_add_detached(
        &self,
        clone: &Path,
        path: &Path,
        commit: &str,
    ) -> Result<(), String> {
        let p = path.display().to_string();
        self.run(
            clone,
            &["worktree", "add", "--detach", &p, commit],
            LOCAL_GIT * 4,
        )
        .map(|_| ())
    }

    /// Fetch one refspec from origin (`+refs/pull/12/head:refs/remotes/origin/pr/12`).
    pub fn fetch_spec(&self, clone: &Path, spec: &str) -> Result<(), String> {
        self.run(clone, &["fetch", "--no-tags", "origin", spec], FETCH)
            .map(|_| ())
    }

    /// Put a worktree on `commit`: on `branch` when its tip is that commit
    /// (so the item's branch stays checked out), else detached there.
    pub fn checkout_at(
        &self,
        worktree: &Path,
        commit: &str,
        branch: Option<&str>,
    ) -> Result<(), String> {
        if let Some(b) = branch {
            if self
                .rev_parse(worktree, &format!("refs/heads/{b}"))
                .ok()
                .as_deref()
                == Some(commit)
            {
                return self
                    .run(worktree, &["checkout", "-q", b], LOCAL_GIT * 4)
                    .map(|_| ());
            }
        }
        self.run(
            worktree,
            &["checkout", "-q", "--detach", commit],
            LOCAL_GIT * 4,
        )
        .map(|_| ())
    }

    /// Uncommitted entries in a worktree (`git status --porcelain` lines).
    pub fn uncommitted(&self, worktree: &Path) -> Result<u32, String> {
        self.run(worktree, &["status", "--porcelain"], LOCAL_GIT)
            .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count() as u32)
    }

    /// The last commit that touched `file` in the repo at `dir`.
    pub fn last_commit_of(&self, dir: &Path, file: &str) -> Option<String> {
        self.run(dir, &["log", "-1", "--format=%H", "--", file], LOCAL_GIT)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    /// `core.longpaths` of the clone (git's own setting; unset is off).
    pub fn long_paths(&self, clone: &Path) -> LongPaths {
        match self
            .cli
            .run(&self.inv(clone, &["config", "--get", "core.longpaths"], LOCAL_GIT))
        {
            Ok(o) if o.ok() => match o.stdout.trim().to_ascii_lowercase().as_str() {
                "true" | "yes" | "on" | "1" => LongPaths::On,
                _ => LongPaths::Off,
            },
            Ok(o) if o.status == Some(1) => LongPaths::Off,
            _ => LongPaths::Unknown,
        }
    }

    /// HEAD, branch, uncommitted entries, commits not on origin, and
    /// ahead / behind of `origin/<base>`.
    pub fn facts(&self, worktree: &Path, base: &str) -> RepoFacts {
        let mut f = RepoFacts::default();
        match self.rev_parse(worktree, "HEAD") {
            Ok(h) => f.head = Some(h),
            Err(e) => {
                f.error = Some(e);
                return f;
            }
        }
        f.branch = self
            .run(worktree, &["rev-parse", "--abbrev-ref", "HEAD"], LOCAL_GIT)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|b| b != "HEAD");
        match self.run(worktree, &["status", "--porcelain"], LOCAL_GIT) {
            Ok(s) => f.dirty = s.lines().filter(|l| !l.trim().is_empty()).count() as u32,
            Err(e) => f.error = Some(e),
        }
        match self.run(
            worktree,
            &["rev-list", "--count", "HEAD", "--not", "--remotes=origin"],
            LOCAL_GIT,
        ) {
            Ok(s) => f.unpushed = s.trim().parse().unwrap_or(0),
            Err(e) => f.error = Some(e),
        }
        let range = format!("refs/remotes/origin/{base}...HEAD");
        if let Ok(s) = self.run(
            worktree,
            &["rev-list", "--left-right", "--count", &range],
            LOCAL_GIT,
        ) {
            let mut it = s.split_whitespace().filter_map(|n| n.parse::<u32>().ok());
            f.behind = it.next();
            f.ahead = it.next();
        }
        f
    }
}

// ----------------------------------------------------------------- facts

/// One worktree as git last said.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoFacts {
    pub head: Option<String>,
    /// The branch checked out; `None` when HEAD is detached.
    pub branch: Option<String>,
    /// Where the item's own branch points (it differs from `head` while
    /// the worktree is detached, or on another branch).
    pub tip: Option<String>,
    pub dirty: u32,
    pub unpushed: u32,
    pub ahead: Option<u32>,
    pub behind: Option<u32>,
    pub error: Option<String>,
}

/// What one item's folder looks like to git.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ItemFacts {
    pub at: u64,
    pub repos: IndexMap<String, RepoFacts>,
    /// Per repo the item lacks: whether origin has the pattern's branch
    /// (and when that was asked).
    pub remote: IndexMap<String, (Option<bool>, u64)>,
}

/// A component built from an older commit than its worktree's HEAD.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Drift {
    pub item: String,
    pub component: String,
    pub repo: String,
    pub branch: String,
    pub mode: Mode,
    pub built: String,
    pub head: String,
}

impl Drift {
    pub fn line(&self) -> String {
        format!(
            "{} was built from {}; {} is now at {}",
            self.component,
            c8(&self.built),
            self.branch,
            c8(&self.head)
        )
    }
}

/// Everything the Work list shows from git.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkFacts {
    /// Each clone's identity (`None` = unclassifiable) and its remote error.
    pub identities: IndexMap<String, Result<Option<RepoIdentity>, String>>,
    pub identities_at: Option<u64>,
    pub items: IndexMap<String, ItemFacts>,
    /// Drift of the slot's active item.
    pub drift: Vec<Drift>,
    /// Ask everything again on the next pass.
    pub stale: bool,
    /// Counts invalidations, so a pass that began before one does not
    /// overwrite the cache with what it saw before the change.
    pub generation: u64,
}

/// The cache the background thread fills and requests read.
#[derive(Default)]
pub struct WorkCache {
    facts: Mutex<WorkFacts>,
}

impl WorkCache {
    pub fn snapshot(&self) -> WorkFacts {
        self.facts.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Forget the ages so the next pass asks git again (after a workspace
    /// change).
    pub fn invalidate(&self) {
        let mut f = self.facts.lock().unwrap_or_else(|p| p.into_inner());
        f.stale = true;
        f.generation += 1;
    }

    /// One pass, on the caller's thread (the background refresher's): ask
    /// git what is missing or older than its time to live, and recompute
    /// the drift of the active item. No lock is held while git runs.
    pub fn refresh(&self, world: &World, loaded: &Loaded) {
        let now = world.clock.now_secs();
        let before = self.snapshot();
        let git = Git::new(&*world.git_cli);
        let work_dir = loaded.work_dir();
        let stale = |at: u64, ttl: u64| before.stale || now.saturating_sub(at) >= ttl;
        let identities = if before
            .identities_at
            .is_none_or(|t| stale(t, IDENTITY_TTL_SECS))
        {
            Some(repo_identities(&git, loaded))
        } else {
            None
        };
        let items = list_items(&work_dir);
        let mut new_items: IndexMap<String, ItemFacts> = IndexMap::new();
        let work = loaded.stack.work_def();
        for (dir, rec) in &items {
            let old = before.items.get(&rec.key);
            let mut facts = match old {
                Some(o) if !stale(o.at, FACTS_TTL_SECS) => ItemFacts {
                    repos: o.repos.clone(),
                    at: o.at,
                    remote: IndexMap::new(),
                },
                _ => {
                    let mut repos = IndexMap::new();
                    for (repo, r) in &rec.repos {
                        let wt = dir.join(&r.path);
                        let mut f = git.facts(&wt, &r.base);
                        f.tip = git.rev_parse(&wt, &format!("refs/heads/{}", r.branch)).ok();
                        repos.insert(repo.clone(), f);
                    }
                    ItemFacts {
                        at: now,
                        repos,
                        remote: IndexMap::new(),
                    }
                }
            };
            let branch = fill_branch(
                &work.branch,
                &rec.key,
                &slug(rec.title.as_deref().unwrap_or("")),
            );
            for repo in loaded.stack.repos().keys() {
                if rec.repos.contains_key(repo) {
                    continue;
                }
                let known = old.and_then(|o| o.remote.get(repo)).copied();
                let answer = match known {
                    Some((v, at)) if v.is_some() && !stale(at, REMOTE_TTL_SECS) => (v, at),
                    _ => {
                        let v = match (&branch, clone_dir(loaded, repo)) {
                            (Ok(b), Some(clone)) => {
                                git.remote_branch(&clone, b).ok().map(|s| s.is_some())
                            }
                            _ => None,
                        };
                        (v, now)
                    }
                };
                facts.remote.insert(repo.clone(), answer);
            }
            new_items.insert(rec.key.clone(), facts);
        }
        let drift = compute_drift(loaded, &items, &new_items);
        let mut f = self.facts.lock().unwrap_or_else(|p| p.into_inner());
        if f.generation != before.generation {
            // Something changed while git was asked: what this pass saw may
            // predate it, and the next pass asks again.
            return;
        }
        if let Some(ids) = identities {
            f.identities = ids;
            f.identities_at = Some(now);
        }
        f.items = new_items;
        f.drift = drift;
        f.stale = false;
    }
}

/// Where a definition repo's shared clone is.
pub fn clone_dir(loaded: &Loaded, repo: &str) -> Option<PathBuf> {
    loaded
        .stack
        .repos()
        .get(repo)
        .map(|r| loaded.stack.repo_dir(&loaded.dir, &r.path))
}

/// Each definition repo named the way PR rows name it: the `work.repos`
/// remote override, else the clone's origin, classified.
pub fn repo_identities(
    git: &Git<'_>,
    loaded: &Loaded,
) -> IndexMap<String, Result<Option<RepoIdentity>, String>> {
    let work = loaded.stack.work_def();
    let hosts = override_hosts(work.repos.values().filter_map(|r| r.remote.clone()));
    let mut out = IndexMap::new();
    for (name, r) in loaded.stack.repos() {
        let remote = match work.repos.get(&name).and_then(|o| o.remote.clone()) {
            Some(r) => Ok(r),
            None => git.remote_url(&loaded.stack.repo_dir(&loaded.dir, &r.path)),
        };
        out.insert(name, remote.map(|url| classify_remote(&url, &hosts)));
    }
    out
}

/// Drift of the slot's active item: each component built from one of its
/// worktrees whose HEAD has moved since, unless dismissed at that HEAD.
fn compute_drift(
    loaded: &Loaded,
    items: &[(PathBuf, ItemRecord)],
    facts: &IndexMap<String, ItemFacts>,
) -> Vec<Drift> {
    let Some(active) = &loaded.binding.item else {
        return Vec::new();
    };
    let Some((_, rec)) = items.iter().find(|(_, r)| &r.key == active) else {
        return Vec::new();
    };
    let Some(f) = facts.get(active) else {
        return Vec::new();
    };
    let cache = ProvenanceCache::load(&loaded.work_dir(), &loaded.instance_id());
    let mut out = Vec::new();
    for (component, p) in &cache.components {
        if !p.mode.uses_repo() {
            continue;
        }
        let Some(repo) = loaded.stack.repo_of(component, p.mode) else {
            continue;
        };
        let (Some(r), Some(built)) = (rec.repos.get(&repo), p.commit.as_ref()) else {
            continue;
        };
        let Some(head) = f.repos.get(&repo).and_then(|x| x.head.clone()) else {
            continue;
        };
        if &head == built || rec.keep_as_built.get(component) == Some(&head) {
            continue;
        }
        out.push(Drift {
            item: rec.key.clone(),
            component: component.clone(),
            repo,
            branch: r.branch.clone(),
            mode: p.mode,
            built: built.clone(),
            head,
        });
    }
    out
}

/// The Environment view's alert per drifted component.
pub fn drift_alerts(facts: &WorkFacts) -> Vec<Value> {
    facts
        .drift
        .iter()
        .map(|d| {
            json!({
                "text": format!("{} ({})", d.line(), d.item),
                "severity": "warn",
                "componentId": d.component,
            })
        })
        .collect()
}

// --------------------------------------------------------------- binding

/// The binding `rec` gives each component: a component whose source or
/// dev repo is one of the item's builds (source) or runs (dev) from the
/// item's branch, keeping its current mode when that one uses the repo,
/// else source, else dev.
pub fn bind_targets(
    runner: &StackRunner<'_>,
    obs: &Observation,
    rec: &ItemRecord,
) -> IndexMap<String, BindTarget> {
    let stack = runner.stack;
    let mut out = IndexMap::new();
    for (name, c) in &stack.components {
        let usable: Vec<Mode> = [Mode::Source, Mode::Dev]
            .into_iter()
            .filter(|m| c.has_mode(*m))
            .filter(|m| {
                stack
                    .repo_of(name, *m)
                    .is_some_and(|r| rec.repos.contains_key(&r))
            })
            .collect();
        if usable.is_empty() {
            continue;
        }
        let current = mode_now(runner, obs, name);
        let mode = match current {
            Some(m) if usable.contains(&m) => m,
            _ if usable.contains(&Mode::Source) => Mode::Source,
            _ => Mode::Dev,
        };
        let repo = stack.repo_of(name, mode).expect("usable modes name a repo");
        out.insert(
            name.clone(),
            BindTarget {
                mode,
                git_ref: rec.repos[&repo].branch.clone(),
                rebuild: false,
            },
        );
    }
    out
}

/// Components the previous item bound that the new one does not: back to
/// their default mode when that one needs no repo.
fn releases(
    runner: &StackRunner<'_>,
    obs: &Observation,
    previous: Option<&ItemRecord>,
    bind: &IndexMap<String, BindTarget>,
) -> IndexMap<String, Mode> {
    let mut out = IndexMap::new();
    let Some(prev) = previous else {
        return out;
    };
    for (name, c) in &runner.stack.components {
        if bind.contains_key(name) {
            continue;
        }
        let Some(m) = mode_now(runner, obs, name).filter(|m| m.uses_repo()) else {
            continue;
        };
        if !runner
            .stack
            .repo_of(name, m)
            .is_some_and(|r| prev.repos.contains_key(&r))
        {
            continue;
        }
        if let Some(d) = c.default_mode().filter(|d| !d.uses_repo()) {
            out.insert(name.clone(), d);
        }
    }
    out
}

/// The `activate` change for `rec` and the first sentence of its confirm.
pub fn activation(
    loaded: &Loaded,
    runner: &StackRunner<'_>,
    obs: &Observation,
    rec: &ItemRecord,
) -> (Change, String) {
    let bind = bind_targets(runner, obs, rec);
    let previous = loaded
        .binding
        .item
        .as_deref()
        .filter(|p| *p != rec.key)
        .and_then(|p| {
            ItemRecord::load(&ItemRecord::dir(&loaded.work_dir(), p))
                .ok()
                .flatten()
        });
    let release = releases(runner, obs, previous.as_ref(), &bind);
    let mut parts: Vec<String> = bind
        .iter()
        .map(|(c, t)| {
            let verb = if t.mode == Mode::Dev { "run" } else { "build" };
            format!("{verb} {c} from {}", t.git_ref)
        })
        .collect();
    parts.extend(release.iter().map(|(c, m)| format!("{c} back to {m}")));
    let head = if parts.is_empty() {
        format!(
            "Bind slot {} to {}: nothing in the stack builds from its repos yet; everything else unchanged.",
            loaded.slot, rec.item_ref.key
        )
    } else {
        format!(
            "Bind slot {} to {}: {}; others unchanged.",
            loaded.slot,
            rec.item_ref.key,
            parts.join(", ")
        )
    };
    (
        Change::Bind {
            item: rec.key.clone(),
            bind,
            release,
        },
        head,
    )
}

/// [`activation`] with the automatic save of the item the slot leaves (a
/// rebind), or, with `no_save`, the confirm saying it is not saved.
pub fn activation_saving(
    loaded: &Loaded,
    runner: &StackRunner<'_>,
    obs: &Observation,
    rec: &ItemRecord,
    no_save: bool,
) -> (Change, String, Option<crate::states::SaveSpec>) {
    let (change, head) = activation(loaded, runner, obs, rec);
    match crate::states::bound_item(loaded).filter(|p| *p != rec.key) {
        Some(prev) if no_save => (
            change,
            format!("{head} Without saving the stack state of {prev} first."),
            None,
        ),
        Some(prev) => (
            change,
            format!(
                "Saves the stack state of {prev} first (if that fails, nothing else runs). {head}"
            ),
            Some(crate::states::SaveSpec {
                item: prev.to_string(),
                reason: stack_core::states::SaveReason::BeforeRebind,
                skip_in_volume: false,
            }),
        ),
        None => (change, head, None),
    }
}

/// One way to archive an item, as offered.
pub struct ArchiveVariant {
    pub id: String,
    pub label: String,
    pub confirm: String,
    pub save: Option<crate::states::SaveSpec>,
    pub keep_data: bool,
}

/// `archive` (saving the stack state first when the item is the one the
/// slot runs), `archive-no-save` beside it then, and `archive-keep-data`
/// when a sensitive state has host copies that archive would delete.
pub fn archive_variants(loaded: &Loaded, dir: &Path, rec: &ItemRecord) -> Vec<ArchiveVariant> {
    let saves = crate::states::bound_item(loaded) == Some(rec.key.as_str());
    let sensitive_host = stack_core::states::list_states(dir).iter().any(|s| {
        s.sensitive
            && s.live_snapshots()
                .any(|x| x.location == stack_core::definition::SnapshotLocation::Host)
    });
    let base = archive_confirm(rec);
    let drop = if sensitive_host {
        " Sensitive saved states keep only their references: their snapshots' host copies are deleted."
    } else {
        ""
    };
    let save_spec = || crate::states::SaveSpec {
        item: rec.key.clone(),
        reason: stack_core::states::SaveReason::BeforeArchive,
        skip_in_volume: false,
    };
    let first = |s: String| {
        if saves {
            format!(
                "Saves the stack state of {} first (if that fails, nothing else runs). {s}",
                rec.item_ref.key
            )
        } else {
            s
        }
    };
    let mut out = vec![ArchiveVariant {
        id: ARCHIVE_ID.to_string(),
        label: "Archive".to_string(),
        confirm: first(format!("{base}{drop}")),
        save: saves.then(save_spec),
        keep_data: false,
    }];
    if saves {
        out.push(ArchiveVariant {
            id: format!("{ARCHIVE_ID}{}", crate::states::NO_SAVE_SUFFIX),
            label: "Archive without saving".to_string(),
            confirm: format!("{base}{drop} Without saving the stack state first."),
            save: None,
            keep_data: false,
        });
    }
    if sensitive_host {
        out.push(ArchiveVariant {
            id: format!("{ARCHIVE_ID}-keep-data"),
            label: "Archive and keep data".to_string(),
            confirm: first(format!(
                "{base} Sensitive saved states keep their snapshots' host copies (inside the work folder)."
            )),
            save: saves.then(save_spec),
            keep_data: true,
        });
    }
    out
}

/// The `rebuild-changed` change: the drifted components, rebuilt from
/// where their branches are now.
pub fn rebuild_change(rec: &ItemRecord, drift: &[Drift]) -> (Change, String) {
    let mine: Vec<&Drift> = drift.iter().filter(|d| d.item == rec.key).collect();
    let bind = mine
        .iter()
        .map(|d| {
            (
                d.component.clone(),
                BindTarget {
                    mode: d.mode,
                    git_ref: d.branch.clone(),
                    rebuild: true,
                },
            )
        })
        .collect();
    let parts: Vec<String> = mine
        .iter()
        .map(|d| {
            format!(
                "{} from {} at {} (built from {})",
                d.component,
                d.branch,
                c8(&d.head),
                c8(&d.built)
            )
        })
        .collect();
    (
        Change::Bind {
            item: rec.key.clone(),
            bind,
            release: IndexMap::new(),
        },
        format!("Rebuild {}.", parts.join(", ")),
    )
}

/// A whole confirm: the sentence, then what the plan runs.
pub fn full_confirm(head: &str, plan: &Plan) -> String {
    let steps = confirm_text(plan);
    if steps.is_empty() {
        head.to_string()
    } else {
        format!("{head} {steps}")
    }
}

/// The value an action's `params.fingerprint` carries: the instance's
/// state and the confirm text, so a call is refused when either changed.
pub fn fingerprint(state: &str, confirm: &str) -> String {
    let mut h = Sha256::new();
    h.update(state.as_bytes());
    h.update([0u8]);
    h.update(confirm.as_bytes());
    h.finalize()
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}

// ---------------------------------------------------------- target item

/// The tracker's row an item action was sent with, checked. Every field is
/// untrusted: lengths are capped, control characters dropped, the key is
/// required, a URL must be http(s), branches must be usable names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetItem {
    pub source: String,
    pub key: String,
    pub title: Option<String>,
    pub url: Option<String>,
    pub status: Option<ItemStatus>,
    pub prs: Vec<TargetPr>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetPr {
    pub forge: String,
    pub repo: String,
    pub number: String,
    pub source_branch: Option<String>,
}

fn clean(text: &str, cap: usize) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .take(cap)
        .collect::<String>()
        .trim()
        .to_string()
}

fn str_field(v: &Value, name: &str, cap: usize) -> Option<String> {
    v.get(name)
        .and_then(Value::as_str)
        .map(|s| clean(s, cap))
        .filter(|s| !s.is_empty())
}

/// Refresh what the workspace in `dir` remembers of its owner's row
/// (title, link, status: "done in tracker" and the tracker-status banner
/// read it) from `item`, the owner's row as the app last received it.
/// Checked like Start work's row; a row that does not parse, or names
/// another key, changes nothing. Saved only when something changed.
/// Whether it did.
pub fn refresh_owner(dir: &Path, rec: &mut ItemRecord, item: &Value, now_utc: &str) -> bool {
    if !item.is_object() {
        return false;
    }
    let owner = json!({
        "view": "work",
        "key": rec.item_ref.key,
        "source": rec.item_ref.source,
        "item": item,
    });
    let Ok(t) = parse_target_item(&owner) else {
        return false;
    };
    let title = t.title.or(rec.title.clone());
    let url = t.url.or(rec.url.clone());
    let status = t.status.or(rec.status.clone());
    if title == rec.title && url == rec.url && status == rec.status {
        return false;
    }
    rec.title = title;
    rec.url = url;
    rec.status = status;
    rec.updated_utc = now_utc.to_string();
    rec.save(dir).is_ok()
}

/// `POST /api/work/owner` (the manifest's `ownerUpdates`): the app pushes
/// an owner row it polled, `{source, key, item}`, when its content changed.
/// A row this source has a workspace for (exactly that source and key)
/// refreshes the workspace's copy; any other row is ignored with `200`,
/// since the app sends every row this source adds to and need not know
/// which are current. A malformed body is `400`.
pub fn owner_update(work_dir: &Path, body: &Value, now_utc: &str) -> (u16, Value) {
    let field = |name: &str| {
        body.get(name)
            .and_then(Value::as_str)
            .filter(|v| !v.trim().is_empty() && v.chars().count() <= KEY_CAP)
            .filter(|v| !v.chars().any(char::is_control))
    };
    let (Some(source), Some(key)) = (field("source"), field("key")) else {
        return (
            400,
            json!({ "kind": "bad-request", "message": "the body needs `source` and `key` strings" }),
        );
    };
    let Some(item) = body.get("item").filter(|i| i.is_object()) else {
        return (
            400,
            json!({ "kind": "bad-request", "message": "the body carries no `item` object" }),
        );
    };
    let found = list_items(work_dir)
        .into_iter()
        .find(|(_, r)| r.item_ref.source == source && r.item_ref.key == key);
    let Some((dir, mut rec)) = found else {
        return (
            200,
            json!({ "message": format!("no workspace for {source}:{key}; ignored") }),
        );
    };
    let message = if refresh_owner(&dir, &mut rec, item, now_utc) {
        format!("{key}: the tracker's row was updated")
    } else {
        format!("{key}: nothing changed")
    };
    (200, json!({ "message": message }))
}

/// `target` of a `start-work` call: `{view: "work", key, source, item}`.
pub fn parse_target_item(target: &Value) -> Result<TargetItem, String> {
    let source = str_field(target, "source", KEY_CAP).ok_or("the call names no `target.source`")?;
    let key = target
        .get("key")
        .and_then(Value::as_str)
        .ok_or("the call names no `target.key`")?;
    if key.chars().count() > KEY_CAP || key.chars().any(char::is_control) || key.trim().is_empty() {
        return Err("`target.key` is empty, too long or has control characters".into());
    }
    let item = target
        .get("item")
        .filter(|i| i.is_object())
        .ok_or("the call carries no `target.item` object")?;
    if let Some(k) = item.get("key") {
        if k.as_str() != Some(key) {
            return Err("`target.item.key` is not `target.key`".into());
        }
    }
    let url = str_field(item, "url", URL_CAP).filter(|u| {
        let l = u.to_ascii_lowercase();
        l.starts_with("https://") || l.starts_with("http://")
    });
    let status = item.get("status").and_then(|s| {
        Some(ItemStatus {
            label: str_field(s, "label", LABEL_CAP)?,
            category: str_field(s, "category", 10)
                .filter(|c| matches!(c.as_str(), "todo" | "doing" | "done")),
        })
    });
    let prs = match item.get("prs") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(list)) => list
            .iter()
            .take(PRS_CAP)
            .filter_map(|p| {
                let forge = str_field(p, "forge", 40)?;
                let repo = str_field(p, "repo", KEY_CAP)?;
                let number = match p.get("number") {
                    Some(Value::String(s)) => clean(s, 40),
                    Some(Value::Number(n)) => n.to_string(),
                    _ => String::new(),
                };
                let source_branch = str_field(p, "sourceBranch", 256)
                    .map(|b| {
                        b.strip_prefix("refs/heads/")
                            .map(str::to_string)
                            .unwrap_or(b)
                    })
                    .filter(|b| is_valid_branch(b));
                Some(TargetPr {
                    forge,
                    repo,
                    number,
                    source_branch,
                })
            })
            .collect(),
        Some(_) => return Err("`target.item.prs` is not a list".into()),
    };
    Ok(TargetItem {
        source,
        key: key.to_string(),
        title: str_field(item, "title", TITLE_CAP),
        url,
        status,
        prs,
    })
}

// ------------------------------------------------------------ job work

/// A git change to an item workspace, stored with its job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "op")]
pub enum WorkspaceOp {
    /// Worktrees for the repos the item already has PR branches in.
    StartWork {
        item: String,
        checkouts: Vec<Checkout>,
        /// Repos skipped, one line each, for the job log.
        notes: Vec<String>,
    },
    AddRepo {
        item: String,
        checkout: Checkout,
    },
    Archive {
        item: String,
        /// Save the item's stack state first (it is active in the slot).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        save: Option<crate::states::SaveSpec>,
        /// Keep the host copies of sensitive states' snapshots.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        keep_data: bool,
    },
    Restore {
        item: String,
    },
}

/// One worktree to create.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Checkout {
    pub repo: String,
    pub branch: String,
    pub how: CheckoutHow,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum CheckoutHow {
    /// The branch exists on origin; check it out.
    Existing,
    /// The branch does not exist; create it from origin's `base`.
    Create { base: String },
}

/// The confirm of an Add repo, from what will happen.
pub fn add_repo_confirm(c: &Checkout) -> String {
    match &c.how {
        CheckoutHow::Existing => format!("Check out existing branch `{}` in {}", c.branch, c.repo),
        CheckoutHow::Create { base } => {
            format!("Create branch `{}` from `{base}` in {}", c.branch, c.repo)
        }
    }
}

/// A job's failure: exit code and the reason.
pub type OpFailure = (i32, String);

/// Run one workspace job; `say` writes a line to the pane and the log.
pub fn run_op(
    world: &World,
    loaded: &Loaded,
    op: &WorkspaceOp,
    say: &mut dyn FnMut(&str),
) -> Result<(), OpFailure> {
    let git = Git::new(&*world.git_cli);
    let work_dir = loaded.work_dir();
    let now = world.clock.now_utc();
    match op {
        WorkspaceOp::StartWork {
            item,
            checkouts,
            notes,
        } => {
            let dir = ItemRecord::dir(&work_dir, item);
            let mut rec = load_record(&dir, item)?;
            for n in notes {
                say(&format!("· {n}"));
            }
            let mut failed = Vec::new();
            for c in checkouts {
                if rec.repos.contains_key(&c.repo) {
                    say(&format!("· {}: already has a worktree", c.repo));
                    continue;
                }
                if let Err(e) = add_worktree(&git, loaded, &dir, &mut rec, c, &now, say) {
                    say(&format!("✗ {}: {e}", c.repo));
                    failed.push(c.repo.clone());
                }
            }
            if checkouts.is_empty() {
                say("no worktree to create yet; add repos from the item's row");
            }
            if failed.is_empty() {
                Ok(())
            } else {
                Err((
                    1,
                    format!("could not create the worktree of {}", failed.join(", ")),
                ))
            }
        }
        WorkspaceOp::AddRepo { item, checkout } => {
            let dir = ItemRecord::dir(&work_dir, item);
            let mut rec = load_record(&dir, item)?;
            if rec.repos.contains_key(&checkout.repo) {
                return Err((
                    2,
                    format!("{item} already has a worktree of {}", checkout.repo),
                ));
            }
            let clone = clone_dir(loaded, &checkout.repo)
                .ok_or_else(|| (2, format!("the stack has no repo `{}`", checkout.repo)))?;
            say(&format!(
                "looking for `{}` on origin of {}…",
                checkout.branch, checkout.repo
            ));
            let now_there = git
                .remote_branch(&clone, &checkout.branch)
                .map_err(|e| (1, format!("cannot ask origin: {e}")))?
                .is_some();
            let expected = matches!(checkout.how, CheckoutHow::Existing);
            if now_there != expected {
                return Err((
                    2,
                    format!(
                        "conflict (409): origin {} `{}` now, which is not what was confirmed; nothing was done. Look again and confirm the new action",
                        if now_there { "has" } else { "no longer has" },
                        checkout.branch
                    ),
                ));
            }
            add_worktree(&git, loaded, &dir, &mut rec, checkout, &now, say).map_err(|e| (1, e))
        }
        WorkspaceOp::Archive {
            item, keep_data, ..
        } => archive(&git, loaded, item, *keep_data, &now, say),
        WorkspaceOp::Restore { item } => restore(&git, loaded, item, &now, say),
    }
}

fn load_record(dir: &Path, item: &str) -> Result<ItemRecord, OpFailure> {
    ItemRecord::load(dir)
        .map_err(|e| (2, e))?
        .ok_or_else(|| (2, format!("no workspace {item}")))
}

/// Fetch, check the path, add the worktree, record it.
fn add_worktree(
    git: &Git<'_>,
    loaded: &Loaded,
    item_dir: &Path,
    rec: &mut ItemRecord,
    c: &Checkout,
    now: &str,
    say: &mut dyn FnMut(&str),
) -> Result<(), String> {
    let clone =
        clone_dir(loaded, &c.repo).ok_or_else(|| format!("the stack has no repo `{}`", c.repo))?;
    if !clone.exists() {
        return Err(format!("the shared clone is not at {}", clone.display()));
    }
    // The folder's label is always 8 characters, so the final length is
    // known before git is asked anything that changes the clone.
    let longest = item_dir
        .join(SRC_DIR)
        .join(format!("{}-{}", c.repo, "0".repeat(8)));
    check_path_length(&longest, cfg!(windows), git.long_paths(&clone))?;
    let start = match &c.how {
        CheckoutHow::Existing => {
            say(&format!("▶ {}: fetch `{}`", c.repo, c.branch));
            git.fetch_branch(&clone, &c.branch)?
        }
        CheckoutHow::Create { base } => {
            if git.local_branch_exists(&clone, &c.branch) {
                return Err(format!(
                    "the clone already has a local branch `{}`; nothing was created",
                    c.branch
                ));
            }
            say(&format!("▶ {}: fetch `{base}`", c.repo));
            git.fetch_branch(&clone, base)?
        }
    };
    let rel = PathBuf::from(SRC_DIR).join(format!("{}-{}", c.repo, c8(&start)));
    let path = item_dir.join(&rel);
    match &c.how {
        CheckoutHow::Existing => {
            say(&format!(
                "▶ {}: worktree of `{}` at {}",
                c.repo,
                c.branch,
                rel.display()
            ));
            git.worktree_add_existing(&clone, &path, &c.branch)?;
        }
        CheckoutHow::Create { base } => {
            say(&format!(
                "▶ {}: new branch `{}` from `{base}` at {}",
                c.repo,
                c.branch,
                rel.display()
            ));
            git.worktree_add_new(
                &clone,
                &path,
                &c.branch,
                &format!("refs/remotes/origin/{base}"),
            )?;
        }
    }
    rec.repos.insert(
        c.repo.clone(),
        ItemRepo {
            branch: c.branch.clone(),
            path: rel,
            c8: c8(&start),
            base: loaded.stack.work_def().base_of(&c.repo).to_string(),
            created_utc: now.to_string(),
        },
    );
    rec.updated_utc = now.to_string();
    rec.save(item_dir)?;
    say(&format!("✓ {}: {}", c.repo, path.display()));
    Ok(())
}

/// What keeps an item from being archived: each worktree with uncommitted
/// changes or commits origin does not have, asked live.
pub fn archive_blockers(git: &Git<'_>, item_dir: &Path, rec: &ItemRecord) -> Vec<String> {
    let mut out = Vec::new();
    for (repo, r) in &rec.repos {
        let wt = item_dir.join(&r.path);
        if !wt.exists() {
            continue;
        }
        let f = git.facts(&wt, &r.base);
        if let Some(e) = &f.error {
            out.push(format!("{repo}: cannot be checked ({e})"));
            continue;
        }
        if f.dirty > 0 {
            out.push(format!(
                "{repo}: {} uncommitted change{}",
                f.dirty,
                plural(f.dirty)
            ));
        }
        if f.unpushed > 0 {
            out.push(format!(
                "{repo}: {} unpushed commit{}",
                f.unpushed,
                plural(f.unpushed)
            ));
        }
    }
    out
}

fn plural(n: u32) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

fn archive(
    git: &Git<'_>,
    loaded: &Loaded,
    item: &str,
    keep_data: bool,
    now: &str,
    say: &mut dyn FnMut(&str),
) -> Result<(), OpFailure> {
    let work_dir = loaded.work_dir();
    let dir = ItemRecord::dir(&work_dir, item);
    let mut rec = load_record(&dir, item)?;
    let blockers = archive_blockers(git, &dir, &rec);
    if !blockers.is_empty() {
        return Err((
            2,
            format!(
                "not archived, commit and push first: {}",
                blockers.join("; ")
            ),
        ));
    }
    let target = ItemRecord::archived_dir(&work_dir, item);
    if target.exists() {
        return Err((
            2,
            format!("{} already exists; nothing was archived", target.display()),
        ));
    }
    for (repo, r) in &rec.repos {
        let wt = dir.join(&r.path);
        if !wt.exists() {
            say(&format!("· {repo}: no worktree at {}", r.path.display()));
            continue;
        }
        let clone = clone_dir(loaded, repo)
            .ok_or_else(|| (1, format!("the stack has no repo `{repo}`")))?;
        say(&format!("▶ {repo}: remove worktree {}", r.path.display()));
        git.worktree_remove(&clone, &wt).map_err(|e| (1, e))?;
    }
    let _ = fs::remove_dir(dir.join(SRC_DIR));
    // States stay with the item; a sensitive one keeps only its references.
    if !keep_data {
        crate::states::drop_sensitive_host_data(&dir, now, say);
    }
    rec.archived_utc = Some(now.to_string());
    rec.updated_utc = now.to_string();
    rec.save(&dir).map_err(|e| (1, e))?;
    fs::create_dir_all(work_dir.join(ARCHIVE_DIR)).map_err(|e| (1, e.to_string()))?;
    fs::rename(&dir, &target).map_err(|e| (1, format!("cannot move {}: {e}", dir.display())))?;
    say(&format!("✓ moved to {}", target.display()));
    let mut active = ActiveRecord::load(&work_dir).map_err(|e| (1, e.to_string()))?;
    let before = active.slots.len();
    active.slots.retain(|_, s| s.item != item);
    if active.slots.len() != before {
        active.save(&work_dir).map_err(|e| (1, e.to_string()))?;
        say("✓ no longer the active item");
    }
    Ok(())
}

fn restore(
    git: &Git<'_>,
    loaded: &Loaded,
    item: &str,
    now: &str,
    say: &mut dyn FnMut(&str),
) -> Result<(), OpFailure> {
    let work_dir = loaded.work_dir();
    let from = ItemRecord::archived_dir(&work_dir, item);
    let dir = ItemRecord::dir(&work_dir, item);
    let mut rec = load_record(&from, item)?;
    if dir.exists() {
        return Err((2, format!("{} exists; nothing was restored", dir.display())));
    }
    fs::rename(&from, &dir).map_err(|e| (1, format!("cannot move {}: {e}", from.display())))?;
    say(&format!("✓ moved back to {}", dir.display()));
    let mut failed = Vec::new();
    for (repo, r) in &rec.repos {
        let Some(clone) = clone_dir(loaded, repo) else {
            say(&format!("✗ {repo}: the stack no longer has this repo"));
            failed.push(repo.clone());
            continue;
        };
        let wt = dir.join(&r.path);
        let outcome = (|| {
            check_path_length(&wt, cfg!(windows), git.long_paths(&clone))?;
            if !git.local_branch_exists(&clone, &r.branch) {
                say(&format!("▶ {repo}: fetch `{}`", r.branch));
                git.fetch_branch(&clone, &r.branch)?;
            }
            say(&format!(
                "▶ {repo}: worktree of `{}` at {}",
                r.branch,
                r.path.display()
            ));
            git.worktree_add_existing(&clone, &wt, &r.branch)
        })();
        match outcome {
            Ok(()) => say(&format!("✓ {repo}: {}", wt.display())),
            Err(e) => {
                say(&format!("✗ {repo}: {e}"));
                failed.push(repo.clone());
            }
        }
    }
    rec.archived_utc = None;
    rec.updated_utc = now.to_string();
    rec.save(&dir).map_err(|e| (1, e))?;
    if failed.is_empty() {
        Ok(())
    } else {
        Err((
            1,
            format!("could not recreate the worktree of {}", failed.join(", ")),
        ))
    }
}

// ------------------------------------------------------------ the views

/// What the Work view and the actions need to know about the instance.
pub struct WorkInputs<'a, 'r> {
    pub world: &'a World,
    /// What the last resume calls found blocked, per state.
    pub blocks: &'a crate::states::ResumeBlocks,
    pub loaded: &'a Loaded,
    pub runner: &'a StackRunner<'r>,
    /// The instance as last observed; `None` when the engines could not be
    /// asked (Activate and Rebuild changed are then not offered).
    pub obs: Option<&'a Observation>,
    pub facts: &'a WorkFacts,
    pub now_utc: &'a str,
}

fn badge(text: impl Into<String>, tone: &str) -> Value {
    json!({ "text": text.into(), "tone": tone })
}

/// The branch the naming pattern gives `rec`.
pub fn pattern_branch(loaded: &Loaded, rec: &ItemRecord) -> Result<String, String> {
    fill_branch(
        &loaded.stack.work_def().branch,
        &rec.key,
        &slug(rec.title.as_deref().unwrap_or("")),
    )
}

/// The Add repo checkout of `repo` for `rec`, when the cache knows
/// whether origin has the branch.
pub fn planned_checkout(
    loaded: &Loaded,
    facts: &WorkFacts,
    rec: &ItemRecord,
    repo: &str,
) -> Option<Checkout> {
    let branch = pattern_branch(loaded, rec).ok()?;
    let exists = facts.items.get(&rec.key)?.remote.get(repo)?.0?;
    Some(Checkout {
        repo: repo.to_string(),
        branch,
        how: if exists {
            CheckoutHow::Existing
        } else {
            CheckoutHow::Create {
                base: loaded.stack.work_def().base_of(repo).to_string(),
            }
        },
    })
}

pub(crate) fn endpoint(
    id: &str,
    label: &str,
    mutates: bool,
    confirm: Option<String>,
    params: Value,
) -> Value {
    let mut a = json!({ "kind": "endpoint", "id": id, "label": label, "mutates": mutates, "params": params });
    if let Some(c) = confirm {
        a["confirm"] = Value::String(c);
    }
    a
}

/// The archive confirm.
fn archive_confirm(rec: &ItemRecord) -> String {
    if rec.repos.is_empty() {
        format!(
            "Archive {}: move its folder to {ARCHIVE_DIR}/{}.",
            rec.item_ref.key, rec.key
        )
    } else {
        format!(
            "Archive {}: remove the worktrees of {} (refused if any has uncommitted or unpushed work) and move the folder to {ARCHIVE_DIR}/{}.",
            rec.item_ref.key,
            rec.repos.keys().cloned().collect::<Vec<_>>().join(", "),
            rec.key
        )
    }
}

/// "detached at <c8> — switch back to <branch>" while the worktree's HEAD
/// is on no branch.
fn detached_text(f: &RepoFacts, r: &ItemRepo) -> Option<String> {
    if f.error.is_some() || f.branch.is_some() {
        return None;
    }
    let head = f.head.as_deref()?;
    Some(format!(
        "detached at {} — switch back to {}",
        c8(head),
        r.branch
    ))
}

/// The overlay entry of one item.
pub fn overlay(inp: &WorkInputs<'_, '_>, dir: &Path, rec: &ItemRecord) -> Value {
    let loaded = inp.loaded;
    let active = loaded.binding.item.as_deref() == Some(rec.key.as_str());
    let facts = inp.facts.items.get(&rec.key);
    let repos = loaded.stack.repos();
    let mut branches = Vec::new();
    for (repo, r) in &rec.repos {
        if let Some(Ok(Some(id))) = inp.facts.identities.get(repo) {
            branches.push(json!({ "forge": id.forge, "repo": id.repo, "branch": r.branch }));
        }
    }
    let mut badges = Vec::new();
    match facts {
        None => badges.push(badge("checking git…", "muted")),
        Some(f) => {
            let dirty: u32 = f.repos.values().map(|r| r.dirty).sum();
            let unpushed: u32 = f.repos.values().map(|r| r.unpushed).sum();
            if dirty > 0 {
                badges.push(badge(format!("{dirty} uncommitted"), "warn"));
            }
            if unpushed > 0 {
                badges.push(badge(format!("{unpushed} unpushed"), "warn"));
            }
        }
    }
    if active {
        badges.push(badge(format!("active in slot {}", loaded.slot), "ok"));
    } else {
        badges.push(badge("not active", "muted"));
    }
    if inp.facts.drift.iter().any(|d| d.item == rec.key) {
        badges.push(badge("built from older commits", "warn"));
    }
    if rec.done_in_tracker() {
        badges.push(badge("done in tracker — archive?", "info"));
    }
    if let Some(f) = facts {
        for (repo, r) in &rec.repos {
            if let Some(text) = f.repos.get(repo).and_then(|x| detached_text(x, r)) {
                badges.push(badge(format!("{repo} {text}"), "warn"));
            }
        }
    }

    let mut rows = Vec::new();
    for (repo, r) in &rec.repos {
        let f = facts.and_then(|f| f.repos.get(repo));
        let mut parts = vec![r.branch.clone(), r.c8.clone()];
        let mut row_badges = Vec::new();
        match f {
            None => parts.push("checking…".into()),
            Some(f) if f.error.is_some() => {
                row_badges.push(json!({ "text": "git failed", "tone": "bad", "title": f.error }));
            }
            Some(f) => {
                if let (Some(a), Some(b)) = (f.ahead, f.behind) {
                    parts.push(format!("{a} ahead, {b} behind {}", r.base));
                }
                if f.dirty > 0 {
                    row_badges.push(badge(format!("{} uncommitted", f.dirty), "warn"));
                }
                if f.unpushed > 0 {
                    row_badges.push(badge(format!("{} unpushed", f.unpushed), "warn"));
                }
                if f.branch.as_deref().is_some_and(|b| b != r.branch) {
                    row_badges.push(badge(
                        format!("on {}", f.branch.clone().unwrap_or_default()),
                        "warn",
                    ));
                }
                if let Some(text) = detached_text(f, r) {
                    row_badges.push(badge(text, "warn"));
                }
            }
        }
        let mut row = json!({ "label": repo, "value": parts.join(" · ") });
        if !row_badges.is_empty() {
            row["badges"] = Value::Array(row_badges);
        }
        rows.push(row);
    }
    for repo in repos.keys().filter(|r| !rec.repos.contains_key(*r)) {
        rows.push(json!({ "label": repo, "value": "not in this item" }));
    }

    let mut actions = Vec::new();
    if !active {
        if let Some(obs) = inp.obs {
            for no_save in [false, true] {
                let (change, head, save) = activation_saving(loaded, inp.runner, obs, rec, no_save);
                if no_save && save.is_none() && crate::states::bound_item(loaded).is_none() {
                    break;
                }
                if let Ok(plan) = inp.runner.plan(obs, &change) {
                    let confirm = full_confirm(&head, &plan);
                    let fp = fingerprint(&obs.state.fingerprint(), &confirm);
                    let (id, label) = if no_save {
                        (
                            format!("{ACTIVATE_ID}{}", crate::states::NO_SAVE_SUFFIX),
                            format!("Activate in slot {} without saving", loaded.slot),
                        )
                    } else {
                        (
                            ACTIVATE_ID.to_string(),
                            format!("Activate in slot {}", loaded.slot),
                        )
                    };
                    actions.push(endpoint(
                        &id,
                        &label,
                        true,
                        Some(confirm),
                        json!({ "fingerprint": fp }),
                    ));
                }
            }
        }
    }
    for repo in repos.keys().filter(|r| !rec.repos.contains_key(*r)) {
        if let Some(c) = planned_checkout(loaded, inp.facts, rec, repo) {
            let expect = match c.how {
                CheckoutHow::Existing => "existing",
                CheckoutHow::Create { .. } => "new",
            };
            actions.push(endpoint(
                &format!("{ADD_REPO_PREFIX}{repo}"),
                &format!("Add {repo}"),
                true,
                Some(add_repo_confirm(&c)),
                json!({ "repo": repo, "expect": expect, "branch": c.branch }),
            ));
        }
    }
    for (repo, r) in &rec.repos {
        actions.push(json!({
            "kind": "terminal",
            "id": format!("{TERMINAL_PREFIX}{repo}"),
            "label": format!("Terminal in {repo}"),
            "cwd": dir.join(&r.path).display().to_string(),
        }));
    }
    // A worktree left detached (a resume of the exact saved commits) keeps
    // a way back to the branch until HEAD is on one again.
    for (repo, r) in &rec.repos {
        if facts
            .and_then(|f| f.repos.get(repo))
            .and_then(|f| detached_text(f, r))
            .is_some()
        {
            actions.push(json!({
                "kind": "terminal",
                "id": format!("{SWITCH_PREFIX}{repo}"),
                "label": format!("Switch {repo} back to {}", r.branch),
                "cwd": dir.join(&r.path).display().to_string(),
                "command": ["git", "switch", r.branch],
            }));
        }
    }
    let tips: IndexMap<String, String> = facts
        .map(|f| {
            f.repos
                .iter()
                .filter_map(|(r, x)| x.tip.clone().map(|t| (r.clone(), t)))
                .collect()
        })
        .unwrap_or_default();
    let saved = crate::states::overlay_parts(
        inp.world,
        loaded,
        inp.obs,
        dir,
        rec,
        inp.blocks,
        &tips,
        inp.now_utc,
    );
    badges.extend(saved.badges);
    actions.extend(saved.actions);
    actions.extend(crate::seeds::overlay_action(loaded, rec));
    for v in archive_variants(loaded, dir, rec) {
        actions.push(endpoint(&v.id, &v.label, true, Some(v.confirm), json!({})));
    }
    let mut sections = vec![json!({ "id": "repos", "title": "Repos", "rows": rows })];
    sections.extend(saved.section);

    let status = local_status::overlay_parts(loaded, rec);
    let mut actions_all = status.actions;
    actions_all.extend(actions);
    let mut banners = Vec::new();
    banners.extend(drift_banner(inp, rec));
    banners.extend(status.banners);

    let mut entry = json!({
        "ref": { "source": rec.item_ref.source, "key": rec.item_ref.key },
        "key": rec.item_ref.key,
        "title": rec.title,
        "url": rec.url,
        "status": rec.status.as_ref().map(|s| json!({ "label": s.label, "category": s.category })),
        "folder": dir.display().to_string(),
        "active": active,
        "branches": branches,
        "badges": badges,
        "sections": sections,
        "banners": banners,
        "actions": actions_all,
    });
    if let Some(ls) = status.local_status {
        entry["localStatus"] = ls;
    }
    entry
}

/// The drift banner of the active item, with Rebuild changed (planned)
/// and Keep as built. It sits on the item's own entry; its actions still
/// name the item in `params.item`.
fn drift_banner(inp: &WorkInputs<'_, '_>, rec: &ItemRecord) -> Option<Value> {
    let mine: Vec<&Drift> = inp
        .facts
        .drift
        .iter()
        .filter(|d| d.item == rec.key)
        .collect();
    if mine.is_empty() {
        return None;
    }
    let mut actions = Vec::new();
    if let Some(obs) = inp.obs {
        let (change, head) = rebuild_change(rec, &inp.facts.drift);
        if let Ok(plan) = inp.runner.plan(obs, &change) {
            let confirm = full_confirm(&head, &plan);
            let fp = fingerprint(&obs.state.fingerprint(), &confirm);
            actions.push(endpoint(
                &format!("{REBUILD_PREFIX}{}", rec.key),
                "Rebuild changed",
                true,
                Some(confirm),
                json!({ "item": rec.key, "fingerprint": fp }),
            ));
        }
    }
    actions.push(endpoint(
        &format!("{KEEP_PREFIX}{}", rec.key),
        "Keep as built",
        false,
        None,
        json!({ "item": rec.key }),
    ));
    Some(json!({
        "id": format!("drift-{}", rec.key),
        "tone": "warn",
        "title": format!("{}: the stack runs older commits than its branches", rec.item_ref.key),
        "lines": mine.iter().map(|d| d.line()).collect::<Vec<_>>(),
        "actions": actions,
    }))
}

/// `GET /api/work`.
pub fn work_list(inp: &WorkInputs<'_, '_>) -> Value {
    let work_dir = inp.loaded.work_dir();
    let items = list_items(&work_dir);
    let mut errors = Vec::new();
    for (repo, id) in &inp.facts.identities {
        if let Err(e) = id {
            errors.push(json!({ "source": format!("git:{repo}"), "message": e }));
        }
    }
    let mut banners = Vec::new();
    let archived = list_archived(&work_dir);
    if !archived.is_empty() {
        banners.push(json!({
            "id": "archived",
            "tone": "info",
            "title": format!("{} archived workspace{}", archived.len(), if archived.len() == 1 { "" } else { "s" }),
            "actions": [{ "kind": "panel", "id": "archived", "label": "Archived", "endpoint": ARCHIVED_PATH }],
        }));
    }
    json!({
        "freshness": { "asOf": inp.now_utc, "errors": errors },
        "items": items.iter().map(|(d, r)| overlay(inp, d, r)).collect::<Vec<_>>(),
        "banners": banners,
    })
}

fn restore_confirm(rec: &ItemRecord) -> String {
    if rec.repos.is_empty() {
        format!(
            "Restore {}: move it back to the Work list.",
            rec.item_ref.key
        )
    } else {
        format!(
            "Restore {}: recreate the worktrees of {} and move it back to the Work list.",
            rec.item_ref.key,
            rec.repos
                .iter()
                .map(|(r, x)| format!("{r} (`{}`)", x.branch))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

/// `GET /api/work/archived`: the Archived panel.
pub fn archived_panel(loaded: &Loaded, now_utc: &str) -> Value {
    let sections: Vec<Value> = list_archived(&loaded.work_dir())
        .iter()
        .map(|(_, rec)| {
            let title = match &rec.title {
                Some(t) => format!("{} — {t}", rec.item_ref.key),
                None => rec.item_ref.key.clone(),
            };
            let mut rows: Vec<Value> = rec
                .repos
                .iter()
                .map(|(r, x)| json!({ "label": r, "value": format!("{} · {}", x.branch, x.c8) }))
                .collect();
            if let Some(at) = &rec.archived_utc {
                rows.push(json!({ "label": "Archived", "value": at }));
            }
            json!({
                "id": rec.key,
                "title": title,
                "rows": rows,
                "actions": [endpoint(
                    &format!("{RESTORE_PREFIX}{}", rec.key),
                    "Restore",
                    true,
                    Some(restore_confirm(rec)),
                    json!({ "item": rec.key }),
                )],
            })
        })
        .collect();
    json!({
        "title": "Archived workspaces",
        "freshness": { "asOf": now_utc, "errors": [] },
        "sections": sections,
    })
}

/// The manifest's `itemActions`.
pub fn item_actions(loaded: Option<&Loaded>) -> Value {
    let sources = loaded
        .map(|l| l.stack.work_def().sources)
        .unwrap_or_else(|| vec![ANY_SOURCE.to_string()]);
    json!([{
        "kind": "endpoint",
        "id": START_WORK_ID,
        "label": "Start work",
        "mutates": true,
        "confirm": "Create a workspace for {key} — {title}",
        "on": { "view": "work", "sources": sources },
    }])
}

// --------------------------------------------------------- acceptance

/// What an accepted Work action leads to.
#[derive(Debug)]
pub enum Accepted {
    /// Done now: `200 {message}`.
    Done(String),
    /// A job to store and run.
    Job {
        offered: Box<Offered>,
        mutates: bool,
    },
    /// Refused: status, kind, message.
    Refused(u16, &'static str, String),
}

pub(crate) fn refused(status: u16, kind: &'static str, message: impl Into<String>) -> Accepted {
    Accepted::Refused(status, kind, message.into())
}

pub(crate) fn job(id: &str, key: &str, label: &str, confirm: String, work: JobWork) -> Accepted {
    Accepted::Job {
        offered: Box::new(Offered {
            id: id.to_string(),
            key: Some(key.to_string()),
            label: label.to_string(),
            confirm,
            work,
            disabled: None,
        }),
        mutates: true,
    }
}

/// Accept (or refuse) a Work view action: `start-work` from another
/// source's row, the overlay's own actions, the drift banner's and the
/// Archived panel's. `actor` is the call's `X-Chappa-Actor`.
pub fn accept(
    world: &World,
    inp: &WorkInputs<'_, '_>,
    body: &Value,
    actor: Option<&str>,
) -> Accepted {
    let id = body.get("id").and_then(Value::as_str).unwrap_or("");
    let confirmed = body.get("confirmed").and_then(Value::as_bool) == Some(true);
    let target = body.get("target").cloned().unwrap_or(Value::Null);
    let params = body.get("params").cloned().unwrap_or(Value::Null);
    let work_dir = inp.loaded.work_dir();
    if id == START_WORK_ID {
        return start_work(world, inp, &target, confirmed, actor);
    }
    if let Some(item) = id.strip_prefix(RESTORE_PREFIX) {
        let Some((_, rec)) = list_archived(&work_dir)
            .into_iter()
            .find(|(_, r)| r.key == item)
        else {
            return refused(409, "not-offered", format!("no archived workspace {item}"));
        };
        if !confirmed {
            return refused(
                409,
                "confirmation-required",
                "a mutating action must be confirmed",
            );
        }
        if ItemRecord::dir(&work_dir, item).exists() {
            return refused(
                409,
                "conflict",
                format!("a workspace {item} exists again; nothing to restore into"),
            );
        }
        return job(
            id,
            &rec.key,
            "Restore",
            restore_confirm(&rec),
            JobWork::Workspace {
                op: WorkspaceOp::Restore {
                    item: rec.key.clone(),
                },
            },
        );
    }
    // The rest act on one item: the row's key, or a banner's `params.item`.
    let source = target.get("source").and_then(Value::as_str);
    let found = match (
        target.get("key").and_then(Value::as_str),
        params.get("item").and_then(Value::as_str),
    ) {
        (Some(k), _) => find_item(&work_dir, source, k),
        (None, Some(i)) => list_items(&work_dir).into_iter().find(|(_, r)| r.key == i),
        (None, None) => None,
    };
    let Some((dir, mut rec)) = found else {
        return refused(404, "unknown-target", "no workspace for this item");
    };
    // An overlay call that carries the owner's row refreshes what the
    // workspace remembers of it (title, link, status: "done in tracker").
    if let Some(item) = target.get("item") {
        refresh_owner(&dir, &mut rec, item, inp.now_utc);
    }
    if local_status::handles(id) {
        return local_status::accept(
            inp.loaded,
            &dir,
            &mut rec,
            id,
            &params,
            body.get("note").and_then(Value::as_str),
            actor,
            confirmed,
            inp.now_utc,
        );
    }
    if id == crate::seeds::SET_SEEDS_ID {
        return crate::seeds::accept(inp.loaded, &dir, &mut rec, &params, confirmed, inp.now_utc);
    }
    if crate::states::handles(id) {
        return crate::states::accept(
            world,
            inp.loaded,
            inp.obs,
            &dir,
            &rec,
            id,
            &params,
            confirmed,
            inp.blocks,
            inp.now_utc,
        );
    }
    if let Some(repo) = id.strip_prefix(ADD_REPO_PREFIX) {
        return add_repo(inp, &rec, repo, &params, confirmed);
    }
    let activate_no_save = format!("{ACTIVATE_ID}{}", crate::states::NO_SAVE_SUFFIX);
    if id == ACTIVATE_ID || id == activate_no_save {
        let Some(obs) = inp.obs else {
            return refused(409, "observe", "cannot look at the stack now");
        };
        if inp.loaded.binding.item.as_deref() == Some(rec.key.as_str()) {
            return refused(
                409,
                "not-offered",
                format!(
                    "{} is already active in slot {}",
                    rec.item_ref.key, inp.loaded.slot
                ),
            );
        }
        let no_save = id == activate_no_save;
        if no_save && crate::states::bound_item(inp.loaded).is_none() {
            return refused(
                409,
                "not-offered",
                "no other item has the slot, so there is nothing to save",
            );
        }
        let (change, head, save) = activation_saving(inp.loaded, inp.runner, obs, &rec, no_save);
        return planned_job(
            world, inp, obs, &rec, id, "Activate", change, &head, save, &params, confirmed,
        );
    }
    if id.strip_prefix(REBUILD_PREFIX).is_some() {
        let Some(obs) = inp.obs else {
            return refused(409, "observe", "cannot look at the stack now");
        };
        if !inp.facts.drift.iter().any(|d| d.item == rec.key) {
            return refused(409, "not-offered", "nothing has drifted");
        }
        let (change, head) = rebuild_change(&rec, &inp.facts.drift);
        return planned_job(
            world,
            inp,
            obs,
            &rec,
            id,
            "Rebuild changed",
            change,
            &head,
            None,
            &params,
            confirmed,
        );
    }
    if id.strip_prefix(KEEP_PREFIX).is_some() {
        let mine: Vec<&Drift> = inp
            .facts
            .drift
            .iter()
            .filter(|d| d.item == rec.key)
            .collect();
        if mine.is_empty() {
            return refused(409, "not-offered", "nothing has drifted");
        }
        let mut rec = rec.clone();
        for d in &mine {
            rec.keep_as_built
                .insert(d.component.clone(), d.head.clone());
        }
        rec.updated_utc = inp.now_utc.to_string();
        if let Err(e) = rec.save(&dir) {
            return refused(500, "workspace", e);
        }
        return Accepted::Done(format!(
            "Kept as built: {} (until the branch moves again)",
            mine.iter()
                .map(|d| d.component.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(v) = archive_variants(inp.loaded, &dir, &rec)
        .into_iter()
        .find(|v| v.id == id)
    {
        if !confirmed {
            return refused(
                409,
                "confirmation-required",
                "a mutating action must be confirmed",
            );
        }
        let blockers = archive_blockers(&Git::new(&*world.git_cli), &dir, &rec);
        if !blockers.is_empty() {
            return refused(
                409,
                "dirty",
                format!(
                    "not archived, commit and push first: {}",
                    blockers.join("; ")
                ),
            );
        }
        return job(
            &v.id,
            &rec.key,
            "Archive",
            v.confirm,
            JobWork::Workspace {
                op: WorkspaceOp::Archive {
                    item: rec.key.clone(),
                    save: v.save,
                    keep_data: v.keep_data,
                },
            },
        );
    }
    refused(
        409,
        "not-offered",
        format!("`{id}` is not an action of this workspace"),
    )
}

#[allow(clippy::too_many_arguments)]
fn planned_job(
    world: &World,
    inp: &WorkInputs<'_, '_>,
    obs: &Observation,
    rec: &ItemRecord,
    id: &str,
    label: &str,
    change: Change,
    head: &str,
    save: Option<crate::states::SaveSpec>,
    params: &Value,
    confirmed: bool,
) -> Accepted {
    let plan = match inp.runner.plan(obs, &change) {
        Ok(p) => p,
        Err(e) => return refused(409, "plan", e.to_string()),
    };
    let confirm = full_confirm(head, &plan);
    if !confirmed {
        return refused(
            409,
            "confirmation-required",
            "a mutating action must be confirmed",
        );
    }
    let fp = fingerprint(&obs.state.fingerprint(), &confirm);
    if params.get("fingerprint").and_then(Value::as_str) != Some(fp.as_str()) {
        return refused(
            409,
            "stale-plan",
            format!("the stack or the item changed since this was shown; what would run now is: {confirm}"),
        );
    }
    // The plan builds from the item's worktrees: judge it bound to them.
    let bound = Binding::of_item(&inp.loaded.work_dir(), &rec.key);
    let runner = inp
        .loaded
        .plain_runner(world, inp.now_utc)
        .with_binding(bound.worktrees, Some(inp.loaded.guard()));
    let (inst, _) = match runner.instance(inp.loaded.slot) {
        Ok(i) => i,
        Err(e) => return refused(409, "definition", e.to_string()),
    };
    if let Err(e) = runner.guard_plan(&inst, obs, &plan) {
        return refused(409, "shared-checkout", e.to_string());
    }
    job(
        id,
        &rec.key,
        label,
        confirm,
        JobWork::Plan {
            change,
            plan: Box::new(plan),
            save,
            remove_volumes: false,
        },
    )
}

fn add_repo(
    inp: &WorkInputs<'_, '_>,
    rec: &ItemRecord,
    repo: &str,
    params: &Value,
    confirmed: bool,
) -> Accepted {
    if !inp.loaded.stack.repos().contains_key(repo) {
        return refused(
            409,
            "not-offered",
            format!("the stack has no repo `{repo}`"),
        );
    }
    if rec.repos.contains_key(repo) {
        return refused(
            409,
            "not-offered",
            format!("{} already has {repo}", rec.item_ref.key),
        );
    }
    let Some(c) = planned_checkout(inp.loaded, inp.facts, rec, repo) else {
        return refused(
            409,
            "not-offered",
            format!("origin of {repo} has not been asked yet; look again"),
        );
    };
    if !confirmed {
        return refused(
            409,
            "confirmation-required",
            "a mutating action must be confirmed",
        );
    }
    let expect = match c.how {
        CheckoutHow::Existing => "existing",
        CheckoutHow::Create { .. } => "new",
    };
    if params.get("expect").and_then(Value::as_str) != Some(expect)
        || params.get("branch").and_then(Value::as_str) != Some(c.branch.as_str())
    {
        return refused(
            409,
            "stale-plan",
            format!(
                "this changed since it was shown; now: {}",
                add_repo_confirm(&c)
            ),
        );
    }
    job(
        &format!("{ADD_REPO_PREFIX}{repo}"),
        &rec.key,
        &format!("Add {repo}"),
        add_repo_confirm(&c),
        JobWork::Workspace {
            op: WorkspaceOp::AddRepo {
                item: rec.key.clone(),
                checkout: c,
            },
        },
    )
}

/// `start-work`: check the row it was sent, create the folder and its
/// record now (so a second call finds it), and hand the worktrees to a job.
fn start_work(
    world: &World,
    inp: &WorkInputs<'_, '_>,
    target: &Value,
    confirmed: bool,
    actor: Option<&str>,
) -> Accepted {
    if target.get("view").and_then(Value::as_str) != Some("work") {
        return refused(
            404,
            "unknown-target",
            "Start work is offered on Work rows only",
        );
    }
    let t = match parse_target_item(target) {
        Ok(t) => t,
        Err(e) => return refused(400, "bad-item", e),
    };
    let work = inp.loaded.stack.work_def();
    if !work
        .sources
        .iter()
        .any(|s| s == ANY_SOURCE || *s == t.source)
    {
        return refused(
            409,
            "not-offered",
            format!("Start work is not offered on rows of `{}`", t.source),
        );
    }
    if !confirmed {
        return refused(
            409,
            "confirmation-required",
            "a mutating action must be confirmed",
        );
    }
    let key = match item_dir_name(&t.key) {
        Ok(k) => k,
        Err(e) => return refused(400, "bad-item", e),
    };
    let work_dir = inp.loaded.work_dir();
    let dir = ItemRecord::dir(&work_dir, &key);
    let item_ref = ItemRef {
        source: t.source.clone(),
        key: t.key.clone(),
    };
    match ItemRecord::load(&dir) {
        Ok(Some(mut rec)) if rec.item_ref == item_ref => {
            rec.title = t.title.clone().or(rec.title);
            rec.url = t.url.clone().or(rec.url);
            rec.status = t.status.clone().or(rec.status);
            rec.updated_utc = inp.now_utc.to_string();
            if rec.workflow.is_none() {
                rec.workflow = local_status::start_record(inp.loaded, &rec, actor, inp.now_utc);
            }
            let _ = rec.save(&dir);
            return Accepted::Done(format!("{} already started ({})", t.key, dir.display()));
        }
        Ok(Some(rec)) => {
            return refused(
                409,
                "conflict",
                format!(
                    "the folder {key} belongs to {}:{}; nothing was created",
                    rec.item_ref.source, rec.item_ref.key
                ),
            )
        }
        Ok(None) => {}
        Err(e) => return refused(500, "workspace", e),
    }
    if ItemRecord::archived_dir(&work_dir, &key).exists() {
        return refused(
            409,
            "archived",
            format!("{key} is archived; restore it from the Archived panel"),
        );
    }
    let mut rec = ItemRecord::new(item_ref, &key, inp.now_utc);
    rec.title = t.title.clone();
    rec.url = t.url.clone();
    rec.status = t.status.clone();
    rec.workflow = local_status::start_record(inp.loaded, &rec, actor, inp.now_utc);
    if let Err(e) = fs::create_dir_all(&dir)
        .map_err(|e| e.to_string())
        .and_then(|_| rec.save(&dir))
    {
        return refused(500, "workspace", e);
    }
    let git = Git::new(&*world.git_cli);
    let identities = repo_identities(&git, inp.loaded);
    let mut checkouts = Vec::new();
    let mut notes = Vec::new();
    for repo in inp.loaded.stack.repos().keys() {
        let id = match identities.get(repo) {
            Some(Ok(Some(id))) => id,
            Some(Ok(None)) => {
                notes.push(format!(
                    "{repo}: its remote is not a forge PR rows name; skipped"
                ));
                continue;
            }
            Some(Err(e)) => {
                notes.push(format!("{repo}: skipped, its remote is unknown ({e})"));
                continue;
            }
            None => continue,
        };
        let prs: Vec<&TargetPr> = t
            .prs
            .iter()
            .filter(|p| id.matches(&p.forge, &p.repo))
            .collect();
        match prs
            .iter()
            .find_map(|p| p.source_branch.as_ref().map(|b| (p, b)))
        {
            Some((_, branch)) => checkouts.push(Checkout {
                repo: repo.clone(),
                branch: branch.clone(),
                how: CheckoutHow::Existing,
            }),
            None if prs.is_empty() => notes.push(format!(
                "{repo}: no PR of {} in this item; skipped",
                id.repo
            )),
            None => notes.push(format!(
                "{repo}: PR {}#{} names no source branch; skipped",
                prs[0].repo, prs[0].number
            )),
        }
    }
    let confirm = format!(
        "Create a workspace for {}{}",
        t.key,
        t.title
            .as_deref()
            .map(|x| format!(" — {x}"))
            .unwrap_or_default()
    );
    job(
        START_WORK_ID,
        &key,
        "Start work",
        confirm,
        JobWork::Workspace {
            op: WorkspaceOp::StartWork {
                item: key.clone(),
                checkouts,
                notes,
            },
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_target_item_is_checked_as_untrusted_input() {
        let ok = json!({
            "view": "work", "key": "ABC-1", "source": "tracker",
            "item": {
                "key": "ABC-1", "title": "Fix\u{7}  login", "url": "javascript:alert(1)",
                "status": { "label": "In Progress", "category": "doing" },
                "prs": [
                    { "forge": "github", "repo": "owner/api", "number": 7, "sourceBranch": "refs/heads/task/ABC-1-fix" },
                    { "forge": "github", "repo": "owner/web", "number": "8", "sourceBranch": "bad..branch" },
                    { "repo": "no-forge" }
                ]
            }
        });
        let t = parse_target_item(&ok).unwrap();
        assert_eq!(t.title.as_deref(), Some("Fix  login"));
        assert_eq!(t.url, None);
        assert_eq!(
            t.status.as_ref().unwrap().category.as_deref(),
            Some("doing")
        );
        assert_eq!(t.prs.len(), 2);
        assert_eq!(t.prs[0].number, "7");
        assert_eq!(t.prs[0].source_branch.as_deref(), Some("task/ABC-1-fix"));
        assert_eq!(t.prs[1].source_branch, None);

        for bad in [
            json!({ "view": "work", "key": "ABC-1", "source": "tracker" }),
            json!({ "view": "work", "key": "ABC-1", "source": "tracker", "item": "ABC-1" }),
            json!({ "view": "work", "source": "tracker", "item": {} }),
            json!({ "view": "work", "key": "ABC-1", "item": {} }),
            json!({ "view": "work", "key": "ABC-1", "source": "tracker", "item": { "key": "ABC-2" } }),
            json!({ "view": "work", "key": "", "source": "tracker", "item": {} }),
            json!({ "view": "work", "key": "A\nB", "source": "tracker", "item": {} }),
            json!({ "view": "work", "key": "ABC-1", "source": "tracker", "item": { "prs": "x" } }),
        ] {
            assert!(parse_target_item(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn fingerprints_cover_state_and_confirm() {
        let a = fingerprint("s1", "Bind slot 0 to ABC-1");
        assert_eq!(a.len(), 16);
        assert_ne!(a, fingerprint("s2", "Bind slot 0 to ABC-1"));
        assert_ne!(a, fingerprint("s1", "Bind slot 0 to ABC-2"));
        assert_eq!(a, fingerprint("s1", "Bind slot 0 to ABC-1"));
    }

    #[test]
    fn add_repo_confirms_say_what_will_happen() {
        let c = Checkout {
            repo: "api".into(),
            branch: "task/ABC-1-fix-login".into(),
            how: CheckoutHow::Existing,
        };
        assert_eq!(
            add_repo_confirm(&c),
            "Check out existing branch `task/ABC-1-fix-login` in api"
        );
        let c = Checkout {
            how: CheckoutHow::Create {
                base: "main".into(),
            },
            ..c
        };
        assert_eq!(
            add_repo_confirm(&c),
            "Create branch `task/ABC-1-fix-login` from `main` in api"
        );
    }
}
