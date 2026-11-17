//! Placeholders in definition strings.
//!
//! A definition is written once and runs in many slots, so its URLs, image
//! tags and hook argv carry placeholders that are filled per instance:
//!
//! | placeholder | value |
//! |---|---|
//! | `{slot}` | the slot number |
//! | `{port:<component>}` | that component's first host port in this slot; `{port:<component>:<n>}` its n-th (0-based) |
//! | `{image:<component>}` | the image reference the component actually runs (a digest once resolved) |
//! | `{ref:<component>}` | the git ref the component is built from |
//! | `{item}` | the bound work item's key |
//! | `{channel}` | the prebuilt channel or tag the user picked |
//! | `{hostname}` | the stack's single hostname |
//! | `{host}` | how to reach the host from where the string is used: see [`HostSide`] |
//! | `{container}` | the container a hook runs against |
//! | `{snapshot}` | the generated name of a data snapshot |
//! | `{param:<name>}` | a parameter the caller supplied (state-seed arguments) |
//! | `{instance}` | the instance id, `<stack>-<slot>` |
//! | `{branch:<repo>}` | the branch the bound work item has in its worktree of that clone |
//! | `{worktree:<repo>}` | the folder of the bound work item's worktree of that clone |
//! | `{row}` | the name of one row a script component's status reported, for a verb run on that row (see [`crate::script`]) |
//! | `{rows}` | a whole argv word of a script's `up`: the picked rows' names, one word each (none when nothing was picked) |
//! | `{secret:<name>}` | a random value the runner generated for this instance and keeps under `.work/` (see [`crate::instance_secrets`]) |
//!
//! `{host}` differs per engine because containers reach the host under a
//! different name on each: `host.docker.internal` from docker containers,
//! `host.containers.internal` from podman ones, and plain `localhost` from
//! the host itself. Cross-engine traffic always goes over host ports, so
//! `{host}:{port:db}` is the one spelling that works everywhere.
//!
//! Escaping: `{{` is a literal `{` and `}}` a literal `}`. Placeholders do
//! not nest. An unknown name, a missing argument, an argument on a
//! placeholder that takes none, or an unterminated brace is an error that
//! says exactly what was wrong, so a typo fails at validation time.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::fmt;

use crate::driver::EngineKind;

/// One placeholder, parsed but not yet resolved.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Placeholder {
    Slot,
    Port {
        component: String,
        index: u16,
    },
    Image(String),
    Ref(String),
    Item,
    Channel,
    Hostname,
    Host,
    Container,
    Snapshot,
    Param(String),
    Instance,
    /// The bound item's branch in its worktree of a clone.
    Branch(String),
    /// The bound item's worktree folder of a clone.
    Worktree(String),
    /// A value generated for the instance, by name.
    Secret(String),
    /// The reported row a script verb runs on.
    Row,
    /// The picked rows of a script's `up`; expands to several argv words,
    /// so it only stands as a whole word (see [`crate::script::prepare`]).
    Rows,
}

impl Placeholder {
    /// The component this placeholder refers to, if any. Validation uses it
    /// to check that every `{port:x}` / `{image:x}` / `{ref:x}` names a
    /// component that exists.
    pub fn component(&self) -> Option<&str> {
        match self {
            Placeholder::Port { component, .. } => Some(component),
            Placeholder::Image(c) | Placeholder::Ref(c) => Some(c),
            _ => None,
        }
    }
}

impl fmt::Display for Placeholder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Placeholder::Slot => write!(f, "{{slot}}"),
            Placeholder::Port {
                component,
                index: 0,
            } => write!(f, "{{port:{component}}}"),
            Placeholder::Port { component, index } => write!(f, "{{port:{component}:{index}}}"),
            Placeholder::Image(c) => write!(f, "{{image:{c}}}"),
            Placeholder::Ref(c) => write!(f, "{{ref:{c}}}"),
            Placeholder::Item => write!(f, "{{item}}"),
            Placeholder::Channel => write!(f, "{{channel}}"),
            Placeholder::Hostname => write!(f, "{{hostname}}"),
            Placeholder::Host => write!(f, "{{host}}"),
            Placeholder::Container => write!(f, "{{container}}"),
            Placeholder::Snapshot => write!(f, "{{snapshot}}"),
            Placeholder::Param(p) => write!(f, "{{param:{p}}}"),
            Placeholder::Instance => write!(f, "{{instance}}"),
            Placeholder::Branch(r) => write!(f, "{{branch:{r}}}"),
            Placeholder::Worktree(r) => write!(f, "{{worktree:{r}}}"),
            Placeholder::Secret(n) => write!(f, "{{secret:{n}}}"),
            Placeholder::Row => write!(f, "{{row}}"),
            Placeholder::Rows => write!(f, "{{rows}}"),
        }
    }
}

