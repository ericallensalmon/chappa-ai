//! Proving the control surface is the real app BEFORE the bearer token is
//! sent to it.
//!
//! ## Why
//! The app listens on a fixed loopback port. Any local user can bind that
//! port first (the app then only logs its bind failure), and a client that
//! simply connects and sends `Authorization: Bearer <token>` would hand the
//! token to whoever holds the port, then feed that program's answers to an
//! agent as tool results.
//!
//! ## Scheme
//! 1. On a successful bind the app writes `<app-config>/control_endpoint`
//!    (owner-only, beside the token): `{addr, nonce, pid}` — the address it
//!    actually bound and a fresh random nonce for this run.
//! 2. The client reads that file, connects to `addr`, and on that connection
//!    asks `GET /identity?challenge=<32 random bytes, hex>` WITHOUT any
//!    credentials.
//! 3. The app answers `{addr, nonce, proof}` where
//!    `proof = BLAKE3-keyed(K, challenge \0 addr \0 nonce)` and
//!    `K = BLAKE3-derive_key(CONTEXT, token)`.
//! 4. The client recomputes the proof from its own copy of the token, and
//!    checks `addr`/`nonce` equal the endpoint file's. Only then does it send
//!    the real request — with the token — on the SAME connection.
//!
//! Why this does not leak the token: nothing derived from the token travels
//! toward the server before the server has proved it already knows the token.
//! A program without the token cannot compute a proof for a fresh,
//! unpredictable challenge, and it cannot replay one it collected earlier
//! (each challenge is new). The proof covers the address the server is bound
//! to, and the client checks it against the address it connected to, so a
//! program on the port cannot relay the challenge to a real instance listening
//! somewhere else and pass that instance's answer off as its own. Keeping the
//! real request on the verified connection closes the gap a second connection
//! would open (the app exiting and the port being taken in between).
//!
//! The server, for its part, answers this route for anyone: the proof is
//! keyed with a domain-separated key derived from the token, not the token
//! itself, so asking it to sign chosen challenges reveals nothing usable as a
//! bearer credential.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use crate::ToolError;

/// The unauthenticated route that answers the challenge.
pub const IDENTITY_PATH: &str = "/identity";
/// The endpoint file the app writes beside the token on each successful bind.
pub const ENDPOINT_FILE: &str = "control_endpoint";
/// BLAKE3 key-derivation context: binds the derived key to this one use.
const CONTEXT: &str = "chappa-ai control-surface identity proof v1";
/// Challenge length in bytes (sent as hex).
pub const CHALLENGE_BYTES: usize = 32;
/// The identity reply is tiny; anything bigger is not the app.
const MAX_IDENTITY_REPLY: usize = 16 * 1024;
/// A program on the port that stalls instead of answering is given this
/// long, not the full tool-call read timeout.
const IDENTITY_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// The proof the app answers for `challenge` (hex), given the address it is
/// bound to and its per-run nonce. Shared by the app and the client so the
/// two can never disagree on the construction.
pub fn identity_proof(token: &str, challenge: &str, addr: &str, nonce: &str) -> String {
    let key = blake3::derive_key(CONTEXT, token.as_bytes());
    let mut hasher = blake3::Hasher::new_keyed(&key);
    hasher.update(challenge.as_bytes());
    hasher.update(&[0]);
    hasher.update(addr.as_bytes());
    hasher.update(&[0]);
    hasher.update(nonce.as_bytes());
    hasher.finalize().to_hex().to_string()
}

/// Whether `s` is a well-formed challenge (exactly [`CHALLENGE_BYTES`] as
/// lowercase hex). The app refuses anything else.
pub fn is_valid_challenge(s: &str) -> bool {
    s.len() == CHALLENGE_BYTES * 2 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// What the endpoint file says about the running app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub addr: String,
    pub nonce: String,
    pub pid: Option<u64>,
}

impl Endpoint {
    /// The file's JSON form (what the app writes).
    pub fn to_json(&self) -> String {
        serde_json::json!({"addr": self.addr, "nonce": self.nonce, "pid": self.pid}).to_string()
    }
}

