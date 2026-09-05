//! Agent-tool registry: `agent_tools.json` in the Tauri
//! app-config dir, the same missing-file / unknown-field / out-of-range
//! tolerance as settings.json, atomic writes.
//!
//! Chappa-side only — agent tools live in chappa-ai's own store; there is
//! no shared file, so the only parity constraint is the tool NAMES on the
//! MCP surface (`list_agent_tools`, `spawn_agent`).
//!
//! Model, in one sentence: an [`AgentTool`] is structured fields (program,
//! verbatim argv, model, runtime, env) and the command line is DERIVED from
//! them for display ([`AgentTool::command_line`]) — never stored, never parsed
//! back except through the explicit command-string importer
//! ([`parse_command_line`]) behind the "Add from command…" paste box. Field
//! notes: downstream attribution parses NAMES today; here the name is
//! free text and model/runtime/container are fields.
//!
//! Wire shape (snake_case — it doubles as the MCP `list_agent_tools` row):
//!
//! ```json
//! {"id": 3, "name": "fast agent", "tool_type": "opencode",
//!  "program": "opencode", "args": ["-m", "gateway/model-fast"],
//!  "model": "model-fast",
//!  "runtime": {"kind": "docker_exec", "container": "dev-worker",
//!              "user": "dev", "workdir": "/workspace/app", "tty": true,
//!              "max_busy_in_container": 1},
//!  "env": {"CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN": "1"},
//!  "enabled": true, "max_busy": null}
//! ```

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use tauri::State;

pub const FILE_NAME: &str = "agent_tools.json";

/// The tool types with known launch conventions, plus `custom` for the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ToolType {
    Claude,
    Opencode,
    Codex,
    Gemini,
    Copilot,
    Kimi,
    Amp,
    #[default]
    Custom,
}

impl ToolType {
    pub const ALL: [ToolType; 8] = [
        ToolType::Claude,
        ToolType::Opencode,
        ToolType::Codex,
        ToolType::Gemini,
        ToolType::Copilot,
        ToolType::Kimi,
        ToolType::Amp,
        ToolType::Custom,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ToolType::Claude => "claude",
            ToolType::Opencode => "opencode",
            ToolType::Codex => "codex",
            ToolType::Gemini => "gemini",
            ToolType::Copilot => "copilot",
            ToolType::Kimi => "kimi",
            ToolType::Amp => "amp",
            ToolType::Custom => "custom",
        }
    }

    /// The default program name for a type (what the template seeds and what
    /// the importer uses to classify a bare program token).
    fn program(self) -> &'static str {
        match self {
            ToolType::Custom => "",
            other => other.as_str(),
        }
    }

    /// Classify a program token (`claude`, `/usr/bin/claude`, `claude.exe`).
    fn from_program(program: &str) -> ToolType {
        let base = program
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(program)
            .trim_end_matches(".exe")
            .trim_end_matches(".cmd");
        ToolType::ALL
            .into_iter()
            .find(|t| *t != ToolType::Custom && t.program() == base)
            .unwrap_or(ToolType::Custom)
    }
}

/// Where the agent CLI runs. Internally tagged on `kind` so the UI's
/// Host/Docker segmented picker maps one-to-one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Runtime {
    Host,
    DockerExec {
        container: String,
        #[serde(default)]
        user: Option<String>,
        #[serde(default)]
        workdir: Option<String>,
        /// `-it` (default true). False = `-i` only.
        #[serde(default = "default_true")]
        tty: bool,
        /// Shared across every tool naming this container.
        #[serde(default)]
        max_busy_in_container: Option<u32>,
    },
}

fn default_true() -> bool {
    true
}

impl Default for Runtime {
    fn default() -> Self {
        Runtime::Host
    }
}

impl Runtime {
    pub fn container(&self) -> Option<&str> {
        match self {
            Runtime::DockerExec { container, .. } => Some(container),
            Runtime::Host => None,
        }
    }

    pub fn is_docker(&self) -> bool {
        matches!(self, Runtime::DockerExec { .. })
    }
}