/// A parsed template: literal text and placeholders in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Literal(String),
    Placeholder(Placeholder),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlaceholderError {
    #[error("unknown placeholder `{{{name}}}`")]
    Unknown { name: String },
    #[error("placeholder `{{{name}}}` needs an argument, as in `{{{name}:<name>}}`")]
    MissingArgument { name: String },
    #[error("placeholder `{{{name}}}` takes no argument")]
    UnexpectedArgument { name: String },
    #[error("`{{port:{component}:{index}}}`: the port index is not a number")]
    BadPortIndex { component: String, index: String },
    #[error("unterminated `{{` at byte {at}; write `{{{{` for a literal brace")]
    Unterminated { at: usize },
    #[error("stray `}}` at byte {at}; write `}}}}` for a literal brace")]
    StrayClose { at: usize },
    #[error("`{{secret:{name}}}`: a secret's name is letters, digits, `-` and `_`")]
    BadSecretName { name: String },
    #[error("no value for `{what}` in this context")]
    NoValue { what: String },
    #[error("`{what}` names one reported row; run this verb from that row")]
    NoRow { what: String },
    #[error("`{{rows}}` stands for several argv words, so it must be a whole word of its own")]
    RowsInsideWord,
    #[error("`{what}` needs a bound work item; activate one in this slot first")]
    NoItem { what: String },
    #[error("`{what}`: the bound work item has no worktree of `{repo}`")]
    NoWorktree { what: String, repo: String },
    #[error("`{what}` names unknown component `{component}`")]
    UnknownComponent { what: String, component: String },
    #[error("`{what}`: component `{component}` publishes {available} port(s), index {index} is out of range")]
    PortOutOfRange {
        what: String,
        component: String,
        index: u16,
        available: usize,
    },
}

/// Which side of an engine boundary a string is used on. Decides `{host}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HostSide {
    /// Inside a docker container.
    Docker,
    /// Inside a podman container.
    Podman,
    /// On the host itself (a `process` component, a probe, the agent env).
    Host,
}

impl HostSide {
    /// Where a string that runs inside a container of this engine reaches
    /// the host. Engines that are not container engines (terraform,
    /// process, script) run on the host side.
    pub fn for_engine(kind: EngineKind) -> HostSide {
        match kind {
            EngineKind::Docker => HostSide::Docker,
            EngineKind::Podman => HostSide::Podman,
            EngineKind::Terraform | EngineKind::Process | EngineKind::Script => HostSide::Host,
        }
    }

    /// The address `{host}` resolves to.
    pub fn address(self) -> &'static str {
        match self {
            HostSide::Docker => "host.docker.internal",
            HostSide::Podman => "host.containers.internal",
            HostSide::Host => "localhost",
        }
    }
}

/// Everything a resolution may need. Fields left at their default make the
/// matching placeholder a [`PlaceholderError::NoValue`], so a template is
/// never half-filled: either the caller knows the value or the string is
/// not usable yet (an `{image:x}` before `x` was built, say).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Context {
    pub slot: Option<u32>,
    pub ports: IndexMap<String, Vec<u16>>,
    pub images: IndexMap<String, String>,
    pub refs: IndexMap<String, String>,
    pub item: Option<String>,
    pub channel: Option<String>,
    pub hostname: Option<String>,
    pub host: Option<HostSide>,
    pub container: Option<String>,
    pub snapshot: Option<String>,
    pub params: IndexMap<String, String>,
    pub instance: Option<String>,
    /// The bound item's branch per clone name.
    pub branches: IndexMap<String, String>,
    /// The bound item's worktree folder per clone name.
    pub worktrees: IndexMap<String, String>,
    /// The instance's generated secrets.
    pub secrets: crate::instance_secrets::Secrets,
    /// The reported row a script verb runs on (`{row}`).
    pub row: Option<String>,
}

