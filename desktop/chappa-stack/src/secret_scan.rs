//! Secret scanning of what the stack builds from and what agents work in:
//! the bound item's worktrees, the commits they carry ahead of their base,
//! each source build's context, and the env files and instance env the
//! stack hands to components. The verdict goes out as the `secretScan`
//! block of the Environment payload, which the app enforces like the
//! `dataSafety` block.
//!
//! A credential in a worktree (an untracked `.env`, a key committed and
//! then deleted on the branch, a token in a config file) is one `cat` away
//! from an agent's context, and anything a build copies into an image is
//! readable by whoever can pull it. The runner (listing, gitleaks, the
//! baseline) is stack-core's `safety::secrets`; this module decides when
//! to scan, keeps the results per instance and says what follows.
//!
//! # When a scan runs
//!
//! | when | what | where |
//! |---|---|---|
//! | Start work, Add repo, Restore, and an activation | the working tree of each of the item's worktrees (untracked and ignored files included, the definition's `secrets.excludes` left out) and the commits each carries ahead of its repo's base | inside the job, after the worktrees exist |
//! | before every source build | the build context, `.dockerignore` honoured | inside the job, before any step runs |
//! | a plan that starts or builds anything | the compose `env_file` entries and the resolved instance env | inside the job |
//! | every [`RESCAN_EVERY`] | the bound worktrees' files whose size or modification time changed since their last scan (and the commits, when HEAD moved) | on a background thread, never a request |
//! | every [`FULL_RESCAN_SECS`] at most | everything above again, in full | the same thread |
//!
//! The scanner is gitleaks on `PATH` ([`GitleaksOnPath`]), with the
//! gitleaks config the app passes (`--gitleaks-config`, from its Settings)
//! or the same built-in default the app uses; without one, a stack that
//! declares `secrets:` reads as `error` (see below) and one that does not
//! reads as `unscanned`.
//!
//! # Paths and the one baseline
//!
//! Every finding's file is named relative to the stack project's folder
//! (`.work/ABC-1/src/api-1a2b3c4d/.env`), whichever folder was scanned. The
//! app's own pre-agent scan names files the same way, so one entry in
//! `<project>/.work/secrets-baseline.json` silences a finding on both
//! sides. The baseline is applied when the block is built, not when the
//! scan runs, so an acknowledgement takes effect at once.
//!
//! # The wire block
//!
//! ```json
//! "secretScan": {
//!   "instance": "local/0",
//!   "state": "clean" | "not-clean" | "acknowledged" | "unscanned" | "error",
//!   "policy": "warn" | "block-agents" | "block-build" | null,
//!   "blocksAgents": false,
//!   "lastScanUtc": "2026-11-15T12:00:00Z" | null,
//!   "findings": [ { "rule": "github-pat", "file": ".work/ABC-1/src/api-1a2b3c4d/.env", "line": 1 },
//!                 { "rule": "generic-api-key", "file": ".work/ABC-1/src/api-1a2b3c4d/cfg.txt", "line": 2, "commit": "f6bb19e1…" } ],
//!   "skipped": 0,
//!   "bound": { "source": "tracker", "key": "ABC-1", "folders": ["/srv/example/.work/ABC-1", "…/src/api-1a2b3c4d"] } | null,
//!   "project": "/srv/example",
//!   "reasons": [ { "component": null, "text": "api worktree: 1 possible secret (.env:1 github-pat)" } ],
//!   "ack": { "by": "human:alex", "note": "…", "atUtc": "…" } | null,
//!   "ackActionId": "secret-scan-ack"
//! }
//! ```
//!
//! - `instance`, `bound`, `project`, `reasons`, `ack`, `ackActionId` and
//!   `lastScanUtc` mean what they mean in `dataSafety`. `bound` is the same
//!   value: the slot's active item and the folders agents work in for it.
//! - `findings` are the findings no acknowledgement covers: rule, file,
//!   line and, for one found in a commit, the commit. Never a value.
//! - `skipped` counts files left out for the per-file size cap or for being
//!   a link: a warning, not a gap that blocks.
//! - `state` is the worst of: `error` (a scan could not finish and no
//!   acknowledgement covers it: no scanner while `secrets:` is declared, a
//!   timeout, a scanner error, or the size limit for one scan reached so
//!   that readable files went unread), `not-clean` (findings), `unscanned`
//!   (a bound worktree not scanned yet, or no scanner and no `secrets:`),
//!   `acknowledged` (findings or a failed scan acknowledged), `clean`.
//! - `policy` is `secrets.onFinding` (default `block-agents`) while the
//!   state is `not-clean` or `error`, else `null`.
//! - **`blocksAgents` is the one field to enforce on.** True when a finding
//!   stands under a policy other than `warn`, or when a scan of a bound
//!   worktree's files could not finish and is not acknowledged, whatever
//!   the policy: an agent there would read files nobody checked. A bound
//!   worktree that is `unscanned` (not scanned yet, or no scanner at all)
//!   blocks the same way a finding does, under the same policy, and its
//!   reason line says which. A failed
//!   scan of commits, a build context or env files only warns (agents do
//!   not read those from the worktree); under `block-build` a failed build
//!   context scan fails the build instead.
//!
//! # Acknowledging
//!
//! `secret-scan-ack` (view-level, `mutates: true`, `note: "required"`)
//! confirms with every finding named as `file:line (rule)` and every
//! failed worktree scan named with its reason. It writes the findings into
//! the project's baseline (who = `X-Chappa-Actor`, else "the app") and
//! records an acknowledgement of each failed worktree scan on the
//! instance, which lasts until that worktree's next scan succeeds.
//!
//! Results live in `<work>/instances/<instance>/secret-scan.json`: rules,
//! files, lines, commits, counts and file stamps, never a value. The
//! resolved instance env is written to a scratch file under `.work/` for
//! the length of its scan only, and is never logged.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use stack_core::definition::Mode;
use stack_core::driver::ResolvedInstance;
use stack_core::instance_secrets::Secrets;
use stack_core::jobs::write_atomic;
use stack_core::planner::{Plan, Step};
use stack_core::runner::StackRunner;
use stack_core::safety::secrets::config::{choose, ProjectScanConfig};
use stack_core::safety::secrets::generated::{find_in_commits, find_in_tracked};
use stack_core::safety::secrets::{
    list_target, policy_for, scan, Baseline, GitleaksScanner, ListLimits, ScanTarget, SkipReason,
    SkippedFile,
};
use stack_core::safety::{Finding, Location, Policy, SafetyError, SecretScanner};
use stack_core::workspace::ItemRecord;

use crate::procs::instance_dir;
use crate::project::{Binding, Loaded};
use crate::world::World;

pub const STORE_FILE: &str = "secret-scan.json";
/// The Environment banner's id while the project's own gitleaks config is
/// not the one in use.
pub const CONFIG_REVIEW_BANNER: &str = "secret-scan-config";
/// The acknowledge action's id (the wire block names it too).
pub const ACK_ACTION_ID: &str = "secret-scan-ack";
/// Where the baseline (and the scanner's scratch space) lives, under the
/// stack project: the same folder the app's pre-agent scan uses.
pub const BASELINE_DIR: &str = ".work";
/// How often the bound worktrees are looked at for changed files.
pub const RESCAN_EVERY: Duration = crate::background::SCRIPT_STATUS_TTL;
/// The longest time between two full rescans.
pub const FULL_RESCAN_SECS: u64 = 10 * 60;
/// How long one scanner run may take.
pub const SCAN_TIMEOUT: Duration = Duration::from_secs(300);
/// Findings named in one reason line before "and N more".
const NAMED_IN_LINE: usize = 5;

// ------------------------------------------------------------- scanner

pub type SharedScanner = Arc<dyn SecretScanner + Send + Sync>;

