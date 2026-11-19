//! The source manifest (`GET /chappa/v1/manifest`): parsing, the closed
//! endpoint list, the poll-interval clamp, the trust hash, and the two
//! address rules (loopback only; paths relative to the source's address).
//!
//! Everything here is pure, so the rules are tested without a socket.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use project_model::link_hosts::{any_port_host, check_any_port_host};
use serde::Serialize;
use serde_json::Value;

use super::actions::{parse_action, NoteRule, SourceAction};
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
/// The views whose rows another source's `itemActions` may be offered on.
pub const ITEM_ACTION_VIEWS: [&str; 2] = ["work", "prs"];
/// `on.sources` value meaning "the rows of every other source".
pub const ANY_SOURCE: &str = "*";

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
        validate_endpoint_path(path)
            .map_err(|why| ManifestError::Invalid(format!("{what}: {why}")))?;
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
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
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
    /// Actions this source offers on OTHER sources' rows (a "Start work"
    /// before the source has anything of its own about the item).
    pub item_actions: Vec<ItemAction>,
    /// Where the app posts the owner rows this source adds to, each time
    /// one changes (`{source, key, item}`), so it sees a tracker's status
    /// move without waiting for the next action.
    pub owner_updates: Option<Endpoint>,
}

/// One `itemActions` entry: always an `endpoint` action (an agent or a
/// terminal would need a folder that cannot exist yet), plus where it is
/// offered. A mutating one carries its confirm text, or the manifest is
/// refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemAction {
    pub action: SourceAction,
    /// `work` or `prs`.
    pub view: String,
    /// The manifest ids of the sources whose rows get it, sorted; `["*"]`
    /// for every other source.
    pub sources: Vec<String>,
}

impl ItemAction {
    pub fn id(&self) -> &str {
        self.action.id()
    }

    /// Whether it is offered on a `view` row owned by the source whose
    /// manifest id is `owner`.
    pub fn offered_on(&self, view: &str, owner: &str) -> bool {
        self.view == view && self.sources.iter().any(|s| s == ANY_SOURCE || s == owner)
    }
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
                write!(
                    f,
                    "the source speaks contract version {c}; this app speaks {CONTRACT_VERSION}"
                )
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
            .ok_or_else(|| {
                ManifestError::Invalid(format!("`source.{key}` must be a non-empty string"))
            })
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
            let Some(view) = declared.get(name) else {
                continue;
            };
            let view = view.as_object().ok_or_else(|| {
                ManifestError::Invalid(format!("`views.{name}` must be an object"))
            })?;
            let list = view.get("list").and_then(Value::as_str).ok_or_else(|| {
                ManifestError::Invalid(format!("`views.{name}.list` must be a path"))
            })?;
            let list = Endpoint::declared(list, &format!("views.{name}.list"))?;
            let item = match view.get("item") {
                None | Some(Value::Null) => None,
                Some(Value::String(path)) => {
                    Some(Endpoint::declared(path, &format!("views.{name}.item"))?)
                }
                Some(_) => {
                    return Err(ManifestError::Invalid(format!(
                        "`views.{name}.item` must be a path"
                    )))
                }
            };
            let poll = match view.get("pollSeconds") {
                None | Some(Value::Null) => POLL_DEFAULT,
                Some(Value::Number(n)) => clamp_poll(n.as_f64().unwrap_or(0.0)),
                Some(_) => {
                    return Err(ManifestError::Invalid(format!(
                        "`views.{name}.pollSeconds` must be a number"
                    )))
                }
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
                        .ok_or_else(|| {
                            ManifestError::Invalid(format!(
                                "`{key}` entries must be non-empty strings"
                            ))
                        })
                })
                .collect(),
            Some(_) => Err(ManifestError::Invalid(format!("`{key}` must be a list"))),
        }
    };
    let link_hosts = strings("linkHosts")?;
    for host in &link_hosts {
        validate_link_host(host)
            .map_err(|why| ManifestError::Invalid(format!("linkHosts `{host}`: {why}")))?;
    }
    let roots = strings("roots")?;
    for root in &roots {
        validate_root(root)
            .map_err(|why| ManifestError::Invalid(format!("roots `{root}`: {why}")))?;
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
        roots,
        item_actions: parse_item_actions(obj.get("itemActions"))?,
        owner_updates: optional("ownerUpdates")?,
    })
}

