//! The source manifest (`GET /chappa/v1/manifest`): parsing, the closed
//! endpoint list, the poll-interval clamp, the trust hash, and the two
//! address rules (loopback only; paths relative to the source's address).
//!
//! Everything here is pure, so the rules are tested without a socket.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

use super::identity::MANIFEST_PATH;

/// The one contract version this build speaks. Changes within it are
/// additive; a breaking change would move to `/chappa/v2`.
pub const CONTRACT_VERSION: u64 = 1;
/// `pollSeconds` bounds. Below 15 s a poller would be a load on the
/// source's upstream; above an hour the view would be stale by any reading.
pub const POLL_MIN: Duration = Duration::from_secs(15);
pub const POLL_MAX: Duration = Duration::from_secs(3600);
/// What a view polls at when the manifest names no interval.
pub const POLL_DEFAULT: Duration = Duration::from_secs(60);
/// The optional `workflows` list is fetched like a view, at this cadence.
pub const WORKFLOWS_POLL: Duration = Duration::from_secs(300);
/// A failing view backs off exponentially up to this multiple of its
/// interval, so a source that is down is probed, not hammered, and comes
/// back within eight intervals of recovering.
pub const BACKOFF_CAP_MULTIPLIER: u32 = 8;
/// The rail views a source can serve, in rail order.
pub const VIEW_NAMES: [&str; 4] = ["work", "prs", "attention", "environment"];
/// The pseudo-view under which the `workflows` list is cached.
pub const WORKFLOWS_VIEW: &str = "workflows";

/// A path the source named in its manifest. Private field: the ONLY ways
/// to get one are parsing a manifest and [`Endpoint::manifest`], so a request
/// for a path the source did not declare cannot be expressed at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Endpoint(String);

impl Endpoint {
    /// The manifest route itself, the one fixed path of the contract.
    pub fn manifest() -> Self {
        Endpoint(MANIFEST_PATH.to_owned())
    }

    pub fn path(&self) -> &str {
        &self.0
    }

    fn declared(path: &str, what: &str) -> Result<Self, ManifestError> {
        validate_endpoint_path(path).map_err(|why| ManifestError::Invalid(format!("{what}: {why}")))?;
        Ok(Endpoint(path.to_owned()))
    }

    /// A path a TRUSTED source's own payload named (a `panel` action's
    /// `endpoint`). The caller has already found the path in the cached
    /// payload — that lookup is the closed list for panels, as the manifest
    /// is for views — and this applies the same shape rules.
    pub(crate) fn named_in_payload(path: &str) -> Result<Self, String> {
        validate_endpoint_path(path)?;
        Ok(Endpoint(path.to_owned()))
    }

    /// This endpoint with its `{placeholder}` filled in (`jobs/{id}`,
    /// `item/{key}`). The value is percent-encoded so a job id with `/`,
    /// `?` or spaces stays one path segment and cannot reach another route.
    pub fn with_placeholder(&self, name: &str, value: &str) -> Endpoint {
        let slot = format!("{{{name}}}");
        Endpoint(self.0.replace(&slot, &percent_encode(value)))
    }

    /// Whether the path carries `{name}` (a `jobs` path needs `{id}`).
    pub fn has_placeholder(&self, name: &str) -> bool {
        self.0.contains(&format!("{{{name}}}"))
    }
}

/// RFC 3986 unreserved characters pass; everything else, byte by byte, is
/// `%XX`. Strict on purpose: the value fills ONE path segment.
pub fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// One view the source serves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ViewEndpoint {
    pub list: Endpoint,
    /// Item detail, with a `{key}` placeholder (Work only).
    pub item: Option<Endpoint>,
    /// Already clamped to [`POLL_MIN`]..=[`POLL_MAX`].
    pub poll: Duration,
}

