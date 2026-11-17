//! Which gitleaks config a scan uses, and the rules a project's own config
//! must pass.
//!
//! A project may name a gitleaks config of its own in `chappa.yml`:
//!
//! ```yaml
//! secretScan:
//!   config: tools/gitleaks.toml   # relative to the project folder
//! ```
//!
//! That file is in the tree it helps scan, so anyone who can edit the tree
//! can edit what the scan looks for. It is therefore treated like a
//! command: its content hash is part of the project's trust, the trust
//! prompt shows it (allowlisted `paths` called out as files the scan skips
//! entirely), and a scan uses it only while it is exactly the trusted
//! content. Every scan re-reads the file and checks its hash, then hands
//! gitleaks the bytes it checked (written into the scan's scratch folder),
//! never the path, so a file changed between the check and the scan is
//! never read.
//!
//! **Precedence** ([`choose`]): the project's config while trusted and
//! valid, else the config Settings ▸ Security names, else the built-in
//! default ([`super::gitleaks::DEFAULT_CONFIG`]). Both fallbacks keep
//! gitleaks' default rules, and a project config is layered on top of them (it
//! adds allowlists and rules, or turns rules off, all shown in the prompt), so falling back can only report more, never fewer,
//! findings. While a named project config is not used, [`Chosen::review`]
//! says why, and the gates show it as a banner asking the person to review
//! it.
//!
//! **Layering** ([`layered`]): a project config adds to the rules a scan
//! would use without it; it never replaces them. Its checked text is
//! written into the scan's scratch folder with its `[extend]` rewritten to
//! `path = <base>` (its `disabledRules` kept), where the base is:
//!
//! - with no Settings config: the app's own config
//!   ([`super::gitleaks::DEFAULT_CONFIG`], written beside it), which itself
//!   extends gitleaks' defaults. So: project → app → gitleaks defaults, and
//!   the app's extra rules (credentials written inline) still fire;
//! - with a Settings config: that file. It is the person's replacement for
//!   the app's config (it does not carry the app's extra rules when used on
//!   its own either), so: project → Settings → whatever it extends. gitleaks
//!   follows at most two levels of `[extend] path` and silently drops the
//!   rest (measured with 8.30: a third level's defaults were not loaded), so
//!   a Settings config that itself extends a file cannot be stacked on: the
//!   project config is then not used, the Settings config is, and
//!   [`Chosen::review`] says why.
//!
//! **Rules** ([`check`]), applied when the project is loaded and before
//! trust is offered, and again before every scan: the file must keep
//! gitleaks' defaults (`[extend] useDefault = true`), and `[extend] path`
//! is refused, because it would pull in a second file the trust hash does
//! not cover. The file is parsed as TOML; a file that fails a rule is shown
//! as an error and never used.

use std::fs;
use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

/// The largest project config read.
pub const MAX_CONFIG_BYTES: u64 = 256 * 1024;

/// sha256 of the exact bytes, lowercase hex: what the trust covers and what
/// the stack runner is handed to check against.
pub fn content_hash(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// What a valid config says that the trust prompt calls out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigFacts {
    /// Allowlisted `paths` patterns: files these match are skipped
    /// entirely (by every rule for a top-level allowlist, by one rule for a
    /// rule's own), as `(rule id or None, pattern)`.
    pub skipped_paths: Vec<(Option<String>, String)>,
    /// `[extend] disabledRules`: rules of the configs below it (gitleaks'
    /// defaults, the app's own) this file switches off.
    pub disabled_rules: Vec<String>,
}

impl ConfigFacts {
    /// The prompt's lines: the rules it turns off, then one line per
    /// skipped-paths pattern.
    pub fn prompt_lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.disabled_rules.is_empty() {
            out.push(format!(
                "turns off these rules: {}",
                self.disabled_rules.join(", ")
            ));
        }
        out.extend(self.skipped_paths.iter().map(|(rule, p)| match rule {
            None => format!("skips these files entirely: {p}"),
            Some(r) => format!("skips these files entirely for rule `{r}`: {p}"),
        }));
        out
    }
}

