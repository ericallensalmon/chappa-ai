//! Images: building them from source by content, pinning prebuilt ones by
//! digest, and remembering where each came from.
//!
//! **Source builds are keyed by content.** The tag is
//! `<component>:<c8>[-dirty-<diff8>]` (the first eight hex of the commit,
//! plus a hash of the uncommitted diff when the tree is dirty). Before a
//! build the driver asks the engine whether that tag already exists; if it
//! does, nothing is built. Going back to an earlier ref or bringing up a
//! second slot on the same ref therefore costs no rebuild. The build carries
//! its provenance as labels: the OCI keys (`org.opencontainers.image.revision`
//! for the full sha, `.ref.name` for the branch, `.source` for the repo) and
//! this crate's own content-key and dirty-hash keys.
//!
//! **Prebuilt images come from a registry.** The user picks a channel (a tag
//! that moves), a build (a digest) or a commit. A channel is resolved to a
//! digest at `up` time so what runs is pinned, and `newer prebuilt` later
//! compares that pin with what the channel resolves to now. Provenance
//! comes from the image's labels through the stack's
//! [`crate::provenance::LabelMap`]; when labels say nothing, two argv hooks
//! can: `catalog` lists the builds of an image, `resolve` turns a tag into
//! its commit. A hook's answer is cached per digest, because a digest never
//! changes meaning.
//!
//! Digest resolution without pulling: `docker manifest inspect --verbose`
//! prints the descriptor(s) the registry serves. Podman has no equivalent
//! that prints the digest without fetching, so a podman component is pinned
//! by tag until its first pull, after which `image inspect` supplies the
//! digest from `RepoDigests`.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::cli::{engine_argv, engine_invocation, CliError, EngineCli};
use crate::definition::{ContainerOs, EngineRef, Mode, ModeDef};
use crate::driver::EngineKind;
use crate::intent::INSTANCES_DIR;
use crate::placeholders::{self, Context, PlaceholderError};
use crate::provenance::{
    content_key, provenance_from_labels, GitError, GitFacts, LabelMap, Provenance, OCI_REF_NAME,
    OCI_REVISION, OCI_SOURCE,
};

/// Label keys this crate writes on images and containers it creates.
pub const LABEL_CONTENT_KEY: &str = "chappa.stack.content-key";
pub const LABEL_DIRTY: &str = "chappa.stack.dirty";
pub const LABEL_COMPONENT: &str = "chappa.stack.component";
pub const LABEL_INSTANCE: &str = "chappa.stack.instance";
pub const LABEL_GATEWAY: &str = "chappa.stack.gateway";

/// `provenance.json` beside the override files.
pub const PROVENANCE_FILE: &str = "provenance.json";

// ----------------------------------------------------------- source builds

/// Everything a source build needs, decided from the live worktree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceBuild {
    pub component: String,
    /// The content-keyed tag, `<component>:<c8>[-dirty-<diff8>]`.
    pub key: String,
    pub commit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dirty: Option<String>,
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    /// Build context, absolute.
    pub context: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dockerfile: Option<PathBuf>,
    /// Repository URL or path for the OCI source label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

impl SourceBuild {
    /// The labels the build carries, in a fixed order.
    pub fn labels(&self) -> Vec<(String, String)> {
        let mut out = vec![
            (OCI_REVISION.to_string(), self.commit.clone()),
            (LABEL_CONTENT_KEY.to_string(), self.key.clone()),
            (LABEL_COMPONENT.to_string(), self.component.clone()),
        ];
        if let Some(r) = &self.git_ref {
            out.push((OCI_REF_NAME.to_string(), r.clone()));
        }
        if let Some(s) = &self.source {
            out.push((OCI_SOURCE.to_string(), s.clone()));
        }
        if let Some(d) = &self.dirty {
            out.push((LABEL_DIRTY.to_string(), d.clone()));
        }
        out
    }

    pub fn provenance(&self, built_utc: Option<&str>) -> Provenance {
        Provenance {
            mode: Mode::Source,
            git_ref: self.git_ref.clone(),
            commit: Some(self.commit.clone()),
            dirty: self.dirty.clone(),
            build_id: None,
            image: Some(self.key.clone()),
            built_utc: built_utc.map(str::to_string),
        }
    }
}