/// The parsed, validated manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Manifest {
    pub id: String,
    pub label: String,
    pub version: String,
    /// Only the known view names, in [`VIEW_NAMES`] order.
    pub views: BTreeMap<String, ViewEndpoint>,
    pub workflows: Option<Endpoint>,
    pub actions: Option<Endpoint>,
    pub jobs: Option<Endpoint>,
    /// As declared (the trust hash sorts its own copy).
    pub link_hosts: Vec<String>,
    pub roots: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    /// A different contract version: the source (or the app) needs an update.
    NeedsUpdate { contract: Option<u64> },
    /// Not a manifest this build can use; the reason is shown as the error.
    Invalid(String),
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ManifestError::NeedsUpdate { contract: Some(c) } => {
                write!(f, "the source speaks contract version {c}; this app speaks {CONTRACT_VERSION}")
            }
            ManifestError::NeedsUpdate { contract: None } => {
                f.write_str("the manifest names no contract version")
            }
            ManifestError::Invalid(why) => write!(f, "unusable manifest: {why}"),
        }
    }
}

/// Parse and validate a manifest body. Unknown fields and unknown view
/// names are ignored (additive changes within the contract). A wrong
/// contract version is reported as [`ManifestError::NeedsUpdate`] before
/// anything else is looked at, so the user sees "needs update", not a
/// validation message about a shape this build does not know.
pub fn parse_manifest(value: &Value) -> Result<Manifest, ManifestError> {
    let obj = value
        .as_object()
        .ok_or_else(|| ManifestError::Invalid("not a JSON object".into()))?;
    match obj.get("contract").and_then(Value::as_u64) {
        Some(CONTRACT_VERSION) => {}
        other => return Err(ManifestError::NeedsUpdate { contract: other }),
    }
    let source = obj
        .get("source")
        .and_then(Value::as_object)
        .ok_or_else(|| ManifestError::Invalid("no `source` object".into()))?;
    let text = |map: &serde_json::Map<String, Value>, key: &str| -> Result<String, ManifestError> {
        map.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| ManifestError::Invalid(format!("`source.{key}` must be a non-empty string")))
    };
    let id = text(source, "id")?;
    let label = text(source, "label")?;
    let version = text(source, "version")?;

    let mut views = BTreeMap::new();
    if let Some(declared) = obj.get("views") {
        let declared = declared
            .as_object()
            .ok_or_else(|| ManifestError::Invalid("`views` must be an object".into()))?;
        for name in VIEW_NAMES {
            let Some(view) = declared.get(name) else { continue };
            let view = view
                .as_object()
                .ok_or_else(|| ManifestError::Invalid(format!("`views.{name}` must be an object")))?;
            let list = view
                .get("list")
                .and_then(Value::as_str)
                .ok_or_else(|| ManifestError::Invalid(format!("`views.{name}.list` must be a path")))?;
            let list = Endpoint::declared(list, &format!("views.{name}.list"))?;
            let item = match view.get("item") {
                None | Some(Value::Null) => None,
                Some(Value::String(path)) => Some(Endpoint::declared(path, &format!("views.{name}.item"))?),
                Some(_) => return Err(ManifestError::Invalid(format!("`views.{name}.item` must be a path"))),
            };
            let poll = match view.get("pollSeconds") {
                None | Some(Value::Null) => POLL_DEFAULT,
                Some(Value::Number(n)) => clamp_poll(n.as_f64().unwrap_or(0.0)),
                Some(_) => return Err(ManifestError::Invalid(format!("`views.{name}.pollSeconds` must be a number"))),
            };
            views.insert(name.to_owned(), ViewEndpoint { list, item, poll });
        }
    }
    let optional = |key: &str| -> Result<Option<Endpoint>, ManifestError> {
        match obj.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(path)) => Endpoint::declared(path, key).map(Some),
            Some(_) => Err(ManifestError::Invalid(format!("`{key}` must be a path"))),
        }
    };
    let strings = |key: &str| -> Result<Vec<String>, ManifestError> {
        match obj.get(key) {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::Array(items)) => items
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .ok_or_else(|| ManifestError::Invalid(format!("`{key}` entries must be non-empty strings")))
                })
                .collect(),
            Some(_) => Err(ManifestError::Invalid(format!("`{key}` must be a list"))),
        }
    };
    let link_hosts = strings("linkHosts")?;
    for host in &link_hosts {
        validate_link_host(host).map_err(|why| ManifestError::Invalid(format!("linkHosts `{host}`: {why}")))?;
    }
    Ok(Manifest {
        id,
        label,
        version,
        views,
        workflows: optional("workflows")?,
        actions: optional("actions")?,
        jobs: optional("jobs")?,
        link_hosts,
        roots: strings("roots")?,
    })
}

