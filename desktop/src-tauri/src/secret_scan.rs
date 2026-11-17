//! The secret scan that runs before an agent launches in a folder.
//!
//! A credential sitting in the tree an agent is about to read (a committed
//! key, an untracked `.env`, a token in a config file) is one `cat` away
//! from the agent's context and its provider's logs. So before ANY agent
//! spawn with a working directory — from the rail, the control surface, a
//! record start or a source action — that directory is scanned with the
//! runner in `stack_core::safety::secrets` (untracked and ignored files
//! included, values redacted) against the project's local baseline under
//! `<project>/.work/`.
//!
//! Settings ▸ Security owns the switch ("Scan for secrets before launching
//! an agent": automatic = on when gitleaks is installed; a project can
//! override it) and the policy: `block-agents` (default) refuses the launch
//! naming each file and rule — never a value — and the user may acknowledge
//! the findings into the baseline; `warn` launches and raises a
//! notification.
//!
//! A scan that cannot finish (no scanner while the switch says to scan, a
//! timeout, a scanner error, or the size limit for one scan reached so that
//! readable files went unread) REFUSES the launch under either policy: the
//! folder is the agent's own working folder, so it would read exactly the
//! files nobody checked. The refusal says why (never a value) and the
//! launch dialog offers "Launch anyway": a confirm with a required note,
//! good for that one launch, recorded in the agent's input journal under
//! the person's actor id, and never written to the baseline. Turning the
//! scan off (the setting, or per project) still skips it entirely. Files
//! left out only for the per-file size cap or for being links are counted
//! and shown; they do not refuse.
//!
//! Findings name their file relative to the PROJECT's folder (an agent
//! started in `.work/ABC-1/src/api-1a2b3c4d` sees `.env` there reported as
//! `.work/ABC-1/src/api-1a2b3c4d/.env`), which is how a stack's own scans
//! name it too, so one acknowledgement in the baseline covers both. A
//! folder outside the project keeps names relative to itself.
//!
//! The scan runs on the spawn's own thread, which is already off the UI
//! thread (the Tauri command is `spawn_blocking`, the control route runs on
//! the server thread).

use std::path::{Path, PathBuf};
use std::time::Duration;

use project_model::settings::{SecretScanPolicy, Settings};
use serde::{Deserialize, Serialize};
use stack_core::safety::secrets::config::{choose, Chosen, ProjectScanConfig};
use stack_core::safety::secrets::{
    scan, Baseline, GitleaksScanner, ListLimits, ScanTarget, SkipReason,
};
use stack_core::safety::{Finding, Location, Policy, SafetyError, SecretScanner, Severity};
use tauri::{AppHandle, Emitter};

/// How long one pre-launch scan may take before it counts as unfinished.
pub const SCAN_TIMEOUT: Duration = Duration::from_secs(60);
/// The refusal's prefix; the JSON after it is a [`ScanBlock`]. One string
/// error, because every spawn path answers `Result<_, String>`.
pub const BLOCK_PREFIX: &str = "secret-scan-blocked:";
/// The event a non-blocking outcome raises: `{term_id, notice: ScanNotice}`.
pub const EVENT: &str = "agent://secret-scan";
/// Where a project's baseline (and the scanner's scratch space) lives.
pub const WORK_DIR: &str = ".work";

/// One finding as shown: the file, the rule and the line. There is no
/// field for the matched text, and `Finding` upstream has none either.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FindingDto {
    pub file: String,
    pub rule: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
}

impl FindingDto {
    fn from_finding(f: &Finding) -> Option<Self> {
        match &f.location {
            Location::Source { file, line, .. } => Some(Self {
                file: file.to_string_lossy().replace('\\', "/"),
                rule: f.rule.clone(),
                line: *line,
            }),
            Location::Data { .. } => None,
        }
    }

    /// The upstream finding this stands for (for the baseline). Commits are
    /// not part of a working-tree scan, so the fingerprint is `file:rule:line`.
    fn to_finding(&self) -> Finding {
        Finding {
            rule: self.rule.clone(),
            location: Location::Source {
                file: PathBuf::from(&self.file),
                line: self.line,
                commit: None,
            },
            count: 1,
            severity: Severity::Bad,
        }
    }
}

/// The refusal a blocking finding produces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanBlock {
    pub project_root: String,
    pub cwd: String,
    pub findings: Vec<FindingDto>,
    /// Files the scan did not read (too large, symlinks), so a clean-looking
    /// list is not mistaken for a complete one.
    pub skipped: usize,
    /// Set when the scan did not finish: why. The launch dialog then offers
    /// "Launch anyway" instead of acknowledging (there are no findings).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incomplete: Option<String>,
    /// Set while the project's own gitleaks config is not what the scan
    /// used (not trusted, changed since, failing the config rules): the
    /// dialog asks to review it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_review: Option<String>,
}

/// A notice for a launch that went ahead: `warn` findings, a scan that did
/// not finish and was overridden with "Launch anyway", or a clean scan that
/// used a fallback because the project's own gitleaks config needs review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ScanNotice {
    Warn {
        cwd: String,
        findings: Vec<FindingDto>,
        skipped: usize,
        #[serde(rename = "configReview", skip_serializing_if = "Option::is_none")]
        config_review: Option<String>,
    },
    LaunchedAnyway {
        cwd: String,
        reason: String,
        note: String,
        #[serde(rename = "configReview", skip_serializing_if = "Option::is_none")]
        config_review: Option<String>,
    },
    ConfigReview {
        cwd: String,
        reason: String,
    },
}