/// Read and validate the endpoint file. A missing file means no app has
/// bound its control surface (or an app too old to write one is running);
/// the error says both, and the token is never sent anywhere.
pub fn read_endpoint_file(path: &Path) -> Result<Endpoint, ToolError> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        ToolError::Unreachable(format!(
            "no endpoint file at {} ({e}); the app writes it each time its control surface starts \
             (an app older than this server never writes it: update the app)",
            path.display()
        ))
    })?;
    let bad = |why: &str| ToolError::Untrusted(format!("endpoint file {} is unusable: {why}", path.display()));
    let value: Value = serde_json::from_str(&text).map_err(|_| bad("not JSON"))?;
    let addr = value["addr"].as_str().ok_or_else(|| bad("no addr"))?.to_owned();
    let nonce = value["nonce"].as_str().ok_or_else(|| bad("no nonce"))?.to_owned();
    let parsed: SocketAddr = addr.parse().map_err(|_| bad("addr is not ip:port"))?;
    // The app only ever binds loopback; anything else in the file was not
    // written by it.
    if !parsed.ip().is_loopback() {
        return Err(bad("addr is not a loopback address"));
    }
    if nonce.is_empty() {
        return Err(bad("empty nonce"));
    }
    Ok(Endpoint { addr, nonce, pid: value["pid"].as_u64() })
}

/// A fresh challenge from the OS RNG. An RNG failure is an error, never a
/// predictable fallback (a predictable challenge would let a program collect
/// answers from the real app ahead of time and replay them later).
pub fn new_challenge() -> Result<String, ToolError> {
    let mut bytes = [0u8; CHALLENGE_BYTES];
    getrandom::fill(&mut bytes).map_err(|e| ToolError::Untrusted(format!("no randomness for the challenge: {e}")))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Run the challenge on an open connection. `expected` = the endpoint file's
/// record (the address and run the answer must describe); `None` = an
/// explicitly configured URL, where only the proof itself is checked (a
/// proxy in front of the app reports the app's own address, not the URL's).
///
/// On success the connection is ready for the authenticated request and the
/// verified `(addr, nonce)` is returned. Nothing token-bearing has been
/// written to the socket either way.
pub fn verify_connection(
    stream: &mut TcpStream,
    host_header: &str,
    prefix: &str,
    token: &str,
    expected: Option<&Endpoint>,
) -> Result<(String, String), ToolError> {
    let where_ = stream
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| host_header.to_owned());
    let challenge = new_challenge()?;
    let _ = stream.set_read_timeout(Some(IDENTITY_READ_TIMEOUT));
    let req = format!(
        "GET {prefix}{IDENTITY_PATH}?challenge={challenge} HTTP/1.1\r\nHost: {host_header}\r\nContent-Length: 0\r\n\r\n"
    );
    stream
        .write_all(req.as_bytes())
        .map_err(|e| ToolError::Unreachable(format!("{where_}: {e}")))?;
    let (status, body) = read_one_reply(stream).map_err(|why| {
        ToolError::Untrusted(format!("the program at {where_} did not answer the identity check ({why})"))
    })?;
    if status != 200 {
        return Err(ToolError::Untrusted(format!(
            "the program at {where_} answered {status} to the identity check: either something other than \
             chappa-ai holds the port, or the app is older than this server (update the app)"
        )));
    }
    let value: Value = serde_json::from_slice(&body)
        .map_err(|_| ToolError::Untrusted(format!("the program at {where_} sent a malformed identity answer")))?;
    let field = |k: &str| value[k].as_str().unwrap_or("").to_owned();
    let (addr, nonce, proof) = (field("addr"), field("nonce"), field("proof"));
    let want = identity_proof(token, &challenge, &addr, &nonce);
    if !constant_time_eq(want.as_bytes(), proof.as_bytes()) {
        return Err(ToolError::Untrusted(format!(
            "the program at {where_} could not prove it holds the control token: another program may be \
             holding the port, or the token file belongs to a different app config"
        )));
    }
    if let Some(ep) = expected {
        if addr != ep.addr || nonce != ep.nonce {
            return Err(ToolError::Untrusted(format!(
                "the app at {where_} proved itself for {addr} (run {nonce}), not the endpoint file's \
                 {} (run {}): the endpoint file is stale or the answer was relayed",
                ep.addr, ep.nonce
            )));
        }
    }
    Ok((addr, nonce))
}

/// Connect to the endpoint file's address and verify it — the app's own
/// check, on a failed bind, of whether an existing endpoint file belongs to a
/// live instance (keep it) or a dead run (remove it). Never sends the token.
pub fn probe_endpoint(endpoint_file: &Path, token: &str, connect_timeout: Duration) -> Result<Endpoint, ToolError> {
    let ep = read_endpoint_file(endpoint_file)?;
    let addr: SocketAddr = ep
        .addr
        .parse()
        .map_err(|_| ToolError::Untrusted(format!("bad endpoint addr {}", ep.addr)))?;
    let mut stream = TcpStream::connect_timeout(&addr, connect_timeout)
        .map_err(|e| ToolError::Unreachable(format!("{addr}: {e}")))?;
    let _ = stream.set_write_timeout(Some(connect_timeout));
    verify_connection(&mut stream, &ep.addr, "", token, Some(&ep))?;
    Ok(ep)
}

