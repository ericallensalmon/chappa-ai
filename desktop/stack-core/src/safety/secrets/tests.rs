//! Runner tests: targets and policy over an in-process fake, scanners over
//! stub scripts, and the real gitleaks when it is installed.

use super::test_support::{git, git_available, planted_token, write};
use super::*;
use crate::safety::Severity;
use std::cell::RefCell;
use std::time::Duration;

fn src(rule: &str, file: &str, line: u32) -> Finding {
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

fn file_of(f: &Finding) -> String {
    match &f.location {
        Location::Source { file, .. } => file.to_string_lossy().replace('\\', "/"),
        Location::Data { .. } => panic!("data finding"),
    }
}

fn no_git() -> ListLimits {
    ListLimits {
        use_git: false,
        ..ListLimits::default()
    }
}

/// Records what it was asked to scan and answers with canned findings.
#[derive(Default)]
struct Fake {
    answer: Vec<Finding>,
    files_calls: RefCell<Vec<(PathBuf, Vec<String>)>>,
    commit_calls: RefCell<Vec<(PathBuf, String)>>,
}

impl SecretScanner for Fake {
    fn scan_files(&self, root: &Path, files: &[PathBuf]) -> Result<Vec<Finding>, SafetyError> {
        self.files_calls.borrow_mut().push((
            root.to_path_buf(),
            files
                .iter()
                .map(|f| f.to_string_lossy().replace('\\', "/"))
                .collect(),
        ));
        Ok(self.answer.clone())
    }
    fn scan_commits(&self, repo: &Path, range: &str) -> Result<Vec<Finding>, SafetyError> {
        self.commit_calls
            .borrow_mut()
            .push((repo.to_path_buf(), range.to_string()));
        Ok(self.answer.clone())
    }
}

// ------------------------------------------------------------ policy

#[test]
fn policy_table() {
    let f = vec![src("r", "a", 1)];
    let table = [
        (Policy::Warn, "warn", false, false),
        (Policy::BlockAgents, "block-agents", true, false),
        (Policy::BlockBuild, "block-build", true, true),
        // data-only policies read as the default
        (Policy::Stop, "block-agents", true, false),
        (Policy::BlockUp, "block-agents", true, false),
    ];
    for (policy, want, agents, build) in table {
        let v = Verdict::from_findings(policy, f.clone());
        let json = serde_json::to_value(&v).unwrap();
        assert_eq!(json["verdict"], want, "{policy}");
        assert_eq!(v.blocks_agents(), agents, "{policy}");
        assert_eq!(v.blocks_build(), build, "{policy}");
        assert_eq!(v.findings(), &f[..]);
        assert!(!v.is_clean());

        let clean = Verdict::from_findings(policy, Vec::new());
        assert_eq!(clean, Verdict::Clean, "{policy}");
        assert!(!clean.blocks_agents() && !clean.blocks_build());
        assert!(clean.findings().is_empty());
    }
    assert_eq!(policy_for(None), Policy::BlockAgents);
    let def = crate::definition::SecretsDef {
        on_finding: Policy::BlockBuild,
    };
    assert_eq!(policy_for(Some(&def)), Policy::BlockBuild);
}

// ------------------------------------------------------------ targets over the fake

#[test]
fn working_tree_scans_the_list_and_keeps_findings_inside_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, ".env", "A=1\n");
    write(root, "src/a.rs", "x\n");
    write(root, "node_modules/p/i.js", "x\n");
    write(root, "big.bin", &"x".repeat(20));
    let fake = Fake {
        answer: vec![
            src("r", ".env", 1),
            src("r", "src/a.rs", 1),
            // a scanner that read past the list cannot widen the scope
            src("r", "node_modules/p/i.js", 1),
            src("r", "outside.txt", 1),
        ],
        ..Fake::default()
    };
    let limits = ListLimits {
        max_file_bytes: 10,
        ..no_git()
    };
    let mut baseline = Baseline::default();
    baseline.acknowledge(
        &[src("r", "src/a.rs", 1)],
        "me",
        "fixture",
        "2026-01-01T00:00:00Z",
    );
    let target = ScanTarget::WorkingTree {
        root: root.to_path_buf(),
        excludes: vec![],
    };
    let report = scan(&fake, &target, &limits, &baseline).unwrap();
    assert_eq!(
        fake.files_calls.borrow()[0].1,
        [".env".to_string(), "src/a.rs".to_string()]
    );
    assert_eq!(report.findings, [src("r", ".env", 1)]);
    assert_eq!(report.baselined, 1);
    assert_eq!(report.scanned_files, 2);
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(report.skipped[0].path, "big.bin");
}