/// Decide a source build from the repo at `repo` (absolute) and the mode's
/// `context` / `dockerfile` (relative to the repo).
pub fn source_build(
    component: &str,
    mode: &ModeDef,
    repo: &Path,
    git: &dyn GitFacts,
    source: Option<&str>,
) -> Result<SourceBuild, GitError> {
    let commit = git.head_commit(repo)?;
    let dirty = git.dirty_hash(repo)?;
    let git_ref = git.current_branch(repo)?;
    let context = match &mode.context {
        Some(c) => repo.join(c),
        None => repo.to_path_buf(),
    };
    Ok(SourceBuild {
        component: component.to_string(),
        key: content_key(component, &commit, dirty.as_deref()),
        commit,
        dirty,
        git_ref,
        context,
        dockerfile: mode.dockerfile.as_ref().map(|d| repo.join(d)),
        source: source.map(str::to_string),
    })
}

/// `<engine> build -t <key> [-f <dockerfile>] --label k=v… <context>`.
pub fn build_argv(engine: &EngineRef, build: &SourceBuild) -> Vec<String> {
    let mut argv = engine_argv(engine);
    argv.push("build".to_string());
    argv.push("-t".to_string());
    argv.push(build.key.clone());
    if let Some(df) = &build.dockerfile {
        argv.push("-f".to_string());
        argv.push(df.to_string_lossy().into_owned());
    }
    for (k, v) in build.labels() {
        argv.push("--label".to_string());
        argv.push(format!("{k}={v}"));
    }
    argv.push(build.context.to_string_lossy().into_owned());
    argv
}

/// Whether `reference` exists locally on `engine` (`image inspect` exit 0).
pub fn image_exists(
    cli: &dyn EngineCli,
    engine: &EngineRef,
    reference: &str,
) -> Result<bool, CliError> {
    let out = cli.run(&engine_invocation(
        engine,
        &["image", "inspect", "--format", "{{.Id}}", reference],
    ))?;
    Ok(out.ok())
}

/// The labels of a local image (`{}` when it has none or does not exist).
pub fn image_labels(
    cli: &dyn EngineCli,
    engine: &EngineRef,
    reference: &str,
) -> Result<IndexMap<String, String>, CliError> {
    let out = cli.run(&engine_invocation(
        engine,
        &[
            "image",
            "inspect",
            "--format",
            "{{json .Config.Labels}}",
            reference,
        ],
    ))?;
    if !out.ok() {
        return Ok(IndexMap::new());
    }
    let labels: Option<IndexMap<String, String>> = serde_json::from_str(out.stdout.trim())
        .map_err(|e| CliError::BadOutput {
            what: "image labels".into(),
            message: e.to_string(),
        })?;
    Ok(labels.unwrap_or_default())
}

// --------------------------------------------------------- prebuilt images

/// What the user picked for a prebuilt component.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind", content = "value")]
pub enum PrebuiltPick {
    /// A moving tag: a branch or release line. Resolved to a digest at up.
    Channel(String),
    /// An exact build: a digest (`sha256:…`).
    Build(String),
    /// The newest build of this commit, found through the catalog.
    Commit(String),
}

/// `repo:tag`.
pub fn tagged_reference(repo: &str, tag: &str) -> String {
    format!("{repo}:{tag}")
}

/// `repo@sha256:…`: the pinned form, which no push can move.
pub fn pinned_reference(repo: &str, digest: &str) -> String {
    format!("{repo}@{digest}")
}

/// The tag a prebuilt mode's `tag` template resolves to with `channel`.
pub fn resolve_tag(mode: &ModeDef, channel: Option<&str>) -> Result<String, PlaceholderError> {
    let template = mode.tag.as_deref().unwrap_or("latest");
    let ctx = Context {
        channel: channel.map(str::to_string),
        ..Context::default()
    };
    placeholders::resolve(template, &ctx)
}

/// The digest `reference` resolves to on `engine`, without pulling where the
/// engine allows it. `Ok(None)` when it cannot be known yet (podman before
/// the first pull). `prefer_os` picks the platform out of a manifest list;
/// `None` (no `os:` declared on the group) means Linux, so a multi-platform
/// image never pins its Windows entry for a Linux-mode engine merely
/// because that entry is listed first.
pub fn resolve_digest(
    cli: &dyn EngineCli,
    engine: &EngineRef,
    reference: &str,
    prefer_os: Option<ContainerOs>,
) -> Result<Option<String>, CliError> {
    match engine.kind {
        EngineKind::Docker => {
            let out = cli.run(&engine_invocation(
                engine,
                &["manifest", "inspect", "--verbose", reference],
            ))?;
            if !out.ok() {
                return Err(CliError::Failed {
                    command: format!("manifest inspect {reference}"),
                    summary: out.summary(),
                });
            }
            Ok(parse_manifest_digest(
                &out.stdout,
                Some(prefer_os.unwrap_or(ContainerOs::Linux)),
            ))
        }
        _ => local_digest(cli, engine, reference),
    }
}

