//! HTTP probes: is the component answering, by its own rule?
//!
//! Three rules, fixed so every stack means the same thing by "up":
//!
//! | rule | passes on | for |
//! |---|---|---|
//! | `health` | 2xx only | real health or readiness routes |
//! | `web` | 2xx or 3xx | web apps whose root or login page redirects |
//! | `any` | any HTTP status | hosts with no health route: "it answered" |
//!
//! No HTTP response at all (connection refused, timeout, a reset) is down
//! under every rule. `health` is the strict default: a 404 on a guessed
//! route or a 301 on `/` must not read as healthy.
//!
//! The request goes through [`ProbeHttp`], so tests script responses. The
//! built-in [`StdHttp`] speaks plain HTTP/1.1 over `std::net`. It does not
//! speak TLS: an `https` probe comes back [`ProbeOutcome::Unknown`] with the
//! reason, because accepting the stack's own self-signed certificate (and
//! only that one) is the TLS work's job, and refusing it would make every
//! TLS component look down.

use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::definition::ProbeRule;

/// Default per-probe deadline (connect + first line).
pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// What a request produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum ProbeResponse {
    Status {
        status: u16,
    },
    NoResponse {
        reason: String,
    },
    /// The client cannot make this request (an `https` URL, a bad URL).
    Unsupported {
        reason: String,
    },
}

pub trait ProbeHttp {
    fn get(&self, url: &str, timeout: Duration) -> ProbeResponse;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "outcome")]
pub enum ProbeOutcome {
    Up {
        status: u16,
    },
    Down {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<u16>,
        reason: String,
    },
    Unknown {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeResult {
    pub url: String,
    pub rule: ProbeRule,
    pub outcome: ProbeOutcome,
}

impl ProbeResult {
    /// `Some(true)` up, `Some(false)` down, `None` unknown.
    pub fn up(&self) -> Option<bool> {
        match self.outcome {
            ProbeOutcome::Up { .. } => Some(true),
            ProbeOutcome::Down { .. } => Some(false),
            ProbeOutcome::Unknown { .. } => None,
        }
    }

    pub fn status(&self) -> Option<u16> {
        match self.outcome {
            ProbeOutcome::Up { status }
            | ProbeOutcome::Down {
                status: Some(status),
                ..
            } => Some(status),
            _ => None,
        }
    }
}

/// Does `status` pass `rule`?
pub fn passes(rule: ProbeRule, status: u16) -> bool {
    match rule {
        ProbeRule::Health => (200..300).contains(&status),
        ProbeRule::Web => (200..400).contains(&status),
        ProbeRule::Any => (100..600).contains(&status),
    }
}

/// Apply the rule to a response.
pub fn evaluate(rule: ProbeRule, response: &ProbeResponse) -> ProbeOutcome {
    match response {
        ProbeResponse::Status { status } => {
            if passes(rule, *status) {
                ProbeOutcome::Up { status: *status }
            } else {
                ProbeOutcome::Down {
                    status: Some(*status),
                    reason: format!("HTTP {status} fails the {} rule", rule_name(rule)),
                }
            }
        }
        ProbeResponse::NoResponse { reason } => ProbeOutcome::Down {
            status: None,
            reason: reason.clone(),
        },
        ProbeResponse::Unsupported { reason } => ProbeOutcome::Unknown {
            reason: reason.clone(),
        },
    }
}

fn rule_name(rule: ProbeRule) -> &'static str {
    match rule {
        ProbeRule::Health => "health",
        ProbeRule::Web => "web",
        ProbeRule::Any => "any",
    }
}

/// Probe `url` and judge it.
pub fn probe(http: &dyn ProbeHttp, url: &str, rule: ProbeRule, timeout: Duration) -> ProbeResult {
    let response = http.get(url, timeout);
    ProbeResult {
        url: url.to_string(),
        rule,
        outcome: evaluate(rule, &response),
    }
}

// --------------------------------------------------------------------- url

/// The parts of an `http://host[:port]/path?query` URL this client needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedUrl {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    /// Path and query, starting with `/`.
    pub target: String,
}

