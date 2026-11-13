//! Session-id capture off the raw output stream.
//!
//! Some agent CLIs print the id of the conversation they are running (at
//! startup, or as they exit) but offer no structured channel for it. The
//! [`LineMatcher`] turns the pty byte stream into plain-text lines — escape
//! sequences stripped, so a colour code in the middle of a line cannot hide
//! the id — and hands each completed line to an embedder-supplied
//! [`SessionMatcher`], which answers the id when the line carries one.
//!
//! Matching runs on the BYTE stream rather than the rendered screen on
//! purpose: a 36-character id wraps at the terminal width and would never
//! match row by row, and the stream is the one view that still exists after
//! the child has exited (an id printed on the way out must still count).

use std::fmt;
use std::sync::Arc;

/// Longest line the matcher buffers. A line past this is dropped whole
/// (its bytes still reach the parser untouched) — nothing that carries a
/// session id is that long, and a hostile stream must not grow memory.
pub const MAX_LINE: usize = 8 * 1024;

/// The embedder's recognizer: one plain-text line in, the session id out
/// when the line carries one. Supplied per spawn, so the engine knows
/// nothing about any particular CLI's wording.
#[derive(Clone)]
pub struct SessionMatcher(pub Arc<dyn Fn(&str) -> Option<String> + Send + Sync>);

impl SessionMatcher {
    pub fn new(f: impl Fn(&str) -> Option<String> + Send + Sync + 'static) -> Self {
        Self(Arc::new(f))
    }

    pub fn matches(&self, line: &str) -> Option<String> {
        (self.0)(line)
    }
}

impl fmt::Debug for SessionMatcher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionMatcher(..)")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Strip {
    Text,
    Escape,
    Csi,
    Osc,
    /// Inside an OSC that ended with ESC: the next byte is the `\` of ST.
    OscEsc,
}

/// Streaming line assembler: strips escape sequences and control bytes,
/// splits on CR / LF, and runs the matcher over every completed line. Call
/// [`LineMatcher::feed`] for every chunk, in order.
#[derive(Debug)]
pub struct LineMatcher {
    matcher: SessionMatcher,
    line: Vec<u8>,
    dropped: bool,
    strip: Strip,
}

impl LineMatcher {
    pub fn new(matcher: SessionMatcher) -> Self {
        Self {
            matcher,
            line: Vec::new(),
            dropped: false,
            strip: Strip::Text,
        }
    }

    /// Scan `chunk`; returns the ids matched by the lines that completed
    /// inside it, in stream order (so the last element is the newest).
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        for &b in chunk {
            match self.strip {
                Strip::Text => match b {
                    0x1B => self.strip = Strip::Escape,
                    b'\n' | b'\r' => self.finish_line(&mut out),
                    0x00..=0x1F | 0x7F => {}
                    _ => self.push(b),
                },
                Strip::Escape => {
                    self.strip = match b {
                        b'[' => Strip::Csi,
                        b']' => Strip::Osc,
                        // `ESC ESC` stays in escape (a pad byte); any other
                        // byte completes a two-byte escape.
                        0x1B => Strip::Escape,
                        _ => Strip::Text,
                    };
                }
                Strip::Csi => {
                    // Parameter and intermediate bytes continue the sequence;
                    // a final byte (0x40..=0x7E) or a control byte ends it.
                    if (0x40..=0x7E).contains(&b) || b < 0x20 || b == 0x7F {
                        self.strip = Strip::Text;
                    }
                }
                Strip::Osc => match b {
                    0x07 | 0x18 | 0x1A => self.strip = Strip::Text,
                    0x1B => self.strip = Strip::OscEsc,
                    _ => {}
                },
                Strip::OscEsc => {
                    // `ESC \` closes the string; anything else is a new escape.
                    self.strip = if b == b'\\' { Strip::Text } else { Strip::Escape };
                    if self.strip == Strip::Escape && b == b'[' {
                        self.strip = Strip::Csi;
                    } else if self.strip == Strip::Escape && b == b']' {
                        self.strip = Strip::Osc;
                    }
                }
            }
        }
        out
    }

    fn push(&mut self, b: u8) {
        if self.dropped {
            return;
        }
        if self.line.len() < MAX_LINE {
            self.line.push(b);
        } else {
            self.dropped = true;
            self.line.clear();
        }
    }

    fn finish_line(&mut self, out: &mut Vec<String>) {
        if !self.dropped && !self.line.is_empty() {
            let text = String::from_utf8_lossy(&self.line);
            if let Some(id) = self.matcher.matches(&text) {
                out.push(id);
            }
        }
        self.line.clear();
        self.dropped = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn after(prefix: &'static str) -> SessionMatcher {
        SessionMatcher::new(move |line| line.strip_prefix(prefix).map(|rest| rest.trim().to_owned()))
    }

    #[test]
    fn matches_completed_lines_in_stream_order() {
        let mut m = LineMatcher::new(after("session: "));
        assert!(m.feed(b"session: abc").is_empty(), "an unterminated line waits");
        assert_eq!(m.feed(b"\nother\r\nsession: def\n"), vec!["abc", "def"]);
    }

    #[test]
    fn escape_sequences_and_controls_are_stripped() {
        let mut m = LineMatcher::new(after("session: "));
        let ids = m.feed(b"\x1b[32msession: \x1b[1mab\x1b[0mcd\x1b]0;title\x07\x1b]8;;x\x1b\\\t\n");
        assert_eq!(ids, vec!["abcd"]);
    }

    #[test]
    fn a_split_escape_sequence_is_still_stripped() {
        let mut m = LineMatcher::new(after("session: "));
        assert!(m.feed(b"session: \x1b").is_empty());
        assert!(m.feed(b"[3").is_empty());
        assert_eq!(m.feed(b"1mxyz\n"), vec!["xyz"]);
    }

    #[test]
    fn an_oversized_line_is_dropped_whole() {
        let mut m = LineMatcher::new(after("session: "));
        let long = vec![b'a'; MAX_LINE + 10];
        assert!(m.feed(b"session: ").is_empty());
        assert!(m.feed(&long).is_empty());
        assert!(m.feed(b"\n").is_empty(), "the oversized line never reaches the matcher");
        assert_eq!(m.feed(b"session: ok\n"), vec!["ok"], "the next line is matched normally");
    }
}
