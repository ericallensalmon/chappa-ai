//! The `script` driver: a component, or a whole existing stack, run by the
//! project's own scripts.
//!
//! Anything the other drivers cannot express plugs in here: a stack that
//! already has its bring-up scripts keeps them, and the runner calls them
//! per verb. Every verb maps to one script when the definition names one,
//! and a verb with no script is simply not offered for that component.
//!
//! # Definition
//!
//! ```yaml
//! components:
//!   stack:
//!     driver: script
//!     up:      [node, scripts/up.js]          # required
//!     down:    [node, scripts/down.js]
//!     stop:    [node, scripts/stop.js]
//!     start:   [node, scripts/start.js]
//!     restart: [node, scripts/restart.js]
//!     rebuild: [node, scripts/rebuild.js]
//!     status:  [node, scripts/status.js]      # prints the status JSON below
//!     plan:    [node, scripts/plan.js]        # run as `plan <verb>`; prints what <verb> would do
//!     cwd: scripts/..                         # project-relative; default the project folder
//!     env: { API_URL: "http://{hostname}:{port:stack}" }   # values may use placeholders
//!     timeouts: { up: 30m, status: 20s }      # per verb; s, m, h or d
//!     repos: [../api, ../web]                 # shared clones the scripts build from
//! ```
//!
//! | verb | default limit | desired state after it |
//! |---|---|---|
//! | `up` / `start` / `restart` | 15 min | running |
//! | `rebuild` | 60 min | running |
//! | `stop` | 5 min | stopped |
//! | `down` | 5 min | absent |
//! | `status` | 20 s | (none; it only reports) |
//! | `plan` | 60 s | (none; it only describes) |
//!
//! `up` runs every time it is asked for (stack-wide or on the component):
//! the runner cannot see inside the script, so the script itself decides
//! what is already done. A stack-wide `stop`, `start`, `down` or `rebuild`
//! skips a script component that has no script for that verb and says so in
//! its confirm.
//!
//! # Argv only, never a shell
//!
//! Each script is an argv list, spawned as is; no shell ever parses it, so
//! a value with spaces stays one argument and nothing in it is expanded. A
//! relative program path (`scripts/up.cmd`) is taken from `cwd`. Two
//! platform rules follow:
//!
//! - **PowerShell** files cannot be run directly: name the interpreter,
//!   `[powershell, -NoProfile, -File, scripts/up.ps1]` on Windows (or
//!   `[pwsh, -NoProfile, -File, scripts/up.ps1]` where PowerShell 7 is
//!   installed). A `.ps1` as the first word is a validation warning and is
//!   refused when it would run.
//! - **Batch files** (`.cmd` / `.bat`) run through `cmd /C`, and then no
//!   argument may carry a character `cmd` would interpret (`& | < > ^ % "`):
//!   such a call is refused rather than escaped.
//!
//! Windows:
//!
//! ```yaml
//! up:   [powershell, -NoProfile, -ExecutionPolicy, Bypass, -File, scripts/up.ps1, -Slot, "{slot}"]
//! stop: [scripts\stop.cmd, "{instance}"]
//! ```
//!
//! Unix:
//!
//! ```yaml
//! up:   [bash, scripts/up.sh, "{slot}"]
//! stop: [./scripts/stop.sh, "{instance}"]
//! ```
//!
//! # Placeholders and environment
//!
//! Every word of a script, and every `env:` value, may use the usual
//! placeholders (`{slot}`, `{instance}`, `{port:<component>}`, `{hostname}`
//! …) and, for the work item bound to the slot, `{item}`,
//! `{branch:<repo>}` and `{worktree:<repo>}`. Resolving one of those three
//! with no item bound is an error for that verb, shown in its confirm before
//! anything runs. Each script also gets, besides the instance variables
//! every process gets (`CHAPPA_STACK`, `CHAPPA_STACK_SLOT`,
//! `CHAPPA_STACK_INSTANCE`, ports, outputs):
//!
//! | variable | value |
//! |---|---|
//! | `CHAPPA_STACK_ITEM` | the bound item's key, empty when none |
//! | `CHAPPA_STACK_WORKTREE_<REPO>` | one per worktree of the bound item (`<REPO>` upper-cased, `[A-Z0-9_]`) |
//!
//! That is how an existing whole-stack script builds from the active
//! item's worktrees: it reads `CHAPPA_STACK_WORKTREE_API` instead of a
//! hard-coded checkout.
//!
//! # Status JSON
//!
//! `status` prints, on stdout (at most 1 MiB):
//!
//! ```json
//! { "groups": [{"id": "core", "label": "Core"}, {"id": "apis", "label": "APIs"}],
//!   "components": [ { "name": "api", "group": "apis", "state": "up", "since": "2026-11-15T09:00:00Z",
//!                     "exitCode": 0, "detail": "…", "links": [{"label": "API", "url": "http://…"}] } ],
//!   "facts":  [{"label": "Built from", "value": "main @ 1a2b3c4d"}],
//!   "alerts": [{"tone": "warn", "text": "…"}] }
//! ```
//!
//! `state` is one of the nine Environment states (`up`, `starting`,
//! `unhealthy`, `crashloop`, `stopped-problem`, `stopped-idle`,
//! `oneshot-ok`, `oneshot-failed`, `not-deployed`); `tone` is `warn`, `bad`
//! or `info`. Everything else of a component is optional and fills one
//! column of its row:
//!
//! | field | shape | shown as |
//! |---|---|---|
//! | `group` | a group id | the group it sits in |
//! | `detail` | text | the row's second line (container and engine, say) |
//! | `since` | UTC time it started | UPTIME, counted live |
//! | `uptime` | text (`2d 7h`) | UPTIME, when there is no `since` |
//! | `restarts`, `exitCode` | whole numbers | the row's page |
//! | `source` | `{mode: prebuilt\|source\|dev, ref?, commit?}` (`commit` a hex hash) | MODE / SOURCE |
//! | `probe` | `{url, rule?: health\|web\|any, status?}` | ENDPOINT |
//! | `links` | `[{label, url}]` | LINKS (only the stack's own hosts) |
//!
//! A `probe` is the check the script made itself; the runner does not call
//! the URL. Whether it passes is judged here from `status` by the same
//! rules as every probe (`health` 2xx, `web` 2xx/3xx, `any` any status;
//! `health` when no rule is given), and no `status` means no answer, which
//! is down. An `up` the script might add is not read.
//!
//! One script may report many components (a wrapped whole stack): each
//! shows as its own row. A script for a single component may report one
//! entry named after the component, or leave `name` out.
//!
//! **Groups.** `groups` lists the script's groups in display order (`id`
//! `[A-Za-z0-9._-]`, at most 40 characters; `label` optional, the id when
//! missing). A component's `group` names one of them, or any other id (it
//! then shows under that id). A component with no `group` goes to the
//! script component's own group from the definition, else to a group
//! named after the script component. A group id the definition also uses
//! is the same group.
//!
//! **Where the verbs go.** A script's verbs act on everything it runs, so
//! each is drawn once, not on every row: on the group header when every
//! row of the script lands in one group that holds nothing else, else
//! with the stack's own actions at the top of the Environment. A script
//! that reports only itself keeps its verbs on its one row.
//!
//! **What was asked of one row.** A verb run on one row (`stop` with
//! `{row}`, an `up` of picked rows) records that row's desired state in the
//! instance's intent record, so a row stopped on request shows as stopped
//! (gray), not as deployed but not running, while the stack still wants
//! the script's other rows running. A verb on the whole component (Up,
//! Stop, Down, a stack-wide one) clears those per-row records.
//!
//! **Per-row verbs.** A mutating verb whose argv has a `{row}` word (or a
//! word containing `{row}`) runs on one row: it is offered on every row
//! the last status report named, and `{row}` becomes that row's `name`,
//! as one argv element, never through a shell. A name the last report
//! did not list is refused, and so is a name starting with `-` (it would
//! read as an option). `{row}` is not allowed in `status`, `plan` or
//! `env:`. A per-row verb's confirm shows its resolved argv; the `plan`
//! script is not asked per row.
//!
//! ```yaml
//! restart: [node, scripts/restart.js, "{row}"]      # one Restart per row
//! ```
//!
//! **Picking what comes up.** An `up` argv with a whole `{rows}` word
//! lets the user pick reported rows (or the groups that hold them) when
//! bringing the script's stack up, and lets each row that is not running
//! be brought up on its own. `{rows}` becomes the picked rows' names, one
//! argv element each, in the order the status reported them; with nothing
//! picked (a plain Up, or the stack's own Bring up) it becomes no words at
//! all, which the script should read as "everything". Every name is
//! checked against the last status report first. Without `{rows}` the
//! script's Up is whole-stack only.
//!
//! ```yaml
//! up: [node, scripts/up.js, "{rows}"]               # up.js api web, or up.js
//! ```
//!
//! The reported state goes through the same desired-state rule as every
//! other driver: a reported `stopped-idle` while the stack wants the
//! component running shows as `stopped-problem`, and the other way round.
//!
//! Output that is not this shape, is too large, or does not come within the
//! status limit makes the component `unhealthy` when it is wanted running
//! (the closest of the nine to "running as far as the stack knows, but it
//! cannot be checked"), else the state its recorded intent implies, with a
//! `bad` alert quoting the parse error, never the output itself.
//!
//! Status runs off the request path: a host asks through
//! [`ScriptOutputs`], which answers from what it already has and refreshes
//! on its own thread, so showing the Environment never waits on a script.
//!
//! **Without a `status` script** the state comes from the intent record
//! and the outcome of the last script run ([`ScriptRuns`]): a successful
//! `up` reads as `up` (marked "no status script"), `down` as not deployed,
//! `stop` as stopped, and a failed run as the failed state.
//!
//! # Plan and confirm
//!
//! With `plan`, the confirm of a mutating verb runs `plan <verb>` (the verb
//! appended as the last word) and shows the first 20 lines of what it
//! prints (at most 2 KB, control characters removed) under the line "Run
//! the `<verb>` script for <component>". Without `plan`, the confirm is that
//! line and the resolved argv. A plan that fails says so, and the verb is
//! still offered. While a plan has not answered yet the verb is offered
//! disabled ([`PLAN_PENDING`]), so nobody confirms a run they were not
//! shown.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use crate::cli::{CliError, EngineCli, Invocation};
use crate::definition::{
    is_valid_key, parse_interval, ComponentDef, ComponentKind, ComponentStop, Diagnostic,
    DriverKind, Mode, ProbeRule, Rule, Severity, StackDef,
};
use crate::driver::{ComponentState, ResolvedInstance};
use crate::intent::{Desired, INSTANCES_DIR};
use crate::jobs::{write_atomic, RunBlock};
use crate::placeholders::{self, Placeholder, PlaceholderError};
use crate::process::{env_name, instance_env};