/// The configs the pre-agent scan chooses from besides Settings: the
/// project's own (pinned at its trusted hash, see [`crate::scan_config`])
/// and the open project folders (a Settings config inside one is refused).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanConfigs {
    pub project: ProjectScanConfig,
    pub open_roots: Vec<PathBuf>,
}

impl ScanConfigs {
    /// The config this scan uses, in order: the trusted project config,
    /// the Settings config, the built-in default.
    pub fn chosen(&self, settings: &Settings) -> Chosen {
        let settings_config = crate::scan_config::settings_config(
            settings,
            self.open_roots.iter().map(PathBuf::as_path),
        );
        choose(&self.project, settings_config.as_deref())
    }
}

/// What the gate decided, before it is turned into an error or a notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateOutcome {
    /// The scan is off here.
    Skipped,
    Clean {
        skipped: usize,
    },
    Blocked(ScanBlock),
    Warn {
        findings: Vec<FindingDto>,
        skipped: usize,
    },
    /// The scan did not run or did not finish: the launch is refused unless
    /// the person launches anyway.
    Incomplete(String),
}

/// Whether the scan runs in a project: the project's override, else the
/// app setting, else automatic (on when a scanner was found).
pub fn effective_enabled(
    setting: Option<bool>,
    project_pref: Option<bool>,
    scanner_found: bool,
) -> bool {
    project_pref.or(setting).unwrap_or(scanner_found)
}

/// The scanner the settings name: the configured program if any, else
/// gitleaks on `PATH`. The scratch dir is the project's `.work`, which the
/// file lister always skips, so staging is never itself scanned.
pub fn detect_scanner(settings: &Settings, project_root: &Path) -> Option<GitleaksScanner> {
    let configured = Some(settings.secret_scan_program.trim())
        .filter(|p| !p.is_empty())
        .map(Path::new);
    GitleaksScanner::detect(configured, project_root.join(WORK_DIR)).map(|mut g| {
        g.timeout = SCAN_TIMEOUT;
        // The person's own config from Settings, never one from the tree;
        // without it the scanner writes the app's config for each run.
        g.config = Some(settings.secret_scan_config.trim())
            .filter(|p| !p.is_empty())
            .map(PathBuf::from);
        g
    })
}

fn to_policy(policy: SecretScanPolicy) -> Policy {
    match policy {
        SecretScanPolicy::Warn => Policy::Warn,
        SecretScanPolicy::BlockAgents => Policy::BlockAgents,
    }
}

/// Scan `cwd` with `scanner` under `policy`, subtracting the baseline kept
/// in `<project_root>/.work`. Pure over the scanner, so tests use a fake.
pub fn evaluate(
    scanner: &dyn SecretScanner,
    policy: SecretScanPolicy,
    project_root: &Path,
    cwd: &Path,
    limits: &ListLimits,
) -> GateOutcome {
    let work = project_root.join(WORK_DIR);
    let baseline = match Baseline::load(&work) {
        Ok(b) => b,
        Err(e) => {
            return GateOutcome::Incomplete(format!("the secret baseline could not be read: {e}"))
        }
    };
    let target = ScanTarget::WorkingTree {
        root: cwd.to_path_buf(),
        excludes: Vec::new(),
    };
    // Scanned without the baseline, then named from the project and only
    // then compared with it, so the baseline holds project-relative names.
    let report = match scan(scanner, &target, limits, &Baseline::default()) {
        Ok(report) => report,
        Err(SafetyError::Timeout { seconds, .. }) => {
            return GateOutcome::Incomplete(format!(
                "the secret scan did not finish within {seconds} s"
            ));
        }
        Err(e) => return GateOutcome::Incomplete(format!("the secret scan failed: {e}")),
    };
    let prefix = project_prefix(project_root, cwd);
    let named: Vec<Finding> = report
        .findings
        .into_iter()
        .map(|f| from_project(&prefix, f))
        .collect();
    let (fresh, _) = baseline.apply(named);
    let findings: Vec<FindingDto> = fresh.iter().filter_map(FindingDto::from_finding).collect();
    let over_total = report
        .skipped
        .iter()
        .filter(|s| matches!(s.reason, SkipReason::OverTotal { .. }))
        .count();
    let skipped = report.skipped.len() - over_total;
    let blocks = !matches!(to_policy(policy), Policy::Warn);
    if !findings.is_empty() && blocks {
        return GateOutcome::Blocked(ScanBlock {
            project_root: project_root.display().to_string(),
            cwd: cwd.display().to_string(),
            findings,
            skipped,
            incomplete: None,
            config_review: None,
        });
    }
    if over_total > 0 {
        return GateOutcome::Incomplete(format!(
            "the secret scan reached its size limit, so {over_total} file{} in the folder {} not read",
            if over_total == 1 { "" } else { "s" },
            if over_total == 1 { "was" } else { "were" }
        ));
    }
    if findings.is_empty() {
        GateOutcome::Clean { skipped }
    } else {
        GateOutcome::Warn { findings, skipped }
    }
}

