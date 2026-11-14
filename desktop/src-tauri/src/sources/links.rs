//! Link enforcement for sources: a URL a source hands over (a `link`
//! action, a `Link` in a row, a work item's `url`) opens only when its
//! host is one the user trusted with the source's manifest, and its scheme
//! is `https` — or plain `http` to this machine. Checked here, in Rust,
//! before the opener is called, so a source cannot open `file:`,
//! `javascript:` or a host it did not declare — and the webview never
//! decides this on its own.
//!
//! Host matching is EXACT: `github.com` does not cover `gist.github.com`
//! or `api.github.com`. A source that links to several hosts declares each
//! one; a wildcard rule would make the trust dialog's host list mean less
//! than it shows. A port is part of the host (`dev.example.test:14001` is
//! not `dev.example.test`); the scheme's default port may be left out on
//! either side.
//!
//! **Plain http.** A local dev server (a stack the user runs, a preview
//! build) answers on `http://<name>:<port>`, and those links are useless if
//! they never open. `http` is accepted only when, on top of the host rule
//! above and no userinfo, the host is this machine: a loopback IP literal
//! (`127.0.0.0/8`, `::1`), `localhost`, or a name that resolves — at click
//! time, here, with a short deadline ([`RESOLVE_DEADLINE`]) — ONLY to
//! loopback addresses. One non-loopback address in the answer, an empty
//! answer or a failed lookup refuses the link: traffic that could leave the
//! machine stays on https.
//!
//! The caveat, accepted knowingly: the browser resolves the name again when
//! it opens the link, so a name whose records change between our lookup
//! and the browser's could reach another address. That needs the name to
//! be one the user already trusted for this source AND its resolution to
//! flip within the click — the trust step is the real gate; the lookup only
//! keeps a trusted public name from being opened over plain http.
//!
//! Listing an action ([`source_link_shape_check`]) checks everything but
//! the lookup, so drawing a view never waits on DNS; running it does the
//! full check.

use std::net::{IpAddr, ToSocketAddrs};
use std::sync::mpsc;
use std::time::Duration;

/// How long a click waits for a name's addresses before refusing the link.
pub const RESOLVE_DEADLINE: Duration = Duration::from_secs(2);

/// What the plain-http refusals say, in one place.
pub const PLAIN_HTTP_RULE: &str = "plain http is only opened for addresses on this machine";

/// The parts of a URL the check looks at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedLink {
    /// Lowercased host, with `:port` only when the port is not the
    /// scheme's default (443 for https, 80 for http).
    pub host: String,
    /// The link is plain http (to this machine).
    pub plain_http: bool,
}

/// A name lookup: every address `host` resolves to. Injected so the rule
/// is testable without DNS; the app uses [`SystemResolver`].
pub trait HostResolver {
    fn resolve(&self, host: &str, port: u16) -> Result<Vec<IpAddr>, String>;
}

/// The OS resolver, bounded by a deadline: the lookup runs on its own
/// thread and a slow one is abandoned (and refuses the link) rather than
/// holding the caller.
pub struct SystemResolver {
    pub deadline: Duration,
}

impl Default for SystemResolver {
    fn default() -> Self {
        Self { deadline: RESOLVE_DEADLINE }
    }
}

impl HostResolver for SystemResolver {
    fn resolve(&self, host: &str, port: u16) -> Result<Vec<IpAddr>, String> {
        let (tx, rx) = mpsc::channel();
        let name = host.to_owned();
        std::thread::Builder::new()
            .name("link host lookup".into())
            .spawn(move || {
                let answer = (name.as_str(), port)
                    .to_socket_addrs()
                    .map(|addrs| addrs.map(|a| a.ip()).collect::<Vec<_>>())
                    .map_err(|e| e.to_string());
                let _ = tx.send(answer);
            })
            .map_err(|e| format!("cannot look up `{host}`: {e}"))?;
        match rx.recv_timeout(self.deadline) {
            Ok(answer) => answer,
            Err(_) => Err(format!("looking up `{host}` took longer than {} s", self.deadline.as_secs())),
        }
    }
}

