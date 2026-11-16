//! What the person trusts when they trust a stack: one hash over everything
//! the definition can make run.
//!
//! A stack runner re-reads its definition on every request and runs status
//! and plan scripts on its own schedule, so a `git pull` or an agent's edit
//! of `chappa.yml` or `chappa.stack.yml` would otherwise change what runs on
//! the next poll without anyone being asked. The app records
//! [`definition_trust_hash`] when the person trusts the project, starts the
//! runner with it, and the runner recomputes it before it runs anything; a
//! different hash runs nothing until the person trusts the new definition.
//!
//! **What it covers.** The whole parsed `stacks:` document, so every argv
//! (component verbs and scripts including `plan`, data and snapshot hooks,
//! data-safety `marker` / `sample`, catalog and resolve hooks), every `env`
//! entry, every `cwd`, every build (repo, context, dockerfile), image,
//! compose file and terraform folder; and the `chappa.yml` entry of every
//! project command a process component runs (`command:`, a dev mode's
//! `command:` or `watch:`). It is computed over the parsed values written
//! as canonical JSON (object keys sorted, no whitespace), so reformatting,
//! comments and key order change nothing, and any changed value changes it.
//!
//! **What it does not cover:** the contents of files the definition names
//! (a script's source, a compose file, a Dockerfile, the data-safety rule
//! file). Those are reviewed like any other file in the project.

use indexmap::IndexMap;
use serde_json::Value;
use serde_yaml::Value as YamlValue;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

use crate::credentials;
use crate::definition::{ComponentDef, LoadError, StacksDocument, PROJECT_FILE_NAME};

/// The value a runner is started with when nothing was trusted.
pub const UNTRUSTED: &str = "none";

/// The trusted view of a project's stack definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefinitionTrust {
    /// [`definition_trust_hash`], lowercase hex.
    pub hash: String,
    /// The file the `stacks:` document came from.
    pub file: PathBuf,
    /// One entry per stack and component: what it can run, for the trust
    /// prompt ([`definition_commands`]).
    pub commands: Vec<String>,
}

/// Read the project's stack definition and the project commands its process
/// components name, and hash them. `Ok(None)` when the project defines no
/// stack.
pub fn definition_trust(dir: &Path) -> Result<Option<DefinitionTrust>, LoadError> {
    let Some((file, doc)) = StacksDocument::load_from_project(dir)? else {
        return Ok(None);
    };
    let processes = project_processes(dir)?;
    Ok(Some(DefinitionTrust {
        hash: definition_trust_hash(&doc, &processes),
        commands: definition_commands(&doc, &processes),
        file,
    }))
}

/// The `processes:` entries of the project's `chappa.yml`, as written.
pub fn project_processes(dir: &Path) -> Result<IndexMap<String, YamlValue>, LoadError> {
    let path = dir.join(PROJECT_FILE_NAME);
    let Some(text) = crate::definition::read_capped(&path)? else {
        return Ok(IndexMap::new());
    };
    let value: YamlValue = serde_yaml::from_str(&text).map_err(|e| LoadError::Parse {
        path: path.clone(),
        message: e.to_string(),
    })?;
    let mut out = IndexMap::new();
    if let Some(YamlValue::Mapping(map)) = value.get("processes") {
        for (k, v) in map {
            if let Some(name) = k.as_str() {
                out.insert(name.to_string(), v.clone());
            }
        }
    }
    Ok(out)
}

/// The project commands a component runs through the process driver.
fn named_commands(c: &ComponentDef) -> Vec<&str> {
    let mut names: Vec<&str> = c.command.iter().map(String::as_str).collect();
    for m in c.modes.values() {
        names.extend(m.command.iter().map(String::as_str));
        names.extend(m.watch.iter().map(String::as_str));
    }
    names.sort_unstable();
    names.dedup();
    names
}

