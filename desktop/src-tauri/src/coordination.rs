//! Coordination surface I: scratchpads + todos over a SEPARATE
//! `coordination.db` (SQLite via rusqlite, bundled) in the app-config dir,
//! next to settings.json. It is the app's OWN database and shares a file with
//! nothing else: agent scratchpads are working memory for a project, and
//! mixing them into a database someone's real notes live in is how you lose
//! the notes.
//!
//! Everything here is pure logic over one `Connection` behind a mutex: the
//! HTTP shape lives in `coordination_http.rs`, the two Tauri commands for the
//! read-only rail list are at the bottom of this file.
//!
//! Lock discipline (result): the connection mutex is the ONLY lock
//! this module takes, every operation runs inside `with_conn`, nothing under
//! the guard calls back into another self-locking method, and nothing under
//! it unwraps — a poisoned mutex is `CoordError::Internal`, never a panic.
//!
//! Rules that are fixed, not choices:
//! - every write records `actor` (the caller's `CHAPPA_AI_PROCESS_ID`, or
//!   `"user"` when absent) as `updated_by`;
//! - revision guards: `expected_revision` optional on append/append_section,
//!   REQUIRED on write/edit/rename/clear/tags; a mismatch is a typed
//!   `revision_conflict` error carrying `current_revision` — never a silent
//!   overwrite;
//! - markdown sections are the edit unit: `append_section` only under an
//!   EXISTING heading, section resolution = exact text match after trimming,
//!   duplicates resolve to the FIRST;
//! - a leading H1 in written content overrides `name`;
//! - todo locks are leases by ACTOR id; a foreign live lock refuses edits
//!   with a typed `locked` error; expired leases are ignored;
//! - blocker cycles are refused; `is_blocked` is DERIVED from open blockers;
//! - transfer clears blockers + lock and keeps comments.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const DB_FILE: &str = "coordination.db";
/// The schema this build writes. Migrations only ever ADD from here on.
/// 1 = scratchpads + todos; 2 = timers (`timers`,
/// `timer_firings`); 3 = timer hardening (uuid-pinned timer targets, the
/// `timer_firings(process_id, at)` index). Every bump is additive.
pub const SCHEMA_VERSION: i64 = 3;
/// Default todo lock lease (the MCP default is 300 s).
pub const DEFAULT_LEASE_MS: u64 = 300_000;
pub const MAX_LEASE_MS: u64 = 24 * 3_600_000;
pub const DEFAULT_LIST_LIMIT: usize = 50;
pub const MAX_LIST_LIMIT: usize = 200;
/// The actor every write is attributed to when the request carries none.
pub const USER_ACTOR: &str = "user";

pub const PRIORITIES: [&str; 4] = ["low", "normal", "high", "urgent"];
pub const STATUSES: [&str; 3] = ["open", "in_progress", "done"];
/// `current_content` on a `revision_conflict` is capped here (bytes, on a
/// char boundary) and flagged `current_content_truncated` — a 40 KB pad must
/// not ride along on every stale write.
pub const CONFLICT_CONTENT_CAP: usize = 8 * 1024;
/// `scratchpad_history` rows kept per pad (oldest pruned on insert).
pub const HISTORY_CAP: i64 = 500;

// ---- errors -----------------------------------------------------------------

/// Typed failures. The HTTP layer maps each variant to a status and the JSON
/// shape agents key on (`error` is a stable machine word; `message` is the
/// human line).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoordError {
    NotFound(String),
    /// `expected_revision` did not match. `current_content` is filled only
    /// for the content-bearing scratchpad ops (write/edit/clear — the caller
    /// re-bases), capped at [`CONFLICT_CONTENT_CAP`] with
    /// `current_content_truncated`; `None` for rename/tags/delete and todos.
    RevisionConflict {
        current_revision: i64,
        current_content: Option<String>,
        current_content_truncated: bool,
    },
    /// A foreign, unexpired todo lock.
    Locked {
        locked_by: String,
        lock_expires_at: i64,
    },
    /// Adding the blocker would make a cycle.
    Cycle(String),
    /// Bad arguments (missing heading, out-of-range lines, unknown status…).
    Invalid(String),
    Internal(String),
}

impl CoordError {
    pub fn status(&self) -> u16 {
        match self {
            CoordError::NotFound(_) => 404,
            CoordError::RevisionConflict { .. } | CoordError::Locked { .. } | CoordError::Cycle(_) => 409,
            CoordError::Invalid(_) => 400,
            CoordError::Internal(_) => 500,
        }
    }

    /// The wire shape: `{error, message, ...variant fields}`.
    pub fn to_json(&self) -> Value {
        match self {
            CoordError::NotFound(m) => json!({"error": "not_found", "message": m}),
            CoordError::RevisionConflict { current_revision, current_content, current_content_truncated } => {
                let mut v = json!({
                    "error": "revision_conflict",
                    "message": format!("expected_revision does not match the current revision {current_revision}; re-read and retry"),
                    "current_revision": current_revision,
                });
                if let Some(content) = current_content {
                    v["current_content"] = json!(content);
                    v["current_content_truncated"] = json!(current_content_truncated);
                }
                v
            }
            CoordError::Locked { locked_by, lock_expires_at } => json!({
                "error": "locked",
                "message": format!("todo is locked by actor {locked_by} until {lock_expires_at} (unix ms)"),
                "locked_by": locked_by,
                "lock_expires_at": lock_expires_at,
            }),
            CoordError::Cycle(m) => json!({"error": "cycle", "message": m}),
            CoordError::Invalid(m) => json!({"error": "invalid", "message": m}),
            CoordError::Internal(m) => json!({"error": "internal", "message": m}),
        }
    }
}

impl std::fmt::Display for CoordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_json()["message"].as_str().unwrap_or("error"))
    }
}

impl From<rusqlite::Error> for CoordError {
    fn from(err: rusqlite::Error) -> Self {
        CoordError::Internal(format!("sqlite: {err}"))
    }
}

impl From<CoordError> for String {
    fn from(err: CoordError) -> Self {
        err.to_string()
    }
}

pub type Result<T> = std::result::Result<T, CoordError>;

/// Unix epoch ms as SQLite stores it (`i64`). `pub(crate)`: the timer
/// service's wall clock reads the same function.
pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---- store ------------------------------------------------------------------

/// Managed state: one connection behind a mutex. `Option` because `Default`
/// must work before the app-config path is known (same shape as `Projects`);
/// every operation on an uninitialized store is `Internal`.
#[derive(Clone, Default)]
pub struct Coordination {
    inner: Arc<Mutex<Option<Connection>>>,
}

impl Coordination {
    /// `<app-config>/coordination.db`. Errors are logged, not fatal: the app
    /// still runs, only the coordination tools answer 500.
    pub fn init(&self, config_dir: &Path) {
        if let Err(err) = self.open(config_dir.join(DB_FILE)) {
            log::error!("[coordination] cannot open {DB_FILE}: {err}");
            eprintln!("[coordination] cannot open {DB_FILE}: {err}");
        }
    }

    /// Open (creating + migrating) the database at `path`.
    pub fn open(&self, path: PathBuf) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| CoordError::Internal(e.to_string()))?;
        }
        let conn = open_db(&path)?;
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| CoordError::Internal("coordination state poisoned".into()))?;
        *guard = Some(conn);
        Ok(())
    }

    /// In-memory store (tests).
    pub fn in_memory() -> Result<Self> {
        let this = Self::default();
        let conn = Connection::open_in_memory()?;
        migrate(&conn)?;
        let mut guard = this
            .inner
            .lock()
            .map_err(|_| CoordError::Internal("coordination state poisoned".into()))?;
        *guard = Some(conn);
        drop(guard);
        Ok(this)
    }

    /// Run `f` inside one transaction on the connection. The ONE lock in
    /// this module; `f` must not call back into `Coordination`.
    ///
    /// `pub(crate)` so the timer store (`timers.rs`) shares the same
    /// connection and the same discipline instead of opening a second one.
    pub(crate) fn with_tx<T>(&self, f: impl FnOnce(&Transaction<'_>) -> Result<T>) -> Result<T> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| CoordError::Internal("coordination state poisoned".into()))?;
        let conn = guard
            .as_mut()
            .ok_or_else(|| CoordError::Internal("coordination store not initialized".into()))?;
        // Immediate: take the write lock up front so a read-then-write
        // operation can never be upgraded out from under by another
        // connection mid-transaction (busy_timeout covers the wait).
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let out = f(&tx)?;
        tx.commit()?;
        Ok(out)
    }

    /// `PRAGMA user_version` (tests assert the migration is idempotent).
    pub fn schema_version(&self) -> Result<i64> {
        self.with_tx(|tx| Ok(user_version(tx)?))
    }
}

fn open_db(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(Duration::from_secs(2))?;
    // WAL: readers never block the writer; the value comes back as a string.
    let _: String = conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    migrate(&conn)?;
    Ok(conn)
}

fn user_version(conn: &Connection) -> rusqlite::Result<i64> {
    conn.pragma_query_value(None, "user_version", |r| r.get(0))
}