/// Most status output read, in bytes.
pub const STATUS_CAP: usize = 1 << 20;
/// Lines of a plan's output shown in a confirm.
pub const PLAN_LINES: usize = 20;
/// Bytes of a plan's output shown in a confirm.
pub const PLAN_BYTES: usize = 2048;
/// Characters kept of a reported `detail`, fact or alert.
const TEXT_CAP: usize = 300;
/// Most rows, facts and alerts one status report may carry.
const MAX_ENTRIES: usize = 200;
/// What separates a script component from a component it reports in a
/// row id (`stack/api`); component names never contain it.
pub const ROW_SEPARATOR: char = '/';
/// The file of last script outcomes, per instance.
pub const RUNS_FILE: &str = "scripts.json";
/// The environment variable that names the bound item.
pub const ITEM_ENV: &str = "CHAPPA_STACK_ITEM";
/// Prefix of the per-worktree environment variables.
pub const WORKTREE_ENV_PREFIX: &str = "CHAPPA_STACK_WORKTREE_";

// ------------------------------------------------------------------ verbs

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScriptVerb {
    Up,
    Start,
    Stop,
    Restart,
    Rebuild,
    Down,
    Status,
    Plan,
}

impl ScriptVerb {
    pub const ALL: [ScriptVerb; 8] = [
        ScriptVerb::Up,
        ScriptVerb::Start,
        ScriptVerb::Stop,
        ScriptVerb::Restart,
        ScriptVerb::Rebuild,
        ScriptVerb::Down,
        ScriptVerb::Status,
        ScriptVerb::Plan,
    ];

    /// The verbs that change the stack, in the order they are offered.
    pub const MUTATING: [ScriptVerb; 6] = [
        ScriptVerb::Up,
        ScriptVerb::Start,
        ScriptVerb::Stop,
        ScriptVerb::Restart,
        ScriptVerb::Rebuild,
        ScriptVerb::Down,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ScriptVerb::Up => "up",
            ScriptVerb::Start => "start",
            ScriptVerb::Stop => "stop",
            ScriptVerb::Restart => "restart",
            ScriptVerb::Rebuild => "rebuild",
            ScriptVerb::Down => "down",
            ScriptVerb::Status => "status",
            ScriptVerb::Plan => "plan",
        }
    }

    pub fn parse(s: &str) -> Option<ScriptVerb> {
        ScriptVerb::ALL.into_iter().find(|v| v.as_str() == s)
    }

    /// How long the script may run when the definition sets no limit.
    pub fn default_timeout(self) -> Duration {
        let mins = |m: u64| Duration::from_secs(m * 60);
        match self {
            ScriptVerb::Up | ScriptVerb::Start | ScriptVerb::Restart => mins(15),
            ScriptVerb::Stop | ScriptVerb::Down => mins(5),
            ScriptVerb::Rebuild => mins(60),
            ScriptVerb::Status => Duration::from_secs(20),
            ScriptVerb::Plan => Duration::from_secs(60),
        }
    }

    /// The desired state the intent record keeps after this verb ran.
    pub fn desired_after(self) -> Option<Desired> {
        match self {
            ScriptVerb::Up | ScriptVerb::Start | ScriptVerb::Restart | ScriptVerb::Rebuild => {
                Some(Desired::Running)
            }
            ScriptVerb::Stop => Some(Desired::Stopped),
            ScriptVerb::Down => Some(Desired::Absent),
            ScriptVerb::Status | ScriptVerb::Plan => None,
        }
    }

    /// Whether this verb brings the component up (data-safety checks
    /// after up treat it as a start).
    pub fn starts(self) -> bool {
        self.desired_after() == Some(Desired::Running)
    }
}