/// A root must name one folder whatever the app's working directory is:
/// rooted (`/srv/x`, `C:\x`, and on Windows `\x`), never relative
/// (`x`, `./x`, `~/x`, `C:x`), and free of `..`.
fn validate_root(root: &str) -> Result<(), &'static str> {
    let path = std::path::Path::new(root);
    if !path.has_root() {
        return Err("must be an absolute folder");
    }
    if path
        .components()
        .any(|c| c == std::path::Component::ParentDir)
    {
        return Err("must not climb with `..`");
    }
    Ok(())
}

/// `itemActions`: a list of endpoint actions, each with `on: {view, sources}`.
/// Any bad entry refuses the manifest (as a bad path does): an action the
/// user is asked to trust must mean exactly what it says.
fn parse_item_actions(value: Option<&Value>) -> Result<Vec<ItemAction>, ManifestError> {
    let list = match value {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(list)) => list,
        Some(_) => {
            return Err(ManifestError::Invalid(
                "`itemActions` must be a list".into(),
            ))
        }
    };
    let mut out: Vec<ItemAction> = Vec::new();
    for (i, raw) in list.iter().enumerate() {
        let bad = |why: String| ManifestError::Invalid(format!("itemActions[{i}]: {why}"));
        let action = parse_action(raw).map_err(bad)?;
        if !matches!(action, SourceAction::Endpoint { .. }) {
            return Err(bad(format!(
                "`{}` is a `{}` action; only `endpoint` actions can be offered on other sources' rows",
                action.id(),
                action.kind()
            )));
        }
        if action.is_unconfirmable_mutation() {
            return Err(bad(format!(
                "`{}` changes something and has no `confirm` text",
                action.id()
            )));
        }
        if out.iter().any(|a| a.id() == action.id()) {
            return Err(bad(format!("the id `{}` is used twice", action.id())));
        }
        let on = raw
            .get("on")
            .and_then(Value::as_object)
            .ok_or_else(|| bad("`on` must be an object with `view` and `sources`".into()))?;
        let view = on
            .get("view")
            .and_then(Value::as_str)
            .filter(|v| ITEM_ACTION_VIEWS.contains(v))
            .ok_or_else(|| {
                bad(format!(
                    "`on.view` must be one of {}",
                    ITEM_ACTION_VIEWS.join(", ")
                ))
            })?
            .to_owned();
        let mut sources: Vec<String> = match on.get("sources") {
            Some(Value::String(s)) if s == ANY_SOURCE => vec![ANY_SOURCE.to_owned()],
            Some(Value::Array(ids)) => ids
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .ok_or_else(|| {
                            bad("`on.sources` entries must be non-empty source ids".into())
                        })
                })
                .collect::<Result<_, _>>()?,
            _ => {
                return Err(bad(
                    "`on.sources` must be a list of source ids or \"*\"".into()
                ))
            }
        };
        if sources.iter().any(|s| s == ANY_SOURCE) {
            sources = vec![ANY_SOURCE.to_owned()];
        }
        sources.sort();
        sources.dedup();
        out.push(ItemAction {
            action,
            view,
            sources,
        });
    }
    Ok(out)
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
        return Err(format!(
            "`{path}` is not relative to the source's address (must start with `/`)"
        ));
    }
    if path.starts_with("//") {
        return Err(format!("`{path}` would name another host"));
    }
    if path.contains("://") {
        return Err(format!("`{path}` is a URL, not a path"));
    }
    if path.bytes().any(|b| b <= b' ' || b == 0x7f) {
        return Err(format!(
            "`{path}` contains whitespace or control characters"
        ));
    }
    if path.split(['/', '?', '#']).any(|seg| seg == "..") {
        return Err(format!("`{path}` climbs with `..`"));
    }
    Ok(())
}