/// Read exactly one `Content-Length`-framed reply off a keep-alive
/// connection, bounded by [`MAX_IDENTITY_REPLY`]. Chunked framing is refused:
/// the app answers this route with a small fixed-length body.
fn read_one_reply(stream: &mut TcpStream) -> Result<(u16, Vec<u8>), String> {
    let mut raw = Vec::new();
    let mut buf = [0u8; 2048];
    let head_end = loop {
        if let Some(pos) = crate::find(&raw, b"\r\n\r\n") {
            break pos;
        }
        if raw.len() > MAX_IDENTITY_REPLY {
            return Err("reply headers too large".into());
        }
        let n = stream.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("connection closed".into());
        }
        raw.extend_from_slice(&buf[..n]);
    };
    let head = std::str::from_utf8(&raw[..head_end]).map_err(|_| "non-UTF-8 headers".to_owned())?;
    let mut lines = head.split("\r\n");
    let status: u16 = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or("malformed status line")?;
    let mut length = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else { continue };
        if name.trim().eq_ignore_ascii_case("content-length") {
            length = value.trim().parse::<usize>().ok();
        } else if name.trim().eq_ignore_ascii_case("transfer-encoding") {
            return Err("unexpected transfer-encoding".into());
        }
    }
    let length = length.ok_or("no content-length")?;
    if length > MAX_IDENTITY_REPLY {
        return Err("reply body too large".into());
    }
    let body_start = head_end + 4;
    while raw.len() < body_start + length {
        let n = stream.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("connection closed mid-reply".into());
        }
        raw.extend_from_slice(&buf[..n]);
    }
    // The server writes nothing past this reply until the next request, so
    // there is never a leftover to carry into the authenticated exchange.
    if raw.len() != body_start + length {
        return Err("unexpected bytes after the reply".into());
    }
    Ok((status, raw[body_start..].to_vec()))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_proof_depends_on_every_input() {
        let base = identity_proof("tok", "c", "127.0.0.1:1", "n");
        assert_eq!(base.len(), 64);
        assert_eq!(base, identity_proof("tok", "c", "127.0.0.1:1", "n"));
        assert_ne!(base, identity_proof("tok2", "c", "127.0.0.1:1", "n"));
        assert_ne!(base, identity_proof("tok", "d", "127.0.0.1:1", "n"));
        assert_ne!(base, identity_proof("tok", "c", "127.0.0.1:2", "n"));
        assert_ne!(base, identity_proof("tok", "c", "127.0.0.1:1", "m"));
        // The separators keep field boundaries unambiguous.
        assert_ne!(identity_proof("tok", "ab", "c", "n"), identity_proof("tok", "a", "bc", "n"));
    }

    #[test]
    fn challenges_are_fresh_and_well_formed() {
        let a = new_challenge().unwrap();
        let b = new_challenge().unwrap();
        assert!(is_valid_challenge(&a), "{a}");
        assert_ne!(a, b);
        assert!(!is_valid_challenge("abc"));
        assert!(!is_valid_challenge(&a.to_uppercase()));
    }

    #[test]
    fn endpoint_files_must_name_a_loopback_address() {
        let dir = std::env::temp_dir().join(format!("chappa-ai-mcp-ep-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(ENDPOINT_FILE);
        std::fs::write(&path, r#"{"addr":"10.0.0.5:8324","nonce":"n"}"#).unwrap();
        assert!(matches!(read_endpoint_file(&path), Err(ToolError::Untrusted(_))));
        std::fs::write(&path, "garbage").unwrap();
        assert!(matches!(read_endpoint_file(&path), Err(ToolError::Untrusted(_))));
        std::fs::write(&path, r#"{"addr":"127.0.0.1:8324","nonce":"n","pid":7}"#).unwrap();
        let ep = read_endpoint_file(&path).unwrap();
        assert_eq!(ep, Endpoint { addr: "127.0.0.1:8324".into(), nonce: "n".into(), pid: Some(7) });
        std::fs::remove_file(&path).unwrap();
        assert!(matches!(read_endpoint_file(&path), Err(ToolError::Unreachable(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
