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
//! notification. A scan that cannot finish (no scanner after all, a
//! timeout, a scanner error) fails OPEN to a warning that says so: a broken
//! scanner must not make every agent unlaunchable.
//!
//! The scan runs on the spawn's own thread, which is already off the UI
//! thread (the Tauri command is `spawn_blocking`, the control route runs on
//! the server thread).

use std::path::{Path, PathBuf};
use std::time::Duration;

use project_model::settings::{SecretScanPolicy, Settings};
use serde::{Deserialize, Serialize};
use stack_core::safety::secrets::{scan, Baseline, GitleaksScanner, ListLimits, ScanTarget};
use stack_core::safety::{Finding, Location, Policy, SafetyError, SecretScanner, Severity};
use tauri::{AppHandle, Emitter};

/// How long one pre-launch scan may take before it fails open.
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
            Location::Source { file, line, .. } => Some(Self { file: file.to_string_lossy().replace('\\', "/"), rule: f.rule.clone(), line: *line }),
            Location::Data { .. } => None,
        }
    }

    /// The upstream finding this stands for (for the baseline). Commits are
    /// not part of a working-tree scan, so the fingerprint is `file:rule:line`.
    fn to_finding(&self) -> Finding {
        Finding {
            rule: self.rule.clone(),
            location: Location::Source { file: PathBuf::from(&self.file), line: self.line, commit: None },
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
}

/// A notice for a launch that went ahead: `warn` findings, or an
/// `incomplete` scan with the reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ScanNotice {
    Warn { cwd: String, findings: Vec<FindingDto>, skipped: usize },
    Incomplete { cwd: String, reason: String },
}

/// What the gate decided, before it is turned into an error or a notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateOutcome {
    /// The scan is off here.
    Skipped,
    Clean { skipped: usize },
    Blocked(ScanBlock),
    Warn { findings: Vec<FindingDto>, skipped: usize },
    /// The scan did not run or did not finish; the launch goes ahead.
    Incomplete(String),
}

/// Whether the scan runs in a project: the project's override, else the
/// app setting, else automatic (on when a scanner was found).
pub fn effective_enabled(setting: Option<bool>, project_pref: Option<bool>, scanner_found: bool) -> bool {
    project_pref.or(setting).unwrap_or(scanner_found)
}