impl fmt::Display for ScriptVerb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The argv the definition names for `verb`, if any.
pub fn argv(def: &ComponentDef, verb: ScriptVerb) -> Option<&[String]> {
    match verb {
        ScriptVerb::Up => def.up.as_deref(),
        ScriptVerb::Start => def.start.as_deref(),
        ScriptVerb::Stop => def.stop.as_ref().and_then(ComponentStop::script),
        ScriptVerb::Restart => def.restart.as_deref(),
        ScriptVerb::Rebuild => def.rebuild.as_deref(),
        ScriptVerb::Down => def.down.as_deref(),
        ScriptVerb::Status => def.status.as_deref(),
        ScriptVerb::Plan => def.plan.as_deref(),
    }
}

/// Whether `def` is a script component with a script for `verb`.
pub fn has(def: &ComponentDef, verb: ScriptVerb) -> bool {
    def.driver == DriverKind::Script && argv(def, verb).is_some()
}

/// The argv word that stands for the picked rows.
pub const ROWS_WORD: &str = "{rows}";

/// Whether `text` uses placeholder `p`.
fn mentions(text: &str, p: Placeholder) -> bool {
    placeholders::references(text).is_ok_and(|r| r.contains(&p))
}

/// Whether `text` uses the `{row}` placeholder.
fn mentions_row(text: &str) -> bool {
    mentions(text, Placeholder::Row)
}

/// Whether the definition's argv for `verb` has a whole `{rows}` word: its
/// `up` can then be given the rows to bring up.
pub fn takes_rows(def: &ComponentDef, verb: ScriptVerb) -> bool {
    argv(def, verb).is_some_and(|words| words.iter().any(|w| w == ROWS_WORD))
}

/// Whether the definition's argv for `verb` uses `{row}`: the verb then
/// runs on one reported row, not on the whole component.
pub fn takes_row(def: &ComponentDef, verb: ScriptVerb) -> bool {
    argv(def, verb).is_some_and(|words| words.iter().any(|w| mentions_row(w)))
}

/// Why `name` cannot stand in for `{row}`, if it cannot: it must be a
/// reported name (`[A-Za-z0-9._-]`, at most 40 characters) and must not
/// start with `-`, which a program would read as an option.
pub fn row_name_problem(name: &str) -> Option<String> {
    if !is_valid_key(name) {
        return Some(format!("`{name}` is not a reported row name"));
    }
    if name.starts_with('-') {
        return Some(format!(
            "`{name}` starts with `-`, which the script would read as an option"
        ));
    }
    None
}

/// The limit for `verb`: the definition's `timeouts.<verb>`, else the
/// default. An unreadable or zero entry (a validation error) falls back
/// to the default rather than to no limit.
pub fn timeout(def: &ComponentDef, verb: ScriptVerb) -> Duration {
    def.timeouts
        .get(verb.as_str())
        .and_then(|t| parse_interval(t))
        .filter(|d| !d.is_zero())
        .unwrap_or_else(|| verb.default_timeout())
}

/// `20 s`, `15 min`.
pub fn duration_text(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 60 && s.is_multiple_of(60) {
        format!("{} min", s / 60)
    } else {
        format!("{s} s")
    }
}

/// True when the program is a PowerShell file, which no platform runs
/// directly: the interpreter has to be named first.
pub fn needs_interpreter(argv: &[String]) -> bool {
    argv.first().is_some_and(|p| {
        Path::new(p)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("ps1"))
    })
}

/// A relative path with no `..`, which therefore stays inside the folder it
/// is joined to.
fn stays_inside(p: &Path) -> bool {
    !p.as_os_str().is_empty()
        && p.components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}

/// The script-specific findings for one component (any driver: the script
/// keys on another driver's component are reported as unused).
pub fn validate_component(c: &ComponentDef, cpath: &str) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let mut push = |severity: Severity, rule: Rule, path: String, message: String| {
        out.push(Diagnostic {
            severity,
            rule,
            path,
            message,
        })
    };
    if c.driver != DriverKind::Script {
        let mut unused: Vec<&str> = ScriptVerb::ALL
            .into_iter()
            .filter(|v| argv(c, *v).is_some())
            .map(ScriptVerb::as_str)
            .collect();
        if c.cwd.is_some() {
            unused.push("cwd");
        }
        if !c.timeouts.is_empty() {
            unused.push("timeouts");
        }
        if !c.repos.is_empty() {
            unused.push("repos");
        }
        for key in unused {
            push(
                Severity::Warning,
                Rule::NotForDriver,
                format!("{cpath}.{key}"),
                format!(
                    "only script components use `{key}`; a {} component ignores it",
                    c.driver
                ),
            );
        }
        return out;
    }
    for verb in ScriptVerb::ALL {
        let Some(words) = argv(c, verb) else {
            continue;
        };
        let path = format!("{cpath}.{verb}");
        if words.first().is_none_or(|w| w.trim().is_empty()) {
            push(
                Severity::Error,
                Rule::ScriptArgv,
                path,
                format!("the `{verb}` script is empty; give it an argv such as [node, scripts/{verb}.js]"),
            );
            continue;
        }
        if needs_interpreter(words) {
            push(
                Severity::Warning,
                Rule::ScriptArgv,
                path,
                format!(
                    "`{}` is a PowerShell script, which does not run on its own; name the interpreter first: [powershell, -NoProfile, -File, {}]",
                    words[0], words[0]
                ),
            );
        }
    }
    for verb in ScriptVerb::ALL {
        let Some(words) = argv(c, verb) else { continue };
        let whole = words.iter().any(|w| w == ROWS_WORD);
        let inside = words
            .iter()
            .any(|w| w != ROWS_WORD && mentions(w, Placeholder::Rows));
        if inside || (whole && verb != ScriptVerb::Up) {
            push(
                Severity::Error,
                Rule::ScriptArgv,
                format!("{cpath}.{verb}"),
                if inside {
                    "`{rows}` stands for several argv words, so it must be a whole word of its own"
                        .into()
                } else {
                    format!("`{{rows}}` is the rows picked for `up`; the `{verb}` script cannot take it")
                },
            );
        }
    }
    for verb in [ScriptVerb::Status, ScriptVerb::Plan] {
        if takes_row(c, verb) {
            push(
                Severity::Error,
                Rule::ScriptArgv,
                format!("{cpath}.{verb}"),
                format!("`{{row}}` names the row a verb runs on; the `{verb}` script runs for the whole component, so it has none"),
            );
        }
    }
    for (key, value) in &c.env {
        if mentions_row(value) || mentions(value, Placeholder::Rows) {
            push(
                Severity::Error,
                Rule::ScriptArgv,
                format!("{cpath}.env.{key}"),
                "`{row}` and `{rows}` only work in the argv of a verb; the environment is the same for every row".into(),
            );
        }
    }
    if let Some(cwd) = &c.cwd {
        if !stays_inside(cwd) {
            push(
                Severity::Error,
                Rule::PathOutsideProject,
                format!("{cpath}.cwd"),
                format!(
                    "`{}` must be a folder inside the project, written relative to it, without `..`",
                    cwd.display()
                ),
            );
        }
    }
    for (key, value) in &c.timeouts {
        let path = format!("{cpath}.timeouts.{key}");
        let Some(verb) = ScriptVerb::parse(key) else {
            push(
                Severity::Error,
                Rule::UnknownKey,
                path,
                format!("`{key}` is not a script verb (up, start, stop, restart, rebuild, down, status, plan)"),
            );
            continue;
        };
        match parse_interval(value) {
            None => push(
                Severity::Error,
                Rule::InvalidDuration,
                path,
                format!(
                    "`{value}` is not a duration; write a whole number and s, m, h or d (`30m`)"
                ),
            ),
            Some(d) if d.is_zero() => push(
                Severity::Error,
                Rule::OutOfRange,
                path,
                "a limit of zero would stop the script at once".into(),
            ),
            Some(_) if argv(c, verb).is_none() => push(
                Severity::Warning,
                Rule::UnknownKey,
                path,
                format!("there is no `{verb}` script, so this limit is unused"),
            ),
            Some(_) => {}
        }
    }
    if !c.modes.is_empty() {
        push(
            Severity::Warning,
            Rule::NotForDriver,
            format!("{cpath}.modes"),
            "script components have no modes; the scripts decide what they build or pull".into(),
        );
    }
    for (i, r) in c.repos.iter().enumerate() {
        if crate::definition::repo_name(r).is_none() {
            push(
                Severity::Error,
                Rule::MissingField,
                format!("{cpath}.repos[{i}]"),
                format!("`{}` names no folder", r.display()),
            );
        }
    }
    out
}