/// `user_version` 0 → 1 creates everything. A future bump adds columns only
/// (never rewrites rows). Re-opening an up-to-date file is a no-op.
fn migrate(conn: &Connection) -> Result<()> {
    let version = user_version(conn)?;
    if version < 1 {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS scratchpads (
                id INTEGER PRIMARY KEY,
                project_id INTEGER NULL,
                name TEXT NOT NULL,
                content TEXT NOT NULL DEFAULT '',
                revision INTEGER NOT NULL DEFAULT 1,
                tags TEXT NOT NULL DEFAULT '[]',
                archived INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                updated_by TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS scratchpad_history (
                id INTEGER PRIMARY KEY,
                scratchpad_id INTEGER NOT NULL,
                revision INTEGER NOT NULL,
                actor TEXT NOT NULL,
                op TEXT NOT NULL,
                at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS scratchpad_history_pad ON scratchpad_history(scratchpad_id);
            CREATE TABLE IF NOT EXISTS todos (
                id INTEGER PRIMARY KEY,
                project_id INTEGER NOT NULL,
                title TEXT NOT NULL,
                body TEXT NOT NULL DEFAULT '',
                priority TEXT NOT NULL DEFAULT 'normal',
                status TEXT NOT NULL DEFAULT 'open',
                completed INTEGER NOT NULL DEFAULT 0,
                tags TEXT NOT NULL DEFAULT '[]',
                revision INTEGER NOT NULL DEFAULT 1,
                locked_by TEXT NULL,
                lock_expires_at INTEGER NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                updated_by TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS todo_blockers (
                todo_id INTEGER NOT NULL,
                blocked_by_todo_id INTEGER NOT NULL,
                PRIMARY KEY (todo_id, blocked_by_todo_id)
            );
            CREATE TABLE IF NOT EXISTS todo_comments (
                id INTEGER PRIMARY KEY,
                todo_id INTEGER NOT NULL,
                actor TEXT NOT NULL,
                body TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS todo_comments_todo ON todo_comments(todo_id);",
        )?;
    }
    // Timers. ADDITIVE — a v1 file keeps every scratchpad/todo row
    // and simply gains two tables.
    if version < 2 {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS timers (
                id INTEGER PRIMARY KEY,
                name TEXT NULL,
                kind TEXT NOT NULL,
                owner TEXT NOT NULL,
                project_id INTEGER NULL,
                delivery_process_id INTEGER NOT NULL,
                body TEXT NOT NULL,
                watch TEXT NOT NULL DEFAULT '[]',
                ignored TEXT NOT NULL DEFAULT '[]',
                delay_ms INTEGER NOT NULL DEFAULT 0,
                repeat_every_ms INTEGER NULL,
                next_fire_at INTEGER NULL,
                deadline_at INTEGER NULL,
                idle_ms INTEGER NOT NULL DEFAULT 0,
                confirm_ms INTEGER NOT NULL DEFAULT 0,
                rearm INTEGER NOT NULL DEFAULT 1,
                armed INTEGER NOT NULL DEFAULT 1,
                status TEXT NOT NULL,
                fired_count INTEGER NOT NULL DEFAULT 0,
                remaining_ms INTEGER NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS timers_status ON timers(status);
            CREATE TABLE IF NOT EXISTS timer_firings (
                id INTEGER PRIMARY KEY,
                timer_id INTEGER NOT NULL,
                at INTEGER NOT NULL,
                reason TEXT NOT NULL,
                process_id INTEGER NOT NULL,
                delivered INTEGER NOT NULL,
                detail TEXT NULL,
                coalesced_with INTEGER NULL,
                receipt TEXT NULL
            );
            CREATE INDEX IF NOT EXISTS timer_firings_timer ON timer_firings(timer_id);",
        )?;
    }
    // Timer targets are pinned by the process UUID (numeric
    // ids restart at 1 every launch), and the dedupe/prune queries walk
    // `timer_firings` by process and time. ADDITIVE: two nullable/defaulted
    // columns and one index; every existing row survives untouched.
    // `timer_firings.delivered` keeps its NOT NULL and gains two values:
    // 2 = in flight (persisted BEFORE the delivery is attempted), 3 =
    // coalesced with another firing (`coalesced_with` = that firing's id).
    if version < 3 {
        conn.execute_batch(
            "ALTER TABLE timers ADD COLUMN delivery_uuid TEXT NULL;
            ALTER TABLE timers ADD COLUMN watch_uuids TEXT NOT NULL DEFAULT '[]';
            CREATE INDEX IF NOT EXISTS timer_firings_process ON timer_firings(process_id, at);",
        )?;
    }
    if version < SCHEMA_VERSION {
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    }
    Ok(())
}

// ---- markdown ---------------------------------------------------------------

/// One markdown heading: 0-based line, level 1..=6, text after the hashes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Heading {
    pub line: usize,
    pub level: u8,
    pub text: String,
}

/// Content as lines. A trailing newline does not produce a phantom empty
/// last line; empty content is zero lines.
pub fn lines_of(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = content.split('\n').collect();
    if content.ends_with('\n') {
        lines.pop();
    }
    lines
}

/// True for a line that CommonMark treats as an indented code block: four
/// or more leading spaces, or a leading tab (tabs count as four columns).
fn is_indented_code(line: &str) -> bool {
    let mut cols = 0;
    for c in line.chars() {
        match c {
            ' ' => cols += 1,
            '\t' => cols += 4,
            _ => break,
        }
        if cols >= 4 {
            return true;
        }
    }
    false
}

/// ATX heading → `(level, text)`. Up to three spaces of indent (four is an
/// indented code block); a trailing `#` run is the CommonMark closing
/// sequence only when a space precedes it — `## C#` keeps its text,
/// `## Foo ##` is `Foo`.
fn parse_heading(line: &str) -> Option<(u8, String)> {
    if is_indented_code(line) {
        return None;
    }
    let trimmed = line.trim_start();
    let hashes = trimmed.bytes().take_while(|b| *b == b'#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &trimmed[hashes..];
    if !(rest.is_empty() || rest.starts_with(' ') || rest.starts_with('\t')) {
        return None;
    }
    let text = rest.trim();
    let without_closing = text.trim_end_matches('#');
    let text = if without_closing.len() != text.len()
        && (without_closing.is_empty() || without_closing.ends_with([' ', '\t']))
    {
        without_closing.trim_end()
    } else {
        text
    };
    Some((hashes as u8, text.to_owned()))
}

/// Every heading outside fenced and indented code blocks.
pub fn headings(content: &str) -> Vec<Heading> {
    let mut out = Vec::new();
    let mut in_fence = false;
    for (i, line) in lines_of(content).iter().enumerate() {
        let t = line.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        // parse_heading already refuses indented code lines.
        if let Some((level, text)) = parse_heading(line) {
            out.push(Heading { line: i, level, text });
        }
    }
    out
}

/// The section under `heading`: `(heading line, end line exclusive, level)`.
/// Resolution is exact text match after trimming (the whole heading line
/// `## Foo` and the bare text `Foo` both match); duplicates → the FIRST.
pub fn section_range(content: &str, heading: &str) -> Option<(usize, usize)> {
    let want = heading.trim();
    let want_text = parse_heading(want).map(|(_, t)| t);
    let all = headings(content);
    let idx = all.iter().position(|h| {
        h.text == want || want_text.as_deref() == Some(h.text.as_str())
    })?;
    let start = all[idx].line;
    let level = all[idx].level;
    let end = all[idx + 1..]
        .iter()
        .find(|h| h.level <= level)
        .map(|h| h.line)
        .unwrap_or_else(|| lines_of(content).len());
    Some((start, end))
}

fn join(lines: &[&str]) -> String {
    let mut out = lines.join("\n");
    if !lines.is_empty() {
        out.push('\n');
    }
    out
}

/// Append `extra` at the end of the section under `heading` (before the
/// next heading, keeping the section's trailing blank lines after it).
pub fn append_under(content: &str, heading: &str, extra: &str) -> Result<String> {
    let (start, end) = section_range(content, heading).ok_or_else(|| {
        CoordError::Invalid(format!("no section with heading `{}` — append_section only appends under an EXISTING heading", heading.trim()))
    })?;
    let lines = lines_of(content);
    let mut cut = end;
    while cut > start + 1 && lines[cut - 1].trim().is_empty() {
        cut -= 1;
    }
    let mut out: Vec<&str> = lines[..cut].to_vec();
    let extra_lines = lines_of(extra);
    out.extend(extra_lines.iter().copied());
    out.extend(lines[cut..].iter().copied());
    Ok(join(&out))
}

/// Replace the section under `heading`. Content starting with a heading
/// replaces the whole section (heading included); otherwise the matched
/// heading line is kept and the body under it is replaced.
pub fn replace_section(content: &str, heading: &str, replacement: &str) -> Result<String> {
    let (start, end) = section_range(content, heading)
        .ok_or_else(|| CoordError::Invalid(format!("no section with heading `{}`", heading.trim())))?;
    let lines = lines_of(content);
    let keep_heading = parse_heading(replacement.lines().next().unwrap_or("")).is_none();
    let mut out: Vec<&str> = lines[..start].to_vec();
    if keep_heading {
        out.push(lines[start]);
    }
    out.extend(lines_of(replacement).iter().copied());
    out.extend(lines[end..].iter().copied());
    Ok(join(&out))
}

/// Replace 1-based inclusive `start_line..=end_line`, bounds-checked.
pub fn replace_lines(content: &str, start_line: usize, end_line: usize, replacement: &str) -> Result<String> {
    let lines = lines_of(content);
    if start_line == 0 || end_line < start_line || end_line > lines.len() {
        return Err(CoordError::Invalid(format!(
            "line range {start_line}..={end_line} is out of bounds (1-based inclusive, {} lines)",
            lines.len()
        )));
    }
    let mut out: Vec<&str> = lines[..start_line - 1].to_vec();
    out.extend(lines_of(replacement).iter().copied());
    out.extend(lines[end_line..].iter().copied());
    Ok(join(&out))
}

/// A leading `# Title` (first non-blank line) overrides the given name.
pub fn title_override(content: &str) -> Option<String> {
    let first = content.lines().find(|l| !l.trim().is_empty())?;
    match parse_heading(first) {
        Some((1, text)) if !text.is_empty() => Some(text),
        _ => None,
    }
}

// ---- rows -------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Scratchpad {
    pub scratchpad_id: i64,
    pub project_id: Option<i64>,
    pub name: String,
    pub content: String,
    pub revision: i64,
    pub tags: Vec<String>,
    pub archived: bool,
    pub created_at: i64,
    pub updated_at: i64,
    pub updated_by: String,
}

impl Scratchpad {
    fn from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        let tags: String = r.get("tags")?;
        Ok(Self {
            scratchpad_id: r.get("id")?,
            project_id: r.get("project_id")?,
            name: r.get("name")?,
            content: r.get("content")?,
            revision: r.get("revision")?,
            tags: serde_json::from_str(&tags).unwrap_or_default(),
            archived: r.get::<_, i64>("archived")? != 0,
            created_at: r.get("created_at")?,
            updated_at: r.get("updated_at")?,
            updated_by: r.get("updated_by")?,
        })
    }

    /// The list row: everything but the content, plus a line count.
    pub fn summary(&self) -> Value {
        json!({
            "scratchpad_id": self.scratchpad_id,
            "project_id": self.project_id,
            "name": self.name,
            "revision": self.revision,
            "tags": self.tags,
            "archived": self.archived,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
            "updated_by": self.updated_by,
            "line_count": lines_of(&self.content).len(),
            "bytes": self.content.len(),
        })
    }

    fn receipt(&self) -> Value {
        json!({"scratchpad_id": self.scratchpad_id, "project_id": self.project_id, "revision": self.revision})
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Todo {
    pub todo_id: i64,
    pub project_id: i64,
    pub title: String,
    pub body: String,
    pub priority: String,
    pub status: String,
    pub completed: bool,
    pub tags: Vec<String>,
    pub revision: i64,
    pub locked_by: Option<String>,
    pub lock_expires_at: Option<i64>,
    /// Derived: any blocker that is not completed.
    pub is_blocked: bool,
    pub blocker_ids: Vec<i64>,
    pub comment_count: i64,
    pub created_at: i64,
    pub updated_at: i64,
    pub updated_by: String,
}

impl Todo {
    fn from_row(r: &rusqlite::Row<'_>, now: i64) -> rusqlite::Result<Self> {
        let tags: String = r.get("tags")?;
        let locked_by: Option<String> = r.get("locked_by")?;
        let lock_expires_at: Option<i64> = r.get("lock_expires_at")?;
        // An expired lease is reported as no lock at all.
        let live = matches!(lock_expires_at, Some(exp) if exp > now) && locked_by.is_some();
        Ok(Self {
            todo_id: r.get("id")?,
            project_id: r.get("project_id")?,
            title: r.get("title")?,
            body: r.get("body")?,
            priority: r.get("priority")?,
            status: r.get("status")?,
            completed: r.get::<_, i64>("completed")? != 0,
            tags: serde_json::from_str(&tags).unwrap_or_default(),
            revision: r.get("revision")?,
            locked_by: if live { locked_by } else { None },
            lock_expires_at: if live { lock_expires_at } else { None },
            is_blocked: false,
            blocker_ids: Vec::new(),
            comment_count: 0,
            created_at: r.get("created_at")?,
            updated_at: r.get("updated_at")?,
            updated_by: r.get("updated_by")?,
        })
    }

    /// The list row (no body).
    pub fn summary(&self) -> Value {
        let mut v = serde_json::to_value(self).unwrap_or_default();
        if let Some(obj) = v.as_object_mut() {
            obj.remove("body");
        }
        v
    }

    fn receipt(&self) -> Value {
        json!({"todo_id": self.todo_id, "project_id": self.project_id, "revision": self.revision})
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Comment {
    pub comment_id: i64,
    pub todo_id: i64,
    pub actor: String,
    pub body: String,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Comment {
    fn from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            comment_id: r.get("id")?,
            todo_id: r.get("todo_id")?,
            actor: r.get("actor")?,
            body: r.get("body")?,
            created_at: r.get("created_at")?,
            updated_at: r.get("updated_at")?,
        })
    }
}

/// `response_mode`: slim receipts by default, `rich` = the full row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResponseMode {
    #[default]
    Slim,
    Rich,
}

impl ResponseMode {
    pub fn parse(s: Option<&str>) -> Result<Self> {
        match s {
            None | Some("slim") => Ok(Self::Slim),
            Some("rich") => Ok(Self::Rich),
            Some(other) => Err(CoordError::Invalid(format!("response_mode must be slim or rich (got `{other}`)"))),
        }
    }
}

fn tags_json(tags: &[String]) -> String {
    serde_json::to_string(tags).unwrap_or_else(|_| "[]".into())
}

fn normalize_tags(tags: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in tags {
        let t = t.trim().to_owned();
        if !t.is_empty() && !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

// ---- scratchpad arguments ---------------------------------------------------

/// How to read a pad.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ReadMode {
    /// Whole content (with an optional line window).
    #[default]
    Full,
    /// Heading outline only.
    Outline,
    /// One section by heading.
    Section(String),
}

impl ReadMode {
    /// Both spellings are accepted: `full | content | headings | section`
    /// and the older `full | outline | section | lines`.
    pub fn parse(mode: Option<&str>, section_heading: Option<&str>) -> Result<Self> {
        match mode {
            None | Some("full") | Some("content") | Some("lines") => Ok(Self::Full),
            Some("outline") | Some("headings") => Ok(Self::Outline),
            Some("section") => section_heading
                .map(|h| Self::Section(h.to_owned()))
                .ok_or_else(|| CoordError::Invalid("mode=section requires section_heading (or heading)".into())),
            Some(other) => Err(CoordError::Invalid(format!(
                "mode must be full | outline | section | lines (got `{other}`)"
            ))),
        }
    }
}

/// `scratchpad_edit` target: a section or a 1-based inclusive line range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditTarget {
    Section(String),
    Lines { start_line: usize, end_line: usize },
}

impl EditTarget {
    /// Accepts the short forms `{heading}` / `{start_line, end_line}` and
    /// the tagged forms `{type: "section", section_heading}` or
    /// `{type: "line_range", offset, limit}` (0-based offset + count).
    pub fn parse(target: &Value) -> Result<Self> {
        if let Some(h) = target.get("heading").or_else(|| target.get("section_heading")).and_then(Value::as_str) {
            return Ok(Self::Section(h.to_owned()));
        }
        if let (Some(s), Some(e)) = (
            target.get("start_line").and_then(Value::as_u64),
            target.get("end_line").and_then(Value::as_u64),
        ) {
            return Ok(Self::Lines { start_line: s as usize, end_line: e as usize });
        }
        if let (Some(o), Some(l)) = (
            target.get("offset").and_then(Value::as_u64),
            target.get("limit").and_then(Value::as_u64),
        ) {
            if l == 0 {
                return Err(CoordError::Invalid("line_range limit must be >= 1".into()));
            }
            return Ok(Self::Lines { start_line: o as usize + 1, end_line: (o + l) as usize });
        }
        Err(CoordError::Invalid(
            "target must be {heading} | {start_line, end_line} (1-based inclusive) | {type: section, section_heading} | {type: line_range, offset, limit}".into(),
        ))
    }
}

#[derive(Debug, Clone, Default)]
pub struct ScratchpadListQuery {
    /// `Some(None)` = global pads only; `None` = every project.
    pub project_id: Option<Option<i64>>,
    /// With `project_id: Some(Some(p))`: also list the global pads (the
    /// rail shows both — a plain Claude Code session without
    /// `CHAPPA_AI_PROJECT_ID` writes global pads).
    pub include_global: bool,
    pub query: Option<String>,
    pub tags: Vec<String>,
    pub include_archived: bool,
    pub offset: usize,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Default)]
pub struct FindQuery {
    pub query: String,
    pub case_sensitive: bool,
    pub limit: Option<usize>,
    pub context_lines: Option<usize>,
    /// all | headings | content
    pub scope: Option<String>,
}

// ---- scratchpad operations --------------------------------------------------

fn load_pad(tx: &Transaction<'_>, id: i64) -> Result<Scratchpad> {
    tx.query_row("SELECT * FROM scratchpads WHERE id = ?1", params![id], Scratchpad::from_row)
        .optional()?
        .ok_or_else(|| CoordError::NotFound(format!("no such scratchpad: {id}")))
}

/// Whether a stale write on this op should carry the pad's content back
/// (only the ops that replace content can use it to re-base).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Guard {
    /// write / edit / clear: guard required, conflict carries content.
    Content,
    /// rename / tags / delete: guard required, conflict carries no content.
    Meta,
    /// append / append_section / transfer: guard optional, no content.
    Optional,
}