/// A manifest path is RELATIVE TO THE SOURCE'S ADDRESS: an absolute path on
/// the same host, nothing more. A URL (`http://…`), a protocol-relative
/// `//host/…`, a bare `host/path`, or anything with whitespace or control
/// characters is refused, so a manifest can never point the poller at
/// another host, and so the request line stays one line.
pub fn validate_endpoint_path(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("empty path".into());
    }
    if !path.starts_with('/') {
        return Err(format!("`{path}` is not relative to the source's address (must start with `/`)"));
    }
    if path.starts_with("//") {
        return Err(format!("`{path}` would name another host"));
    }
    if path.contains("://") {
        return Err(format!("`{path}` is a URL, not a path"));
    }
    if path.bytes().any(|b| b <= b' ' || b == 0x7f) {
        return Err(format!("`{path}` contains whitespace or control characters"));
    }
    if path.split(['/', '?', '#']).any(|seg| seg == "..") {
        return Err(format!("`{path}` climbs with `..`"));
    }
    Ok(())
}

/// A link host is a bare host name (optionally `:port`): no scheme, no
/// path, no wildcard. The later link enforcement compares hosts exactly.
fn validate_link_host(host: &str) -> Result<(), String> {
    if host.contains("://") || host.contains('/') || host.contains('*') {
        return Err("must be a bare host name, no scheme, path or wildcard".into());
    }
    if host.bytes().any(|b| b <= b' ' || b == 0x7f) {
        return Err("contains whitespace or control characters".into());
    }
    Ok(())
}

/// Clamp a declared `pollSeconds` into the contract's range; a value that is
/// not a positive number polls at the minimum.
pub fn clamp_poll(seconds: f64) -> Duration {
    if !seconds.is_finite() || seconds <= 0.0 {
        return POLL_MIN;
    }
    Duration::from_secs_f64(seconds).clamp(POLL_MIN, POLL_MAX)
}

/// The delay before the next attempt after `failures` consecutive failures
/// of a view polled every `poll`: `poll × 2^failures`, capped at
/// `poll × 8`. Zero failures is the plain interval.
pub fn backoff_delay(poll: Duration, failures: u32) -> Duration {
    let factor = 2u32.saturating_pow(failures.min(8)).min(BACKOFF_CAP_MULTIPLIER);
    poll.saturating_mul(factor)
}

/// The address a source listens on must be a loopback IP literal with a
/// port. A host name (even `localhost`) is refused: it needs a resolver,
/// and what it resolves to is not under this check's control.
pub fn parse_loopback_addr(text: &str) -> Result<SocketAddr, String> {
    let addr: SocketAddr = text
        .trim()
        .parse()
        .map_err(|_| format!("`{text}` is not an `ip:port` address"))?;
    if !addr.ip().is_loopback() {
        return Err(format!("`{text}` is not a loopback address; a source must listen on 127.0.0.1 or [::1]"));
    }
    if addr.port() == 0 {
        return Err(format!("`{text}` names no port"));
    }
    Ok(addr)
}

/// What the user approves: the source's id and the lists it asked for.
/// Sorted copies, so the hash does not change when a source reorders them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrustSummary {
    pub id: String,
    pub label: String,
    pub link_hosts: Vec<String>,
    pub roots: Vec<String>,
    pub views: Vec<String>,
    pub hash: String,
}

