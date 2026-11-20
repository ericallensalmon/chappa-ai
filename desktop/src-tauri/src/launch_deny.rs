//! Deny rules an agent CLI enforces on itself, added at launch.
//!
//! An agent runs as the person, so nothing in the operating system keeps it
//! out of the app's own folders (the control token, settings and stores) or
//! out of the other projects the person has open. Some agent CLIs have a
//! permission system of their own; when the app launches one of those it
//! tells the CLI to refuse those places, so an agent whose permissions are
//! limited cannot read the person's token or change the app's state without
//! a prompt the person sees. It is a guard rail for a permission-limited
//! agent, not a boundary: an agent running with its permissions bypassed,
//! or a script it runs that opens files itself, is not stopped.
//!
//! Claude Code is the CLI with such a system today. Its permission rules
//! ([`deny_rules`]) are `Read(<path>)` and `Edit(<path>)` deny rules on
//! absolute paths (`//` + the path in POSIX form, a Windows drive letter
//! becoming `/c/...`); deny rules from every settings source apply before
//! any allow rule and in every permission mode, and Claude also applies
//! them to the shell commands it recognizes as file commands (`cat`,
//! `head`, `sed`, redirections). Its shell sandbox (macOS, Linux, WSL2; not
//! native Windows) adds the same paths to `sandbox.filesystem.denyRead` and
//! `denyWrite`, which only take effect when the person has turned that
//! sandbox on — the app does not turn it on, since it also limits network
//! access and writes.
//!
//! The rules travel in a settings file written for each launch
//! (`<launch dir>/<spawn uuid>.json`, owner-only, inside the app's data
//! folder so the rules deny the file itself) and named by the launch's one
//! `--settings` argument. A tool that already carries a `--settings` (the
//! app's session-hook file, or one of the person's own, inline JSON or a
//! path) has that settings object copied into the launch file with the
//! rules appended, and its argument pointed at the launch file; rules in it
//! that were anchored at its own folder are rewritten to absolute paths so
//! they keep matching what they matched. A tool the person took the hook's
//! `--settings` off gets a launch file with the rules and nothing else. The
//! file is removed when the row exits or closes (and any left by a crash
//! when the app starts).
//!
//! Other CLIs are reported as "not supported by this tool" ([`protection`]):
//! their documented permission settings either cannot be added for a
//! single launch without replacing the person's own choice, or let a later
//! (project) rule re-allow what an earlier one denied.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::agent_tools::{AgentTool, Runtime, ToolType, SETTINGS_FLAG};
use crate::registry::TermId;

/// The folder under the app's data folder that holds the launch files.
pub const LAUNCH_SUBDIR: &str = "agent-launch";

/// The tools whose path rules Claude Code matches against a path (a
/// `Write`, `Glob` or `Grep` rule is accepted but checked as `Edit`/`Read`).
const PATH_RULE_TOOLS: [&str; 6] = ["Read", "Edit", "Write", "NotebookEdit", "Glob", "Grep"];

// ---- which folders ------------------------------------------------------------

/// The folders a launched agent is denied: every folder the app keeps
/// private state in, plus every open project's folder and worktree that is
/// not the agent's own. An open folder that holds one of the agent's own
/// roots, or the folder it starts in, stays reachable (denying it would
/// deny the agent its own work); a project nested inside the agent's own
/// folder is still denied. Pure; `case_insensitive` as on Windows.
pub fn denied_dirs(
    private: &[PathBuf],
    open: &[PathBuf],
    own: &[PathBuf],
    cwd: Option<&Path>,
    case_insensitive: bool,
) -> Vec<PathBuf> {
    let inside =
        |root: &Path, child: &Path| crate::roots::same_or_inside(root, child, case_insensitive);
    let mut out: Vec<PathBuf> = Vec::new();
    let mut push = |p: &Path| {
        if !out.iter().any(|q| inside(q, p) && inside(p, q)) {
            out.push(p.to_path_buf());
        }
    };
    for p in private {
        push(p);
    }
    for p in open {
        let holds_own = own.iter().any(|o| inside(p, o));
        let holds_cwd = cwd.is_some_and(|c| inside(p, c));
        if !holds_own && !holds_cwd {
            push(p);
        }
    }
    out
}

// ---- rule text ----------------------------------------------------------------