/// Where the stack's scanner comes from. `work_dir` is the scanner's
/// scratch space (the project's `.work`, which no listing reads).
pub trait ScannerSource {
    fn scanner(&self, work_dir: &Path) -> Option<SharedScanner>;

    /// Why the project's own gitleaks config is not what the scans use
    /// now (not trusted, changed since, or failing the config rules): the
    /// Environment view shows it as a banner asking to review it.
    fn config_review(&self) -> Option<String> {
        None
    }
}

/// gitleaks on `PATH`, the default, with the config chosen in this order
/// ([`stack_core::safety::secrets::config::choose`]): the project's own
/// (`serve --project-gitleaks-config` with the hash the person trusted,
/// re-read and re-hashed for every scan and used only while it matches),
/// else the one the app's Settings name (`serve --gitleaks-config`), else
/// stack-core's [`DEFAULT_CONFIG`](stack_core::safety::secrets::gitleaks::DEFAULT_CONFIG),
/// which the app uses too when nothing is set. Both are passed on to every
/// job, so the app's pre-agent scan and every scan here use the same rules.
/// No other config file in a scanned tree is ever read.
#[derive(Debug, Clone, Default)]
pub struct GitleaksOnPath {
    pub config: Option<PathBuf>,
    pub project: ProjectScanConfig,
}

impl GitleaksOnPath {
    pub fn with_config(config: Option<PathBuf>) -> Self {
        Self {
            config,
            project: ProjectScanConfig::None,
        }
    }
}

impl ScannerSource for GitleaksOnPath {
    fn scanner(&self, work_dir: &Path) -> Option<SharedScanner> {
        GitleaksScanner::detect(None, work_dir).map(|mut g| {
            g.timeout = SCAN_TIMEOUT;
            choose(&self.project, self.config.as_deref()).apply(&mut g);
            Arc::new(g) as SharedScanner
        })
    }

    fn config_review(&self) -> Option<String> {
        choose(&self.project, self.config.as_deref()).review
    }
}

/// No scanner installed.
pub struct NoScanner;

impl ScannerSource for NoScanner {
    fn scanner(&self, _work_dir: &Path) -> Option<SharedScanner> {
        None
    }
}

/// One scanner whatever the folder (tests).
pub struct FixedScanner(pub SharedScanner);

impl ScannerSource for FixedScanner {
    fn scanner(&self, _work_dir: &Path) -> Option<SharedScanner> {
        Some(self.0.clone())
    }
}

// --------------------------------------------------------------- store

/// What a target is, which decides what a failed scan of it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TargetKind {
    WorkingTree,
    CommitsAhead,
    BuildContext,
    EnvFiles,
}

impl TargetKind {
    /// Agents read these files from the worktree: a scan of them that did
    /// not finish keeps agents out.
    pub fn agents_read(self) -> bool {
        matches!(self, TargetKind::WorkingTree)
    }
}

/// A file as last scanned: enough to see that it changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Stamp {
    pub mtime_ms: u64,
    pub size: u64,
}

/// One target's last scan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetEntry {
    pub kind: TargetKind,
    /// `api worktree`, `api build context` …
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<PathBuf>,
    /// The component a build context belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component: Option<String>,
    pub scanned_utc: String,
    /// Every finding, acknowledged ones included (the baseline is applied
    /// when the block is built). Files relative to the stack project.
    #[serde(default)]
    pub findings: Vec<Finding>,
    /// Why the scan did not finish, in this program's words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Files left out for the per-file size cap or for being a link.
    #[serde(default)]
    pub skipped: usize,
    /// Working trees: each file's stamp, by path relative to `root`.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub stamps: IndexMap<String, Stamp>,
    /// Commit scans: the HEAD scanned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
}

impl TargetEntry {
    fn new(kind: TargetKind, label: &str, root: Option<&Path>, now: &str) -> Self {
        TargetEntry {
            kind,
            label: label.to_string(),
            root: root.map(Path::to_path_buf),
            component: None,
            scanned_utc: now.to_string(),
            findings: Vec::new(),
            error: None,
            skipped: 0,
            stamps: IndexMap::new(),
            head: None,
        }
    }
}

/// An acknowledgement of scans that did not finish.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorAck {
    pub by: String,
    pub note: String,
    pub at_utc: String,
    /// The target keys acknowledged; each goes once its scan succeeds.
    pub covers: Vec<String>,
}

/// Every target's last scan on one instance.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanStore {
    #[serde(default)]
    pub targets: IndexMap<String, TargetEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_full_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_ack: Option<ErrorAck>,
}

pub fn store_path(work_dir: &Path, instance: &str) -> PathBuf {
    instance_dir(work_dir, instance).join(STORE_FILE)
}

