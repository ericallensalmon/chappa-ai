//! `chappa.yml` parse + defaults + working-dir containment.
//!
//! The rules this parser enforces, each one deliberate:
//!
//! - `auto_start` defaults TRUE, `auto_restart` false: a project you open
//!   should come up, but a process you stopped should stay stopped.
//! - Unknown keys at any level are ignored rather than rejected, so a file
//!   written by a newer version still loads here — but each one is logged
//!   once, so a typo is discoverable instead of silently inert.
//! - A blank or comment-only file is a valid config with an EMPTY process
//!   map, not a parse error.
//! - `working_dir` must resolve INSIDE the project root: parent traversal
//!   and symlink escapes are rejected, while ABSOLUTE paths that land
//!   inside the root are accepted. A process whose working_dir is invalid
//!   is reported per-process and the config still loads; that one process
//!   fails at spawn time.
//! - File cap 1MB; larger files are refused (`load` → None).
//!
//! `ProcessDef`'s field order is the order a written file gets, so a
//! write→load roundtrip preserves key order (IndexMap + serde_yaml).
//!
//! ## Write-back passthrough
//! The app WRITES this file, and a human edits it too, so a write-back must
//! not quietly discard what it did not understand. Every level of the model
//! carries an `unknown: IndexMap<String, serde_yaml::Value>` flattened
//! passthrough: a key this version does not model (a newer field, a user's
//! own annotation) survives with its value and its order among the other
//! unknown keys intact. What the passthrough CANNOT keep, stated here
//! because a write-back silently loses it:
//! - comments (serde_yaml drops them; the first write per file is preceded by
//!   a `chappa.yml.chappa-bak` copy — see `writeback.rs`);
//! - the POSITION of unknown keys relative to the known ones: serde's
//!   `flatten` emits them after the modelled fields;
//! - explicit `null`s for known optional keys (`icon: null`,
//!   `working_dir: null`) — they re-emit as the key being absent, which
//!   loads identically (both mean "default");
//! - scalar spelling (`yes`/`on` → `true`, quoting style, flow vs block
//!   collections, anchors/aliases — resolved on parse).

use std::fs;
use std::path::{Component, Path, PathBuf};

use indexmap::IndexMap;
use log::warn;
use serde::{Deserialize, Serialize};
use serde_yaml::Value as YamlValue;

/// Hard cap on the project file size (larger configs are refused).
pub const MAX_YML_BYTES: usize = 1 << 20; // 1 MiB

/// The project-file name: chappa-ai's own per-project process file,
/// always, with no toggle. This crate reads, watches and backwrites exactly
/// this one name and no other; every site refers to this constant, so a
/// stray literal is a bug.
pub const CHAPPA_YML_NAME: &str = "chappa.yml";

/// One project process. Field order = the example descriptor's order, then the
/// passthrough of keys chappa-ai does not model.
///
/// `env` is an `IndexMap`: a `BTreeMap` re-sorted the user's
/// variables alphabetically on write-back, which the "order too" rule
/// forbids. Spawn semantics are unchanged (the actor applies them as a set).
///
/// `PartialEq` only — `serde_yaml::Value` carries floats, so `Eq` is gone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcessDef {
    /// Required; run via the platform execution profile (`cmd /C` Windows,
    /// `sh -c` elsewhere) — see src-tauri/src/projects.rs.
    pub command: String,
    /// `None` = project root. Must resolve inside the project root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<PathBuf>,
    /// Default TRUE (documented default).
    #[serde(default = "default_auto_start")]
    pub auto_start: bool,
    #[serde(default)]
    pub auto_restart: bool,
    /// Glob patterns, matched relative to the project root. Invalid globs are
    /// ignored, never fail the config.
    #[serde(default)]
    pub restart_when_changed: Vec<String>,
    #[serde(default)]
    pub env: IndexMap<String, String>,
    /// Keys chappa-ai does not model, preserved verbatim through write-back.
    #[serde(default, flatten, skip_serializing_if = "IndexMap::is_empty")]
    pub unknown: IndexMap<String, YamlValue>,
}

impl Default for ProcessDef {
    fn default() -> Self {
        Self {
            command: String::new(),
            working_dir: None,
            auto_start: default_auto_start(),
            auto_restart: false,
            restart_when_changed: Vec::new(),
            env: IndexMap::new(),
            unknown: IndexMap::new(),
        }
    }
}

fn default_auto_start() -> bool {
    true
}