/// `text` (an absolute path as the app shows it, Windows verbatim prefix
/// already stripped) as the absolute pattern Claude Code's path rules use:
/// `//` + the POSIX form, a drive letter `C:\x` becoming `//c/x`, with the
/// gitignore pattern characters escaped so the path matches only itself.
/// `None` for what cannot be written as one: a relative path, a network
/// (UNC) path, or a filesystem or drive root (a rule there would deny
/// everything).
pub fn rule_root(text: &str) -> Option<String> {
    if text.starts_with(r"\\") || text.starts_with("//") {
        return None;
    }
    let bytes = text.as_bytes();
    let posix = if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        let rest = &text[2..];
        if !(rest.is_empty() || rest.starts_with('\\') || rest.starts_with('/')) {
            return None;
        }
        let drive = (bytes[0] as char).to_ascii_lowercase();
        format!(
            "/{drive}{}",
            escape_pattern(&rest.replace('\\', "/"), false)
        )
    } else if text.starts_with('/') {
        escape_pattern(text, true)
    } else {
        return None;
    };
    let trimmed = posix.trim_end_matches('/');
    // At least one folder below the root (a drive's `/c` alone is a root).
    let depth = trimmed.split('/').filter(|s| !s.is_empty()).count();
    let needed = if bytes.get(1) == Some(&b':') { 2 } else { 1 };
    if depth < needed {
        return None;
    }
    Some(format!("/{trimmed}"))
}

/// Backslash-escape the gitignore pattern characters; a backslash in a
/// POSIX file name is escaped too (on Windows it is a separator, already
/// turned into `/`).
fn escape_pattern(text: &str, escape_backslash: bool) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '*' | '?' | '[' | ']') || (escape_backslash && c == '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The deny rules for `paths` (as the app shows them): `Read` and `Edit` of
/// the folder and of everything under it. `Read` also refuses Claude's
/// search tools there and its writes; `Edit` covers every built-in tool
/// that changes files.
pub fn deny_rules(paths: &[String]) -> Vec<String> {
    let mut rules = Vec::new();
    for root in paths.iter().filter_map(|p| rule_root(p)) {
        for tool in ["Read", "Edit"] {
            for rule in [format!("{tool}({root})"), format!("{tool}({root}/**)")] {
                if !rules.contains(&rule) {
                    rules.push(rule);
                }
            }
        }
    }
    rules
}

/// The shell sandbox's deny list for `paths`: plain absolute POSIX paths
/// (the sandbox runs on macOS, Linux and WSL2 only, so a Windows path has
/// no entry).
pub fn sandbox_paths(paths: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in paths {
        if p.starts_with('/') && !p.starts_with("//") && p.trim_end_matches('/').len() > 1 {
            let p = p.trim_end_matches('/').to_owned();
            if !out.contains(&p) {
                out.push(p);
            }
        }
    }
    out
}

// ---- the settings object ------------------------------------------------------

/// One `--settings` on a command line: where it is and what it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsArg {
    /// The index of the flag word.
    pub index: usize,
    /// `--settings=<value>` (one word) rather than `--settings <value>`.
    pub joined: bool,
    /// The value; `None` when the flag is the last word.
    pub value: Option<String>,
}

/// Every `--settings` among `args`, up to `--` (after it every word is the
/// prompt, not an option).
pub fn settings_args(args: &[String]) -> Vec<SettingsArg> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            break;
        }
        if a == SETTINGS_FLAG {
            out.push(SettingsArg {
                index: i,
                joined: false,
                value: args.get(i + 1).cloned(),
            });
            i += 2;
            continue;
        }
        if let Some(value) = a
            .strip_prefix(SETTINGS_FLAG)
            .and_then(|rest| rest.strip_prefix('='))
        {
            out.push(SettingsArg {
                index: i,
                joined: true,
                value: Some(value.to_owned()),
            });
        }
        i += 1;
    }
    out
}

/// Whether a `--settings` value is inline JSON rather than a file path.
fn is_inline(value: &str) -> bool {
    value.trim_start().starts_with('{')
}

/// Rewrite the rules and paths in `settings` that were relative to the
/// folder it was read from (`anchor`), so they match the same places from
/// the launch file's folder: a path rule `Tool(/x)` (single slash = the
/// settings file's folder) becomes `Tool(//<anchor>/x)`, and a relative
/// sandbox filesystem path or additional directory is joined onto `anchor`.
pub fn rebase(settings: &mut Value, anchor: &Path) {
    let anchor_text = crate::roots::display(anchor);
    let Some(anchor_rule) = rule_root(&anchor_text) else {
        return;
    };
    if let Some(perms) = settings
        .get_mut("permissions")
        .and_then(Value::as_object_mut)
    {
        for list in ["allow", "deny", "ask"] {
            if let Some(rules) = perms.get_mut(list).and_then(Value::as_array_mut) {
                for rule in rules.iter_mut() {
                    if let Some(text) = rule.as_str() {
                        if let Some(rebased) = rebase_rule(text, &anchor_rule) {
                            *rule = Value::String(rebased);
                        }
                    }
                }
            }
        }
        if let Some(dirs) = perms
            .get_mut("additionalDirectories")
            .and_then(Value::as_array_mut)
        {
            rebase_paths(dirs, anchor);
        }
    }
    if let Some(fs) = settings
        .get_mut("sandbox")
        .and_then(|s| s.get_mut("filesystem"))
        .and_then(Value::as_object_mut)
    {
        for (_, list) in fs.iter_mut() {
            if let Some(paths) = list.as_array_mut() {
                rebase_paths(paths, anchor);
            }
        }
    }
}

