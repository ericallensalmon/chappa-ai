//! The stack definition as part of a project's trust.
//!
//! A project trusted by the `chappa.yml` content hash would leave a stack
//! runner free to run whatever its definition says next: the runner re-reads
//! `stacks:` (or `chappa.stack.yml`) on every request and runs status and
//! plan scripts on its own. So for every `source:` command that starts the
//! stack runner (`chappa-stack … serve`), the project's trust hash also
//! covers [`stack_core::trust::definition_trust_hash`] of the folder it
//! serves, the trust prompt lists what that definition can run (exactly as
//! written, credential-shaped values flagged beside it), and the runner is
//! started with
//! `--trusted-definition <hash>` (or `none` while nothing is trusted). The
//! runner runs nothing while its definition hashes differently; when the
//! person trusts the new content, the app starts it again with the new hash.
//!
//! What is stored is the hash, keyed like every other project trust record,
//! never the command list (see [`project_trust_hash`]).

use std::path::{Path, PathBuf};

use project_model::yml::{CommandLine, ProcessDef, ProjectYml};
use stack_core::safety::secrets::config::ProjectScanConfig;
use stack_core::trust::{definition_trust, DefinitionTrust, UNTRUSTED};

use crate::scan_config::ScanConfigSource;

/// The argument the runner takes the trusted hash in.
pub const TRUSTED_DEFINITION_ARG: &str = "--trusted-definition";
/// The argument the runner takes the gitleaks config from Settings ▸
/// Security in, so its scans and the app's pre-agent scan use one set of
/// rules. A path the person chose; never one from a scanned tree.
pub const GITLEAKS_CONFIG_ARG: &str = "--gitleaks-config";
/// The arguments the runner takes the project's own gitleaks config in: the
/// file, and the hash it was trusted at (`none` while it is not). The runner
/// re-hashes the file before every scan and uses it only while it matches.
pub const PROJECT_GITLEAKS_CONFIG_ARG: &str = "--project-gitleaks-config";
pub const PROJECT_GITLEAKS_CONFIG_HASH_ARG: &str = "--project-gitleaks-config-hash";

/// The words of a command as it is written (a shell line split on spaces).
fn words(def: &ProcessDef) -> Vec<String> {
    match &def.command {
        CommandLine::Argv(w) => w.clone(),
        CommandLine::Shell(line) => line.split_whitespace().map(str::to_owned).collect(),
    }
}

/// Whether `def` starts the stack runner's server: a word naming the
/// `chappa-stack` program (a path, a `.exe`, or cargo's `-p chappa-stack`)
/// followed later by `serve`.
///
/// Both path separators count on every OS: `chappa.yml` is shared between
/// machines, and the same file must give the same trust hash wherever it
/// is opened.
pub fn is_stack_serve(def: &ProcessDef) -> bool {
    if !def.is_source() {
        return false;
    }
    let w = words(def);
    let named = w.iter().position(|word| {
        let word = word.trim_matches(['"', '\'']);
        let file = word.rsplit(['/', '\\']).next().unwrap_or(word);
        Path::new(file)
            .file_stem()
            .is_some_and(|s| s.to_string_lossy().eq_ignore_ascii_case("chappa-stack"))
    });
    named.is_some_and(|i| {
        w[i + 1..]
            .iter()
            .any(|x| x.trim_matches(['"', '\'']) == "serve")
    })
}

/// The folder whose stack definition `def` serves.
pub fn served_folder(root: &Path, def: &ProcessDef) -> PathBuf {
    let cwd = def
        .resolve_working_dir(root)
        .ok()
        .flatten()
        .unwrap_or_else(|| root.to_path_buf());
    crate::sources::launch_folder_of(&cwd, &words(def))
}

/// One stack-runner command of a project and what its definition is now.
#[derive(Debug, Clone)]
pub struct StackSource {
    pub name: String,
    pub folder: PathBuf,
    /// `Ok(None)`: the folder defines no stack. `Err`: it does not load.
    pub definition: Result<Option<DefinitionTrust>, String>,
}

impl StackSource {
    /// The hash as it goes into the trust hash and onto the command line.
    pub fn hash(&self) -> String {
        match &self.definition {
            Ok(Some(t)) => t.hash.clone(),
            Ok(None) => "no-stack".into(),
            Err(e) => format!(
                "unreadable:{}",
                project_model::hash::content_hash(e.as_bytes())
            ),
        }
    }

    /// The hash the runner may run under, when the definition loaded.
    pub fn runnable_hash(&self) -> Option<String> {
        match &self.definition {
            Ok(Some(t)) => Some(t.hash.clone()),
            _ => None,
        }
    }
}

