//! A small HTTP/1.1 client over `std::net`, shared by the stdio MCP server
//! (one request per connection against the app's control surface) and the
//! desktop app's source poller (keep-alive connections to local source
//! processes). No HTTP crate: both callers talk to loopback servers they
//! know, and the whole protocol surface they need is a request line, a few
//! headers and the two body framings (`Content-Length` and
//! `Transfer-Encoding: chunked`).
//!
//! Framing rules, in order: a `1xx`, `204` or `304` reply has no body; a
//! chunked reply is decoded chunk by chunk (extensions ignored, trailers
//! read and dropped); a `Content-Length` reply is read exactly; anything
//! else is read to the end of the stream and ends the connection. The body
//! cap is enforced while reading, so a misbehaving server cannot make a
//! client allocate without bound. Bytes past one reply stay in the
//! connection's buffer for the next reply, which is what keep-alive needs.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use serde_json::Value;

/// Headers larger than this are not an answer from anything this client
/// talks to.
const MAX_HEADER_BYTES: usize = 64 * 1024;

/// One parsed reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    /// Header names lowercased, values trimmed, in wire order.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Whether the connection may carry another request after this reply
    /// (HTTP/1.1 keep-alive unless the server said `Connection: close`, or
    /// the body had to be read to the end of the stream).
    pub keep_alive: bool,
}

impl Response {
    /// The first header named `name` (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| v.as_str())
    }

    /// The body as JSON, or `None` when it is not JSON.
    pub fn json(&self) -> Option<Value> {
        serde_json::from_slice(&self.body).ok()
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// Why a request produced no reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpError {
    /// Connect, write or read failed (includes a timeout).
    Io(String),
    /// The peer closed before a whole reply arrived.
    Closed,
    /// The bytes were not an HTTP/1.1 reply this client understands.
    Malformed(String),
    /// Headers or body exceeded the caller's cap; `limit` is the cap hit.
    TooLarge { limit: usize },
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HttpError::Io(e) => write!(f, "{e}"),
            HttpError::Closed => f.write_str("connection closed before the reply was complete"),
            HttpError::Malformed(why) => write!(f, "malformed HTTP reply: {why}"),
            HttpError::TooLarge { limit } => write!(f, "reply larger than the {limit}-byte cap"),
        }
    }
}

impl From<std::io::Error> for HttpError {
    fn from(e: std::io::Error) -> Self {
        HttpError::Io(e.to_string())
    }
}

/// One TCP connection that can carry several requests in turn. Owns the
/// read-ahead buffer, so bytes of a later reply that arrive with an earlier
/// one are never lost.
pub struct Connection {
    stream: TcpStream,
    host: String,
    buffered: Vec<u8>,
    reusable: bool,
}

impl Connection {
    /// Connect to `addr`, sending `host` in the `Host` header of every
    /// request. Both timeouts apply to every request on the connection.
    pub fn connect(
        addr: SocketAddr,
        host: &str,
        connect_timeout: Duration,
        read_timeout: Duration,
    ) -> Result<Self, HttpError> {
        let stream = TcpStream::connect_timeout(&addr, connect_timeout)
            .map_err(|e| HttpError::Io(format!("{addr}: {e}")))?;
        stream.set_write_timeout(Some(connect_timeout))?;
        stream.set_read_timeout(Some(read_timeout))?;
        Ok(Self::from_stream(stream, host))
    }

    /// Wrap an already connected stream (its timeouts are kept).
    pub fn from_stream(stream: TcpStream, host: &str) -> Self {
        Self {
            stream,
            host: host.to_owned(),
            buffered: Vec::new(),
            reusable: true,
        }
    }

    /// Whether another request may go out on this connection.
    pub fn is_reusable(&self) -> bool {
        self.reusable
    }

    /// Bytes already read past the last reply (normally none: the servers
    /// this talks to never send unsolicited data).
    pub fn pending(&self) -> &[u8] {
        &self.buffered
    }

    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> Result<(), HttpError> {
        self.stream.set_read_timeout(timeout)?;
        Ok(())
    }

    pub fn read_timeout(&self) -> Option<Duration> {
        self.stream.read_timeout().ok().flatten()
    }