fn rebase_rule(rule: &str, anchor_rule: &str) -> Option<String> {
    let open = rule.find('(')?;
    let (tool, rest) = rule.split_at(open);
    if !PATH_RULE_TOOLS.contains(&tool) || !rule.ends_with(')') {
        return None;
    }
    let spec = &rest[1..rest.len() - 1];
    if !spec.starts_with('/') || spec.starts_with("//") {
        return None;
    }
    Some(format!("{tool}({anchor_rule}{spec})"))
}

fn rebase_paths(paths: &mut [Value], anchor: &Path) {
    for p in paths.iter_mut() {
        if let Some(text) = p.as_str() {
            let absolute =
                text.starts_with('/') || text.starts_with('~') || Path::new(text).is_absolute();
            if !absolute {
                let joined = anchor.join(text.trim_start_matches("./"));
                *p = Value::String(crate::roots::display(&joined));
            }
        }
    }
}

/// `base` (a settings object, `{}` for none) with `rules` appended to
/// `permissions.deny` and `sandbox` appended to
/// `sandbox.filesystem.denyRead` and `denyWrite`. Every other key is kept
/// as it was. A settings value that is not an object, or a `permissions` /
/// `deny` of the wrong shape, is refused rather than overwritten.
pub fn merge_denies(
    mut base: Value,
    rules: &[String],
    sandbox: &[String],
) -> Result<Value, String> {
    let root = base
        .as_object_mut()
        .ok_or("the tool's --settings is not a JSON object")?;
    let perms = object_at(root, "permissions")?;
    append(perms, "deny", rules)?;
    if !sandbox.is_empty() {
        let sb = object_at(root, "sandbox")?;
        let fs = object_at(sb, "filesystem")?;
        append(fs, "denyRead", sandbox)?;
        append(fs, "denyWrite", sandbox)?;
    }
    Ok(base)
}

fn object_at<'a>(
    map: &'a mut Map<String, Value>,
    key: &str,
) -> Result<&'a mut Map<String, Value>, String> {
    map.entry(key.to_owned())
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| format!("the tool's --settings has a `{key}` that is not an object"))
}

fn append(map: &mut Map<String, Value>, key: &str, items: &[String]) -> Result<(), String> {
    let list = map
        .entry(key.to_owned())
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or_else(|| format!("the tool's --settings has a `{key}` that is not a list"))?;
    for item in items {
        if !list.iter().any(|v| v.as_str() == Some(item)) {
            list.push(Value::String(item.clone()));
        }
    }
    Ok(())
}

/// `args` with their one `--settings` pointing at `file`, or with
/// `--settings <file>` put first when there is none. More than one
/// `--settings` is refused: which of them the CLI would keep is not
/// documented, so the launch file could be the one dropped.
pub fn launch_args(args: &[String], file: &str) -> Result<Vec<String>, String> {
    let found = settings_args(args);
    if found.len() > 1 {
        return Err(more_than_one_settings());
    }
    let mut out = args.to_vec();
    match found.first() {
        None => {
            out.splice(0..0, [SETTINGS_FLAG.to_owned(), file.to_owned()]);
        }
        Some(arg) if arg.joined => out[arg.index] = format!("{SETTINGS_FLAG}={file}"),
        Some(arg) if arg.value.is_some() => out[arg.index + 1] = file.to_owned(),
        Some(_) => {
            return Err(format!(
                "{SETTINGS_FLAG} is the last word and names nothing"
            ))
        }
    }
    Ok(out)
}

fn more_than_one_settings() -> String {
    format!(
        "the command line carries more than one {SETTINGS_FLAG}; the app adds its deny rules to the \
         one settings object a launch has, so merge them into one"
    )
}

/// The settings object a `--settings` value names and the folder its
/// relative rules are anchored at: the parsed JSON and `cwd` for inline
/// JSON, the file's content and its folder for a path (relative to `cwd`).
fn load_base(value: &str, cwd: &Path) -> Result<(Value, PathBuf), String> {
    if is_inline(value) {
        let parsed = serde_json::from_str(value)
            .map_err(|e| format!("the tool's inline {SETTINGS_FLAG} is not valid JSON: {e}"))?;
        return Ok((parsed, cwd.to_path_buf()));
    }
    let path = cwd.join(value);
    let text = std::fs::read_to_string(&path).map_err(|e| {
        format!(
            "cannot read the tool's settings file {} to add the app's deny rules: {e}",
            path.display()
        )
    })?;
    let parsed = serde_json::from_str(&text).map_err(|e| {
        format!(
            "the tool's settings file {} is not valid JSON, so the app cannot add its deny rules: {e}",
            path.display()
        )
    })?;
    let anchor = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| cwd.to_path_buf());
    Ok((parsed, anchor))
}