#[test]
fn empty_list_never_calls_the_scanner() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "node_modules/a.js", "x\n");
    let fake = Fake::default();
    let report = scan(
        &fake,
        &ScanTarget::WorkingTree {
            root: dir.path().to_path_buf(),
            excludes: vec![],
        },
        &no_git(),
        &Baseline::default(),
    )
    .unwrap();
    assert!(fake.files_calls.borrow().is_empty());
    assert_eq!(report, ScanReport::default());
}

#[test]
fn build_context_honours_dockerignore_only_when_asked() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        ".dockerignore",
        "*.env\nsecrets/\n!secrets/public.txt\n",
    );
    write(root, "prod.env", "x\n");
    write(root, "sub/dev.env", "x\n");
    write(root, "secrets/key.pem", "x\n");
    write(root, "secrets/public.txt", "x\n");
    write(root, "Dockerfile", "FROM scratch\n");
    let listed = |dockerignore| {
        list_target(
            &ScanTarget::BuildContext {
                root: root.to_path_buf(),
                dockerignore,
            },
            &no_git(),
        )
        .unwrap()
        .files
        .into_iter()
        .map(|f| f.path)
        .collect::<Vec<_>>()
    };
    assert_eq!(
        listed(true),
        [
            ".dockerignore",
            "Dockerfile",
            "secrets/public.txt",
            "sub/dev.env"
        ]
    );
    assert_eq!(listed(false).len(), 6);

    let bare = tempfile::tempdir().unwrap();
    write(bare.path(), "a.txt", "x\n");
    let files = list_target(
        &ScanTarget::BuildContext {
            root: bare.path().to_path_buf(),
            dockerignore: true,
        },
        &no_git(),
    )
    .unwrap();
    assert_eq!(files.files.len(), 1, "no .dockerignore excludes nothing");
}

#[test]
fn files_target_passes_only_the_surviving_paths() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("r");
    write(&root, "a.txt", "x\n");
    write(&root, "b.log", "x\n");
    write(&root, "target/c.txt", "x\n");
    let fake = Fake {
        answer: vec![src("r", "a.txt", 2)],
        ..Fake::default()
    };
    let target = ScanTarget::Files {
        root: root.clone(),
        paths: vec![
            root.join("a.txt"),
            PathBuf::from("b.log"),
            PathBuf::from("target/c.txt"),
            dir.path().join("elsewhere.txt"),
            PathBuf::from("gone.txt"),
        ],
        excludes: vec!["*.log".into()],
    };
    let report = scan(&fake, &target, &no_git(), &Baseline::default()).unwrap();
    assert_eq!(fake.files_calls.borrow()[0].1, ["a.txt".to_string()]);
    assert_eq!(report.findings, [src("r", "a.txt", 2)]);
    assert_eq!(report.outside_root, 1);
}

#[test]
fn env_files_are_scanned_from_their_directory_and_named_as_given() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "conf/app.env", "A=1\n");
    write(dir.path(), "huge.env", &"x".repeat(30));
    let given = dir.path().join("conf/app.env");
    let fake = Fake {
        answer: vec![src("r", "app.env", 1), src("r", "other.env", 1)],
        ..Fake::default()
    };
    let limits = ListLimits {
        max_file_bytes: 20,
        ..no_git()
    };
    let target = ScanTarget::EnvFiles {
        paths: vec![
            given.clone(),
            dir.path().join("huge.env"),
            dir.path().join("missing.env"),
        ],
    };
    let report = scan(&fake, &target, &limits, &Baseline::default()).unwrap();
    let calls = fake.files_calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, dir.path().join("conf"));
    assert_eq!(calls[0].1, ["app.env".to_string()]);
    assert_eq!(report.findings.len(), 1);
    assert_eq!(
        file_of(&report.findings[0]),
        given.to_string_lossy().replace('\\', "/")
    );
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(report.scanned_files, 1);
}

