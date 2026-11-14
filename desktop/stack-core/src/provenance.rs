//! What a component was built from, and how far the world has moved since.
//!
//! Every instance records, per component, a [`Provenance`]: the mode, the
//! branch and commit, the hash of the uncommitted diff if it was built dirty,
//! the registry build it resolved to, the digest actually running. Images
//! are keyed by that content (`<component>:<c8>[-dirty-<diff8>]`), so going
//! back to an earlier ref or bringing up a second slot on the same ref
//! reuses the image with no rebuild.
//!
//! *Staleness* compares the provenance with the live worktree or registry:
//! `behind N` when the branch moved on, `dirty since build` when the worktree
//! has changes the image lacks, `ref changed` when the worktree sits on
//! another branch, `newer prebuilt` when the channel tag resolves to a new
//! digest. Each is a badge with a Rebuild or Pull action.
//!
//! Git facts come through the [`GitFacts`] trait. [`GitCli`] is the real one:
//! it runs `git` argv-only, never through a shell. Tests build temp repos.
//!
//! Prebuilt images carry provenance in their labels. The OCI annotation keys
//! are the only built-in map; a stack adds its own keys per field and they
//! are tried after the OCI ones, in the order written.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::definition::{LabelMapDef, Mode};

/// Short commit prefix used in folder names, image tags and status rows,
/// so all three match at a glance.
pub const SHORT_SHA_LEN: usize = 8;

pub const OCI_REVISION: &str = "org.opencontainers.image.revision";
pub const OCI_REF_NAME: &str = "org.opencontainers.image.ref.name";
pub const OCI_SOURCE: &str = "org.opencontainers.image.source";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Provenance {
    pub mode: Mode,
    /// Branch.
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    /// Full sha; shown short.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Hash of the uncommitted diff, when built dirty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dirty: Option<String>,
    /// prebuilt: the registry tag or build number it resolved to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_id: Option<String>,
    /// Digest actually running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub built_utc: Option<String>,
}

impl Provenance {
    pub fn new(mode: Mode) -> Self {
        Self {
            mode,
            git_ref: None,
            commit: None,
            dirty: None,
            build_id: None,
            image: None,
            built_utc: None,
        }
    }

    pub fn short_commit(&self) -> Option<&str> {
        self.commit.as_deref().map(short)
    }

    /// The content key of a source build, when the commit is known.
    pub fn content_key(&self, component: &str) -> Option<String> {
        self.commit
            .as_deref()
            .map(|c| content_key(component, c, self.dirty.as_deref()))
    }
}

/// First [`SHORT_SHA_LEN`] chars (or the whole string when shorter).
pub fn short(sha: &str) -> &str {
    let end = sha
        .char_indices()
        .nth(SHORT_SHA_LEN)
        .map(|(i, _)| i)
        .unwrap_or(sha.len());
    &sha[..end]
}

/// `<component>:<c8>` or `<component>:<c8>-dirty-<diff8>`: the tag a source
/// build gets, so an identical tree never builds twice.
pub fn content_key(component: &str, commit: &str, dirty: Option<&str>) -> String {
    match dirty.filter(|d| !d.is_empty()) {
        Some(d) => format!("{component}:{}-dirty-{}", short(commit), short(d)),
        None => format!("{component}:{}", short(commit)),
    }
}

// ---------------------------------------------------------------- git facts

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("could not run {program}: {source}")]
    Spawn {
        program: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("git {} failed{}: {stderr}", args.join(" "), status.map(|s| format!(" (exit {s})")).unwrap_or_default())]
    Failed {
        args: Vec<String>,
        status: Option<i32>,
        stderr: String,
    },
    #[error("git {} printed non-UTF-8 output", args.join(" "))]
    NotUtf8 { args: Vec<String> },
    #[error("git {} printed `{output}`, not a commit sha", args.join(" "))]
    NotASha { args: Vec<String>, output: String },
}