/// A link host is a bare host name (optionally `:port`): no scheme, no
/// path. The one wildcard is a whole port, `host:*` (any port on that
/// host), and only on a host that names one party (see
/// [`project_model::link_hosts`]). The later link enforcement compares
/// hosts exactly.
fn validate_link_host(host: &str) -> Result<(), String> {
    if host.contains("://") || host.contains('/') {
        return Err("must be a bare host name, no scheme or path".into());
    }
    if host.bytes().any(|b| b <= b' ' || b == 0x7f) {
        return Err("contains whitespace or control characters".into());
    }
    if let Some(bare) = any_port_host(host) {
        return check_any_port_host(&bare);
    }
    if host.contains('*') {
        return Err(
            "the only wildcard allowed is a whole port (`host:*`), never part of the host".into(),
        );
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
    let factor = 2u32
        .saturating_pow(failures.min(8))
        .min(BACKOFF_CAP_MULTIPLIER);
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
        return Err(format!(
            "`{text}` is not a loopback address; a source must listen on 127.0.0.1 or [::1]"
        ));
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
    /// The actions it offers on other sources' rows, sorted by id.
    pub item_actions: Vec<ItemActionSummary>,
    /// It is sent the rows of other sources' items it adds to, whenever one
    /// changes.
    pub owner_updates: bool,
    pub hash: String,
}

/// What the trust dialog says about one item action, and what the webview
/// needs to know which rows to ask about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemActionSummary {
    pub id: String,
    pub label: String,
    pub view: String,
    pub sources: Vec<String>,
    pub mutates: bool,
    pub confirm: Option<String>,
}

/// JSON with every object's keys sorted, so a source re-serialising the same
/// `params` in another key order keeps the same hash.
fn canonical_value(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .into_iter()
                .map(|k| format!("{}:{}", Value::String(k.clone()), canonical_value(&map[k])))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Array(items) => format!(
            "[{}]",
            items
                .iter()
                .map(canonical_value)
                .collect::<Vec<_>>()
                .join(",")
        ),
        other => other.to_string(),
    }
}

/// One item action in canonical form: everything that decides what it does
/// and where it is offered.
fn canonical_item_action(item: &ItemAction) -> String {
    let quoted = |s: &str| Value::String(s.to_owned()).to_string();
    let (mutates, confirm, params, note, inputs) = match &item.action {
        SourceAction::Endpoint {
            mutates,
            confirm,
            params,
            note,
            inputs,
            ..
        } => (
            *mutates,
            confirm.as_deref(),
            params.as_ref(),
            *note,
            inputs.as_slice(),
        ),
        _ => (false, None, None, None, &[][..]),
    };
    let note = note.map(|n| match n {
        NoteRule::Optional => "optional",
        NoteRule::Required => "required",
    });
    let sources: Vec<String> = item.sources.iter().map(|s| quoted(s)).collect();
    // What it asks for is part of what it does; left out when it asks for
    // nothing, so an item action trusted before inputs existed keeps its form.
    let inputs = if inputs.is_empty() {
        String::new()
    } else {
        format!(
            ",\"inputs\":{}",
            canonical_value(&serde_json::to_value(inputs).unwrap_or(Value::Null))
        )
    };
    format!(
        "{{\"id\":{},\"label\":{},\"mutates\":{},\"confirm\":{},\"note\":{},\"params\":{},\"view\":{},\"sources\":[{}]{}}}",
        quoted(item.id()),
        quoted(item.action.label()),
        mutates,
        confirm.map_or_else(|| "null".to_owned(), quoted),
        note.map_or_else(|| "null".to_owned(), quoted),
        params.map_or_else(|| "null".to_owned(), canonical_value),
        quoted(&item.view),
        sources.join(","),
        inputs
    )
}

fn sorted_item_actions(manifest: &Manifest) -> Vec<&ItemAction> {
    let mut items: Vec<&ItemAction> = manifest.item_actions.iter().collect();
    items.sort_by(|a, b| a.id().cmp(b.id()));
    items
}