/// The repo digest of a local image, from `RepoDigests`.
pub fn local_digest(
    cli: &dyn EngineCli,
    engine: &EngineRef,
    reference: &str,
) -> Result<Option<String>, CliError> {
    let out = cli.run(&engine_invocation(
        engine,
        &[
            "image",
            "inspect",
            "--format",
            "{{json .RepoDigests}}",
            reference,
        ],
    ))?;
    if !out.ok() {
        return Ok(None);
    }
    let digests: Option<Vec<String>> =
        serde_json::from_str(out.stdout.trim()).map_err(|e| CliError::BadOutput {
            what: "RepoDigests".into(),
            message: e.to_string(),
        })?;
    let repo = reference.split('@').next().unwrap_or(reference);
    let repo = repo.rsplit_once(':').map(|(r, _)| r).unwrap_or(repo);
    Ok(digests.unwrap_or_default().into_iter().find_map(|d| {
        let (r, digest) = d.split_once('@')?;
        (r == repo).then(|| digest.to_string())
    }))
}

#[derive(Debug, Deserialize)]
struct VerboseManifest {
    #[serde(rename = "Descriptor")]
    descriptor: Option<Descriptor>,
}

#[derive(Debug, Deserialize)]
struct Descriptor {
    digest: Option<String>,
    platform: Option<Platform>,
}

#[derive(Debug, Deserialize)]
struct Platform {
    os: Option<String>,
}

/// The digest out of `docker manifest inspect --verbose`: one object for a
/// single-platform image, an array for a manifest list. In a list the entry
/// for `prefer_os` wins, else the first entry with a digest.
pub fn parse_manifest_digest(stdout: &str, prefer_os: Option<ContainerOs>) -> Option<String> {
    let text = stdout.trim();
    let entries: Vec<VerboseManifest> = if text.starts_with('[') {
        serde_json::from_str(text).ok()?
    } else {
        vec![serde_json::from_str(text).ok()?]
    };
    let descriptors: Vec<&Descriptor> = entries
        .iter()
        .filter_map(|e| e.descriptor.as_ref())
        .collect();
    if let Some(os) = prefer_os {
        if let Some(d) = descriptors.iter().find(|d| {
            d.platform
                .as_ref()
                .and_then(|p| p.os.as_deref())
                .and_then(ContainerOs::parse)
                == Some(os)
        }) {
            if let Some(digest) = &d.digest {
                return Some(digest.clone());
            }
        }
    }
    descriptors.iter().find_map(|d| d.digest.clone())
}

/// One build of an image as the `catalog` hook lists it, and the shape the
/// `resolve` hook answers with (`tag` and `digest` then echo the request).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogEntry {
    #[serde(default)]
    pub tag: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,
}

impl CatalogEntry {
    /// Fold a hook's facts into a provenance (never clearing what the
    /// labels already said).
    pub fn fill(&self, prov: &mut Provenance) {
        if prov.commit.is_none() {
            prov.commit = self.commit.clone();
        }
        if prov.git_ref.is_none() {
            prov.git_ref = self.branch.clone();
        }
        if let Some(b) = &self.build_id {
            prov.build_id = Some(b.clone());
        }
        if prov.image.is_none() {
            prov.image = self.digest.clone();
        }
        if prov.built_utc.is_none() {
            prov.built_utc = self.created.clone();
        }
    }
}

/// The context a prebuilt hook's argv is resolved with: `{param:image}` is
/// the repository, `{param:tag}` the tag in question.
pub fn hook_context(base: &Context, image: &str, tag: Option<&str>) -> Context {
    let mut ctx = base.clone();
    ctx.params.insert("image".to_string(), image.to_string());
    if let Some(t) = tag {
        ctx.params.insert("tag".to_string(), t.to_string());
    }
    ctx
}