#[test]
fn commits_ahead_checks_the_base_before_scanning() {
    let fake = Fake::default();
    let dir = tempfile::tempdir().unwrap();
    for bad in [
        "",
        "-x",
        "--output=/tmp/x",
        "main HEAD",
        "a..b",
        "main\n",
        "'m'",
    ] {
        let err = scan(
            &fake,
            &ScanTarget::CommitsAhead {
                root: dir.path().to_path_buf(),
                base: bad.into(),
            },
            &ListLimits::default(),
            &Baseline::default(),
        )
        .unwrap_err();
        assert!(
            matches!(err, SafetyError::InvalidTarget(_)),
            "{bad:?}: {err}"
        );
    }
    assert!(fake.commit_calls.borrow().is_empty());

    if !git_available() {
        eprintln!("git not on PATH; skipping the repository half");
        return;
    }
    let repo = dir.path().join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    write(&repo, "a.txt", "a\n");
    git(&repo, &["add", "a.txt"]);
    git(&repo, &["commit", "-q", "-m", "base"]);
    let unknown = scan(
        &fake,
        &ScanTarget::CommitsAhead {
            root: repo.clone(),
            base: "no-such-branch".into(),
        },
        &ListLimits::default(),
        &Baseline::default(),
    )
    .unwrap_err();
    assert!(
        matches!(unknown, SafetyError::InvalidTarget(_)),
        "{unknown}"
    );
    scan(
        &fake,
        &ScanTarget::CommitsAhead {
            root: repo.clone(),
            base: "main".into(),
        },
        &ListLimits::default(),
        &Baseline::default(),
    )
    .unwrap();
    assert_eq!(
        fake.commit_calls.borrow().as_slice(),
        [(repo, "main..HEAD".to_string())]
    );
}