/// The parsed `chappa.yml`. `processes` preserves file order (IndexMap).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectYml {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_initials: Option<String>,
    #[serde(default)]
    pub processes: IndexMap<String, ProcessDef>,
    /// Top-level keys chappa-ai does not model, preserved through write-back.
    #[serde(default, flatten, skip_serializing_if = "IndexMap::is_empty")]
    pub unknown: IndexMap<String, YamlValue>,
}

impl Default for ProjectYml {
    fn default() -> Self {
        Self {
            name: None,
            icon: None,
            icon_initials: None,
            processes: IndexMap::new(),
            unknown: IndexMap::new(),
        }
    }
}

/// Unknown keys land here via `#[serde(flatten)]` so we can log them once.
#[derive(Debug, Deserialize)]
struct ProjectYmlRaw {
    name: Option<String>,
    icon: Option<String>,
    icon_initials: Option<String>,
    processes: Option<IndexMap<String, ProcessDefRaw>>,
    #[serde(flatten)]
    unknown: IndexMap<String, YamlValue>,
}

impl Default for ProjectYmlRaw {
    fn default() -> Self {
        Self {
            name: None,
            icon: None,
            icon_initials: None,
            processes: None,
            unknown: IndexMap::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct ProcessDefRaw {
    command: String,
    #[serde(default)]
    working_dir: Option<PathBuf>,
    #[serde(default = "default_auto_start")]
    auto_start: bool,
    #[serde(default)]
    auto_restart: bool,
    #[serde(default)]
    restart_when_changed: Vec<String>,
    #[serde(default)]
    env: IndexMap<String, String>,
    #[serde(flatten)]
    unknown: IndexMap<String, YamlValue>,
}

impl ProjectYml {
    /// Read and parse `<dir>/chappa.yml`. `None` when the file is missing,
    /// larger than 1 MiB, or structurally invalid (e.g. a process without
    /// `command`). A blank or comment-only file is a VALID empty config.
    pub fn load(dir: &Path) -> Option<ProjectYml> {
        Self::load_result(dir).ok()
    }

    /// [`load`](Self::load) with the REASON: the UI shows it while
    /// mutations are disabled ("never write while a load error is
    /// outstanding … visible reason").
    pub fn load_result(dir: &Path) -> Result<ProjectYml, String> {
        Self::load_result_with_hash(dir).map(|(yml, _)| yml)
    }

    /// [`load_result`](Self::load_result) plus the content hash of the exact
    /// bytes that were parsed — the concurrent-change guard compares
    /// it against a re-hash immediately before the atomic rename. One read
    /// serves both parse and hash, so the two can never disagree about which
    /// file version they describe.
    pub fn load_result_with_hash(dir: &Path) -> Result<(ProjectYml, String), String> {
        let path = dir.join(CHAPPA_YML_NAME);
        let meta = fs::metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if meta.len() as usize > MAX_YML_BYTES {
            warn!("{CHAPPA_YML_NAME} exceeds {MAX_YML_BYTES} bytes; refusing to load");
            return Err(format!("{CHAPPA_YML_NAME} exceeds {MAX_YML_BYTES} bytes"));
        }
        let bytes = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let text = std::str::from_utf8(&bytes)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let yml = Self::parse(text)?;
        Ok((yml, crate::hash::content_hash(&bytes)))
    }

    /// Parse `text`. Blank/comment-only → empty config (0.6.4).
    pub fn from_str(text: &str) -> Option<ProjectYml> {
        Self::parse(text).ok()
    }

    /// [`from_str`](Self::from_str) with serde_yaml's error text.
    pub fn parse(text: &str) -> Result<ProjectYml, String> {
        if is_blank_or_comment_only(text) {
            return Ok(ProjectYml::default());
        }
        let raw = match serde_yaml::from_str::<ProjectYmlRaw>(text) {
            Ok(raw) => raw,
            Err(e) => return Err(format!("chappa.yml: {e}")),
        };
        for key in raw.unknown.keys() {
            warn!("chappa.yml: ignoring unknown top-level key `{key}` (kept for write-back)");
        }
        let processes = raw
            .processes
            .map(|map| {
                map.into_iter()
                    .map(|(name, p)| {
                        for key in p.unknown.keys() {
                            warn!(
                                "chappa.yml: ignoring unknown key `{key}` on process `{name}` (kept for write-back)"
                            );
                        }
                        (
                            name,
                            ProcessDef {
                                command: p.command,
                                working_dir: p.working_dir,
                                auto_start: p.auto_start,
                                auto_restart: p.auto_restart,
                                restart_when_changed: p.restart_when_changed,
                                env: p.env,
                                unknown: p.unknown,
                            },
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(ProjectYml {
            name: raw.name,
            icon: raw.icon,
            icon_initials: raw.icon_initials,
            processes,
            unknown: raw.unknown,
        })
    }

    /// Serialize back to yaml (key order preserved; used by the
    /// write-back).
    pub fn to_yaml_string(&self) -> Result<String, serde_yaml::Error> {
        serde_yaml::to_string(self)
    }

    /// Replace `<dir>/chappa.yml` atomically (temp + rename) with this model.
    /// The write-back entry point is `writeback::apply`, which adds the
    /// load-error guard and the one-time backup around this.
    pub fn write(&self, dir: &Path) -> Result<(), String> {
        let text = self.to_yaml_string().map_err(|e| e.to_string())?;
        crate::atomic::write_text_atomic(&dir.join(CHAPPA_YML_NAME), &text, "yml.chappa-tmp")
    }

    // ---- the mutation operations behind the command menu ---------

    /// `+ Add command`: insert a NEW process at the end. The name must be
    /// unused; `def` is validated against `root` (see [`ProcessDef::validate`]).
    pub fn add_process(&mut self, root: &Path, name: &str, def: ProcessDef) -> Result<(), String> {
        let name = valid_name(name)?;
        if self.processes.contains_key(&name) {
            return Err(format!("a command named `{name}` already exists"));
        }
        def.validate(root)?;
        self.processes.insert(name, def);
        Ok(())
    }

    /// `Edit command…`: replace the definition of `old_name`, optionally
    /// renaming it to `new_name`. A rename is a KEY MOVE — the entry keeps its
    /// position in the file. Unknown keys of the old entry are carried over
    /// unless `def` already carries its own (the editor passes them through).
    pub fn edit_process(
        &mut self,
        root: &Path,
        old_name: &str,
        new_name: &str,
        mut def: ProcessDef,
    ) -> Result<(), String> {
        let new_name = valid_name(new_name)?;
        let idx = self
            .processes
            .get_index_of(old_name)
            .ok_or_else(|| format!("no command named `{old_name}`"))?;
        if new_name != old_name && self.processes.contains_key(&new_name) {
            return Err(format!("a command named `{new_name}` already exists"));
        }
        def.validate(root)?;
        if def.unknown.is_empty() {
            def.unknown = self.processes[idx].unknown.clone();
        }
        if new_name == old_name {
            self.processes[idx] = def;
        } else {
            // Position-preserving key move: drop the old slot (shifting the
            // tail down one), append under the new name, move it back up.
            self.processes.shift_remove_index(idx);
            self.processes.insert(new_name, def);
            let last = self.processes.len() - 1;
            self.processes.move_index(last, idx);
        }
        Ok(())
    }

    /// `Delete command "<name>"`. The tail shifts up (order preserved).
    pub fn remove_process(&mut self, name: &str) -> Result<ProcessDef, String> {
        self.processes
            .shift_remove(name)
            .ok_or_else(|| format!("no command named `{name}`"))
    }

    /// `Duplicate to ▸`: append a copy of `def` under `name`, or — when that
    /// name is taken in THIS file (the same-project duplicate, or two projects
    /// sharing a name) — under `<name> copy`, `<name> copy 2`, … Returns the
    /// name actually used. `def` keeps its unknown keys.
    pub fn duplicate_process(
        &mut self,
        root: &Path,
        name: &str,
        def: ProcessDef,
    ) -> Result<String, String> {
        let base = valid_name(name)?;
        let mut candidate = base.clone();
        let mut n = 1;
        while self.processes.contains_key(&candidate) {
            n += 1;
            candidate = if n == 2 {
                format!("{base} copy")
            } else {
                format!("{base} copy {}", n - 1)
            };
        }
        def.validate(root)?;
        self.processes.insert(candidate.clone(), def);
        Ok(candidate)
    }
}

/// A process name must be non-blank after trimming (a blank yml key would be
/// an unclickable row); the TRIMMED name is what gets stored.
fn valid_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("command name must not be blank".to_owned());
    }
    Ok(name.to_owned())
}

/// True when every non-empty line is a comment — accepted as an
/// empty config (0.6.4). `serde_yaml` itself would error on a pure-comment
/// document, so this guard runs before parsing.
fn is_blank_or_comment_only(text: &str) -> bool {
    text.lines()
        .all(|line| line.trim().is_empty() || line.trim_start().starts_with('#'))
}

impl ProcessDef {
    /// Resolve this process's working directory against the project root:
    /// - `None` → `Ok(None)` (caller uses the project root itself);
    /// - relative → `root.join(dir)`, must resolve inside `root`;
    /// - absolute → kept as-is, must resolve inside `root`.
    /// Symlink escapes are rejected. `Err` marks THIS process invalid — the
    /// config still loads.
    pub fn resolve_working_dir(&self, project_root: &Path) -> Result<Option<PathBuf>, String> {
        match &self.working_dir {
            None => Ok(None),
            Some(dir) => resolve_contained(project_root, dir).map(Some),
        }
    }

    /// Write-back validation: a non-blank command, env keys that
    /// are non-blank, and the containment rule REVALIDATED for
    /// `working_dir` — the editor must not be able to store what the spawn
    /// would refuse. Invalid globs pass (they are ignored at runtime).
    pub fn validate(&self, project_root: &Path) -> Result<(), String> {
        if self.command.trim().is_empty() {
            return Err("command must not be blank".to_owned());
        }
        if let Some(key) = self.env.keys().find(|k| k.trim().is_empty()) {
            return Err(format!("env variable name must not be blank (value `{}`)", self.env[key]));
        }
        self.resolve_working_dir(project_root).map(|_| ())
    }

    /// True when a change between two definitions needs the LIVE process
    /// restarted: only the spawn inputs count. `auto_start`/`auto_restart`
    /// are policy read at the next start/exit, and unknown keys are opaque to
    /// chappa-ai — neither justifies killing a running process (the
    /// watcher guard).
    pub fn runtime_eq(&self, other: &ProcessDef) -> bool {
        self.command == other.command
            && self.working_dir == other.working_dir
            && self.env == other.env
            && self.restart_when_changed == other.restart_when_changed
    }
}

/// The containment algorithm. Exported so tests (and the glue) can exercise it
/// directly. `dir` may be absolute or relative to `root`.
pub fn resolve_contained(root: &Path, dir: &Path) -> Result<PathBuf, String> {
    let joined = if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        root.join(dir)
    };
    let resolved = resolve_best_effort(&joined);
    // Resolve the root the same way as the child: canonicalize adds the
    // `\\?\` verbatim prefix on Windows and resolves a symlinked root (macOS
    // `/var` → `/private/var`), so a lexically-normalized root would never
    // prefix-match a canonicalized child.
    let root_resolved = resolve_best_effort(root);
    if resolved.starts_with(&root_resolved) {
        Ok(resolved)
    } else {
        Err(format!(
            "working_dir `{}` resolves outside the project root `{}`",
            dir.display(),
            root.display()
        ))
    }
}

/// Resolve `path` following symlinks for whatever part of it exists (via
/// canonicalize on the longest existing ancestor), then lexically normalize
/// the remainder (`.`/`..`). Handles: non-existent directories, `..` that
/// resolve inside the root, and symlink escapes whose target exists.
fn resolve_best_effort(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if existing.exists() {
            break;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name.to_os_string());
                existing = parent.to_path_buf();
            }
            _ => break,
        }
    }
    // Canonicalize the longest existing ancestor (resolves symlinks + `..`).
    let mut resolved = strip_verbatim(existing.canonicalize().unwrap_or(existing));
    for comp in tail.iter().rev() {
        resolved.push(comp);
    }
    normalize_lexically(&resolved)
}

/// `canonicalize` on Windows returns a `\\?\` verbatim path; strip the prefix
/// so resolved paths compare and display like their inputs (the containment
/// prefix check and process-spawn APIs both want the plain form).
#[allow(unused_mut)]
fn strip_verbatim(mut path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let s = path.as_os_str().to_string_lossy().into_owned();
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            path = PathBuf::from(format!(r"\\{rest}"));
        } else if let Some(rest) = s.strip_prefix(r"\\?\") {
            path = PathBuf::from(rest.to_owned());
        }
    }
    path
}

