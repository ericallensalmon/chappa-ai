//! Every git the workspace's crates start goes through `stack_core::git`,
//! which keeps a repository's own config from running commands. This test
//! reads the sources of every crate and fails on a git spawned any other
//! way, so a new direct spawn cannot slip in unnoticed.
//!
//! Test code is exempt (it sets repositories up with a plain git on
//! purpose): `tests/` folders, `tests.rs` files and `#[cfg(test)]` modules.

use regex::Regex;
use std::fs;
use std::path::{Path, PathBuf};

/// The one file allowed to spawn git directly.
const HELPER: &str = "stack-core/src/git.rs";

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        if p.is_dir() {
            if name != "tests" && name != "target" {
                rust_files(&p, out);
            }
        } else if name.ends_with(".rs") && name != "tests.rs" {
            out.push(p);
        }
    }
}

/// `text` without its `#[cfg(test)]` modules (inline ones, by brace
/// count) and without line comments.
fn non_test_code(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.trim() == "#[cfg(test)]" {
            let next = lines.get(i + 1).map(|l| l.trim()).unwrap_or("");
            let is_mod = next.starts_with("mod ")
                || next.starts_with("pub mod ")
                || next.starts_with("pub(crate) mod ");
            if is_mod && next.ends_with('{') {
                let mut depth = 0i32;
                let mut j = i + 1;
                while j < lines.len() {
                    depth += lines[j].matches('{').count() as i32;
                    depth -= lines[j].matches('}').count() as i32;
                    j += 1;
                    if depth <= 0 {
                        break;
                    }
                }
                i = j;
                continue;
            }
        }
        let code = match line.find("//") {
            Some(at) => &line[..at],
            None => line,
        };
        out.push_str(code);
        out.push('\n');
        i += 1;
    }
    out
}

fn direct_spawns(code: &str) -> Vec<String> {
    let patterns = [
        // `Command::new("git")`, `Command::new(git)`, `Command::new(&self.git)` …
        r#"Command::new\(\s*"git(\.exe)?"\s*\)"#,
        r#"Command::new\(\s*&?[A-Za-z_.]*\bgit[A-Za-z_]*\s*\)"#,
        // An engine invocation whose program is git.
        r#"Invocation::(new|from_argv)\(\s*"git""#,
        r#""git"\.(to_string|to_owned|into)\(\)"#,
        r#"PathBuf::from\(\s*"git"\s*\)"#,
    ];
    let mut hits = Vec::new();
    for p in patterns {
        let re = Regex::new(p).unwrap();
        for m in re.find_iter(code) {
            hits.push(m.as_str().to_string());
        }
    }
    hits
}

#[test]
fn no_crate_spawns_git_outside_the_helper() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut files = Vec::new();
    for crate_dir in fs::read_dir(workspace).unwrap().flatten() {
        let src = crate_dir.path().join("src");
        if crate_dir.path().join("Cargo.toml").is_file() && src.is_dir() {
            rust_files(&src, &mut files);
        }
    }
    assert!(files.len() > 50, "found only {} source files", files.len());
    let mut found = Vec::new();
    for f in &files {
        let rel = f
            .strip_prefix(workspace)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if rel == HELPER {
            continue;
        }
        let text = fs::read_to_string(f).unwrap();
        for hit in direct_spawns(&non_test_code(&text)) {
            found.push(format!("{rel}: {hit}"));
        }
    }
    assert!(
        found.is_empty(),
        "git spawned outside stack_core::git (use git::command or git::invocation):\n{}",
        found.join("\n")
    );
}

#[test]
fn the_check_sees_the_spawns_it_is_for() {
    for code in [
        r#"let out = Command::new("git").arg("status");"#,
        "let mut cmd = Command::new(git);",
        "Command::new(&self.git)",
        "Command::new(git_program)",
        r#"Invocation::new("git", &["status"])"#,
        r#"let argv = vec!["git".to_string()];"#,
        r#"program: PathBuf::from("git"),"#,
    ] {
        assert!(!direct_spawns(code).is_empty(), "{code}");
    }
    for code in [
        r#"Command::new(&self.program).arg("git")"#,
        r#"json!({"command": ["git", "switch", b]})"#,
        "Command::new(docker_program())",
    ] {
        assert!(direct_spawns(code).is_empty(), "{code}");
    }
    let stripped = non_test_code(
        "fn a() {}\n#[cfg(test)]\nmod tests {\n    fn b() { Command::new(\"git\"); }\n}\nfn c() {}\n",
    );
    assert!(direct_spawns(&stripped).is_empty(), "{stripped}");
    assert!(stripped.contains("fn c()"));
}