/// One registered agent tool. Field-level `default` on purpose (the
/// settings.rs lesson): one half-written entry in a hand-edited file must
/// degrade alone, never reset the whole registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AgentTool {
    /// Stable, never reused.
    pub id: u32,
    /// Display only — NEVER parsed.
    pub name: String,
    pub tool_type: ToolType,
    /// The agent CLI, e.g. `claude`.
    pub program: String,
    /// Verbatim argv, never re-split.
    pub args: Vec<String>,
    /// Attribution field.
    pub model: Option<String>,
    pub runtime: Runtime,
    /// Merged into the spawn env (document order).
    pub env: IndexMap<String, String>,
    pub enabled: bool,
    /// Per-tool concurrency.
    pub max_busy: Option<u32>,
    /// `tty` (default — a pty + VT parsing, the terminal panel) or
    /// `json` (the CLI's machine mode over pipes, typed `agent://event`s,
    /// the transcript view). Only tool types with a machine mode
    /// (`agent_json::machine_mode`) can spawn as `json`.
    pub transport: Transport,
}

/// How an agent's bytes reach the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    #[default]
    Tty,
    Json,
}

impl Transport {
    pub fn as_str(self) -> &'static str {
        match self {
            Transport::Tty => "tty",
            Transport::Json => "json",
        }
    }
}

/// The env var that keeps Claude Code in normal scrollback (
/// forensics). Shipped ONLY as a template default — spawn code knows nothing
/// about claude, and the user can delete the row.
pub const CLAUDE_ALT_SCREEN_ENV: (&str, &str) = ("CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN", "1");

impl AgentTool {
    /// Seed defaults for a new tool of `tool_type`. The claude template ships
    /// `CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN=1` VISIBLE in the env rows and
    /// deletable — the var is the documented control and may vanish upstream,
    /// so it must be a user-editable default, never hardcoded in spawn code.
    pub fn template(tool_type: ToolType) -> AgentTool {
        let mut env = IndexMap::new();
        if tool_type == ToolType::Claude {
            env.insert(
                CLAUDE_ALT_SCREEN_ENV.0.to_owned(),
                CLAUDE_ALT_SCREEN_ENV.1.to_owned(),
            );
        }
        AgentTool {
            id: 0,
            name: match tool_type {
                ToolType::Custom => String::new(),
                other => other.as_str().to_owned(),
            },
            tool_type,
            program: tool_type.program().to_owned(),
            args: Vec::new(),
            model: None,
            runtime: Runtime::Host,
            env,
            enabled: true,
            max_busy: None,
            // Always tty by default; the Agents pane OFFERS json for the
            // types that have a machine mode (claude, opencode).
            transport: Transport::Tty,
        }
    }

    /// The parity `command` string: `docker exec -u U -it -w W [-e K=V…]
    /// CONTAINER program args…`, or `program args…` for Host. Display only.
    pub fn command_line(&self) -> String {
        let mut words: Vec<String> = Vec::new();
        if self.runtime.is_docker() {
            words.push("docker".into());
            words.extend(docker_exec_prefix(&self.runtime, &self.env));
        }
        words.push(self.program.clone());
        words.extend(self.args.iter().cloned());
        words.iter().map(|w| quote_word(w)).collect::<Vec<_>>().join(" ")
    }

    /// Prune what the lenient parse let through and clamp the limits (0 = no
    /// limit is expressed as `None`, never as a zero that refuses everything).
    fn normalize(&mut self) {
        self.name = self.name.trim().to_owned();
        self.program = self.program.trim().to_owned();
        if self.max_busy == Some(0) {
            self.max_busy = None;
        }
        if let Runtime::DockerExec {
            container,
            user,
            workdir,
            max_busy_in_container,
            ..
        } = &mut self.runtime
        {
            *container = container.trim().to_owned();
            if user.as_deref().is_some_and(|u| u.trim().is_empty()) {
                *user = None;
            }
            if workdir.as_deref().is_some_and(|w| w.trim().is_empty()) {
                *workdir = None;
            }
            if *max_busy_in_container == Some(0) {
                *max_busy_in_container = None;
            }
        }
        if self.model.as_deref().is_some_and(|m| m.trim().is_empty()) {
            self.model = None;
        }
        self.env.retain(|k, _| !k.trim().is_empty());
    }

    /// An entry that can neither be listed nor spawned.
    fn is_usable(&self) -> bool {
        !self.program.is_empty()
            && match &self.runtime {
                Runtime::Host => true,
                Runtime::DockerExec { container, .. } => !container.is_empty(),
            }
    }
}