#[test]
fn targets_round_trip_as_tagged_json() {
    let t = ScanTarget::Files {
        root: PathBuf::from("r"),
        paths: vec![PathBuf::from("a")],
        excludes: vec![],
    };
    let json = serde_json::to_value(&t).unwrap();
    assert_eq!(json["kind"], "files");
    assert_eq!(serde_json::from_value::<ScanTarget>(json).unwrap(), t);
    let w: ScanTarget = serde_json::from_str(r#"{"kind":"working-tree","root":"x"}"#).unwrap();
    assert_eq!(
        w,
        ScanTarget::WorkingTree {
            root: PathBuf::from("x"),
            excludes: vec![]
        }
    );
}

// ------------------------------------------------------------ stub scripts

/// A script `name` in `dir` that runs `body`. Windows gets a `.cmd`, other
/// systems an executable `sh` script.
fn script(dir: &Path, name: &str, windows: &str, unix: &str) -> PathBuf {
    if cfg!(windows) {
        let p = dir.join(format!("{name}.cmd"));
        fs::write(
            &p,
            format!("@echo off\r\n{}\r\n", windows.replace('\n', "\r\n")),
        )
        .unwrap();
        p
    } else {
        let p = dir.join(name);
        fs::write(&p, format!("#!/bin/sh\n{unix}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }
}

/// A stand-in for gitleaks: records its argv and working directory, copies
/// `canned.json` to wherever `--report-path` points, exits with `code`.
/// (`shift` moves `%0` too in a batch file, so its directory is saved first.)
fn stub_gitleaks(dir: &Path, code: i32) -> PathBuf {
    script(
        dir,
        "gitleaks",
        &format!(
            "set \"D=%~dp0\"\n\
             echo %* > \"%D%args.txt\"\n\
             cd > \"%D%cwd.txt\"\n\
             :loop\n\
             if \"%~1\"==\"\" goto done\n\
             if \"%~1\"==\"--report-path\" copy /y \"%D%canned.json\" \"%~2\" >nul\n\
             shift\n\
             goto loop\n\
             :done\n\
             exit /b {code}"
        ),
        &format!(
            "d=$(dirname \"$0\")\n\
             echo \"$@\" > \"$d/args.txt\"\n\
             pwd > \"$d/cwd.txt\"\n\
             while [ $# -gt 0 ]; do\n\
             if [ \"$1\" = \"--report-path\" ]; then cp \"$d/canned.json\" \"$2\"; fi\n\
             shift\n\
             done\n\
             exit {code}"
        ),
    )
}

/// An unredacted gitleaks-shaped report holding `value` in every field a
/// value can sit in.
fn canned_gitleaks(value: &str, file: &str) -> String {
    format!(
        r#"[{{"RuleID":"github-pat","Description":"d","StartLine":1,"EndLine":1,
            "StartColumn":7,"EndColumn":46,"Match":"TOKEN={value}","Secret":"{value}",
            "Line":"TOKEN={value}","File":"{file}","Commit":"","Entropy":5.1,
            "Author":"","Email":"","Date":"","Message":"{value}","Tags":[],
            "Fingerprint":"{file}:github-pat:1:{value}"}}]"#
    )
}

#[test]
fn stub_gitleaks_is_found_on_a_temp_path_and_its_report_is_stripped() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let stub = stub_gitleaks(&bin, gitleaks::LEAK_EXIT_CODE);
    let v = planted_token();
    fs::write(bin.join("canned.json"), canned_gitleaks(&v, ".env")).unwrap();

    let found = find_on_path(
        "gitleaks",
        Some(bin.as_os_str()),
        Some(".exe;.cmd".as_ref()),
    );
    assert_eq!(found.as_deref(), Some(stub.as_path()));
    assert_eq!(
        find_on_path("gitleaks", Some(dir.path().as_os_str()), None),
        None
    );
    assert_eq!(find_on_path("gitleaks", None, None), None);

    let root = dir.path().join("project");
    write(&root, ".env", "TOKEN=placeholder\n");
    write(&root, "src/a.rs", "x\n");
    let work = dir.path().join("work");
    let scanner = GitleaksScanner::new(found.unwrap(), &work);
    let report = scan(
        &scanner,
        &ScanTarget::WorkingTree {
            root: root.clone(),
            excludes: vec![],
        },
        &no_git(),
        &Baseline::default(),
    )
    .unwrap();
    assert_eq!(report.findings, [src("github-pat", ".env", 1)]);
    let shown = format!("{report:?} {}", serde_json::to_string(&report).unwrap());
    assert!(!shown.contains(&v), "{shown}");

    let args = fs::read_to_string(bin.join("args.txt")).unwrap();
    for want in [
        "dir .",
        "--redact",
        "--report-format json",
        "--report-path",
        "--exit-code 42",
        "--gitleaks-ignore-path",
        "--log-level error",
    ] {
        assert!(args.contains(want), "{want} missing from {args}");
    }
    assert!(!args.contains("--baseline-path"), "{args}");
    let cwd = fs::read_to_string(bin.join("cwd.txt")).unwrap();
    assert!(cwd.contains("stage"), "runs from the staging dir: {cwd}");
    let leftovers: Vec<_> = fs::read_dir(&work).unwrap().collect();
    assert!(leftovers.is_empty(), "staging and report removed");
}

#[test]
fn stub_gitleaks_failures_are_errors_not_clean_scans() {
    let dir = tempfile::tempdir().unwrap();
    let stub = stub_gitleaks(dir.path(), 1);
    fs::write(dir.path().join("canned.json"), "[]").unwrap();
    let root = dir.path().join("p");
    write(&root, "a.txt", "x\n");
    let scanner = GitleaksScanner::new(stub, dir.path().join("work"));
    let err = scanner
        .scan_files(&root, &[PathBuf::from("a.txt")])
        .unwrap_err();
    assert!(err.to_string().contains("exit Some(1)"), "{err}");

    // exit 0 with an error on stderr: gitleaks does this for a bad range
    let noisy = script(
        dir.path(),
        "noisy",
        "echo fatal: unknown revision 1>&2\nexit /b 0",
        "echo 'fatal: unknown revision' >&2\nexit 0",
    );
    let scanner = GitleaksScanner::new(noisy, dir.path().join("work"));
    let err = scanner.scan_commits(&root, "main..HEAD").unwrap_err();
    assert!(err.to_string().contains("unknown revision"), "{err}");
}