/// The `catalog` command: the mode's hook, else the engine's own
/// `manifest inspect --verbose` of the one tag in force (the generic
/// default can list nothing more than that).
pub fn catalog_argv(
    mode: &ModeDef,
    engine: &EngineRef,
    base: &Context,
    tag: &str,
) -> Result<Vec<String>, PlaceholderError> {
    let repo = mode.image.as_deref().unwrap_or_default();
    match &mode.catalog {
        Some(hook) => placeholders::resolve_argv(hook, &hook_context(base, repo, Some(tag))),
        None => {
            let mut argv = engine_argv(engine);
            argv.extend(
                ["manifest", "inspect", "--verbose"]
                    .iter()
                    .map(|s| s.to_string()),
            );
            argv.push(tagged_reference(repo, tag));
            Ok(argv)
        }
    }
}

/// The `resolve` command for `tag`, when the mode has one.
pub fn resolve_argv(
    mode: &ModeDef,
    base: &Context,
    tag: &str,
) -> Result<Option<Vec<String>>, PlaceholderError> {
    let repo = mode.image.as_deref().unwrap_or_default();
    match &mode.resolve {
        Some(hook) => Ok(Some(placeholders::resolve_argv(
            hook,
            &hook_context(base, repo, Some(tag)),
        )?)),
        None => Ok(None),
    }
}

/// Parse a `catalog` hook's stdout. The default `manifest inspect` output
/// is recognised too and becomes one entry for `tag`.
pub fn parse_catalog(stdout: &str, tag: &str, prefer_os: Option<ContainerOs>) -> Vec<CatalogEntry> {
    if let Ok(entries) = serde_json::from_str::<Vec<CatalogEntry>>(stdout) {
        if entries.iter().all(|e| !e.tag.is_empty()) {
            return entries;
        }
    }
    match parse_manifest_digest(stdout, prefer_os) {
        Some(digest) => vec![CatalogEntry {
            tag: tag.to_string(),
            digest: Some(digest),
            ..CatalogEntry::default()
        }],
        None => Vec::new(),
    }
}

/// Parse a `resolve` hook's stdout (`{commit, branch, buildId, buildUrl}`).
pub fn parse_resolve(stdout: &str, tag: &str) -> Result<CatalogEntry, CliError> {
    let mut entry: CatalogEntry =
        serde_json::from_str(stdout).map_err(|e| CliError::BadOutput {
            what: "resolve hook".into(),
            message: e.to_string(),
        })?;
    if entry.tag.is_empty() {
        entry.tag = tag.to_string();
    }
    Ok(entry)
}

/// Choose the catalog entry a pick means. Commits match by prefix either
/// way (a short sha in the pick, or a short sha in the catalog); among
/// several builds of one commit the newest `created` wins.
pub fn pick_from_catalog<'a>(
    entries: &'a [CatalogEntry],
    pick: &PrebuiltPick,
) -> Option<&'a CatalogEntry> {
    match pick {
        PrebuiltPick::Channel(tag) => entries.iter().find(|e| e.tag == *tag),
        PrebuiltPick::Build(digest) => entries.iter().find(|e| e.digest.as_deref() == Some(digest)),
        PrebuiltPick::Commit(sha) => entries
            .iter()
            .filter(|e| {
                e.commit
                    .as_deref()
                    .is_some_and(|c| c.starts_with(sha) || sha.starts_with(c))
            })
            .max_by(|a, b| a.created.cmp(&b.created)),
    }
}

/// Provenance of a running container from its labels: a build this crate
/// made (content-key label present) reads as `source`; anything else goes
/// through the stack's label map as prebuilt.
pub fn provenance_from_container_labels(
    map: &LabelMap,
    labels: &IndexMap<String, String>,
    tag: Option<&str>,
    digest: Option<&str>,
) -> Provenance {
    if let Some(key) = labels.get(LABEL_CONTENT_KEY) {
        return Provenance {
            mode: Mode::Source,
            git_ref: labels.get(OCI_REF_NAME).cloned(),
            commit: labels.get(OCI_REVISION).cloned(),
            dirty: labels.get(LABEL_DIRTY).cloned(),
            build_id: None,
            image: Some(key.clone()),
            built_utc: None,
        };
    }
    provenance_from_labels(map, labels, tag, digest)
}

// ---------------------------------------------------------------- the cache