/// Resolve `.` and `..` components purely lexically (no filesystem access).
/// Component-wise, so `/a/b/../c` → `/a/c`; a `..` above the root stays a
/// `..` (never escapes upward past `/`).
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::RootDir | Component::Prefix(_) | Component::Normal(_) => {
                out.push(comp.as_os_str());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fixture is the example descriptor VERBATIM (fixture-copied here so
    /// tests never depend on the repo file). Key order and explicit `null`s
    /// must round-trip identically.
    const EXAMPLE: &str = r#"# Process config for this project — copy to chappa.yml and adjust.
# Each entry becomes a process tab when the project opens.
#
# Note: shared services (a database, a message broker) have NO entry here on
# purpose. Run them once from whichever project is always open, or from a
# compose file, so a single owner holds the port for every project.
name: chappa-ai
icon: null
processes:
  # Dev server with hot reload. `auto_start` means it comes up with the
  # project; `auto_restart: false` because a crash here should stay visible.
  dev server:
    command: npm run dev
    working_dir: null          # project root
    auto_start: true
    auto_restart: false
    restart_when_changed: []
    env: {}
  # Type-checks in watch mode alongside the dev server.
  typecheck watch:
    command: npm run typecheck -- --watch
    working_dir: null
    auto_start: true
    auto_restart: false
    restart_when_changed: []
    env: {}
  # A one-shot you spawn as a tab when you want it. Not auto_start —
  # it's an interactive run, not a service.
  test runner:
    command: npm test
    working_dir: null
    auto_start: false
    auto_restart: false
    restart_when_changed: []
    env: {}
"#;

    fn example_expected() -> ProjectYml {
        ProjectYml {
            name: Some("chappa-ai".into()),
            icon: None,
            icon_initials: None,
            processes: [
                (
                    "dev server".into(),
                    ProcessDef {
                        command: "npm run dev".into(),
                        working_dir: None,
                        auto_start: true,
                        auto_restart: false,
                        restart_when_changed: vec![],
                        env: IndexMap::new(),
                        unknown: IndexMap::new(),
                    },
                ),
                (
                    "typecheck watch".into(),
                    ProcessDef {
                        command: "npm run typecheck -- --watch".into(),
                        working_dir: None,
                        auto_start: true,
                        auto_restart: false,
                        restart_when_changed: vec![],
                        env: IndexMap::new(),
                        unknown: IndexMap::new(),
                    },
                ),
                (
                    "test runner".into(),
                    ProcessDef {
                        command: "npm test".into(),
                        working_dir: None,
                        auto_start: false,
                        auto_restart: false,
                        restart_when_changed: vec![],
                        env: IndexMap::new(),
                        unknown: IndexMap::new(),
                    },
                ),
            ]
            .into_iter()
            .collect(),
            unknown: IndexMap::new(),
        }
    }

    #[test]
    fn parses_example_descriptor_verbatim_with_exact_equality() {
        let parsed = ProjectYml::from_str(EXAMPLE).expect("example must parse");
        assert_eq!(parsed, example_expected());
        // Process order is preserved (IndexMap) — index assertion is the point.
        let names: Vec<&str> = parsed.processes.keys().map(String::as_str).collect();
        assert_eq!(
            names,
            vec!["dev server", "typecheck watch", "test runner"]
        );
    }

    #[test]
    fn roundtrip_write_load_is_stable_and_order_preserving() {
        let parsed = ProjectYml::from_str(EXAMPLE).unwrap();
        let text = parsed.to_yaml_string().unwrap();
        let again = ProjectYml::from_str(&text).unwrap();
        assert_eq!(parsed, again);
        let names: Vec<&str> = again.processes.keys().map(String::as_str).collect();
        assert_eq!(
            names,
            vec!["dev server", "typecheck watch", "test runner"]
        );
    }

    #[test]
    fn minimal_process_defaults() {
        // Only `command:` present → auto_start TRUE, auto_restart false,
        // empty globs/env, working_dir None.
        let yml = ProjectYml::from_str("processes:\n  build:\n    command: make build\n").unwrap();
        let p = &yml.processes["build"];
        assert_eq!(p.command, "make build");
        assert!(p.auto_start, "auto_start defaults TRUE (documented default)");
        assert!(!p.auto_restart);
        assert!(p.restart_when_changed.is_empty());
        assert!(p.env.is_empty());
        assert_eq!(p.working_dir, None);
    }

    #[test]
    fn missing_processes_is_an_empty_map() {
        let yml = ProjectYml::from_str("name: proj\n").unwrap();
        assert!(yml.processes.is_empty());
        let yml = ProjectYml::from_str("processes: null\n").unwrap();
        assert!(yml.processes.is_empty());
    }

    #[test]
    fn blank_and_comment_only_files_are_valid_empty_configs() {
        for text in ["", "   \n\t\n", "# nothing here\n\n# still nothing\n"] {
            let yml = ProjectYml::from_str(text);
            assert!(yml.is_some(), "`{text:?}` must load");
            assert!(yml.unwrap().processes.is_empty());
        }
    }

    #[test]
    fn unknown_keys_are_ignored_but_known_values_survive() {
        let yml = ProjectYml::from_str(
            r#"
frobnicate: true
name: chappa-ai
processes:
  build:
    command: make
    bogus_nested: [1, 2]
    auto_start: false
"#,
        )
        .unwrap();
        assert_eq!(yml.name.as_deref(), Some("chappa-ai"));
        assert!(!yml.processes["build"].auto_start);
        assert_eq!(yml.processes["build"].command, "make");
    }

    #[test]
    fn missing_command_fails_the_load() {
        assert!(ProjectYml::from_str("processes:\n  x:\n    auto_start: true\n").is_none());
        assert!(ProjectYml::from_str("processes:\n  x: null\n").is_none());
    }

    #[test]
    fn working_dir_null_is_none_and_resolves_to_root() {
        let yml =
            ProjectYml::from_str("processes:\n  s:\n    command: a\n    working_dir: null\n").unwrap();
        assert_eq!(yml.processes["s"].working_dir, None);
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            yml.processes["s"].resolve_working_dir(root.path()).unwrap(),
            None
        );
    }

    #[test]
    fn working_dir_containment_table() {
        let root = tempfile::tempdir().unwrap();
        let inside = root.path().join("work");
        fs::create_dir(&inside).unwrap();
        // Compare against the root as the resolver sees it: on Windows the
        // temp dir can be handed out as an 8.3 short name (`RUNNER~1`) that
        // canonicalize expands, so the raw tempdir path would never prefix-match.
        let root_resolved = resolve_best_effort(root.path());

        // Accepted: relative inside, absolute inside, `.` → root.
        for (label, dir) in [
            ("relative inside", "work".as_ref()),
            ("absolute inside", inside.as_path()),
            ("dot is root", Path::new(".")),
        ] {
            let resolved = resolve_contained(root.path(), dir)
                .unwrap_or_else(|e| panic!("{label} must be accepted: {e}"));
            assert!(
                resolved.starts_with(&root_resolved),
                "{label}: {resolved:?} must stay under {root_resolved:?}"
            );
        }

        // Rejected: parent traversal, absolute outside, a nonexistent `..`
        // escape, and a symlink escape.
        let outside = tempfile::tempdir().unwrap();
        for (label, dir) in [
            ("parent traversal", "../x".as_ref()),
            ("absolute outside", outside.path()),
            ("lexical .. escape", Path::new("missing/../../x")),
        ] {
            assert!(
                resolve_contained(root.path(), dir).is_err(),
                "{label} must be rejected"
            );
        }
        // The symlink escape is unix-only: `std::os::unix` does not exist on
        // Windows, so an ungated call did not merely fail at runtime — it
        // failed to COMPILE, taking every other project-model test on the host
        // with it. Linux coverage is unchanged.
        #[cfg(unix)]
        {
            let symlink = root.path().join("escape");
            std::os::unix::fs::symlink(outside.path(), &symlink).unwrap();
            assert!(
                resolve_contained(root.path(), symlink.as_path()).is_err(),
                "symlink escape must be rejected"
            );
        }
    }

    #[test]
    fn invalid_working_dir_errors_per_process_but_config_loads() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join(CHAPPA_YML_NAME),
            format!(
                "processes:\n  ok:\n    command: echo hi\n  bad:\n    command: echo oops\n    working_dir: {}\n",
                outside.path().display()
            ),
        )
        .unwrap();
        let yml = ProjectYml::load(root.path()).expect("config still loads");
        assert_eq!(yml.processes.len(), 2);
        assert!(yml.processes["ok"].resolve_working_dir(root.path()).is_ok());
        assert!(yml.processes["bad"]
            .resolve_working_dir(root.path())
            .is_err());
    }

    #[test]
    fn load_reads_dir_and_refuses_oversized_files() {
        let root = tempfile::tempdir().unwrap();
        assert!(ProjectYml::load(root.path()).is_none(), "missing file → None");

        fs::write(
            root.path().join(CHAPPA_YML_NAME),
            "processes:\n  a:\n    command: echo hi\n",
        )
        .unwrap();
        assert_eq!(ProjectYml::load(root.path()).unwrap().processes.len(), 1);

        // Oversized → refused (file cap).
        let big = root.path().join(CHAPPA_YML_NAME);
        fs::write(&big, vec![b'x'; MAX_YML_BYTES + 1]).unwrap();
        assert!(ProjectYml::load(root.path()).is_none());
    }
}