#[test]
fn a_hung_scanner_times_out() {
    let dir = tempfile::tempdir().unwrap();
    let slow = script(dir.path(), "slow", "ping -n 6 127.0.0.1 >nul", "sleep 5");
    let mut hook = HookScanner::new(vec![slow.to_string_lossy().into_owned()], dir.path());
    hook.timeout = Duration::from_millis(300);
    let started = std::time::Instant::now();
    let err = hook.scan_commits(dir.path(), "a..b").unwrap_err();
    assert!(matches!(err, SafetyError::Timeout { .. }), "{err}");
    assert!(started.elapsed() < Duration::from_secs(4));
}

#[test]
fn hook_prints_gitleaks_json_and_sees_its_placeholders() {
    let dir = tempfile::tempdir().unwrap();
    let v = planted_token();
    fs::write(
        dir.path().join("canned.json"),
        canned_gitleaks(&v, "src/a.rs"),
    )
    .unwrap();
    let hook_path = script(
        dir.path(),
        "hook",
        "type \"%~1\" > \"%~dp0list.txt\"\n\
         echo %CHAPPA_SCAN_ROOT%> \"%~dp0root.txt\"\n\
         echo leaked %CHAPPA_SCAN_RANGE% 1>&2\n\
         type \"%~dp0canned.json\"\n\
         exit /b 1",
        "d=$(dirname \"$0\")\n\
         cat \"$1\" > \"$d/list.txt\"\n\
         echo \"$CHAPPA_SCAN_ROOT\" > \"$d/root.txt\"\n\
         cat \"$d/canned.json\"\n\
         exit 1",
    );
    let root = dir.path().join("p");
    write(&root, "src/a.rs", "x\n");
    write(&root, ".env", "x\n");
    let hook = HookScanner::new(
        vec![hook_path.to_string_lossy().into_owned(), "{list}".into()],
        dir.path().join("work"),
    );
    let report = scan(
        &hook,
        &ScanTarget::WorkingTree {
            root: root.clone(),
            excludes: vec![],
        },
        &no_git(),
        &Baseline::default(),
    )
    .unwrap();
    assert_eq!(report.findings, [src("github-pat", "src/a.rs", 1)]);
    assert!(!format!("{report:?}").contains(&v));
    let list = fs::read_to_string(dir.path().join("list.txt")).unwrap();
    assert_eq!(list.lines().collect::<Vec<_>>(), [".env", "src/a.rs"]);
    let seen_root = fs::read_to_string(dir.path().join("root.txt")).unwrap();
    assert_eq!(seen_root.trim(), root.to_string_lossy());
}

#[test]
fn hook_prints_sarif() {
    let dir = tempfile::tempdir().unwrap();
    let v = planted_token();
    let root = dir.path().join("p");
    write(&root, "cfg/app.json", "x\n");
    let abs_uri = format!(
        "file:///{}",
        root.join("cfg/app.json")
            .to_string_lossy()
            .replace('\\', "/")
            .trim_start_matches('/')
    );
    fs::write(
        dir.path().join("canned.json"),
        format!(
            r#"{{"version":"2.1.0","runs":[{{"results":[{{"ruleId":"private-key",
                "message":{{"text":"{v}"}},
                "locations":[{{"physicalLocation":{{"artifactLocation":{{"uri":"{abs_uri}"}},
                "region":{{"startLine":12,"snippet":{{"text":"{v}"}}}}}}}}]}}]}}]}}"#
        ),
    )
    .unwrap();
    let hook_path = script(
        dir.path(),
        "sarif",
        "type \"%~dp0canned.json\"",
        "cat \"$(dirname \"$0\")/canned.json\"",
    );
    let hook = HookScanner::new(
        vec![hook_path.to_string_lossy().into_owned()],
        dir.path().join("work"),
    );
    let report = scan(
        &hook,
        &ScanTarget::WorkingTree {
            root: root.clone(),
            excludes: vec![],
        },
        &no_git(),
        &Baseline::default(),
    )
    .unwrap();
    assert_eq!(report.findings, [src("private-key", "cfg/app.json", 12)]);
    assert!(!format!("{report:?}").contains(&v));
}

