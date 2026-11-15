//! Item workspaces: the folder a work item gets in the stack project, and
//! the rules that keep it safe to create from a tracker's untrusted text.
//!
//! ```text
//! .work/
//!   <KEY>/                      one per work item (its key, sanitised for paths)
//!     item.json                 the record: which tracker row it adds to, its repos and branches
//!     src/<repo>-<c8>/          a git worktree of the shared clone <repo> on the item's branch;
//!                               c8 = the first 8 hex of the commit it was created at (a
//!                               creation-time label: the branch moves on, the name does not)
//!   _archive/<KEY>/             finished items: item.json kept, worktrees removed
//!   active.json                 the active item per slot (see [`crate::intent::ActiveRecord`])
//! shared-checkouts-allow.txt    shared clones a stack may build from directly
//! ```
//!
//! Everything here is pure (no git, no engine): the key and slug rules,
//! the branch pattern, how a clone's remote names it in the terms PR rows
//! use (`github` `owner/name`, `azure-devops` `org/project/name`), the
//! Windows path-length check, the allow file, and the item record.
//!
//! Why the key rules are strict: the key comes from a tracker, through the
//! app, as untrusted input, and becomes a folder name and part of a branch
//! name. `[A-Za-z0-9._-]`, at most 40 characters, never `.` or `..`, never a
//! name Windows reserves for a device: whatever a tracker sends, the folder
//! lands inside `.work/` and nowhere else.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::definition::KEY_MAX_LEN;

/// The record file in each item folder.
pub const ITEM_FILE: &str = "item.json";
/// Where finished items go.
pub const ARCHIVE_DIR: &str = "_archive";
/// The worktrees' folder inside an item folder.
pub const SRC_DIR: &str = "src";
/// Shared clones that may be built from directly, one name per line, in
/// the stack project.
pub const ALLOW_FILE: &str = "shared-checkouts-allow.txt";
/// The longest `{slug}`.
pub const SLUG_MAX: usize = 30;
/// The longest branch name accepted.
pub const BRANCH_MAX: usize = 200;
/// The classic Windows path limit, in effect unless long paths are on.
pub const WINDOWS_MAX_PATH: usize = 260;

/// Folder names Windows keeps for devices, whatever the extension.
const RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// The folder name for a tracker key: every run of characters outside
/// `[A-Za-z0-9._-]` becomes one `-`, leading and trailing `.` and `-` go
/// (so `..` and `../x` cannot climb out, and Windows cannot drop a trailing
/// dot), the result is cut to 40 characters, and a device name (`CON`,
/// `nul.txt`) gets a leading `_`. A key with nothing usable left is
/// refused rather than given a made-up name, since two such keys would
/// share one folder.
pub fn item_dir_name(key: &str) -> Result<String, String> {
    let mut out = String::new();
    let mut pending = false;
    for ch in key.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            if pending && !out.is_empty() {
                out.push('-');
            }
            pending = false;
            out.push(ch);
        } else {
            pending = true;
        }
    }
    let trimmed = |s: &str| s.trim_matches(|c| c == '.' || c == '-').to_string();
    let mut name = trimmed(&out);
    let reserved = {
        let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
        RESERVED.contains(&stem.as_str())
    };
    if reserved {
        name.insert(0, '_');
    }
    name.truncate(KEY_MAX_LEN);
    let name = trimmed(&name);
    if name.is_empty() {
        return Err(format!(
            "the key `{}` has nothing usable in a folder name ([A-Za-z0-9._-])",
            key.chars().take(80).collect::<String>()
        ));
    }
    Ok(name)
}

/// `{slug}` from a title: lowercase, every run outside `[a-z0-9]` one `-`,
/// no leading or trailing `-`, at most 30 characters.
pub fn slug(title: &str) -> String {
    let mut out = String::new();
    let mut pending = false;
    for ch in title.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            if pending && !out.is_empty() {
                out.push('-');
            }
            pending = false;
            out.push(ch);
        } else {
            pending = true;
        }
    }
    out.truncate(SLUG_MAX);
    out.trim_end_matches('-').to_string()
}