/// Check a config's text against the rules (see the module docs).
pub fn check(text: &str) -> Result<ConfigFacts, String> {
    let table: toml::Table = text
        .parse()
        .map_err(|e: toml::de::Error| format!("it is not valid TOML: {}", e.message()))?;
    let extend = match table.get("extend") {
        Some(toml::Value::Table(t)) => t,
        Some(_) => return Err("`extend` must be a table (`[extend]`)".into()),
        None => {
            return Err(
                "it must keep gitleaks' default rules: add `[extend]` with `useDefault = true`"
                    .into(),
            )
        }
    };
    if extend.contains_key("path") {
        return Err("`[extend] path` is refused: it would pull in another file, which the project's trust does not cover".into());
    }
    if let Some(key) = extend
        .keys()
        .find(|k| !matches!(k.as_str(), "useDefault" | "disabledRules"))
    {
        return Err(format!(
            "`[extend] {key}` is not allowed (only `useDefault` and `disabledRules`)"
        ));
    }
    if extend.get("useDefault") != Some(&toml::Value::Boolean(true)) {
        return Err("it must keep gitleaks' default rules: `[extend] useDefault = true`".into());
    }
    let mut facts = ConfigFacts::default();
    match extend.get("disabledRules") {
        None => {}
        Some(toml::Value::Array(ids)) => {
            for id in ids {
                match id.as_str() {
                    Some(s) => facts.disabled_rules.push(s.to_owned()),
                    None => return Err("`[extend] disabledRules` must list rule ids".into()),
                }
            }
        }
        Some(_) => return Err("`[extend] disabledRules` must list rule ids".into()),
    }
    collect_paths(&table, None, &mut facts);
    if let Some(toml::Value::Array(rules)) = table.get("rules") {
        for rule in rules {
            if let toml::Value::Table(r) = rule {
                let id = r
                    .get("id")
                    .and_then(toml::Value::as_str)
                    .unwrap_or("?")
                    .to_string();
                collect_paths(r, Some(&id), &mut facts);
            }
        }
    }
    Ok(facts)
}

/// The `paths` of `allowlist` and of each `allowlists` entry of `t`.
fn collect_paths(t: &toml::Table, rule: Option<&str>, facts: &mut ConfigFacts) {
    let mut lists: Vec<&toml::Table> = Vec::new();
    if let Some(toml::Value::Table(a)) = t.get("allowlist") {
        lists.push(a);
    }
    if let Some(toml::Value::Array(all)) = t.get("allowlists") {
        lists.extend(all.iter().filter_map(toml::Value::as_table));
    }
    for list in lists {
        if let Some(toml::Value::Array(paths)) = list.get("paths") {
            for p in paths {
                let shown = p
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| p.to_string());
                facts.skipped_paths.push((rule.map(str::to_owned), shown));
            }
        }
    }
}

/// Why `rel` (as written in `chappa.yml`) is not a usable path to the
/// project's config: it must be relative to the project folder and stay
/// inside it. Both separators count on every OS, as `chappa.yml` is shared
/// between machines.
pub fn path_problem(rel: &str) -> Option<String> {
    let rel = rel.trim();
    if rel.is_empty() {
        return Some("the path is empty".into());
    }
    let unified = rel.replace('\\', "/");
    let drive = unified.len() >= 2 && unified.as_bytes()[1] == b':';
    if unified.starts_with('/') || drive || Path::new(rel).is_absolute() {
        return Some(format!(
            "`{rel}` is absolute; name the file relative to the project folder"
        ));
    }
    if unified.split('/').any(|part| part == "..") {
        return Some(format!(
            "`{rel}` leaves the project folder (`..` is not allowed)"
        ));
    }
    None
}

/// `rel` resolved inside `root`: checked by [`path_problem`], then, where the
/// file exists, its real location must still be inside the project (a link
/// out of it is refused).
pub fn resolve_in_project(root: &Path, rel: &str) -> Result<PathBuf, String> {
    if let Some(p) = path_problem(rel) {
        return Err(p);
    }
    let mut path = root.to_path_buf();
    for part in rel.trim().split(['/', '\\']) {
        if !part.is_empty() && part != "." {
            path.push(part);
        }
    }
    if let (Ok(real), Ok(real_root)) = (fs::canonicalize(&path), fs::canonicalize(root)) {
        if !real.starts_with(&real_root) {
            return Err(format!("`{}` leads out of the project folder", rel.trim()));
        }
    }
    Ok(path)
}