/// The `docker exec` words after `docker`: `exec [-u U] -it|-i [-w W] [-e K=V…]
/// CONTAINER`. The ONE spelling shared by the display command line and the
/// real spawn plan (`agents::build_spawn_plan`), so what the Agents pane
/// shows is what runs. Empty for a Host runtime.
pub fn docker_exec_prefix(runtime: &Runtime, env: &IndexMap<String, String>) -> Vec<String> {
    let Runtime::DockerExec {
        container,
        user,
        workdir,
        tty,
        ..
    } = runtime
    else {
        return Vec::new();
    };
    let mut words: Vec<String> = vec!["exec".into()];
    if let Some(user) = user {
        words.push("-u".into());
        words.push(user.clone());
    }
    words.push(if *tty { "-it" } else { "-i" }.into());
    if let Some(workdir) = workdir {
        words.push("-w".into());
        words.push(workdir.clone());
    }
    for (k, v) in env {
        words.push("-e".into());
        words.push(format!("{k}={v}"));
    }
    words.push(container.clone());
    words
}

// ---- shell-words ------------------------------------------------------------

/// Quote one word for display when it carries whitespace or quotes (double-
/// quote style, `\"` and `\\` escaped inside; `render → parse` round-trips
/// through [`shell_words`]). A bare word — Windows paths with backslashes
/// included — is left alone: backslash is literal outside quotes.
fn quote_word(word: &str) -> String {
    if word.is_empty() || word.chars().any(|c| c.is_whitespace() || c == '"' || c == '\'') {
        let escaped = word.replace('\\', "\\\\").replace('"', "\\\"");
        format!("\"{escaped}\"")
    } else {
        word.to_owned()
    }
}

/// Shell-words split for the paste box: whitespace separates, single quotes
/// are literal, double quotes allow `\"` and `\\`. A backslash OUTSIDE quotes
/// is a literal character on every platform — this is not a POSIX shell, and
/// the strings pasted here are Windows command lines as often as not
/// (`C:\Users\…\claude.cmd`); eating the backslashes mangled every path.
/// No expansion of any kind.
pub fn shell_words(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                for c in chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    cur.push(c);
                }
            }
            '"' => {
                in_word = true;
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => match chars.next() {
                            Some(n @ ('"' | '\\')) => cur.push(n),
                            Some(n) => {
                                cur.push('\\');
                                cur.push(n);
                            }
                            None => cur.push('\\'),
                        },
                        other => cur.push(other),
                    }
                }
            }
            c if c.is_whitespace() => {
                if in_word {
                    out.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            other => {
                in_word = true;
                cur.push(other);
            }
        }
    }
    if in_word {
        out.push(cur);
    }
    out
}

/// What the importer could and could not classify.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ParsedCommand {
    pub tool: AgentTool,
    /// True when a `docker exec` prefix was recognised.
    pub docker_detected: bool,
    /// Human-readable caveats for the UI ("could not detect container").
    pub warnings: Vec<String>,
}