/// The handful of git facts staleness needs. One trait so tests can fake
/// them and the real implementation stays small.
pub trait GitFacts {
    /// Full sha of HEAD.
    fn head_commit(&self, repo: &Path) -> Result<String, GitError>;
    /// Current branch, `None` when detached.
    fn current_branch(&self, repo: &Path) -> Result<Option<String>, GitError>;
    /// Commits reachable from `to` but not from `from` (`from..to`).
    fn count_between(&self, repo: &Path, from: &str, to: &str) -> Result<u32, GitError>;
    /// Hash of the uncommitted changes (tracked diff plus untracked file
    /// names), `None` when the worktree is clean.
    fn dirty_hash(&self, repo: &Path) -> Result<Option<String>, GitError>;
    /// Whether `sha` names a commit in this repo.
    fn commit_exists(&self, repo: &Path, sha: &str) -> Result<bool, GitError>;
}

/// `git` on PATH (or a given program), run argv-only with `-C <repo>`.
#[derive(Debug, Clone)]
pub struct GitCli {
    pub program: PathBuf,
}

impl Default for GitCli {
    fn default() -> Self {
        Self {
            program: PathBuf::from("git"),
        }
    }
}

struct Output {
    status: Option<i32>,
    stdout: String,
    stderr: String,
}

impl GitCli {
    fn run(&self, repo: &Path, args: &[&str]) -> Result<Output, GitError> {
        let out = Command::new(&self.program)
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .map_err(|source| GitError::Spawn {
                program: self.program.clone(),
                source,
            })?;
        let args_owned = || args.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let stdout =
            String::from_utf8(out.stdout).map_err(|_| GitError::NotUtf8 { args: args_owned() })?;
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        Ok(Output {
            status: out.status.code(),
            stdout,
            stderr,
        })
    }

    fn run_ok(&self, repo: &Path, args: &[&str]) -> Result<String, GitError> {
        let out = self.run(repo, args)?;
        if out.status == Some(0) {
            Ok(out.stdout)
        } else {
            Err(GitError::Failed {
                args: args.iter().map(|s| s.to_string()).collect(),
                status: out.status,
                stderr: out.stderr.trim().to_string(),
            })
        }
    }
}

fn is_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

impl GitFacts for GitCli {
    fn head_commit(&self, repo: &Path) -> Result<String, GitError> {
        let args = ["rev-parse", "HEAD"];
        let out = self.run_ok(repo, &args)?.trim().to_string();
        if is_sha(&out) {
            Ok(out)
        } else {
            Err(GitError::NotASha {
                args: args.iter().map(|s| s.to_string()).collect(),
                output: out,
            })
        }
    }

    fn current_branch(&self, repo: &Path) -> Result<Option<String>, GitError> {
        let out = self.run_ok(repo, &["rev-parse", "--abbrev-ref", "HEAD"])?;
        let name = out.trim();
        if name.is_empty() || name == "HEAD" {
            Ok(None)
        } else {
            Ok(Some(name.to_string()))
        }
    }

    fn count_between(&self, repo: &Path, from: &str, to: &str) -> Result<u32, GitError> {
        let range = format!("{from}..{to}");
        let args = ["rev-list", "--count", range.as_str()];
        let out = self.run_ok(repo, &args)?;
        out.trim().parse().map_err(|_| GitError::NotASha {
            args: args.iter().map(|s| s.to_string()).collect(),
            output: out.trim().to_string(),
        })
    }

    fn dirty_hash(&self, repo: &Path) -> Result<Option<String>, GitError> {
        let status = self.run_ok(repo, &["status", "--porcelain", "--untracked-files=all"])?;
        if status.trim().is_empty() {
            return Ok(None);
        }
        let diff = self.run_ok(
            repo,
            &["diff", "HEAD", "--no-color", "--no-ext-diff", "--binary"],
        )?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(status.as_bytes());
        hasher.update(b"\0");
        hasher.update(diff.as_bytes());
        Ok(Some(hasher.finalize().to_hex().to_string()))
    }

    fn commit_exists(&self, repo: &Path, sha: &str) -> Result<bool, GitError> {
        let spec = format!("{sha}^{{commit}}");
        let out = self.run(repo, &["rev-parse", "--verify", "--quiet", spec.as_str()])?;
        match out.status {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            status => Err(GitError::Failed {
                args: vec!["rev-parse".into(), "--verify".into(), spec],
                status,
                stderr: out.stderr.trim().to_string(),
            }),
        }
    }
}