/// `current_content` for a conflict: capped at [`CONFLICT_CONTENT_CAP`] on
/// a char boundary, with the truncated flag.
fn conflict_content(content: &str) -> (String, bool) {
    if content.len() <= CONFLICT_CONTENT_CAP {
        return (content.to_owned(), false);
    }
    let cut = floor_char(content, CONFLICT_CONTENT_CAP);
    (content[..cut].to_owned(), true)
}

fn guard_revision(pad: &Scratchpad, expected: Option<i64>, guard: Guard) -> Result<()> {
    match expected {
        Some(e) if e != pad.revision => {
            let (current_content, current_content_truncated) = match guard {
                Guard::Content => {
                    let (c, t) = conflict_content(&pad.content);
                    (Some(c), t)
                }
                Guard::Meta | Guard::Optional => (None, false),
            };
            Err(CoordError::RevisionConflict {
                current_revision: pad.revision,
                current_content,
                current_content_truncated,
            })
        }
        None if guard != Guard::Optional => Err(CoordError::Invalid(
            "expected_revision is required (read the pad first; append/append_section are the guard-optional tools)".into(),
        )),
        _ => Ok(()),
    }
}

/// One history row per write (create/commit/archive/delete all go through
/// here), pruned to the newest [`HISTORY_CAP`] rows per pad.
fn record_history(tx: &Transaction<'_>, id: i64, revision: i64, actor: &str, op: &str, at: i64) -> Result<()> {
    tx.execute(
        "INSERT INTO scratchpad_history (scratchpad_id, revision, actor, op, at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id, revision, actor, op, at],
    )?;
    tx.execute(
        "DELETE FROM scratchpad_history WHERE scratchpad_id = ?1 AND id NOT IN (
            SELECT id FROM scratchpad_history WHERE scratchpad_id = ?1 ORDER BY id DESC LIMIT ?2)",
        params![id, HISTORY_CAP],
    )?;
    Ok(())
}