/// The scanner the settings name: the configured program if any, else
/// gitleaks on `PATH`. The scratch dir is the project's `.work`, which the
/// file lister always skips, so staging is never itself scanned.
pub fn detect_scanner(settings: &Settings, project_root: &Path) -> Option<GitleaksScanner> {
    let configured = Some(settings.secret_scan_program.trim()).filter(|p| !p.is_empty()).map(Path::new);
    GitleaksScanner::detect(configured, project_root.join(WORK_DIR)).map(|mut g| {
        g.timeout = SCAN_TIMEOUT;
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
pub fn evaluate(scanner: &dyn SecretScanner, policy: SecretScanPolicy, project_root: &Path, cwd: &Path, limits: &ListLimits) -> GateOutcome {
    let work = project_root.join(WORK_DIR);
    let baseline = match Baseline::load(&work) {
        Ok(b) => b,
        Err(e) => return GateOutcome::Incomplete(format!("the secret baseline could not be read: {e}")),
    };
    let target = ScanTarget::WorkingTree { root: cwd.to_path_buf(), excludes: Vec::new() };
    let report = match scan(scanner, &target, limits, &baseline) {
        Ok(report) => report,
        Err(SafetyError::Timeout { seconds, .. }) => {
            return GateOutcome::Incomplete(format!("the secret scan did not finish within {seconds} s"));
        }
        Err(e) => return GateOutcome::Incomplete(format!("the secret scan failed: {e}")),
    };
    let findings: Vec<FindingDto> = report.findings.iter().filter_map(FindingDto::from_finding).collect();
    let skipped = report.skipped.len();
    if findings.is_empty() {
        return GateOutcome::Clean { skipped };
    }
    match to_policy(policy) {
        Policy::Warn => GateOutcome::Warn { findings, skipped },
        _ => GateOutcome::Blocked(ScanBlock {
            project_root: project_root.display().to_string(),
            cwd: cwd.display().to_string(),
            findings,
            skipped,
        }),
    }
}

/// The whole gate for one launch: resolve the switch, find the scanner,
/// scan. The outcome is still data; [`gate_launch`] turns it into the
/// spawn's error or notice.
pub fn decide(settings: &Settings, project_pref: Option<bool>, project_root: &Path, cwd: &Path) -> GateOutcome {
    let scanner = detect_scanner(settings, project_root);
    if !effective_enabled(settings.secret_scan_before_agents, project_pref, scanner.is_some()) {
        return GateOutcome::Skipped;
    }
    match scanner {
        Some(scanner) => evaluate(&scanner, settings.secret_scan_policy, project_root, cwd, &ListLimits::default()),
        None => GateOutcome::Incomplete(
            "the secret scan is on but gitleaks was not found (install it, or name its path under Settings ▸ Security)".into(),
        ),
    }
}

/// Run the gate for a spawn into `cwd`. `Err` refuses the launch (a
/// blocking finding, as [`BLOCK_PREFIX`] + JSON); `Ok(Some(notice))` lets it
/// go ahead with something to tell the user; `Ok(None)` is clean or off.
pub fn gate_launch(settings: &Settings, project_pref: Option<bool>, project_root: &Path, cwd: &Path) -> Result<Option<ScanNotice>, String> {
    match decide(settings, project_pref, project_root, cwd) {
        GateOutcome::Skipped | GateOutcome::Clean { .. } => Ok(None),
        GateOutcome::Blocked(block) => Err(block_error(&block)),
        GateOutcome::Warn { findings, skipped } => Ok(Some(ScanNotice::Warn { cwd: cwd.display().to_string(), findings, skipped })),
        GateOutcome::Incomplete(reason) => Ok(Some(ScanNotice::Incomplete { cwd: cwd.display().to_string(), reason })),
    }
}

/// The refusal text: a sentence for logs and plain callers, then the
/// machine-readable block for the dialog.
pub fn block_error(block: &ScanBlock) -> String {
    format!("{BLOCK_PREFIX}{}", serde_json::to_string(block).unwrap_or_default())
}

/// The block behind a refusal, if the error is one.
pub fn parse_block_error(err: &str) -> Option<ScanBlock> {
    serde_json::from_str(err.strip_prefix(BLOCK_PREFIX)?).ok()
}

/// A one-line description of a refusal for places that show plain text.
pub fn describe_block(block: &ScanBlock) -> String {
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
pub fn acknowledge(project_root: &Path, findings: &[FindingDto], who: &str, note: &str) -> Result<usize, String> {
    let work = project_root.join(WORK_DIR);
    let mut baseline = Baseline::load(&work).map_err(|e| e.to_string())?;
    let upstream: Vec<Finding> = findings.iter().map(FindingDto::to_finding).collect();
    let added = baseline.acknowledge(&upstream, who, note, &crate::sources::iso_utc(term_core::actor::epoch_ms()));
    baseline.save(&work).map_err(|e| e.to_string())?;
    log::info!(
        "[secret-scan] {who} acknowledged {} finding(s) in {} ({added} new): {}",
        findings.len(),
        project_root.display(),
        findings.iter().map(|f| format!("{}:{}", f.file, f.rule)).collect::<Vec<_>>().join(", ")
    );
    Ok(added)
}

/// Raise a notice for a launch that went ahead.
pub fn emit_notice(app: Option<&AppHandle>, term_id: u32, notice: &ScanNotice) {
    match notice {
        ScanNotice::Warn { cwd, findings, .. } => log::warn!(
            "[secret-scan] launched in {cwd} with {} finding(s): {}",
            findings.len(),
            findings.iter().map(|f| format!("{}:{}", f.file, f.rule)).collect::<Vec<_>>().join(", ")
        ),
        ScanNotice::Incomplete { cwd, reason } => log::warn!("[secret-scan] launched in {cwd} unscanned: {reason}"),
    }
    if let Some(app) = app {
        let _ = app.emit(EVENT, serde_json::json!({ "term_id": term_id, "notice": notice }));
    }
}

/// What the Security tab shows: whether a scanner was found and where.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScannerInfo {
    pub found: bool,
    pub program: Option<String>,
    /// The switch as it stands app-wide with the automatic default applied.
    pub effective_enabled: bool,
}

pub fn scanner_info(settings: &Settings) -> ScannerInfo {
    let scanner = detect_scanner(settings, Path::new("."));
    ScannerInfo {
        found: scanner.is_some(),
        program: scanner.as_ref().map(|s| s.program.display().to_string()),
        effective_enabled: effective_enabled(settings.secret_scan_before_agents, None, scanner.is_some()),
    }
}

// ---- Tauri commands ----------------------------------------------------------------

#[tauri::command]
pub fn secret_scan_info(settings: tauri::State<'_, crate::settings::SettingsState>) -> ScannerInfo {
    scanner_info(&settings.snapshot())
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
    let (_, root) = projects.project_root(project_id).ok_or_else(|| format!("no such project: {project_id}"))?;
    acknowledge(&root, &findings, "user", note.as_deref().unwrap_or("acknowledged from the launch dialog"))
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
            Self { answer: RefCell::new(findings), fail: None }
        }
    }

    impl SecretScanner for Fake {
        fn scan_files(&self, _root: &Path, _files: &[PathBuf]) -> Result<Vec<Finding>, SafetyError> {
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
            location: Location::Source { file: PathBuf::from(file), line: Some(line), commit: None },
            count: 1,
            severity: Severity::Bad,
        }
    }

    fn no_git() -> ListLimits {
        ListLimits { use_git: false, ..ListLimits::default() }
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
        assert!(effective_enabled(Some(false), Some(true), false), "the project wins");
        assert!(!effective_enabled(Some(true), Some(false), true));
    }

    #[test]
    fn block_agents_refuses_with_file_and_rule_and_never_a_value() {
        let (_dir, root) = tree();
        let fake = Fake::with(vec![finding("sub/.env", "generic-api-key", 1)]);
        let outcome = evaluate(&fake, SecretScanPolicy::BlockAgents, &root, &root, &no_git());
        let GateOutcome::Blocked(block) = outcome else { panic!("{outcome:?}") };
        assert_eq!(block.findings, vec![FindingDto { file: "sub/.env".into(), rule: "generic-api-key".into(), line: Some(1) }]);
        let err = block_error(&block);
        assert!(err.starts_with(BLOCK_PREFIX));
        assert!(!err.contains("value"), "the refusal never carries matched text: {err}");
        assert_eq!(parse_block_error(&err).unwrap(), block);
        assert!(parse_block_error("busy guard: refused").is_none());
        let text = describe_block(&block);
        assert!(text.contains("sub/.env:1 (generic-api-key)") && text.contains("acknowledge"), "{text}");
    }

    #[test]
    fn warn_lets_the_launch_through_with_the_findings() {
        let (_dir, root) = tree();
        let fake = Fake::with(vec![finding("sub/.env", "r", 1), finding("readme", "s", 3)]);
        match evaluate(&fake, SecretScanPolicy::Warn, &root, &root, &no_git()) {
            GateOutcome::Warn { findings, .. } => assert_eq!(findings.len(), 2),
            other => panic!("{other:?}"),
        }
        assert_eq!(evaluate(&Fake::with(vec![]), SecretScanPolicy::BlockAgents, &root, &root, &no_git()), GateOutcome::Clean { skipped: 0 });
    }

    #[test]
    fn acknowledging_writes_the_baseline_and_the_next_scan_is_clean() {
        let (_dir, root) = tree();
        let fake = Fake::with(vec![finding("sub/.env", "generic-api-key", 1)]);
        let GateOutcome::Blocked(block) = evaluate(&fake, SecretScanPolicy::BlockAgents, &root, &root, &no_git()) else { panic!() };
        assert_eq!(acknowledge(&root, &block.findings, "user", "test fixture").unwrap(), 1);
        let baseline = std::fs::read_to_string(root.join(WORK_DIR).join("secrets-baseline.json")).unwrap();
        assert!(baseline.contains("generic-api-key") && !baseline.contains("KEY=value"), "{baseline}");
        let notes = std::fs::read_to_string(root.join(WORK_DIR).join("secrets-baseline.notes.json")).unwrap();
        assert!(notes.contains("test fixture"));
        assert_eq!(evaluate(&fake, SecretScanPolicy::BlockAgents, &root, &root, &no_git()), GateOutcome::Clean { skipped: 0 });
        // Acknowledging again is idempotent (nothing new).
        assert_eq!(acknowledge(&root, &block.findings, "user", "again").unwrap(), 0);
        // A finding on another line is not covered.
        let moved = Fake::with(vec![finding("sub/.env", "generic-api-key", 2)]);
        assert!(matches!(evaluate(&moved, SecretScanPolicy::BlockAgents, &root, &root, &no_git()), GateOutcome::Blocked(_)));
    }

    #[test]
    fn a_timeout_or_scanner_error_fails_open_to_an_incomplete_notice() {
        let (_dir, root) = tree();
        let hung = Fake { answer: RefCell::new(vec![]), fail: Some(|| SafetyError::Timeout { what: "gitleaks".into(), seconds: 60 }) };
        match evaluate(&hung, SecretScanPolicy::BlockAgents, &root, &root, &no_git()) {
            GateOutcome::Incomplete(reason) => assert!(reason.contains("did not finish within 60 s"), "{reason}"),
            other => panic!("{other:?}"),
        }
        let broken = Fake { answer: RefCell::new(vec![]), fail: Some(|| SafetyError::InvalidTarget("no such scanner".into())) };
        assert!(matches!(evaluate(&broken, SecretScanPolicy::BlockAgents, &root, &root, &no_git()), GateOutcome::Incomplete(_)));
    }

    #[test]
    fn decide_skips_when_off_and_reports_a_missing_scanner_when_forced_on() {
        let (_dir, root) = tree();
        let mut settings = Settings::default();
        settings.secret_scan_before_agents = Some(false);
        assert_eq!(decide(&settings, None, &root, &root), GateOutcome::Skipped);
        // Forced on with a program that does not exist: the setting names a
        // path, nothing is found there, and the gate fails open saying so.
        settings.secret_scan_before_agents = Some(true);
        settings.secret_scan_program = root.join("no-such-gitleaks").display().to_string();
        match decide(&settings, None, &root, &root) {
            GateOutcome::Incomplete(reason) => assert!(reason.contains("not found"), "{reason}"),
            other => panic!("{other:?}"),
        }
        // A project override turns it off whatever the app says.
        assert_eq!(decide(&settings, Some(false), &root, &root), GateOutcome::Skipped);
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
                .args(["-c", "user.name=t", "-c", "user.email=t@example.test", "-c", "commit.gpgsign=false"])
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
        let outcome = evaluate(&scanner, SecretScanPolicy::BlockAgents, &root, &root, &ListLimits::default());
        let GateOutcome::Blocked(block) = outcome else { panic!("{outcome:?}") };
        assert_eq!(block.findings.len(), 1, "{block:?}");
        assert_eq!(block.findings[0].file, ".env");
        assert!(!block_error(&block).contains(&token));
        acknowledge(&root, &block.findings, "user", "test").unwrap();
        assert!(matches!(evaluate(&scanner, SecretScanPolicy::BlockAgents, &root, &root, &ListLimits::default()), GateOutcome::Clean { .. }));
        assert!(!std::fs::read_to_string(root.join(WORK_DIR).join("secrets-baseline.json")).unwrap().contains(&token));
    }
}