/// Parse a command-style string into structured fields. Shell-words
/// split, then recognise `docker exec` flags `-u/-w/-i/-t/-it/-e` up to the
/// container token; anything the parser can't classify becomes
/// `Runtime::Host` with the whole string as program + args, flagged.
pub fn parse_command_line(line: &str) -> ParsedCommand {
    let words = shell_words(line);
    let mut warnings = Vec::new();
    let mut tool = AgentTool::template(ToolType::Custom);
    if words.is_empty() {
        warnings.push("empty command".to_owned());
        return ParsedCommand {
            tool,
            docker_detected: false,
            warnings,
        };
    }
    let mut docker_detected = false;
    let mut rest: &[String] = &words;
    if words.len() >= 2 && words[0] == "docker" && words[1] == "exec" {
        let mut user = None;
        let mut workdir = None;
        // `-i` is implied by every spawn (the pty IS stdin); only `-t` is a
        // field, so the interactive flag is recognised and dropped.
        let mut tty = false;
        let mut env = IndexMap::new();
        let mut i = 2;
        let mut container: Option<String> = None;
        let mut unknown_flags = Vec::new();
        while i < words.len() {
            let w = words[i].as_str();
            match w {
                "-u" | "--user" => {
                    user = words.get(i + 1).cloned();
                    i += 2;
                }
                "-w" | "--workdir" => {
                    workdir = words.get(i + 1).cloned();
                    i += 2;
                }
                "-e" | "--env" => {
                    if let Some(kv) = words.get(i + 1) {
                        match kv.split_once('=') {
                            Some((k, v)) => {
                                env.insert(k.to_owned(), v.to_owned());
                            }
                            None => warnings.push(format!("env `{kv}` without a value was dropped")),
                        }
                    }
                    i += 2;
                }
                "-i" | "--interactive" => {
                    i += 1;
                }
                "-t" | "--tty" | "-it" | "-ti" => {
                    tty = true;
                    i += 1;
                }
                f if f.starts_with("--user=") => {
                    user = Some(f["--user=".len()..].to_owned());
                    i += 1;
                }
                f if f.starts_with("--workdir=") => {
                    workdir = Some(f["--workdir=".len()..].to_owned());
                    i += 1;
                }
                f if f.starts_with('-') => {
                    // `--privileged`, `-d`, … : recorded, then skipped. A flag
                    // we do not model cannot be rendered back, so say so.
                    unknown_flags.push(f.to_owned());
                    i += 1;
                }
                _ => {
                    container = Some(w.to_owned());
                    i += 1;
                    break;
                }
            }
        }
        match container {
            Some(container) if i <= words.len() && !words[i..].is_empty() => {
                docker_detected = true;
                if !unknown_flags.is_empty() {
                    warnings.push(format!(
                        "unsupported docker flags dropped: {}",
                        unknown_flags.join(" ")
                    ));
                }
                tool.runtime = Runtime::DockerExec {
                    container,
                    user,
                    workdir,
                    tty,
                    max_busy_in_container: None,
                };
                tool.env = env;
                rest = &words[i..];
            }
            _ => {
                warnings.push("could not detect container".to_owned());
                rest = &words;
            }
        }
    }
    let program = rest[0].clone();
    tool.tool_type = ToolType::from_program(&program);
    tool.program = program;
    tool.args = rest[1..].to_vec();
    tool.model = detect_model(&tool.args);
    tool.name = match tool.tool_type {
        ToolType::Custom => tool.program.clone(),
        other => other.as_str().to_owned(),
    };
    if let Some(model) = &tool.model {
        tool.name = format!("{} · {}", tool.name, model);
    }
    ParsedCommand {
        tool,
        docker_detected,
        warnings,
    }
}

/// `-m X` / `--model X` / `--model=X` → the attribution field, with a
/// provider prefix (anything before the last `/`) stripped for display.
fn detect_model(args: &[String]) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let raw = if a == "-m" || a == "--model" {
            i += 1;
            args.get(i).cloned()
        } else {
            a.strip_prefix("--model=").map(str::to_owned)
        };
        if let Some(raw) = raw {
            let trimmed = raw.rsplit('/').next().unwrap_or(&raw).to_owned();
            return Some(trimmed);
        }
        i += 1;
    }
    None
}

// ---- store ------------------------------------------------------------------

/// On-disk shape: the tools plus the never-reused id counter.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
struct FileShape {
    next_id: u32,
    tools: Vec<AgentTool>,
}

/// JSON store over an injected path (tests: tempdir). Loads on construction,
/// persists atomically on every mutation.
#[derive(Debug, Clone)]
pub struct AgentToolStore {
    path: PathBuf,
    next_id: u32,
    tools: Vec<AgentTool>,
}