// -------------------------------------------------------------------- runs

/// What a script component's scripts see of the work item bound to the
/// slot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScriptBinding {
    /// The item's key.
    pub item: Option<String>,
    /// The item's branch per clone name.
    pub branches: IndexMap<String, String>,
    /// The item's worktree per clone name.
    pub worktrees: IndexMap<String, PathBuf>,
    /// The reported row a per-row verb runs on (`{row}`).
    pub row: Option<String>,
    /// The reported rows picked for `up` (`{rows}`); empty = everything.
    pub rows: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScriptError {
    #[error("`{0}` is not a script component")]
    NotScript(String),
    #[error("`{component}` has no `{verb}` script")]
    NoScript { component: String, verb: ScriptVerb },
    #[error("the `{verb}` script of `{component}` cannot run: {source}")]
    Placeholder {
        component: String,
        verb: ScriptVerb,
        #[source]
        source: PlaceholderError,
    },
    #[error("the `{verb}` script of `{component}` cannot run: {reason}")]
    Argv {
        component: String,
        verb: ScriptVerb,
        reason: String,
    },
}

/// One script run, resolved: what is spawned, where, with what added to
/// the environment, and for how long at most.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScriptRun {
    pub component: String,
    pub verb: ScriptVerb,
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    #[serde(default)]
    pub env: IndexMap<String, String>,
    pub timeout_secs: u64,
}

impl ScriptRun {
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs)
    }

    /// The argv as spawned: a relative program path with a folder in it
    /// (`scripts/up.cmd`) is taken from the working directory, the same on
    /// every platform.
    pub fn spawn_argv(&self) -> Vec<String> {
        let mut argv = self.argv.clone();
        if let Some(first) = argv.first_mut() {
            let p = Path::new(first.as_str());
            let has_folder = first.contains('/') || first.contains('\\');
            if has_folder && p.is_relative() {
                *first = self.cwd.join(p).display().to_string();
            }
        }
        argv
    }

    /// The run as a job step's run block.
    pub fn run_block(&self) -> RunBlock {
        RunBlock::new(self.spawn_argv(), &self.cwd)
            .with_env(self.env.clone())
            .titled(format!("{}: {} script", self.component, self.verb))
    }

    /// The run as a quick captured call (status, plan).
    pub fn invocation(&self) -> Invocation {
        let mut inv = Invocation::from_argv(&self.spawn_argv())
            .cwd(&self.cwd)
            .timeout(self.timeout());
        inv.env = self.env.clone();
        inv
    }

    /// The argv as one line for a confirm: words with spaces are quoted.
    pub fn command_line(&self) -> String {
        self.argv
            .iter()
            .map(|w| {
                if w.is_empty() || w.contains(char::is_whitespace) {
                    format!("\"{w}\"")
                } else {
                    w.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The instance the run belongs to (`CHAPPA_STACK_INSTANCE` in its
    /// environment), which is what its host serialises script runs by.
    pub fn instance(&self) -> Option<&str> {
        self.env.get("CHAPPA_STACK_INSTANCE").map(String::as_str)
    }

    /// Stable identity of exactly this run (argv, folder, environment,
    /// limit): what a cache of its output is keyed by.
    pub fn key(&self) -> String {
        let text = serde_json::to_string(self).expect("a run serializes");
        blake3::hash(text.as_bytes()).to_hex()[..16].to_string()
    }
}

/// Resolve `verb` of `component` for this instance. `extra` words are
/// appended after the resolved argv (the verb name, for `plan`).
#[allow(clippy::too_many_arguments)]
pub fn prepare(
    inst: &ResolvedInstance,
    stack: &StackDef,
    project_dir: &Path,
    component: &str,
    verb: ScriptVerb,
    binding: &ScriptBinding,
    images: &IndexMap<String, String>,
    extra: &[String],
) -> Result<ScriptRun, ScriptError> {
    let def = stack
        .components
        .get(component)
        .filter(|d| d.driver == DriverKind::Script)
        .ok_or_else(|| ScriptError::NotScript(component.to_string()))?;
    let words = argv(def, verb).ok_or_else(|| ScriptError::NoScript {
        component: component.to_string(),
        verb,
    })?;
    let argv_error = |reason: String| ScriptError::Argv {
        component: component.to_string(),
        verb,
        reason,
    };
    if words.first().is_none_or(|w| w.trim().is_empty()) {
        return Err(argv_error("the command is empty".into()));
    }
    if needs_interpreter(words) {
        return Err(argv_error(format!(
            "`{0}` is a PowerShell script, which does not run on its own; name the interpreter first, as in [powershell, -NoProfile, -File, {0}]",
            words[0]
        )));
    }
    if let Some(problem) = binding
        .row
        .iter()
        .chain(&binding.rows)
        .find_map(|r| row_name_problem(r))
    {
        return Err(argv_error(problem));
    }
    let mut ctx = inst.context_for(component);
    ctx.row = binding.row.clone();
    // A script runs on the host, whatever engine its group names.
    ctx.host = Some(placeholders::HostSide::Host);
    ctx.images = images.clone();
    if binding.item.is_some() {
        ctx.item = binding.item.clone();
    }
    ctx.branches = binding.branches.clone();
    ctx.worktrees = binding
        .worktrees
        .iter()
        .map(|(r, p)| (r.clone(), p.display().to_string()))
        .collect();
    let placeholder = |source: PlaceholderError| ScriptError::Placeholder {
        component: component.to_string(),
        verb,
        source,
    };
    // `{rows}` is several words (or none), so it is expanded here, each
    // name one element, before the rest is resolved word by word.
    let mut resolved: Vec<String> = Vec::new();
    for word in words {
        if word == ROWS_WORD {
            resolved.extend(binding.rows.iter().cloned());
        } else {
            resolved.push(placeholders::resolve(word, &ctx).map_err(placeholder)?);
        }
    }
    resolved.extend(extra.iter().cloned());
    let cwd = match &def.cwd {
        None => project_dir.to_path_buf(),
        Some(rel) if stays_inside(rel) => project_dir.join(rel),
        Some(rel) => {
            return Err(argv_error(format!(
                "its cwd `{}` is not a folder inside the project",
                rel.display()
            )))
        }
    };
    let mut env = instance_env(inst, stack, images);
    env.insert(ITEM_ENV.to_string(), ctx.item.clone().unwrap_or_default());
    for (repo, path) in &binding.worktrees {
        env.insert(
            format!("{WORKTREE_ENV_PREFIX}{}", env_name(repo)),
            path.display().to_string(),
        );
    }
    for (k, template) in &def.env {
        env.insert(
            k.clone(),
            placeholders::resolve(template, &ctx).map_err(placeholder)?,
        );
    }
    Ok(ScriptRun {
        component: component.to_string(),
        verb,
        argv: resolved,
        cwd,
        env,
        timeout_secs: timeout(def, verb).as_secs().max(1),
    })
}

// ------------------------------------------------------------- status JSON

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportedLink {
    pub label: String,
    pub url: String,
}

/// A group a status script names, in its display order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportedGroup {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportedFact {
    pub label: String,
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AlertTone {
    Warn,
    Bad,
    Info,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportedAlert {
    pub tone: AlertTone,
    pub text: String,
}

/// One component as a status script reported it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportedComponent {
    pub name: String,
    pub state: ComponentState,
    /// The group it shows in; `None` = the script component's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<ReportedLink>,
    /// How long it has been up, as text ("2d 7h"), when `since` is not given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uptime: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restarts: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<ReportedSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe: Option<ReportedProbe>,
}

/// Where a reported component's code comes from, in the Environment's
/// own terms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportedSource {
    pub mode: Mode,
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

/// An HTTP check the status script made. Whether it passes is decided here,
/// by the contract's rule from the status it got, never taken from the
/// script: no status means no answer, which is down whatever the rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportedProbe {
    pub url: String,
    pub rule: ProbeRule,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
}

impl ReportedProbe {
    pub fn up(&self) -> bool {
        self.status
            .is_some_and(|s| crate::probe::passes(self.rule, s))
    }
}

/// A status script's whole report, checked.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ScriptStatus {
    /// The groups in display order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<ReportedGroup>,
    pub components: Vec<ReportedComponent>,
    #[serde(default)]
    pub facts: Vec<ReportedFact>,
    #[serde(default)]
    pub alerts: Vec<ReportedAlert>,
}

/// What one run of a status script came to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum ScriptReport {
    Status {
        status: ScriptStatus,
    },
    /// The script did not answer usably; `reason` never quotes its output.
    Failed {
        reason: String,
    },
}

/// Printable text only, capped: what a report may put on screen.
fn clean_text(s: &str, cap: usize) -> String {
    let mut out: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(cap)
        .collect();
    if s.chars().count() > cap {
        out.push('…');
    }
    out.trim().to_string()
}

/// A serde error said without quoting the input: its category and where.
fn json_error(e: &serde_json::Error) -> String {
    use serde_json::error::Category;
    let what = match e.classify() {
        Category::Syntax => "not valid JSON",
        Category::Eof => "JSON that ends too early",
        Category::Data => "JSON of the wrong shape",
        Category::Io => "unreadable",
    };
    format!("{what} (line {}, column {})", e.line(), e.column())
}

fn string_field(
    obj: &serde_json::Map<String, Value>,
    key: &str,
    at: &str,
) -> Result<Option<String>, String> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("{at}: `{key}` is not a string")),
    }
}