/// Whether a branch pattern only uses `{key}` and `{slug}`; `Ok(true)` when
/// it has `{key}` (without it every item gets the same branch).
pub fn check_branch_pattern(pattern: &str) -> Result<bool, String> {
    let mut has_key = false;
    let mut rest = pattern;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        let close = after
            .find('}')
            .ok_or_else(|| format!("`{pattern}` has a `{{` without its `}}`"))?;
        match &after[..close] {
            "key" => has_key = true,
            "slug" => {}
            other => {
                return Err(format!(
                    "`{{{other}}}` is not a branch placeholder; use {{key}} and {{slug}}"
                ))
            }
        }
        rest = &after[close + 1..];
    }
    if rest.contains('}') {
        return Err(format!("`{pattern}` has a `}}` without its `{{`"));
    }
    let sample = fill_branch(pattern, "ABC-1", "fix-login")?;
    if !is_valid_branch(&sample) {
        return Err(format!(
            "`{pattern}` does not make a usable branch name (`{sample}`)"
        ));
    }
    Ok(has_key)
}

/// The pattern with `{key}` (the item's folder name, already sanitised) and
/// `{slug}` filled in. An empty slug leaves no dangling separator:
/// `task/{key}-{slug}` with no title is `task/ABC-1`.
pub fn fill_branch(pattern: &str, key: &str, slug: &str) -> Result<String, String> {
    let filled = pattern.replace("{key}", key).replace("{slug}", slug);
    let mut out = String::new();
    for ch in filled.chars() {
        if ch == '-' && out.ends_with('-') {
            continue;
        }
        out.push(ch);
    }
    let out = out
        .trim_end_matches(['-', '_', '/', '.'])
        .trim_start_matches(['-', '/'])
        .replace("/-", "/")
        .replace("-/", "/");
    if is_valid_branch(&out) {
        Ok(out)
    } else {
        Err(format!("`{out}` is not a usable branch name"))
    }
}

/// The parts of git's branch-name rules that matter for names made from a
/// pattern: printable ASCII without space or `~^:?*[\`, no `..`, `@{`,
/// `//`, no part starting with `.` or ending in `.lock`, no leading or
/// trailing `/`, not ending in `.`, not `@`, at most 200 characters.
pub fn is_valid_branch(name: &str) -> bool {
    if name.is_empty() || name.len() > BRANCH_MAX || name == "@" {
        return false;
    }
    if name
        .bytes()
        .any(|b| b <= b' ' || b == 0x7f || b"~^:?*[\\".contains(&b) || !b.is_ascii())
    {
        return false;
    }
    if name.contains("..") || name.contains("@{") || name.contains("//") {
        return false;
    }
    if name.starts_with('/') || name.ends_with('/') || name.ends_with('.') || name.starts_with('-')
    {
        return false;
    }
    name.split('/')
        .all(|part| !part.starts_with('.') && !part.ends_with(".lock"))
}

/// The first 8 hex of a commit: the label a worktree folder carries.
pub fn c8(commit: &str) -> String {
    commit.chars().take(8).collect()
}

/// A repo in the terms PR rows use.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RepoIdentity {
    /// `github` or `azure-devops`.
    pub forge: String,
    /// `owner/name` (github) or `org/project/name` (azure-devops).
    pub repo: String,
}

impl RepoIdentity {
    fn new(forge: &str, repo: String) -> Self {
        Self {
            forge: forge.to_string(),
            repo,
        }
    }

    /// Whether a PR's `{forge, repo}` is this repo (case-insensitive: both
    /// forges treat names that way).
    pub fn matches(&self, forge: &str, repo: &str) -> bool {
        self.forge.eq_ignore_ascii_case(forge) && self.repo.eq_ignore_ascii_case(repo)
    }
}