/// The canonical JSON the trust hash is taken over:
/// `{"id":…,"linkHosts":[sorted],"roots":[sorted],"views":[sorted keys]}`
/// with exactly those keys in that order and no whitespace, plus
/// `"itemActions":[…sorted by id]` after `views` when the source offers
/// any, then `"ownerUpdates":"<path>"` when it asks to be sent owner rows
/// (each left out otherwise, so a source without them keeps the hash it was
/// trusted under). Built by hand rather than through a map so the key order
/// cannot depend on how `serde_json` was compiled.
pub fn canonical_trust_json(manifest: &Manifest) -> String {
    let mut hosts = manifest.link_hosts.clone();
    hosts.sort();
    hosts.dedup();
    let mut roots = manifest.roots.clone();
    roots.sort();
    roots.dedup();
    let views: Vec<&String> = manifest.views.keys().collect(); // BTreeMap: sorted
    let list = |items: &[String]| -> String {
        let quoted: Vec<String> = items
            .iter()
            .map(|s| Value::String(s.clone()).to_string())
            .collect();
        format!("[{}]", quoted.join(","))
    };
    let view_names: Vec<String> = views.into_iter().cloned().collect();
    let items = sorted_item_actions(manifest);
    let item_actions = if items.is_empty() {
        String::new()
    } else {
        format!(
            ",\"itemActions\":[{}]",
            items
                .into_iter()
                .map(canonical_item_action)
                .collect::<Vec<_>>()
                .join(",")
        )
    };
    let owner_updates = match &manifest.owner_updates {
        Some(path) => format!(
            ",\"ownerUpdates\":{}",
            Value::String(path.path().to_owned())
        ),
        None => String::new(),
    };
    format!(
        "{{\"id\":{},\"linkHosts\":{},\"roots\":{},\"views\":{}{}{}}}",
        Value::String(manifest.id.clone()),
        list(&hosts),
        list(&roots),
        list(&view_names),
        item_actions,
        owner_updates
    )
}