// ---------------------------------------------------------------- staleness

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum Staleness {
    /// The branch has `count` commits the image lacks.
    Behind { count: u32 },
    /// The worktree's uncommitted changes differ from what was built.
    DirtySinceBuild,
    /// The worktree is on another branch than the one built.
    RefChanged { built: String, now: String },
    /// The channel tag now resolves to a different digest.
    NewerPrebuilt { built: String, now: String },
}

/// What fixes a stale component.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StaleAction {
    Rebuild,
    Pull,
}

impl Staleness {
    /// Badge text.
    pub fn badge(&self) -> String {
        match self {
            Staleness::Behind { count } => format!("behind {count}"),
            Staleness::DirtySinceBuild => "dirty since build".to_string(),
            Staleness::RefChanged { .. } => "ref changed".to_string(),
            Staleness::NewerPrebuilt { .. } => "newer prebuilt".to_string(),
        }
    }

    pub fn action(&self) -> StaleAction {
        match self {
            Staleness::NewerPrebuilt { .. } => StaleAction::Pull,
            _ => StaleAction::Rebuild,
        }
    }
}

impl fmt::Display for Staleness {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Staleness::RefChanged { built, now } => write!(f, "ref changed ({built} → {now})"),
            Staleness::NewerPrebuilt { built, now } => {
                write!(f, "newer prebuilt ({} → {})", short(built), short(now))
            }
            other => f.write_str(&other.badge()),
        }
    }
}

/// Staleness of a source or dev component against its live worktree.
/// Prebuilt components have no worktree and get an empty list.
pub fn worktree_staleness(
    prov: &Provenance,
    repo: &Path,
    git: &dyn GitFacts,
) -> Result<Vec<Staleness>, GitError> {
    let mut out = Vec::new();
    if !prov.mode.uses_repo() {
        return Ok(out);
    }
    if let Some(commit) = &prov.commit {
        if git.commit_exists(repo, commit)? {
            let count = git.count_between(repo, commit, "HEAD")?;
            if count > 0 {
                out.push(Staleness::Behind { count });
            }
        }
    }
    let now_dirty = git.dirty_hash(repo)?;
    if now_dirty.is_some() && now_dirty != prov.dirty {
        out.push(Staleness::DirtySinceBuild);
    }
    if let (Some(built), Some(now)) = (&prov.git_ref, git.current_branch(repo)?) {
        if *built != now {
            out.push(Staleness::RefChanged {
                built: built.clone(),
                now,
            });
        }
    }
    Ok(out)
}

/// `newer prebuilt` when the channel now resolves to another digest than
/// the one running. `None` for anything but a prebuilt component with a
/// known image.
pub fn prebuilt_staleness(prov: &Provenance, current_digest: &str) -> Option<Staleness> {
    if prov.mode != Mode::Prebuilt {
        return None;
    }
    let built = prov.image.as_deref()?;
    if built == current_digest {
        None
    } else {
        Some(Staleness::NewerPrebuilt {
            built: built.to_string(),
            now: current_digest.to_string(),
        })
    }
}

// --------------------------------------------------------------- label map

/// Which image labels fill which provenance field, each tried in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LabelMap {
    pub commit: Vec<String>,
    pub branch: Vec<String>,
    pub repo: Vec<String>,
    pub mode: Vec<String>,
    pub build_id: Vec<String>,
}

impl Default for LabelMap {
    /// The OCI annotation keys, and nothing else.
    fn default() -> Self {
        Self {
            commit: vec![OCI_REVISION.to_string()],
            branch: vec![OCI_REF_NAME.to_string()],
            repo: vec![OCI_SOURCE.to_string()],
            mode: Vec::new(),
            build_id: Vec::new(),
        }
    }
}

/// What the labels said.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LabelHit {
    pub commit: Option<String>,
    pub branch: Option<String>,
    pub repo: Option<String>,
    pub mode: Option<String>,
    pub build_id: Option<String>,
}