/// sha256 (lowercase hex) of the canonical form of `doc` plus the
/// `processes` entries its process components name (see the module docs).
pub fn definition_trust_hash(
    doc: &StacksDocument,
    processes: &IndexMap<String, YamlValue>,
) -> String {
    let mut used = serde_json::Map::new();
    for stack in doc.stacks.values() {
        for c in stack.components.values() {
            for name in named_commands(c) {
                let entry = processes.get(name).map(to_json).unwrap_or(Value::Null);
                used.insert(name.to_string(), entry);
            }
        }
    }
    let whole = serde_json::json!({
        "stacks": to_json(doc),
        "commands": Value::Object(used),
    });
    let mut text = String::new();
    canonical(&whole, &mut text);
    let mut h = Sha256::new();
    h.update(b"chappa stack definition v1\0");
    h.update(text.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn to_json<T: serde::Serialize + std::fmt::Debug>(v: &T) -> Value {
    // A YAML mapping with a key JSON cannot hold still has to hash: its
    // debug text is just as deterministic.
    serde_json::to_value(v).unwrap_or_else(|_| Value::String(format!("{v:?}")))
}

/// JSON text with object keys sorted at every level and no whitespace.
fn canonical(v: &Value, out: &mut String) {
    match v {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(k.clone()).to_string());
                out.push(':');
                canonical(&map[k], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical(item, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

/// An argv as the prompt shows it.
fn argv_text(argv: &[String]) -> String {
    credentials::shown_argv(argv)
}

/// `NAME=value` as the prompt shows it.
fn env_text(name: &str, value: &str) -> String {
    credentials::shown_env(name, value)
}

fn command_text(entry: Option<&YamlValue>) -> String {
    let Some(entry) = entry else {
        return "(not defined in chappa.yml)".to_string();
    };
    let mut text = match entry.get("command") {
        Some(YamlValue::String(s)) => credentials::shown_line(s),
        Some(YamlValue::Sequence(words)) => argv_text(
            &words
                .iter()
                .map(|w| {
                    w.as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("{w:?}"))
                })
                .collect::<Vec<_>>(),
        ),
        _ => "(no command)".to_string(),
    };
    if let Some(dir) = entry.get("working_dir").and_then(YamlValue::as_str) {
        text.push_str(&format!(" (in {dir})"));
    }
    if let Some(YamlValue::Mapping(env)) = entry.get("env") {
        for (k, v) in env {
            let k = k.as_str().unwrap_or("?");
            let v = v
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| format!("{v:?}"));
            text.push_str(&format!(" env {}", env_text(k, &v)));
        }
    }
    text
}

/// What each stack and component can run, one prompt entry each: the first
/// line names it, the indented lines list its argv, project commands,
/// builds, images, files, `cwd` and `env`.
pub fn definition_commands(
    doc: &StacksDocument,
    processes: &IndexMap<String, YamlValue>,
) -> Vec<String> {
    let mut out = Vec::new();
    for (stack_name, stack) in &doc.stacks {
        if !stack.env.is_empty() {
            let mut entry = format!("stack `{stack_name}`: environment of every component");
            for (k, v) in &stack.env {
                entry.push_str(&format!("\n    env {}", env_text(k, v)));
            }
            out.push(entry);
        }
        for (name, c) in &stack.components {
            let mut lines: Vec<String> = Vec::new();
            for (path, argv) in c.hooks() {
                lines.push(format!("{path}: {}", argv_text(argv)));
            }
            for cmd in named_commands(c) {
                lines.push(format!(
                    "runs `{cmd}`: {}",
                    command_text(processes.get(cmd))
                ));
            }
            if let Some(f) = &c.file {
                lines.push(format!("compose file {}", f.display()));
            }
            if let Some(d) = &c.dir {
                lines.push(format!("terraform in {}", d.display()));
            }
            for (mode, m) in &c.modes {
                if let Some(image) = &m.image {
                    let tag = m.tag.as_deref().unwrap_or("latest");
                    lines.push(format!("{mode}: image {image}:{tag}"));
                }
                if m.context.is_some() || m.dockerfile.is_some() {
                    lines.push(format!(
                        "{mode}: builds {} with {}",
                        m.repo
                            .as_deref()
                            .unwrap_or(Path::new("."))
                            .join(m.context.as_deref().unwrap_or(Path::new(".")))
                            .display(),
                        m.dockerfile
                            .as_deref()
                            .unwrap_or(Path::new("Dockerfile"))
                            .display()
                    ));
                }
            }
            if let Some(dir) = &c.cwd {
                lines.push(format!("in {}", dir.display()));
            }
            for (k, v) in &c.env {
                lines.push(format!("env {}", env_text(k, v)));
            }
            if lines.is_empty() {
                continue;
            }
            let driver = format!("{:?}", c.driver).to_lowercase();
            let mut entry = format!("stack `{stack_name}` · {name} ({driver})");
            for l in lines {
                entry.push_str("\n    ");
                entry.push_str(&l);
            }
            out.push(entry);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash_of(yml: &str, stack_file: Option<&str>) -> String {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("chappa.yml"), yml).unwrap();
        if let Some(s) = stack_file {
            std::fs::write(tmp.path().join("chappa.stack.yml"), s).unwrap();
        }
        definition_trust(tmp.path()).unwrap().unwrap().hash
    }

    const BASE: &str = r#"
processes:
  web-dev:
    command: [npm, run, dev]
    working_dir: web
    env: { NODE_ENV: development }
  unrelated:
    command: [node, other.js]
stacks:
  app:
    env: { SHARED: "1" }
    components:
      db:
        driver: compose
        file: stack/db.compose.yml
        service: db
        modes:
          prebuilt: { image: registry.example/db, tag: "16" }
          source: { repo: ../db, context: ., dockerfile: Dockerfile }
        dataSafety:
          marker: [docker, exec, "{container}", /scripts/marker.sh]
          sample: [docker, exec, "{container}", /scripts/sample.sh]
      web:
        driver: process
        command: web-dev
      app:
        driver: script
        up: [node, scripts/up.js]
        status: [node, scripts/status.js]
        plan: [node, scripts/plan.js]
        cwd: tools
        env: { API: "http://localhost:{port:app}" }
"#;

    #[test]
    fn the_hash_ignores_formatting_comments_and_key_order() {
        let base = hash_of(BASE, None);
        assert_eq!(base.len(), 64);
        let reformatted = BASE
            .replace("[npm, run, dev]", "\n      - npm\n      - run\n      - dev")
            .replace("stacks:\n", "# the stacks\nstacks:   # trailing\n")
            .replace(
                "        file: stack/db.compose.yml\n        service: db\n",
                "        service: db\n        file: stack/db.compose.yml\n",
            );
        assert_ne!(reformatted, BASE);
        assert_eq!(hash_of(&reformatted, None), base);
        // A command no component runs is not part of it.
        assert_eq!(hash_of(&BASE.replace("other.js", "else.js"), None), base);
    }

    #[test]
    fn every_executable_field_changes_the_hash() {
        let base = hash_of(BASE, None);
        for (from, to) in [
            ("scripts/up.js", "scripts/up2.js"),
            ("scripts/status.js", "scripts/evil.js"),
            ("scripts/plan.js", "scripts/plan2.js"),
            ("/scripts/marker.sh", "/scripts/m2.sh"),
            ("/scripts/sample.sh", "/scripts/s2.sh"),
            ("cwd: tools", "cwd: tools2"),
            (
                "API: \"http://localhost:{port:app}\"",
                "API: \"http://evil\"",
            ),
            ("SHARED: \"1\"", "SHARED: \"2\""),
            ("dockerfile: Dockerfile", "dockerfile: Evil.Dockerfile"),
            ("context: .", "context: sub"),
            ("registry.example/db", "registry.example/other"),
            ("[npm, run, dev]", "[npm, run, evil]"),
            ("working_dir: web", "working_dir: web2"),
            ("NODE_ENV: development", "NODE_ENV: production"),
            ("command: web-dev", "command: unrelated"),
        ] {
            assert!(BASE.contains(from), "{from}");
            assert_ne!(
                hash_of(&BASE.replace(from, to), None),
                base,
                "{from} -> {to}"
            );
        }
        // Adding a hook changes it too.
        let more = BASE.replace(
            "        plan: [node, scripts/plan.js]\n",
            "        plan: [node, scripts/plan.js]\n        down: [node, scripts/down.js]\n",
        );
        assert_ne!(hash_of(&more, None), base);
    }

    #[test]
    fn a_separate_stack_file_is_covered_the_same_way() {
        let (procs, stacks) = BASE.split_once("stacks:").unwrap();
        let split = hash_of(procs, Some(&format!("stacks:{stacks}")));
        assert_eq!(
            split,
            hash_of(BASE, None),
            "where the document lives does not matter"
        );
        let changed = hash_of(
            procs,
            Some(&format!(
                "stacks:{}",
                stacks.replace("scripts/up.js", "x.js")
            )),
        );
        assert_ne!(changed, split);
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("chappa.yml"), "processes: {}\n").unwrap();
        assert!(definition_trust(tmp.path()).unwrap().is_none());
    }

    #[test]
    fn the_prompt_lists_what_each_component_can_run() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("chappa.yml"), BASE).unwrap();
        let t = definition_trust(tmp.path()).unwrap().unwrap();
        let all = t.commands.join("\n");
        for needle in [
            "up: [node, scripts/up.js]",
            "plan: [node, scripts/plan.js]",
            "dataSafety.sample: [docker, exec, {container}, /scripts/sample.sh]",
            "runs `web-dev`: [npm, run, dev] (in web) env NODE_ENV=development",
            "in tools",
            "env API=http://localhost:{port:app}",
            "env SHARED=1",
            "source: builds",
            "prebuilt: image registry.example/db:16",
            "compose file stack/db.compose.yml",
        ] {
            assert!(all.contains(needle), "{needle} missing from:\n{all}");
        }
        assert!(
            t.commands
                .iter()
                .any(|c| c.starts_with("stack `app` · app (script)")),
            "{all}"
        );
    }

    #[test]
    fn inline_credentials_are_masked_in_the_prompt_but_still_hashed() {
        let pw = ["hu", "nt", "er2"].concat();
        let yml = format!(
            "stacks:
  app:
    components:
      db:
        driver: script
        up: [psql, \"postgresql://app:{pw}@db:5432/x\"]
        env: {{ DB_PASSWORD: {pw}, DB_HOST: db }}
"
        );
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("chappa.yml"), &yml).unwrap();
        let t = definition_trust(tmp.path()).unwrap().unwrap();
        let all = t.commands.join(
            "
",
        );
        assert!(!all.contains(&pw), "{all}");
        assert!(all.contains("postgresql://app:••••@db:5432/x"), "{all}");
        assert!(all.contains("DB_PASSWORD=•••• (set inline)"), "{all}");
        assert!(all.contains("DB_HOST=db"), "{all}");
        assert!(all.contains(crate::credentials::ADVICE), "{all}");
        let other = yml.replace(&pw, "different9");
        std::fs::write(tmp.path().join("chappa.yml"), &other).unwrap();
        assert_ne!(definition_trust(tmp.path()).unwrap().unwrap().hash, t.hash);
        // The definition check warns, naming the field, never the value.
        let doc = StacksDocument::parse_from_project_file(&yml)
            .unwrap()
            .unwrap();
        let warnings: Vec<_> = crate::definition::validate(&doc, &Default::default())
            .into_iter()
            .filter(|d| d.rule == crate::definition::Rule::InlineCredential)
            .collect();
        let paths: Vec<&str> = warnings.iter().map(|d| d.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "stacks.app.components.db.up",
                "stacks.app.components.db.env.DB_PASSWORD"
            ]
        );
        assert!(warnings.iter().all(|d| !d.message.contains(&pw)));
    }
}