/// The folder as the app shows it, plus where it resolves to when that
/// differs (a symlinked folder): a rule on either keeps the other out.
fn path_texts(dirs: &[PathBuf]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for d in dirs {
        for text in [
            crate::roots::display(d),
            crate::roots::display(&crate::roots::resolve_best_effort(d)),
        ] {
            if !out.contains(&text) {
                out.push(text);
            }
        }
    }
    out
}

/// Write the launch file for `args` and return the args that name it. The
/// file holds the tool's own settings object (if it has one) with the deny
/// rules for `denied` appended. Fails closed: a tool's own settings that
/// cannot be read or merged refuses the launch rather than starting it
/// without the rules.
pub fn prepare_launch(
    args: &[String],
    denied: &[PathBuf],
    cwd: Option<&Path>,
    file: &Path,
) -> Result<Vec<String>, String> {
    let found = settings_args(args);
    if found.len() > 1 {
        return Err(more_than_one_settings());
    }
    let cwd: PathBuf = match cwd {
        Some(c) => c.to_path_buf(),
        None => std::env::current_dir().unwrap_or_default(),
    };
    let mut base = json!({});
    if let Some(arg) = found.first() {
        let value = arg
            .value
            .as_deref()
            .ok_or_else(|| format!("{SETTINGS_FLAG} is the last word and names nothing"))?;
        let (loaded, anchor) = load_base(value, &cwd)?;
        base = loaded;
        rebase(&mut base, &anchor);
    }
    let texts = path_texts(denied);
    let merged = merge_denies(base, &deny_rules(&texts), &sandbox_paths(&texts))?;
    let file_arg = file
        .to_str()
        .ok_or("the app's data folder is not a valid UTF-8 path")?
        .to_owned();
    if let Some(dir) = file.parent() {
        project_model::atomic::ensure_private_dir(dir)?;
    }
    let text = serde_json::to_string_pretty(&merged).map_err(|e| e.to_string())?;
    project_model::atomic::write_private_text_atomic(file, &text, "launch.tmp")?;
    launch_args(args, &file_arg)
}

// ---- which tools --------------------------------------------------------------

/// Whether the app adds deny rules when it launches `tool`: a Claude Code
/// tool running on this machine (a container's CLI sees its own
/// filesystem, where these paths mean nothing).
pub fn applies(tool: &AgentTool) -> bool {
    tool.tool_type == ToolType::Claude && matches!(tool.runtime, Runtime::Host)
}

/// The flag among `args` (up to `--`) that starts Claude Code with its
/// permission prompts bypassed, or able to switch to that mode.
pub fn bypass_flag(args: &[String]) -> Option<String> {
    mode_flag(
        args,
        |mode| mode == "bypassPermissions",
        &[
            "--dangerously-skip-permissions",
            "--allow-dangerously-skip-permissions",
        ],
    )
}

/// The flag among `args` that starts Claude Code in auto mode, where a
/// classifier rather than the person approves shell commands.
pub fn auto_flag(args: &[String]) -> Option<String> {
    mode_flag(args, |mode| mode == "auto", &[])
}

fn mode_flag(
    args: &[String],
    mode_matches: impl Fn(&str) -> bool,
    bare: &[&str],
) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if a == "--" {
            return None;
        }
        if bare.contains(&a) {
            return Some(a.to_owned());
        }
        if a == "--permission-mode" {
            if let Some(mode) = args.get(i + 1).filter(|m| mode_matches(m)) {
                return Some(format!("--permission-mode {mode}"));
            }
            i += 2;
            continue;
        }
        if let Some(mode) = a.strip_prefix("--permission-mode=") {
            if mode_matches(mode) {
                return Some(a.to_owned());
            }
        }
        i += 1;
    }
    None
}

/// What Settings ▸ Agents says about a tool's launch protection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtectionKind {
    /// The CLI's own permissions refuse the denied folders.
    Protected,
    /// The CLI runs (or may run) with its permission prompts bypassed.
    Bypass,
    /// The CLI's classifier approves shell commands instead of the person.
    Auto,
    /// The CLI has no deny mechanism the app can add at launch.
    Unsupported,
    /// The CLI runs in a container; host paths mean nothing there.
    Container,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolProtection {
    pub kind: ProtectionKind,
    /// The short line shown on the tool's row.
    pub label: String,
    /// What it covers and what it does not.
    pub detail: String,
}