impl LabelHit {
    pub fn is_empty(&self) -> bool {
        self.commit.is_none()
            && self.branch.is_none()
            && self.repo.is_none()
            && self.mode.is_none()
            && self.build_id.is_none()
    }
}

fn append_unique(base: &mut Vec<String>, extra: &[String]) {
    for k in extra {
        if !base.iter().any(|b| b == k) {
            base.push(k.clone());
        }
    }
}

fn first_hit(keys: &[String], labels: &IndexMap<String, String>) -> Option<String> {
    keys.iter()
        .filter_map(|k| labels.get(k))
        .map(|v| v.trim())
        .find(|v| !v.is_empty())
        .map(str::to_string)
}

impl LabelMap {
    /// The defaults followed by a stack's own keys, duplicates dropped.
    pub fn with_overrides(overrides: &LabelMapDef) -> LabelMap {
        let mut map = LabelMap::default();
        append_unique(&mut map.commit, &overrides.commit);
        append_unique(&mut map.branch, &overrides.branch);
        append_unique(&mut map.repo, &overrides.repo);
        append_unique(&mut map.mode, &overrides.mode);
        append_unique(&mut map.build_id, &overrides.build_id);
        map
    }

    /// Read an image's labels: per field, the first key present with a
    /// non-blank value wins.
    pub fn read(&self, labels: &IndexMap<String, String>) -> LabelHit {
        LabelHit {
            commit: first_hit(&self.commit, labels),
            branch: first_hit(&self.branch, labels),
            repo: first_hit(&self.repo, labels),
            mode: first_hit(&self.mode, labels),
            build_id: first_hit(&self.build_id, labels),
        }
    }
}

