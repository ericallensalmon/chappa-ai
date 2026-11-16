//! The HTTP side of one source: a keep-alive connection that is verified
//! with the identity challenge before the token goes out on it, and then
//! fetches declared endpoints with `Authorization: Bearer <token>`.
//!
//! The token lives here and in the registry's launch record; it is never
//! part of a DTO. Every fetch takes an [`Endpoint`], which only a parsed
//! manifest (or [`Endpoint::manifest`]) can produce, so this client cannot
//! be asked for a path the source did not declare.

use std::net::SocketAddr;
use std::time::Duration;

use chappa_ai_mcp::http::{Connection, HttpError};
use serde_json::Value;

use super::identity::{self, IDENTITY_PATH};
use super::manifest::{parse_loopback_addr, Endpoint};

/// Connect and per-read timeouts. A source is a local process; anything
/// slower than this is stuck, and the poller must not stall on it.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
pub const READ_TIMEOUT: Duration = Duration::from_secs(20);
/// The identity reply is tiny; anything bigger is not a source.
const MAX_IDENTITY_REPLY: usize = 16 * 1024;
/// The most a view payload may be. Generous (a large PR list with reasons
/// and checks fits in a fraction of this); a cap exists so a broken source
/// cannot grow the app's memory without bound.
pub const MAX_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceError {
    /// Nothing listening, connection refused or timed out, or the
    /// connection broke mid-reply.
    Unreachable(String),
    /// The program at the address did not prove it holds the token; the
    /// token was NOT sent.
    Untrusted(String),
    /// The source answered with an error status. `kind`/`message` are the
    /// contract's error body when it sent one.
    Http {
        status: u16,
        kind: String,
        message: String,
    },
    /// A 2xx body that is not JSON.
    Malformed(String),
}

impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SourceError::Unreachable(why) => write!(f, "unreachable: {why}"),
            SourceError::Untrusted(why) => write!(f, "identity check failed: {why}"),
            SourceError::Http {
                status,
                kind,
                message,
            } if kind.is_empty() => {
                write!(f, "the source answered {status}: {message}")
            }
            SourceError::Http {
                status,
                kind,
                message,
            } => write!(f, "the source answered {status} ({kind}): {message}"),
            SourceError::Malformed(why) => write!(f, "malformed reply: {why}"),
        }
    }
}

impl From<HttpError> for SourceError {
    fn from(e: HttpError) -> Self {
        match e {
            HttpError::Malformed(why) => SourceError::Malformed(why),
            HttpError::TooLarge { limit } => {
                SourceError::Malformed(format!("reply larger than the {limit}-byte cap"))
            }
            other => SourceError::Unreachable(other.to_string()),
        }
    }
}

pub struct SourceClient {
    addr: SocketAddr,
    /// The literal address text the source was given (the proof covers it).
    addr_text: String,
    token_hex: String,
    token: Vec<u8>,
    conn: Option<Connection>,
}

impl SourceClient {
    /// A client for the source at `addr_text` (a loopback `ip:port`) holding
    /// `token_hex`. Refuses a non-loopback address and a malformed token
    /// before anything is connected.
    pub fn new(addr_text: &str, token_hex: &str) -> Result<Self, String> {
        let addr = parse_loopback_addr(addr_text)?;
        let token = identity::decode_token(token_hex)
            .ok_or_else(|| "the token is not 32 bytes of lowercase hex".to_owned())?;
        Ok(Self {
            addr,
            addr_text: addr_text.trim().to_owned(),
            token_hex: token_hex.to_owned(),
            token,
            conn: None,
        })
    }

    pub fn addr_text(&self) -> &str {
        &self.addr_text
    }

    /// Run the identity check on a fresh connection and keep the connection
    /// for the requests that follow. Readiness of a launched source = this
    /// succeeding.
    pub fn verify(&mut self) -> Result<(), SourceError> {
        self.conn = None;
        let mut conn =
            Connection::connect(self.addr, &self.addr_text, CONNECT_TIMEOUT, READ_TIMEOUT)
                .map_err(|e| SourceError::Unreachable(e.to_string()))?;
        self.handshake(&mut conn)?;
        self.conn = Some(conn);
        Ok(())
    }

    /// The challenge on an open connection. Nothing token-bearing is written
    /// before the proof verifies; on any failure the connection is dropped
    /// by the caller and never carries the token.
    fn handshake(&self, conn: &mut Connection) -> Result<(), SourceError> {
        let challenge = identity::new_challenge().map_err(SourceError::Untrusted)?;
        let peer = conn.peer();
        // A program on the port that stalls instead of answering gets a short
        // leash, not the full read timeout.
        let _ = conn.set_read_timeout(Some(Duration::from_secs(5)));
        let path = format!("{IDENTITY_PATH}?challenge={challenge}");
        let reply = conn
            .request("GET", &path, &[], None, MAX_IDENTITY_REPLY)
            .map_err(|e| {
                SourceError::Untrusted(format!(
                    "the program at {peer} did not answer the identity check ({e})"
                ))
            })?;
        let _ = conn.set_read_timeout(Some(READ_TIMEOUT));
        if reply.status != 200 {
            return Err(SourceError::Untrusted(format!(
                "the program at {peer} answered {} to the identity check: something other than the source holds the port, or the source is out of date",
                reply.status
            )));
        }
        let value: Value = reply.json().ok_or_else(|| {
            SourceError::Untrusted(format!(
                "the program at {peer} sent a malformed identity answer"
            ))
        })?;
        let proof = value["proof"].as_str().unwrap_or("");
        let want = identity::source_proof(&self.token, &challenge, &self.addr_text);
        if !identity::constant_time_eq(want.as_bytes(), proof.as_bytes()) {
            return Err(SourceError::Untrusted(format!(
                "the program at {peer} could not prove it holds the source token: another program may be holding the port"
            )));
        }
        if !conn.pending().is_empty() {
            return Err(SourceError::Untrusted(format!(
                "the program at {peer} sent bytes past its identity answer"
            )));
        }
        Ok(())
    }