/// The canonical JSON the trust hash is taken over:
/// `{"id":…,"linkHosts":[sorted],"roots":[sorted],"views":[sorted keys]}`
/// with exactly those keys in that order and no whitespace. Built by hand
/// rather than through a map so the key order cannot depend on how
/// `serde_json` was compiled.
pub fn canonical_trust_json(manifest: &Manifest) -> String {
    let mut hosts = manifest.link_hosts.clone();
    hosts.sort();
    hosts.dedup();
    let mut roots = manifest.roots.clone();
    roots.sort();
    roots.dedup();
    let views: Vec<&String> = manifest.views.keys().collect(); // BTreeMap: sorted
    let list = |items: &[String]| -> String {
        let quoted: Vec<String> = items.iter().map(|s| Value::String(s.clone()).to_string()).collect();
        format!("[{}]", quoted.join(","))
    };
    let view_names: Vec<String> = views.into_iter().cloned().collect();
    format!(
        "{{\"id\":{},\"linkHosts\":{},\"roots\":{},\"views\":{}}}",
        Value::String(manifest.id.clone()),
        list(&hosts),
        list(&roots),
        list(&view_names)
    )
}

/// The trust hash: BLAKE3 hex of [`canonical_trust_json`], like the project
/// file's content hash. Trust covers exactly this hash; any change to the
/// id, the hosts, the roots or the set of views re-arms the gate.
pub fn trust_hash(manifest: &Manifest) -> String {
    project_model::hash::content_hash(canonical_trust_json(manifest).as_bytes())
}