#[test]
fn hook_failures_do_not_quote_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let v = planted_token();
    let bad = script(
        dir.path(),
        "bad",
        &format!("echo {v} 1>&2\nexit /b 3"),
        &format!("echo {v} >&2\nexit 3"),
    );
    let hook = HookScanner::new(vec![bad.to_string_lossy().into_owned()], dir.path());
    let err = hook.scan_commits(dir.path(), "a..b").unwrap_err();
    let shown = format!("{err} {err:?}");
    assert!(shown.contains("Some(3)"), "{shown}");
    assert!(!shown.contains(&v), "{shown}");

    let junk = script(
        dir.path(),
        "junk",
        &format!("echo {v}"),
        &format!("echo {v}"),
    );
    let hook = HookScanner::new(vec![junk.to_string_lossy().into_owned()], dir.path());
    let err = hook.scan_commits(dir.path(), "a..b").unwrap_err();
    assert!(!format!("{err} {err:?}").contains(&v), "{err}");

    let empty = HookScanner::new(vec![], dir.path());
    assert!(empty.scan_commits(dir.path(), "a..b").is_err());
}

// ------------------------------------------------------------ the real gitleaks

fn real_gitleaks(work: &Path) -> Option<GitleaksScanner> {
    if !git_available() {
        eprintln!("git not on PATH; skipping");
        return None;
    }
    match GitleaksScanner::detect(None, work) {
        Some(g) => Some(g),
        None => {
            eprintln!("gitleaks not on PATH; skipping");
            None
        }
    }
}

/// A repository with a clean tracked file, the planted token in an ignored
/// `.env`, and the same token where listing must skip it.
fn leaky_repo(dir: &Path) -> PathBuf {
    let repo = dir.join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    let token = planted_token();
    write(&repo, ".gitignore", ".env\n");
    write(&repo, "src/main.rs", "fn main() {}\n");
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "base"]);
    write(&repo, ".env", &format!("TOKEN={token}\n"));
    write(
        &repo,
        "node_modules/pkg/index.js",
        &format!("t='{token}'\n"),
    );
    write(&repo, "target/debug/build.txt", &format!("t={token}\n"));
    repo
}

#[test]
fn real_gitleaks_flags_an_ignored_env_file_and_the_baseline_silences_it() {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    let Some(scanner) = real_gitleaks(&work) else {
        return;
    };
    eprintln!("gitleaks {}", scanner.version().unwrap());
    let repo = leaky_repo(dir.path());
    let target = ScanTarget::WorkingTree {
        root: repo.clone(),
        excludes: vec![],
    };
    let limits = ListLimits::default();
    let report = scan(&scanner, &target, &limits, &Baseline::default()).unwrap();
    assert_eq!(report.scanned_files, 3, "{report:?}"); // .env .gitignore src/main.rs
    assert_eq!(report.findings, [src("github-pat", ".env", 1)]);
    let token = planted_token();
    let shown = format!("{report:?} {}", serde_json::to_string(&report).unwrap());
    assert!(!shown.contains(&token), "{shown}");
    let verdict = Verdict::from_findings(Policy::BlockAgents, report.findings.clone());
    assert!(verdict.blocks_agents() && !verdict.blocks_build());

    // acknowledge into the project's .work, as the gate would
    let dot_work = repo.join(".work");
    let mut baseline = Baseline::load(&dot_work).unwrap();
    baseline.acknowledge(
        &report.findings,
        "me",
        "test fixture",
        "2026-01-01T00:00:00Z",
    );
    baseline.save(&dot_work).unwrap();
    let on_disk = fs::read_to_string(Baseline::path(&dot_work)).unwrap();
    assert!(!on_disk.contains(&token));
    let again = scan(
        &scanner,
        &target,
        &limits,
        &Baseline::load(&dot_work).unwrap(),
    )
    .unwrap();
    assert!(again.findings.is_empty(), "{again:?}");
    assert_eq!(again.baselined, 1);
    assert_eq!(
        Verdict::from_findings(Policy::BlockAgents, again.findings),
        Verdict::Clean
    );

    // the same through one-file-at-a-time scanning (no staging links)
    let per_file = GitleaksScanner {
        stage_with_links: false,
        ..scanner.clone()
    };
    let report = scan(&per_file, &target, &limits, &Baseline::default()).unwrap();
    assert_eq!(report.findings, [src("github-pat", ".env", 1)]);

    // and through the incremental rescan
    let changed = ScanTarget::Files {
        root: repo.clone(),
        paths: vec![repo.join(".env"), repo.join("node_modules/pkg/index.js")],
        excludes: vec![],
    };
    let report = scan(&scanner, &changed, &limits, &Baseline::default()).unwrap();
    assert_eq!(report.scanned_files, 1);
    assert_eq!(report.findings, [src("github-pat", ".env", 1)]);
    assert!(
        fs::read_dir(&work).unwrap().next().is_none(),
        "work dir left clean"
    );
}