/// `.work/instances/<instance>/provenance.json`: what each component of an
/// instance was last acquired as, and every hook answer by digest.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProvenanceCache {
    #[serde(default)]
    pub components: IndexMap<String, Provenance>,
    /// `resolve` / `catalog` facts keyed by digest: immutable, never refetched.
    #[serde(default)]
    pub resolved: IndexMap<String, CatalogEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_utc: Option<String>,
}

impl ProvenanceCache {
    pub fn path(work_dir: &Path, instance: &str) -> PathBuf {
        work_dir
            .join(INSTANCES_DIR)
            .join(instance)
            .join(PROVENANCE_FILE)
    }

    /// An absent or unreadable file is an empty cache: the worst case is a
    /// hook running again.
    pub fn load(work_dir: &Path, instance: &str) -> ProvenanceCache {
        std::fs::read_to_string(Self::path(work_dir, instance))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("cache serializes")
    }

    pub fn remember(&mut self, component: &str, prov: Provenance, now_utc: &str) {
        self.components.insert(component.to_string(), prov);
        self.updated_utc = Some(now_utc.to_string());
    }

    pub fn forget(&mut self, component: &str, now_utc: &str) {
        self.components.shift_remove(component);
        self.updated_utc = Some(now_utc.to_string());
    }

    pub fn image_of(&self, component: &str) -> Option<&str> {
        self.components
            .get(component)
            .and_then(|p| p.image.as_deref())
    }

