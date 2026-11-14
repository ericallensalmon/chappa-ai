//! Link enforcement for sources: a URL a source hands over (a `link`
//! action, a `Link` in a row, a work item's `url`) opens only when its
//! scheme is `https` and its host is one the user trusted with the
//! source's manifest. Checked here, in Rust, before the opener is called,
//! so a source cannot open `file:`, `javascript:` or a host it did not
//! declare — and the webview never decides this on its own.
//!
//! Host matching is EXACT: `github.com` does not cover `gist.github.com`
//! or `api.github.com`. A source that links to several hosts declares each
//! one; a wildcard rule would make the trust dialog's host list mean less
//! than it shows.

/// The parts of a URL the check looks at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedLink {
    /// Lowercased host, with `:port` only when the port is not 443.
    pub host: String,
}

/// Why a link is refused, as text for the user.
pub fn source_link_check(url: &str, trusted_link_hosts: &[String]) -> Result<CheckedLink, String> {
    let url = url.trim();
    if url.is_empty() {
        return Err("the link has no address".into());
    }
    if url.bytes().any(|b| b <= b' ' || b == 0x7f) {
        return Err("the link contains whitespace or control characters".into());
    }
    let (scheme, rest) = split_scheme(url).ok_or_else(|| "the link has no scheme; only https links open".to_owned())?;
    if scheme != "https" {
        return Err(format!("only https links open; this one is `{scheme}:`"));
    }
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
        Some(443) | None => host.clone(),
        Some(p) => format!("{host}:{p}"),
    };
    let trusted = trusted_link_hosts.iter().any(|h| host_key(h) == shown);
    if !trusted {
        return Err(format!(
            "`{shown}` is not one of the hosts this source may link to ({}); hosts match exactly, not by subdomain",
            if trusted_link_hosts.is_empty() { "none declared".to_owned() } else { trusted_link_hosts.join(", ") }
        ));
    }
    Ok(CheckedLink { host: shown })
}

/// A trusted entry, normalised the same way as the URL's authority.
fn host_key(entry: &str) -> String {
    match split_port(entry) {
        Ok((host, Some(443))) | Ok((host, None)) => host.to_ascii_lowercase(),
        Ok((host, Some(p))) => format!("{}:{p}", host.to_ascii_lowercase()),
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

    fn hosts(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn https_on_a_declared_host_opens_and_the_host_is_normalised() {
        let trusted = hosts(&["github.com", "Dev.Example.Test:8443"]);
        assert_eq!(source_link_check("https://github.com/owner/repo/pull/12", &trusted).unwrap().host, "github.com");
        assert_eq!(source_link_check("HTTPS://GitHub.COM:443/x?y#z", &trusted).unwrap().host, "github.com");
        assert_eq!(source_link_check("https://dev.example.test:8443/", &trusted).unwrap().host, "dev.example.test:8443");
        assert_eq!(source_link_check("https://github.com", &trusted).unwrap().host, "github.com");
    }

    #[test]
    fn other_schemes_and_undeclared_hosts_are_refused_with_the_reason() {
        let trusted = hosts(&["github.com"]);
        let refuse = |url: &str| source_link_check(url, &trusted).unwrap_err();
        assert!(refuse("http://github.com/x").contains("only https"));
        assert!(refuse("file:///etc/passwd").contains("only https"));
        assert!(refuse("javascript:alert(1)").contains("no scheme") || refuse("javascript:alert(1)").contains("only https"));
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
    }
}