impl ScanStore {
    /// A missing file is an empty store; one that cannot be read is an
    /// error, so the instance cannot read as clean.
    pub fn load(work_dir: &Path, instance: &str) -> Result<ScanStore, String> {
        let path = store_path(work_dir, instance);
        match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|e| format!("{} cannot be read: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ScanStore::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    pub fn save(&self, work_dir: &Path, instance: &str) -> Result<(), String> {
        let path = store_path(work_dir, instance);
        write_atomic(
            &path,
            &serde_json::to_string_pretty(self).expect("store serializes"),
        )
        .map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Record a scan. One that finished ends the acknowledgement of its
    /// target's earlier failure.
    pub fn put(&mut self, key: &str, entry: TargetEntry) {
        if entry.error.is_none() {
            if let Some(ack) = self.error_ack.as_mut() {
                ack.covers.retain(|k| k != key);
                if ack.covers.is_empty() {
                    self.error_ack = None;
                }
            }
        }
        self.targets.insert(key.to_string(), entry);
    }

    fn covered(&self, key: &str) -> bool {
        self.error_ack
            .as_ref()
            .is_some_and(|a| a.covers.iter().any(|k| k == key))
    }
}

// ------------------------------------------------------------- naming

fn slashed(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

fn absolute(p: &Path) -> PathBuf {
    std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf())
}

/// `path` as findings name it: relative to the stack project when inside
/// it, else absolute; forward slashes.
pub fn shown(project: &Path, path: &Path) -> String {
    let abs = absolute(path);
    match abs.strip_prefix(absolute(project)) {
        Ok(rel) => slashed(rel),
        Err(_) => slashed(&abs),
    }
}

fn join_shown(prefix: &str, file: &Path) -> PathBuf {
    let file = slashed(file);
    if prefix.is_empty() {
        PathBuf::from(file)
    } else {
        PathBuf::from(format!("{prefix}/{file}"))
    }
}

/// Findings of a scan rooted at `root`, renamed relative to the project.
fn rebased(project: &Path, root: &Path, findings: Vec<Finding>) -> Vec<Finding> {
    let prefix = shown(project, root);
    findings
        .into_iter()
        .map(|mut f| {
            if let Location::Source { file, .. } = &mut f.location {
                *file = join_shown(&prefix, file);
            }
            f
        })
        .collect()
}

fn file_of(f: &Finding) -> Option<String> {
    match &f.location {
        Location::Source { file, .. } => Some(slashed(file)),
        Location::Data { .. } => None,
    }
}

/// `file:line (rule)`.
pub fn finding_line(f: &Finding) -> String {
    match &f.location {
        Location::Source { file, line, commit } => {
            let mut s = slashed(file);
            if let Some(l) = line {
                s.push_str(&format!(":{l}"));
            }
            if let Some(c) = commit {
                s.push_str(&format!(" @{}", &c[..c.len().min(8)]));
            }
            format!("{s} ({})", f.rule)
        }
        Location::Data { .. } => f.rule.clone(),
    }
}

pub fn tree_key(project: &Path, root: &Path) -> String {
    format!("tree:{}", shown(project, root))
}

pub fn commits_key(project: &Path, root: &Path) -> String {
    format!("commits:{}", shown(project, root))
}

pub fn build_key(component: &str) -> String {
    format!("build:{component}")
}

pub const ENV_KEY: &str = "env";

// ------------------------------------------------------------ scanning

fn limits() -> ListLimits {
    ListLimits::default()
}

fn failure(e: &SafetyError) -> String {
    match e {
        SafetyError::Timeout { seconds, .. } => {
            format!("the scan did not finish within {seconds} s")
        }
        other => format!("the scan failed: {other}"),
    }
}

/// Files skipped only for the per-file cap or for being a link are a
/// warning; reaching the size limit for one scan leaves readable files
/// unread, which is a scan that did not finish.
fn judge_skips(e: &mut TargetEntry, skipped: &[SkippedFile]) {
    let over_total = skipped
        .iter()
        .filter(|s| matches!(s.reason, SkipReason::OverTotal { .. }))
        .count();
    e.skipped = skipped.len() - over_total;
    if over_total > 0 && e.error.is_none() {
        e.error = Some(format!(
            "the scan reached its size limit, so {over_total} file{} {} not read",
            if over_total == 1 { "" } else { "s" },
            if over_total == 1 { "was" } else { "were" }
        ));
    }
}

fn stamp_of(path: &Path) -> Option<Stamp> {
    let meta = fs::symlink_metadata(path).ok()?;
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    Some(Stamp {
        mtime_ms,
        size: meta.len(),
    })
}

/// Every file the tree's listing names (scanned or skipped), stamped.
fn stamps_of(root: &Path, rels: impl Iterator<Item = String>) -> IndexMap<String, Stamp> {
    rels.filter_map(|r| stamp_of(&root.join(&r)).map(|s| (r, s)))
        .collect()
}

/// The full scan of one worktree's files.
pub fn scan_tree(
    scanner: &dyn SecretScanner,
    project: &Path,
    root: &Path,
    excludes: &[String],
    generated: &Secrets,
    label: &str,
    now: &str,
) -> TargetEntry {
    let mut e = TargetEntry::new(TargetKind::WorkingTree, label, Some(root), now);
    let listed = list_target(
        &ScanTarget::WorkingTree {
            root: root.to_path_buf(),
            excludes: excludes.to_vec(),
        },
        &limits(),
    );
    let list = match listed {
        Ok(l) => l,
        Err(err) => {
            e.error = Some(format!("its files could not be listed: {err}"));
            return e;
        }
    };
    e.stamps = stamps_of(
        root,
        list.files
            .iter()
            .map(|f| f.path.clone())
            .chain(list.skipped.iter().map(|s| s.path.clone())),
    );
    let target = ScanTarget::Files {
        root: root.to_path_buf(),
        paths: list.paths(),
        excludes: excludes.to_vec(),
    };
    match scan(scanner, &target, &limits(), &Baseline::default()) {
        Ok(r) => e.findings = rebased(project, root, r.findings),
        Err(err) => e.error = Some(failure(&err)),
    }
    match find_in_tracked(&limits().git, root, None, generated) {
        Ok(found) => e.findings.extend(rebased(project, root, found)),
        Err(err) => {
            e.error.get_or_insert(failure(&err));
        }
    }
    judge_skips(&mut e, &list.skipped);
    e
}

/// The incremental rescan: only the files whose stamp changed, and the
/// findings of files that went. `None` when nothing changed. A tree whose
/// last scan failed is scanned in full.
pub fn rescan_tree(
    scanner: &dyn SecretScanner,
    project: &Path,
    root: &Path,
    excludes: &[String],
    generated: &Secrets,
    prev: &TargetEntry,
    now: &str,
) -> Option<TargetEntry> {
    if prev.error.is_some() {
        return Some(scan_tree(
            scanner,
            project,
            root,
            excludes,
            generated,
            &prev.label,
            now,
        ));
    }
    let list = match list_target(
        &ScanTarget::WorkingTree {
            root: root.to_path_buf(),
            excludes: excludes.to_vec(),
        },
        &limits(),
    ) {
        Ok(l) => l,
        Err(err) => {
            let mut e = prev.clone();
            e.scanned_utc = now.to_string();
            e.error = Some(format!("its files could not be listed: {err}"));
            return Some(e);
        }
    };
    let stamps = stamps_of(
        root,
        list.files
            .iter()
            .map(|f| f.path.clone())
            .chain(list.skipped.iter().map(|s| s.path.clone())),
    );
    let changed: Vec<String> = stamps
        .iter()
        .filter(|(k, v)| prev.stamps.get(*k) != Some(*v))
        .map(|(k, _)| k.clone())
        .collect();
    let gone: Vec<String> = prev
        .stamps
        .keys()
        .filter(|k| !stamps.contains_key(*k))
        .cloned()
        .collect();
    if changed.is_empty() && gone.is_empty() {
        return None;
    }
    let prefix = shown(project, root);
    let touched: BTreeSet<String> = changed
        .iter()
        .chain(&gone)
        .map(|r| slashed(&join_shown(&prefix, Path::new(r))))
        .collect();
    let mut e = prev.clone();
    e.scanned_utc = now.to_string();
    e.stamps = stamps;
    e.findings
        .retain(|f| file_of(f).is_none_or(|p| !touched.contains(&p)));
    if !changed.is_empty() {
        let target = ScanTarget::Files {
            root: root.to_path_buf(),
            paths: changed.iter().map(PathBuf::from).collect(),
            excludes: excludes.to_vec(),
        };
        match scan(scanner, &target, &limits(), &Baseline::default()) {
            Ok(r) => e.findings.extend(rebased(project, root, r.findings)),
            Err(err) => e.error = Some(failure(&err)),
        }
        let only: BTreeSet<String> = changed.iter().cloned().collect();
        match find_in_tracked(&limits().git, root, Some(&only), generated) {
            Ok(found) => e.findings.extend(rebased(project, root, found)),
            Err(err) => {
                e.error.get_or_insert(failure(&err));
            }
        }
    }
    judge_skips(&mut e, &list.skipped);
    Some(e)
}

/// The commits `root` carries ahead of `base`, compared with the remote's
/// copy of the base first (what a push would be measured against), else the
/// local branch.
#[allow(clippy::too_many_arguments)]
pub fn scan_commits(
    scanner: &dyn SecretScanner,
    project: &Path,
    root: &Path,
    base: &str,
    generated: &Secrets,
    label: &str,
    head: Option<String>,
    now: &str,
) -> TargetEntry {
    let mut e = TargetEntry::new(TargetKind::CommitsAhead, label, Some(root), now);
    e.head = head;
    let candidates = [format!("origin/{base}"), base.to_string()];
    for cand in &candidates {
        let target = ScanTarget::CommitsAhead {
            root: root.to_path_buf(),
            base: cand.clone(),
        };
        match scan(scanner, &target, &limits(), &Baseline::default()) {
            Ok(r) => {
                e.findings = rebased(project, root, r.findings);
                let range = format!("{cand}..HEAD");
                match find_in_commits(&limits().git, root, &range, generated) {
                    Ok(found) => e.findings.extend(rebased(project, root, found)),
                    Err(err) => e.error = Some(failure(&err)),
                }
                return e;
            }
            Err(SafetyError::InvalidTarget(_)) => continue,
            Err(err) => {
                e.error = Some(failure(&err));
                return e;
            }
        }
    }
    e.error = Some(format!(
        "neither `origin/{base}` nor `{base}` is a commit to compare with"
    ));
    e
}

/// A source build's context, as the build would send it.
pub fn scan_build(
    scanner: &dyn SecretScanner,
    project: &Path,
    component: &str,
    context: &Path,
    now: &str,
) -> TargetEntry {
    let label = format!("{component} build context");
    let mut e = TargetEntry::new(TargetKind::BuildContext, &label, Some(context), now);
    e.component = Some(component.to_string());
    let target = ScanTarget::BuildContext {
        root: context.to_path_buf(),
        dockerignore: true,
    };
    match scan(scanner, &target, &limits(), &Baseline::default()) {
        Ok(r) => {
            e.findings = rebased(project, context, r.findings);
            judge_skips(&mut e, &r.skipped);
        }
        Err(err) => e.error = Some(failure(&err)),
    }
    e
}

/// A scratch file removed when dropped, whatever happens in between.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// The env files the stack passes (named relative to the project) and the
/// resolved instance env (`instance env: NAME`, by variable, so a finding
/// names the variable and never its value). The env is written to a
/// scratch file under `scratch_dir` for the scan only.
pub fn scan_env(
    scanner: &dyn SecretScanner,
    project: &Path,
    scratch_dir: &Path,
    files: &[PathBuf],
    env: &IndexMap<String, String>,
    now: &str,
) -> TargetEntry {
    let mut e = TargetEntry::new(
        TargetKind::EnvFiles,
        "env files and instance env",
        None,
        now,
    );
    if !files.is_empty() {
        let target = ScanTarget::EnvFiles {
            paths: files.to_vec(),
        };
        match scan(scanner, &target, &limits(), &Baseline::default()) {
            Ok(r) => {
                e.skipped += r.skipped.len();
                for mut f in r.findings {
                    if let Location::Source { file, .. } = &mut f.location {
                        *file = PathBuf::from(shown(project, file));
                    }
                    e.findings.push(f);
                }
            }
            Err(err) => e.error = Some(failure(&err)),
        }
    }
    if env.is_empty() {
        return e;
    }
    let mut names: Vec<&String> = env.keys().collect();
    names.sort();
    let text: String = names
        .iter()
        .map(|n| format!("{n}={}\n", env[*n].replace(['\r', '\n'], " ")))
        .collect();
    if let Err(err) = fs::create_dir_all(scratch_dir) {
        e.error
            .get_or_insert(format!("the instance env could not be scanned: {err}"));
        return e;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let path = scratch_dir.join(format!(
        "secret-scan-env-{}-{nanos}.env",
        std::process::id()
    ));
    if let Err(err) = fs::write(&path, text.as_bytes()) {
        e.error
            .get_or_insert(format!("the instance env could not be scanned: {err}"));
        return e;
    }
    let scratch = Scratch(path);
    let target = ScanTarget::EnvFiles {
        paths: vec![scratch.0.clone()],
    };
    match scan(scanner, &target, &limits(), &Baseline::default()) {
        Ok(r) => {
            for mut f in r.findings {
                if let Location::Source { file, line, .. } = &mut f.location {
                    let name = line
                        .and_then(|l| names.get(l.saturating_sub(1) as usize))
                        .map(|n| n.as_str())
                        .unwrap_or("?");
                    *file = PathBuf::from(format!("instance env: {name}"));
                    *line = None;
                }
                e.findings.push(f);
            }
        }
        Err(err) => {
            e.error
                .get_or_insert(format!("the instance env: {}", failure(&err)));
        }
    }
    drop(scratch);
    e
}

// ---------------------------------------------------------- what to scan

/// Whether this stack is scanned at all: it declares `secrets:`, or a
/// scanner is installed (the default policy then applies).
pub fn wanted(loaded: &Loaded, scanner_found: bool) -> bool {
    loaded.stack.secrets.is_some() || scanner_found
}

fn excludes(loaded: &Loaded) -> Vec<String> {
    loaded
        .stack
        .secrets
        .as_ref()
        .map(|s| s.excludes.clone())
        .unwrap_or_default()
}

/// A bound worktree: its repo, folder and the base its commits count from.
#[derive(Debug, Clone)]
pub struct Tree {
    pub repo: String,
    pub root: PathBuf,
    pub base: String,
}

/// The worktrees of `binding`, with their bases (the item's record, else
/// the definition's, else `main`).
pub fn trees_of(loaded: &Loaded, binding: &Binding) -> Vec<Tree> {
    let record = binding.item.as_deref().and_then(|item| {
        ItemRecord::load(&ItemRecord::dir(&loaded.work_dir(), item))
            .ok()
            .flatten()
    });
    let work = loaded.stack.work_def();
    binding
        .worktrees
        .iter()
        .map(|(repo, root)| Tree {
            repo: repo.clone(),
            root: root.clone(),
            base: record
                .as_ref()
                .and_then(|r| r.repos.get(repo))
                .map(|r| r.base.clone())
                .filter(|b| !b.is_empty())
                .unwrap_or_else(|| work.base_of(repo).to_string()),
        })
        .collect()
}

fn scratch_of(loaded: &Loaded) -> PathBuf {
    loaded.dir.join(BASELINE_DIR)
}

/// The values generated for the stack's instances, which a worktree's
/// tracked files and commits must not hold.
fn generated_of(loaded: &Loaded) -> Secrets {
    stack_core::instance_secrets::existing(&loaded.work_dir())
}

/// Scan every tree in full, and the commits each carries.
fn scan_trees(
    world: &World,
    loaded: &Loaded,
    scanner: &dyn SecretScanner,
    trees: &[Tree],
    store: &mut ScanStore,
    say: &mut dyn FnMut(&str),
) {
    let now = world.clock.now_utc();
    let ex = excludes(loaded);
    let generated = generated_of(loaded);
    for t in trees {
        let entry = scan_tree(
            scanner,
            &loaded.dir,
            &t.root,
            &ex,
            &generated,
            &format!("{} worktree", t.repo),
            &now,
        );
        say(&outcome_line(&entry));
        store.put(&tree_key(&loaded.dir, &t.root), entry);
        let head = world.git.head_commit(&t.root).ok();
        let entry = scan_commits(
            scanner,
            &loaded.dir,
            &t.root,
            &t.base,
            &generated,
            &format!("{} commits ahead of {}", t.repo, t.base),
            head,
            &now,
        );
        say(&outcome_line(&entry));
        store.put(&commits_key(&loaded.dir, &t.root), entry);
    }
}

/// One line for a job's log: what was scanned and what it found, by file,
/// line and rule.
pub fn outcome_line(e: &TargetEntry) -> String {
    match (&e.error, e.findings.is_empty()) {
        (Some(err), _) => format!("✗ secret scan: {} did not finish: {err}", e.label),
        (None, true) => format!("✓ secret scan: {} clean", e.label),
        (None, false) => format!(
            "✗ secret scan: {}: {}",
            e.label,
            e.findings
                .iter()
                .map(finding_line)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn update_store(
    loaded: &Loaded,
    instance: &str,
    f: impl FnOnce(&mut ScanStore),
) -> Result<(), String> {
    let work_dir = loaded.work_dir();
    // A store that cannot be read is replaced: its acknowledgements of
    // failed scans are lost, which keeps agents out rather than letting
    // them in.
    let mut store = ScanStore::load(&work_dir, instance).unwrap_or_default();
    f(&mut store);
    store.save(&work_dir, instance)
}

/// The scans that follow opening an item, adding a worktree to it,
/// restoring it or activating it: each of its worktrees and their commits.
pub fn scan_item(world: &World, loaded: &Loaded, item: &str, say: &mut dyn FnMut(&str)) {
    let scanner = world.secrets.scanner(&scratch_of(loaded));
    if !wanted(loaded, scanner.is_some()) {
        return;
    }
    let binding = Binding::of_item(&loaded.work_dir(), item);
    let trees = trees_of(loaded, &binding);
    if trees.is_empty() {
        return;
    }
    let Some(scanner) = scanner else {
        say("✗ secret scan: no scanner was found (install gitleaks); the item's worktrees are not scanned");
        return;
    };
    say("secret scan: the item's worktrees…");
    let instance = loaded.instance_id();
    let mut fresh = ScanStore::default();
    scan_trees(world, loaded, &*scanner, &trees, &mut fresh, say);
    let saved = update_store(loaded, &instance, |store| {
        for (k, e) in fresh.targets {
            store.put(&k, e);
        }
    });
    if let Err(e) = saved {
        say(&format!(
            "✗ secret scan: the results could not be kept: {e}"
        ));
    }
}

/// The build context of a component in source mode: its repo (the bound
/// worktree when there is one) and the mode's `context`.
pub fn build_context(
    loaded: &Loaded,
    runner: &StackRunner<'_>,
    component: &str,
) -> Option<PathBuf> {
    let mode = loaded.stack.components.get(component)?.mode(Mode::Source)?;
    let repo = runner.repo_path(component, Mode::Source).ok()?;
    Some(match &mode.context {
        Some(c) => repo.join(c),
        None => repo,
    })
}

/// Before the steps of a plan: scan the context of every source build in
/// it. Under `block-build`, a finding no acknowledgement covers, or a
/// context that could not be scanned, fails the job before anything is
/// built; under the other policies the build goes ahead.
pub fn gate_builds(
    world: &World,
    loaded: &Loaded,
    runner: &StackRunner<'_>,
    plan: &Plan,
    say: &mut dyn FnMut(&str),
) -> Result<(), String> {
    let builds: Vec<&str> = plan
        .steps
        .iter()
        .filter_map(|s| match s {
            Step::Build { component, .. } => Some(component.as_str()),
            _ => None,
        })
        .collect();
    if builds.is_empty() {
        return Ok(());
    }
    let scanner = world.secrets.scanner(&scratch_of(loaded));
    if !wanted(loaded, scanner.is_some()) {
        return Ok(());
    }
    let policy = policy_for(loaded.stack.secrets.as_ref());
    let Some(scanner) = scanner else {
        let why = "no scanner was found (install gitleaks), so the build contexts are not scanned";
        say(&format!("✗ secret scan: {why}"));
        return if policy == Policy::BlockBuild {
            Err(format!("not built: {why} (policy block-build)"))
        } else {
            Ok(())
        };
    };
    let baseline = Baseline::load(&scratch_of(loaded)).unwrap_or_default();
    let now = world.clock.now_utc();
    let mut problems = Vec::new();
    let mut entries = Vec::new();
    for c in builds {
        let Some(context) = build_context(loaded, runner, c) else {
            continue;
        };
        let entry = scan_build(&*scanner, &loaded.dir, c, &context, &now);
        say(&outcome_line(&entry));
        let live: Vec<String> = entry
            .findings
            .iter()
            .filter(|f| !baseline.contains(f))
            .map(finding_line)
            .collect();
        if let Some(err) = &entry.error {
            problems.push(format!("{} could not be scanned ({err})", entry.label));
        } else if !live.is_empty() {
            problems.push(format!("{}: {}", entry.label, live.join(", ")));
        }
        entries.push((build_key(c), entry));
    }
    if let Err(e) = update_store(loaded, &loaded.instance_id(), |store| {
        for (k, e) in entries {
            store.put(&k, e);
        }
    }) {
        say(&format!(
            "✗ secret scan: the results could not be kept: {e}"
        ));
    }
    if policy == Policy::BlockBuild && !problems.is_empty() {
        return Err(format!(
            "not built: possible secrets would go into the image ({}); fix them or acknowledge them in Environment (policy block-build)",
            problems.join("; ")
        ));
    }
    Ok(())
}

/// The env files of every compose service of the instance, absolute.
pub fn env_files(
    loaded: &Loaded,
    runner: &StackRunner<'_>,
    inst: &ResolvedInstance,
) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for project in runner.projects(inst).values() {
        for file in &project.files {
            let Ok(text) = fs::read_to_string(file) else {
                continue;
            };
            let dir = file.parent().unwrap_or(&loaded.dir);
            for service in project.services.values() {
                for p in stack_core::compose::base_env_files(&text, service) {
                    let abs = if p.is_absolute() { p } else { dir.join(p) };
                    if !out.contains(&abs) {
                        out.push(abs);
                    }
                }
            }
        }
    }
    out
}

/// The env the stack hands its components, as it is scanned: the instance
/// env and each component's own (`KEY (component)`), as far as it resolves
/// now, with every value the runner generated (`{secret:<name>}`) put back
/// as its placeholder. Those live only in the work folder; a credential
/// written into the definition or a file it names is still scanned as is.
pub fn instance_env(loaded: &Loaded, inst: &ResolvedInstance) -> IndexMap<String, String> {
    let mut env = stack_core::process::instance_env(inst, &loaded.stack, &IndexMap::new());
    for (name, c) in &inst.components {
        let ctx = inst.context_for(name);
        for (k, template) in &c.env {
            if let Ok(v) = stack_core::placeholders::resolve(template, &ctx) {
                env.insert(format!("{k} ({name})"), v);
            }
        }
    }
    for v in env.values_mut() {
        *v = inst.secrets.mask(v);
    }
    env
}

/// Scan the env files and the instance env (a plan that starts or builds).
pub fn scan_env_for(
    world: &World,
    loaded: &Loaded,
    runner: &StackRunner<'_>,
    inst: &ResolvedInstance,
    say: &mut dyn FnMut(&str),
) {
    let Some(scanner) = world.secrets.scanner(&scratch_of(loaded)) else {
        return;
    };
    let entry = scan_env(
        &*scanner,
        &loaded.dir,
        &scratch_of(loaded),
        &env_files(loaded, runner, inst),
        &instance_env(loaded, inst),
        &world.clock.now_utc(),
    );
    if !entry.findings.is_empty() || entry.error.is_some() {
        say(&outcome_line(&entry));
    }
    if let Err(e) = update_store(loaded, &loaded.instance_id(), |store| {
        store.put(ENV_KEY, entry)
    }) {
        say(&format!(
            "✗ secret scan: the results could not be kept: {e}"
        ));
    }
}

/// One rescan pass: the bound worktrees' changed files (or everything,
/// when a full rescan is due or a tree was never scanned), and with a full
/// pass the build contexts and env too. Returns whether anything was
/// scanned. Runs on the background thread.
pub fn rescan(
    world: &World,
    loaded: &Loaded,
    runner: &StackRunner<'_>,
    inst: Option<&ResolvedInstance>,
) -> bool {
    let Some(scanner) = world.secrets.scanner(&scratch_of(loaded)) else {
        return false;
    };
    let instance = loaded.instance_id();
    let work_dir = loaded.work_dir();
    let mut store = ScanStore::load(&work_dir, &instance).unwrap_or_default();
    let now_secs = world.clock.now_secs();
    let now = world.clock.now_utc();
    let full = store
        .last_full_secs
        .is_none_or(|t| now_secs >= t.saturating_add(FULL_RESCAN_SECS));
    let ex = excludes(loaded);
    let generated = generated_of(loaded);
    let mut scanned = false;
    for t in trees_of(loaded, &loaded.binding) {
        let tk = tree_key(&loaded.dir, &t.root);
        let label = format!("{} worktree", t.repo);
        let entry = match store.targets.get(&tk).filter(|_| !full) {
            None => Some(scan_tree(
                &*scanner,
                &loaded.dir,
                &t.root,
                &ex,
                &generated,
                &label,
                &now,
            )),
            Some(prev) => rescan_tree(&*scanner, &loaded.dir, &t.root, &ex, &generated, prev, &now),
        };
        if let Some(e) = entry {
            store.put(&tk, e);
            scanned = true;
        }
        let ck = commits_key(&loaded.dir, &t.root);
        let head = world.git.head_commit(&t.root).ok();
        let moved = store
            .targets
            .get(&ck)
            .is_none_or(|e| e.error.is_some() || head.is_none() || e.head != head);
        if full || moved {
            let e = scan_commits(
                &*scanner,
                &loaded.dir,
                &t.root,
                &t.base,
                &generated,
                &format!("{} commits ahead of {}", t.repo, t.base),
                head,
                &now,
            );
            store.put(&ck, e);
            scanned = true;
        }
    }
    if full {
        let builds: Vec<(String, TargetEntry)> = store
            .targets
            .iter()
            .filter(|(_, e)| e.kind == TargetKind::BuildContext)
            .map(|(k, e)| (k.clone(), e.clone()))
            .collect();
        for (k, prev) in builds {
            let (Some(c), Some(root)) = (prev.component.as_deref(), prev.root.as_deref()) else {
                continue;
            };
            let root = build_context(loaded, runner, c).unwrap_or_else(|| root.to_path_buf());
            if root.is_dir() {
                store.put(&k, scan_build(&*scanner, &loaded.dir, c, &root, &now));
                scanned = true;
            }
        }
        if let Some(inst) = inst {
            let e = scan_env(
                &*scanner,
                &loaded.dir,
                &scratch_of(loaded),
                &env_files(loaded, runner, inst),
                &instance_env(loaded, inst),
                &now,
            );
            store.put(ENV_KEY, e);
            scanned = true;
        }
        store.last_full_secs = Some(now_secs);
    }
    if let Err(e) = store.save(&work_dir, &instance) {
        eprintln!("chappa-stack: secret scan results could not be kept: {e}");
    }
    scanned
}

// -------------------------------------------------------------- the view

/// A scan that did not finish, as the view says it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorLine {
    pub key: String,
    pub label: String,
    pub component: Option<String>,
    pub text: String,
    /// Agents would read the unscanned files.
    pub agents: bool,
    /// An acknowledgement covers it.
    pub covered: bool,
}

/// One target's part of the view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetView {
    pub key: String,
    pub entry: TargetEntry,
    pub live: Vec<Finding>,
    pub acked: usize,
}

/// The instance's secret scan, as the payload and the actions see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretsView {
    pub instance: String,
    /// The stack declares `secrets:`.
    pub configured: bool,
    pub scanner_found: bool,
    pub policy: Policy,
    pub targets: Vec<TargetView>,
    pub errors: Vec<ErrorLine>,
    /// Bound worktrees not scanned yet.
    pub unscanned: Vec<String>,
    pub ack: Value,
    pub state: &'static str,
}

/// Put together the view from the definition, the binding, the stored
/// scans and the baseline.
pub fn summarize(
    loaded: &Loaded,
    store: &Result<ScanStore, String>,
    baseline: &Result<Baseline, String>,
    scanner_found: bool,
) -> SecretsView {
    let configured = loaded.stack.secrets.is_some();
    let policy = policy_for(loaded.stack.secrets.as_ref());
    let instance = crate::data_safety::instance_name(&loaded.stack_name, loaded.slot);
    let mut view = SecretsView {
        instance,
        configured,
        scanner_found,
        policy,
        targets: Vec::new(),
        errors: Vec::new(),
        unscanned: Vec::new(),
        ack: Value::Null,
        state: "clean",
    };
    if !wanted(loaded, scanner_found) {
        // Nothing can scan here, and the stack asks for nothing: the bound
        // worktrees are still folders nobody checked.
        view.state = "unscanned";
        for t in trees_of(loaded, &loaded.binding) {
            view.unscanned.push(format!("{} worktree", t.repo));
        }
        return view;
    }
    let empty = ScanStore::default();
    let st = store.as_ref().unwrap_or(&empty);
    let trees = trees_of(loaded, &loaded.binding);
    let covered = |k: &str| st.covered(k);
    if let Err(e) = store {
        for t in &trees {
            let key = tree_key(&loaded.dir, &t.root);
            view.errors.push(ErrorLine {
                covered: covered(&key),
                key,
                label: format!("{} worktree", t.repo),
                component: None,
                text: format!("the stored scan results cannot be read ({e})"),
                agents: true,
            });
        }
    }
    if !scanner_found {
        if trees.is_empty() {
            view.errors.push(ErrorLine {
                key: "scanner".into(),
                label: "secret scan".into(),
                component: None,
                text: "no scanner was found (install gitleaks), so nothing is scanned".into(),
                agents: false,
                covered: false,
            });
        }
        for t in &trees {
            let key = tree_key(&loaded.dir, &t.root);
            view.errors.push(ErrorLine {
                covered: covered(&key),
                key,
                label: format!("{} worktree", t.repo),
                component: None,
                text: "no scanner was found (install gitleaks), so it is not scanned".into(),
                agents: true,
            });
        }
    }
    let mut relevant: Vec<String> = Vec::new();
    for t in &trees {
        let tk = tree_key(&loaded.dir, &t.root);
        if scanner_found && !st.targets.contains_key(&tk) && store.is_ok() {
            view.unscanned.push(format!("{} worktree", t.repo));
        }
        relevant.push(tk);
        relevant.push(commits_key(&loaded.dir, &t.root));
    }
    for (k, e) in &st.targets {
        if matches!(e.kind, TargetKind::BuildContext | TargetKind::EnvFiles) {
            relevant.push(k.clone());
        }
    }
    let base = baseline.as_ref().ok();
    if let Err(e) = baseline {
        view.errors.push(ErrorLine {
            key: "baseline".into(),
            label: "acknowledged findings".into(),
            component: None,
            text: format!("cannot be read ({e}); every finding counts"),
            agents: false,
            covered: false,
        });
    }
    let mut acked_notes: Vec<(String, String, String)> = Vec::new();
    for key in relevant {
        let Some(entry) = st.targets.get(&key) else {
            continue;
        };
        let (acked, live): (Vec<&Finding>, Vec<&Finding>) = entry
            .findings
            .iter()
            .partition(|f| base.is_some_and(|b| b.contains(f)));
        if let Some(b) = base {
            for f in &acked {
                if let Some(n) =
                    stack_core::safety::secrets::fingerprint(f).and_then(|fp| b.note(&fp))
                {
                    acked_notes.push((n.at_utc.clone(), n.who.clone(), n.why.clone()));
                }
            }
        }
        if let Some(err) = &entry.error {
            if scanner_found {
                view.errors.push(ErrorLine {
                    key: key.clone(),
                    label: entry.label.clone(),
                    component: entry.component.clone(),
                    text: format!("did not finish: {err}"),
                    agents: entry.kind.agents_read(),
                    covered: covered(&key),
                });
            }
        }
        view.targets.push(TargetView {
            key,
            entry: entry.clone(),
            live: live.into_iter().cloned().collect(),
            acked: acked.len(),
        });
    }
    if let Some(a) = &st.error_ack {
        if view.errors.iter().any(|e| e.covered) {
            acked_notes.push((a.at_utc.clone(), a.by.clone(), a.note.clone()));
        }
    }
    view.ack = acked_notes
        .into_iter()
        .max()
        .map(|(at, by, note)| json!({ "by": by, "note": note, "atUtc": at }))
        .unwrap_or(Value::Null);
    let live = view.targets.iter().any(|t| !t.live.is_empty());
    let acked = view.targets.iter().any(|t| t.acked > 0) || view.errors.iter().any(|e| e.covered);
    view.state = if view.errors.iter().any(|e| !e.covered) {
        "error"
    } else if live {
        "not-clean"
    } else if !view.unscanned.is_empty() {
        "unscanned"
    } else if acked {
        "acknowledged"
    } else {
        "clean"
    };
    view
}

impl SecretsView {
    /// Whether anything is scanned at all here.
    pub fn active(&self) -> bool {
        self.configured || self.scanner_found
    }

    pub fn live(&self) -> impl Iterator<Item = &Finding> {
        self.targets.iter().flat_map(|t| t.live.iter())
    }

    /// A failed worktree scan no acknowledgement covers.
    fn agent_errors(&self) -> impl Iterator<Item = &ErrorLine> {
        self.errors.iter().filter(|e| e.agents && !e.covered)
    }

    /// The one field the app enforces on. A bound worktree nobody has
    /// scanned (not yet, or there is no scanner) blocks the way a finding
    /// does, under the same policy: an agent there would read files nobody
    /// checked, and "not checked" must not pass where "found something"
    /// would not.
    pub fn blocks_agents(&self) -> bool {
        (self.live().next().is_some() && self.policy.blocks_agents())
            || self.agent_errors().next().is_some()
            || self.unscanned_blocks()
    }

    /// Unscanned bound worktrees keep agents out (see [`Self::blocks_agents`]).
    fn unscanned_blocks(&self) -> bool {
        !self.unscanned.is_empty() && self.policy.blocks_agents()
    }

    /// Why an unscanned worktree is unscanned, as the reason line says it.
    fn unscanned_text(&self) -> &'static str {
        if self.scanner_found {
            "not scanned yet; agents wait for its first scan"
        } else {
            "not scanned: no scanner was found (install gitleaks)"
        }
    }

    pub fn policy_in_force(&self) -> Option<Policy> {
        matches!(self.state, "not-clean" | "error").then_some(self.policy)
    }

    pub fn last_scan(&self) -> Option<&str> {
        self.targets
            .iter()
            .map(|t| t.entry.scanned_utc.as_str())
            .max()
    }

    pub fn skipped(&self) -> usize {
        self.targets.iter().map(|t| t.entry.skipped).sum()
    }

    /// One line per target with something to say, then each failed scan
    /// that is not a target's (no scanner, unreadable files): `(component,
    /// text, blocks agents)`.
    pub fn reasons(&self) -> Vec<(Option<String>, String, bool)> {
        let mut out = Vec::new();
        let findings_block = self.policy.blocks_agents();
        for t in &self.targets {
            if !t.live.is_empty() {
                let n = t.live.len();
                let mut named: Vec<String> = t
                    .live
                    .iter()
                    .take(NAMED_IN_LINE)
                    .map(finding_line)
                    .collect();
                if n > NAMED_IN_LINE {
                    named.push(format!("and {} more", n - NAMED_IN_LINE));
                }
                out.push((
                    t.entry.component.clone(),
                    format!(
                        "{}: {n} possible secret{} ({})",
                        t.entry.label,
                        if n == 1 { "" } else { "s" },
                        named.join(", ")
                    ),
                    findings_block,
                ));
            }
        }
        for e in &self.errors {
            if e.covered {
                continue;
            }
            out.push((
                e.component.clone(),
                format!("{}: {}", e.label, e.text),
                e.agents,
            ));
        }
        let blocks = self.unscanned_blocks();
        for u in &self.unscanned {
            out.push((None, format!("{u}: {}", self.unscanned_text()), blocks));
        }
        out
    }

    /// The `secretScan` block (see the module docs).
    pub fn wire(&self, loaded: &Loaded) -> Value {
        let findings: Vec<Value> = self
            .live()
            .filter_map(|f| match &f.location {
                Location::Source { file, line, commit } => {
                    let mut v = json!({ "rule": f.rule, "file": slashed(file) });
                    if let Some(l) = line {
                        v["line"] = (*l).into();
                    }
                    if let Some(c) = commit {
                        v["commit"] = c.clone().into();
                    }
                    Some(v)
                }
                Location::Data { .. } => None,
            })
            .collect();
        json!({
            "instance": self.instance,
            "state": self.state,
            "policy": self.policy_in_force().map(Policy::as_str),
            "blocksAgents": self.blocks_agents(),
            "lastScanUtc": self.last_scan(),
            "findings": findings,
            "skipped": self.skipped(),
            "bound": crate::data_safety::bound(loaded),
            "project": crate::data_safety::project_dir(&loaded.dir),
            "reasons": self
                .reasons()
                .into_iter()
                .map(|(c, text, _)| json!({ "component": c, "text": text }))
                .collect::<Vec<_>>(),
            "ack": self.ack,
            "ackActionId": ACK_ACTION_ID,
        })
    }

    /// What an acknowledgement would cover now.
    pub fn ack_fingerprint(&self) -> String {
        let mut h = Sha256::new();
        for f in self.live() {
            h.update(
                stack_core::safety::secrets::fingerprint(f)
                    .unwrap_or_default()
                    .as_bytes(),
            );
            h.update(b"\n");
        }
        for e in self.agent_errors() {
            h.update(e.key.as_bytes());
            h.update(b"\n");
        }
        crate::identity::to_hex(&h.finalize())
    }

    /// The confirm: every finding as `file:line (rule)`, every failed
    /// worktree scan with its reason.
    pub fn ack_confirm(&self) -> String {
        let mut parts = Vec::new();
        let live: Vec<String> = self.live().map(finding_line).collect();
        if !live.is_empty() {
            parts.push(format!(
                "Acknowledge {} possible secret{} on {}: {}. They go into the project's local baseline (.work/secrets-baseline.json, never committed) and stop blocking agents here and at agent launch until the file changes around them.",
                live.len(),
                if live.len() == 1 { "" } else { "s" },
                self.instance,
                live.join(", ")
            ));
        }
        let errors: Vec<String> = self
            .agent_errors()
            .map(|e| format!("{}: {}", e.label, e.text))
            .collect();
        if !errors.is_empty() {
            parts.push(format!(
                "Let agents start without a complete scan on {}: {}. This lasts until the next scan of each succeeds.",
                self.instance,
                errors.join("; ")
            ));
        }
        parts.join(" ")
    }

    /// The acknowledge action, while there is something to acknowledge.
    pub fn ack_action(&self) -> Option<Value> {
        (self.live().next().is_some() || self.agent_errors().next().is_some()).then(|| {
            json!({
                "kind": "endpoint",
                "id": ACK_ACTION_ID,
                "label": "Acknowledge secret-scan findings",
                "mutates": true,
                "confirm": self.ack_confirm(),
                "note": "required",
                "params": { "fingerprint": self.ack_fingerprint() },
            })
        })
    }

    /// Record the acknowledgement: findings into the baseline, failed
    /// worktree scans on the instance. How many of each.
    pub fn acknowledge(
        &self,
        loaded: &Loaded,
        who: &str,
        note: &str,
        now_utc: &str,
    ) -> Result<(usize, usize), String> {
        let live: Vec<Finding> = self.live().cloned().collect();
        let mut added = 0;
        if !live.is_empty() {
            let dir = scratch_of(loaded);
            let mut baseline = Baseline::load(&dir).map_err(|e| e.to_string())?;
            added = baseline.acknowledge(&live, who, note, now_utc);
            baseline.save(&dir).map_err(|e| e.to_string())?;
        }
        let keys: Vec<String> = self.agent_errors().map(|e| e.key.clone()).collect();
        let n_errors = keys.len();
        if !keys.is_empty() {
            let work_dir = loaded.work_dir();
            let instance = loaded.instance_id();
            let mut store = ScanStore::load(&work_dir, &instance).unwrap_or_default();
            let mut covers = store.error_ack.take().map(|a| a.covers).unwrap_or_default();
            for k in keys {
                if !covers.contains(&k) {
                    covers.push(k);
                }
            }
            store.error_ack = Some(ErrorAck {
                by: who.to_string(),
                note: note.to_string(),
                at_utc: now_utc.to_string(),
                covers,
            });
            store.save(&work_dir, &instance)?;
        }
        log_ack(who, live.len(), added, n_errors);
        Ok((live.len(), n_errors))
    }

    /// Environment alerts, one per reason line.
    pub fn alerts(&self) -> Vec<Value> {
        self.reasons()
            .into_iter()
            .map(|(component, text, blocks)| {
                let mut a = json!({
                    "text": if blocks { format!("secret scan: {text} — agents blocked") } else { format!("secret scan: {text}") },
                    "severity": if blocks { "bad" } else { "warn" },
                });
                if let Some(c) = component {
                    a["componentId"] = c.into();
                }
                a
            })
            .collect()
    }

    /// The banner while agents are blocked, with the acknowledge action.
    pub fn banner(&self) -> Option<Value> {
        if !self.blocks_agents() {
            return None;
        }
        let title = if self.live().next().is_none() && self.agent_errors().next().is_none() {
            format!(
                "Agents are blocked on {}: the folders they would read are not scanned yet",
                self.instance
            )
        } else if self.agent_errors().next().is_some() && self.live().next().is_none() {
            format!("Agents are blocked on {}: the secret scan of the folders they would read did not finish", self.instance)
        } else {
            format!(
                "Agents are blocked on {}: possible secrets in what they would read",
                self.instance
            )
        };
        let lines: Vec<String> = self
            .alerts()
            .iter()
            .filter_map(|a| a["text"].as_str().map(str::to_string))
            .collect();
        let mut b = json!({ "id": "secret-scan", "tone": "bad", "title": title, "lines": lines });
        if let Some(a) = self.ack_action() {
            b["actions"] = json!([a]);
        }
        Some(b)
    }

    /// The one badge the view's header shows for the scan: its state in a
    /// word or two, and how loud (the rows of [`SecretsView::facts`] are
    /// its details).
    pub fn status(&self) -> Value {
        let text = match self.state {
            "clean" => "clean",
            "acknowledged" => "acknowledged",
            "unscanned" => "not scanned yet",
            "not-clean" => "possible secrets",
            _ => "did not finish",
        };
        let tone = match self.state {
            _ if self.blocks_agents() => "bad",
            "clean" | "acknowledged" => "ok",
            "unscanned" => "warn",
            _ => "bad",
        };
        json!({ "label": "Scan", "text": text, "tone": tone })
    }

    /// The Secret scan facts section.
    pub fn facts(&self) -> Value {
        let row = |label: &str, value: String| json!({ "label": label, "value": value });
        let mut rows = vec![
            row("State", self.state.to_string()),
            row(
                "Policy",
                format!(
                    "{}{}",
                    self.policy.as_str(),
                    if self.configured {
                        ""
                    } else {
                        " (the default)"
                    }
                ),
            ),
            row(
                "Scanner",
                if self.scanner_found {
                    "gitleaks".into()
                } else {
                    "none found (install gitleaks)".into()
                },
            ),
            row("Last scan", self.last_scan().unwrap_or("never").to_string()),
        ];
        if self.skipped() > 0 {
            rows.push(row(
                "Not read",
                format!("{} file(s) over the size cap or links", self.skipped()),
            ));
        }
        if let Some(by) = self.ack["by"].as_str() {
            rows.push(row(
                "Acknowledged",
                format!(
                    "by {by} at {}: {}",
                    self.ack["atUtc"].as_str().unwrap_or(""),
                    self.ack["note"].as_str().unwrap_or("")
                ),
            ));
        }
        for t in &self.targets {
            let value = match (&t.entry.error, t.live.len()) {
                (Some(e), _) => format!("did not finish: {e}"),
                (None, 0) if t.acked > 0 => format!("{} acknowledged", t.acked),
                (None, 0) => "clean".to_string(),
                (None, n) => format!("{n} possible secret{}", if n == 1 { "" } else { "s" }),
            };
            rows.push(row(&t.entry.label, value));
        }
        for u in &self.unscanned {
            rows.push(row(u, self.unscanned_text().into()));
        }
        let mut section = json!({
            "id": "secret-scan",
            "title": "Secret scan",
            "status": self.status(),
            "rows": rows,
        });
        if let Some(a) = self.ack_action() {
            section["actions"] = json!([a]);
        }
        section
    }
}

fn log_ack(who: &str, findings: usize, new: usize, errors: usize) {
    eprintln!(
        "chappa-stack: {who} acknowledged {findings} secret-scan finding(s) ({new} new) and {errors} unfinished scan(s)"
    );
}

/// The block for a stack that cannot be read: it cannot tell, and says so;
/// it blocks nothing (which folders it would cover is unknown).
pub fn unavailable_block(instance: &str, project: &Path, reason: &str) -> Value {
    json!({
        "instance": instance,
        "state": "error",
        "policy": null,
        "blocksAgents": false,
        "lastScanUtc": null,
        "findings": [],
        "skipped": 0,
        "bound": null,
        "project": crate::data_safety::project_dir(project),
        "reasons": [{ "component": null, "text": format!("the stack cannot be read: {reason}") }],
        "ack": null,
        "ackActionId": ACK_ACTION_ID,
    })
}

/// The view now, for the payload and the ack action.
pub fn view_of(world: &World, loaded: &Loaded) -> SecretsView {
    let scanner_found = world.secrets.scanner(&scratch_of(loaded)).is_some();
    let store = ScanStore::load(&loaded.work_dir(), &loaded.instance_id());
    let baseline = Baseline::load(&scratch_of(loaded)).map_err(|e| e.to_string());
    summarize(loaded, &store, &baseline, scanner_found)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skipped(path: &str, reason: SkipReason) -> SkippedFile {
        SkippedFile {
            path: path.into(),
            reason,
        }
    }

    #[test]
    fn a_file_over_the_per_file_cap_warns_and_the_total_cap_is_a_scan_that_did_not_finish() {
        let mut e = TargetEntry::new(TargetKind::WorkingTree, "api worktree", None, "t");
        judge_skips(
            &mut e,
            &[
                skipped("big.bin", SkipReason::TooLarge { size: 2 << 20 }),
                skipped("link", SkipReason::Symlink),
            ],
        );
        assert_eq!((e.skipped, e.error.as_deref()), (2, None));
        let mut e = TargetEntry::new(TargetKind::WorkingTree, "api worktree", None, "t");
        judge_skips(
            &mut e,
            &[
                skipped("big.bin", SkipReason::TooLarge { size: 2 << 20 }),
                skipped("late.txt", SkipReason::OverTotal { size: 10 }),
            ],
        );
        assert_eq!(e.skipped, 1);
        assert_eq!(
            e.error.as_deref(),
            Some("the scan reached its size limit, so 1 file was not read")
        );
    }

    #[test]
    fn paths_are_named_from_the_project_and_a_finished_scan_ends_its_acknowledgement() {
        let project = Path::new("/srv/example");
        assert_eq!(
            shown(
                project,
                Path::new("/srv/example/.work/ABC-1/src/api-1a2b3c4d")
            ),
            ".work/ABC-1/src/api-1a2b3c4d"
        );
        assert_eq!(
            slashed(&join_shown(".work/x", Path::new("a/b.txt"))),
            ".work/x/a/b.txt"
        );
        assert_eq!(slashed(&join_shown("", Path::new(".env"))), ".env");
        let mut store = ScanStore {
            error_ack: Some(ErrorAck {
                by: "a".into(),
                note: "b".into(),
                at_utc: "t".into(),
                covers: vec!["tree:x".into(), "tree:y".into()],
            }),
            ..ScanStore::default()
        };
        let mut failed = TargetEntry::new(TargetKind::WorkingTree, "x", None, "t");
        failed.error = Some("down".into());
        store.put("tree:x", failed);
        assert!(store.covered("tree:x"), "a failure keeps it");
        store.put(
            "tree:x",
            TargetEntry::new(TargetKind::WorkingTree, "x", None, "t"),
        );
        assert!(!store.covered("tree:x") && store.covered("tree:y"));
        store.put(
            "tree:y",
            TargetEntry::new(TargetKind::WorkingTree, "y", None, "t"),
        );
        assert_eq!(store.error_ack, None);
    }
}