impl AgentToolStore {
    /// Missing / unreadable / unparseable → empty registry, never an error.
    /// Unknown fields are ignored; a half-written entry degrades alone.
    pub fn new(path: PathBuf) -> Self {
        let parsed = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok());
        let mut next_id = 1;
        let mut tools = Vec::new();
        if let Some(value) = parsed {
            next_id = value
                .get("next_id")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32)
                .unwrap_or(1)
                .max(1);
            if let Some(list) = value.get("tools").and_then(|v| v.as_array()) {
                for raw in list {
                    if let Ok(mut tool) = serde_json::from_value::<AgentTool>(raw.clone()) {
                        tool.normalize();
                        if tool.id == 0 || !tool.is_usable() {
                            continue;
                        }
                        if tools.iter().any(|t: &AgentTool| t.id == tool.id) {
                            continue; // duplicate id: first wins
                        }
                        tools.push(tool);
                    }
                }
            }
        }
        // The counter must sit above every stored id even if the file's
        // counter was hand-edited backwards: ids are never reused.
        let max_id = tools.iter().map(|t| t.id).max().unwrap_or(0);
        if next_id <= max_id {
            next_id = max_id + 1;
        }
        Self {
            path,
            next_id,
            tools,
        }
    }

    pub fn list(&self) -> &[AgentTool] {
        &self.tools
    }

    pub fn get(&self, id: u32) -> Option<&AgentTool> {
        self.tools.iter().find(|t| t.id == id)
    }

    /// Insert (`id == 0`) or replace (`id` known). Returns the stored tool
    /// with its id assigned. Errors: blank program/container, unknown id.
    pub fn upsert(&mut self, mut tool: AgentTool) -> Result<AgentTool, String> {
        tool.normalize();
        if tool.program.is_empty() {
            return Err("program is required".to_owned());
        }
        if tool.transport == Transport::Json && crate::agent_json::machine_mode(tool.tool_type).is_none() {
            return Err(format!(
                "tool type \"{}\" has no machine mode — the json transport needs one of: {}",
                tool.tool_type.as_str(),
                crate::agent_json::MACHINE_MODE_TYPES
                    .iter()
                    .map(|t| t.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if let Runtime::DockerExec { container, .. } = &tool.runtime {
            if container.is_empty() {
                return Err("container is required for a docker-exec runtime".to_owned());
            }
        }
        if tool.name.is_empty() {
            tool.name = tool.program.clone();
        }
        if tool.id == 0 {
            tool.id = self.next_id;
            self.next_id = self.next_id.wrapping_add(1).max(1);
            self.tools.push(tool.clone());
        } else {
            let slot = self
                .tools
                .iter_mut()
                .find(|t| t.id == tool.id)
                .ok_or_else(|| format!("no such agent tool: {}", tool.id))?;
            *slot = tool.clone();
        }
        self.save()?;
        Ok(tool)
    }

    pub fn delete(&mut self, id: u32) -> Result<(), String> {
        let before = self.tools.len();
        self.tools.retain(|t| t.id != id);
        if self.tools.len() == before {
            return Err(format!("no such agent tool: {id}"));
        }
        self.save()
    }

    fn save(&self) -> Result<(), String> {
        project_model::atomic::write_json_atomic(
            &self.path,
            &FileShape {
                next_id: self.next_id,
                tools: self.tools.clone(),
            },
        )
    }
}

/// Managed state: the store, initialized once from setup (the SettingsState
/// shape — `Option` because `Default` must work before the path is known).
#[derive(Clone, Default)]
pub struct AgentToolsState {
    inner: Arc<Mutex<Option<AgentToolStore>>>,
}

impl AgentToolsState {
    pub fn init(&self, config_dir: PathBuf) {
        let store = AgentToolStore::new(config_dir.join(FILE_NAME));
        if let Ok(mut guard) = self.inner.lock() {
            *guard = Some(store);
        }
    }

    /// Tests: a store over an explicit file.
    pub fn with_path(path: PathBuf) -> Self {
        let state = Self::default();
        if let Ok(mut guard) = state.inner.lock() {
            *guard = Some(AgentToolStore::new(path));
        }
        state
    }

    pub fn list(&self) -> Vec<AgentTool> {
        self.inner
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|s| s.list().to_vec()))
            .unwrap_or_default()
    }

    pub fn get(&self, id: u32) -> Option<AgentTool> {
        self.inner
            .lock()
            .ok()
            .and_then(|g| g.as_ref().and_then(|s| s.get(id).cloned()))
    }

    pub fn upsert(&self, tool: AgentTool) -> Result<AgentTool, String> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| "agent tools state poisoned".to_owned())?;
        guard
            .as_mut()
            .ok_or_else(|| "agent tools store not initialized".to_owned())?
            .upsert(tool)
    }

    pub fn delete(&self, id: u32) -> Result<(), String> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| "agent tools state poisoned".to_owned())?;
        guard
            .as_mut()
            .ok_or_else(|| "agent tools store not initialized".to_owned())?
            .delete(id)
    }
}

/// Refuse a delete while a live agent references the tool — the error NAMES
/// the processes. Pure over the registry's agent rows so the tests need no
/// pty.
pub fn delete_refusal(id: u32, live: &[crate::registry::AgentRow]) -> Option<String> {
    let names: Vec<String> = live
        .iter()
        .filter(|r| r.tool_id == id && r.child_alive)
        .map(|r| format!("{} (process {})", r.name, r.id))
        .collect();
    if names.is_empty() {
        None
    } else {
        Some(format!(
            "agent tool {id} is in use by a live agent: {} — close it first",
            names.join(", ")
        ))
    }
}