/// Write the new content/name/tags/project, bump the revision, record the
/// history op. Returns the fresh row. (`archived` is not written here —
/// archive is the one flag change that does NOT bump the revision.)
fn commit_pad(tx: &Transaction<'_>, pad: &Scratchpad, actor: &str, op: &str) -> Result<Scratchpad> {
    let now = now_ms();
    let revision = pad.revision + 1;
    tx.execute(
        "UPDATE scratchpads SET project_id = ?2, name = ?3, content = ?4, revision = ?5, tags = ?6, updated_at = ?7, updated_by = ?8 WHERE id = ?1",
        params![
            pad.scratchpad_id,
            pad.project_id,
            pad.name,
            pad.content,
            revision,
            tags_json(&pad.tags),
            now,
            actor
        ],
    )?;
    record_history(tx, pad.scratchpad_id, revision, actor, op, now)?;
    load_pad(tx, pad.scratchpad_id)
}

impl Coordination {
    /// `scratchpad_write` without `scratchpad_id`: create.
    pub fn scratchpad_create(
        &self,
        actor: &str,
        project_id: Option<i64>,
        name: &str,
        content: &str,
        tags: Vec<String>,
    ) -> Result<Scratchpad> {
        let name = title_override(content).unwrap_or_else(|| name.trim().to_owned());
        if name.is_empty() {
            return Err(CoordError::Invalid("name must not be empty".into()));
        }
        self.with_tx(|tx| {
            let now = now_ms();
            tx.execute(
                "INSERT INTO scratchpads (project_id, name, content, revision, tags, archived, created_at, updated_at, updated_by) VALUES (?1, ?2, ?3, 1, ?4, 0, ?5, ?5, ?6)",
                params![project_id, name, content, tags_json(&normalize_tags(tags)), now, actor],
            )?;
            let id = tx.last_insert_rowid();
            record_history(tx, id, 1, actor, "create", now)?;
            load_pad(tx, id)
        })
    }

    /// `scratchpad_write` with `scratchpad_id`: full overwrite at
    /// `expected_revision` (required). `tags: None` keeps the current tags.
    pub fn scratchpad_overwrite(
        &self,
        actor: &str,
        id: i64,
        name: &str,
        content: &str,
        tags: Option<Vec<String>>,
        expected_revision: Option<i64>,
    ) -> Result<Scratchpad> {
        self.with_tx(|tx| {
            let mut pad = load_pad(tx, id)?;
            guard_revision(&pad, expected_revision, Guard::Content)?;
            let name = title_override(content).unwrap_or_else(|| name.trim().to_owned());
            if name.is_empty() {
                return Err(CoordError::Invalid("name must not be empty".into()));
            }
            pad.name = name;
            pad.content = content.to_owned();
            if let Some(tags) = tags {
                pad.tags = normalize_tags(tags);
            }
            commit_pad(tx, &pad, actor, "write")
        })
    }

    pub fn scratchpad_get(&self, id: i64) -> Result<Scratchpad> {
        self.with_tx(|tx| load_pad(tx, id))
    }

    pub fn scratchpad_rename(&self, actor: &str, id: i64, name: &str, expected_revision: Option<i64>) -> Result<Scratchpad> {
        let name = name.trim().to_owned();
        if name.is_empty() {
            return Err(CoordError::Invalid("name must not be empty".into()));
        }
        self.with_tx(|tx| {
            let mut pad = load_pad(tx, id)?;
            guard_revision(&pad, expected_revision, Guard::Meta)?;
            pad.name = name;
            commit_pad(tx, &pad, actor, "rename")
        })
    }

    pub fn scratchpad_add_tags(&self, actor: &str, id: i64, tags: Vec<String>, expected_revision: Option<i64>) -> Result<Scratchpad> {
        self.with_tx(|tx| {
            let mut pad = load_pad(tx, id)?;
            guard_revision(&pad, expected_revision, Guard::Meta)?;
            let mut all = pad.tags.clone();
            all.extend(tags);
            pad.tags = normalize_tags(all);
            commit_pad(tx, &pad, actor, "add_tags")
        })
    }

    pub fn scratchpad_remove_tags(&self, actor: &str, id: i64, tags: Vec<String>, expected_revision: Option<i64>) -> Result<Scratchpad> {
        let remove = normalize_tags(tags);
        self.with_tx(|tx| {
            let mut pad = load_pad(tx, id)?;
            guard_revision(&pad, expected_revision, Guard::Meta)?;
            pad.tags.retain(|t| !remove.contains(t));
            commit_pad(tx, &pad, actor, "remove_tags")
        })
    }

    /// Append at the end (guard optional: last-writer-wins is acceptable for
    /// appends). A newline is inserted when the content does not end in one.
    pub fn scratchpad_append(&self, actor: &str, id: i64, content: &str, expected_revision: Option<i64>) -> Result<Scratchpad> {
        self.with_tx(|tx| {
            let mut pad = load_pad(tx, id)?;
            guard_revision(&pad, expected_revision, Guard::Optional)?;
            if !pad.content.is_empty() && !pad.content.ends_with('\n') {
                pad.content.push('\n');
            }
            pad.content.push_str(content);
            if !pad.content.ends_with('\n') {
                pad.content.push('\n');
            }
            commit_pad(tx, &pad, actor, "append")
        })
    }

    /// Append under an EXISTING heading; a missing heading is an error and
    /// changes nothing (the transaction never writes).
    pub fn scratchpad_append_section(
        &self,
        actor: &str,
        id: i64,
        heading: &str,
        content: &str,
        expected_revision: Option<i64>,
    ) -> Result<Scratchpad> {
        self.with_tx(|tx| {
            let mut pad = load_pad(tx, id)?;
            guard_revision(&pad, expected_revision, Guard::Optional)?;
            pad.content = append_under(&pad.content, heading, content)?;
            commit_pad(tx, &pad, actor, "append_section")
        })
    }

    /// Replace one section or line range. The name is NEVER touched here:
    /// the leading-H1 title override applies on create and full overwrite
    /// only, so an explicit `scratchpad_rename` survives later edits.
    pub fn scratchpad_edit(
        &self,
        actor: &str,
        id: i64,
        target: &EditTarget,
        content: &str,
        expected_revision: Option<i64>,
    ) -> Result<Scratchpad> {
        self.with_tx(|tx| {
            let mut pad = load_pad(tx, id)?;
            guard_revision(&pad, expected_revision, Guard::Content)?;
            pad.content = match target {
                EditTarget::Section(h) => replace_section(&pad.content, h, content)?,
                EditTarget::Lines { start_line, end_line } => replace_lines(&pad.content, *start_line, *end_line, content)?,
            };
            commit_pad(tx, &pad, actor, "edit")
        })
    }

    pub fn scratchpad_clear(&self, actor: &str, id: i64, expected_revision: Option<i64>) -> Result<Scratchpad> {
        self.with_tx(|tx| {
            let mut pad = load_pad(tx, id)?;
            guard_revision(&pad, expected_revision, Guard::Content)?;
            pad.content.clear();
            commit_pad(tx, &pad, actor, "clear")
        })
    }

    /// Delete at `expected_revision` (required). History is kept, with the
    /// same convention as every other op (the revision the op produced).
    pub fn scratchpad_delete(&self, actor: &str, id: i64, expected_revision: Option<i64>) -> Result<Scratchpad> {
        self.with_tx(|tx| {
            let pad = load_pad(tx, id)?;
            guard_revision(&pad, expected_revision, Guard::Meta)?;
            tx.execute("DELETE FROM scratchpads WHERE id = ?1", params![id])?;
            record_history(tx, id, pad.revision + 1, actor, "delete", now_ms())?;
            Ok(pad)
        })
    }

    /// Hide from the default list (or un-hide with `archived: false`). No
    /// guard and NO revision bump: the flag
    /// touches no content, so a concurrent guarded writer must not be
    /// bounced by it. The history row is logged at the current revision.
    pub fn scratchpad_archive(&self, actor: &str, id: i64, archived: bool) -> Result<Scratchpad> {
        self.with_tx(|tx| {
            let pad = load_pad(tx, id)?;
            let now = now_ms();
            tx.execute(
                "UPDATE scratchpads SET archived = ?2, updated_at = ?3, updated_by = ?4 WHERE id = ?1",
                params![id, archived as i64, now, actor],
            )?;
            record_history(tx, id, pad.revision, actor, if archived { "archive" } else { "unarchive" }, now)?;
            load_pad(tx, id)
        })
    }

    /// Move to another project (`None` = global). Only the project changes.
    pub fn scratchpad_transfer(
        &self,
        actor: &str,
        id: i64,
        target_project_id: Option<i64>,
        expected_revision: Option<i64>,
    ) -> Result<Scratchpad> {
        self.with_tx(|tx| {
            let mut pad = load_pad(tx, id)?;
            guard_revision(&pad, expected_revision, Guard::Optional)?;
            pad.project_id = target_project_id;
            commit_pad(tx, &pad, actor, "transfer")
        })
    }