/// The folder `cwd` is in, relative to the project (`""` when it is the
/// project itself, or outside it).
fn project_prefix(project_root: &Path, cwd: &Path) -> String {
    let strip = |root: &Path, dir: &Path| {
        dir.strip_prefix(root)
            .ok()
            .map(|r| r.to_string_lossy().replace('\\', "/"))
    };
    if let (Ok(root), Ok(dir)) = (
        std::fs::canonicalize(project_root),
        std::fs::canonicalize(cwd),
    ) {
        if let Some(rel) = strip(&root, &dir) {
            return rel;
        }
    }
    let abs = |p: &Path| std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    strip(&abs(project_root), &abs(cwd)).unwrap_or_default()
}

/// A finding of a scan rooted at the agent's folder, named from the project.
fn from_project(prefix: &str, mut f: Finding) -> Finding {
    if let Location::Source { file, .. } = &mut f.location {
        if !prefix.is_empty() {
            *file = PathBuf::from(format!(
                "{prefix}/{}",
                file.to_string_lossy().replace('\\', "/")
            ));
        }
    }
    f
}

/// The whole gate for one launch: resolve the switch, find the scanner,
/// scan. The outcome is still data; [`gate_launch`] turns it into the
/// spawn's error or notice.
pub fn decide(
    settings: &Settings,
    project_pref: Option<bool>,
    project_root: &Path,
    cwd: &Path,
) -> GateOutcome {
    decide_with(
        settings,
        project_pref,
        project_root,
        cwd,
        &ScanConfigs::default(),
    )
    .0
}

/// [`decide`] with the project's own config and the open project folders:
/// the outcome, and why the project's config was not used when it was not
/// (for the review banner). A scan that is off says nothing about configs.
pub fn decide_with(
    settings: &Settings,
    project_pref: Option<bool>,
    project_root: &Path,
    cwd: &Path,
    configs: &ScanConfigs,
) -> (GateOutcome, Option<String>) {
    // A folder that does not exist holds nothing an agent could read (the
    // spawn fails, or starts elsewhere, on its own terms).
    if !cwd.is_dir() {
        return (GateOutcome::Skipped, None);
    }
    let scanner = detect_scanner(settings, project_root);
    if !effective_enabled(
        settings.secret_scan_before_agents,
        project_pref,
        scanner.is_some(),
    ) {
        return (GateOutcome::Skipped, None);
    }
    match scanner {
        Some(mut scanner) => {
            // Chosen (and the project's file re-hashed) right before the scan.
            let chosen = configs.chosen(settings);
            chosen.apply(&mut scanner);
            let outcome = evaluate(
                &scanner,
                settings.secret_scan_policy,
                project_root,
                cwd,
                &ListLimits::default(),
            );
            (outcome, chosen.review)
        }
        None => (
            GateOutcome::Incomplete(
                "the secret scan is on but gitleaks was not found (install it, or name its path under Settings ▸ Security)".into(),
            ),
            None,
        ),
    }
}

/// Run the gate for a spawn into `cwd`. `Err` refuses the launch (a
/// blocking finding, or a scan that did not finish, as [`BLOCK_PREFIX`] +
/// JSON); `Ok(Some(notice))` lets it go ahead with something to tell the
/// user; `Ok(None)` is clean or off. `launch_anyway` is the person's note
/// from the "Launch anyway" confirm: it lets THIS launch past a scan that
/// did not finish (never past findings, which are acknowledged instead).
pub fn gate_launch(
    settings: &Settings,
    project_pref: Option<bool>,
    project_root: &Path,
    cwd: &Path,
    launch_anyway: Option<&str>,
) -> Result<Option<ScanNotice>, String> {
    gate_launch_with(
        settings,
        project_pref,
        project_root,
        cwd,
        launch_anyway,
        &ScanConfigs::default(),
    )
}

/// [`gate_launch`] with the project's own config and the open project
/// folders. While the project's config is not the one used, the refusal
/// and every notice carry why, and a clean launch raises a notice of its
/// own asking to review it.
pub fn gate_launch_with(
    settings: &Settings,
    project_pref: Option<bool>,
    project_root: &Path,
    cwd: &Path,
    launch_anyway: Option<&str>,
    configs: &ScanConfigs,
) -> Result<Option<ScanNotice>, String> {
    let (outcome, config_review) = decide_with(settings, project_pref, project_root, cwd, configs);
    match outcome {
        GateOutcome::Skipped => Ok(None),
        GateOutcome::Clean { .. } => Ok(config_review.map(|reason| ScanNotice::ConfigReview {
            cwd: cwd.display().to_string(),
            reason,
        })),
        GateOutcome::Blocked(mut block) => {
            block.config_review = config_review;
            Err(block_error(&block))
        }
        GateOutcome::Warn { findings, skipped } => Ok(Some(ScanNotice::Warn {
            cwd: cwd.display().to_string(),
            findings,
            skipped,
            config_review,
        })),
        GateOutcome::Incomplete(reason) => {
            match launch_anyway.map(str::trim).filter(|n| !n.is_empty()) {
                Some(note) => Ok(Some(ScanNotice::LaunchedAnyway {
                    cwd: cwd.display().to_string(),
                    reason,
                    note: note.chars().take(1000).collect(),
                    config_review,
                })),
                None => Err(block_error(&ScanBlock {
                    project_root: project_root.display().to_string(),
                    cwd: cwd.display().to_string(),
                    findings: Vec::new(),
                    skipped: 0,
                    incomplete: Some(reason),
                    config_review,
                })),
            }
        }
    }
}