/// The trust hash: BLAKE3 hex of [`canonical_trust_json`], like the project
/// file's content hash. Trust covers exactly this hash; any change to the
/// id, the hosts, the roots, the set of views or the item actions re-arms
/// the gate.
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
        item_actions: sorted_item_actions(manifest)
            .into_iter()
            .map(|item| {
                let (mutates, confirm) = match &item.action {
                    SourceAction::Endpoint {
                        mutates, confirm, ..
                    } => (*mutates, confirm.clone()),
                    _ => (false, None),
                };
                ItemActionSummary {
                    id: item.id().to_owned(),
                    label: item.action.label().to_owned(),
                    view: item.view.clone(),
                    sources: item.sources.clone(),
                    mutates,
                    confirm,
                }
            })
            .collect(),
        owner_updates: manifest.owner_updates.is_some(),
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
        assert_eq!(
            (m.id.as_str(), m.label.as_str(), m.version.as_str()),
            ("fixture", "Fixture", "0.1.0")
        );
        assert_eq!(
            m.views.keys().cloned().collect::<Vec<_>>(),
            ["attention", "environment", "prs", "work"]
        );
        assert_eq!(m.views["work"].list.path(), "/api/work");
        assert_eq!(
            m.views["work"].item.as_ref().unwrap().path(),
            "/api/work/{key}"
        );
        assert_eq!(m.views["work"].poll, Duration::from_secs(60));
        assert_eq!(m.views["prs"].poll, Duration::from_secs(300));
        assert_eq!(
            m.views["attention"].poll, POLL_DEFAULT,
            "no pollSeconds = the default"
        );
        assert!(m.views["prs"].item.is_none());
        assert_eq!(m.workflows.as_ref().unwrap().path(), "/api/workflows");
        assert_eq!(m.actions.as_ref().unwrap().path(), "/api/actions");
        assert_eq!(m.jobs.as_ref().unwrap().path(), "/api/jobs/{id}");
        assert_eq!(m.link_hosts, ["registry.example", "dev.example.test"]);
        assert_eq!(m.roots, ["/srv/example"]);
    }

    #[test]
    fn a_relative_root_refuses_the_manifest() {
        let with = |root: &str| {
            let mut v: Value = serde_json::from_str(FIXTURE).unwrap();
            v["roots"] = serde_json::json!([root]);
            parse_manifest(&v)
        };
        for bad in ["srv/example", "./x", "~/code", "..", "/srv/../etc", "C:x"] {
            match with(bad) {
                Err(ManifestError::Invalid(why)) => assert!(why.contains("roots"), "{why}"),
                other => panic!("{bad}: {other:?}"),
            }
        }
        assert_eq!(with("/srv/example").unwrap().roots, ["/srv/example"]);
        if cfg!(windows) {
            assert!(with(r"C:\srv\example").is_ok());
        }
    }

    #[test]
    fn placeholders_are_filled_with_one_encoded_segment() {
        let jobs = fixture().jobs.unwrap();
        assert!(jobs.has_placeholder("id"));
        assert!(!jobs.has_placeholder("key"));
        assert_eq!(
            jobs.with_placeholder("id", "job-1").path(),
            "/api/jobs/job-1"
        );
        assert_eq!(
            jobs.with_placeholder("id", "a/b?c=d e#f").path(),
            "/api/jobs/a%2Fb%3Fc%3Dd%20e%23f"
        );
        assert_eq!(
            jobs.with_placeholder("id", "../other").path(),
            "/api/jobs/..%2Fother"
        );
        assert_eq!(percent_encode("A-z_0.9~"), "A-z_0.9~");
        assert_eq!(percent_encode("é"), "%C3%A9");
        // A payload-named panel path gets the same shape rules as the manifest.
        assert_eq!(
            Endpoint::named_in_payload("/api/panel/validate")
                .unwrap()
                .path(),
            "/api/panel/validate"
        );
        assert!(Endpoint::named_in_payload("http://10.0.0.5/x").is_err());
        assert!(Endpoint::named_in_payload("/api/../x").is_err());
        assert!(Endpoint::named_in_payload("api/panel").is_err());
    }

    #[test]
    fn a_wrong_or_missing_contract_is_needs_update_not_a_crash() {
        let mut v: Value = serde_json::from_str(FIXTURE).unwrap();
        v["contract"] = json!(2);
        assert_eq!(
            parse_manifest(&v),
            Err(ManifestError::NeedsUpdate { contract: Some(2) })
        );
        v.as_object_mut().unwrap().remove("contract");
        assert_eq!(
            parse_manifest(&v),
            Err(ManifestError::NeedsUpdate { contract: None })
        );
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
        let bare = parse_manifest(
            &json!({"contract": 1, "source": {"id": "x", "label": "X", "version": "1"}}),
        )
        .unwrap();
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
            assert!(
                validate_endpoint_path(bad).is_err(),
                "{bad:?} must be refused"
            );
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

    /// `host:*` is any port on one named host; every other wildcard, and a
    /// port wildcard on something that is not one party's host, refuses the
    /// manifest with the reason.
    #[test]
    fn link_hosts_take_a_port_wildcard_on_a_named_host_only() {
        let with = |host: &str| {
            parse_manifest(&json!({
                "contract": 1, "source": {"id": "x", "label": "X", "version": "1"},
                "linkHosts": [host]
            }))
        };
        for ok in [
            "localhost:*",
            "127.0.0.1:*",
            "[::1]:*",
            "192.0.2.7:*",
            "dev.example.test:*",
            "example.com:*",
            "Dev.Example.Test:*",
        ] {
            assert!(
                with(ok).is_ok(),
                "{ok} must be accepted: {:?}",
                with(ok).err()
            );
        }
        let refused = [
            ("*", "only wildcard"),
            ("*:*", "bare `*`"),
            (":*", "needs a host"),
            ("*.example.com:*", "only the port"),
            ("dev.*.example.com:*", "only the port"),
            ("*.example.com", "only wildcard"),
            ("example.com:8*", "only wildcard"),
            ("com:*", "single label"),
            ("test:*", "single label"),
            ("devbox:*", "single label"),
            ("co.uk:*", "public suffix"),
            ("github.io:*", "public suffix"),
            ("::1:*", "brackets"),
        ];
        for (bad, why) in refused {
            match with(bad) {
                Err(ManifestError::Invalid(msg)) => assert!(msg.contains(why), "{bad:?}: {msg}"),
                other => panic!("{bad:?} must be refused, got {other:?}"),
            }
        }
    }

    #[test]
    fn only_loopback_ip_literals_are_accepted_as_source_addresses() {
        assert!(parse_loopback_addr("127.0.0.1:5000").is_ok());
        assert!(
            parse_loopback_addr("127.5.6.7:1").is_ok(),
            "the whole 127/8 block is loopback"
        );
        assert!(parse_loopback_addr("[::1]:9").is_ok());
        assert!(
            parse_loopback_addr(" 127.0.0.1:5000 ").is_ok(),
            "surrounding whitespace is trimmed"
        );
        for bad in [
            "10.0.0.5:5000",
            "0.0.0.0:5000",
            "[::]:5000",
            "192.168.1.2:80",
            "localhost:5000",
            "127.0.0.1",
            "127.0.0.1:0",
            "http://127.0.0.1:5000",
            "",
        ] {
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
        assert_eq!(
            backoff_delay(poll, 4),
            Duration::from_secs(480),
            "capped at poll × 8"
        );
        assert_eq!(backoff_delay(poll, 40), Duration::from_secs(480));
        assert_eq!(
            backoff_delay(POLL_MAX, u32::MAX),
            POLL_MAX * 8,
            "no overflow at the extremes"
        );
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
        // An entry that gains a port wildcard is a different approval.
        let mut any_port = m.clone();
        any_port.link_hosts = vec!["dev.example.test:*".into(), "registry.example".into()];
        assert_ne!(trust_hash(&any_port), hash);
        assert!(canonical_trust_json(&any_port)
            .contains(r#""linkHosts":["dev.example.test:*","registry.example"]"#));
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
        assert!(summary.item_actions.is_empty());
    }

    /// The overlay fixture's manifest: a second source that adds to the
    /// first one's work items and offers actions on its rows.
    const OVERLAY_FIXTURE: &str =
        include_str!("../../../sources/fixture/data-overlay/manifest.json");

    fn with_item_actions(items: Value) -> Result<Manifest, ManifestError> {
        parse_manifest(&json!({
            "contract": 1, "source": {"id": "ws", "label": "Workspaces", "version": "1"},
            "views": {"work": {"list": "/api/work"}}, "actions": "/api/actions",
            "itemActions": items
        }))
    }

    #[test]
    fn item_actions_are_endpoint_actions_with_a_place_to_be_offered() {
        let m = parse_manifest(&serde_json::from_str(OVERLAY_FIXTURE).unwrap()).unwrap();
        assert_eq!(
            m.item_actions
                .iter()
                .map(ItemAction::id)
                .collect::<Vec<_>>(),
            ["start-work", "review-later"]
        );
        let start = &m.item_actions[0];
        assert_eq!(
            (start.view.as_str(), start.sources.as_slice()),
            ("work", &["fixture".to_owned()][..])
        );
        assert!(start.offered_on("work", "fixture"));
        assert!(
            !start.offered_on("work", "other"),
            "only the listed sources' rows"
        );
        assert!(!start.offered_on("prs", "fixture"), "only the named view");
        let later = &m.item_actions[1];
        assert_eq!(
            (later.view.as_str(), later.sources.as_slice()),
            ("prs", &[ANY_SOURCE.to_owned()][..])
        );
        assert!(later.offered_on("prs", "anything"));

        // "*" as a string, or inside a list, means every other source.
        let ok = |sources: Value| {
            with_item_actions(json!([{"kind": "endpoint", "id": "x", "label": "X", "mutates": false, "on": {"view": "work", "sources": sources}}]))
                .unwrap()
                .item_actions[0]
                .sources
                .clone()
        };
        assert_eq!(ok(json!("*")), ["*"]);
        assert_eq!(ok(json!(["tracker", "*"])), ["*"]);
        assert_eq!(ok(json!(["b", "a", "b"])), ["a", "b"]);
        assert!(ok(json!([])).is_empty(), "an empty list is offered nowhere");

        let bad = |entry: Value| match with_item_actions(json!([entry])) {
            Err(ManifestError::Invalid(why)) => why,
            other => panic!("{other:?}"),
        };
        let on = json!({"view": "work", "sources": "*"});
        assert!(bad(
            json!({"kind": "agent", "id": "a", "label": "A", "cwd": "/x", "prompt": "p", "on": on})
        )
        .contains("only `endpoint`"));
        assert!(bad(
            json!({"kind": "link", "id": "l", "label": "L", "url": "https://x", "on": on})
        )
        .contains("only `endpoint`"));
        assert!(bad(
            json!({"kind": "endpoint", "id": "m", "label": "M", "mutates": true, "on": on})
        )
        .contains("no `confirm`"));
        assert!(
            bad(json!({"kind": "endpoint", "id": "m", "label": "M", "mutates": false}))
                .contains("`on`")
        );
        assert!(bad(json!({"kind": "endpoint", "id": "m", "label": "M", "mutates": false, "on": {"view": "environment", "sources": "*"}})).contains("`on.view`"));
        assert!(bad(json!({"kind": "endpoint", "id": "m", "label": "M", "mutates": false, "on": {"view": "work"}})).contains("`on.sources`"));
        assert!(bad(json!({"kind": "endpoint", "id": "m", "label": "M", "mutates": false, "on": {"view": "work", "sources": "tracker"}})).contains("`on.sources`"));
        assert!(bad(json!({"kind": "endpoint", "id": "m", "label": "M", "mutates": false, "on": {"view": "work", "sources": [""]}})).contains("non-empty"));
        assert!(
            bad(json!({"kind": "endpoint", "label": "M", "mutates": false, "on": on}))
                .contains("`id`")
        );
        let twice = with_item_actions(json!([
            {"kind": "endpoint", "id": "m", "label": "M", "mutates": false, "on": on},
            {"kind": "endpoint", "id": "m", "label": "N", "mutates": false, "on": on}
        ]));
        assert!(matches!(twice, Err(ManifestError::Invalid(why)) if why.contains("used twice")));
        assert!(
            matches!(with_item_actions(json!({"id": "m"})), Err(ManifestError::Invalid(why)) if why.contains("must be a list"))
        );
        assert!(with_item_actions(Value::Null)
            .unwrap()
            .item_actions
            .is_empty());
    }

    #[test]
    fn owner_updates_are_a_declared_path_and_part_of_what_trust_covers() {
        let base = json!({"contract": 1, "source": {"id": "ws", "label": "Workspaces", "version": "1"}, "views": {"work": {"list": "/api/work"}}});
        let plain = parse_manifest(&base).unwrap();
        assert!(plain.owner_updates.is_none());
        assert!(!trust_summary(&plain).owner_updates);
        let mut v = base.clone();
        v["ownerUpdates"] = json!("/api/work/owner");
        let asking = parse_manifest(&v).unwrap();
        assert_eq!(
            asking.owner_updates.as_ref().unwrap().path(),
            "/api/work/owner"
        );
        assert!(trust_summary(&asking).owner_updates);
        assert!(
            canonical_trust_json(&asking)
                .ends_with(r#","views":["work"],"ownerUpdates":"/api/work/owner"}"#),
            "{}",
            canonical_trust_json(&asking)
        );
        assert!(
            !canonical_trust_json(&plain).contains("ownerUpdates"),
            "a source without it keeps its old canonical form"
        );
        assert_ne!(trust_hash(&asking), trust_hash(&plain));
        let mut moved = v.clone();
        moved["ownerUpdates"] = json!("/api/owner");
        assert_ne!(
            trust_hash(&parse_manifest(&moved).unwrap()),
            trust_hash(&asking)
        );
        for bad in [
            json!("http://10.0.0.5/owner"),
            json!("owner"),
            json!("/a/../b"),
            json!(7),
        ] {
            let mut v = base.clone();
            v["ownerUpdates"] = bad.clone();
            assert!(
                matches!(parse_manifest(&v), Err(ManifestError::Invalid(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn item_actions_are_part_of_what_trust_covers() {
        let m = parse_manifest(&serde_json::from_str(OVERLAY_FIXTURE).unwrap()).unwrap();
        let canon = canonical_trust_json(&m);
        assert!(
            canon.ends_with(r#","views":["work"],"itemActions":[{"id":"review-later","label":"Review later","mutates":false,"confirm":null,"note":null,"params":null,"view":"prs","sources":["*"]},{"id":"start-work","label":"Start work","mutates":true,"confirm":"Create a workspace for {key} — {title}","note":"optional","params":{"from":"tracker","slot":0},"view":"work","sources":["fixture"]}],"ownerUpdates":"/api/work/owner"}"#),
            "{canon}"
        );
        let hash = trust_hash(&m);
        // Reordering the entries or their params' keys is not a change…
        let mut reordered = m.clone();
        reordered.item_actions.reverse();
        assert_eq!(trust_hash(&reordered), hash);
        let mut v: Value = serde_json::from_str(OVERLAY_FIXTURE).unwrap();
        v["itemActions"][0]["params"] = json!({"slot": 0, "from": "tracker"});
        assert_eq!(trust_hash(&parse_manifest(&v).unwrap()), hash);
        // …but every part of what an item action does or where it shows is.
        for change in [
            json!({"label": "Begin"}),
            json!({"confirm": "Do it to {key}?"}),
            json!({"mutates": false, "confirm": null}),
            json!({"note": "required"}),
            json!({"params": {"from": "tracker", "slot": 1}}),
            json!({"on": {"view": "work", "sources": "*"}}),
            json!({"on": {"view": "prs", "sources": ["fixture"]}}),
            json!({"id": "begin"}),
        ] {
            let mut v: Value = serde_json::from_str(OVERLAY_FIXTURE).unwrap();
            for (k, val) in change.as_object().unwrap() {
                v["itemActions"][0][k] = val.clone();
            }
            assert_ne!(trust_hash(&parse_manifest(&v).unwrap()), hash, "{change}");
        }
        let mut none = m.clone();
        none.item_actions.clear();
        assert_ne!(trust_hash(&none), hash, "dropping them is a change too");
        assert!(
            !canonical_trust_json(&none).contains("itemActions"),
            "a source without item actions keeps its old canonical form"
        );
        // Inputs are part of what an item action does.
        let mut v: Value = serde_json::from_str(OVERLAY_FIXTURE).unwrap();
        v["itemActions"][0]["inputs"] =
            json!([{"name": "slot", "label": "Slot", "kind": "number", "max": 3}]);
        let asking = parse_manifest(&v).unwrap();
        assert_ne!(trust_hash(&asking), hash);
        assert!(canonical_trust_json(&asking).contains(r#""inputs":[{"default":null,"kind":"number","label":"Slot","max":3,"name":"slot","required":false}]"#), "{}", canonical_trust_json(&asking));
        let summary = trust_summary(&m);
        assert_eq!(
            summary
                .item_actions
                .iter()
                .map(|a| a.id.as_str())
                .collect::<Vec<_>>(),
            ["review-later", "start-work"]
        );
        let start = &summary.item_actions[1];
        assert_eq!(
            (start.label.as_str(), start.view.as_str(), start.mutates),
            ("Start work", "work", true)
        );
        assert_eq!(
            start.confirm.as_deref(),
            Some("Create a workspace for {key} — {title}")
        );
        assert_eq!(start.sources, ["fixture"]);
    }
}