/// `(host, path)` of a remote URL in any of git's forms: `scheme://[user@]
/// host[:port]/path` or the scp-like `[user@]host:path`. The host comes
/// back lowercase without user or port; the path without leading or
/// trailing `/` and without `.git`. A local path is `None`.
pub fn split_remote(url: &str) -> Option<(String, String)> {
    let url = url.trim();
    let (authority, path) = if let Some((scheme, rest)) = url.split_once("://") {
        if !matches!(
            scheme.to_ascii_lowercase().as_str(),
            "https" | "http" | "ssh" | "git" | "git+ssh" | "ssh+git"
        ) {
            return None;
        }
        match rest.split_once('/') {
            Some((a, p)) => (a, p),
            None => (rest, ""),
        }
    } else {
        // scp-like: a colon before any slash, and more than a drive letter.
        let colon = url.find(':')?;
        if url[..colon].contains('/') || url[..colon].contains('\\') {
            return None;
        }
        let host_part = &url[..colon];
        let host_only = host_part.rsplit('@').next().unwrap_or(host_part);
        if host_only.len() <= 1 {
            return None;
        }
        (host_part, &url[colon + 1..])
    };
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = host.split(':').next().unwrap_or(host).to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    let mut path = path.trim_matches('/').to_string();
    if let Some(p) = path.strip_suffix(".git") {
        path = p.to_string();
    }
    let path = path.trim_end_matches('/').to_string();
    if path.is_empty() {
        return None;
    }
    Some((host, percent_decode(&path)))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && bytes[i + 1].is_ascii_hexdigit()
            && bytes[i + 2].is_ascii_hexdigit()
        {
            let hex = |b: u8| (b as char).to_digit(16).unwrap_or(0) as u8;
            out.push(hex(bytes[i + 1]) * 16 + hex(bytes[i + 2]));
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

/// Name a remote the way PR rows do.
///
/// - `dev.azure.com/{org}/{project}/_git/{name}`, `{org}.visualstudio.com/
///   [DefaultCollection/]{project}/_git/{name}` and the ssh forms
///   `ssh.dev.azure.com:v3/{org}/{project}/{name}` /
///   `vs-ssh.visualstudio.com:v3/…` are `azure-devops` `org/project/name`;
/// - `github.com/{owner}/{name}`, and the same two-part path on any host in
///   `github_hosts` (an enterprise server the definition names in a
///   `work.repos` remote), is `github` `owner/name`;
/// - anything else is `None`: such a repo is left out of `branches[]`.
pub fn classify_remote(url: &str, github_hosts: &[String]) -> Option<RepoIdentity> {
    let (host, path) = split_remote(url)?;
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    let ado = |org: &str, project: &str, name: &str| {
        Some(RepoIdentity::new(
            "azure-devops",
            format!("{org}/{project}/{name}"),
        ))
    };
    if host == "dev.azure.com" {
        return match parts.as_slice() {
            [org, project, "_git", name] => ado(org, project, name),
            _ => None,
        };
    }
    if host == "ssh.dev.azure.com" || host == "vs-ssh.visualstudio.com" {
        return match parts.as_slice() {
            ["v3", org, project, name] => ado(org, project, name),
            _ => None,
        };
    }
    if let Some(org) = host.strip_suffix(".visualstudio.com") {
        return match parts.as_slice() {
            [project, "_git", name] => ado(org, project, name),
            [_collection, project, "_git", name] => ado(org, project, name),
            _ => None,
        };
    }
    let github = host == "github.com" || github_hosts.iter().any(|h| h.eq_ignore_ascii_case(&host));
    if github {
        if let [owner, name] = parts.as_slice() {
            return Some(RepoIdentity::new("github", format!("{owner}/{name}")));
        }
    }
    None
}

/// The hosts of every `work.repos` remote override: a server the
/// definition points a clone at is taken as a GitHub-style forge when its
/// path is `owner/name`.
pub fn override_hosts(remotes: impl IntoIterator<Item = impl AsRef<str>>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for r in remotes {
        if let Some((host, _)) = split_remote(r.as_ref()) {
            if !out.contains(&host) {
                out.push(host);
            }
        }
    }
    out
}

/// Whether git on this machine accepts paths past 260 characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LongPaths {
    On,
    Off,
    /// Could not be read; treated as off.
    Unknown,
}

/// Refuse a worktree path git would fail on halfway: on Windows, longer
/// than 260 characters while long paths are not known to be on.
pub fn check_path_length(path: &Path, windows: bool, long_paths: LongPaths) -> Result<(), String> {
    let len = path.as_os_str().to_string_lossy().chars().count();
    if !windows || len <= WINDOWS_MAX_PATH || long_paths == LongPaths::On {
        return Ok(());
    }
    let state = match long_paths {
        LongPaths::Off => "long paths are off (git's core.longpaths)",
        _ => "long paths could not be confirmed on, so they are assumed off",
    };
    Err(format!(
        "the worktree path would be {len} characters, past Windows' {WINDOWS_MAX_PATH}, and {state}; \
         nothing was created. Move the stack's work folder or root closer to the drive, or turn on \
         long paths (`git config --global core.longpaths true`)"
    ))
}

/// The allow file's clone names: one per line, `#` starts a comment.
pub fn parse_allow_file(text: &str) -> BTreeSet<String> {
    text.lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|l| !l.is_empty())
        .map(|l| l.trim_matches(['/', '\\']).to_string())
        .map(|l| crate::definition::repo_name(Path::new(&l)).unwrap_or(l))
        .collect()
}