/// The stack-runner commands of a project, in file order.
pub fn stack_sources<'a>(
    root: &Path,
    processes: impl IntoIterator<Item = (&'a String, &'a ProcessDef)>,
) -> Vec<StackSource> {
    processes
        .into_iter()
        .filter(|(_, d)| is_stack_serve(d))
        .map(|(name, d)| {
            let folder = served_folder(root, d);
            StackSource {
                name: name.clone(),
                definition: definition_trust(&folder).map_err(|e| e.to_string()),
                folder,
            }
        })
        .collect()
}

/// The project's trust hash: the `chappa.yml` content hash alone when no
/// command runs a stack (so every existing trust record stays valid), else
/// a hash over it and each stack runner's definition hash. Only this hash
/// is ever stored.
pub fn project_trust_hash(
    file_hash: &str,
    stacks: &[StackSource],
    scan_config: Option<&ScanConfigSource>,
) -> String {
    if stacks.is_empty() && scan_config.is_none() {
        return file_hash.to_owned();
    }
    let mut text = format!("chappa project trust v1\0{file_hash}");
    for s in stacks {
        text.push_str(&format!("\0{}\0{}", s.name, s.hash()));
    }
    // The project's own gitleaks config is trusted like a command: its
    // path and content hash, so an edit of the file alone asks again.
    if let Some(c) = scan_config {
        text.push_str(&format!("\0secretScan.config\0{}", c.hash_part()));
    }
    project_model::hash::content_hash(text.as_bytes())
}

/// The trust hash of what is on disk now, `None` without a readable
/// `chappa.yml`.
pub fn trust_hash_on_disk(root: &Path) -> Option<String> {
    let (yml, file_hash) = ProjectYml::load_result_with_hash(root).ok()?;
    Some(project_trust_hash(
        &file_hash,
        &stack_sources(root, &yml.processes),
        ScanConfigSource::of(root, &yml).as_ref(),
    ))
}

/// The trust prompt's entries for the stack definitions: one header per
/// runner, then what each component can run, exactly as written (see
/// [`stack_core::trust::visible`]: nothing masked, line breaks and control
/// characters inside a value shown as glyphs).
pub fn prompt_lines(stacks: &[StackSource]) -> Vec<String> {
    use stack_core::trust::visible;
    let mut out = Vec::new();
    for s in stacks {
        match &s.definition {
            Ok(Some(t)) => {
                out.push(format!(
                    "`{}` runs the stack definition in {} (every command below runs without asking again)",
                    visible(&s.name),
                    visible(&t.file.display().to_string())
                ));
                out.extend(t.commands.iter().cloned());
            }
            Ok(None) => out.push(format!(
                "`{}` serves {}, which defines no stack",
                visible(&s.name),
                visible(&s.folder.display().to_string())
            )),
            // Nothing in a definition that does not load runs, so its
            // error text may keep a credential masked.
            Err(e) => out.push(format!(
                "`{}` serves {}, whose stack definition does not load ({}); nothing in it will run",
                visible(&s.name),
                visible(&s.folder.display().to_string()),
                visible(&stack_core::credentials::mask_text(e))
            )),
        }
    }
    out
}

/// `def` with the trusted definition hash appended to its command line
/// (`none` when nothing is trusted). The hash is hex, so it needs no
/// quoting in a shell line.
pub fn with_trusted_definition(def: &ProcessDef, hash: Option<&str>) -> ProcessDef {
    let value = hash.unwrap_or(UNTRUSTED);
    let mut out = def.clone();
    out.command = match &def.command {
        CommandLine::Argv(w) => {
            let mut w = w.clone();
            w.push(TRUSTED_DEFINITION_ARG.into());
            w.push(value.into());
            CommandLine::Argv(w)
        }
        CommandLine::Shell(line) => {
            CommandLine::Shell(format!("{line} {TRUSTED_DEFINITION_ARG} {value}"))
        }
    };
    out
}

/// What a call that would change something through a stack runner gets
/// while its definition is not the trusted one: acknowledgements are
/// refused like every action, and the way out is to trust it again.
pub const UNTRUSTED_REFUSAL: &str = "The stack definition is not the one trusted in this app (it changed, or it was never trusted), so the stack is read-only: no action or acknowledgement is taken until you review the project's trust prompt and trust it again.";