// ---- commands ---------------------------------------------------------------

/// What the webview's `list_agent_tools` answers: the rows plus the tool
/// types with a machine mode (the Transport picker offers `json` only for
/// those) — ONE Rust source, no frontend copy.
#[derive(Debug, Clone, Serialize)]
pub struct AgentToolsListing {
    pub tools: Vec<AgentTool>,
    pub machine_mode_types: Vec<ToolType>,
}

#[tauri::command]
pub fn list_agent_tools(tools: State<'_, AgentToolsState>) -> Result<AgentToolsListing, String> {
    Ok(AgentToolsListing {
        tools: tools.list(),
        machine_mode_types: crate::agent_json::MACHINE_MODE_TYPES.to_vec(),
    })
}

#[tauri::command]
pub fn upsert_agent_tool(
    tools: State<'_, AgentToolsState>,
    tool: AgentTool,
) -> Result<AgentTool, String> {
    tools.upsert(tool)
}

#[tauri::command]
pub fn delete_agent_tool(
    tools: State<'_, AgentToolsState>,
    registry: State<'_, crate::registry::Registry>,
    id: u32,
) -> Result<(), String> {
    if let Some(reason) = delete_refusal(id, &registry.agent_rows()) {
        return Err(reason);
    }
    tools.delete(id)
}

/// The paste box: a command-style string → prefilled tool + caveats.
#[tauri::command]
pub fn parse_agent_command(line: String) -> ParsedCommand {
    parse_command_line(&line)
}