/// The allow file of a stack project, or nothing when it does not exist.
pub fn load_allow_file(project_dir: &Path) -> BTreeSet<String> {
    fs::read_to_string(project_dir.join(ALLOW_FILE))
        .map(|t| parse_allow_file(&t))
        .unwrap_or_default()
}

// ------------------------------------------------------------- the record

/// Which source's row an item adds to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemRef {
    pub source: String,
    pub key: String,
}

/// The owner's status as last sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemStatus {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
}

/// One repo of an item: its worktree and branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemRepo {
    pub branch: String,
    /// The worktree, relative to the item folder (`src/api-1a2b3c4d`).
    pub path: PathBuf,
    /// The commit the worktree was created at (its folder's label).
    pub c8: String,
    /// The branch it is compared against for ahead / behind.
    pub base: String,
    pub created_utc: String,
}

/// `item.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemRecord {
    #[serde(rename = "ref")]
    pub item_ref: ItemRef,
    /// The folder name (the key, sanitised).
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ItemStatus>,
    #[serde(default)]
    pub repos: IndexMap<String, ItemRepo>,
    /// Components whose drift was dismissed, with the worktree HEAD it was
    /// dismissed at: the banner comes back once HEAD moves again.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub keep_as_built: IndexMap<String, String>,
    pub created_utc: String,
    pub updated_utc: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_utc: Option<String>,
    /// Where the item stands in the stack's local workflow; none until
    /// the stack has a `workflow:` block and the item was started or moved
    /// under it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<crate::workflow::WorkflowRecord>,
    /// The state seeds the item asks for, per component: replayed after
    /// the general seed when a recipe component is rebuilt, and recorded
    /// in every saved state.
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub data_seeds: IndexMap<String, Vec<crate::states::SeedChoice>>,
}

impl ItemRecord {
    pub fn new(item_ref: ItemRef, key: &str, now_utc: &str) -> Self {
        Self {
            item_ref,
            key: key.to_string(),
            title: None,
            url: None,
            status: None,
            repos: IndexMap::new(),
            keep_as_built: IndexMap::new(),
            created_utc: now_utc.to_string(),
            updated_utc: now_utc.to_string(),
            archived_utc: None,
            workflow: None,
            data_seeds: IndexMap::new(),
        }
    }

    /// `<work>/<key>`.
    pub fn dir(work_dir: &Path, key: &str) -> PathBuf {
        work_dir.join(key)
    }

    /// `<work>/_archive/<key>`.
    pub fn archived_dir(work_dir: &Path, key: &str) -> PathBuf {
        work_dir.join(ARCHIVE_DIR).join(key)
    }