/// The refusal for the stack runner `def` of the project at `root` when
/// the definition it serves does not hash to `trusted` (the hash it was
/// launched with). `None` when it does, or when the folder defines no
/// stack (nothing to protect).
pub fn refusal(root: &Path, def: &ProcessDef, trusted: Option<&str>) -> Option<String> {
    let now = match definition_trust(&served_folder(root, def)) {
        Ok(None) => return None,
        Ok(Some(t)) => Some(t.hash),
        Err(_) => None,
    };
    match (now, trusted) {
        (Some(now), Some(trusted)) if now == trusted => None,
        _ => Some(UNTRUSTED_REFUSAL.to_owned()),
    }
}

/// `def` with `flag <path>` added to its command line. A list command takes
/// the path as one word. A shell line gets it quoted, which is only done for
/// paths made of characters every shell leaves alone inside quotes;
/// anything else is refused (`what` names the path in the error) rather
/// than passed in a form some shell would expand.
fn with_path_arg(
    def: &ProcessDef,
    flag: &str,
    path: &str,
    what: &str,
) -> Result<ProcessDef, String> {
    let mut out = def.clone();
    out.command = match &def.command {
        CommandLine::Argv(w) => {
            let mut w = w.clone();
            w.push(flag.into());
            w.push(path.into());
            CommandLine::Argv(w)
        }
        CommandLine::Shell(line) => {
            let plain = path
                .chars()
                .all(|c| c.is_alphanumeric() || " /\\:._-+~,@()[]{}=#".contains(c));
            if !plain {
                return Err(format!(
                    "{what} ({path}) holds characters a shell would read; write this command as a list (`command: [...]`) or move the file"
                ));
            }
            let quote = if cfg!(windows) { '"' } else { '\'' };
            CommandLine::Shell(format!("{line} {flag} {quote}{path}{quote}"))
        }
    };
    Ok(out)
}

/// `def` with the gitleaks config from Settings ▸ Security on its command
/// line (`config` trimmed; empty = nothing added, and the runner uses the
/// built-in default the app uses too).
pub fn with_gitleaks_config(def: &ProcessDef, config: &str) -> Result<ProcessDef, String> {
    let config = config.trim();
    if config.is_empty() {
        return Ok(def.clone());
    }
    with_path_arg(
        def,
        GITLEAKS_CONFIG_ARG,
        config,
        "the gitleaks config path in Settings ▸ Security",
    )
}

/// `def` with the project's own gitleaks config on its command line: the
/// file and the hash it was trusted at (`none` while it is not trusted or
/// not valid). Nothing is added when the project names none.
pub fn with_project_scan_config(
    def: &ProcessDef,
    config: &ProjectScanConfig,
) -> Result<ProcessDef, String> {
    let Some((path, hash)) = config.args() else {
        return Ok(def.clone());
    };
    let with_path = with_path_arg(
        def,
        PROJECT_GITLEAKS_CONFIG_ARG,
        &path.display().to_string(),
        "the project's secret-scan config path",
    )?;
    // The hash is hex or `none`: no quoting needed.
    let mut out = with_path.clone();
    out.command = match &with_path.command {
        CommandLine::Argv(w) => {
            let mut w = w.clone();
            w.push(PROJECT_GITLEAKS_CONFIG_HASH_ARG.into());
            w.push(hash.into());
            CommandLine::Argv(w)
        }
        CommandLine::Shell(line) => {
            CommandLine::Shell(format!("{line} {PROJECT_GITLEAKS_CONFIG_HASH_ARG} {hash}"))
        }
    };
    Ok(out)
}