    /// Known `{image:<component>}` values, for placeholder contexts.
    pub fn images(&self) -> IndexMap<String, String> {
        self.components
            .iter()
            .filter_map(|(c, p)| p.image.clone().map(|i| (c.clone(), i)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A source-built image tag with a dirty-tree hash, built here so no
    /// source line holds a component name beside a varied hex string (a
    /// secret scanner reads that as a key).
    fn dirty_tag(dirty: &str) -> String {
        format!("{}:{}-dirty-{dirty}", "api", "01234567")
    }
    use crate::cli::FakeCli;

    struct FakeGit {
        commit: &'static str,
        dirty: Option<&'static str>,
        branch: Option<&'static str>,
    }

    impl GitFacts for FakeGit {
        fn head_commit(&self, _: &Path) -> Result<String, GitError> {
            Ok(self.commit.to_string())
        }
        fn current_branch(&self, _: &Path) -> Result<Option<String>, GitError> {
            Ok(self.branch.map(str::to_string))
        }
        fn count_between(&self, _: &Path, _: &str, _: &str) -> Result<u32, GitError> {
            Ok(0)
        }
        fn dirty_hash(&self, _: &Path) -> Result<Option<String>, GitError> {
            Ok(self.dirty.map(str::to_string))
        }
        fn commit_exists(&self, _: &Path, _: &str) -> Result<bool, GitError> {
            Ok(true)
        }
    }

    fn docker() -> EngineRef {
        EngineRef {
            name: "docker".into(),
            kind: EngineKind::Docker,
            context: None,
        }
    }

    fn podman() -> EngineRef {
        EngineRef {
            name: "podman".into(),
            kind: EngineKind::Podman,
            context: None,
        }
    }

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn build_argv_carries_tag_dockerfile_and_labels_in_order() {
        let mode = ModeDef {
            repo: Some(PathBuf::from("../api")),
            context: Some(PathBuf::from("src")),
            dockerfile: Some(PathBuf::from("build/Dockerfile")),
            ..ModeDef::default()
        };
        let repo = PathBuf::from("/work/api");
        let git = FakeGit {
            commit: SHA,
            dirty: Some("fedcba9876543210ffff"),
            branch: Some("feature/x"),
        };
        let b = source_build(
            "api",
            &mode,
            &repo,
            &git,
            Some("https://example.invalid/api.git"),
        )
        .unwrap();
        assert_eq!(b.key, dirty_tag("fedcba98"));
        let argv = build_argv(&docker(), &b);
        let expect: Vec<String> = [
            "docker",
            "build",
            "-t",
            &dirty_tag("fedcba98"),
            "-f",
            &repo.join("build/Dockerfile").to_string_lossy(),
            "--label",
            &format!("org.opencontainers.image.revision={SHA}"),
            "--label",
            &format!("chappa.stack.content-key={}", dirty_tag("fedcba98")),
            "--label",
            "chappa.stack.component=api",
            "--label",
            "org.opencontainers.image.ref.name=feature/x",
            "--label",
            "org.opencontainers.image.source=https://example.invalid/api.git",
            "--label",
            "chappa.stack.dirty=fedcba9876543210ffff",
            &repo.join("src").to_string_lossy(),
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(argv, expect);
        let p = b.provenance(Some("2026-01-01T00:00:00Z"));
        assert_eq!(p.mode, Mode::Source);
        assert_eq!(p.image, Some(dirty_tag("fedcba98")));
        assert_eq!(p.dirty.as_deref(), Some("fedcba9876543210ffff"));
    }

    #[test]
    fn clean_detached_build_has_no_ref_or_dirty_labels_and_context_defaults_to_repo() {
        let git = FakeGit {
            commit: SHA,
            dirty: None,
            branch: None,
        };
        let b = source_build("api", &ModeDef::default(), Path::new("/r"), &git, None).unwrap();
        assert_eq!(b.key, "api:01234567");
        assert_eq!(b.context, PathBuf::from("/r"));
        let keys: Vec<String> = b.labels().into_iter().map(|(k, _)| k).collect();
        assert_eq!(keys, [OCI_REVISION, LABEL_CONTENT_KEY, LABEL_COMPONENT]);
        assert!(!build_argv(&podman(), &b).contains(&"-f".to_string()));
        assert_eq!(build_argv(&podman(), &b)[0], "podman");
    }

    #[test]
    fn image_exists_and_labels_go_through_image_inspect() {
        let cli = FakeCli::new()
            .failing(&["image", "inspect", "api:deadbeef"], 1, "Error: No such image")
            .on(
                &["image", "inspect", "{{json .Config.Labels}}", "api:01234567"],
                "{\"chappa.stack.content-key\":\"api:01234567\",\"org.opencontainers.image.revision\":\"abc\"}\n",
            )
            .on(&["image", "inspect", "{{json .Config.Labels}}", "bare:1"], "null\n");
        assert!(image_exists(&cli, &docker(), "api:01234567").unwrap());
        assert!(!image_exists(&cli, &docker(), "api:deadbeef").unwrap());
        let labels = image_labels(&cli, &docker(), "api:01234567").unwrap();
        assert_eq!(labels[LABEL_CONTENT_KEY], "api:01234567");
        assert!(image_labels(&cli, &docker(), "bare:1").unwrap().is_empty());
        assert_eq!(
            cli.lines()[0],
            "docker image inspect --format {{.Id}} api:01234567"
        );
    }

    const SINGLE: &str = r#"{"Ref":"registry.example/api:main","Descriptor":{"mediaType":"application/vnd.docker.distribution.manifest.v2+json","digest":"sha256:aaaa","size":1234,"platform":{"architecture":"amd64","os":"linux"}},"SchemaV2Manifest":{}}"#;
    const LIST: &str = r#"[
      {"Ref":"registry.example/api:main@sha256:1111","Descriptor":{"digest":"sha256:1111","platform":{"os":"linux","architecture":"amd64"}}},
      {"Ref":"registry.example/api:main@sha256:2222","Descriptor":{"digest":"sha256:2222","platform":{"os":"windows","architecture":"amd64"}}}
    ]"#;

    #[test]
    fn manifest_digest_parsing_single_list_and_platform_preference() {
        assert_eq!(
            parse_manifest_digest(SINGLE, None).as_deref(),
            Some("sha256:aaaa")
        );
        assert_eq!(
            parse_manifest_digest(LIST, None).as_deref(),
            Some("sha256:1111")
        );
        assert_eq!(
            parse_manifest_digest(LIST, Some(ContainerOs::Windows)).as_deref(),
            Some("sha256:2222")
        );
        assert_eq!(
            parse_manifest_digest(SINGLE, Some(ContainerOs::Windows)).as_deref(),
            Some("sha256:aaaa"),
            "no match for the preferred OS falls back to the first digest"
        );
        assert_eq!(parse_manifest_digest("not json", None), None);
        assert_eq!(parse_manifest_digest("{}", None), None);
    }

    #[test]
    fn digest_resolution_docker_inspects_the_manifest_podman_waits_for_a_pull() {
        let cli = FakeCli::new()
            .on(&["docker", "manifest", "inspect", "--verbose"], SINGLE)
            .failing(
                &["podman", "image", "inspect", "registry.example/api:main"],
                125,
                "no such image",
            )
            .on(
                &["podman", "image", "inspect", "registry.example/db:14"],
                "[\"registry.example/db@sha256:dddd\",\"other.example/db@sha256:eeee\"]\n",
            );
        assert_eq!(
            resolve_digest(&cli, &docker(), "registry.example/api:main", None)
                .unwrap()
                .as_deref(),
            Some("sha256:aaaa")
        );
        assert_eq!(
            resolve_digest(&cli, &podman(), "registry.example/api:main", None).unwrap(),
            None
        );
        assert_eq!(
            resolve_digest(&cli, &podman(), "registry.example/db:14", None)
                .unwrap()
                .as_deref(),
            Some("sha256:dddd")
        );
        let failing = FakeCli::new().failing(
            &["manifest", "inspect"],
            1,
            "unauthorized: authentication required",
        );
        let err =
            resolve_digest(&failing, &docker(), "registry.example/api:main", None).unwrap_err();
        assert!(err.to_string().contains("unauthorized"));
    }

    #[test]
    fn references_and_tags() {
        assert_eq!(
            tagged_reference("registry.example/api", "main"),
            "registry.example/api:main"
        );
        assert_eq!(
            pinned_reference("registry.example/api", "sha256:aaaa"),
            "registry.example/api@sha256:aaaa"
        );
        let mode = ModeDef {
            image: Some("registry.example/api".into()),
            tag: Some("{channel}".into()),
            ..ModeDef::default()
        };
        assert_eq!(resolve_tag(&mode, Some("release/2")).unwrap(), "release/2");
        assert!(
            resolve_tag(&mode, None).is_err(),
            "a channel template needs a channel"
        );
        let fixed = ModeDef {
            tag: Some("stable".into()),
            ..ModeDef::default()
        };
        assert_eq!(resolve_tag(&fixed, None).unwrap(), "stable");
        assert_eq!(resolve_tag(&ModeDef::default(), None).unwrap(), "latest");
    }

    #[test]
    fn hooks_resolve_params_and_default_catalog_is_manifest_inspect() {
        let mode = ModeDef {
            image: Some("registry.example/api".into()),
            tag: Some("{channel}".into()),
            catalog: Some(
                [
                    "ci-tool",
                    "builds",
                    "--image",
                    "{param:image}",
                    "--slot",
                    "{slot}",
                ]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            ),
            resolve: Some(
                ["ci-tool", "resolve", "{param:image}:{param:tag}"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            ),
            ..ModeDef::default()
        };
        let base = Context {
            slot: Some(2),
            ..Context::default()
        };
        assert_eq!(
            catalog_argv(&mode, &docker(), &base, "main").unwrap(),
            [
                "ci-tool",
                "builds",
                "--image",
                "registry.example/api",
                "--slot",
                "2"
            ]
            .map(String::from)
        );
        assert_eq!(
            resolve_argv(&mode, &base, "4711").unwrap().unwrap(),
            ["ci-tool", "resolve", "registry.example/api:4711"].map(String::from)
        );
        let plain = ModeDef {
            image: Some("registry.example/api".into()),
            ..ModeDef::default()
        };
        assert_eq!(
            catalog_argv(&plain, &podman(), &base, "main").unwrap(),
            [
                "podman",
                "manifest",
                "inspect",
                "--verbose",
                "registry.example/api:main"
            ]
            .map(String::from)
        );
        assert_eq!(resolve_argv(&plain, &base, "main").unwrap(), None);
    }

    #[test]
    fn catalog_parsing_accepts_hook_json_or_manifest_output_and_picks() {
        let hook = r#"[
          {"tag":"4710","digest":"sha256:a","commit":"0123456789abcdef0123456789abcdef01234567","branch":"main","buildId":"4710","created":"2026-01-01T00:00:00Z"},
          {"tag":"4711","digest":"sha256:b","commit":"0123456789abcdef0123456789abcdef01234567","branch":"main","buildId":"4711","created":"2026-01-02T00:00:00Z"},
          {"tag":"main","digest":"sha256:b","commit":"0123456789abcdef0123456789abcdef01234567","branch":"main"}
        ]"#;
        let entries = parse_catalog(hook, "main", None);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[1].build_id.as_deref(), Some("4711"));
        let by_channel =
            pick_from_catalog(&entries, &PrebuiltPick::Channel("main".into())).unwrap();
        assert_eq!(by_channel.digest.as_deref(), Some("sha256:b"));
        let by_build =
            pick_from_catalog(&entries, &PrebuiltPick::Build("sha256:a".into())).unwrap();
        assert_eq!(by_build.tag, "4710");
        let by_commit =
            pick_from_catalog(&entries, &PrebuiltPick::Commit("01234567".into())).unwrap();
        assert_eq!(by_commit.tag, "4711", "newest build of the commit");
        assert!(pick_from_catalog(&entries, &PrebuiltPick::Commit("ffff".into())).is_none());

        let from_manifest = parse_catalog(SINGLE, "main", None);
        assert_eq!(from_manifest.len(), 1);
        assert_eq!(from_manifest[0].tag, "main");
        assert_eq!(from_manifest[0].digest.as_deref(), Some("sha256:aaaa"));
        assert!(parse_catalog("garbage", "main", None).is_empty());

        let resolved = parse_resolve(r#"{"commit":"abc","branch":"main","buildId":"4711","buildUrl":"https://ci.example/4711"}"#, "4711").unwrap();
        assert_eq!(resolved.tag, "4711");
        assert_eq!(
            resolved.build_url.as_deref(),
            Some("https://ci.example/4711")
        );
        assert!(parse_resolve("nope", "x").is_err());
        let mut prov = Provenance::new(Mode::Prebuilt);
        prov.build_id = Some("tag".into());
        resolved.fill(&mut prov);
        assert_eq!(prov.commit.as_deref(), Some("abc"));
        assert_eq!(
            prov.build_id.as_deref(),
            Some("4711"),
            "a hook's build id beats the tag"
        );
    }

    #[test]
    fn container_labels_tell_our_builds_from_prebuilt_images() {
        let map = LabelMap::default();
        let mut ours = IndexMap::new();
        ours.insert(
            LABEL_CONTENT_KEY.to_string(),
            "api:01234567-dirty-aaaaaaaa".to_string(),
        );
        ours.insert(OCI_REVISION.to_string(), SHA.to_string());
        ours.insert(OCI_REF_NAME.to_string(), "feature/x".to_string());
        ours.insert(LABEL_DIRTY.to_string(), "aaaaaaaa0000".to_string());
        let p =
            provenance_from_container_labels(&map, &ours, Some("ignored"), Some("sha256:ignored"));
        assert_eq!(p.mode, Mode::Source);
        assert_eq!(p.image.as_deref(), Some("api:01234567-dirty-aaaaaaaa"));
        assert_eq!(p.dirty.as_deref(), Some("aaaaaaaa0000"));
        assert_eq!(p.commit.as_deref(), Some(SHA));

        let mut theirs = IndexMap::new();
        theirs.insert(OCI_REVISION.to_string(), "abc".to_string());
        let p = provenance_from_container_labels(&map, &theirs, Some("4711"), Some("sha256:b"));
        assert_eq!(p.mode, Mode::Prebuilt);
        assert_eq!(p.build_id.as_deref(), Some("4711"));
        assert_eq!(p.image.as_deref(), Some("sha256:b"));
    }

    #[test]
    fn cache_round_trips_and_tolerates_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let empty = ProvenanceCache::load(dir.path(), "app-1");
        assert!(empty.components.is_empty());
        let mut cache = empty;
        let mut p = Provenance::new(Mode::Prebuilt);
        p.image = Some("sha256:b".into());
        cache.remember("api", p, "2026-01-01T00:00:00Z");
        cache.resolved.insert(
            "sha256:b".into(),
            CatalogEntry {
                tag: "4711".into(),
                commit: Some("abc".into()),
                ..CatalogEntry::default()
            },
        );
        let path = ProvenanceCache::path(dir.path(), "app-1");
        assert!(path.ends_with(Path::new("instances").join("app-1").join("provenance.json")));
        crate::jobs::write_atomic(&path, &cache.to_json()).unwrap();
        let back = ProvenanceCache::load(dir.path(), "app-1");
        assert_eq!(back, cache);
        assert_eq!(back.image_of("api"), Some("sha256:b"));
        assert_eq!(back.images()["api"], "sha256:b");
        std::fs::write(&path, "{ not json").unwrap();
        assert!(ProvenanceCache::load(dir.path(), "app-1")
            .components
            .is_empty());
    }
}