    /// Where the connection goes, as text (falls back to the host header).
    pub fn peer(&self) -> String {
        self.stream
            .peer_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|_| self.host.clone())
    }

    /// One request and its reply. `headers` go out verbatim after `Host`;
    /// `Content-Length` is always sent (0 for no body). A reply whose body
    /// exceeds `max_body` is an error and ends the connection, since the
    /// unread rest would otherwise be taken for the next reply.
    pub fn request(
        &mut self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
        max_body: usize,
    ) -> Result<Response, HttpError> {
        if !self.reusable {
            return Err(HttpError::Closed);
        }
        let body = body.unwrap_or(&[]);
        let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {}\r\n", self.host);
        for (name, value) in headers {
            req.push_str(name);
            req.push_str(": ");
            req.push_str(value);
            req.push_str("\r\n");
        }
        req.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
        let mut bytes = req.into_bytes();
        bytes.extend_from_slice(body);
        if let Err(e) = self.stream.write_all(&bytes) {
            self.reusable = false;
            return Err(e.into());
        }
        match read_response(&mut self.stream, &mut self.buffered, max_body) {
            Ok(response) => {
                if !response.keep_alive {
                    self.reusable = false;
                }
                Ok(response)
            }
            Err(e) => {
                self.reusable = false;
                Err(e)
            }
        }
    }
}

/// Read one reply off `reader`. `buffered` holds bytes already read from
/// the same stream (the leftover of a previous reply); on return it holds
/// whatever arrived past this reply. `max_body` caps the decoded body.
pub fn read_response<R: Read>(
    reader: &mut R,
    buffered: &mut Vec<u8>,
    max_body: usize,
) -> Result<Response, HttpError> {
    let mut chunk = [0u8; 8192];
    // Headers.
    let head_end = loop {
        if let Some(pos) = find(buffered, b"\r\n\r\n") {
            break pos;
        }
        if buffered.len() > MAX_HEADER_BYTES {
            return Err(HttpError::TooLarge { limit: MAX_HEADER_BYTES });
        }
        let n = reader.read(&mut chunk)?;
        if n == 0 {
            return Err(HttpError::Closed);
        }
        buffered.extend_from_slice(&chunk[..n]);
    };
    let head = std::str::from_utf8(&buffered[..head_end])
        .map_err(|_| HttpError::Malformed("non-UTF-8 headers".into()))?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let mut parts = status_line.splitn(3, ' ');
    let version = parts.next().unwrap_or("");
    if !version.starts_with("HTTP/1.") {
        return Err(HttpError::Malformed(format!("status line `{status_line}`")));
    }
    let status: u16 = parts
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| HttpError::Malformed(format!("status line `{status_line}`")))?;
    let mut headers = Vec::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            return Err(HttpError::Malformed(format!("header line `{line}`")));
        };
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
    }
    let header = |name: &str| headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str());
    let connection = header("connection").map(|v| v.to_ascii_lowercase());
    let mut keep_alive = match connection.as_deref() {
        Some(v) if v.split(',').any(|t| t.trim() == "close") => false,
        Some(v) if v.split(',').any(|t| t.trim() == "keep-alive") => true,
        _ => version == "HTTP/1.1",
    };
    let chunked = header("transfer-encoding")
        .map(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case("chunked")))
        .unwrap_or(false);
    let length = match header("content-length") {
        Some(v) => Some(
            v.parse::<usize>()
                .map_err(|_| HttpError::Malformed(format!("content-length `{v}`")))?,
        ),
        None => None,
    };
    buffered.drain(..head_end + 4);

    let no_body = (100..200).contains(&status) || status == 204 || status == 304;
    let body = if no_body {
        Vec::new()
    } else if chunked {
        read_chunked(reader, buffered, max_body)?
    } else if let Some(length) = length {
        if length > max_body {
            return Err(HttpError::TooLarge { limit: max_body });
        }
        fill(reader, buffered, length)?;
        buffered.drain(..length).collect()
    } else {
        // No framing: the body is everything until the server closes.
        keep_alive = false;
        loop {
            if buffered.len() > max_body {
                return Err(HttpError::TooLarge { limit: max_body });
            }
            let n = reader.read(&mut chunk)?;
            if n == 0 {
                break;
            }
            buffered.extend_from_slice(&chunk[..n]);
        }
        std::mem::take(buffered)
    };
    Ok(Response {
        status,
        headers,
        body,
        keep_alive,
    })
}

/// Make sure `buffered` holds at least `len` bytes, reading as needed.
fn fill<R: Read>(reader: &mut R, buffered: &mut Vec<u8>, len: usize) -> Result<(), HttpError> {
    let mut chunk = [0u8; 8192];
    while buffered.len() < len {
        let n = reader.read(&mut chunk)?;
        if n == 0 {
            return Err(HttpError::Closed);
        }
        buffered.extend_from_slice(&chunk[..n]);
    }
    Ok(())
}