/// The stack runner `def` as it is launched: the gitleaks config from
/// Settings, the project's own config with its trusted hash, then the
/// trusted definition hash (always the last two words).
pub fn launch_def(
    def: &ProcessDef,
    trusted: Option<&str>,
    gitleaks_config: &str,
    project_scan: &ProjectScanConfig,
) -> Result<ProcessDef, String> {
    Ok(with_trusted_definition(
        &with_project_scan_config(&with_gitleaks_config(def, gitleaks_config)?, project_scan)?,
        trusted,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const STACK: &str = "stacks:\n  app:\n    components:\n      web:\n        driver: script\n        up: [node, scripts/up.js]\n        status: [node, scripts/status.js]\n";

    fn yml_with_runner(extra: &str) -> String {
        format!(
            "processes:\n  stack:\n    command: [cargo, run, -q, -p, chappa-stack, --, serve]\n    source: {{}}\n  web:\n    command: npm run dev\n{extra}"
        )
    }

    #[test]
    fn the_runner_is_recognised_in_its_usual_spellings_only_as_a_source() {
        let def = |cmd: &str, source: bool| {
            let mut d = ProcessDef {
                command: CommandLine::Shell(cmd.into()),
                ..ProcessDef::default()
            };
            if source {
                d.source = Some(Default::default());
            }
            d
        };
        assert!(is_stack_serve(&def("chappa-stack serve --slot 1", true)));
        assert!(is_stack_serve(&def(
            "C:\\tools\\chappa-stack.exe serve",
            true
        )));
        assert!(is_stack_serve(&def("/opt/tools/chappa-stack serve", true)));
        assert!(is_stack_serve(&def(
            "./target/debug/chappa-stack serve",
            true
        )));
        assert!(is_stack_serve(&def(
            "cargo run -p chappa-stack -- serve",
            true
        )));
        assert!(!is_stack_serve(&def("chappa-stack serve", false)));
        assert!(!is_stack_serve(&def("chappa-stack run-job 1", true)));
        assert!(!is_stack_serve(&def("node sources/github/index.mjs", true)));
    }

    #[test]
    fn a_definition_change_changes_the_trust_hash_and_nothing_else_does_without_a_runner() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("chappa.yml"), yml_with_runner(STACK)).unwrap();
        let first = trust_hash_on_disk(root).unwrap();
        let file_hash = project_model::hash::file_content_hash(&root.join("chappa.yml")).unwrap();
        assert_ne!(first, file_hash, "a runner adds its definition");
        // The stack moves to its own file, then changes there.
        std::fs::write(root.join("chappa.yml"), yml_with_runner("")).unwrap();
        std::fs::write(root.join("chappa.stack.yml"), STACK).unwrap();
        let split = trust_hash_on_disk(root).unwrap();
        std::fs::write(
            root.join("chappa.stack.yml"),
            STACK.replace("status.js", "evil.js"),
        )
        .unwrap();
        let changed = trust_hash_on_disk(root).unwrap();
        assert_ne!(
            split, changed,
            "an edit of chappa.stack.yml alone asks again"
        );
        // A project without a runner keeps the plain file hash.
        let plain = tempfile::tempdir().unwrap();
        std::fs::write(
            plain.path().join("chappa.yml"),
            "processes:\n  web:\n    command: npm run dev\n",
        )
        .unwrap();
        assert_eq!(
            trust_hash_on_disk(plain.path()).unwrap(),
            project_model::hash::file_content_hash(&plain.path().join("chappa.yml")).unwrap()
        );
    }

    #[test]
    fn the_prompt_lists_the_stack_commands_exactly_and_the_launch_carries_the_hash() {
        let pw = ["hu", "nt", "er2"].concat();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let stack = format!("{STACK}        env: {{ DB_PASSWORD: {pw} }}\n");
        std::fs::write(root.join("chappa.yml"), yml_with_runner(&stack)).unwrap();
        let yml = ProjectYml::load_result(root).unwrap();
        let stacks = stack_sources(root, &yml.processes);
        assert_eq!(stacks.len(), 1);
        let lines = prompt_lines(&stacks).join("\n");
        assert!(lines.contains("up: [node, scripts/up.js]"), "{lines}");
        assert!(
            lines.contains("status: [node, scripts/status.js]"),
            "{lines}"
        );
        // Trusting covers the exact text, so the prompt shows it, flagged.
        assert!(
            lines.contains(&format!("DB_PASSWORD={pw}  ⚠ looks like")),
            "{lines}"
        );
        assert!(!lines.contains(stack_core::credentials::MASK), "{lines}");
        let hash = stacks[0].runnable_hash().unwrap();
        let launched = with_trusted_definition(&yml.processes["stack"], Some(&hash));
        match &launched.command {
            CommandLine::Argv(w) => assert_eq!(
                &w[w.len() - 2..],
                [TRUSTED_DEFINITION_ARG.to_string(), hash.clone()]
            ),
            other => panic!("{other:?}"),
        }
        let none = with_trusted_definition(
            &ProcessDef {
                command: CommandLine::Shell("chappa-stack serve".into()),
                ..ProcessDef::default()
            },
            None,
        );
        assert_eq!(
            none.command,
            CommandLine::Shell("chappa-stack serve --trusted-definition none".into())
        );
    }
}

#[cfg(test)]
mod launch_tests {
    use super::*;

    fn shell(line: &str) -> ProcessDef {
        ProcessDef {
            command: CommandLine::Shell(line.into()),
            ..ProcessDef::default()
        }
    }