/// The journal line a "Launch anyway" leaves on the agent's row.
pub fn launch_anyway_record(notice: &ScanNotice) -> Option<String> {
    match notice {
        ScanNotice::LaunchedAnyway {
            cwd, reason, note, ..
        } => Some(format!(
            "launched anyway: the secret scan of {cwd} did not finish ({reason}); note: {note}"
        )),
        ScanNotice::Warn { .. } | ScanNotice::ConfigReview { .. } => None,
    }
}

/// The refusal text: a sentence for logs and plain callers, then the
/// machine-readable block for the dialog.
pub fn block_error(block: &ScanBlock) -> String {
    format!(
        "{BLOCK_PREFIX}{}",
        serde_json::to_string(block).unwrap_or_default()
    )
}

/// The block behind a refusal, if the error is one.
pub fn parse_block_error(err: &str) -> Option<ScanBlock> {
    serde_json::from_str(err.strip_prefix(BLOCK_PREFIX)?).ok()
}

/// A one-line description of a refusal for places that show plain text.
pub fn describe_block(block: &ScanBlock) -> String {
    if let Some(reason) = &block.incomplete {
        return format!(
            "the agent was not launched: the secret scan of {} did not finish ({reason}); fix the scanner, turn the scan off for the project, or launch anyway from the app",
            block.cwd
        );
    }
    let list: Vec<String> = block
        .findings
        .iter()
        .map(|f| match f.line {
            Some(l) => format!("{}:{} ({})", f.file, l, f.rule),
            None => format!("{} ({})", f.file, f.rule),
        })
        .collect();
    format!(
        "the agent was not launched: {} secret{} found in {} — {}; fix or acknowledge them first",
        block.findings.len(),
        if block.findings.len() == 1 { "" } else { "s" },
        block.cwd,
        list.join(", ")
    )
}

/// Record `findings` in the project's baseline with the user's note, so
/// they stop blocking launches until the file changes around them.
pub fn acknowledge(
    project_root: &Path,
    findings: &[FindingDto],
    who: &str,
    note: &str,
) -> Result<usize, String> {
    let work = project_root.join(WORK_DIR);
    let mut baseline = Baseline::load(&work).map_err(|e| e.to_string())?;
    let upstream: Vec<Finding> = findings.iter().map(FindingDto::to_finding).collect();
    let added = baseline.acknowledge(
        &upstream,
        who,
        note,
        &crate::sources::iso_utc(term_core::actor::epoch_ms()),
    );
    baseline.save(&work).map_err(|e| e.to_string())?;
    log::info!(
        "[secret-scan] {who} acknowledged {} finding(s) in {} ({added} new): {}",
        findings.len(),
        project_root.display(),
        findings
            .iter()
            .map(|f| format!("{}:{}", f.file, f.rule))
            .collect::<Vec<_>>()
            .join(", ")
    );
    Ok(added)
}

/// Raise a notice for a launch that went ahead.
pub fn emit_notice(app: Option<&AppHandle>, term_id: u32, notice: &ScanNotice) {
    match notice {
        ScanNotice::Warn { cwd, findings, .. } => log::warn!(
            "[secret-scan] launched in {cwd} with {} finding(s): {}",
            findings.len(),
            findings
                .iter()
                .map(|f| format!("{}:{}", f.file, f.rule))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        ScanNotice::LaunchedAnyway { cwd, reason, .. } => {
            log::warn!("[secret-scan] launched anyway in {cwd}, unscanned: {reason}")
        }
        ScanNotice::ConfigReview { cwd, reason } => {
            log::warn!("[secret-scan] launched in {cwd}: {reason}")
        }
    }
    if let Some(app) = app {
        let _ = app.emit(
            EVENT,
            serde_json::json!({ "term_id": term_id, "notice": notice }),
        );
    }
}

/// The "Add to .gitignore" action: append `line` to the project's root
/// `.gitignore` (created when missing, never a duplicate line). Only a line
/// naming one of the known agent settings files is accepted.
pub fn add_agent_settings_ignore_line(project_root: &Path, line: &str) -> Result<bool, String> {
    let known = stack_core::safety::agent_settings::AGENT_SETTINGS_FILES
        .iter()
        .any(|f| line.trim().ends_with(&format!("/{f}")));
    if !known {
        return Err(format!("`{line}` does not name an agent settings file"));
    }
    stack_core::safety::agent_settings::add_to_gitignore(project_root, line)
        .map_err(|e| format!("cannot update .gitignore: {e}"))
}

/// What the Security tab shows: whether a scanner was found and where.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScannerInfo {
    pub found: bool,
    pub program: Option<String>,
    /// The switch as it stands app-wide with the automatic default applied.
    pub effective_enabled: bool,
    /// Why the gitleaks config the settings name is not used (it lies
    /// inside an open project folder), when it is not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_refused: Option<String>,
}