    /// `Ok(None)` when the folder has no record.
    pub fn load(item_dir: &Path) -> Result<Option<Self>, String> {
        let path = item_dir.join(ITEM_FILE);
        match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .map(Some)
                .map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    pub fn save(&self, item_dir: &Path) -> Result<(), String> {
        crate::jobs::write_atomic(
            &item_dir.join(ITEM_FILE),
            &serde_json::to_string_pretty(self).expect("item record serializes"),
        )
        .map_err(|e| e.to_string())
    }

    /// The worktree of `repo`, absolute.
    pub fn worktree(&self, item_dir: &Path, repo: &str) -> Option<PathBuf> {
        self.repos.get(repo).map(|r| item_dir.join(&r.path))
    }

    /// Whether the owner's last status said the item is finished.
    pub fn done_in_tracker(&self) -> bool {
        self.status
            .as_ref()
            .and_then(|s| s.category.as_deref())
            .is_some_and(|c| c == "done")
    }
}

/// Every item folder under the work folder with its record, in name order
/// (folders without a record, `_archive`, `jobs` and `instances` are not
/// items).
pub fn list_items(work_dir: &Path) -> Vec<(PathBuf, ItemRecord)> {
    list_in(work_dir)
}

/// Every archived item.
pub fn list_archived(work_dir: &Path) -> Vec<(PathBuf, ItemRecord)> {
    list_in(&work_dir.join(ARCHIVE_DIR))
}

fn list_in(dir: &Path) -> Vec<(PathBuf, ItemRecord)> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<(PathBuf, ItemRecord)> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter(|e| e.file_name() != ARCHIVE_DIR)
        .filter_map(|e| {
            let p = e.path();
            ItemRecord::load(&p).ok().flatten().map(|r| (p, r))
        })
        .collect();
    out.sort_by(|a, b| a.1.key.cmp(&b.1.key));
    out
}