/// Whether `path` lies inside `folder` (both made absolute; resolved where
/// they exist, so a link or a `..` cannot hide it).
pub fn is_inside(path: &Path, folder: &Path) -> bool {
    let (p, f) = (resolved(path), resolved(folder));
    let lower = |p: &Path| {
        if cfg!(windows) {
            PathBuf::from(p.to_string_lossy().to_lowercase())
        } else {
            p.to_path_buf()
        }
    };
    lower(&p).starts_with(lower(&f))
}

/// `p` made absolute with `.`/`..` folded, its longest existing part
/// resolved (links followed), and the Windows `\\?\` prefix dropped, so
/// two spellings of one place compare equal whether or not it exists.
fn resolved(p: &Path) -> PathBuf {
    let abs = lexical(&std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf()));
    let mut existing = abs.clone();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    while !existing.exists() {
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name.to_os_string());
                existing = parent.to_path_buf();
            }
            _ => break,
        }
    }
    let mut out = fs::canonicalize(&existing).unwrap_or(existing);
    let s = out.to_string_lossy().into_owned();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        out = PathBuf::from(format!(r"\\{rest}"));
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        out = PathBuf::from(rest);
    }
    for part in tail.iter().rev() {
        out.push(part);
    }
    out
}

fn lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Read a config file: its text, its hash and what [`check`] says about it.
/// The text is `Err` only when the file cannot be read at all.
pub fn read(path: &Path) -> Result<(String, String, Result<ConfigFacts, String>), String> {
    let meta = fs::metadata(path).map_err(|e| format!("it cannot be read: {e}"))?;
    if meta.len() > MAX_CONFIG_BYTES {
        return Err(format!("it is larger than {} KiB", MAX_CONFIG_BYTES / 1024));
    }
    let bytes = fs::read(path).map_err(|e| format!("it cannot be read: {e}"))?;
    let hash = content_hash(&bytes);
    let text = String::from_utf8(bytes).map_err(|_| "it is not UTF-8 text".to_string())?;
    let facts = check(&text);
    Ok((text, hash, facts))
}

/// The project's config as the gates know it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ProjectScanConfig {
    /// The project names none.
    #[default]
    None,
    /// Named and trusted with this content hash.
    Trusted { path: PathBuf, hash: String },
    /// Named but not trusted (or not valid): never used. `reason` says why.
    NotUsed { path: PathBuf, reason: String },
}

/// The value of the trusted-hash argument that says nothing is trusted.
pub const UNTRUSTED: &str = "none";

impl ProjectScanConfig {
    /// From what the app hands the stack runner: the config's path and its
    /// trusted hash (a 64-hex hash, or [`UNTRUSTED`]). A path without a
    /// hash is not trusted; a hash without a path is a mistake.
    pub fn from_args(path: Option<PathBuf>, hash: Option<&str>) -> Result<Self, String> {
        match (path, hash.map(str::trim)) {
            (None, None) => Ok(ProjectScanConfig::None),
            (None, Some(_)) => Err("a project config hash needs the config's path".into()),
            (Some(path), Some(h)) if h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()) => {
                Ok(ProjectScanConfig::Trusted {
                    path,
                    hash: h.to_ascii_lowercase(),
                })
            }
            (Some(path), None) | (Some(path), Some(UNTRUSTED)) => {
                let reason = untrusted_reason(&path);
                Ok(ProjectScanConfig::NotUsed { path, reason })
            }
            (Some(_), Some(other)) => Err(format!(
                "the project config hash must be 64 hex characters or `{UNTRUSTED}`, not `{other}`"
            )),
        }
    }

    /// The trusted-hash argument for this config ([`UNTRUSTED`] unless
    /// trusted); `None` when the project names no config.
    pub fn args(&self) -> Option<(&Path, &str)> {
        match self {
            ProjectScanConfig::None => None,
            ProjectScanConfig::Trusted { path, hash } => Some((path, hash)),
            ProjectScanConfig::NotUsed { path, .. } => Some((path, UNTRUSTED)),
        }
    }
}