fn list<'a>(doc: &'a serde_json::Map<String, Value>, key: &str) -> Result<&'a [Value], String> {
    match doc.get(key) {
        None | Some(Value::Null) => Ok(&[]),
        Some(Value::Array(a)) if a.len() <= MAX_ENTRIES => Ok(a),
        Some(Value::Array(_)) => Err(format!("`{key}` has more than {MAX_ENTRIES} entries")),
        Some(_) => Err(format!("`{key}` is not a list")),
    }
}

/// Check a status script's stdout against the status shape. `component`
/// names an entry that leaves `name` out (allowed when it is the only one).
/// The error says what is wrong and where, and never repeats the output.
pub fn parse_status(stdout: &str, component: &str) -> Result<ScriptStatus, String> {
    if stdout.len() > STATUS_CAP {
        return Err(format!("the output is over {} KiB", STATUS_CAP / 1024));
    }
    let doc: Value = serde_json::from_str(stdout.trim()).map_err(|e| json_error(&e))?;
    let Value::Object(doc) = doc else {
        return Err("the output is not a JSON object".into());
    };
    let mut groups: Vec<ReportedGroup> = Vec::new();
    for (i, g) in list(&doc, "groups")?.iter().enumerate() {
        let at = format!("groups[{i}]");
        let Value::Object(g) = g else {
            return Err(format!("{at} is not an object"));
        };
        let id = string_field(g, "id", &at)?.ok_or_else(|| format!("{at} has no `id`"))?;
        if !is_valid_key(&id) {
            return Err(format!(
                "{at}: `id` must match [A-Za-z0-9._-] and be at most 40 characters"
            ));
        }
        if groups.iter().any(|x| x.id == id) {
            return Err(format!("{at}: `{id}` is listed twice"));
        }
        let label = string_field(g, "label", &at)?
            .map(|l| clean_text(&l, 60))
            .filter(|l| !l.is_empty())
            .unwrap_or_else(|| id.clone());
        groups.push(ReportedGroup { id, label });
    }
    let entries = list(&doc, "components")?;
    let mut components: Vec<ReportedComponent> = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        let at = format!("components[{i}]");
        let Value::Object(e) = e else {
            return Err(format!("{at} is not an object"));
        };
        let name = match string_field(e, "name", &at)? {
            Some(n) if is_valid_key(&n) => n,
            Some(_) => {
                return Err(format!(
                    "{at}: `name` must match [A-Za-z0-9._-] and be at most 40 characters"
                ))
            }
            None if entries.len() == 1 => component.to_string(),
            None => {
                return Err(format!(
                    "{at} has no `name`, and there is more than one entry"
                ))
            }
        };
        let state_text = string_field(e, "state", &at)?
            .ok_or_else(|| format!("{at} (`{name}`) has no `state`"))?;
        let state = ComponentState::ALL
            .into_iter()
            .find(|s| s.as_str() == state_text)
            .ok_or_else(|| {
                format!(
                    "{at} (`{name}`): `state` is not one of {}",
                    ComponentState::ALL.map(ComponentState::as_str).join(", ")
                )
            })?;
        if components.iter().any(|c| c.name == name) {
            return Err(format!("{at}: `{name}` is reported twice"));
        }
        let group = string_field(e, "group", &at)?;
        if group.as_deref().is_some_and(|g| !is_valid_key(g)) {
            return Err(format!(
                "{at} (`{name}`): `group` must match [A-Za-z0-9._-] and be at most 40 characters"
            ));
        }
        let exit_code = match e.get("exitCode") {
            None | Some(Value::Null) => None,
            Some(v) => Some(
                v.as_i64()
                    .and_then(|n| i32::try_from(n).ok())
                    .ok_or_else(|| format!("{at} (`{name}`): `exitCode` is not a whole number"))?,
            ),
        };
        let mut links = Vec::new();
        for (j, l) in list(e, "links")?.iter().enumerate() {
            let lat = format!("{at}.links[{j}]");
            let Value::Object(l) = l else {
                return Err(format!("{lat} is not an object"));
            };
            let url = string_field(l, "url", &lat)?.ok_or_else(|| format!("{lat} has no `url`"))?;
            let label = string_field(l, "label", &lat)?.unwrap_or_else(|| "Open".into());
            links.push(ReportedLink {
                label: clean_text(&label, 60),
                url: clean_text(&url, 2000),
            });
        }
        let uptime = string_field(e, "uptime", &at)?.map(|s| clean_text(&s, 40));
        let restarts =
            match e.get("restarts") {
                None | Some(Value::Null) => None,
                Some(v) => Some(v.as_u64().and_then(|n| u32::try_from(n).ok()).ok_or_else(
                    || format!("{at} (`{name}`): `restarts` is not a whole number of 0 or more"),
                )?),
            };
        let source = match e.get("source") {
            None | Some(Value::Null) => None,
            Some(Value::Object(s)) => {
                let sat = format!("{at}.source");
                let mode =
                    string_field(s, "mode", &sat)?.ok_or_else(|| format!("{sat} has no `mode`"))?;
                let mode = Mode::parse(&mode)
                    .ok_or_else(|| format!("{sat}: `mode` is not prebuilt, source or dev"))?;
                let commit = string_field(s, "commit", &sat)?;
                if commit.as_deref().is_some_and(|c| {
                    c.is_empty() || c.len() > 64 || !c.bytes().all(|b| b.is_ascii_hexdigit())
                }) {
                    return Err(format!("{sat}: `commit` is not a commit hash"));
                }
                Some(ReportedSource {
                    mode,
                    git_ref: string_field(s, "ref", &sat)?
                        .map(|r| clean_text(&r, 200))
                        .filter(|r| !r.is_empty()),
                    commit: commit.map(|c| c.to_ascii_lowercase()),
                })
            }
            Some(_) => return Err(format!("{at} (`{name}`): `source` is not an object")),
        };
        let probe = match e.get("probe") {
            None | Some(Value::Null) => None,
            Some(Value::Object(p)) => {
                let pat = format!("{at}.probe");
                let url =
                    string_field(p, "url", &pat)?.ok_or_else(|| format!("{pat} has no `url`"))?;
                if !(url.starts_with("http://") || url.starts_with("https://")) {
                    return Err(format!("{pat}: `url` is not an http(s) URL"));
                }
                let rule = match string_field(p, "rule", &pat)?.as_deref() {
                    None | Some("health") => ProbeRule::Health,
                    Some("web") => ProbeRule::Web,
                    Some("any") => ProbeRule::Any,
                    Some(_) => return Err(format!("{pat}: `rule` is not health, web or any")),
                };
                let status = match p.get("status") {
                    None | Some(Value::Null) => None,
                    Some(v) => Some(
                        v.as_u64()
                            .and_then(|n| u16::try_from(n).ok())
                            .filter(|n| (100..600).contains(n))
                            .ok_or_else(|| format!("{pat}: `status` is not an HTTP status"))?,
                    ),
                };
                Some(ReportedProbe {
                    url: clean_text(&url, 2000),
                    rule,
                    status,
                })
            }
            Some(_) => return Err(format!("{at} (`{name}`): `probe` is not an object")),
        };
        components.push(ReportedComponent {
            name,
            state,
            group,
            uptime,
            restarts,
            source,
            probe,
            since: string_field(e, "since", &at)?.map(|s| clean_text(&s, 40)),
            exit_code,
            detail: string_field(e, "detail", &at)?.map(|s| clean_text(&s, TEXT_CAP)),
            links,
        });
    }
    let mut facts = Vec::new();
    for (i, f) in list(&doc, "facts")?.iter().enumerate() {
        let at = format!("facts[{i}]");
        let Value::Object(f) = f else {
            return Err(format!("{at} is not an object"));
        };
        let label = string_field(f, "label", &at)?.ok_or_else(|| format!("{at} has no `label`"))?;
        let value = match f.get("value") {
            Some(Value::String(s)) => s.clone(),
            Some(v @ (Value::Number(_) | Value::Bool(_))) => v.to_string(),
            None | Some(Value::Null) => String::new(),
            Some(_) => return Err(format!("{at}: `value` is not text")),
        };
        facts.push(ReportedFact {
            label: clean_text(&label, 60),
            value: clean_text(&value, TEXT_CAP),
        });
    }
    let mut alerts = Vec::new();
    for (i, a) in list(&doc, "alerts")?.iter().enumerate() {
        let at = format!("alerts[{i}]");
        let Value::Object(a) = a else {
            return Err(format!("{at} is not an object"));
        };
        let tone = match string_field(a, "tone", &at)?.as_deref() {
            Some("warn") | None => AlertTone::Warn,
            Some("bad") => AlertTone::Bad,
            Some("info") => AlertTone::Info,
            Some(_) => return Err(format!("{at}: `tone` is not warn, bad or info")),
        };
        let text = string_field(a, "text", &at)?.ok_or_else(|| format!("{at} has no `text`"))?;
        alerts.push(ReportedAlert {
            tone,
            text: clean_text(&text, TEXT_CAP),
        });
    }
    Ok(ScriptStatus {
        groups,
        components,
        facts,
        alerts,
    })
}