/// The full check, with the OS resolver for plain-http names. Blocks for
/// at most [`RESOLVE_DEADLINE`]; call it off the UI thread.
pub fn source_link_check(url: &str, trusted_link_hosts: &[String]) -> Result<CheckedLink, String> {
    source_link_check_with(url, trusted_link_hosts, &SystemResolver::default())
}

/// The full check with an explicit resolver.
pub fn source_link_check_with(url: &str, trusted_link_hosts: &[String], resolver: &dyn HostResolver) -> Result<CheckedLink, String> {
    let parsed = parse_and_match(url, trusted_link_hosts)?;
    if parsed.plain_http {
        if let Locality::NeedsLookup = locality(&parsed.host) {
            let port = parsed.port.unwrap_or(80);
            let addrs = resolver
                .resolve(&parsed.host, port)
                .map_err(|e| format!("{PLAIN_HTTP_RULE}, and `{}` could not be looked up ({e})", parsed.shown))?;
            if addrs.is_empty() {
                return Err(format!("{PLAIN_HTTP_RULE}, and `{}` resolved to no address", parsed.shown));
            }
            if let Some(away) = addrs.iter().find(|a| !is_loopback(a)) {
                return Err(format!(
                    "{PLAIN_HTTP_RULE}; `{}` resolves to {away}, which is not (use https for it)",
                    parsed.shown
                ));
            }
        }
    }
    Ok(CheckedLink { host: parsed.shown, plain_http: parsed.plain_http })
}

/// Everything but the name lookup: what listing an action uses, so a view
/// never waits on DNS. A plain-http link on a trusted NAME passes here and
/// is decided by the lookup when it is opened.
pub fn source_link_shape_check(url: &str, trusted_link_hosts: &[String]) -> Result<CheckedLink, String> {
    let parsed = parse_and_match(url, trusted_link_hosts)?;
    Ok(CheckedLink { host: parsed.shown, plain_http: parsed.plain_http })
}

struct Parsed {
    plain_http: bool,
    /// Lowercased, IPv6 literals in brackets.
    host: String,
    port: Option<u16>,
    shown: String,
}

/// Scheme, shape, userinfo, the trusted-host match and — for http — the
/// loopback rule for literals and names that need no lookup.
fn parse_and_match(url: &str, trusted_link_hosts: &[String]) -> Result<Parsed, String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("the link has no address".into());
    }
    if url.bytes().any(|b| b <= b' ' || b == 0x7f) {
        return Err("the link contains whitespace or control characters".into());
    }
    let (scheme, rest) =
        split_scheme(url).ok_or_else(|| "the link has no scheme; only https links open (and plain http to this machine)".to_owned())?;
    let plain_http = match scheme.as_str() {
        "https" => false,
        "http" => true,
        other => return Err(format!("only https links open (and plain http to this machine); this one is `{other}:`")),
    };
    let default_port = if plain_http { 80 } else { 443 };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() {
        return Err("the link names no host".into());
    }
    // Userinfo (`user@host`) is refused outright: it is the classic way to
    // make `https://github.com@evil.example/` read as a GitHub link.
    if authority.contains('@') {
        return Err("the link carries credentials before the host, which is not allowed".into());
    }
    let (host, port) = split_port(authority)?;
    let host = host.to_ascii_lowercase();
    if host.is_empty() {
        return Err("the link names no host".into());
    }
    let shown = match port {
        Some(p) if p != default_port => format!("{host}:{p}"),
        _ => host.clone(),
    };
    let trusted = trusted_link_hosts.iter().any(|h| host_key(h, default_port) == shown);
    if !trusted {
        return Err(format!(
            "`{shown}` is not one of the hosts this source may link to ({}); hosts match exactly, not by subdomain",
            if trusted_link_hosts.is_empty() { "none declared".to_owned() } else { trusted_link_hosts.join(", ") }
        ));
    }
    if plain_http {
        if let Locality::Elsewhere = locality(&host) {
            return Err(format!("{PLAIN_HTTP_RULE} (`{shown}` is not one); other links must be https"));
        }
    }
    Ok(Parsed { plain_http, host, port, shown })
}

enum Locality {
    /// A loopback literal or `localhost`.
    Here,
    /// An IP literal that is not loopback.
    Elsewhere,
    /// A name: only a lookup can tell.
    NeedsLookup,
}