/// Read one CRLF-terminated line off the front of `buffered` (reading more
/// as needed), without the CRLF. Lines are capped like headers.
fn read_line<R: Read>(reader: &mut R, buffered: &mut Vec<u8>) -> Result<String, HttpError> {
    let mut chunk = [0u8; 8192];
    loop {
        if let Some(pos) = find(buffered, b"\r\n") {
            let line: Vec<u8> = buffered.drain(..pos + 2).collect();
            return std::str::from_utf8(&line[..pos])
                .map(str::to_owned)
                .map_err(|_| HttpError::Malformed("non-UTF-8 chunk line".into()));
        }
        if buffered.len() > MAX_HEADER_BYTES {
            return Err(HttpError::TooLarge { limit: MAX_HEADER_BYTES });
        }
        let n = reader.read(&mut chunk)?;
        if n == 0 {
            return Err(HttpError::Closed);
        }
        buffered.extend_from_slice(&chunk[..n]);
    }
}

/// Decode a chunked body: `<hex size>[;ext]\r\n<data>\r\n` until a zero
/// chunk, then trailers up to an empty line.
fn read_chunked<R: Read>(
    reader: &mut R,
    buffered: &mut Vec<u8>,
    max_body: usize,
) -> Result<Vec<u8>, HttpError> {
    let mut body = Vec::new();
    loop {
        let line = read_line(reader, buffered)?;
        let size_text = line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|_| HttpError::Malformed(format!("chunk size `{line}`")))?;
        if size == 0 {
            break;
        }
        if body.len() + size > max_body {
            return Err(HttpError::TooLarge { limit: max_body });
        }
        fill(reader, buffered, size + 2)?;
        body.extend(buffered.drain(..size));
        let crlf: Vec<u8> = buffered.drain(..2).collect();
        if crlf != b"\r\n" {
            return Err(HttpError::Malformed("chunk data not followed by CRLF".into()));
        }
    }
    // Trailers (none expected, but a server may send them) end at an empty line.
    loop {
        let line = read_line(reader, buffered)?;
        if line.is_empty() {
            break;
        }
    }
    Ok(body)
}