/// Parse an absolute URL. `[v6]` hosts are supported; userinfo is not.
pub fn parse_url(url: &str) -> Result<ParsedUrl, String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| format!("`{url}` has no scheme"))?;
    let scheme = scheme.to_ascii_lowercase();
    let default_port = match scheme.as_str() {
        "http" => 80,
        "https" => 443,
        other => return Err(format!("unsupported scheme `{other}`")),
    };
    let (authority, target) = match rest.find(['/', '?']) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    if authority.is_empty() || authority.contains('@') {
        return Err(format!("`{url}` has no usable host"));
    }
    let (host, port) = if let Some(end) = authority.strip_prefix('[') {
        let (h, after) = end
            .split_once(']')
            .ok_or_else(|| format!("`{url}`: unterminated IPv6 host"))?;
        let port = match after.strip_prefix(':') {
            Some(p) => p.parse::<u16>().map_err(|_| format!("`{url}`: bad port"))?,
            None if after.is_empty() => default_port,
            None => return Err(format!("`{url}`: bad authority")),
        };
        (h.to_string(), port)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (
                h.to_string(),
                p.parse::<u16>()
                    .map_err(|_| format!("`{url}`: bad port `{p}`"))?,
            ),
            None => (authority.to_string(), default_port),
        }
    };
    if host.is_empty() {
        return Err(format!("`{url}` has no usable host"));
    }
    let target = if target.is_empty() || target.starts_with('?') {
        format!("/{target}")
    } else {
        target.to_string()
    };
    Ok(ParsedUrl {
        scheme,
        host,
        port,
        target,
    })
}

// ----------------------------------------------------------------- std http

/// Plain HTTP/1.1 GET over `std::net`. Reads only the status line.
#[derive(Debug, Clone, Default)]
pub struct StdHttp;

impl ProbeHttp for StdHttp {
    fn get(&self, url: &str, timeout: Duration) -> ProbeResponse {
        let parsed = match parse_url(url) {
            Ok(p) => p,
            Err(reason) => return ProbeResponse::Unsupported { reason },
        };
        if parsed.scheme == "https" {
            return ProbeResponse::Unsupported {
                reason: "https probes are not judged until the stack's TLS trust is set up"
                    .to_string(),
            };
        }
        let addrs = match (parsed.host.as_str(), parsed.port).to_socket_addrs() {
            Ok(a) => a.collect::<Vec<_>>(),
            Err(e) => {
                return ProbeResponse::NoResponse {
                    reason: format!("{} does not resolve: {e}", parsed.host),
                }
            }
        };
        let mut last_err = String::from("no address");
        for addr in addrs {
            match TcpStream::connect_timeout(&addr, timeout) {
                Ok(stream) => return request(stream, &parsed, timeout),
                Err(e) => last_err = format!("connect {addr}: {e}"),
            }
        }
        ProbeResponse::NoResponse { reason: last_err }
    }
}