pub fn scanner_info(settings: &Settings) -> ScannerInfo {
    scanner_info_with(settings, &[])
}

/// [`scanner_info`], checking the settings' config against the open project
/// folders `roots`.
pub fn scanner_info_with(settings: &Settings, roots: &[PathBuf]) -> ScannerInfo {
    let scanner = detect_scanner(settings, Path::new("."));
    let set = settings.secret_scan_config.trim();
    ScannerInfo {
        found: scanner.is_some(),
        program: scanner.as_ref().map(|s| s.program.display().to_string()),
        effective_enabled: effective_enabled(
            settings.secret_scan_before_agents,
            None,
            scanner.is_some(),
        ),
        config_refused: (!set.is_empty())
            .then(|| {
                stack_core::safety::secrets::config::settings_path_refusal(
                    Path::new(set),
                    roots.iter().map(PathBuf::as_path),
                )
            })
            .flatten(),
    }
}

// ---- Tauri commands ----------------------------------------------------------------

#[tauri::command]
pub fn secret_scan_info(
    settings: tauri::State<'_, crate::settings::SettingsState>,
    projects: tauri::State<'_, crate::projects::Projects>,
) -> ScannerInfo {
    scanner_info_with(&settings.snapshot(), &projects.open_roots())
}

/// "Add to .gitignore" for an exposed agent settings file (confirmed in the
/// webview first). Answers whether a line was added; the standing warnings
/// are checked again right after, so a resolved one goes away for good.
#[tauri::command]
pub fn add_agent_settings_ignore(
    app: AppHandle,
    projects: tauri::State<'_, crate::projects::Projects>,
    project_root: String,
    line: String,
) -> Result<bool, String> {
    let root = Path::new(&project_root);
    let added = add_agent_settings_ignore_line(root, &line)?;
    let warnings = projects.agent_settings();
    let changed = warnings.revalidate();
    crate::agent_settings::emit(
        Some(&app),
        &warnings,
        None,
        root,
        &crate::agent_settings::Update {
            new: Vec::new(),
            changed,
        },
    );
    Ok(added)
}

/// Every standing agent settings warning (they outlive a restart until
/// resolved), for the webview's Needs you rows at boot.
#[tauri::command]
pub fn agent_settings_warnings(
    projects: tauri::State<'_, crate::projects::Projects>,
) -> serde_json::Value {
    let all = projects.agent_settings().all();
    let mut payload = crate::agent_settings::payload(None, Path::new(""), &[], &all);
    payload["all"].take()
}