    #[test]
    fn the_settings_gitleaks_config_goes_on_the_runner_command_line() {
        let argv = ProcessDef {
            command: CommandLine::Argv(vec!["chappa-stack".into(), "serve".into()]),
            ..ProcessDef::default()
        };
        let hash = "ab".repeat(32);
        let path = r"C:\cfg dir\house rules.toml";
        let launched = launch_def(
            &argv,
            Some(&hash),
            &format!("  {path} "),
            &ProjectScanConfig::None,
        )
        .unwrap();
        assert_eq!(
            launched.command,
            CommandLine::Argv(vec![
                "chappa-stack".into(),
                "serve".into(),
                GITLEAKS_CONFIG_ARG.into(),
                path.into(),
                TRUSTED_DEFINITION_ARG.into(),
                hash.clone(),
            ])
        );
        // Nothing set: nothing added (the runner uses the shared default).
        assert_eq!(
            launch_def(&argv, None, "  ", &ProjectScanConfig::None)
                .unwrap()
                .command,
            with_trusted_definition(&argv, None).command
        );
        // A shell line gets it quoted for the platform's shell.
        let q = if cfg!(windows) { '"' } else { '\'' };
        assert_eq!(
            launch_def(&shell("chappa-stack serve"), None, path, &ProjectScanConfig::None)
                .unwrap()
                .command,
            CommandLine::Shell(format!(
                "chappa-stack serve {GITLEAKS_CONFIG_ARG} {q}{path}{q} {TRUSTED_DEFINITION_ARG} none"
            ))
        );
        // A path a shell would expand is refused, never passed.
        for bad in [
            r"C:\%USERPROFILE%\x.toml",
            "/tmp/$HOME/x.toml",
            "/a/'b'.toml",
        ] {
            let err = launch_def(
                &shell("chappa-stack serve"),
                None,
                bad,
                &ProjectScanConfig::None,
            )
            .unwrap_err();
            assert!(err.contains("as a list"), "{err}");
        }
    }

    #[test]
    fn the_project_config_and_its_trusted_hash_go_before_the_definition_hash() {
        let argv = ProcessDef {
            command: CommandLine::Argv(vec!["chappa-stack".into(), "serve".into()]),
            ..ProcessDef::default()
        };
        let path = std::path::PathBuf::from("proj dir")
            .join("tools")
            .join("gitleaks.toml");
        let hash = "cd".repeat(32);
        let trusted = ProjectScanConfig::Trusted {
            path: path.clone(),
            hash: hash.clone(),
        };
        let launched = launch_def(&argv, None, "", &trusted).unwrap();
        assert_eq!(
            launched.command,
            CommandLine::Argv(vec![
                "chappa-stack".into(),
                "serve".into(),
                PROJECT_GITLEAKS_CONFIG_ARG.into(),
                path.display().to_string(),
                PROJECT_GITLEAKS_CONFIG_HASH_ARG.into(),
                hash.clone(),
                TRUSTED_DEFINITION_ARG.into(),
                UNTRUSTED.into(),
            ])
        );
        // Not trusted: named with `none`, so the runner falls back and says so.
        let not_used = ProjectScanConfig::NotUsed {
            path: path.clone(),
            reason: "is not trusted".into(),
        };
        let q = if cfg!(windows) { '"' } else { '\'' };
        assert_eq!(
            launch_def(&shell("chappa-stack serve"), None, "", &not_used)
                .unwrap()
                .command,
            CommandLine::Shell(format!(
                "chappa-stack serve {PROJECT_GITLEAKS_CONFIG_ARG} {q}{}{q} {PROJECT_GITLEAKS_CONFIG_HASH_ARG} none {TRUSTED_DEFINITION_ARG} none",
                path.display()
            ))
        );
    }

    #[test]
    fn a_definition_other_than_the_trusted_one_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let def = ProcessDef {
            command: CommandLine::Argv(vec!["chappa-stack".into(), "serve".into()]),
            source: Some(Default::default()),
            ..ProcessDef::default()
        };
        // No stack in the folder: nothing to protect.
        assert_eq!(refusal(root, &def, None), None);
        std::fs::write(
            root.join("chappa.stack.yml"),
            "stacks:\n  app:\n    components:\n      web:\n        driver: script\n        up: [node, up.js]\n",
        )
        .unwrap();
        let hash = definition_trust(root).unwrap().unwrap().hash;
        assert_eq!(refusal(root, &def, Some(&hash)), None);
        // Never trusted, or changed since.
        assert_eq!(
            refusal(root, &def, None).as_deref(),
            Some(UNTRUSTED_REFUSAL)
        );
        std::fs::write(
            root.join("chappa.stack.yml"),
            "stacks:\n  app:\n    components:\n      web:\n        driver: script\n        up: [node, other.js]\n",
        )
        .unwrap();
        let why = refusal(root, &def, Some(&hash)).unwrap();
        assert!(why.contains("trust it again"), "{why}");
    }
}