pub fn trust_summary(manifest: &Manifest) -> TrustSummary {
    let mut link_hosts = manifest.link_hosts.clone();
    link_hosts.sort();
    link_hosts.dedup();
    let mut roots = manifest.roots.clone();
    roots.sort();
    roots.dedup();
    TrustSummary {
        id: manifest.id.clone(),
        label: manifest.label.clone(),
        link_hosts,
        roots,
        views: manifest.views.keys().cloned().collect(),
        hash: trust_hash(manifest),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The example source's manifest, verbatim: the parser must accept what
    /// the documentation tells source authors to write.
    const FIXTURE: &str = include_str!("../../../sources/fixture/data/manifest.json");

    fn fixture() -> Manifest {
        parse_manifest(&serde_json::from_str(FIXTURE).unwrap()).unwrap()
    }

    #[test]
    fn the_fixture_manifest_parses_with_every_field() {
        let m = fixture();
        assert_eq!((m.id.as_str(), m.label.as_str(), m.version.as_str()), ("fixture", "Fixture", "0.1.0"));
        assert_eq!(m.views.keys().cloned().collect::<Vec<_>>(), ["attention", "environment", "prs", "work"]);
        assert_eq!(m.views["work"].list.path(), "/api/work");
        assert_eq!(m.views["work"].item.as_ref().unwrap().path(), "/api/work/{key}");
        assert_eq!(m.views["work"].poll, Duration::from_secs(60));
        assert_eq!(m.views["prs"].poll, Duration::from_secs(300));
        assert_eq!(m.views["attention"].poll, POLL_DEFAULT, "no pollSeconds = the default");
        assert!(m.views["prs"].item.is_none());
        assert_eq!(m.workflows.as_ref().unwrap().path(), "/api/workflows");
        assert_eq!(m.actions.as_ref().unwrap().path(), "/api/actions");
        assert_eq!(m.jobs.as_ref().unwrap().path(), "/api/jobs/{id}");
        assert_eq!(m.link_hosts, ["registry.example", "dev.example.test"]);
        assert_eq!(m.roots, ["/srv/example"]);
    }

    #[test]
    fn placeholders_are_filled_with_one_encoded_segment() {
        let jobs = fixture().jobs.unwrap();
        assert!(jobs.has_placeholder("id"));
        assert!(!jobs.has_placeholder("key"));
        assert_eq!(jobs.with_placeholder("id", "job-1").path(), "/api/jobs/job-1");
        assert_eq!(jobs.with_placeholder("id", "a/b?c=d e#f").path(), "/api/jobs/a%2Fb%3Fc%3Dd%20e%23f");
        assert_eq!(jobs.with_placeholder("id", "../other").path(), "/api/jobs/..%2Fother");
        assert_eq!(percent_encode("A-z_0.9~"), "A-z_0.9~");
        assert_eq!(percent_encode("é"), "%C3%A9");
        // A payload-named panel path gets the same shape rules as the manifest.
        assert_eq!(Endpoint::named_in_payload("/api/panel/validate").unwrap().path(), "/api/panel/validate");
        assert!(Endpoint::named_in_payload("http://10.0.0.5/x").is_err());
        assert!(Endpoint::named_in_payload("/api/../x").is_err());
        assert!(Endpoint::named_in_payload("api/panel").is_err());
    }

    #[test]
    fn a_wrong_or_missing_contract_is_needs_update_not_a_crash() {
        let mut v: Value = serde_json::from_str(FIXTURE).unwrap();
        v["contract"] = json!(2);
        assert_eq!(parse_manifest(&v), Err(ManifestError::NeedsUpdate { contract: Some(2) }));
        v.as_object_mut().unwrap().remove("contract");
        assert_eq!(parse_manifest(&v), Err(ManifestError::NeedsUpdate { contract: None }));
        // The version check comes first: a v2 manifest with shapes this build
        // does not know is still "needs update".
        assert!(matches!(
            parse_manifest(&json!({"contract": 2, "views": "not an object"})),
            Err(ManifestError::NeedsUpdate { .. })
        ));
    }

    #[test]
    fn unknown_fields_and_unknown_views_are_ignored() {
        let v = json!({
            "contract": 1,
            "source": {"id": "x", "label": "X", "version": "1", "extra": true},
            "views": {"work": {"list": "/w", "future": 1}, "calendar": {"list": "/cal"}},
            "future": {"a": 1}
        });
        let m = parse_manifest(&v).unwrap();
        assert_eq!(m.views.keys().cloned().collect::<Vec<_>>(), ["work"]);
        assert!(m.link_hosts.is_empty() && m.roots.is_empty());
        // Every view is optional.
        let bare = parse_manifest(&json!({"contract": 1, "source": {"id": "x", "label": "X", "version": "1"}})).unwrap();
        assert!(bare.views.is_empty());
    }

    #[test]
    fn poll_seconds_are_clamped_to_the_contract_range() {
        assert_eq!(clamp_poll(14.0), Duration::from_secs(15));
        assert_eq!(clamp_poll(15.0), Duration::from_secs(15));
        assert_eq!(clamp_poll(3600.0), Duration::from_secs(3600));
        assert_eq!(clamp_poll(3601.0), Duration::from_secs(3600));
        assert_eq!(clamp_poll(0.0), Duration::from_secs(15));
        assert_eq!(clamp_poll(-5.0), Duration::from_secs(15));
        assert_eq!(clamp_poll(f64::NAN), Duration::from_secs(15));
        let m = parse_manifest(&json!({
            "contract": 1, "source": {"id": "x", "label": "X", "version": "1"},
            "views": {"work": {"list": "/w", "pollSeconds": 1}, "prs": {"list": "/p", "pollSeconds": 99999}}
        }))
        .unwrap();
        assert_eq!(m.views["work"].poll, POLL_MIN);
        assert_eq!(m.views["prs"].poll, POLL_MAX);
    }

    #[test]
    fn endpoint_paths_must_be_relative_to_the_source() {
        for ok in ["/api/work", "/api/work/{key}", "/a?b=c", "/"] {
            assert!(validate_endpoint_path(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "api/work",
            "http://127.0.0.1:5000/api/work",
            "https://evil.example/api",
            "//evil.example/api",
            "/api/../other",
            "/api/w ork",
            "/api/w\u{7f}ork",
            "/api/w\r\nX-Injected: 1",
        ] {
            assert!(validate_endpoint_path(bad).is_err(), "{bad:?} must be refused");
        }
        // A manifest naming such a path is unusable as a whole.
        let v = json!({
            "contract": 1, "source": {"id": "x", "label": "X", "version": "1"},
            "views": {"work": {"list": "http://10.0.0.5/api/work"}}
        });
        assert!(matches!(parse_manifest(&v), Err(ManifestError::Invalid(_))));
        let v = json!({
            "contract": 1, "source": {"id": "x", "label": "X", "version": "1"},
            "actions": "//other.host/actions"
        });
        assert!(matches!(parse_manifest(&v), Err(ManifestError::Invalid(_))));
    }

    #[test]
    fn link_hosts_are_bare_host_names() {
        let with = |host: &str| {
            parse_manifest(&json!({
                "contract": 1, "source": {"id": "x", "label": "X", "version": "1"},
                "linkHosts": [host]
            }))
        };
        assert!(with("github.com").is_ok());
        assert!(with("git.example.test:8443").is_ok());
        assert!(with("https://github.com").is_err());
        assert!(with("github.com/org").is_err());
        assert!(with("*.github.com").is_err());
    }

    #[test]
    fn only_loopback_ip_literals_are_accepted_as_source_addresses() {
        assert!(parse_loopback_addr("127.0.0.1:5000").is_ok());
        assert!(parse_loopback_addr("127.5.6.7:1").is_ok(), "the whole 127/8 block is loopback");
        assert!(parse_loopback_addr("[::1]:9").is_ok());
        assert!(parse_loopback_addr(" 127.0.0.1:5000 ").is_ok(), "surrounding whitespace is trimmed");
        for bad in ["10.0.0.5:5000", "0.0.0.0:5000", "[::]:5000", "192.168.1.2:80", "localhost:5000", "127.0.0.1", "127.0.0.1:0", "http://127.0.0.1:5000", ""] {
            assert!(parse_loopback_addr(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn backoff_doubles_per_failure_and_caps_at_eight_intervals() {
        let poll = Duration::from_secs(60);
        assert_eq!(backoff_delay(poll, 0), Duration::from_secs(60));
        assert_eq!(backoff_delay(poll, 1), Duration::from_secs(120));
        assert_eq!(backoff_delay(poll, 2), Duration::from_secs(240));
        assert_eq!(backoff_delay(poll, 3), Duration::from_secs(480));
        assert_eq!(backoff_delay(poll, 4), Duration::from_secs(480), "capped at poll × 8");
        assert_eq!(backoff_delay(poll, 40), Duration::from_secs(480));
        assert_eq!(backoff_delay(POLL_MAX, u32::MAX), POLL_MAX * 8, "no overflow at the extremes");
    }

    #[test]
    fn the_trust_hash_covers_id_hosts_roots_and_view_names_in_canonical_form() {
        let m = fixture();
        assert_eq!(
            canonical_trust_json(&m),
            r#"{"id":"fixture","linkHosts":["dev.example.test","registry.example"],"roots":["/srv/example"],"views":["attention","environment","prs","work"]}"#
        );
        let hash = trust_hash(&m);
        assert_eq!(hash.len(), 64);
        // Reordering the declared lists does not change the hash…
        let mut reordered = m.clone();
        reordered.link_hosts.reverse();
        assert_eq!(trust_hash(&reordered), hash);
        // …nor does anything the hash does not cover (paths, intervals,
        // version, label), because those are not what the user approved.
        let mut cosmetic = m.clone();
        cosmetic.version = "9.9.9".into();
        cosmetic.label = "Other".into();
        cosmetic.views.get_mut("work").unwrap().poll = POLL_MAX;
        cosmetic.actions = None;
        assert_eq!(trust_hash(&cosmetic), hash);
        // …but a new host, a new root, a new view or another id all do.
        let mut host = m.clone();
        host.link_hosts.push("evil.example".into());
        assert_ne!(trust_hash(&host), hash);
        let mut root = m.clone();
        root.roots.push("/".into());
        assert_ne!(trust_hash(&root), hash);
        let mut fewer = m.clone();
        fewer.views.remove("prs");
        assert_ne!(trust_hash(&fewer), hash);
        let mut id = m.clone();
        id.id = "fixture2".into();
        assert_ne!(trust_hash(&id), hash);
        // Strings are JSON-escaped, so a quote in an id cannot forge the shape.
        let mut quoted = m.clone();
        quoted.id = "a\",\"linkHosts\":[\"x".into();
        assert!(canonical_trust_json(&quoted).starts_with(r#"{"id":"a\",\"linkHosts\":[\"x","#));
        let summary = trust_summary(&m);
        assert_eq!(summary.hash, hash);
        assert_eq!(summary.link_hosts, ["dev.example.test", "registry.example"]);
        assert_eq!(summary.views, ["attention", "environment", "prs", "work"]);
    }
}