const PROTECTED_DETAIL: &str = "At launch the app adds deny rules so the CLI's file tools refuse the \
app's own folders (control token, settings, stores) and the other open projects. Shell commands are \
covered only where the CLI recognizes the file a command names (cat, head, sed, redirections), and \
for the commands its own shell sandbox runs when you have turned it on (macOS, Linux and WSL2, not \
native Windows); a script that opens files itself outside that sandbox is not stopped, and an allow rule that lets the shell run without asking lets it \
read these folders. A guard rail for an agent whose permissions are limited, not a boundary.";

/// The protection `tool` gets at launch. `own_settings` is the settings
/// object its own `--settings` names, when it could be read: a
/// `permissions.defaultMode` there decides the mode as a flag would.
pub fn protection_of(tool: &AgentTool, own_settings: Option<&Value>) -> ToolProtection {
    let unsupported = |why: &str| {
        ToolProtection {
        kind: ProtectionKind::Unsupported,
        label: "not supported by this tool".into(),
        detail: format!(
            "{why} Nothing keeps this agent out of the app's folders or other projects except running \
             it in a container that does not mount them."
        ),
    }
    };
    match tool.tool_type {
        ToolType::Claude => {}
        ToolType::Codex => {
            return unsupported(
                "Codex's permission profiles can deny paths, but a launch can only add one by replacing \
                 the profile or sandbox mode you chose, so the app does not add one.",
            )
        }
        ToolType::Opencode => {
            return unsupported(
                "opencode's permission rules let the last matching rule win, so a project's own \
                 config can allow again what a launch denies.",
            )
        }
        _ => return unsupported("The app knows no deny mechanism this CLI documents."),
    }
    if tool.runtime.is_docker() {
        return ToolProtection {
            kind: ProtectionKind::Container,
            label: "not added — runs in a container".into(),
            detail: "The container sees only the folders mounted into it: mount neither the app's \
                     folders nor other projects."
                .into(),
        };
    }
    let mode = own_settings
        .and_then(|s| s.pointer("/permissions/defaultMode"))
        .and_then(Value::as_str);
    let any_args = if tool.agent_args_any {
        " Agents may pass any arguments to this tool, so an agent can also start it with its \
         permissions bypassed."
    } else {
        ""
    };
    let bypass = bypass_flag(&tool.args).or_else(|| {
        (mode == Some("bypassPermissions")).then(|| "defaultMode bypassPermissions".to_owned())
    });
    if let Some(flag) = bypass {
        return ToolProtection {
            kind: ProtectionKind::Bypass,
            label: "not protected — the tool runs with permissions bypassed".into(),
            detail: format!(
                "{flag} lets its shell commands run without asking you, so a command can read the \
                 app's folders and other projects. The deny rules are still added; the CLI's own file \
                 tools keep refusing those folders."
            ),
        };
    }
    let auto = auto_flag(&tool.args)
        .or_else(|| (mode == Some("auto")).then(|| "defaultMode auto".to_owned()));
    if let Some(flag) = auto {
        return ToolProtection {
            kind: ProtectionKind::Auto,
            label: "not protected — the tool approves its own shell commands".into(),
            detail: format!(
                "{flag} lets a classifier, not you, approve shell commands, so a command can read the \
                 app's folders without a prompt you see. The deny rules are still added.{any_args}"
            ),
        };
    }
    ToolProtection {
        kind: ProtectionKind::Protected,
        label: "protected by the tool's permissions".into(),
        detail: format!("{PROTECTED_DETAIL}{any_args}"),
    }
}

/// [`protection_of`] with the tool's own `--settings` read from disk when
/// it names a file (relative paths against `cwd`); unreadable = unknown.
pub fn protection(tool: &AgentTool, cwd: &Path) -> ToolProtection {
    let own = settings_args(&tool.args)
        .first()
        .and_then(|a| a.value.clone())
        .and_then(|v| load_base(&v, cwd).ok())
        .map(|(v, _)| v);
    protection_of(tool, own.as_ref())
}

// ---- the files of live launches -----------------------------------------------

/// Where the private folders are and which launch file each live row has.
/// Unconfigured (tests, before setup) = no rules are added.
#[derive(Debug, Clone, Default)]
pub struct LaunchFiles {
    inner: Arc<Mutex<FilesInner>>,
}

#[derive(Debug, Default)]
struct FilesInner {
    config: Option<LaunchConfig>,
    files: HashMap<TermId, PathBuf>,
}

/// The folders every launch is denied and where launch files go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchConfig {
    pub private: Vec<PathBuf>,
    pub launch_dir: PathBuf,
}