/// Why a config nobody trusted is not used: what [`check`] says when it
/// fails a rule, else that it is not trusted.
pub fn untrusted_reason(path: &Path) -> String {
    match read(path) {
        Err(e) => format!("is not used: {e}"),
        Ok((_, _, Err(e))) => format!("is not used: {e}"),
        Ok(_) => "is not trusted (it is new, or it changed since it was trusted)".into(),
    }
}

/// Where the config a scan uses came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSource {
    Project,
    Settings,
    Default,
}

/// The config one scan uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chosen {
    pub source: ConfigSource,
    /// The project config's checked text ([`ConfigSource::Project`]).
    pub text: Option<String>,
    /// The Settings config's path ([`ConfigSource::Settings`]), or the base a
    /// project config is layered on ([`ConfigSource::Project`]; `None`: the
    /// app's own).
    pub path: Option<PathBuf>,
    /// Why the project's own config is not used, for the review banner.
    pub review: Option<String>,
}

impl Chosen {
    /// Point `scanner` at this config.
    pub fn apply(&self, scanner: &mut super::GitleaksScanner) {
        scanner.config_text = self.text.clone();
        scanner.config = self.path.clone();
    }
}

/// The config a scan uses now (see the module docs for the order). A
/// trusted project config is read and its hash checked here, so call this
/// before every scan and use the text it returns.
pub fn choose(project: &ProjectScanConfig, settings: Option<&Path>) -> Chosen {
    let fallback = |review: Option<String>| match settings {
        Some(p) => Chosen {
            source: ConfigSource::Settings,
            text: None,
            path: Some(p.to_path_buf()),
            review,
        },
        None => Chosen {
            source: ConfigSource::Default,
            text: None,
            path: None,
            review,
        },
    };
    let uses = if settings.is_some() {
        "the Settings ▸ Security config"
    } else {
        "the built-in rules"
    };
    let review = |path: &Path, why: &str| {
        format!(
            "The project's secret-scan config ({}) {why}, so scans use {uses} instead (which can only find more). Review it in the project's trust prompt in the app.",
            path.display()
        )
    };
    match project {
        ProjectScanConfig::None => fallback(None),
        ProjectScanConfig::NotUsed { path, reason } => fallback(Some(review(path, reason))),
        ProjectScanConfig::Trusted { path, hash } => match read(path) {
            Err(e) => fallback(Some(review(path, &format!("is not used: {e}")))),
            Ok((_, now, _)) if &now != hash => {
                fallback(Some(review(path, "changed since it was trusted")))
            }
            Ok((_, _, Err(e))) => fallback(Some(review(path, &format!("is not used: {e}")))),
            Ok((_, _, Ok(_))) if settings.is_some_and(extends_a_file) => fallback(Some(review(
                path,
                "cannot be stacked on the Settings ▸ Security config, which itself extends another file (gitleaks follows only two levels)",
            ))),
            Ok((text, _, Ok(_))) => Chosen {
                source: ConfigSource::Project,
                text: Some(text),
                // The base it is layered on (see `layered`).
                path: settings.map(Path::to_path_buf),
                review: None,
            },
        },
    }
}

/// Whether the gitleaks config at `path` has an `[extend] path` of its own.
/// One that cannot be read or parsed counts as extending (it is not
/// stacked on; the scan with it alone reports its own error).
fn extends_a_file(path: &Path) -> bool {
    let Ok(text) = fs::read_to_string(path) else {
        return true;
    };
    match text.parse::<toml::Table>() {
        Ok(t) => t
            .get("extend")
            .and_then(toml::Value::as_table)
            .is_some_and(|e| e.contains_key("path")),
        Err(_) => true,
    }
}