pub(crate) fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn read(raw: &[u8], max: usize) -> Result<(Response, Vec<u8>), HttpError> {
        let mut cursor = Cursor::new(raw.to_vec());
        let mut buffered = Vec::new();
        let response = read_response(&mut cursor, &mut buffered, max)?;
        Ok((response, buffered))
    }

    #[test]
    fn a_content_length_reply_is_read_exactly_and_the_rest_stays_buffered() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\n\r\n{\"ok\":true}HTTP/1.1 204";
        let (r, rest) = read(raw, 1024).unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"{\"ok\":true}");
        assert_eq!(r.json(), Some(serde_json::json!({"ok": true})));
        assert_eq!(r.header("content-type"), Some("application/json"));
        assert!(r.keep_alive, "HTTP/1.1 keeps the connection by default");
        assert_eq!(rest, b"HTTP/1.1 204", "the next reply's bytes are kept, not lost");
    }

    #[test]
    fn a_chunked_reply_is_decoded_with_extensions_and_trailers_dropped() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
                    4;ext=1\r\nWiki\r\n6\r\npedia \r\nE\r\nin \r\n\r\nchunks.\r\n0\r\nExpires: never\r\n\r\nleft";
        let (r, rest) = read(raw, 1024).unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"Wikipedia in \r\n\r\nchunks.");
        assert!(r.keep_alive);
        assert_eq!(rest, b"left");
    }

    #[test]
    fn chunked_json_across_several_small_chunks_parses() {
        let payload = br#"{"items":[1,2,3],"freshness":{"asOf":null}}"#;
        let mut raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        for piece in payload.chunks(7) {
            raw.extend_from_slice(format!("{:x}\r\n", piece.len()).as_bytes());
            raw.extend_from_slice(piece);
            raw.extend_from_slice(b"\r\n");
        }
        raw.extend_from_slice(b"0\r\n\r\n");
        let (r, _) = read(&raw, 1024).unwrap();
        assert_eq!(r.body, payload);
        assert_eq!(r.json().unwrap()["items"], serde_json::json!([1, 2, 3]));
    }

    #[test]
    fn the_body_cap_applies_to_every_framing() {
        let fixed = b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\n01234567890123456789";
        assert_eq!(read(fixed, 19).unwrap_err(), HttpError::TooLarge { limit: 19 });
        assert!(read(fixed, 20).is_ok());
        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\na\r\n0123456789\r\na\r\n0123456789\r\n0\r\n\r\n";
        assert_eq!(read(chunked, 15).unwrap_err(), HttpError::TooLarge { limit: 15 });
        assert_eq!(read(chunked, 20).unwrap().0.body.len(), 20);
        let eof = b"HTTP/1.0 200 OK\r\n\r\nthirty bytes of body text here";
        assert_eq!(read(eof, 10).unwrap_err(), HttpError::TooLarge { limit: 10 });
        let (r, _) = read(eof, 100).unwrap();
        assert_eq!(r.body, b"thirty bytes of body text here");
        assert!(!r.keep_alive, "a read-to-end body ends the connection");
    }

    #[test]
    fn connection_header_and_version_decide_keep_alive() {
        let (r, _) = read(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", 10).unwrap();
        assert!(!r.keep_alive);
        let (r, _) = read(b"HTTP/1.0 200 OK\r\nContent-Length: 0\r\n\r\n", 10).unwrap();
        assert!(!r.keep_alive, "HTTP/1.0 closes unless told otherwise");
        let (r, _) = read(b"HTTP/1.0 200 OK\r\nContent-Length: 0\r\nConnection: keep-alive\r\n\r\n", 10).unwrap();
        assert!(r.keep_alive);
        let (r, rest) = read(b"HTTP/1.1 204 No Content\r\n\r\nnext", 10).unwrap();
        assert_eq!(r.status, 204);
        assert!(r.body.is_empty(), "204 never has a body");
        assert_eq!(rest, b"next");
    }

    #[test]
    fn malformed_and_truncated_replies_are_errors() {
        assert_eq!(read(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nab", 10).unwrap_err(), HttpError::Closed);
        assert_eq!(read(b"HTTP/1.1 200 OK", 10).unwrap_err(), HttpError::Closed);
        assert!(matches!(read(b"NOPE 200 OK\r\n\r\n", 10).unwrap_err(), HttpError::Malformed(_)));
        assert!(matches!(read(b"HTTP/1.1 200 OK\r\nContent-Length: x\r\n\r\n", 10).unwrap_err(), HttpError::Malformed(_)));
        assert!(matches!(
            read(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n", 10).unwrap_err(),
            HttpError::Malformed(_)
        ));
    }

    /// Two requests on ONE connection: the server accepts once and answers
    /// each as it arrives, the first with a fixed length and the second
    /// chunked; the connection stays reusable until the server says close.
    #[test]
    fn a_connection_carries_several_requests_in_turn() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut seen = Vec::new();
            let mut pending = Vec::new();
            let mut buf = [0u8; 1024];
            let replies: [&[u8]; 3] = [
                b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\none",
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\ntwo\r\n0\r\n\r\n",
                b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nthree",
            ];
            for reply in replies {
                while find(&pending, b"\r\n\r\n").is_none() {
                    let n = s.read(&mut buf).unwrap();
                    assert!(n > 0, "client closed early");
                    pending.extend_from_slice(&buf[..n]);
                }
                let end = find(&pending, b"\r\n\r\n").unwrap() + 4;
                let head = String::from_utf8(pending.drain(..end).collect()).unwrap();
                let body_len: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("Content-Length: "))
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                while pending.len() < body_len {
                    let n = s.read(&mut buf).unwrap();
                    pending.extend_from_slice(&buf[..n]);
                }
                let body = String::from_utf8(pending.drain(..body_len).collect()).unwrap();
                seen.push(head + &body);
                s.write_all(reply).unwrap();
            }
            seen
        });
        let mut conn = Connection::connect(addr, "test", Duration::from_secs(2), Duration::from_secs(5)).unwrap();
        let first = conn.request("GET", "/a", &[("X-One", "1")], None, 64).unwrap();
        assert_eq!(first.body, b"one");
        assert!(conn.is_reusable());
        let second = conn.request("POST", "/b", &[], Some(b"{}"), 64).unwrap();
        assert_eq!(second.body, b"two");
        assert!(conn.is_reusable());
        let third = conn.request("GET", "/c", &[], None, 64).unwrap();
        assert_eq!(third.body, b"three");
        assert!(!conn.is_reusable(), "the server said close");
        assert_eq!(conn.request("GET", "/d", &[], None, 64).unwrap_err(), HttpError::Closed);
        let seen = server.join().unwrap();
        assert_eq!(seen[0], "GET /a HTTP/1.1\r\nHost: test\r\nX-One: 1\r\nContent-Length: 0\r\n\r\n");
        assert_eq!(seen[1], "POST /b HTTP/1.1\r\nHost: test\r\nContent-Length: 2\r\n\r\n{}");
        assert_eq!(seen[2], "GET /c HTTP/1.1\r\nHost: test\r\nContent-Length: 0\r\n\r\n");
    }
}

