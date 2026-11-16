//! The `host:*` form of a source's `linkHosts` entry: "any port on this
//! host". One rule for everything that writes or reads such an entry, so a
//! source never declares one the app would refuse (a refused entry makes
//! the whole manifest unusable).
//!
//! A port wildcard only widens trust along one axis, the port, and only on
//! a host that already names one party: this machine (`localhost`,
//! `127.0.0.1`, `[::1]`), an IP literal, or a name of at least two labels
//! that is not itself a public suffix. A bare `*`, a `*` anywhere in the
//! host, and a public suffix (`com:*`, `test:*`, `co.uk:*`) are refused:
//! those would cover hosts the user cannot see in the trust dialog.
//!
//! The public-suffix check is a short built-in list ([`PUBLIC_SUFFIXES`]),
//! not the full public suffix list: every single-label name is refused
//! anyway, so the list only needs the multi-label suffixes under which
//! unrelated parties register names (country second levels, shared hosting
//! domains). A name it misses is still exact on the host, so the cost of a
//! gap is one over-broad port rule on a host the user approved by name.

use std::net::IpAddr;

/// What follows the host in an any-port entry.
pub const ANY_PORT: &str = ":*";

/// Multi-label suffixes under which unrelated parties hold names. Single
/// labels (`com`, `test`, `dev`, …) need no entry: they are refused by the
/// two-label rule.
pub const PUBLIC_SUFFIXES: &[&str] = &[
    "co.uk",
    "org.uk",
    "ac.uk",
    "gov.uk",
    "me.uk",
    "ltd.uk",
    "plc.uk",
    "com.au",
    "net.au",
    "org.au",
    "edu.au",
    "gov.au",
    "co.nz",
    "org.nz",
    "co.jp",
    "ne.jp",
    "or.jp",
    "co.kr",
    "or.kr",
    "com.br",
    "com.cn",
    "net.cn",
    "org.cn",
    "com.mx",
    "co.in",
    "co.za",
    "com.tr",
    "com.tw",
    "com.hk",
    "com.sg",
    "github.io",
    "gitlab.io",
    "pages.dev",
    "workers.dev",
    "vercel.app",
    "netlify.app",
    "herokuapp.com",
    "appspot.com",
    "blogspot.com",
    "azurewebsites.net",
    "cloudfront.net",
    "amazonaws.com",
    "web.app",
    "firebaseapp.com",
    "ngrok.io",
    "ngrok-free.app",
];

/// The host of an any-port entry (`dev.example.test:*` → `dev.example.test`,
/// `[::1]:*` → `[::1]`), lowercased; `None` when the entry is not one.
pub fn any_port_host(entry: &str) -> Option<String> {
    entry.strip_suffix(ANY_PORT).map(str::to_ascii_lowercase)
}

/// Whether `host` (the part before `:*`) may carry a port wildcard; the
/// error says why not.
pub fn check_any_port_host(host: &str) -> Result<(), String> {
    if host.is_empty() || host == "*" {
        return Err("a port wildcard needs a host; a bare `*` would cover every host".into());
    }
    if host.contains('*') {
        return Err("only the port may be `*`; a wildcard in the host name is not allowed".into());
    }
    let host = host.to_ascii_lowercase();
    if host == "localhost" {
        return Ok(());
    }
    if let Some(inner) = host.strip_prefix('[') {
        let inner = inner
            .strip_suffix(']')
            .ok_or("an IPv6 host must be closed with `]`")?;
        return match inner.parse::<IpAddr>() {
            Ok(IpAddr::V6(_)) => Ok(()),
            _ => Err(format!("`{host}` is not an IPv6 address")),
        };
    }
    if host.contains(':') {
        return Err("an IPv6 host must be written in brackets (`[::1]:*`)".into());
    }
    if host.parse::<IpAddr>().is_ok() {
        return Ok(());
    }
    let labels: Vec<&str> = host.split('.').collect();
    if labels.iter().any(|l| l.is_empty()) {
        return Err(format!("`{host}` has an empty label"));
    }
    if labels
        .iter()
        .any(|l| !l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
    {
        return Err(format!("`{host}` is not a host name"));
    }
    if labels.len() < 2 {
        return Err(format!("`{host}` is a single label (a top-level domain or a bare name); a port wildcard needs a host such as `dev.example.test`"));
    }
    if PUBLIC_SUFFIXES.contains(&host.as_str()) {
        return Err(format!(
            "`{host}` is a public suffix that unrelated sites share"
        ));
    }
    Ok(())
}

/// What the trust dialog says for an entry: "any port on <host>" for an
/// any-port entry, the entry itself otherwise.
pub fn describe(entry: &str) -> String {
    match any_port_host(entry) {
        Some(host) => format!("any port on {host}"),
        None => entry.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn any_port_hosts_are_accepted_on_a_named_party_only() {
        for ok in [
            "localhost",
            "127.0.0.1",
            "[::1]",
            "192.0.2.7",
            "[2001:db8::1]",
            "dev.example.test",
            "example.com",
            "a.b.c.example.com",
            "Dev.Example.Test",
            "api-1.example.test",
        ] {
            assert!(
                check_any_port_host(ok).is_ok(),
                "{ok} must be accepted: {:?}",
                check_any_port_host(ok)
            );
        }
        let refused = [
            ("", "needs a host"),
            ("*", "bare `*`"),
            ("*.example.com", "only the port"),
            ("dev*.example.com", "only the port"),
            ("com", "single label"),
            ("test", "single label"),
            ("devbox", "single label"),
            ("co.uk", "public suffix"),
            ("GitHub.io", "public suffix"),
            ("::1", "brackets"),
            ("[::1", "closed"),
            ("[127.0.0.1]", "not an IPv6"),
            ("example..com", "empty label"),
            ("example.com.", "empty label"),
            ("exa_mple.com", "not a host name"),
        ];
        for (bad, why) in refused {
            let err = check_any_port_host(bad).unwrap_err();
            assert!(err.contains(why), "{bad:?}: {err}");
        }
    }

    #[test]
    fn entries_are_recognised_and_described() {
        assert_eq!(
            any_port_host("Dev.Example.Test:*").as_deref(),
            Some("dev.example.test")
        );
        assert_eq!(any_port_host("[::1]:*").as_deref(), Some("[::1]"));
        assert_eq!(any_port_host("dev.example.test:8443"), None);
        assert_eq!(any_port_host("dev.example.test"), None);
        assert_eq!(describe("localhost:*"), "any port on localhost");
        assert_eq!(describe("github.com"), "github.com");
    }
}