#[test]
fn real_gitleaks_finds_a_secret_committed_then_deleted_on_a_branch() {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    let Some(scanner) = real_gitleaks(&work) else {
        return;
    };
    let repo = dir.path().join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    write(&repo, "a.txt", "a\n");
    git(&repo, &["add", "a.txt"]);
    git(&repo, &["commit", "-q", "-m", "base"]);
    git(&repo, &["checkout", "-q", "-b", "item"]);
    write(&repo, "cfg.txt", &format!("token: {}\n", planted_token()));
    git(&repo, &["add", "cfg.txt"]);
    git(&repo, &["commit", "-q", "-m", "add config"]);
    let added = git(&repo, &["rev-parse", "HEAD"]).trim().to_string();
    git(&repo, &["rm", "-q", "cfg.txt"]);
    git(&repo, &["commit", "-q", "-m", "remove config"]);

    let tree = scan(
        &scanner,
        &ScanTarget::WorkingTree {
            root: repo.clone(),
            excludes: vec![],
        },
        &ListLimits::default(),
        &Baseline::default(),
    )
    .unwrap();
    assert!(tree.findings.is_empty(), "the tree is clean now: {tree:?}");

    let ahead = ScanTarget::CommitsAhead {
        root: repo.clone(),
        base: "main".into(),
    };
    let report = scan(
        &scanner,
        &ahead,
        &ListLimits::default(),
        &Baseline::default(),
    )
    .unwrap();
    let rules: Vec<&str> = report.findings.iter().map(|f| f.rule.as_str()).collect();
    assert!(rules.contains(&"github-pat"), "{report:?}");
    for f in &report.findings {
        assert_eq!(
            f.location,
            Location::Source {
                file: PathBuf::from("cfg.txt"),
                line: Some(1),
                commit: Some(added.clone()),
            }
        );
        assert!(fingerprint(f).unwrap().starts_with(&added));
    }
    assert!(!format!("{report:?}").contains(&planted_token()));

    // nothing ahead of the branch itself
    let none = scan(
        &scanner,
        &ScanTarget::CommitsAhead {
            root: repo.clone(),
            base: "item".into(),
        },
        &ListLimits::default(),
        &Baseline::default(),
    )
    .unwrap();
    assert!(none.findings.is_empty());
}

#[test]
fn real_gitleaks_over_env_files_and_build_contexts() {
    let dir = tempfile::tempdir().unwrap();
    let work = dir.path().join("work");
    let Some(scanner) = real_gitleaks(&work) else {
        return;
    };
    let token = planted_token();
    write(dir.path(), "outside/app.env", &format!("API={token}\n"));
    let env = dir.path().join("outside/app.env");
    let report = scan(
        &scanner,
        &ScanTarget::EnvFiles {
            paths: vec![env.clone()],
        },
        &ListLimits::default(),
        &Baseline::default(),
    )
    .unwrap();
    assert_eq!(report.findings.len(), 1, "{report:?}");
    assert_eq!(
        file_of(&report.findings[0]),
        env.to_string_lossy().replace('\\', "/")
    );

    let ctx = dir.path().join("ctx");
    write(&ctx, "Dockerfile", "FROM scratch\nCOPY . /app\n");
    write(&ctx, "deploy/prod.env", &format!("API={token}\n"));
    write(&ctx, ".dockerignore", "deploy/\n");
    let run = |dockerignore| {
        scan(
            &scanner,
            &ScanTarget::BuildContext {
                root: ctx.clone(),
                dockerignore,
            },
            &no_git(),
            &Baseline::default(),
        )
        .unwrap()
    };
    assert!(run(true).findings.is_empty());
    let unignored = run(false);
    assert_eq!(unignored.findings.len(), 1, "{unignored:?}");
    assert_eq!(file_of(&unignored.findings[0]), "deploy/prod.env");
}