/// Parse `template` into segments. Pure syntax: no values are consulted.
pub fn parse(template: &str) -> Result<Vec<Segment>, PlaceholderError> {
    let mut segments = Vec::new();
    let mut literal = String::new();
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'{' if bytes.get(i + 1) == Some(&b'{') => {
                literal.push('{');
                i += 2;
            }
            b'}' if bytes.get(i + 1) == Some(&b'}') => {
                literal.push('}');
                i += 2;
            }
            b'}' => return Err(PlaceholderError::StrayClose { at: i }),
            b'{' => {
                let start = i;
                let Some(rel) = template[i + 1..].find(['{', '}']) else {
                    return Err(PlaceholderError::Unterminated { at: start });
                };
                let end = i + 1 + rel;
                if bytes[end] == b'{' {
                    return Err(PlaceholderError::Unterminated { at: start });
                }
                if !literal.is_empty() {
                    segments.push(Segment::Literal(std::mem::take(&mut literal)));
                }
                segments.push(Segment::Placeholder(parse_one(&template[i + 1..end])?));
                i = end + 1;
            }
            _ => {
                // Copy whole UTF-8 characters so multibyte text survives.
                let ch = template[i..].chars().next().expect("in bounds");
                literal.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    if !literal.is_empty() {
        segments.push(Segment::Literal(literal));
    }
    Ok(segments)
}

fn parse_one(body: &str) -> Result<Placeholder, PlaceholderError> {
    let (name, arg) = match body.split_once(':') {
        Some((n, a)) => (n.trim(), Some(a.trim())),
        None => (body.trim(), None),
    };
    let needs_arg = |p: fn(String) -> Placeholder| match arg {
        Some(a) if !a.is_empty() => Ok(p(a.to_string())),
        _ => Err(PlaceholderError::MissingArgument {
            name: name.to_string(),
        }),
    };
    let no_arg = |p: Placeholder| match arg {
        None => Ok(p),
        Some(_) => Err(PlaceholderError::UnexpectedArgument {
            name: name.to_string(),
        }),
    };
    match name {
        "slot" => no_arg(Placeholder::Slot),
        "item" => no_arg(Placeholder::Item),
        "channel" => no_arg(Placeholder::Channel),
        "hostname" => no_arg(Placeholder::Hostname),
        "host" => no_arg(Placeholder::Host),
        "container" => no_arg(Placeholder::Container),
        "snapshot" => no_arg(Placeholder::Snapshot),
        "instance" => no_arg(Placeholder::Instance),
        "row" => no_arg(Placeholder::Row),
        "rows" => no_arg(Placeholder::Rows),
        "branch" => needs_arg(Placeholder::Branch),
        "worktree" => needs_arg(Placeholder::Worktree),
        "image" => needs_arg(Placeholder::Image),
        "ref" => needs_arg(Placeholder::Ref),
        "param" => needs_arg(Placeholder::Param),
        "secret" => {
            let p = needs_arg(Placeholder::Secret)?;
            match &p {
                Placeholder::Secret(n) if !crate::instance_secrets::is_valid_name(n) => {
                    Err(PlaceholderError::BadSecretName { name: n.clone() })
                }
                _ => Ok(p),
            }
        }
        "port" => {
            let Some(a) = arg.filter(|a| !a.is_empty()) else {
                return Err(PlaceholderError::MissingArgument {
                    name: name.to_string(),
                });
            };
            let (component, index) = match a.split_once(':') {
                None => (a, 0),
                Some((c, idx)) => {
                    let index =
                        idx.trim()
                            .parse::<u16>()
                            .map_err(|_| PlaceholderError::BadPortIndex {
                                component: c.trim().to_string(),
                                index: idx.trim().to_string(),
                            })?;
                    (c.trim(), index)
                }
            };
            if component.is_empty() {
                return Err(PlaceholderError::MissingArgument {
                    name: name.to_string(),
                });
            }
            Ok(Placeholder::Port {
                component: component.to_string(),
                index,
            })
        }
        other => Err(PlaceholderError::Unknown {
            name: other.to_string(),
        }),
    }
}

/// Every placeholder in `template`, in order, duplicates included.
pub fn references(template: &str) -> Result<Vec<Placeholder>, PlaceholderError> {
    Ok(parse(template)?
        .into_iter()
        .filter_map(|s| match s {
            Segment::Placeholder(p) => Some(p),
            Segment::Literal(_) => None,
        })
        .collect())
}

/// Fill `template` from `ctx`. Fails on the first placeholder without a value.
pub fn resolve(template: &str, ctx: &Context) -> Result<String, PlaceholderError> {
    let mut out = String::with_capacity(template.len());
    for segment in parse(template)? {
        match segment {
            Segment::Literal(s) => out.push_str(&s),
            Segment::Placeholder(p) => out.push_str(&resolve_one(&p, ctx)?),
        }
    }
    Ok(out)
}

/// Fill every element of an argv template. A hook such as
/// `[docker, exec, "{container}", /scripts/seed.sh, "{param:count}"]` is
/// resolved word by word, so a value containing spaces stays one argument.
pub fn resolve_argv(argv: &[String], ctx: &Context) -> Result<Vec<String>, PlaceholderError> {
    argv.iter().map(|word| resolve(word, ctx)).collect()
}

fn resolve_one(p: &Placeholder, ctx: &Context) -> Result<String, PlaceholderError> {
    let what = p.to_string();
    let no_value = || PlaceholderError::NoValue { what: what.clone() };
    match p {
        Placeholder::Slot => ctx.slot.map(|s| s.to_string()).ok_or_else(no_value),
        Placeholder::Item => ctx
            .item
            .clone()
            .ok_or_else(|| PlaceholderError::NoItem { what: what.clone() }),
        Placeholder::Instance => ctx.instance.clone().ok_or_else(no_value),
        Placeholder::Row => ctx
            .row
            .clone()
            .ok_or_else(|| PlaceholderError::NoRow { what: what.clone() }),
        // A whole `{rows}` word is expanded before resolution; one that
        // gets here sits inside another word.
        Placeholder::Rows => Err(PlaceholderError::RowsInsideWord),
        Placeholder::Branch(repo) | Placeholder::Worktree(repo) => {
            if ctx.item.is_none() {
                return Err(PlaceholderError::NoItem { what: what.clone() });
            }
            let map = match p {
                Placeholder::Branch(_) => &ctx.branches,
                _ => &ctx.worktrees,
            };
            map.get(repo)
                .cloned()
                .ok_or_else(|| PlaceholderError::NoWorktree {
                    what: what.clone(),
                    repo: repo.clone(),
                })
        }
        Placeholder::Channel => ctx.channel.clone().ok_or_else(no_value),
        Placeholder::Hostname => ctx.hostname.clone().ok_or_else(no_value),
        Placeholder::Host => ctx
            .host
            .map(|h| h.address().to_string())
            .ok_or_else(no_value),
        Placeholder::Container => ctx.container.clone().ok_or_else(no_value),
        Placeholder::Snapshot => ctx.snapshot.clone().ok_or_else(no_value),
        Placeholder::Param(name) => ctx.params.get(name).cloned().ok_or_else(no_value),
        Placeholder::Secret(name) => ctx
            .secrets
            .get(name)
            .map(str::to_owned)
            .ok_or_else(no_value),
        Placeholder::Image(c) => {
            ctx.images
                .get(c)
                .cloned()
                .ok_or_else(|| PlaceholderError::UnknownComponent {
                    what: what.clone(),
                    component: c.clone(),
                })
        }
        Placeholder::Ref(c) => {
            ctx.refs
                .get(c)
                .cloned()
                .ok_or_else(|| PlaceholderError::UnknownComponent {
                    what: what.clone(),
                    component: c.clone(),
                })
        }
        Placeholder::Port { component, index } => {
            let ports =
                ctx.ports
                    .get(component)
                    .ok_or_else(|| PlaceholderError::UnknownComponent {
                        what: what.clone(),
                        component: component.clone(),
                    })?;
            ports
                .get(*index as usize)
                .map(|p| p.to_string())
                .ok_or_else(|| PlaceholderError::PortOutOfRange {
                    what: what.clone(),
                    component: component.clone(),
                    index: *index,
                    available: ports.len(),
                })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Context {
        let mut c = Context {
            slot: Some(2),
            item: Some("KEY-12".into()),
            channel: Some("main".into()),
            hostname: Some("dev.example.test".into()),
            host: Some(HostSide::Podman),
            container: Some("app-2-db".into()),
            snapshot: Some("20260101T000000Z".into()),
            ..Context::default()
        };
        c.ports.insert("api".into(), vec![14200, 14201]);
        c.ports.insert("db".into(), vec![14202]);
        c.images
            .insert("api".into(), "registry.example/api@sha256:abc".into());
        c.refs.insert("api".into(), "feature/x".into());
        c.params.insert("count".into(), "3".into());
        c.instance = Some("app-2".into());
        c.branches.insert("api".into(), "task/KEY-12".into());
        c.worktrees.insert(
            "api".into(),
            "/srv/example/.work/KEY-12/src/api-01234567".into(),
        );
        c.secrets = crate::instance_secrets::Secrets::from_pairs([("db", "generated-value")]);
        c
    }

    #[test]
    fn parses_literals_and_placeholders_in_order() {
        let segs = parse("http://{hostname}:{port:api}/health").unwrap();
        assert_eq!(
            segs,
            vec![
                Segment::Literal("http://".into()),
                Segment::Placeholder(Placeholder::Hostname),
                Segment::Literal(":".into()),
                Segment::Placeholder(Placeholder::Port {
                    component: "api".into(),
                    index: 0
                }),
                Segment::Literal("/health".into()),
            ]
        );
    }

    #[test]
    fn resolves_every_placeholder_kind() {
        let c = ctx();
        let table = [
            ("{slot}", "2"),
            ("{port:api}", "14200"),
            ("{port:api:1}", "14201"),
            ("{port:db}", "14202"),
            ("{image:api}", "registry.example/api@sha256:abc"),
            ("{ref:api}", "feature/x"),
            ("{item}", "KEY-12"),
            ("{channel}", "main"),
            ("{hostname}", "dev.example.test"),
            ("{host}", "host.containers.internal"),
            ("{container}", "app-2-db"),
            ("{snapshot}", "20260101T000000Z"),
            ("{param:count}", "3"),
            ("{instance}", "app-2"),
            ("{branch:api}", "task/KEY-12"),
            (
                "{worktree:api}",
                "/srv/example/.work/KEY-12/src/api-01234567",
            ),
            ("{secret:db}", "generated-value"),
        ];
        for (template, want) in table {
            assert_eq!(resolve(template, &c).unwrap(), want, "{template}");
        }
    }

    #[test]
    fn a_secret_needs_a_plain_name_and_a_generated_value() {
        assert_eq!(
            parse("{secret:../x}").unwrap_err(),
            PlaceholderError::BadSecretName {
                name: "../x".into()
            }
        );
        assert!(matches!(
            parse("{secret}").unwrap_err(),
            PlaceholderError::MissingArgument { .. }
        ));
        assert_eq!(
            resolve("{secret:other}", &ctx()).unwrap_err(),
            PlaceholderError::NoValue {
                what: "{secret:other}".into()
            }
        );
    }

    #[test]
    fn host_resolves_per_engine() {
        assert_eq!(
            HostSide::for_engine(EngineKind::Docker).address(),
            "host.docker.internal"
        );
        assert_eq!(
            HostSide::for_engine(EngineKind::Podman).address(),
            "host.containers.internal"
        );
        for k in [
            EngineKind::Terraform,
            EngineKind::Process,
            EngineKind::Script,
        ] {
            assert_eq!(HostSide::for_engine(k), HostSide::Host);
        }
        assert_eq!(HostSide::Host.address(), "localhost");
    }

    #[test]
    fn doubled_braces_are_literal() {
        assert_eq!(
            resolve("{{slot}} is {slot}", &ctx()).unwrap(),
            "{slot} is 2"
        );
        assert_eq!(resolve("a}}b{{c", &ctx()).unwrap(), "a}b{c");
        assert_eq!(parse("{{}}").unwrap(), vec![Segment::Literal("{}".into())]);
    }

    #[test]
    fn unknown_placeholder_names_itself() {
        assert_eq!(
            parse("{prot:api}").unwrap_err(),
            PlaceholderError::Unknown {
                name: "prot".into()
            }
        );
        assert_eq!(
            parse("{ports:api}").unwrap_err().to_string(),
            "unknown placeholder `{ports}`"
        );
    }

    #[test]
    fn argument_rules_are_enforced() {
        assert_eq!(
            parse("{port}").unwrap_err(),
            PlaceholderError::MissingArgument {
                name: "port".into()
            }
        );
        assert_eq!(
            parse("{port:}").unwrap_err(),
            PlaceholderError::MissingArgument {
                name: "port".into()
            }
        );
        assert_eq!(
            parse("{image}").unwrap_err(),
            PlaceholderError::MissingArgument {
                name: "image".into()
            }
        );
        assert_eq!(
            parse("{slot:1}").unwrap_err(),
            PlaceholderError::UnexpectedArgument {
                name: "slot".into()
            }
        );
        assert_eq!(
            parse("{port:api:x}").unwrap_err(),
            PlaceholderError::BadPortIndex {
                component: "api".into(),
                index: "x".into()
            }
        );
    }

    #[test]
    fn brace_errors_report_position() {
        assert_eq!(
            parse("ab{slot").unwrap_err(),
            PlaceholderError::Unterminated { at: 2 }
        );
        assert_eq!(
            parse("{a{b}").unwrap_err(),
            PlaceholderError::Unterminated { at: 0 }
        );
        assert_eq!(
            parse("x}y").unwrap_err(),
            PlaceholderError::StrayClose { at: 1 }
        );
    }

    #[test]
    fn missing_values_fail_rather_than_half_fill() {
        let empty = Context::default();
        assert_eq!(
            resolve("{slot}", &empty).unwrap_err(),
            PlaceholderError::NoValue {
                what: "{slot}".into()
            }
        );
        assert_eq!(
            resolve("{param:count}", &empty).unwrap_err(),
            PlaceholderError::NoValue {
                what: "{param:count}".into()
            }
        );
        assert_eq!(
            resolve("{port:web}", &ctx()).unwrap_err(),
            PlaceholderError::UnknownComponent {
                what: "{port:web}".into(),
                component: "web".into()
            }
        );
        assert_eq!(
            resolve("{image:db}", &ctx()).unwrap_err(),
            PlaceholderError::UnknownComponent {
                what: "{image:db}".into(),
                component: "db".into()
            }
        );
        assert_eq!(
            resolve("{port:db:1}", &ctx()).unwrap_err(),
            PlaceholderError::PortOutOfRange {
                what: "{port:db:1}".into(),
                component: "db".into(),
                index: 1,
                available: 1
            }
        );
    }

    #[test]
    fn item_placeholders_say_when_no_item_is_bound() {
        let mut c = ctx();
        assert_eq!(
            resolve("{worktree:web}", &c).unwrap_err(),
            PlaceholderError::NoWorktree {
                what: "{worktree:web}".into(),
                repo: "web".into()
            }
        );
        c.item = None;
        for t in ["{item}", "{branch:api}", "{worktree:api}"] {
            let e = resolve(t, &c).unwrap_err();
            assert_eq!(e, PlaceholderError::NoItem { what: t.into() }, "{t}");
            assert!(e.to_string().contains("needs a bound work item"), "{e}");
        }
        assert_eq!(
            parse("{branch}").unwrap_err(),
            PlaceholderError::MissingArgument {
                name: "branch".into()
            }
        );
        assert_eq!(
            parse("{instance:x}").unwrap_err(),
            PlaceholderError::UnexpectedArgument {
                name: "instance".into()
            }
        );
    }

    #[test]
    fn references_lists_in_order_with_duplicates() {
        let refs = references("{port:api}-{slot}-{port:api}").unwrap();
        assert_eq!(refs.len(), 3);
        assert_eq!(refs[0], refs[2]);
        assert_eq!(refs[0].component(), Some("api"));
        assert_eq!(refs[1].component(), None);
    }

    #[test]
    fn argv_is_resolved_word_by_word() {
        let mut c = ctx();
        c.params.insert("name".into(), "two words".into());
        let argv: Vec<String> = ["docker", "exec", "{container}", "/s.sh", "{param:name}"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            resolve_argv(&argv, &c).unwrap(),
            vec!["docker", "exec", "app-2-db", "/s.sh", "two words"]
        );
    }

    #[test]
    fn multibyte_literals_survive() {
        assert_eq!(
            resolve("héllo {slot} wörld", &ctx()).unwrap(),
            "héllo 2 wörld"
        );
    }

    #[test]
    fn display_round_trips() {
        for t in [
            "{slot}",
            "{port:api}",
            "{port:api:1}",
            "{image:api}",
            "{param:n}",
            "{host}",
            "{instance}",
            "{branch:api}",
            "{worktree:api}",
        ] {
            let p = references(t).unwrap().remove(0);
            assert_eq!(p.to_string(), t);
        }
    }
}