/// A checked project config's text with its `[extend]` pointing at `base`
/// (see the module docs): `useDefault` dropped (the base brings gitleaks'
/// defaults), `disabledRules` kept, everything else as written.
pub fn layered(project_text: &str, base: &Path) -> Result<String, String> {
    let mut table: toml::Table = project_text
        .parse()
        .map_err(|e: toml::de::Error| format!("it is not valid TOML: {}", e.message()))?;
    let mut extend = match table.remove("extend") {
        Some(toml::Value::Table(t)) => t,
        _ => toml::Table::new(),
    };
    extend.remove("useDefault");
    extend.insert(
        "path".into(),
        toml::Value::String(base.to_string_lossy().into_owned()),
    );
    table.insert("extend".into(), toml::Value::Table(extend));
    toml::to_string(&table).map_err(|e| e.to_string())
}

/// Why a Settings ▸ Security config path cannot be used: it lies inside one
/// of the open project folders `roots`. Such a file is part of a tree it
/// would configure the scan of; a project's own config goes through
/// `chappa.yml`, where it is trusted like a command.
pub fn settings_path_refusal<'a>(
    path: &Path,
    roots: impl IntoIterator<Item = &'a Path>,
) -> Option<String> {
    let root = roots.into_iter().find(|r| is_inside(path, r))?;
    Some(format!(
        "the gitleaks config {} is inside the project folder {}; use the project's own config through chappa.yml (`secretScan: {{ config: <path> }}`), which is trusted like a command",
        path.display(),
        root.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "title = \"project\"\n[extend]\nuseDefault = true\n\n[allowlist]\npaths = ['''fixtures/.*''']\nregexes = ['''^example$''']\n\n[[rules]]\nid = \"house\"\nregex = '''HOUSE-[0-9]{6}'''\n[[rules.allowlists]]\npaths = ['''docs/''']\n";

    #[test]
    fn a_config_must_keep_the_defaults_and_may_not_pull_in_another_file() {
        let facts = check(GOOD).unwrap();
        assert_eq!(
            facts.prompt_lines(),
            [
                "skips these files entirely: fixtures/.*",
                "skips these files entirely for rule `house`: docs/",
            ]
        );
        for (text, says) in [
            ("[allowlist]\npaths = ['''.*''']\n", "useDefault = true"),
            ("[extend]\nuseDefault = false\n", "useDefault = true"),
            ("[extend]\nuseDefault = \"true\"\n", "useDefault = true"),
            (
                "[extend]\nuseDefault = true\npath = \"other.toml\"\n",
                "`[extend] path` is refused",
            ),
            (
                "[extend]\npath = \"other.toml\"\n",
                "`[extend] path` is refused",
            ),
            (
                "[extend]\nuseDefault = true\nurl = \"x\"\n",
                "is not allowed",
            ),
            ("extend = 1\n", "must be a table"),
            ("[extend\nuseDefault = true\n", "not valid TOML"),
        ] {
            let err = check(text).unwrap_err();
            assert!(err.contains(says), "{text:?}: {err}");
        }
        // A string search would be fooled by a comment or a string value.
        let commented = "# [extend]\n# useDefault = true\ntitle = \"useDefault = true\"\n";
        assert!(check(commented).is_err());
    }

    #[test]
    fn turned_off_rules_are_listed_and_the_layered_text_extends_the_base() {
        let text = "[extend]\nuseDefault = true\ndisabledRules = [\"generic-api-key\", \"inline-url-password\"]\n\n[allowlist]\npaths = ['''^samples/''']\n";
        let facts = check(text).unwrap();
        assert_eq!(
            facts.prompt_lines(),
            [
                "turns off these rules: generic-api-key, inline-url-password",
                "skips these files entirely: ^samples/",
            ]
        );
        assert!(check("[extend]\nuseDefault = true\ndisabledRules = \"x\"\n").is_err());
        let base = Path::new("base dir").join("app.toml");
        let out: toml::Table = layered(text, &base).unwrap().parse().unwrap();
        let extend = out["extend"].as_table().unwrap();
        assert_eq!(
            extend.get("path").and_then(toml::Value::as_str),
            Some(base.to_string_lossy().as_ref())
        );
        assert!(!extend.contains_key("useDefault"));
        assert_eq!(extend["disabledRules"].as_array().unwrap().len(), 2);
        assert_eq!(out["allowlist"]["paths"][0].as_str(), Some("^samples/"));
    }

    #[test]
    fn the_path_must_be_relative_and_stay_in_the_project() {
        for ok in [
            "gitleaks.toml",
            "tools/gitleaks.toml",
            "tools\\scan.toml",
            "./a.toml",
        ] {
            assert_eq!(path_problem(ok), None, "{ok}");
        }
        for bad in [
            "",
            "/etc/x.toml",
            "C:\\x.toml",
            "c:/x.toml",
            "../x.toml",
            "a/../../x",
            "a\\..\\x",
        ] {
            assert!(path_problem(bad).is_some(), "{bad}");
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("p");
        fs::create_dir_all(root.join("tools")).unwrap();
        assert_eq!(
            resolve_in_project(&root, "tools/x.toml").unwrap(),
            root.join("tools").join("x.toml")
        );
        assert!(resolve_in_project(&root, "../x.toml").is_err());
    }

    #[test]
    fn a_trusted_project_config_comes_first_and_anything_else_falls_back_with_a_reason() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("gitleaks.toml");
        fs::write(&path, GOOD).unwrap();
        let hash = content_hash(GOOD.as_bytes());
        let settings = tmp.path().join("settings.toml");
        fs::write(&settings, "[extend]\nuseDefault = true\n").unwrap();
        let trusted = ProjectScanConfig::Trusted {
            path: path.clone(),
            hash: hash.clone(),
        };

        let c = choose(&trusted, Some(&settings));
        assert_eq!(
            (c.source, c.review.as_deref()),
            (ConfigSource::Project, None)
        );
        assert_eq!(
            c.text.as_deref(),
            Some(GOOD),
            "gitleaks gets the checked bytes, not the path"
        );
        assert_eq!(
            c.path.as_deref(),
            Some(settings.as_path()),
            "layered on the Settings config"
        );
        assert_eq!(choose(&trusted, None).path, None, "layered on the app's");

        // A Settings config that extends a file cannot be stacked on.
        let nested = tmp.path().join("nested.toml");
        fs::write(&nested, "[extend]\npath = \"settings.toml\"\n").unwrap();
        let c = choose(&trusted, Some(&nested));
        assert_eq!(c.source, ConfigSource::Settings);
        assert!(c.review.unwrap().contains("cannot be stacked"));

        // Changed on disk: Settings, then the default, each with the reason.
        fs::write(&path, GOOD.replace("fixtures", "src")).unwrap();
        let c = choose(&trusted, Some(&settings));
        assert_eq!(c.source, ConfigSource::Settings);
        assert_eq!(c.path.as_deref(), Some(settings.as_path()));
        assert!(c
            .review
            .as_deref()
            .unwrap()
            .contains("changed since it was trusted"));
        let c = choose(&trusted, None);
        assert_eq!((c.source, c.text.clone()), (ConfigSource::Default, None));
        assert!(c.review.unwrap().contains("the built-in rules"));

        // Trusted bytes that break a rule are still never used.
        let bad = "[extend]\nuseDefault = true\npath = \"x.toml\"\n";
        fs::write(&path, bad).unwrap();
        let c = choose(
            &ProjectScanConfig::Trusted {
                path: path.clone(),
                hash: content_hash(bad.as_bytes()),
            },
            None,
        );
        assert_eq!(c.source, ConfigSource::Default);
        assert!(c.review.unwrap().contains("`[extend] path` is refused"));

        let c = choose(
            &ProjectScanConfig::NotUsed {
                path: path.clone(),
                reason: "is not trusted yet".into(),
            },
            None,
        );
        assert!(c.review.unwrap().contains("is not trusted yet"));
        assert_eq!(choose(&ProjectScanConfig::None, None).review, None);
    }

    #[test]
    fn a_settings_path_inside_an_open_project_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("proj");
        fs::create_dir_all(project.join("cfg")).unwrap();
        let inside = project.join("cfg").join("g.toml");
        let outside = tmp.path().join("g.toml");
        let roots = [project.as_path()];
        let why = settings_path_refusal(&inside, roots).unwrap();
        assert!(
            why.contains("use the project's own config through chappa.yml"),
            "{why}"
        );
        assert!(
            settings_path_refusal(&project.join("x").join("..").join("g.toml"), roots).is_some()
        );
        assert_eq!(settings_path_refusal(&outside, roots), None);
    }
}