/// Acknowledge the findings of a refused launch into the project's
/// baseline. The caller then starts the agent again.
#[tauri::command]
pub fn acknowledge_secret_findings(
    projects: tauri::State<'_, crate::projects::Projects>,
    project_id: u32,
    findings: Vec<FindingDto>,
    note: Option<String>,
) -> Result<usize, String> {
    let (_, root) = projects
        .project_root(project_id)
        .ok_or_else(|| format!("no such project: {project_id}"))?;
    // The baseline is the one the project's stack reads too: while its
    // definition is not the trusted one, the stack is read-only from here
    // as well.
    if let Some(why) = projects.stack_refusal(project_id, None) {
        return Err(why);
    }
    acknowledge(
        &root,
        &findings,
        "user",
        note.as_deref()
            .unwrap_or("acknowledged from the launch dialog"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Answers canned findings, or hangs for a timeout test.
    struct Fake {
        answer: RefCell<Vec<Finding>>,
        /// Built per call: `SafetyError` is not `Clone`.
        fail: Option<fn() -> SafetyError>,
    }

    impl Fake {
        fn with(findings: Vec<Finding>) -> Self {
            Self {
                answer: RefCell::new(findings),
                fail: None,
            }
        }
    }

    impl SecretScanner for Fake {
        fn scan_files(
            &self,
            _root: &Path,
            _files: &[PathBuf],
        ) -> Result<Vec<Finding>, SafetyError> {
            if let Some(make) = self.fail {
                return Err(make());
            }
            Ok(self.answer.borrow().clone())
        }
        fn scan_commits(&self, _repo: &Path, _range: &str) -> Result<Vec<Finding>, SafetyError> {
            Ok(Vec::new())
        }
    }

    fn finding(file: &str, rule: &str, line: u32) -> Finding {
        Finding {
            rule: rule.into(),
            location: Location::Source {
                file: PathBuf::from(file),
                line: Some(line),
                commit: None,
            },
            count: 1,
            severity: Severity::Bad,
        }
    }

    fn no_git() -> ListLimits {
        ListLimits {
            use_git: false,
            ..ListLimits::default()
        }
    }

    fn tree() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub").join(".env"), "KEY=value\n").unwrap();
        std::fs::write(root.join("readme"), "hello\n").unwrap();
        (dir, root)
    }

    #[test]
    fn the_switch_follows_project_then_setting_then_scanner_presence() {
        assert!(effective_enabled(None, None, true));
        assert!(!effective_enabled(None, None, false));
        assert!(effective_enabled(Some(true), None, false));
        assert!(!effective_enabled(Some(false), None, true));
        assert!(
            effective_enabled(Some(false), Some(true), false),
            "the project wins"
        );
        assert!(!effective_enabled(Some(true), Some(false), true));
    }

    #[test]
    fn block_agents_refuses_with_file_and_rule_and_never_a_value() {
        let (_dir, root) = tree();
        let fake = Fake::with(vec![finding("sub/.env", "generic-api-key", 1)]);
        let outcome = evaluate(
            &fake,
            SecretScanPolicy::BlockAgents,
            &root,
            &root,
            &no_git(),
        );
        let GateOutcome::Blocked(block) = outcome else {
            panic!("{outcome:?}")
        };
        assert_eq!(
            block.findings,
            vec![FindingDto {
                file: "sub/.env".into(),
                rule: "generic-api-key".into(),
                line: Some(1)
            }]
        );
        let err = block_error(&block);
        assert!(err.starts_with(BLOCK_PREFIX));
        assert!(
            !err.contains("value"),
            "the refusal never carries matched text: {err}"
        );
        assert_eq!(parse_block_error(&err).unwrap(), block);
        assert!(parse_block_error("busy guard: refused").is_none());
        let text = describe_block(&block);
        assert!(
            text.contains("sub/.env:1 (generic-api-key)") && text.contains("acknowledge"),
            "{text}"
        );
    }

    #[test]
    fn warn_lets_the_launch_through_with_the_findings() {
        let (_dir, root) = tree();
        let fake = Fake::with(vec![finding("sub/.env", "r", 1), finding("readme", "s", 3)]);
        match evaluate(&fake, SecretScanPolicy::Warn, &root, &root, &no_git()) {
            GateOutcome::Warn { findings, .. } => assert_eq!(findings.len(), 2),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            evaluate(
                &Fake::with(vec![]),
                SecretScanPolicy::BlockAgents,
                &root,
                &root,
                &no_git()
            ),
            GateOutcome::Clean { skipped: 0 }
        );
    }

    #[test]
    fn acknowledging_writes_the_baseline_and_the_next_scan_is_clean() {
        let (_dir, root) = tree();
        let fake = Fake::with(vec![finding("sub/.env", "generic-api-key", 1)]);
        let GateOutcome::Blocked(block) = evaluate(
            &fake,
            SecretScanPolicy::BlockAgents,
            &root,
            &root,
            &no_git(),
        ) else {
            panic!()
        };
        assert_eq!(
            acknowledge(&root, &block.findings, "user", "test fixture").unwrap(),
            1
        );
        let baseline =
            std::fs::read_to_string(root.join(WORK_DIR).join("secrets-baseline.json")).unwrap();
        assert!(
            baseline.contains("generic-api-key") && !baseline.contains("KEY=value"),
            "{baseline}"
        );
        let notes =
            std::fs::read_to_string(root.join(WORK_DIR).join("secrets-baseline.notes.json"))
                .unwrap();
        assert!(notes.contains("test fixture"));
        assert_eq!(
            evaluate(
                &fake,
                SecretScanPolicy::BlockAgents,
                &root,
                &root,
                &no_git()
            ),
            GateOutcome::Clean { skipped: 0 }
        );
        // Acknowledging again is idempotent (nothing new).
        assert_eq!(
            acknowledge(&root, &block.findings, "user", "again").unwrap(),
            0
        );
        // A finding on another line is not covered.
        let moved = Fake::with(vec![finding("sub/.env", "generic-api-key", 2)]);
        assert!(matches!(
            evaluate(
                &moved,
                SecretScanPolicy::BlockAgents,
                &root,
                &root,
                &no_git()
            ),
            GateOutcome::Blocked(_)
        ));
    }

    #[test]
    fn a_timeout_or_scanner_error_is_an_incomplete_scan() {
        let (_dir, root) = tree();
        let hung = Fake {
            answer: RefCell::new(vec![]),
            fail: Some(|| SafetyError::Timeout {
                what: "gitleaks".into(),
                seconds: 60,
            }),
        };
        for policy in [SecretScanPolicy::BlockAgents, SecretScanPolicy::Warn] {
            match evaluate(&hung, policy, &root, &root, &no_git()) {
                GateOutcome::Incomplete(reason) => {
                    assert!(reason.contains("did not finish within 60 s"), "{reason}")
                }
                other => panic!("{other:?}"),
            }
        }
        let broken = Fake {
            answer: RefCell::new(vec![]),
            fail: Some(|| SafetyError::InvalidTarget("no such scanner".into())),
        };
        assert!(matches!(
            evaluate(
                &broken,
                SecretScanPolicy::BlockAgents,
                &root,
                &root,
                &no_git()
            ),
            GateOutcome::Incomplete(_)
        ));
    }

    #[test]
    fn an_incomplete_scan_refuses_under_either_policy_until_launched_anyway_with_a_note() {
        let (_dir, root) = tree();
        let mut settings = Settings {
            secret_scan_before_agents: Some(true),
            secret_scan_program: root.join("no-such-gitleaks").display().to_string(),
            ..Settings::default()
        };
        for policy in [SecretScanPolicy::BlockAgents, SecretScanPolicy::Warn] {
            settings.secret_scan_policy = policy;
            let err = gate_launch(&settings, None, &root, &root, None).unwrap_err();
            let block = parse_block_error(&err).expect("a refusal the dialog can read");
            assert!(block.findings.is_empty());
            let reason = block.incomplete.clone().unwrap();
            assert!(reason.contains("gitleaks was not found"), "{reason}");
            assert!(
                describe_block(&block).contains("did not finish"),
                "{}",
                describe_block(&block)
            );
            // A blank note is no override.
            assert!(gate_launch(&settings, None, &root, &root, Some("  ")).is_err());
            let notice = gate_launch(
                &settings,
                None,
                &root,
                &root,
                Some("scanner is being reinstalled"),
            )
            .unwrap()
            .unwrap();
            let line = launch_anyway_record(&notice).unwrap();
            assert!(
                line.contains("launched anyway") && line.contains("scanner is being reinstalled"),
                "{line}"
            );
        }
        // The override is not an acknowledgement: nothing reaches the baseline.
        assert!(!root.join(WORK_DIR).join("secrets-baseline.json").exists());
        // Turning the scan off still skips it entirely.
        assert_eq!(
            gate_launch(&settings, Some(false), &root, &root, None),
            Ok(None)
        );
    }

    #[test]
    fn the_total_size_limit_is_an_incomplete_scan_and_the_per_file_cap_only_counts() {
        let (_dir, root) = tree();
        std::fs::write(root.join("big.bin"), vec![b'x'; 4096]).unwrap();
        let fake = Fake::with(vec![]);
        let per_file = ListLimits {
            max_file_bytes: 1024,
            ..no_git()
        };
        assert_eq!(
            evaluate(
                &fake,
                SecretScanPolicy::BlockAgents,
                &root,
                &root,
                &per_file
            ),
            GateOutcome::Clean { skipped: 1 }
        );
        let total = ListLimits {
            max_total_bytes: 8,
            ..no_git()
        };
        match evaluate(&fake, SecretScanPolicy::Warn, &root, &root, &total) {
            GateOutcome::Incomplete(reason) => assert!(reason.contains("size limit"), "{reason}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn findings_in_a_subfolder_are_named_from_the_project_so_one_baseline_entry_serves_both_sides()
    {
        let (_dir, root) = tree();
        let wt = root
            .join(".work")
            .join("ABC-1")
            .join("src")
            .join("api-1a2b3c4d");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".env"), "KEY=value\n").unwrap();
        let fake = Fake::with(vec![finding(".env", "planted", 1)]);
        let GateOutcome::Blocked(block) =
            evaluate(&fake, SecretScanPolicy::BlockAgents, &root, &wt, &no_git())
        else {
            panic!()
        };
        assert_eq!(block.findings[0].file, ".work/ABC-1/src/api-1a2b3c4d/.env");
        acknowledge(&root, &block.findings, "user", "fixture").unwrap();
        let raw =
            std::fs::read_to_string(root.join(WORK_DIR).join("secrets-baseline.json")).unwrap();
        // The same fingerprint the stack's scan of that worktree uses.
        assert!(
            raw.contains("\".work/ABC-1/src/api-1a2b3c4d/.env:planted:1\""),
            "{raw}"
        );
        assert_eq!(
            evaluate(&fake, SecretScanPolicy::BlockAgents, &root, &wt, &no_git()),
            GateOutcome::Clean { skipped: 0 }
        );
    }

    #[test]
    fn decide_skips_when_off_and_reports_a_missing_scanner_when_forced_on() {
        let (_dir, root) = tree();
        let mut settings = Settings {
            secret_scan_before_agents: Some(false),
            ..Settings::default()
        };
        assert_eq!(decide(&settings, None, &root, &root), GateOutcome::Skipped);
        // Forced on with a program that does not exist: the setting names a
        // path, nothing is found there, and the scan is incomplete, saying so.
        settings.secret_scan_before_agents = Some(true);
        settings.secret_scan_program = root.join("no-such-gitleaks").display().to_string();
        match decide(&settings, None, &root, &root) {
            GateOutcome::Incomplete(reason) => assert!(reason.contains("not found"), "{reason}"),
            other => panic!("{other:?}"),
        }
        // A project override turns it off whatever the app says.
        assert_eq!(
            decide(&settings, Some(false), &root, &root),
            GateOutcome::Skipped
        );
        // Automatic + the same missing program = off (no scanner was found).
        settings.secret_scan_before_agents = None;
        assert_eq!(decide(&settings, None, &root, &root), GateOutcome::Skipped);
        let info = scanner_info(&settings);
        assert!(!info.found && !info.effective_enabled);
    }

    /// With gitleaks installed: a planted token in a git-ignored `.env`
    /// blocks, the acknowledgement clears it. Skips (and says so) otherwise.
    #[test]
    fn real_gitleaks_blocks_an_ignored_env_token_until_acknowledged() {
        let settings = Settings::default();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let Some(scanner) = detect_scanner(&settings, &root) else {
            eprintln!("skipping: gitleaks is not on PATH");
            return;
        };
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&root)
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@example.test",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        if !git(&["init", "-q"]) {
            eprintln!("skipping: git is not on PATH");
            return;
        }
        std::fs::write(root.join(".gitignore"), ".env\n").unwrap();
        std::fs::write(root.join("app.txt"), "nothing to see\n").unwrap();
        // Assembled at run time so no source file holds a token-shaped string.
        let token = format!("ghp_{}", "A1b2C3d4".repeat(5));
        std::fs::write(root.join(".env"), format!("GITHUB_TOKEN={token}\n")).unwrap();
        let outcome = evaluate(
            &scanner,
            SecretScanPolicy::BlockAgents,
            &root,
            &root,
            &ListLimits::default(),
        );
        let GateOutcome::Blocked(block) = outcome else {
            panic!("{outcome:?}")
        };
        // The token's own rule, and the app config's rule for a credential
        // assigned inline: both on the one line of `.env`.
        let rules: Vec<&str> = block.findings.iter().map(|f| f.rule.as_str()).collect();
        assert_eq!(
            rules,
            ["github-pat", "inline-credential-assignment"],
            "{block:?}"
        );
        assert!(block.findings.iter().all(|f| f.file == ".env"));
        assert!(!block_error(&block).contains(&token));
        acknowledge(&root, &block.findings, "user", "test").unwrap();
        assert!(matches!(
            evaluate(
                &scanner,
                SecretScanPolicy::BlockAgents,
                &root,
                &root,
                &ListLimits::default()
            ),
            GateOutcome::Clean { .. }
        ));
        assert!(
            !std::fs::read_to_string(root.join(WORK_DIR).join("secrets-baseline.json"))
                .unwrap()
                .contains(&token)
        );
    }

    /// The launch gate with the project's own gitleaks config, trusted at
    /// its hash: its exact-match allowlist lets one public test value
    /// through. Edited since, the gate falls back to the built-in rules
    /// (the value blocks the launch again) and the refusal asks to review
    /// the config. Real gitleaks.
    #[test]
    fn real_gitleaks_uses_the_trusted_project_config_and_falls_back_with_a_review() {
        let settings = Settings::default();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir_all(root.join("tools")).unwrap();
        if detect_scanner(&settings, &root).is_none() {
            eprintln!("skipping: gitleaks is not on PATH");
            return;
        }
        // Assembled at run time so no source line holds a token-shaped string.
        let token = format!("ghp_{}", "PubL1cTe5tVa1ue".repeat(3))[..40].to_string();
        std::fs::write(root.join("sample.txt"), format!("sample = {token}\n")).unwrap();
        let config_path = root.join("tools").join("gitleaks.toml");
        let config =
            format!("[extend]\nuseDefault = true\n\n[allowlist]\nregexes = ['''^{token}$''']\n");
        std::fs::write(&config_path, &config).unwrap();
        let configs = ScanConfigs {
            project: ProjectScanConfig::Trusted {
                path: config_path.clone(),
                hash: stack_core::safety::secrets::config::content_hash(config.as_bytes()),
            },
            open_roots: vec![root.clone()],
        };
        let mut on = settings.clone();
        on.secret_scan_before_agents = Some(true);
        assert_eq!(
            gate_launch_with(&on, None, &root, &root, None, &configs),
            Ok(None),
            "the trusted config's allowlist lets the known value through"
        );
        // Without it, the built-in rules find the value.
        let err =
            gate_launch_with(&on, None, &root, &root, None, &ScanConfigs::default()).unwrap_err();
        assert!(parse_block_error(&err)
            .unwrap()
            .findings
            .iter()
            .any(|f| f.file == "sample.txt"));
        // Edited after it was trusted: not used, blocked, asked to review.
        std::fs::write(&config_path, format!("{config}# edited\n")).unwrap();
        let err = gate_launch_with(&on, None, &root, &root, None, &configs).unwrap_err();
        let block = parse_block_error(&err).unwrap();
        assert!(
            block.findings.iter().any(|f| f.file == "sample.txt"),
            "{block:?}"
        );
        let review = block.config_review.clone().unwrap();
        assert!(review.contains("changed since it was trusted"), "{review}");
        assert!(!err.contains(&token));
        // A clean launch that fell back still says so.
        std::fs::remove_file(root.join("sample.txt")).unwrap();
        match gate_launch_with(&on, None, &root, &root, None, &configs) {
            Ok(Some(ScanNotice::ConfigReview { reason, .. })) => {
                assert!(reason.contains("changed since it was trusted"))
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_ignore_action_only_takes_agent_settings_lines_and_never_duplicates() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            add_agent_settings_ignore_line(tmp.path(), "/.claude/settings.local.json"),
            Ok(true)
        );
        assert_eq!(
            add_agent_settings_ignore_line(tmp.path(), "/.claude/settings.local.json"),
            Ok(false)
        );
        assert_eq!(
            add_agent_settings_ignore_line(tmp.path(), "/sub/.claude/settings.local.json"),
            Ok(true)
        );
        assert!(add_agent_settings_ignore_line(tmp.path(), "/src/main.rs").is_err());
        let text = std::fs::read_to_string(tmp.path().join(".gitignore")).unwrap();
        assert_eq!(
            text,
            "/.claude/settings.local.json\n/sub/.claude/settings.local.json\n"
        );
    }
}