fn locality(host: &str) -> Locality {
    let bare = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
    if let Ok(ip) = bare.parse::<IpAddr>() {
        return if is_loopback(&ip) { Locality::Here } else { Locality::Elsewhere };
    }
    if host == "localhost" || host == "localhost." {
        return Locality::Here;
    }
    Locality::NeedsLookup
}

/// `127.0.0.0/8`, `::1`, and the IPv4-mapped form of a loopback v4 address.
fn is_loopback(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback()),
    }
}

/// A trusted entry, normalised the same way as the URL's authority (the
/// scheme's default port dropped).
fn host_key(entry: &str, default_port: u16) -> String {
    match split_port(entry) {
        Ok((host, Some(p))) if p != default_port => format!("{}:{p}", host.to_ascii_lowercase()),
        Ok((host, _)) => host.to_ascii_lowercase(),
        Err(_) => entry.to_ascii_lowercase(),
    }
}

/// `(lowercase scheme, the rest after `://`)`, or `None` when there is no
/// scheme-shaped prefix (RFC 3986: a letter, then letters, digits, `+`,
/// `-`, `.`).
fn split_scheme(url: &str) -> Option<(String, &str)> {
    let idx = url.find("://")?;
    let scheme = &url[..idx];
    let mut chars = scheme.chars();
    let first_ok = chars.next().is_some_and(|c| c.is_ascii_alphabetic());
    if !first_ok || !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) {
        return None;
    }
    Some((scheme.to_ascii_lowercase(), &url[idx + 3..]))
}

/// `host[:port]`, with an IPv6 literal in brackets kept whole.
fn split_port(authority: &str) -> Result<(&str, Option<u16>), String> {
    if let Some(rest) = authority.strip_prefix('[') {
        let end = rest.find(']').ok_or("the link's IPv6 host is not closed with `]`")?;
        let host = &authority[..end + 2];
        let after = &rest[end + 1..];
        return match after.strip_prefix(':') {
            None if after.is_empty() => Ok((host, None)),
            None => Err("the link's host is malformed".into()),
            Some(port) => Ok((host, Some(parse_port(port)?))),
        };
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => Ok((host, Some(parse_port(port)?))),
        _ => Ok((authority, None)),
    }
}