/// Run a status script through `cli` and check what it printed.
pub fn run_status(cli: &dyn EngineCli, run: &ScriptRun) -> ScriptReport {
    let failed = |reason: String| ScriptReport::Failed { reason };
    match cli.run(&run.invocation()) {
        Err(CliError::Timeout { .. }) => failed(format!(
            "the status script did not finish within {}",
            duration_text(run.timeout())
        )),
        Err(e) => failed(format!("the status script could not run ({e})")),
        Ok(out) if !out.ok() => failed(match out.status {
            Some(code) => format!("the status script exited with code {code}"),
            None => "the status script was killed".to_string(),
        }),
        Ok(out) => match parse_status(&out.stdout, &run.component) {
            Ok(status) => ScriptReport::Status { status },
            Err(e) => failed(format!("its status output cannot be read: {e}")),
        },
    }
}

// ------------------------------------------------------------ last outcome

/// The outcome of the last script a job ran for a component.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LastRun {
    pub verb: ScriptVerb,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub ended_utc: String,
    /// Why it failed, in the runner's words (a timeout, a start failure).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `.work/instances/<instance>/scripts.json`: the last outcome per script
/// component, what state is judged from when there is no status script.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ScriptRuns {
    #[serde(default)]
    pub components: IndexMap<String, LastRun>,
}