/// A new tool's defaults for the edit modal (the claude alt-screen env row
/// among them).
#[tauri::command]
pub fn agent_tool_template(tool_type: ToolType) -> AgentTool {
    AgentTool::template(tool_type)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn docker(container: &str, user: Option<&str>, workdir: Option<&str>, tty: bool) -> Runtime {
        Runtime::DockerExec {
            container: container.into(),
            user: user.map(str::to_owned),
            workdir: workdir.map(str::to_owned),
            tty,
            max_busy_in_container: None,
        }
    }

    fn tool(program: &str, args: &[&str], runtime: Runtime, env: &[(&str, &str)]) -> AgentTool {
        let mut t = AgentTool::template(ToolType::from_program(program));
        t.program = program.into();
        t.args = args.iter().map(|s| s.to_string()).collect();
        t.runtime = runtime;
        t.env = env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        t.model = detect_model(&t.args);
        t.name = match t.tool_type {
            ToolType::Custom => t.program.clone(),
            other => other.as_str().to_owned(),
        };
        if let Some(m) = &t.model {
            t.name = format!("{} · {}", t.name, m);
        }
        t
    }

    /// `parse(render(t)) == t` over fixtures covering every docker flag
    /// combination, a non-docker string, and quoted spaces.
    #[test]
    fn parse_render_round_trips_every_docker_flag_combination() {
        let fixtures = vec![
            tool("claude", &[], Runtime::Host, &[]),
            tool("opencode", &["-m", "gateway/model-fast"], docker("dev-worker", Some("dev"), Some("/workspace/app"), true), &[]),
            tool("opencode", &["-m", "gateway/model-large"], docker("dev-worker", None, Some("/workspace/app"), true), &[]),
            tool("kimi", &[], docker("box", Some("root"), None, true), &[]),
            tool("codex", &[], docker("box", None, None, true), &[]),
            tool("gemini", &[], docker("box", None, None, false), &[]),
            tool("claude", &[], docker("box", Some("me"), Some("/w"), false), &[("CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN", "1"), ("FOO", "bar baz")]),
            tool("amp", &["--prompt", "two words here"], docker("box", Some("u"), Some("/w"), true), &[]),
            tool("/usr/local/bin/my tool", &["--flag", "a \"quoted\" arg"], Runtime::Host, &[]),
        ];
        for t in fixtures {
            let rendered = t.command_line();
            let parsed = parse_command_line(&rendered);
            assert_eq!(parsed.tool, t, "round trip of {rendered:?}");
            assert_eq!(parsed.docker_detected, t.runtime.is_docker(), "{rendered:?}");
            assert!(parsed.warnings.is_empty(), "{rendered:?}: {:?}", parsed.warnings);
        }
    }

    #[test]
    fn command_string_populates_fields_and_model() {
        let p = parse_command_line(
            "docker exec -u dev -it -w /workspace/app dev-worker opencode -m gateway/model-fast",
        );
        assert_eq!(p.tool.tool_type, ToolType::Opencode);
        assert_eq!(p.tool.program, "opencode");
        assert_eq!(p.tool.args, vec!["-m", "gateway/model-fast"]);
        assert_eq!(p.tool.model.as_deref(), Some("model-fast"));
        assert_eq!(p.tool.runtime, docker("dev-worker", Some("dev"), Some("/workspace/app"), true));
        assert!(p.docker_detected);
        assert_eq!(
            p.tool.command_line(),
            "docker exec -u dev -it -w /workspace/app dev-worker opencode -m gateway/model-fast"
        );
    }

    #[test]
    fn unclassifiable_docker_string_falls_back_to_host_and_flags_it() {
        // No program after the container token → cannot tell container from program.
        let p = parse_command_line("docker exec -it dev-worker");
        assert!(!p.docker_detected);
        assert_eq!(p.tool.runtime, Runtime::Host);
        assert_eq!(p.tool.program, "docker");
        assert_eq!(p.tool.args, vec!["exec", "-it", "dev-worker"]);
        assert!(p.warnings.iter().any(|w| w.contains("could not detect container")));
        // A non-docker string is Host with a bare program.
        let p = parse_command_line("claude --dangerously-skip-permissions");
        assert_eq!(p.tool.runtime, Runtime::Host);
        assert_eq!(p.tool.tool_type, ToolType::Claude);
        assert_eq!(p.tool.args, vec!["--dangerously-skip-permissions"]);
        assert!(p.warnings.is_empty());
    }

    #[test]
    fn shell_words_handles_quotes_and_escapes() {
        assert_eq!(shell_words(r#"a "b c" 'd e' f\ g"#), vec!["a", "b c", "d e", "f\\", "g"]);
        assert_eq!(shell_words(r#""with \"inner\" quotes""#), vec![r#"with "inner" quotes"#]);
        assert_eq!(shell_words("   "), Vec::<String>::new());
        assert_eq!(shell_words("x \"\" y"), vec!["x", "", "y"]);
    }

    /// Review fix: a bare backslash is LITERAL outside quotes (this is not a
    /// POSIX shell) — a pasted Windows path must survive the importer and
    /// the display round-trip byte for byte.
    #[test]
    fn backslashes_survive_paste_and_round_trip() {
        let line = r"C:\Users\dev\AppData\Roaming\npm\claude.cmd --flag";
        assert_eq!(
            shell_words(line),
            vec![r"C:\Users\dev\AppData\Roaming\npm\claude.cmd", "--flag"]
        );
        let parsed = parse_command_line(line);
        assert_eq!(parsed.tool.program, r"C:\Users\dev\AppData\Roaming\npm\claude.cmd");
        assert_eq!(parsed.tool.args, vec!["--flag"]);
        assert_eq!(parsed.tool.tool_type, ToolType::Claude);
        assert_eq!(parsed.tool.command_line(), line, "render → parse → render is the identity");
        // A backslash INSIDE quotes still escapes only `"` and `\`; a spaced
        // path is quoted with its backslashes doubled and comes back intact.
        let spaced = r"C:\Program Files\x y\tool.exe";
        assert_eq!(quote_word(spaced), r#""C:\\Program Files\\x y\\tool.exe""#);
        assert_eq!(shell_words(&quote_word(spaced)), vec![spaced]);
        assert_eq!(shell_words(r#""C:\Program Files\tool.exe" -v"#), vec![r"C:\Program Files\tool.exe", "-v"]);
        // The docker prefix is one spelling for the display line and the spawn.
        let tool = parse_command_line("docker exec -u dev -it -w /workspace/app dev-worker opencode").tool;
        assert_eq!(
            docker_exec_prefix(&tool.runtime, &tool.env),
            vec!["exec", "-u", "dev", "-it", "-w", "/workspace/app", "dev-worker"]
        );
        assert!(docker_exec_prefix(&Runtime::Host, &IndexMap::new()).is_empty());
    }

    #[test]
    fn claude_template_ships_the_alt_screen_env_visibly() {
        let t = AgentTool::template(ToolType::Claude);
        assert_eq!(t.env.get("CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN").map(String::as_str), Some("1"));
        assert_eq!(t.program, "claude");
        assert!(t.enabled);
        // Other templates carry no such default — it is claude's, not spawn's.
        assert!(AgentTool::template(ToolType::Opencode).env.is_empty());
        assert!(AgentTool::template(ToolType::Custom).program.is_empty());
    }

    #[test]
    fn store_round_trips_json_and_tolerates_unknown_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent_tools.json");
        let mut store = AgentToolStore::new(path.clone());
        assert!(store.list().is_empty(), "missing file = empty registry");
        let mut t = AgentTool::template(ToolType::Claude);
        t.name = "claude · host".into();
        t.model = Some("opus".into());
        t.max_busy = Some(2);
        let stored = store.upsert(t.clone()).unwrap();
        assert_eq!(stored.id, 1);
        let mut d = parse_command_line("docker exec -u dev -it -w /workspace/app dev-worker opencode -m gateway/model-fast").tool;
        if let Runtime::DockerExec { max_busy_in_container, .. } = &mut d.runtime {
            *max_busy_in_container = Some(1);
        }
        let stored2 = store.upsert(d.clone()).unwrap();
        assert_eq!(stored2.id, 2);

        // Reload: identical.
        let reopened = AgentToolStore::new(path.clone());
        assert_eq!(reopened.list(), store.list());
        assert_eq!(reopened.get(1).unwrap().env.get("CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN").unwrap(), "1");
        assert_eq!(reopened.next_id, 3);

        // Unknown fields (a future version's / a hand edit) are ignored; a
        // half-written entry degrades alone; ids are never reused.
        let text = std::fs::read_to_string(&path).unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
        value["tools"][0]["future_field"] = serde_json::json!({"x": 1});
        value["tools"].as_array_mut().unwrap().push(serde_json::json!({"id": 7, "name": "broken"}));
        value["next_id"] = serde_json::json!(1);
        std::fs::write(&path, value.to_string()).unwrap();
        let mut reopened = AgentToolStore::new(path.clone());
        assert_eq!(reopened.list().len(), 2, "the broken entry is pruned, the rest survive");
        assert_eq!(reopened.list()[0], store.list()[0]);
        let fresh = reopened.upsert(AgentTool { program: "x".into(), ..AgentTool::default() }).unwrap();
        assert_eq!(fresh.id, 3, "counter re-derived above the max stored id");

        // Delete + edit.
        reopened.delete(1).unwrap();
        assert!(reopened.get(1).is_none());
        assert!(reopened.delete(1).is_err());
        let mut edited = reopened.get(2).unwrap().clone();
        edited.enabled = false;
        reopened.upsert(edited).unwrap();
        assert!(!AgentToolStore::new(path).get(2).unwrap().enabled);

        // Corrupt file = empty, never an error.
        let corrupt = dir.path().join("corrupt.json");
        std::fs::write(&corrupt, "{not json").unwrap();
        assert!(AgentToolStore::new(corrupt).list().is_empty());
    }

    #[test]
    fn upsert_rejects_unusable_tools() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = AgentToolStore::new(dir.path().join("t.json"));
        assert!(store.upsert(AgentTool::default()).is_err(), "blank program");
        let mut t = AgentTool::template(ToolType::Claude);
        t.runtime = docker("", None, None, true);
        assert!(store.upsert(t).is_err(), "blank container");
        let mut t = AgentTool::template(ToolType::Claude);
        t.id = 99;
        assert!(store.upsert(t).is_err(), "unknown id");
    }

    #[test]
    fn delete_refusal_names_live_processes() {
        use crate::registry::AgentRow;
        let rows = vec![
            AgentRow { id: 4, name: "worker · build".into(), tool_id: 2, container: Some("dev-worker".into()), child_alive: true, last_output_at: Some(1), spawned_at_ms: 0, spawn_uuid: "u4".into() },
            AgentRow { id: 5, name: "old".into(), tool_id: 2, container: None, child_alive: false, last_output_at: None, spawned_at_ms: 0, spawn_uuid: "u5".into() },
            AgentRow { id: 6, name: "other".into(), tool_id: 3, container: None, child_alive: true, last_output_at: None, spawned_at_ms: 0, spawn_uuid: "u6".into() },
        ];
        let reason = delete_refusal(2, &rows).expect("refused");
        assert!(reason.contains("worker · build (process 4)"), "{reason}");
        assert!(!reason.contains("old"), "an exited agent does not block");
        assert!(delete_refusal(3, &rows).is_some());
        assert!(delete_refusal(9, &rows).is_none());
    }
}