fn parse_port(text: &str) -> Result<u16, String> {
    text.parse::<u16>().map_err(|_| format!("the link's port `{text}` is not a number"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn hosts(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// A resolver with canned answers that records what it was asked.
    struct Fake {
        answer: Result<Vec<IpAddr>, String>,
        asked: RefCell<Vec<(String, u16)>>,
    }

    impl Fake {
        fn answering(answer: Result<Vec<IpAddr>, String>) -> Self {
            Self { answer, asked: RefCell::new(Vec::new()) }
        }
    }

    impl HostResolver for Fake {
        fn resolve(&self, host: &str, port: u16) -> Result<Vec<IpAddr>, String> {
            self.asked.borrow_mut().push((host.to_owned(), port));
            self.answer.clone()
        }
    }

    const V4_HERE: IpAddr = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
    const V4_HERE_TOO: IpAddr = IpAddr::V4(Ipv4Addr::new(127, 0, 1, 1));
    const V6_HERE: IpAddr = IpAddr::V6(Ipv6Addr::LOCALHOST);
    const AWAY: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 7));

    /// A resolver that must not be reached.
    fn never() -> Fake {
        Fake::answering(Err("the resolver was consulted".into()))
    }

    #[test]
    fn https_on_a_declared_host_opens_and_the_host_is_normalised() {
        let trusted = hosts(&["github.com", "Dev.Example.Test:8443"]);
        let r = never();
        let check = |url: &str| source_link_check_with(url, &trusted, &r).unwrap().host;
        assert_eq!(check("https://github.com/owner/repo/pull/12"), "github.com");
        assert_eq!(check("HTTPS://GitHub.COM:443/x?y#z"), "github.com");
        assert_eq!(check("https://dev.example.test:8443/"), "dev.example.test:8443");
        assert_eq!(check("https://github.com"), "github.com");
        assert!(!source_link_check_with("https://github.com", &trusted, &r).unwrap().plain_http);
        assert!(r.asked.borrow().is_empty(), "https never looks anything up");
    }

    #[test]
    fn other_schemes_and_undeclared_hosts_are_refused_with_the_reason() {
        let trusted = hosts(&["github.com"]);
        let r = never();
        let refuse = |url: &str| source_link_check_with(url, &trusted, &r).unwrap_err();
        assert!(refuse("http://github.com/x").contains(PLAIN_HTTP_RULE));
        assert!(refuse("file:///etc/passwd").contains("only https"));
        assert!(refuse("javascript:alert(1)").contains("no scheme"));
        assert!(refuse("ftp://github.com/x").contains("`ftp:`"));
        assert!(refuse("github.com/x").contains("no scheme"));
        assert!(refuse("").contains("no address"));
        assert!(refuse("https://").contains("no host"));
        assert!(refuse("https://github.com /x").contains("whitespace"));
        assert!(refuse("https://github.com@evil.example/").contains("credentials"));
        assert!(refuse("https://evil.example/github.com").contains("`evil.example` is not one of the hosts"));
        assert!(refuse("https://github.com:8443/x").contains("github.com:8443"), "a different port is a different host");
        assert!(refuse("https://github.com:abc/").contains("port"));
    }

    #[test]
    fn subdomains_do_not_inherit_trust() {
        let trusted = hosts(&["github.com"]);
        let err = source_link_check("https://gist.github.com/x", &trusted).unwrap_err();
        assert!(err.contains("not by subdomain"), "{err}");
        assert!(source_link_check("https://github.com.evil.example/", &trusted).is_err());
        assert!(source_link_check("https://api.github.com/", &hosts(&["api.github.com", "github.com"])).is_ok());
    }

    #[test]
    fn ipv6_literals_keep_their_brackets() {
        let trusted = hosts(&["[::1]:8443"]);
        assert_eq!(source_link_check("https://[::1]:8443/ui", &trusted).unwrap().host, "[::1]:8443");
        assert!(source_link_check("https://[::1/ui", &trusted).is_err());
        assert!(source_link_check("https://[::1]x/ui", &trusted).is_err());
    }

    #[test]
    fn an_empty_trust_list_opens_nothing() {
        let err = source_link_check("https://github.com/", &[]).unwrap_err();
        assert!(err.contains("none declared"), "{err}");
        let err = source_link_check_with("http://localhost:3000/", &[], &never()).unwrap_err();
        assert!(err.contains("none declared"), "{err}");
    }

    /// Loopback literals and `localhost` open over http without a lookup,
    /// ports included; the default port may be left out on either side.
    #[test]
    fn plain_http_opens_for_loopback_literals_and_localhost() {
        let trusted = hosts(&["127.0.0.1:8080", "127.0.0.2", "[::1]:3000", "localhost:5173", "localhost"]);
        let r = never();
        let check = |url: &str| source_link_check_with(url, &trusted, &r);
        let ok = check("http://127.0.0.1:8080/health").unwrap();
        assert_eq!((ok.host.as_str(), ok.plain_http), ("127.0.0.1:8080", true));
        assert_eq!(check("http://127.0.0.2/").unwrap().host, "127.0.0.2");
        assert_eq!(check("http://127.0.0.2:80/").unwrap().host, "127.0.0.2", "port 80 is the default");
        assert_eq!(check("http://[::1]:3000/ui").unwrap().host, "[::1]:3000");
        assert_eq!(check("http://LOCALHOST:5173/").unwrap().host, "localhost:5173");
        assert_eq!(check("http://localhost/").unwrap().host, "localhost");
        assert!(r.asked.borrow().is_empty(), "literals and localhost need no lookup");
        // The same host on another port is another host.
        assert!(check("http://localhost:9999/").unwrap_err().contains("not one of the hosts"));
    }

    /// A trusted non-loopback literal is refused over http with the rule,
    /// and still opens over https.
    #[test]
    fn plain_http_to_another_address_is_refused() {
        let trusted = hosts(&["192.0.2.7:8080", "[2001:db8::1]"]);
        let r = never();
        let err = source_link_check_with("http://192.0.2.7:8080/", &trusted, &r).unwrap_err();
        assert!(err.contains(PLAIN_HTTP_RULE) && err.contains("192.0.2.7:8080"), "{err}");
        assert!(source_link_check_with("http://[2001:db8::1]/", &trusted, &r).unwrap_err().contains(PLAIN_HTTP_RULE));
        assert!(source_link_check_with("https://192.0.2.7:8080/", &trusted, &r).is_ok());
        assert!(r.asked.borrow().is_empty());
    }

    /// Userinfo is refused over http too, before any lookup.
    #[test]
    fn plain_http_with_credentials_is_refused() {
        let r = Fake::answering(Ok(vec![V4_HERE]));
        let err = source_link_check_with("http://me@localhost:3000/", &hosts(&["localhost:3000"]), &r).unwrap_err();
        assert!(err.contains("credentials"), "{err}");
        assert!(r.asked.borrow().is_empty());
    }

    /// A trusted name opens over http when every address it resolves to is
    /// loopback (v4 or v6); the lookup is asked for the URL's port.
    #[test]
    fn a_name_resolving_only_to_loopback_opens_over_http() {
        let trusted = hosts(&["dev.example.test:14001"]);
        let r = Fake::answering(Ok(vec![V4_HERE, V4_HERE_TOO, V6_HERE]));
        let ok = source_link_check_with("http://dev.example.test:14001/api", &trusted, &r).unwrap();
        assert_eq!((ok.host.as_str(), ok.plain_http), ("dev.example.test:14001", true));
        assert_eq!(r.asked.borrow().as_slice(), [("dev.example.test".to_owned(), 14001)]);
    }

    /// One address off this machine in the answer refuses the link.
    #[test]
    fn a_name_resolving_to_a_mix_is_refused() {
        let trusted = hosts(&["dev.example.test:14001"]);
        let r = Fake::answering(Ok(vec![V4_HERE, AWAY]));
        let err = source_link_check_with("http://dev.example.test:14001/", &trusted, &r).unwrap_err();
        assert!(err.contains(PLAIN_HTTP_RULE) && err.contains("192.0.2.7"), "{err}");
        let only_away = Fake::answering(Ok(vec![AWAY]));
        assert!(source_link_check_with("http://dev.example.test:14001/", &trusted, &only_away).is_err());
    }

    /// A failed lookup, or an empty answer, refuses the link.
    #[test]
    fn a_name_that_does_not_resolve_is_refused() {
        let trusted = hosts(&["dev.example.test"]);
        let failed = Fake::answering(Err("no such host".into()));
        let err = source_link_check_with("http://dev.example.test/", &trusted, &failed).unwrap_err();
        assert!(err.contains(PLAIN_HTTP_RULE) && err.contains("no such host"), "{err}");
        assert_eq!(failed.asked.borrow()[0].1, 80, "the default port is asked for");
        let empty = Fake::answering(Ok(vec![]));
        assert!(source_link_check_with("http://dev.example.test/", &trusted, &empty).unwrap_err().contains("no address"));
    }

    /// An untrusted name is refused by the host rule before any lookup.
    #[test]
    fn an_untrusted_name_is_never_looked_up() {
        let r = Fake::answering(Ok(vec![V4_HERE]));
        let err = source_link_check_with("http://other.example.test:14001/", &hosts(&["dev.example.test:14001"]), &r).unwrap_err();
        assert!(err.contains("not one of the hosts"), "{err}");
        assert!(r.asked.borrow().is_empty());
    }

    /// Listing an action never looks a name up: a trusted http name passes
    /// the shape check, a non-loopback literal does not.
    #[test]
    fn the_shape_check_skips_the_lookup() {
        let trusted = hosts(&["dev.example.test:14001", "192.0.2.7"]);
        let ok = source_link_shape_check("http://dev.example.test:14001/", &trusted).unwrap();
        assert!(ok.plain_http);
        assert!(source_link_shape_check("http://192.0.2.7/", &trusted).unwrap_err().contains(PLAIN_HTTP_RULE));
        assert!(source_link_shape_check("file:///x", &trusted).is_err());
    }

    /// The system resolver answers for an IP literal (no network needed).
    #[test]
    fn the_system_resolver_answers_literals() {
        let r = SystemResolver::default();
        assert_eq!(r.resolve("127.0.0.1", 80).unwrap(), vec![V4_HERE]);
    }
}