fn request(mut stream: TcpStream, url: &ParsedUrl, timeout: Duration) -> ProbeResponse {
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    let host_header = if url.host.contains(':') {
        format!("[{}]:{}", url.host, url.port)
    } else {
        format!("{}:{}", url.host, url.port)
    };
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: {host_header}\r\nUser-Agent: stack-probe\r\nAccept: */*\r\nConnection: close\r\n\r\n",
        url.target
    );
    if let Err(e) = stream.write_all(req.as_bytes()) {
        return ProbeResponse::NoResponse {
            reason: format!("write: {e}"),
        };
    }
    let mut buf = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.contains(&b'\n') || buf.len() > 4096 {
                    break;
                }
            }
            Err(e) => {
                if buf.is_empty() {
                    return ProbeResponse::NoResponse {
                        reason: format!("read: {e}"),
                    };
                }
                break;
            }
        }
    }
    match parse_status_line(&buf) {
        Some(status) => ProbeResponse::Status { status },
        None => ProbeResponse::NoResponse {
            reason: "not an HTTP response".to_string(),
        },
    }
}

/// `HTTP/1.1 200 OK` → 200.
pub fn parse_status_line(bytes: &[u8]) -> Option<u16> {
    let line = bytes.split(|b| *b == b'\n').next()?;
    let text = std::str::from_utf8(line).ok()?.trim();
    let mut words = text.split_whitespace();
    let version = words.next()?;
    if !version.starts_with("HTTP/") {
        return None;
    }
    let status: u16 = words.next()?.parse().ok()?;
    (100..600).contains(&status).then_some(status)
}

// ---------------------------------------------------------------- fake http

/// Scripted responses by exact URL; unknown URLs get no response.
#[derive(Debug, Default)]
pub struct FakeHttp {
    pub responses: indexmap::IndexMap<String, ProbeResponse>,
    pub calls: std::cell::RefCell<Vec<String>>,
}

impl FakeHttp {
    pub fn status(mut self, url: &str, status: u16) -> Self {
        self.responses
            .insert(url.to_string(), ProbeResponse::Status { status });
        self
    }

    pub fn down(mut self, url: &str, reason: &str) -> Self {
        self.responses.insert(
            url.to_string(),
            ProbeResponse::NoResponse {
                reason: reason.to_string(),
            },
        );
        self
    }
}

impl ProbeHttp for FakeHttp {
    fn get(&self, url: &str, _timeout: Duration) -> ProbeResponse {
        self.calls.borrow_mut().push(url.to_string());
        self.responses
            .get(url)
            .cloned()
            .unwrap_or(ProbeResponse::NoResponse {
                reason: "connection refused".to_string(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn rules_table() {
        // (rule, status, passes)
        let table = [
            (ProbeRule::Health, 200, true),
            (ProbeRule::Health, 204, true),
            (ProbeRule::Health, 299, true),
            (ProbeRule::Health, 300, false),
            (ProbeRule::Health, 301, false),
            (ProbeRule::Health, 404, false),
            (ProbeRule::Health, 500, false),
            (ProbeRule::Health, 199, false),
            (ProbeRule::Web, 200, true),
            (ProbeRule::Web, 302, true),
            (ProbeRule::Web, 399, true),
            (ProbeRule::Web, 400, false),
            (ProbeRule::Web, 401, false),
            (ProbeRule::Web, 503, false),
            (ProbeRule::Any, 200, true),
            (ProbeRule::Any, 404, true),
            (ProbeRule::Any, 500, true),
            (ProbeRule::Any, 599, true),
            (ProbeRule::Any, 100, true),
        ];
        for (rule, status, expected) in table {
            assert_eq!(passes(rule, status), expected, "{rule:?} {status}");
        }
    }

    #[test]
    fn no_response_is_down_under_every_rule_and_unsupported_is_unknown() {
        for rule in [ProbeRule::Health, ProbeRule::Web, ProbeRule::Any] {
            let out = evaluate(
                rule,
                &ProbeResponse::NoResponse {
                    reason: "refused".into(),
                },
            );
            assert_eq!(
                out,
                ProbeOutcome::Down {
                    status: None,
                    reason: "refused".into()
                }
            );
            let unknown = evaluate(
                rule,
                &ProbeResponse::Unsupported {
                    reason: "https".into(),
                },
            );
            assert_eq!(
                unknown,
                ProbeOutcome::Unknown {
                    reason: "https".into()
                }
            );
        }
        let down = evaluate(ProbeRule::Health, &ProbeResponse::Status { status: 404 });
        assert_eq!(
            down,
            ProbeOutcome::Down {
                status: Some(404),
                reason: "HTTP 404 fails the health rule".into()
            }
        );
    }

    #[test]
    fn url_parsing_handles_ports_paths_queries_and_v6() {
        let u = parse_url("http://dev.example.test:14001/health/ready?x=1").unwrap();
        assert_eq!(
            u,
            ParsedUrl {
                scheme: "http".into(),
                host: "dev.example.test".into(),
                port: 14001,
                target: "/health/ready?x=1".into()
            }
        );
        assert_eq!(parse_url("HTTP://h").unwrap().target, "/");
        assert_eq!(parse_url("http://h").unwrap().port, 80);
        assert_eq!(parse_url("https://h").unwrap().port, 443);
        assert_eq!(parse_url("http://h?q").unwrap().target, "/?q");
        let v6 = parse_url("http://[::1]:8080/x").unwrap();
        assert_eq!((v6.host.as_str(), v6.port), ("::1", 8080));
        assert_eq!(parse_url("http://[::1]/x").unwrap().port, 80);
        assert!(parse_url("dev.example.test/x").is_err());
        assert!(parse_url("ftp://h/").is_err());
        assert!(parse_url("http://h:notaport/").is_err());
        assert!(parse_url("http://user@h/").is_err());
        assert!(parse_url("http:///x").is_err());
    }

    #[test]
    fn status_line_parsing() {
        assert_eq!(parse_status_line(b"HTTP/1.1 200 OK\r\n"), Some(200));
        assert_eq!(
            parse_status_line(b"HTTP/1.0 302 Found\r\nLocation: /"),
            Some(302)
        );
        assert_eq!(parse_status_line(b"HTTP/2 503\n"), Some(503));
        assert_eq!(parse_status_line(b"SSH-2.0-OpenSSH\r\n"), None);
        assert_eq!(parse_status_line(b"HTTP/1.1 999 Nope\r\n"), None);
        assert_eq!(parse_status_line(b""), None);
    }

    fn serve_once(status_line: &'static str) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf);
                let _ = s.write_all(status_line.as_bytes());
            }
        });
        port
    }

    #[test]
    fn std_http_reads_a_real_status_line_and_reports_refused_connections() {
        let port = serve_once("HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n");
        let r = probe(
            &StdHttp,
            &format!("http://127.0.0.1:{port}/health"),
            ProbeRule::Health,
            Duration::from_secs(2),
        );
        assert_eq!(r.outcome, ProbeOutcome::Up { status: 204 });

        let port = serve_once("HTTP/1.1 302 Found\r\nLocation: /login\r\n\r\n");
        let r = probe(
            &StdHttp,
            &format!("http://127.0.0.1:{port}/"),
            ProbeRule::Health,
            Duration::from_secs(2),
        );
        assert_eq!(r.up(), Some(false));
        assert_eq!(r.status(), Some(302));

        // Nothing listens on a port we just released.
        let free = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = free.local_addr().unwrap().port();
        drop(free);
        let r = probe(
            &StdHttp,
            &format!("http://127.0.0.1:{port}/"),
            ProbeRule::Any,
            Duration::from_secs(2),
        );
        assert_eq!(r.up(), Some(false), "{:?}", r.outcome);

        let r = probe(
            &StdHttp,
            "https://dev.example.test:14443/",
            ProbeRule::Health,
            Duration::from_secs(1),
        );
        assert_eq!(r.up(), None);
    }

    #[test]
    fn fake_http_answers_by_url() {
        let http = FakeHttp::default()
            .status("http://h:1/health", 200)
            .down("http://h:2/health", "timed out");
        assert_eq!(
            probe(
                &http,
                "http://h:1/health",
                ProbeRule::Health,
                DEFAULT_PROBE_TIMEOUT
            )
            .up(),
            Some(true)
        );
        assert_eq!(
            probe(
                &http,
                "http://h:2/health",
                ProbeRule::Any,
                DEFAULT_PROBE_TIMEOUT
            )
            .up(),
            Some(false)
        );
        assert_eq!(
            probe(&http, "http://h:3/", ProbeRule::Any, DEFAULT_PROBE_TIMEOUT).up(),
            Some(false)
        );
        assert_eq!(http.calls.borrow().len(), 3);
    }
}