impl LaunchFiles {
    /// Set the private folders and the launch folder, and remove the files
    /// a previous run left (no row of this run uses them). The launch
    /// folder is added to the private folders when none holds it, so the
    /// rules always deny the launch file itself.
    pub fn configure(&self, mut private: Vec<PathBuf>, launch_dir: PathBuf) {
        let case_insensitive = cfg!(windows);
        if !private
            .iter()
            .any(|p| crate::roots::same_or_inside(p, &launch_dir, case_insensitive))
        {
            private.push(launch_dir.clone());
        }
        if let Ok(entries) = std::fs::read_dir(&launch_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "json") {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
        if let Ok(mut inner) = self.inner.lock() {
            inner.config = Some(LaunchConfig {
                private,
                launch_dir,
            });
        }
    }

    pub fn config(&self) -> Option<LaunchConfig> {
        self.inner.lock().ok().and_then(|i| i.config.clone())
    }

    /// The launch file for the spawn `uuid`.
    pub fn file_for(config: &LaunchConfig, uuid: &str) -> PathBuf {
        config.launch_dir.join(format!("{uuid}.json"))
    }

    pub fn remember(&self, id: TermId, file: PathBuf) {
        if let Ok(mut inner) = self.inner.lock() {
            if let Some(old) = inner.files.insert(id, file) {
                let _ = std::fs::remove_file(old);
            }
        }
    }

    /// Remove row `id`'s launch file (its run is over, or never started).
    pub fn forget(&self, id: TermId) {
        let file = self.inner.lock().ok().and_then(|mut i| i.files.remove(&id));
        if let Some(file) = file {
            let _ = std::fs::remove_file(file);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|w| (*w).to_owned()).collect()
    }

    /// Whether a `Read(//x/**)`-style deny rule among `rules` covers `path`
    /// (in POSIX rule form), the way an anchored gitignore pattern does.
    fn covered(rules: &[String], tool: &str, path: &str) -> bool {
        let target = rule_root(path).unwrap();
        rules.iter().any(|r| {
            r.strip_prefix(&format!("{tool}("))
                .and_then(|r| r.strip_suffix("/**)"))
                .is_some_and(|root| target.starts_with(&format!("{root}/")))
        })
    }

    #[test]
    fn rule_roots_are_absolute_posix_patterns() {
        assert_eq!(rule_root("/home/a/proj").as_deref(), Some("//home/a/proj"));
        assert_eq!(rule_root("/home/a/proj/").as_deref(), Some("//home/a/proj"));
        assert_eq!(
            rule_root(r"C:\Users\a\AppData\Roaming\app").as_deref(),
            Some("//c/Users/a/AppData/Roaming/app")
        );
        assert_eq!(rule_root("D:/work/x").as_deref(), Some("//d/work/x"));
        assert_eq!(
            rule_root("/home/a/[old] *x?").as_deref(),
            Some(r"//home/a/\[old\] \*x\?")
        );
        // Nothing that would deny a whole drive or filesystem, and nothing
        // relative or on the network.
        for none in [
            "/",
            r"C:\",
            "C:",
            "relative/x",
            r"\\server\share\x",
            "//server/x",
            "C:x",
        ] {
            assert_eq!(rule_root(none), None, "{none}");
        }
    }

    #[test]
    fn the_launch_denies_the_app_folders_and_other_projects_but_not_its_own() {
        let private = vec![PathBuf::from("/cfg/app"), PathBuf::from("/data/app")];
        let open = vec![
            PathBuf::from("/work/own"),
            PathBuf::from("/work/own-wt"),
            PathBuf::from("/work/other"),
            PathBuf::from("/work/other-wt"),
        ];
        let own = vec![PathBuf::from("/work/own"), PathBuf::from("/work/own-wt")];
        let denied = denied_dirs(&private, &open, &own, Some(Path::new("/work/own")), false);
        let texts: Vec<String> = denied.iter().map(|p| p.display().to_string()).collect();
        let rules = deny_rules(&texts);
        for path in [
            "/cfg/app/control_token",
            "/data/app/x.json",
            "/work/other/src/a",
            "/work/other-wt/b",
        ] {
            assert!(covered(&rules, "Read", path), "Read {path}: {rules:#?}");
            assert!(covered(&rules, "Edit", path), "Edit {path}: {rules:#?}");
        }
        for path in ["/work/own/src/a", "/work/own-wt/b"] {
            assert!(
                !covered(&rules, "Read", path),
                "own {path} denied: {rules:#?}"
            );
            assert!(
                !covered(&rules, "Edit", path),
                "own {path} denied: {rules:#?}"
            );
        }
        assert_eq!(sandbox_paths(&texts), texts);
    }

    #[test]
    fn a_folder_holding_the_agents_own_work_stays_reachable_and_a_nested_project_does_not() {
        let open = vec![
            PathBuf::from("/work"),
            PathBuf::from("/work/own"),
            PathBuf::from("/work/own/vendor/other"),
        ];
        let own = vec![PathBuf::from("/work/own")];
        let denied = denied_dirs(&[], &open, &own, Some(Path::new("/work/own")), false);
        assert_eq!(denied, vec![PathBuf::from("/work/own/vendor/other")]);
        // On Windows a differently cased spelling is the same folder.
        let denied = denied_dirs(
            &[],
            &[PathBuf::from(r"C:\Work\Own")],
            &[PathBuf::from(r"c:\work\own")],
            None,
            true,
        );
        assert!(denied.is_empty(), "{denied:?}");
    }

    #[test]
    fn the_launch_file_itself_is_denied() {
        let dir = tempfile::tempdir().unwrap();
        let files = LaunchFiles::default();
        let data = dir.path().join("data");
        files.configure(
            vec![dir.path().join("cfg"), data.clone()],
            data.join(LAUNCH_SUBDIR),
        );
        let config = files.config().unwrap();
        let file = LaunchFiles::file_for(&config, "uuid-1");
        let denied = denied_dirs(&config.private, &[], &[], None, cfg!(windows));
        let args = prepare_launch(&[], &denied, Some(dir.path()), &file).unwrap();
        assert_eq!(
            args,
            vec![SETTINGS_FLAG.to_owned(), file.to_str().unwrap().to_owned()]
        );
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        let rules: Vec<String> = written["permissions"]["deny"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect();
        let file_text = crate::roots::display(&crate::roots::resolve_best_effort(&file));
        assert!(
            covered(&rules, "Edit", &file_text),
            "{file_text}: {rules:#?}"
        );
        assert!(
            covered(&rules, "Read", &file_text),
            "{file_text}: {rules:#?}"
        );
        // A launch dir outside every private folder is added to them.
        let other = LaunchFiles::default();
        other.configure(vec![dir.path().join("cfg")], dir.path().join("elsewhere"));
        assert!(other
            .config()
            .unwrap()
            .private
            .contains(&dir.path().join("elsewhere")));
        // The row's exit removes it.
        files.remember(7, file.clone());
        files.forget(7);
        assert!(!file.exists());
    }

    #[cfg(unix)]
    #[test]
    fn the_launch_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(LAUNCH_SUBDIR).join("u.json");
        prepare_launch(&[], &[PathBuf::from("/cfg/app")], Some(dir.path()), &file).unwrap();
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let mode = std::fs::metadata(file.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn deny_rules_survive_the_tools_own_settings_argument() {
        let dir = tempfile::tempdir().unwrap();
        let own = dir.path().join("mine");
        std::fs::create_dir_all(&own).unwrap();
        let own_file = own.join("settings.json");
        std::fs::write(
            &own_file,
            r#"{"model": "m", "hooks": {"SessionStart": []},
                "permissions": {"allow": ["Bash(git log *)", "Read"], "deny": ["Read(/secrets/**)", "Bash(/bin/rm *)"]}}"#,
        )
        .unwrap();
        let file = dir.path().join(LAUNCH_SUBDIR).join("u.json");
        let denied = vec![PathBuf::from("/cfg/app")];
        for args in [
            s(&["--settings", own_file.to_str().unwrap(), "--model", "x"]),
            vec![
                format!("--settings={}", own_file.display()),
                "--model".into(),
                "x".into(),
            ],
        ] {
            let out = prepare_launch(&args, &denied, Some(dir.path()), &file).unwrap();
            // One --settings, the launch file's, and the rest untouched.
            let found = settings_args(&out);
            assert_eq!(found.len(), 1, "{out:?}");
            assert_eq!(found[0].value.as_deref(), file.to_str());
            assert_eq!(&out[out.len() - 2..], &s(&["--model", "x"])[..]);
            let written: Value =
                serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
            assert_eq!(written["model"], "m");
            assert!(written["hooks"]["SessionStart"].is_array());
            let deny: Vec<&str> = written["permissions"]["deny"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            assert!(deny.contains(&"Read(//cfg/app/**)"), "{deny:?}");
            assert!(deny.contains(&"Edit(//cfg/app/**)"), "{deny:?}");
            // The person's own rule anchored at their file's folder still
            // names that folder; a shell rule is left as written.
            let anchor = rule_root(&crate::roots::display(&own)).unwrap();
            assert!(
                deny.contains(&format!("Read({anchor}/secrets/**)").as_str()),
                "{deny:?}"
            );
            assert!(deny.contains(&"Bash(/bin/rm *)"), "{deny:?}");
            assert_eq!(
                written["permissions"]["allow"],
                json!(["Bash(git log *)", "Read"])
            );
        }
        // Inline JSON is merged the same way.
        let out = prepare_launch(
            &s(&[
                "--settings",
                r#"{"permissions": {"deny": ["Bash(curl *)"]}}"#,
            ]),
            &denied,
            Some(dir.path()),
            &file,
        )
        .unwrap();
        assert_eq!(settings_args(&out)[0].value.as_deref(), file.to_str());
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(written["permissions"]["deny"][0], "Bash(curl *)");
        assert!(written["permissions"]["deny"]
            .as_array()
            .unwrap()
            .contains(&json!("Read(//cfg/app)")));
    }

    #[test]
    fn a_second_settings_argument_or_an_unreadable_one_refuses_the_launch() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("u.json");
        let denied = vec![PathBuf::from("/cfg/app")];
        // Which of two the CLI keeps is not documented: refused, not guessed.
        let two = s(&["--settings", "{}", "-p", "--settings={}"]);
        assert!(prepare_launch(&two, &denied, Some(dir.path()), &file).is_err());
        assert!(launch_args(&two, "f").is_err());
        // Its own settings cannot be read: no launch without the rules.
        let missing = s(&["--settings", "nope.json"]);
        assert!(prepare_launch(&missing, &denied, Some(dir.path()), &file).is_err());
        let broken = s(&["--settings", "{not json"]);
        assert!(prepare_launch(&broken, &denied, Some(dir.path()), &file).is_err());
        let wrong_shape = s(&["--settings", r#"{"permissions": {"deny": "Read"}}"#]);
        assert!(prepare_launch(&wrong_shape, &denied, Some(dir.path()), &file).is_err());
        assert!(!file.exists());
        // A `--settings` in the prompt (after `--`) is the prompt.
        let prompt = s(&["--", "--settings x"]);
        assert_eq!(settings_args(&prompt), vec![]);
        assert_eq!(
            launch_args(&prompt, "f").unwrap(),
            s(&["--settings", "f", "--", "--settings x"])
        );
    }

    #[test]
    fn a_bypass_flag_tool_reports_not_protected() {
        let mut tool = AgentTool::template(ToolType::Claude);
        assert_eq!(protection_of(&tool, None).kind, ProtectionKind::Protected);
        for args in [
            s(&["--dangerously-skip-permissions"]),
            s(&["--allow-dangerously-skip-permissions"]),
            s(&["--permission-mode", "bypassPermissions"]),
            s(&["--model", "x", "--permission-mode=bypassPermissions"]),
        ] {
            tool.args = args.clone();
            let p = protection_of(&tool, None);
            assert_eq!(p.kind, ProtectionKind::Bypass, "{args:?}");
            assert!(p.label.starts_with("not protected"), "{}", p.label);
        }
        // The person's own settings can bypass too.
        tool.args = Vec::new();
        let own = json!({"permissions": {"defaultMode": "bypassPermissions"}});
        assert_eq!(
            protection_of(&tool, Some(&own)).kind,
            ProtectionKind::Bypass
        );
        let own = json!({"permissions": {"defaultMode": "auto"}});
        assert_eq!(protection_of(&tool, Some(&own)).kind, ProtectionKind::Auto);
        tool.args = s(&["--permission-mode", "auto"]);
        assert_eq!(protection_of(&tool, None).kind, ProtectionKind::Auto);
        // The words of a prompt are not flags; other modes keep the rules'
        // protection.
        tool.args = s(&[
            "--permission-mode",
            "acceptEdits",
            "--",
            "--dangerously-skip-permissions",
        ]);
        assert_eq!(protection_of(&tool, None).kind, ProtectionKind::Protected);
    }

    #[test]
    fn a_tool_without_support_reports_not_supported() {
        for t in [
            ToolType::Codex,
            ToolType::Opencode,
            ToolType::Gemini,
            ToolType::Copilot,
            ToolType::Kimi,
            ToolType::Amp,
            ToolType::Custom,
        ] {
            let tool = AgentTool::template(t);
            assert!(!applies(&tool), "{t:?}");
            let p = protection_of(&tool, None);
            assert_eq!(p.kind, ProtectionKind::Unsupported, "{t:?}");
            assert_eq!(p.label, "not supported by this tool");
        }
        let mut docker = AgentTool::template(ToolType::Claude);
        docker.runtime = Runtime::DockerExec {
            container: "c".into(),
            user: None,
            workdir: None,
            tty: true,
            max_busy_in_container: None,
        };
        assert!(!applies(&docker));
        assert_eq!(protection_of(&docker, None).kind, ProtectionKind::Container);
        assert!(applies(&AgentTool::template(ToolType::Claude)));
    }
}