/// The item that adds to `(source, key)`; with no `source`, the one item
/// whose ref key is `key`, or whose folder is `key`.
pub fn find_item(
    work_dir: &Path,
    source: Option<&str>,
    key: &str,
) -> Option<(PathBuf, ItemRecord)> {
    let items = list_items(work_dir);
    if let Some(s) = source {
        if let Some(hit) = items
            .iter()
            .find(|(_, r)| r.item_ref.source == s && r.item_ref.key == key)
        {
            return Some(hit.clone());
        }
    }
    let by_key: Vec<&(PathBuf, ItemRecord)> = items
        .iter()
        .filter(|(_, r)| r.item_ref.key == key)
        .collect();
    if by_key.len() == 1 {
        return Some(by_key[0].clone());
    }
    items.into_iter().find(|(_, r)| r.key == key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_become_folder_names_that_stay_inside_the_work_folder() {
        let ok = [
            ("ABC-1", "ABC-1"),
            ("owner/repo#12", "owner-repo-12"),
            ("../x", "x"),
            ("..\\..\\etc", "etc"),
            ("a/../../b", "a-..-..-b"),
            ("CON", "_CON"),
            ("con.txt", "_con.txt"),
            ("Lpt9", "_Lpt9"),
            ("CONSOLE-1", "CONSOLE-1"),
            ("ÄBC-1", "BC-1"),
            ("日本-7", "7"),
            ("trailing.", "trailing"),
            (" spaced key ", "spaced-key"),
        ];
        for (key, want) in ok {
            assert_eq!(item_dir_name(key).as_deref(), Ok(want), "key {key:?}");
        }
        let long = "A".repeat(60);
        assert_eq!(item_dir_name(&long).unwrap().len(), KEY_MAX_LEN);
        let cut = format!("{}-.-x", "B".repeat(39));
        assert_eq!(item_dir_name(&cut).unwrap(), "B".repeat(39));
        for bad in ["", "..", ".", "/", "日本", "---", "%%%"] {
            assert!(item_dir_name(bad).is_err(), "key {bad:?} must be refused");
        }
        for key in ["../x", "..", "CON", "a/b"] {
            if let Ok(name) = item_dir_name(key) {
                assert!(!name.contains('/') && !name.contains('\\') && name != ".." && name != ".");
            }
        }
    }

    #[test]
    fn slugs_and_branch_patterns() {
        assert_eq!(slug("Fix login"), "fix-login");
        assert_eq!(slug("  Fix   the LOGIN page!! "), "fix-the-login-page");
        assert_eq!(slug("Ünïcode — titles"), "n-code-titles");
        assert_eq!(slug(""), "");
        assert_eq!(slug("a".repeat(40).as_str()).len(), SLUG_MAX);
        assert_eq!(
            slug("abcdefghijklmnopqrstuvwxyz abcd efgh"),
            "abcdefghijklmnopqrstuvwxyz-abc"
        );
        assert_eq!(
            slug("abcdefghijklmnopqrstuvwxyz-abc-d"),
            "abcdefghijklmnopqrstuvwxyz-abc"
        );
        assert_eq!(
            slug("abcdefghijklmnopqrstuvwxyzabc- d"),
            "abcdefghijklmnopqrstuvwxyzabc"
        );

        let p = "task/{key}-{slug}";
        assert_eq!(
            fill_branch(p, "ABC-1", "fix-login").unwrap(),
            "task/ABC-1-fix-login"
        );
        assert_eq!(fill_branch(p, "ABC-1", "").unwrap(), "task/ABC-1");
        assert_eq!(fill_branch("{slug}/{key}", "ABC-1", "").unwrap(), "ABC-1");
        assert_eq!(
            fill_branch("feature/{key}", "ABC-1", "x").unwrap(),
            "feature/ABC-1"
        );
        assert!(fill_branch("task/{key}", ".hidden", "").is_err());

        assert_eq!(check_branch_pattern(p), Ok(true));
        assert_eq!(check_branch_pattern("wip/{slug}"), Ok(false));
        assert!(check_branch_pattern("task/{item}").is_err());
        assert!(check_branch_pattern("task/{key").is_err());
        assert!(check_branch_pattern("task/key}").is_err());
        assert!(check_branch_pattern("task {key}").is_err());
        assert!(check_branch_pattern("task/../{key}").is_err());
    }

    #[test]
    fn branch_names_follow_gits_rules() {
        for ok in ["main", "task/ABC-1-fix", "release/1.2", "a_b"] {
            assert!(is_valid_branch(ok), "{ok}");
        }
        for bad in [
            "", "@", "a b", "a..b", "a@{1}", "/a", "a/", "a.", "a//b", "a/.b", "a.lock",
            "a/b.lock", "a:b", "a~1", "a^", "a?", "a*", "a[", "a\\b", "-a", "é",
        ] {
            assert!(!is_valid_branch(bad), "{bad:?}");
        }
        assert!(is_valid_branch(&"a".repeat(BRANCH_MAX)));
        assert!(!is_valid_branch(&"a".repeat(BRANCH_MAX + 1)));
    }

    #[test]
    fn remotes_are_named_in_the_terms_pr_rows_use() {
        let ghe = vec!["git.example.com".to_string()];
        let gh = |r: &str| Some(RepoIdentity::new("github", r.to_string()));
        let ado = |r: &str| Some(RepoIdentity::new("azure-devops", r.to_string()));
        let table: Vec<(&str, Option<RepoIdentity>)> = vec![
            ("https://github.com/owner/repo.git", gh("owner/repo")),
            ("https://github.com/owner/repo", gh("owner/repo")),
            ("https://github.com/owner/repo/", gh("owner/repo")),
            (
                "https://user:secret@github.com/owner/repo.git",
                gh("owner/repo"),
            ),
            ("git@github.com:owner/repo.git", gh("owner/repo")),
            ("ssh://git@github.com/owner/repo.git", gh("owner/repo")),
            ("ssh://git@github.com:22/owner/repo", gh("owner/repo")),
            ("HTTPS://GitHub.com/Owner/Repo.git", gh("Owner/Repo")),
            ("https://git.example.com/team/api.git", gh("team/api")),
            ("git@git.example.com:team/api.git", gh("team/api")),
            ("https://example.com/team/api.git", None),
            ("https://github.com/owner", None),
            ("https://github.com/owner/repo/extra", None),
            (
                "https://dev.azure.com/org/project/_git/api",
                ado("org/project/api"),
            ),
            (
                "https://org@dev.azure.com/org/project/_git/api",
                ado("org/project/api"),
            ),
            (
                "https://dev.azure.com/org/My%20Project/_git/api",
                ado("org/My Project/api"),
            ),
            ("https://dev.azure.com/org/project/api", None),
            (
                "git@ssh.dev.azure.com:v3/org/project/api",
                ado("org/project/api"),
            ),
            (
                "ssh://git@ssh.dev.azure.com/v3/org/project/api",
                ado("org/project/api"),
            ),
            (
                "https://org.visualstudio.com/project/_git/api",
                ado("org/project/api"),
            ),
            (
                "https://org.visualstudio.com/DefaultCollection/project/_git/api",
                ado("org/project/api"),
            ),
            (
                "org@vs-ssh.visualstudio.com:v3/org/project/api",
                ado("org/project/api"),
            ),
            ("/srv/example/api.git", None),
            ("C:\\code\\x", None),
            ("C:/code/x", None),
            ("file:///srv/example/api.git", None),
            ("../api", None),
            ("", None),
        ];
        for (url, want) in table {
            assert_eq!(classify_remote(url, &ghe), want, "{url}");
        }
        assert_eq!(
            override_hosts([
                "https://git.example.com/a/b",
                "git@git.example.com:c/d",
                "bad"
            ]),
            vec!["git.example.com".to_string()]
        );
        assert!(RepoIdentity::new("github", "Owner/Repo".into()).matches("GitHub", "owner/repo"));
    }

    #[test]
    fn long_windows_paths_are_refused_unless_long_paths_are_on() {
        let at = |n: usize| PathBuf::from("C".repeat(n));
        assert!(check_path_length(&at(260), true, LongPaths::Off).is_ok());
        let e = check_path_length(&at(261), true, LongPaths::Off).unwrap_err();
        assert!(
            e.contains("261 characters") && e.contains("long paths are off"),
            "{e}"
        );
        let e = check_path_length(&at(261), true, LongPaths::Unknown).unwrap_err();
        assert!(e.contains("assumed off"), "{e}");
        assert!(check_path_length(&at(400), true, LongPaths::On).is_ok());
        assert!(check_path_length(&at(400), false, LongPaths::Off).is_ok());
    }

    #[test]
    fn the_allow_file_lists_clone_names() {
        let names =
            parse_allow_file("# shared clones\nweb\n  api  # built for demos\n\n../tools/\n");
        assert_eq!(
            names.into_iter().collect::<Vec<_>>(),
            vec!["api".to_string(), "tools".to_string(), "web".to_string()]
        );
    }

    #[test]
    fn records_round_trip_and_are_found_by_their_ref() {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path();
        let mut rec = ItemRecord::new(
            ItemRef {
                source: "tracker".into(),
                key: "ABC 1".into(),
            },
            "ABC-1",
            "2026-11-14T12:00:00Z",
        );
        rec.status = Some(ItemStatus {
            label: "Done".into(),
            category: Some("done".into()),
        });
        let dir = ItemRecord::dir(work, "ABC-1");
        rec.save(&dir).unwrap();
        assert_eq!(ItemRecord::load(&dir).unwrap().as_ref(), Some(&rec));
        assert!(rec.done_in_tracker());
        fs::create_dir_all(work.join(ARCHIVE_DIR).join("OLD-1")).unwrap();
        fs::create_dir_all(work.join("jobs")).unwrap();
        assert_eq!(list_items(work).len(), 1);
        assert_eq!(
            find_item(work, Some("tracker"), "ABC 1").unwrap().1.key,
            "ABC-1"
        );
        assert_eq!(find_item(work, None, "ABC 1").unwrap().1.key, "ABC-1");
        assert_eq!(find_item(work, None, "ABC-1").unwrap().1.key, "ABC-1");
        assert!(find_item(work, Some("other"), "NOPE").is_none());
        let json: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join(ITEM_FILE)).unwrap()).unwrap();
        assert_eq!(json["ref"]["source"], "tracker");
    }
}