    /// List rows (no content) with `matched_fields` + `snippet` when a
    /// query is given. Archived pads are hidden unless asked for. The
    /// project/archived filters run in SQL and the statement selects the
    /// summary columns only — `content` is fetched just for a text query
    /// (the snippet needs it); tags and the query itself stay in Rust
    /// (JSON array any-of, Unicode case folding).
    pub fn scratchpad_list(&self, q: &ScratchpadListQuery) -> Result<Value> {
        let needle = q.query.as_deref().map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty());
        let (scope_sql, scope_params) = pad_scope_sql(q.project_id, q.include_global);
        let archived_sql = if q.include_archived { "1" } else { "archived = 0" };
        let content_col = if needle.is_some() { "content" } else { "''" };
        let sql = format!(
            "SELECT id, project_id, name, revision, tags, archived, created_at, updated_at, updated_by, \
             {LINE_COUNT_SQL} AS line_count, length(CAST(content AS BLOB)) AS bytes, {content_col} AS content \
             FROM scratchpads WHERE {scope_sql} AND {archived_sql} ORDER BY updated_at DESC, id DESC"
        );
        let rows: Vec<(Value, Vec<String>, String, String)> = self.with_tx(|tx| {
            let mut stmt = tx.prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(scope_params.iter()), |r| {
                let tags_raw: String = r.get("tags")?;
                let tags: Vec<String> = serde_json::from_str(&tags_raw).unwrap_or_default();
                let name: String = r.get("name")?;
                let content: String = r.get("content")?;
                let summary = json!({
                    "scratchpad_id": r.get::<_, i64>("id")?,
                    "project_id": r.get::<_, Option<i64>>("project_id")?,
                    "name": name,
                    "revision": r.get::<_, i64>("revision")?,
                    "tags": tags,
                    "archived": r.get::<_, i64>("archived")? != 0,
                    "created_at": r.get::<_, i64>("created_at")?,
                    "updated_at": r.get::<_, i64>("updated_at")?,
                    "updated_by": r.get::<_, String>("updated_by")?,
                    "line_count": r.get::<_, i64>("line_count")?,
                    "bytes": r.get::<_, i64>("bytes")?,
                });
                Ok((summary, tags, name, content))
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        let mut matched: Vec<Value> = Vec::new();
        for (mut row, tags, name, content) in rows {
            if !q.tags.is_empty() && !q.tags.iter().any(|t| tags.contains(t)) {
                continue;
            }
            if let Some(needle) = &needle {
                let mut fields = Vec::new();
                if name.to_lowercase().contains(needle) {
                    fields.push("name");
                }
                let mut snippet = None;
                if let Some(line) = lines_of(&content).iter().find(|l| l.to_lowercase().contains(needle)) {
                    fields.push("content");
                    snippet = Some(snip(line, needle));
                }
                if fields.is_empty() {
                    continue;
                }
                row["matched_fields"] = json!(fields);
                row["snippet"] = json!(snippet);
            }
            matched.push(row);
        }
        let total = matched.len();
        let limit = q.limit.unwrap_or(DEFAULT_LIST_LIMIT).min(MAX_LIST_LIMIT);
        let page: Vec<Value> = matched.into_iter().skip(q.offset).take(limit).collect();
        Ok(json!({"scratchpads": page, "total": total, "offset": q.offset, "limit": limit}))
    }

    /// Distinct tags (optionally scoped to one project).
    pub fn scratchpad_tags(&self, project_id: Option<Option<i64>>) -> Result<Vec<String>> {
        let (scope_sql, scope_params) = pad_scope_sql(project_id, false);
        self.with_tx(|tx| {
            let mut stmt = tx.prepare(&format!("SELECT tags FROM scratchpads WHERE {scope_sql}"))?;
            let rows = stmt.query_map(rusqlite::params_from_iter(scope_params.iter()), |r| r.get::<_, String>(0))?;
            let mut out: Vec<String> = Vec::new();
            for tags in rows {
                let tags: Vec<String> = serde_json::from_str(&tags?).unwrap_or_default();
                for t in tags {
                    if !out.contains(&t) {
                        out.push(t);
                    }
                }
            }
            out.sort();
            Ok(out)
        })
    }

    /// `scratchpad_read`: the row (content per mode) plus revision metadata.
    pub fn scratchpad_read(&self, id: i64, mode: &ReadMode, offset: usize, limit: Option<usize>) -> Result<Value> {
        let pad = self.scratchpad_get(id)?;
        let all = lines_of(&pad.content);
        let mut out = pad.summary();
        match mode {
            ReadMode::Full => {
                let end = limit.map(|l| (offset + l).min(all.len())).unwrap_or(all.len());
                let window = if offset >= all.len() { &[][..] } else { &all[offset..end] };
                out["content"] = json!(join(window));
                out["offset"] = json!(offset);
                out["returned_lines"] = json!(window.len());
            }
            ReadMode::Outline => {
                out["headings"] = json!(headings(&pad.content));
            }
            ReadMode::Section(h) => {
                let (s, e) = section_range(&pad.content, h)
                    .ok_or_else(|| CoordError::Invalid(format!("no section with heading `{}`", h.trim())))?;
                out["content"] = json!(join(&all[s..e]));
                out["section"] = json!({"heading": h.trim(), "start_line": s + 1, "end_line": e});
            }
        }
        out["mode"] = json!(match mode {
            ReadMode::Full => "full",
            ReadMode::Outline => "outline",
            ReadMode::Section(_) => "section",
        });
        Ok(out)
    }

    /// Literal substring search with context.
    pub fn scratchpad_find(&self, id: i64, q: &FindQuery) -> Result<Value> {
        let needle_raw = q.query.trim();
        if needle_raw.is_empty() {
            return Err(CoordError::Invalid("query must not be empty".into()));
        }
        let pad = self.scratchpad_get(id)?;
        let limit = q.limit.unwrap_or(20).clamp(1, 100);
        let context = q.context_lines.unwrap_or(1).min(3);
        let scope = q.scope.as_deref().unwrap_or("all");
        if !["all", "headings", "content"].contains(&scope) {
            return Err(CoordError::Invalid("scope must be all | headings | content".into()));
        }
        let all = lines_of(&pad.content);
        let needle = if q.case_sensitive { needle_raw.to_owned() } else { needle_raw.to_lowercase() };
        let mut matches = Vec::new();
        let mut total = 0usize;
        for (i, line) in all.iter().enumerate() {
            let is_heading = parse_heading(line).is_some();
            if (scope == "headings" && !is_heading) || (scope == "content" && is_heading) {
                continue;
            }
            let hay = if q.case_sensitive { (*line).to_owned() } else { line.to_lowercase() };
            if !hay.contains(&needle) {
                continue;
            }
            total += 1;
            if matches.len() >= limit {
                continue;
            }
            let before: Vec<&str> = all[i.saturating_sub(context)..i].to_vec();
            let after: Vec<&str> = all[i + 1..(i + 1 + context).min(all.len())].to_vec();
            matches.push(json!({"line": i + 1, "text": line, "before": before, "after": after}));
        }
        let mut out = pad.summary();
        out["query"] = json!(needle_raw);
        out["matches"] = json!(matches);
        out["total_matches"] = json!(total);
        out["truncated"] = json!(total > limit);
        Ok(out)
    }

    /// Last `lines` lines (default 10).
    pub fn scratchpad_tail(&self, id: i64, lines: Option<usize>) -> Result<Value> {
        let pad = self.scratchpad_get(id)?;
        let all = lines_of(&pad.content);
        let n = lines.unwrap_or(10).min(all.len());
        let mut out = pad.summary();
        out["content"] = json!(join(&all[all.len() - n..]));
        out["lines"] = json!(n);
        Ok(out)
    }
}

/// `lines_of(content).len()` in SQL: newline count, plus one for an
/// unterminated last line; empty content is zero lines.
const LINE_COUNT_SQL: &str = "(CASE WHEN content = '' THEN 0 ELSE \
    length(content) - length(replace(content, char(10), '')) + \
    (CASE WHEN substr(content, -1, 1) = char(10) THEN 0 ELSE 1 END) END)";

/// The `WHERE` fragment (and its parameters) for a scratchpad project scope:
/// `None` = every project, `Some(None)` = global only, `Some(Some(p))` =
/// that project (plus the global pads when `include_global`).
fn pad_scope_sql(project_id: Option<Option<i64>>, include_global: bool) -> (String, Vec<i64>) {
    match project_id {
        None => ("1".to_owned(), vec![]),
        Some(None) => ("project_id IS NULL".to_owned(), vec![]),
        Some(Some(p)) if include_global => ("(project_id = ?1 OR project_id IS NULL)".to_owned(), vec![p]),
        Some(Some(p)) => ("project_id = ?1".to_owned(), vec![p]),
    }
}

/// A one-line snippet around the first match (≈80 chars each side).
fn snip(line: &str, needle_lower: &str) -> String {
    let lower = line.to_lowercase();
    let pos = lower.find(needle_lower).unwrap_or(0);
    let start = floor_char(line, pos.saturating_sub(80));
    let end = ceil_char(line, (pos + needle_lower.len() + 80).min(line.len()));
    let mut s = String::new();
    if start > 0 {
        s.push('…');
    }
    s.push_str(&line[start..end]);
    if end < line.len() {
        s.push('…');
    }
    s
}

fn floor_char(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

// ---- todo arguments ---------------------------------------------------------

/// `todo_update`: omitted fields are preserved.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TodoPatch {
    pub title: Option<String>,
    pub body: Option<String>,
    pub priority: Option<String>,
    pub status: Option<String>,
    pub tags: Option<Vec<String>>,
    pub expected_revision: Option<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct TodoListQuery {
    pub project_id: Option<i64>,
    pub status: Option<String>,
    pub completed: Option<bool>,
    pub is_blocked: Option<bool>,
    pub priority: Option<String>,
    pub query: Option<String>,
    pub tags: Vec<String>,
    /// `updated_desc` (default) | updated_asc | created_desc | created_asc |
    /// priority_desc | priority_asc | title
    pub sort: Option<String>,
    pub offset: usize,
    pub limit: Option<usize>,
}

/// `medium` is accepted as a synonym for `normal`.
pub fn parse_priority(p: &str) -> Result<String> {
    let p = p.trim().to_lowercase();
    let p = if p == "medium" { "normal".to_owned() } else { p };
    if PRIORITIES.contains(&p.as_str()) {
        Ok(p)
    } else {
        Err(CoordError::Invalid(format!("priority must be one of {} (got `{p}`)", PRIORITIES.join(" | "))))
    }
}

/// `completed` is accepted as a synonym for `done`.
pub fn parse_status(s: &str) -> Result<String> {
    let s = s.trim().to_lowercase();
    let s = if s == "completed" { "done".to_owned() } else { s };
    if STATUSES.contains(&s.as_str()) {
        Ok(s)
    } else {
        Err(CoordError::Invalid(format!("status must be one of {} (got `{s}`)", STATUSES.join(" | "))))
    }
}

// ---- todo operations --------------------------------------------------------

fn load_todo(tx: &Transaction<'_>, id: i64) -> Result<Todo> {
    let now = now_ms();
    let mut todo = tx
        .query_row("SELECT * FROM todos WHERE id = ?1", params![id], |r| Todo::from_row(r, now))
        .optional()?
        .ok_or_else(|| CoordError::NotFound(format!("no such todo: {id}")))?;
    decorate(tx, &mut todo)?;
    Ok(todo)
}

/// Fill the derived fields: blocker ids, `is_blocked` (any blocker not
/// completed), comment count.
fn decorate(tx: &Transaction<'_>, todo: &mut Todo) -> Result<()> {
    let mut stmt = tx.prepare(
        "SELECT b.blocked_by_todo_id, t.completed FROM todo_blockers b JOIN todos t ON t.id = b.blocked_by_todo_id WHERE b.todo_id = ?1 ORDER BY b.blocked_by_todo_id",
    )?;
    let rows = stmt.query_map(params![todo.todo_id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)? != 0)))?;
    todo.blocker_ids.clear();
    todo.is_blocked = false;
    for row in rows {
        let (id, completed) = row?;
        todo.blocker_ids.push(id);
        if !completed {
            todo.is_blocked = true;
        }
    }
    todo.comment_count = tx.query_row(
        "SELECT COUNT(*) FROM todo_comments WHERE todo_id = ?1",
        params![todo.todo_id],
        |r| r.get(0),
    )?;
    Ok(())
}

/// The lock rule: a lock held by ANOTHER actor that has not expired refuses
/// the edit. Own locks and expired leases pass.
fn guard_lock(todo: &Todo, actor: &str) -> Result<()> {
    match (&todo.locked_by, todo.lock_expires_at) {
        (Some(holder), Some(exp)) if holder != actor && exp > now_ms() => Err(CoordError::Locked {
            locked_by: holder.clone(),
            lock_expires_at: exp,
        }),
        _ => Ok(()),
    }
}

fn guard_todo_revision(todo: &Todo, expected: Option<i64>) -> Result<()> {
    match expected {
        Some(e) if e != todo.revision => Err(CoordError::RevisionConflict {
            current_revision: todo.revision,
            current_content: None,
            current_content_truncated: false,
        }),
        _ => Ok(()),
    }
}

fn commit_todo(tx: &Transaction<'_>, todo: &Todo, actor: &str) -> Result<Todo> {
    let now = now_ms();
    tx.execute(
        "UPDATE todos SET project_id = ?2, title = ?3, body = ?4, priority = ?5, status = ?6, completed = ?7, tags = ?8, revision = ?9, locked_by = ?10, lock_expires_at = ?11, updated_at = ?12, updated_by = ?13 WHERE id = ?1",
        params![
            todo.todo_id,
            todo.project_id,
            todo.title,
            todo.body,
            todo.priority,
            todo.status,
            todo.completed as i64,
            tags_json(&todo.tags),
            todo.revision + 1,
            todo.locked_by,
            todo.lock_expires_at,
            now,
            actor
        ],
    )?;
    load_todo(tx, todo.todo_id)
}

/// Ids of the todos that list `id` as a blocker (their `is_blocked` may
/// change when `id` does).
fn dependents(tx: &Transaction<'_>, id: i64) -> Result<Vec<i64>> {
    let mut stmt = tx.prepare("SELECT todo_id FROM todo_blockers WHERE blocked_by_todo_id = ?1 ORDER BY todo_id")?;
    let rows = stmt.query_map(params![id], |r| r.get::<_, i64>(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Would `todo` blocked-by `blocker` close a loop? True when `todo` is
/// reachable from `blocker` along blocked-by edges (or they are the same).
fn would_cycle(tx: &Transaction<'_>, todo: i64, blocker: i64) -> Result<bool> {
    if todo == blocker {
        return Ok(true);
    }
    let mut stack = vec![blocker];
    let mut seen = std::collections::HashSet::new();
    let mut stmt = tx.prepare("SELECT blocked_by_todo_id FROM todo_blockers WHERE todo_id = ?1")?;
    while let Some(cur) = stack.pop() {
        if !seen.insert(cur) {
            continue;
        }
        let next: Vec<i64> = stmt
            .query_map(params![cur], |r| r.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for n in next {
            if n == todo {
                return Ok(true);
            }
            stack.push(n);
        }
    }
    Ok(false)
}

fn set_blockers_in(tx: &Transaction<'_>, todo: &Todo, blockers: &[i64]) -> Result<()> {
    let mut wanted: Vec<i64> = Vec::new();
    for b in blockers {
        if !wanted.contains(b) {
            wanted.push(*b);
        }
    }
    tx.execute("DELETE FROM todo_blockers WHERE todo_id = ?1", params![todo.todo_id])?;
    for b in &wanted {
        let other = load_todo(tx, *b)?;
        if other.project_id != todo.project_id {
            return Err(CoordError::Invalid(format!(
                "blocker {b} belongs to project {} — blockers must be in the same project ({})",
                other.project_id, todo.project_id
            )));
        }
        if would_cycle(tx, todo.todo_id, *b)? {
            return Err(CoordError::Cycle(format!(
                "todo {} blocked by {b} would create a cycle",
                todo.todo_id
            )));
        }
        tx.execute(
            "INSERT INTO todo_blockers (todo_id, blocked_by_todo_id) VALUES (?1, ?2)",
            params![todo.todo_id, b],
        )?;
    }
    Ok(())
}

fn load_comment(tx: &Transaction<'_>, id: i64) -> Result<Comment> {
    tx.query_row("SELECT * FROM todo_comments WHERE id = ?1", params![id], Comment::from_row)
        .optional()?
        .ok_or_else(|| CoordError::NotFound(format!("no such comment: {id}")))
}

impl Coordination {
    pub fn todo_create(
        &self,
        actor: &str,
        project_id: i64,
        title: &str,
        body: &str,
        priority: Option<&str>,
        tags: Vec<String>,
    ) -> Result<Todo> {
        let title = title.trim().to_owned();
        if title.is_empty() {
            return Err(CoordError::Invalid("title must not be empty".into()));
        }
        let priority = parse_priority(priority.unwrap_or("normal"))?;
        self.with_tx(|tx| {
            let now = now_ms();
            tx.execute(
                "INSERT INTO todos (project_id, title, body, priority, status, completed, tags, revision, created_at, updated_at, updated_by) VALUES (?1, ?2, ?3, ?4, 'open', 0, ?5, 1, ?6, ?6, ?7)",
                params![project_id, title, body, priority, tags_json(&normalize_tags(tags)), now, actor],
            )?;
            load_todo(tx, tx.last_insert_rowid())
        })
    }

    pub fn todo_get(&self, id: i64) -> Result<Todo> {
        self.with_tx(|tx| load_todo(tx, id))
    }

    pub fn todo_update(&self, actor: &str, id: i64, patch: &TodoPatch) -> Result<(Todo, Vec<&'static str>)> {
        let priority = patch.priority.as_deref().map(parse_priority).transpose()?;
        let status = patch.status.as_deref().map(parse_status).transpose()?;
        self.with_tx(|tx| {
            let mut todo = load_todo(tx, id)?;
            guard_lock(&todo, actor)?;
            guard_todo_revision(&todo, patch.expected_revision)?;
            let mut changed = Vec::new();
            if let Some(t) = &patch.title {
                let t = t.trim();
                if t.is_empty() {
                    return Err(CoordError::Invalid("title must not be empty".into()));
                }
                todo.title = t.to_owned();
                changed.push("title");
            }
            if let Some(b) = &patch.body {
                todo.body = b.clone();
                changed.push("body");
            }
            if let Some(p) = priority {
                todo.priority = p;
                changed.push("priority");
            }
            if let Some(s) = status {
                todo.completed = s == "done";
                todo.status = s;
                changed.push("status");
                changed.push("completed");
            }
            if let Some(tags) = &patch.tags {
                todo.tags = normalize_tags(tags.clone());
                changed.push("tags");
            }
            Ok((commit_todo(tx, &todo, actor)?, changed))
        })
    }

    pub fn todo_add_tag(&self, actor: &str, id: i64, tag: &str) -> Result<Todo> {
        let tag = tag.trim().to_owned();
        if tag.is_empty() {
            return Err(CoordError::Invalid("tag must not be empty".into()));
        }
        self.with_tx(|tx| {
            let mut todo = load_todo(tx, id)?;
            guard_lock(&todo, actor)?;
            if !todo.tags.contains(&tag) {
                todo.tags.push(tag);
            }
            commit_todo(tx, &todo, actor)
        })
    }

    pub fn todo_remove_tag(&self, actor: &str, id: i64, tag: &str) -> Result<Todo> {
        let tag = tag.trim().to_owned();
        self.with_tx(|tx| {
            let mut todo = load_todo(tx, id)?;
            guard_lock(&todo, actor)?;
            todo.tags.retain(|t| *t != tag);
            commit_todo(tx, &todo, actor)
        })
    }

    pub fn todo_set_blockers(&self, actor: &str, id: i64, blockers: &[i64]) -> Result<Todo> {
        self.with_tx(|tx| {
            let todo = load_todo(tx, id)?;
            guard_lock(&todo, actor)?;
            set_blockers_in(tx, &todo, blockers)?;
            commit_todo(tx, &todo, actor)
        })
    }

    pub fn todo_add_blocker(&self, actor: &str, id: i64, blocker: i64) -> Result<Todo> {
        self.with_tx(|tx| {
            let todo = load_todo(tx, id)?;
            guard_lock(&todo, actor)?;
            let mut all = todo.blocker_ids.clone();
            if !all.contains(&blocker) {
                all.push(blocker);
            }
            set_blockers_in(tx, &todo, &all)?;
            commit_todo(tx, &todo, actor)
        })
    }

    pub fn todo_remove_blocker(&self, actor: &str, id: i64, blocker: i64) -> Result<Todo> {
        self.with_tx(|tx| {
            let todo = load_todo(tx, id)?;
            guard_lock(&todo, actor)?;
            let all: Vec<i64> = todo.blocker_ids.iter().copied().filter(|b| *b != blocker).collect();
            set_blockers_in(tx, &todo, &all)?;
            commit_todo(tx, &todo, actor)
        })
    }

    /// Mark complete/incomplete. Releases the caller's OWN lock unless
    /// `release_lock: false`. Returns the row and the ids of todos blocked by
    /// it (whose `is_blocked` just changed).
    pub fn todo_complete(&self, actor: &str, id: i64, completed: bool, release_lock: bool) -> Result<(Todo, Vec<i64>)> {
        self.with_tx(|tx| {
            let mut todo = load_todo(tx, id)?;
            guard_lock(&todo, actor)?;
            todo.completed = completed;
            todo.status = if completed {
                "done".to_owned()
            } else if todo.status == "done" {
                "open".to_owned()
            } else {
                todo.status.clone()
            };
            if release_lock && todo.locked_by.as_deref() == Some(actor) {
                todo.locked_by = None;
                todo.lock_expires_at = None;
            }
            let affected = dependents(tx, id)?;
            Ok((commit_todo(tx, &todo, actor)?, affected))
        })
    }

    /// Take (or renew) the lease. A foreign live lock is `Locked`.
    pub fn todo_lock(&self, actor: &str, id: i64, lease_ms: Option<u64>) -> Result<Todo> {
        let lease = lease_ms.unwrap_or(DEFAULT_LEASE_MS).clamp(1, MAX_LEASE_MS) as i64;
        self.with_tx(|tx| {
            let mut todo = load_todo(tx, id)?;
            guard_lock(&todo, actor)?;
            todo.locked_by = Some(actor.to_owned());
            todo.lock_expires_at = Some(now_ms() + lease);
            commit_todo(tx, &todo, actor)
        })
    }

    /// Release a lock you own (an expired or absent lock is a no-op success).
    pub fn todo_unlock(&self, actor: &str, id: i64) -> Result<Todo> {
        self.with_tx(|tx| {
            let mut todo = load_todo(tx, id)?;
            guard_lock(&todo, actor)?;
            if todo.locked_by.is_none() {
                return Ok(todo);
            }
            todo.locked_by = None;
            todo.lock_expires_at = None;
            commit_todo(tx, &todo, actor)
        })
    }

    /// Move to another project: comments kept, blockers (both directions)
    /// and lock cleared. Returns the row and the formerly dependent ids.
    pub fn todo_transfer(&self, actor: &str, id: i64, target_project_id: i64) -> Result<(Todo, Vec<i64>)> {
        self.with_tx(|tx| {
            let mut todo = load_todo(tx, id)?;
            guard_lock(&todo, actor)?;
            let affected = dependents(tx, id)?;
            tx.execute(
                "DELETE FROM todo_blockers WHERE todo_id = ?1 OR blocked_by_todo_id = ?1",
                params![id],
            )?;
            todo.project_id = target_project_id;
            todo.locked_by = None;
            todo.lock_expires_at = None;
            Ok((commit_todo(tx, &todo, actor)?, affected))
        })
    }

    pub fn todo_delete(&self, actor: &str, id: i64) -> Result<(Todo, Vec<i64>)> {
        self.with_tx(|tx| {
            let todo = load_todo(tx, id)?;
            guard_lock(&todo, actor)?;
            let affected = dependents(tx, id)?;
            tx.execute("DELETE FROM todo_blockers WHERE todo_id = ?1 OR blocked_by_todo_id = ?1", params![id])?;
            tx.execute("DELETE FROM todo_comments WHERE todo_id = ?1", params![id])?;
            tx.execute("DELETE FROM todos WHERE id = ?1", params![id])?;
            Ok((todo, affected))
        })
    }

    /// Summaries. `sort` is validated first; project/status/completed/
    /// priority filter in SQL (`body` is only selected for a text query);
    /// `is_blocked` comes from one blocker query, tags/query stay in Rust;
    /// the page alone is decorated (blocker ids, comment count).
    pub fn todo_list(&self, q: &TodoListQuery) -> Result<Value> {
        let sort = q.sort.as_deref().unwrap_or("updated_desc");
        let order = match sort {
            "updated_desc" => "updated_at DESC, id DESC",
            "updated_asc" => "updated_at ASC, id ASC",
            "created_desc" => "created_at DESC, id DESC",
            "created_asc" => "created_at ASC, id ASC",
            "priority_desc" => "priority_rank DESC, id ASC",
            "priority_asc" => "priority_rank ASC, id ASC",
            "title" => "lower(title) ASC, id ASC",
            other => {
                return Err(CoordError::Invalid(format!(
                    "sort must be updated_desc | updated_asc | created_desc | created_asc | priority_desc | priority_asc | title (got `{other}`)"
                )))
            }
        };
        let status = q.status.as_deref().map(parse_status).transpose()?;
        let priority = q.priority.as_deref().map(parse_priority).transpose()?;
        let needle = q.query.as_deref().map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty());

        let mut where_sql: Vec<String> = vec!["1".into()];
        let mut params: Vec<rusqlite::types::Value> = Vec::new();
        if let Some(p) = q.project_id {
            params.push(p.into());
            where_sql.push(format!("project_id = ?{}", params.len()));
        }
        if let Some(s) = status {
            params.push(s.into());
            where_sql.push(format!("status = ?{}", params.len()));
        }
        if let Some(c) = q.completed {
            params.push((c as i64).into());
            where_sql.push(format!("completed = ?{}", params.len()));
        }
        if let Some(p) = priority {
            params.push(p.into());
            where_sql.push(format!("priority = ?{}", params.len()));
        }
        let body_col = if needle.is_some() { "body" } else { "'' AS body" };
        let sql = format!(
            "SELECT id, project_id, title, {body_col}, priority, status, completed, tags, revision, locked_by, lock_expires_at, \
             created_at, updated_at, updated_by, \
             (CASE priority WHEN 'urgent' THEN 3 WHEN 'high' THEN 2 WHEN 'normal' THEN 1 ELSE 0 END) AS priority_rank \
             FROM todos WHERE {} ORDER BY {order}",
            where_sql.join(" AND ")
        );
        self.with_tx(|tx| {
            let now = now_ms();
            let mut stmt = tx.prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |r| Todo::from_row(r, now))?;
            let mut todos = rows.collect::<rusqlite::Result<Vec<_>>>()?;
            // `is_blocked` for every candidate from one query: the todos with
            // at least one blocker that is not completed.
            let blocked: std::collections::HashSet<i64> = {
                let mut stmt = tx.prepare(
                    "SELECT DISTINCT b.todo_id FROM todo_blockers b JOIN todos t ON t.id = b.blocked_by_todo_id WHERE t.completed = 0",
                )?;
                let rows = stmt.query_map([], |r| r.get::<_, i64>(0))?;
                rows.collect::<rusqlite::Result<_>>()?
            };
            for t in &mut todos {
                t.is_blocked = blocked.contains(&t.todo_id);
            }
            let comment_hits: std::collections::HashSet<i64> = match &needle {
                Some(n) => {
                    let mut stmt = tx.prepare("SELECT todo_id, body FROM todo_comments")?;
                    let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
                    let mut hits = std::collections::HashSet::new();
                    for row in rows {
                        let (id, body) = row?;
                        if body.to_lowercase().contains(n) {
                            hits.insert(id);
                        }
                    }
                    hits
                }
                None => Default::default(),
            };
            todos.retain(|t| {
                q.is_blocked.map_or(true, |b| t.is_blocked == b)
                    && (q.tags.is_empty() || q.tags.iter().any(|tag| t.tags.contains(tag)))
                    && needle.as_deref().map_or(true, |n| {
                        t.title.to_lowercase().contains(n)
                            || t.body.to_lowercase().contains(n)
                            || comment_hits.contains(&t.todo_id)
                    })
            });
            let total = todos.len();
            let limit = q.limit.unwrap_or(DEFAULT_LIST_LIMIT).min(MAX_LIST_LIMIT);
            let mut page: Vec<Value> = Vec::new();
            for t in todos.iter_mut().skip(q.offset).take(limit) {
                decorate(tx, t)?;
                page.push(t.summary());
            }
            Ok(json!({"todos": page, "total": total, "offset": q.offset, "limit": limit}))
        })
    }

    pub fn todo_tags(&self, project_id: Option<i64>) -> Result<Vec<String>> {
        self.with_tx(|tx| {
            let mut stmt = tx.prepare("SELECT tags FROM todos WHERE (?1 IS NULL OR project_id = ?1)")?;
            let rows = stmt.query_map(params![project_id], |r| r.get::<_, String>(0))?;
            let mut out: Vec<String> = Vec::new();
            for tags in rows {
                let tags: Vec<String> = serde_json::from_str(&tags?).unwrap_or_default();
                for t in tags {
                    if !out.contains(&t) {
                        out.push(t);
                    }
                }
            }
            out.sort();
            Ok(out)
        })
    }

    // ---- comments ----

    pub fn todo_comment_create(&self, actor: &str, todo_id: i64, body: &str) -> Result<Comment> {
        if body.trim().is_empty() {
            return Err(CoordError::Invalid("body must not be empty".into()));
        }
        self.with_tx(|tx| {
            load_todo(tx, todo_id)?;
            let now = now_ms();
            tx.execute(
                "INSERT INTO todo_comments (todo_id, actor, body, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?4)",
                params![todo_id, actor, body, now],
            )?;
            load_comment(tx, tx.last_insert_rowid())
        })
    }

    pub fn todo_comment_update(&self, _actor: &str, comment_id: i64, body: &str) -> Result<Comment> {
        if body.trim().is_empty() {
            return Err(CoordError::Invalid("body must not be empty".into()));
        }
        self.with_tx(|tx| {
            load_comment(tx, comment_id)?;
            tx.execute(
                "UPDATE todo_comments SET body = ?2, updated_at = ?3 WHERE id = ?1",
                params![comment_id, body, now_ms()],
            )?;
            load_comment(tx, comment_id)
        })
    }

    pub fn todo_comment_delete(&self, _actor: &str, comment_id: i64) -> Result<Comment> {
        self.with_tx(|tx| {
            let c = load_comment(tx, comment_id)?;
            tx.execute("DELETE FROM todo_comments WHERE id = ?1", params![comment_id])?;
            Ok(c)
        })
    }

    pub fn todo_comments(&self, todo_id: i64, offset: usize, limit: Option<usize>) -> Result<Value> {
        self.with_tx(|tx| {
            load_todo(tx, todo_id)?;
            let mut stmt = tx.prepare("SELECT * FROM todo_comments WHERE todo_id = ?1 ORDER BY id")?;
            let rows = stmt.query_map(params![todo_id], Comment::from_row)?;
            let all = rows.collect::<rusqlite::Result<Vec<_>>>()?;
            let total = all.len();
            let limit = limit.unwrap_or(DEFAULT_LIST_LIMIT).min(MAX_LIST_LIMIT);
            let page: Vec<Comment> = all.into_iter().skip(offset).take(limit).collect();
            Ok(json!({"todo_id": todo_id, "comments": page, "total": total, "offset": offset, "limit": limit}))
        })
    }
}

// ---- receipts ---------------------------------------------------------------

/// Slim receipt (`{id, project_id, revision}` + the changed fields) or the
/// full row under `response_mode: rich`.
pub fn pad_receipt(pad: &Scratchpad, mode: ResponseMode, changed: &[(&str, Value)]) -> Value {
    match mode {
        ResponseMode::Rich => serde_json::to_value(pad).unwrap_or_default(),
        ResponseMode::Slim => {
            let mut v = pad.receipt();
            for (k, val) in changed {
                v[*k] = val.clone();
            }
            v
        }
    }
}

pub fn todo_receipt(todo: &Todo, mode: ResponseMode, changed: &[(&str, Value)]) -> Value {
    match mode {
        ResponseMode::Rich => serde_json::to_value(todo).unwrap_or_default(),
        ResponseMode::Slim => {
            let mut v = todo.receipt();
            for (k, val) in changed {
                v[*k] = val.clone();
            }
            v
        }
    }
}

// ---- Tauri commands (the read-only rail list) --------------------

/// `list_scratchpads(project_id, include_global)`: the rail's rows for one
/// project (no content, unarchived only), plus the global pads when
/// `include_global` (a plain Claude Code session without
/// `CHAPPA_AI_PROJECT_ID` creates exactly those, and they must be visible).
#[tauri::command]
pub fn list_scratchpads(
    coordination: tauri::State<'_, Coordination>,
    project_id: Option<i64>,
    include_global: Option<bool>,
) -> std::result::Result<Vec<Value>, String> {
    let listing = coordination.scratchpad_list(&ScratchpadListQuery {
        project_id: Some(project_id),
        include_global: include_global.unwrap_or(false),
        limit: Some(MAX_LIST_LIMIT),
        ..Default::default()
    })?;
    Ok(listing["scratchpads"].as_array().cloned().unwrap_or_default())
}

/// `read_scratchpad(id)`: the full row for the modal.
#[tauri::command]
pub fn read_scratchpad(
    coordination: tauri::State<'_, Coordination>,
    id: i64,
) -> std::result::Result<Scratchpad, String> {
    Ok(coordination.scratchpad_get(id)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "# Title\n\nintro\n\n## Alpha\n\na1\n\n### Alpha.child\n\nac\n\n## Beta\n\nb1\n\n## Alpha\n\ndup\n";

    #[test]
    fn headings_skip_fences_and_resolve_sections() {
        let hs = headings("# A\n```\n# not a heading\n```\n## B\n#nospace\n");
        assert_eq!(hs.iter().map(|h| h.text.as_str()).collect::<Vec<_>>(), ["A", "B"]);
        // Nested child stays inside Alpha; the section ends at Beta (same level).
        assert_eq!(section_range(DOC, "Alpha"), Some((4, 12)));
        assert_eq!(section_range(DOC, "## Alpha"), Some((4, 12)), "full heading line matches too");
        assert_eq!(section_range(DOC, "Alpha.child"), Some((8, 12)));
        // Last section runs to EOF; duplicates resolve to the first.
        assert_eq!(section_range(DOC, "Beta"), Some((12, 16)));
        assert!(section_range(DOC, "Gamma").is_none());
        assert!(section_range(DOC, "alpha").is_none(), "exact after trim, not case-folded");
    }

    /// Indented code blocks (4 spaces / tab) are not headings,
    /// and a trailing `#` run is only stripped as a closing sequence when a
    /// space precedes it (`## C#` keeps its text).
    #[test]
    fn headings_skip_indented_code_and_keep_a_trailing_hash_without_a_space() {
        let hs = headings("# A\n    # indented code\n\t# tab code\n   # three spaces is a heading\n## C#\n## Foo ##\n# #\n");
        let texts: Vec<&str> = hs.iter().map(|h| h.text.as_str()).collect();
        assert_eq!(texts, ["A", "three spaces is a heading", "C#", "Foo", ""]);
        assert_eq!(parse_heading("## C#"), Some((2, "C#".into())));
        assert_eq!(parse_heading("## Foo ##"), Some((2, "Foo".into())));
        assert_eq!(parse_heading("## Foo\t##"), Some((2, "Foo".into())));
        assert_eq!(parse_heading("    # code"), None);
        assert_eq!(parse_heading("\t# code"), None);
        assert_eq!(section_range("# A\n\n    # not a section\n\n# B\n", "A"), Some((0, 4)));
        assert!(section_range("# A\n\n    # not a section\n", "not a section").is_none());
        assert_eq!(section_range("# A\n## C#\nbody\n## D\n", "C#"), Some((1, 3)));
    }

    #[test]
    fn conflict_content_is_capped_on_a_char_boundary() {
        let (c, truncated) = conflict_content("short");
        assert_eq!((c.as_str(), truncated), ("short", false));
        // 'é' is two bytes; make the cap land in the middle of one.
        let big = "é".repeat(CONFLICT_CONTENT_CAP / 2 + 10);
        let (c, truncated) = conflict_content(&big);
        assert!(truncated);
        assert!(c.len() <= CONFLICT_CONTENT_CAP);
        assert!(big.starts_with(&c));
    }

    #[test]
    fn append_under_keeps_trailing_blank_lines_after_the_insert() {
        let out = append_under(DOC, "Beta", "b2").unwrap();
        assert!(out.contains("b1\nb2\n\n## Alpha\n\ndup\n"), "{out}");
        assert!(append_under(DOC, "Gamma", "x").is_err());
    }

    #[test]
    fn replace_section_keeps_or_replaces_the_heading() {
        let out = replace_section(DOC, "Alpha", "new body\n").unwrap();
        assert!(out.contains("## Alpha\nnew body\n## Beta"), "{out}");
        assert!(!out.contains("Alpha.child"), "nested child is part of the section");
        assert!(out.contains("\n## Alpha\n\ndup\n"), "the duplicate (second) is untouched");
        let out = replace_section(DOC, "Beta", "## Beta renamed\nx\n").unwrap();
        assert!(out.contains("## Beta renamed\nx\n## Alpha"), "{out}");
    }

    #[test]
    fn replace_lines_is_one_based_inclusive_and_bounded() {
        let out = replace_lines("a\nb\nc\n", 2, 3, "B\nC\n").unwrap();
        assert_eq!(out, "a\nB\nC\n");
        assert!(replace_lines("a\nb\n", 0, 1, "x").is_err());
        assert!(replace_lines("a\nb\n", 2, 1, "x").is_err());
        assert!(replace_lines("a\nb\n", 1, 3, "x").is_err());
    }

    #[test]
    fn title_override_reads_a_leading_h1_only() {
        assert_eq!(title_override("\n# Real title\nbody").as_deref(), Some("Real title"));
        assert_eq!(title_override("## not h1\n"), None);
        assert_eq!(title_override("text\n# later\n"), None);
    }

    #[test]
    fn edit_target_accepts_both_shapes() {
        assert_eq!(EditTarget::parse(&json!({"heading": "A"})).unwrap(), EditTarget::Section("A".into()));
        assert_eq!(
            EditTarget::parse(&json!({"type": "section", "section_heading": "A"})).unwrap(),
            EditTarget::Section("A".into())
        );
        assert_eq!(
            EditTarget::parse(&json!({"start_line": 2, "end_line": 3})).unwrap(),
            EditTarget::Lines { start_line: 2, end_line: 3 }
        );
        assert_eq!(
            EditTarget::parse(&json!({"type": "line_range", "offset": 1, "limit": 2})).unwrap(),
            EditTarget::Lines { start_line: 2, end_line: 3 }
        );
        assert!(EditTarget::parse(&json!({})).is_err());
    }

    #[test]
    fn errors_map_to_statuses_and_wire_words() {
        let e = CoordError::RevisionConflict { current_revision: 4, current_content: Some("x".into()), current_content_truncated: false };
        assert_eq!(e.status(), 409);
        assert_eq!(e.to_json()["error"], "revision_conflict");
        assert_eq!(e.to_json()["current_revision"], 4);
        assert_eq!(e.to_json()["current_content_truncated"], false);
        let e = CoordError::RevisionConflict { current_revision: 4, current_content: None, current_content_truncated: false };
        assert!(e.to_json().get("current_content").is_none(), "no content key on a meta conflict");
        assert!(e.to_json().get("current_content_truncated").is_none());
        let e = CoordError::Locked { locked_by: "7".into(), lock_expires_at: 99 };
        assert_eq!(e.to_json()["error"], "locked");
        assert_eq!(e.to_json()["locked_by"], "7");
        assert_eq!(CoordError::NotFound("x".into()).status(), 404);
        assert_eq!(CoordError::Invalid("x".into()).status(), 400);
    }
}
