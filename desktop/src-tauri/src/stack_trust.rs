//! The stack definition as part of a project's trust.
//!
//! A project trusted by the `chappa.yml` content hash would leave a stack
//! runner free to run whatever its definition says next: the runner re-reads
//! `stacks:` (or `chappa.stack.yml`) on every request and runs status and
//! plan scripts on its own. So for every `source:` command that starts the
//! stack runner (`chappa-stack … serve`), the project's trust hash also
//! covers [`stack_core::trust::definition_trust_hash`] of the folder it
//! serves, the trust prompt lists what that definition can run (credential-
//! shaped values masked), and the runner is started with
//! `--trusted-definition <hash>` (or `none` while nothing is trusted). The
//! runner runs nothing while its definition hashes differently; when the
//! person trusts the new content, the app starts it again with the new hash.
//!
//! What is stored is the hash, keyed like every other project trust record,
//! never the command list (see [`project_trust_hash`]).

use std::path::{Path, PathBuf};

use project_model::yml::{CommandLine, ProcessDef, ProjectYml};
use stack_core::trust::{definition_trust, DefinitionTrust, UNTRUSTED};

/// The argument the runner takes the trusted hash in.
pub const TRUSTED_DEFINITION_ARG: &str = "--trusted-definition";

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
pub fn is_stack_serve(def: &ProcessDef) -> bool {
    if !def.is_source() {
        return false;
    }
    let w = words(def);
    let named = w.iter().position(|word| {
        let word = word.trim_matches(['"', '\'']);
        Path::new(word)
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
pub fn project_trust_hash(file_hash: &str, stacks: &[StackSource]) -> String {
    if stacks.is_empty() {
        return file_hash.to_owned();
    }
    let mut text = format!("chappa project trust v1\0{file_hash}");
    for s in stacks {
        text.push_str(&format!("\0{}\0{}", s.name, s.hash()));
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
    ))
}

/// The trust prompt's entries for the stack definitions: one header per
/// runner, then what each component can run (values that look like
/// credentials masked).
pub fn prompt_lines(stacks: &[StackSource]) -> Vec<String> {
    let mut out = Vec::new();
    for s in stacks {
        match &s.definition {
            Ok(Some(t)) => {
                out.push(format!(
                    "`{}` runs the stack definition in {} (every command below runs without asking again)",
                    s.name,
                    t.file.display()
                ));
                out.extend(t.commands.iter().cloned());
            }
            Ok(None) => out.push(format!(
                "`{}` serves {}, which defines no stack",
                s.name,
                s.folder.display()
            )),
            Err(e) => out.push(format!(
                "`{}` serves {}, whose stack definition does not load ({}); nothing in it will run",
                s.name,
                s.folder.display(),
                stack_core::credentials::mask_text(e)
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
    fn the_prompt_lists_the_stack_commands_masked_and_the_launch_carries_the_hash() {
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
        assert!(lines.contains("DB_PASSWORD=•••• (set inline)"), "{lines}");
        assert!(!lines.contains(&pw), "{lines}");
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