impl ScriptRuns {
    pub fn path(work_dir: &Path, instance: &str) -> PathBuf {
        work_dir.join(INSTANCES_DIR).join(instance).join(RUNS_FILE)
    }

    /// A missing or unreadable file is an empty record: nothing ran yet.
    pub fn load(work_dir: &Path, instance: &str) -> ScriptRuns {
        fs::read_to_string(Self::path(work_dir, instance))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    /// Record `run` as the last outcome of `component`.
    pub fn record(
        work_dir: &Path,
        instance: &str,
        component: &str,
        run: LastRun,
    ) -> std::io::Result<()> {
        let mut all = Self::load(work_dir, instance);
        all.components.insert(component.to_string(), run);
        write_atomic(
            &Self::path(work_dir, instance),
            &serde_json::to_string_pretty(&all).expect("runs serialize"),
        )
    }
}

// ------------------------------------------------------------------- views

/// Where a view's states came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ViewFrom {
    /// The status script's report.
    Status,
    /// No status script: the intent record and the last run.
    Assumed,
    /// A status script that has not answered yet.
    Pending,
    /// A status script whose answer could not be used.
    Unreadable,
}

/// One row the Environment shows for a script component.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScriptRow {
    /// The component itself, or `<component>/<reported name>`.
    pub id: String,
    pub name: String,
    pub state: ComponentState,
    /// The group the status script put it in, if it named one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<ReportedLink>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uptime: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restarts: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<ReportedSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe: Option<ReportedProbe>,
}

/// Everything the Environment shows for one script component.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScriptView {
    pub component: String,
    pub from: ViewFrom,
    /// The script reports components other than itself: each row is one
    /// of them, and the component has no row of its own.
    pub multi: bool,
    pub rows: Vec<ScriptRow>,
    /// The groups the status script listed, in its order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<ReportedGroup>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub facts: Vec<ReportedFact>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alerts: Vec<ReportedAlert>,
    /// Seconds the instance's current script run has taken, when that is
    /// longer than a status refresh: the rows are the last known state,
    /// not a fresh one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slow_secs: Option<u64>,
}

/// How loud a state is when several rows stand for one component: the
/// worst comes first.
fn rank(s: ComponentState) -> u8 {
    match s {
        ComponentState::Crashloop => 0,
        ComponentState::Unhealthy => 1,
        ComponentState::StoppedProblem => 2,
        ComponentState::OneshotFailed => 3,
        ComponentState::Starting => 4,
        ComponentState::Up => 5,
        ComponentState::OneshotOk => 6,
        ComponentState::StoppedIdle => 7,
        ComponentState::NotDeployed => 8,
    }
}

fn is_running(s: ComponentState) -> bool {
    matches!(
        s,
        ComponentState::Up
            | ComponentState::Starting
            | ComponentState::Unhealthy
            | ComponentState::Crashloop
    )
}

impl ScriptView {
    /// The worst state among the rows: the component's own, for counts and
    /// alerts.
    pub fn state(&self) -> ComponentState {
        self.rows
            .iter()
            .map(|r| r.state)
            .min_by_key(|s| rank(*s))
            .unwrap_or(ComponentState::NotDeployed)
    }

    pub fn running(&self) -> bool {
        self.rows.iter().any(|r| is_running(r.state))
    }

    pub fn exists(&self) -> bool {
        self.rows
            .iter()
            .any(|r| r.state != ComponentState::NotDeployed)
    }
}

/// The row id of `name` as reported by `component`.
pub fn row_id(component: &str, name: &str) -> String {
    if name == component {
        component.to_string()
    } else {
        format!("{component}{ROW_SEPARATOR}{name}")
    }
}

/// The component a row id belongs to: the part before the separator.
pub fn component_of_row(id: &str) -> &str {
    id.split(ROW_SEPARATOR).next().unwrap_or(id)
}

/// A reported state under the desired-state rule: not running and wanted
/// is the problem state, not running and not wanted the idle one, whatever
/// the script called it.
pub fn with_intent(reported: ComponentState, desired: Desired) -> ComponentState {
    match reported {
        ComponentState::StoppedIdle | ComponentState::StoppedProblem => {
            if desired == Desired::Running {
                ComponentState::StoppedProblem
            } else {
                ComponentState::StoppedIdle
            }
        }
        s => s,
    }
}

/// The state without a status report: the desired state and how the last
/// script run went, with the reason shown beside it.
pub fn assumed_state(
    kind: ComponentKind,
    desired: Desired,
    last: Option<&LastRun>,
) -> (ComponentState, String) {
    let code = |l: &LastRun| match (l.exit_code, &l.error) {
        (Some(c), _) => format!("exit {c}"),
        (None, Some(e)) => e.clone(),
        (None, None) => "no exit code".into(),
    };
    if let Some(l) = last.filter(|l| !l.ok) {
        return if l.verb.starts() {
            let state = if kind == ComponentKind::Oneshot {
                ComponentState::OneshotFailed
            } else {
                ComponentState::StoppedProblem
            };
            (
                state,
                format!("its `{}` script failed ({})", l.verb, code(l)),
            )
        } else {
            (
                ComponentState::Unhealthy,
                format!(
                    "its `{}` script failed ({}); it may still be running",
                    l.verb,
                    code(l)
                ),
            )
        };
    }
    let by = last
        .map(|l| format!("the last `{}` script succeeded", l.verb))
        .unwrap_or_else(|| "no script has run here yet".into());
    match desired {
        Desired::Running if kind == ComponentKind::Oneshot => (ComponentState::OneshotOk, by),
        Desired::Running => (ComponentState::Up, format!("up as far as known: {by}")),
        Desired::Stopped => (ComponentState::StoppedIdle, by),
        Desired::Absent => (ComponentState::NotDeployed, by),
    }
}

/// What the Environment shows for `component`: from its status report when
/// it has a status script, else from the intent and the last run.
pub fn view(
    component: &str,
    kind: ComponentKind,
    desired: Desired,
    has_status: bool,
    report: Option<&ScriptReport>,
    last: Option<&LastRun>,
) -> ScriptView {
    view_with_rows(
        component,
        kind,
        desired,
        &IndexMap::new(),
        has_status,
        report,
        last,
    )
}