/// Provenance of a prebuilt image from its labels, tag and digest. The
/// build id is the label's when present, else the tag (the tag is what the
/// user picked, which is often the CI run id itself).
pub fn provenance_from_labels(
    map: &LabelMap,
    labels: &IndexMap<String, String>,
    tag: Option<&str>,
    digest: Option<&str>,
) -> Provenance {
    let hit = map.read(labels);
    let mode = hit
        .mode
        .as_deref()
        .and_then(Mode::parse)
        .unwrap_or(Mode::Prebuilt);
    Provenance {
        mode,
        git_ref: hit.branch,
        commit: hit.commit,
        dirty: None,
        build_id: hit.build_id.or_else(|| tag.map(str::to_string)),
        image: digest.map(str::to_string),
        built_utc: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn labels(pairs: &[(&str, &str)]) -> IndexMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn content_key_shapes() {
        assert_eq!(content_key("api", SHA, None), "api:01234567");
        assert_eq!(content_key("api", SHA, Some("")), "api:01234567");
        assert_eq!(
            content_key("api", SHA, Some("fedcba9876543210")),
            concat!("api:01234567", "-dirty-fedcba98")
        );
        assert_eq!(content_key("api", "abc", None), "api:abc");
        let mut p = Provenance::new(Mode::Source);
        assert_eq!(p.content_key("api"), None);
        p.commit = Some(SHA.into());
        p.dirty = Some("ffffffffffff".into());
        assert_eq!(
            p.content_key("api").as_deref(),
            Some(concat!("api:01234567", "-dirty-ffffffff"))
        );
        assert_eq!(p.short_commit(), Some("01234567"));
    }

    #[test]
    fn provenance_json_uses_ref_and_camel_case() {
        let mut p = Provenance::new(Mode::Prebuilt);
        p.git_ref = Some("main".into());
        p.build_id = Some("4711".into());
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["mode"], "prebuilt");
        assert_eq!(json["ref"], "main");
        assert_eq!(json["buildId"], "4711");
        assert!(json.get("commit").is_none());
        assert_eq!(serde_json::from_value::<Provenance>(json).unwrap(), p);
    }

    #[test]
    fn default_label_map_is_oci_only() {
        let m = LabelMap::default();
        assert_eq!(m.commit, vec![OCI_REVISION]);
        assert_eq!(m.branch, vec![OCI_REF_NAME]);
        assert_eq!(m.repo, vec![OCI_SOURCE]);
        assert!(m.mode.is_empty());
        assert!(m.build_id.is_empty());
    }

    #[test]
    fn overrides_come_after_oci_and_dedupe() {
        let def = LabelMapDef {
            commit: vec![
                "example.vendor.sha".into(),
                OCI_REVISION.into(),
                "example.ci.sourceversion".into(),
            ],
            branch: vec!["example.vendor.branch".into()],
            repo: vec![],
            mode: vec!["example.vendor.mode".into()],
            build_id: vec!["example.ci.buildnumber".into()],
        };
        let m = LabelMap::with_overrides(&def);
        assert_eq!(
            m.commit,
            vec![
                OCI_REVISION,
                "example.vendor.sha",
                "example.ci.sourceversion"
            ]
        );
        assert_eq!(m.branch, vec![OCI_REF_NAME, "example.vendor.branch"]);
        assert_eq!(m.repo, vec![OCI_SOURCE]);
        assert_eq!(m.mode, vec!["example.vendor.mode"]);
        assert_eq!(m.build_id, vec!["example.ci.buildnumber"]);
    }

    #[test]
    fn label_precedence_table() {
        let def = LabelMapDef {
            commit: vec!["example.vendor.sha".into()],
            branch: vec!["example.vendor.branch".into()],
            ..LabelMapDef::default()
        };
        let m = LabelMap::with_overrides(&def);
        // both present: OCI wins
        let hit = m.read(&labels(&[
            (OCI_REVISION, "aaa"),
            ("example.vendor.sha", "bbb"),
        ]));
        assert_eq!(hit.commit.as_deref(), Some("aaa"));
        // only the vendor key: it answers
        let hit = m.read(&labels(&[("example.vendor.sha", "bbb")]));
        assert_eq!(hit.commit.as_deref(), Some("bbb"));
        // blank OCI value falls through to the next key
        let hit = m.read(&labels(&[
            (OCI_REVISION, "  "),
            ("example.vendor.sha", "bbb"),
        ]));
        assert_eq!(hit.commit.as_deref(), Some("bbb"));
        // nothing matching: empty hit
        let hit = m.read(&labels(&[("unrelated", "x")]));
        assert!(hit.is_empty());
        // fields are independent
        let hit = m.read(&labels(&[("example.vendor.branch", "main")]));
        assert_eq!(hit.branch.as_deref(), Some("main"));
        assert_eq!(hit.commit, None);
    }

    #[test]
    fn provenance_from_labels_fills_what_it_can() {
        let m = LabelMap::with_overrides(&LabelMapDef {
            mode: vec!["example.vendor.mode".into()],
            ..Default::default()
        });
        let p = provenance_from_labels(
            &m,
            &labels(&[
                (OCI_REVISION, SHA),
                (OCI_REF_NAME, "release/1.2"),
                ("example.vendor.mode", "source"),
            ]),
            Some("4711"),
            Some("sha256:deadbeef"),
        );
        assert_eq!(p.mode, Mode::Source);
        assert_eq!(p.commit.as_deref(), Some(SHA));
        assert_eq!(p.git_ref.as_deref(), Some("release/1.2"));
        assert_eq!(
            p.build_id.as_deref(),
            Some("4711"),
            "tag stands in for a missing build id label"
        );
        assert_eq!(p.image.as_deref(), Some("sha256:deadbeef"));
        // no labels at all: tag + digest only, mode prebuilt
        let p = provenance_from_labels(&m, &labels(&[]), Some("latest"), Some("sha256:1"));
        assert_eq!(p.mode, Mode::Prebuilt);
        assert_eq!(p.commit, None);
        assert_eq!(p.build_id.as_deref(), Some("latest"));
        // an unknown mode label value is ignored
        let p =
            provenance_from_labels(&m, &labels(&[("example.vendor.mode", "weird")]), None, None);
        assert_eq!(p.mode, Mode::Prebuilt);
        assert_eq!(p.build_id, None);
    }

    #[test]
    fn prebuilt_staleness_compares_digests() {
        let mut p = Provenance::new(Mode::Prebuilt);
        assert_eq!(
            prebuilt_staleness(&p, "sha256:b"),
            None,
            "no image known yet"
        );
        p.image = Some("sha256:a".into());
        assert_eq!(prebuilt_staleness(&p, "sha256:a"), None);
        let s = prebuilt_staleness(&p, "sha256:b").unwrap();
        assert_eq!(
            s,
            Staleness::NewerPrebuilt {
                built: "sha256:a".into(),
                now: "sha256:b".into()
            }
        );
        assert_eq!(s.badge(), "newer prebuilt");
        assert_eq!(s.action(), StaleAction::Pull);
        p.mode = Mode::Source;
        assert_eq!(prebuilt_staleness(&p, "sha256:b"), None);
    }

    #[test]
    fn staleness_badges_actions_and_json() {
        assert_eq!(Staleness::Behind { count: 3 }.badge(), "behind 3");
        assert_eq!(
            Staleness::Behind { count: 3 }.action(),
            StaleAction::Rebuild
        );
        assert_eq!(Staleness::DirtySinceBuild.to_string(), "dirty since build");
        let r = Staleness::RefChanged {
            built: "main".into(),
            now: "feature/x".into(),
        };
        assert_eq!(r.badge(), "ref changed");
        assert_eq!(r.to_string(), "ref changed (main → feature/x)");
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"kind\":\"ref-changed\""), "{json}");
        assert_eq!(
            serde_json::to_string(&Staleness::DirtySinceBuild).unwrap(),
            "{\"kind\":\"dirty-since-build\"}"
        );
    }

    /// A scripted stand-in for git, for the staleness logic itself.
    struct FakeGit {
        head: String,
        branch: Option<String>,
        behind: u32,
        dirty: Option<String>,
        known: Vec<String>,
    }

    impl GitFacts for FakeGit {
        fn head_commit(&self, _: &Path) -> Result<String, GitError> {
            Ok(self.head.clone())
        }
        fn current_branch(&self, _: &Path) -> Result<Option<String>, GitError> {
            Ok(self.branch.clone())
        }
        fn count_between(&self, _: &Path, _: &str, _: &str) -> Result<u32, GitError> {
            Ok(self.behind)
        }
        fn dirty_hash(&self, _: &Path) -> Result<Option<String>, GitError> {
            Ok(self.dirty.clone())
        }
        fn commit_exists(&self, _: &Path, sha: &str) -> Result<bool, GitError> {
            Ok(self.known.iter().any(|k| k == sha))
        }
    }

    fn built(mode: Mode, git_ref: &str, dirty: Option<&str>) -> Provenance {
        let mut p = Provenance::new(mode);
        p.git_ref = Some(git_ref.into());
        p.commit = Some(SHA.into());
        p.dirty = dirty.map(str::to_string);
        p
    }

    #[test]
    fn worktree_staleness_table() {
        let repo = Path::new("unused");
        let clean = |behind, branch: &str, dirty: Option<&str>| FakeGit {
            head: SHA.into(),
            branch: Some(branch.into()),
            behind,
            dirty: dirty.map(str::to_string),
            known: vec![SHA.into()],
        };
        // up to date, clean, same branch
        assert!(worktree_staleness(
            &built(Mode::Source, "main", None),
            repo,
            &clean(0, "main", None)
        )
        .unwrap()
        .is_empty());
        // branch moved on
        assert_eq!(
            worktree_staleness(
                &built(Mode::Source, "main", None),
                repo,
                &clean(2, "main", None)
            )
            .unwrap(),
            vec![Staleness::Behind { count: 2 }]
        );
        // built clean, now dirty
        assert_eq!(
            worktree_staleness(
                &built(Mode::Source, "main", None),
                repo,
                &clean(0, "main", Some("d1"))
            )
            .unwrap(),
            vec![Staleness::DirtySinceBuild]
        );
        // built dirty with the same diff: not stale
        assert!(worktree_staleness(
            &built(Mode::Source, "main", Some("d1")),
            repo,
            &clean(0, "main", Some("d1"))
        )
        .unwrap()
        .is_empty());
        // built dirty, diff changed since
        assert_eq!(
            worktree_staleness(
                &built(Mode::Source, "main", Some("d1")),
                repo,
                &clean(0, "main", Some("d2"))
            )
            .unwrap(),
            vec![Staleness::DirtySinceBuild]
        );
        // built dirty, now clean: the worktree has no changes the image lacks, so this is not
        // "dirty since build"; the surplus baked into the image is outside the badge set
        assert!(worktree_staleness(
            &built(Mode::Source, "main", Some("d1")),
            repo,
            &clean(0, "main", None)
        )
        .unwrap()
        .is_empty());
        // other branch
        assert_eq!(
            worktree_staleness(
                &built(Mode::Dev, "main", None),
                repo,
                &clean(0, "feature/x", None)
            )
            .unwrap(),
            vec![Staleness::RefChanged {
                built: "main".into(),
                now: "feature/x".into()
            }]
        );
        // detached head: no ref comparison
        let mut detached = clean(0, "main", None);
        detached.branch = None;
        assert!(
            worktree_staleness(&built(Mode::Source, "main", None), repo, &detached)
                .unwrap()
                .is_empty()
        );
        // everything at once, in a stable order
        assert_eq!(
            worktree_staleness(
                &built(Mode::Source, "main", None),
                repo,
                &clean(5, "other", Some("d"))
            )
            .unwrap(),
            vec![
                Staleness::Behind { count: 5 },
                Staleness::DirtySinceBuild,
                Staleness::RefChanged {
                    built: "main".into(),
                    now: "other".into()
                },
            ]
        );
        // the built commit is gone from the repo (rewritten history): no behind count, the rest still runs
        let mut gone = clean(9, "main", None);
        gone.known.clear();
        assert!(
            worktree_staleness(&built(Mode::Source, "main", None), repo, &gone)
                .unwrap()
                .is_empty()
        );
        // prebuilt has no worktree
        assert!(worktree_staleness(
            &built(Mode::Prebuilt, "main", None),
            repo,
            &clean(5, "other", Some("d"))
        )
        .unwrap()
        .is_empty());
    }

    // --------------------------------------------------- real git, temp repos

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Run git in `repo` with a configuration that does not depend on the
    /// machine (no global/system config, fixed identity, no signing).
    fn git(repo: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                repo.join("..").join("no-global-config"),
            )
            .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
            .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
            .arg("-C")
            .arg(repo)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.test",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.autocrlf=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn temp_repo() -> Option<(tempfile::TempDir, PathBuf)> {
        if !git_available() {
            eprintln!("git not on PATH; skipping");
            return None;
        }
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        fs::write(repo.join("a.txt"), "one\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "first"]);
        Some((dir, repo))
    }

    fn commit_change(repo: &Path, file: &str, content: &str, msg: &str) {
        fs::write(repo.join(file), content).unwrap();
        git(repo, &["add", "."]);
        git(repo, &["commit", "-q", "-m", msg]);
    }

    #[test]
    fn cli_head_and_branch() {
        let Some((_dir, repo)) = temp_repo() else {
            return;
        };
        let cli = GitCli::default();
        let head = cli.head_commit(&repo).unwrap();
        assert!(is_sha(&head));
        assert_eq!(head, git(&repo, &["rev-parse", "HEAD"]).trim());
        assert_eq!(cli.current_branch(&repo).unwrap().as_deref(), Some("main"));
        git(&repo, &["checkout", "-q", "-b", "feature/x"]);
        assert_eq!(
            cli.current_branch(&repo).unwrap().as_deref(),
            Some("feature/x")
        );
        git(&repo, &["checkout", "-q", "--detach"]);
        assert_eq!(cli.current_branch(&repo).unwrap(), None);
        assert!(cli.commit_exists(&repo, &head).unwrap());
        assert!(!cli.commit_exists(&repo, SHA).unwrap());
    }

    #[test]
    fn cli_count_between_and_dirty_hash() {
        let Some((_dir, repo)) = temp_repo() else {
            return;
        };
        let cli = GitCli::default();
        let first = cli.head_commit(&repo).unwrap();
        assert_eq!(
            cli.dirty_hash(&repo).unwrap(),
            None,
            "fresh commit is clean"
        );
        assert_eq!(cli.count_between(&repo, &first, "HEAD").unwrap(), 0);

        // tracked modification
        fs::write(repo.join("a.txt"), "two\n").unwrap();
        let d1 = cli.dirty_hash(&repo).unwrap().expect("dirty");
        assert_eq!(d1.len(), 64);
        let d1_again = cli.dirty_hash(&repo).unwrap().unwrap();
        assert_eq!(d1, d1_again, "stable for the same change");
        fs::write(repo.join("a.txt"), "three\n").unwrap();
        let d2 = cli.dirty_hash(&repo).unwrap().unwrap();
        assert_ne!(d1, d2, "a different change hashes differently");

        // back to clean
        fs::write(repo.join("a.txt"), "one\n").unwrap();
        assert_eq!(cli.dirty_hash(&repo).unwrap(), None);

        // an untracked file counts as dirty
        fs::write(repo.join("new.txt"), "x").unwrap();
        let d3 = cli.dirty_hash(&repo).unwrap();
        assert!(d3.is_some());
        fs::remove_file(repo.join("new.txt")).unwrap();

        // commits ahead
        commit_change(&repo, "b.txt", "b", "second");
        commit_change(&repo, "c.txt", "c", "third");
        assert_eq!(cli.count_between(&repo, &first, "HEAD").unwrap(), 2);
        let head = cli.head_commit(&repo).unwrap();
        assert_eq!(
            cli.count_between(&repo, &head, &first).unwrap(),
            0,
            "first is behind head, not ahead"
        );
    }

    #[test]
    fn cli_staleness_end_to_end() {
        let Some((_dir, repo)) = temp_repo() else {
            return;
        };
        let cli = GitCli::default();
        let mut prov = Provenance::new(Mode::Source);
        prov.commit = Some(cli.head_commit(&repo).unwrap());
        prov.git_ref = Some("main".into());
        assert!(worktree_staleness(&prov, &repo, &cli).unwrap().is_empty());

        commit_change(&repo, "b.txt", "b", "second");
        assert_eq!(
            worktree_staleness(&prov, &repo, &cli).unwrap(),
            vec![Staleness::Behind { count: 1 }]
        );

        fs::write(repo.join("b.txt"), "changed").unwrap();
        git(&repo, &["checkout", "-q", "-b", "feature/y"]);
        let stale = worktree_staleness(&prov, &repo, &cli).unwrap();
        assert_eq!(
            stale,
            vec![
                Staleness::Behind { count: 1 },
                Staleness::DirtySinceBuild,
                Staleness::RefChanged {
                    built: "main".into(),
                    now: "feature/y".into()
                },
            ]
        );
        // a build at the current dirty state is then up to date on the dirty axis
        prov.commit = Some(cli.head_commit(&repo).unwrap());
        prov.dirty = cli.dirty_hash(&repo).unwrap();
        prov.git_ref = Some("feature/y".into());
        assert!(worktree_staleness(&prov, &repo, &cli).unwrap().is_empty());
        let key = prov.content_key("api").unwrap();
        assert!(key.starts_with("api:") && key.contains("-dirty-"), "{key}");
        assert_eq!(key.len(), "api:".len() + 8 + "-dirty-".len() + 8);
    }

    #[test]
    fn cli_errors_are_typed() {
        let Some((dir, _repo)) = temp_repo() else {
            return;
        };
        let not_repo = dir.path().join("plain");
        fs::create_dir(&not_repo).unwrap();
        let cli = GitCli::default();
        match cli.head_commit(&not_repo) {
            Err(GitError::Failed { args, .. }) => assert_eq!(args, ["rev-parse", "HEAD"]),
            other => panic!("{other:?}"),
        }
        let missing = GitCli {
            program: PathBuf::from("git-that-does-not-exist-anywhere"),
        };
        assert!(matches!(
            missing.head_commit(&not_repo),
            Err(GitError::Spawn { .. })
        ));
    }
}