    /// `GET` a declared endpoint with the token. The connection is reused
    /// while the source keeps it open; a connection the source closed (or
    /// that never existed) is re-made and re-verified first, so the token
    /// only ever rides a connection whose peer just proved itself.
    pub fn get(&mut self, endpoint: &Endpoint) -> Result<Value, SourceError> {
        let reply = self.send("GET", endpoint, &[], None)?;
        if !reply.is_success() {
            return Err(http_error(&reply));
        }
        reply.json().ok_or_else(|| {
            SourceError::Malformed(format!("{} did not answer JSON", endpoint.path()))
        })
    }

    /// `POST` a JSON body to a declared endpoint (an action call, a job's
    /// exit report). Unlike [`SourceClient::get`] the status comes back with
    /// the body, because an action's outcome IS its status (200 done, 202
    /// started, 503 uncertain), and a 4xx/5xx body still carries the
    /// source's `{kind, message}`. Only a transport failure is an `Err`.
    /// A POST is never retried on a dropped connection: the source may have
    /// acted on the first one.
    pub fn post(
        &mut self,
        endpoint: &Endpoint,
        extra_headers: &[(&str, &str)],
        body: &Value,
    ) -> Result<PostReply, SourceError> {
        let text = body.to_string();
        let mut headers = vec![("Content-Type", "application/json")];
        headers.extend_from_slice(extra_headers);
        let reply = self.send_once("POST", endpoint, &headers, Some(text.as_bytes()))?;
        let body = reply.json().unwrap_or(Value::Null);
        Ok(PostReply {
            status: reply.status,
            body,
        })
    }

    /// One request with the token on a verified connection, retried once
    /// on a connection the source dropped between calls (GET only).
    fn send(
        &mut self,
        method: &str,
        endpoint: &Endpoint,
        extra: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<chappa_ai_mcp::http::Response, SourceError> {
        let reusable = self.conn.as_ref().is_some_and(Connection::is_reusable);
        if !reusable {
            self.verify()?;
        }
        let auth = format!("Bearer {}", self.token_hex);
        let mut headers = vec![
            ("Authorization", auth.as_str()),
            ("Accept", "application/json"),
        ];
        headers.extend_from_slice(extra);
        let outcome = self.conn.as_mut().expect("verified above").request(
            method,
            endpoint.path(),
            &headers,
            body,
            MAX_PAYLOAD_BYTES,
        );
        match outcome {
            Ok(reply) => Ok(reply),
            // A keep-alive connection the source dropped between polls reads
            // as closed on the first write or read: one fresh, re-verified
            // connection and one retry, then the error stands.
            Err(HttpError::Closed) | Err(HttpError::Io(_)) if reusable => {
                self.verify()?;
                Ok(self.conn.as_mut().expect("verified above").request(
                    method,
                    endpoint.path(),
                    &headers,
                    body,
                    MAX_PAYLOAD_BYTES,
                )?)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// One request, no retry: always on a connection verified for this
    /// call, so a stale keep-alive cannot swallow a POST.
    fn send_once(
        &mut self,
        method: &str,
        endpoint: &Endpoint,
        extra: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<chappa_ai_mcp::http::Response, SourceError> {
        self.verify()?;
        let auth = format!("Bearer {}", self.token_hex);
        let mut headers = vec![
            ("Authorization", auth.as_str()),
            ("Accept", "application/json"),
        ];
        headers.extend_from_slice(extra);
        Ok(self.conn.as_mut().expect("verified above").request(
            method,
            endpoint.path(),
            &headers,
            body,
            MAX_PAYLOAD_BYTES,
        )?)
    }

    /// The manifest.
    pub fn manifest(&mut self) -> Result<Value, SourceError> {
        self.get(&Endpoint::manifest())
    }
}

/// A POST's status and decoded body (`Null` when the body was not JSON).
#[derive(Debug, Clone, PartialEq)]
pub struct PostReply {
    pub status: u16,
    pub body: Value,
}

impl PostReply {
    /// The source's `{kind, message}` error body as a [`SourceError::Http`].
    pub fn as_error(&self) -> SourceError {
        let text = |k: &str| self.body[k].as_str().unwrap_or("").to_owned();
        SourceError::Http {
            status: self.status,
            kind: text("kind"),
            message: text("message"),
        }
    }
}

fn http_error(reply: &chappa_ai_mcp::http::Response) -> SourceError {
    let body = reply.json().unwrap_or(Value::Null);
    let text = |k: &str| body[k].as_str().unwrap_or("").to_owned();
    let message = if body.is_object() {
        text("message")
    } else {
        String::from_utf8_lossy(&reply.body).trim().to_owned()
    };
    SourceError::Http {
        status: reply.status,
        kind: text("kind"),
        message,
    }
}