/// [`view`] with what was asked of single rows (`stop` on one row, `up` of
/// picked rows): a reported row's state is judged by its own desired
/// state when it has one, else by the component's.
pub fn view_with_rows(
    component: &str,
    kind: ComponentKind,
    desired: Desired,
    row_desired: &IndexMap<String, Desired>,
    has_status: bool,
    report: Option<&ScriptReport>,
    last: Option<&LastRun>,
) -> ScriptView {
    let single = |from: ViewFrom, state: ComponentState, detail: String| ScriptView {
        component: component.to_string(),
        from,
        multi: false,
        rows: vec![ScriptRow {
            id: component.to_string(),
            name: component.to_string(),
            state,
            group: None,
            exit_code: last.and_then(|l| l.exit_code).filter(|c| *c != 0),
            since: None,
            detail: Some(detail),
            links: Vec::new(),
            uptime: None,
            restarts: None,
            source: None,
            probe: None,
        }],
        groups: Vec::new(),
        facts: Vec::new(),
        alerts: Vec::new(),
        slow_secs: None,
    };
    let (assumed, why) = assumed_state(kind, desired, last);
    if !has_status {
        return single(
            ViewFrom::Assumed,
            assumed,
            format!("no status script; {why}"),
        );
    }
    match report {
        None => single(
            ViewFrom::Pending,
            assumed,
            format!("its status script has not answered yet; {why}"),
        ),
        Some(ScriptReport::Failed { reason }) => {
            let state = if desired == Desired::Running {
                ComponentState::Unhealthy
            } else {
                assumed
            };
            let mut v = single(ViewFrom::Unreadable, state, reason.clone());
            v.alerts.push(ReportedAlert {
                tone: AlertTone::Bad,
                text: format!(
                    "{component}: {reason}; shown as {state} until its status script answers"
                ),
            });
            v
        }
        Some(ScriptReport::Status { status }) if status.components.is_empty() => {
            let mut v = single(
                ViewFrom::Status,
                with_intent(ComponentState::StoppedIdle, desired),
                "its status script reports no components".into(),
            );
            if desired != Desired::Running {
                v.rows[0].state = assumed;
            }
            v.facts = status.facts.clone();
            v.alerts = status.alerts.clone();
            v
        }
        Some(ScriptReport::Status { status }) => {
            let multi = status.components.len() > 1 || status.components[0].name != component;
            ScriptView {
                component: component.to_string(),
                from: ViewFrom::Status,
                multi,
                rows: status
                    .components
                    .iter()
                    .map(|c| ScriptRow {
                        id: row_id(component, &c.name),
                        name: c.name.clone(),
                        state: with_intent(
                            c.state,
                            row_desired.get(&c.name).copied().unwrap_or(desired),
                        ),
                        group: c.group.clone(),
                        exit_code: c.exit_code,
                        since: c.since.clone(),
                        detail: c.detail.clone(),
                        links: c.links.clone(),
                        uptime: c.uptime.clone(),
                        restarts: c.restarts,
                        source: c.source.clone(),
                        probe: c.probe.clone(),
                    })
                    .collect(),
                groups: status.groups.clone(),
                facts: status.facts.clone(),
                alerts: status.alerts.clone(),
                slow_secs: None,
            }
        }
    }
}

// --------------------------------------------------------- plan and confirm

/// What a `plan` script said about a verb.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum PlanPreview {
    /// Its output, trimmed for a confirm ([`plan_body`]).
    Ready {
        text: String,
    },
    Failed {
        reason: String,
    },
}

/// The first [`PLAN_LINES`] lines of a plan's output, at most [`PLAN_BYTES`]
/// bytes, control characters removed.
pub fn plan_body(stdout: &str) -> String {
    let mut out = String::new();
    for line in stdout.lines().take(PLAN_LINES) {
        let clean: String = line
            .chars()
            .map(|c| if c == '\t' { ' ' } else { c })
            .filter(|c| !c.is_control())
            .collect();
        let clean = clean.trim_end();
        let room = PLAN_BYTES.saturating_sub(out.len() + usize::from(!out.is_empty()));
        if room == 0 {
            break;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        let mut cut = clean.len().min(room);
        while !clean.is_char_boundary(cut) {
            cut -= 1;
        }
        out.push_str(&clean[..cut]);
        if cut < clean.len() {
            break;
        }
    }
    out.trim_end().to_string()
}

/// Run `plan <verb>` through `cli`.
pub fn run_plan(cli: &dyn EngineCli, run: &ScriptRun) -> PlanPreview {
    let failed = |reason: String| PlanPreview::Failed { reason };
    match cli.run(&run.invocation()) {
        Err(CliError::Timeout { .. }) => failed(format!(
            "it did not finish within {}",
            duration_text(run.timeout())
        )),
        Err(e) => failed(format!("it could not run ({e})")),
        Ok(out) if !out.ok() => failed(match out.status {
            Some(code) => format!("it exited with code {code}"),
            None => "it was killed".to_string(),
        }),
        Ok(out) => PlanPreview::Ready {
            text: plan_body(&out.stdout),
        },
    }
}

/// Why an action whose plan preview has not arrived is disabled, and what
/// its confirm says meanwhile: the action cannot be accepted until the
/// preview is there, so nobody confirms a script run blind.
pub const PLAN_PENDING: &str = "Its plan is still being worked out";

/// "Run the `up` script for api": the line every script confirm starts with.
pub fn confirm_line(component: &str, verb: ScriptVerb) -> String {
    format!("Run the `{verb}` script for {component}")
}

/// The confirm of running `verb` of `component`. `run` is the resolved run
/// (or why it cannot be resolved), `with_plan` whether a `plan` script is
/// configured, and `preview` what it said (`None` while it has not).
pub fn confirm(
    component: &str,
    verb: ScriptVerb,
    run: Result<&ScriptRun, &ScriptError>,
    with_plan: bool,
    preview: Option<&PlanPreview>,
) -> String {
    let line = confirm_line(component, verb);
    let run = match run {
        Ok(r) => r,
        Err(e) => return format!("{line}. It cannot run now: {e}."),
    };
    if !with_plan {
        return format!("{line}: {}.", run.command_line());
    }
    match preview {
        Some(PlanPreview::Ready { text }) if text.is_empty() => {
            format!("{line}. Its plan printed nothing.")
        }
        Some(PlanPreview::Ready { text }) => format!("{line}.\n{text}"),
        Some(PlanPreview::Failed { reason }) => {
            format!("{line}. Its plan script failed ({reason}), so what it will do is not shown.")
        }
        None => format!("{line}. {PLAN_PENDING}."),
    }
}

// ----------------------------------------------------------------- outputs

/// Where status reports and plan previews come from. A host that answers
/// often keeps them in a cache it refreshes on its own thread and answers
/// `None` for one it has not got yet; [`LiveScripts`] runs them in place.
pub trait ScriptOutputs {
    fn status(&self, run: &ScriptRun) -> Option<ScriptReport>;
    fn plan(&self, run: &ScriptRun) -> Option<PlanPreview>;
    /// How long the script run in flight for `run`'s instance has taken,
    /// when it is past the status refresh interval (its status is then
    /// older than usual). `None` for hosts that run scripts in place.
    fn slow(&self, _run: &ScriptRun) -> Option<Duration> {
        None
    }
}

/// Nothing is known: every status is pending and no plan has answered.
/// What a job uses, which never needs either.
pub struct NoScriptOutputs;

impl ScriptOutputs for NoScriptOutputs {
    fn status(&self, _: &ScriptRun) -> Option<ScriptReport> {
        None
    }
    fn plan(&self, _: &ScriptRun) -> Option<PlanPreview> {
        None
    }
}

/// Runs every status and plan script when asked, through a CLI.
pub struct LiveScripts<'a>(pub &'a dyn EngineCli);

impl ScriptOutputs for LiveScripts<'_> {
    fn status(&self, run: &ScriptRun) -> Option<ScriptReport> {
        Some(run_status(self.0, run))
    }
    fn plan(&self, run: &ScriptRun) -> Option<PlanPreview> {
        Some(run_plan(self.0, run))
    }
}

#[cfg(test)]
mod tests;
