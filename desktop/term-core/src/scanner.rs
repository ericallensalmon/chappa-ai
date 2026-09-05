//! Byte-stream pre-scan for OSC 9/99/777/133 (ported from an
//! earlier standalone spike).
//!
//! `OscScanner` sits in front of `alacritty_terminal`'s parser (which drops
//! these sequences) and tees OSC payloads out as structured events while
//! passing every byte through untouched. It is a real state machine: an OSC
//! split across read chunks is completed on the next `feed`, both BEL and
//! `ESC \` terminators are handled, and the payload is capped so a hostile
//! emitter cannot grow memory without bound.
//!
//! The state machine mirrors `vte` 0.15's own OSC handling (`vte::Parser` +
//! `Performer::osc_dispatch`) so extraction and rendering agree on where a
//! sequence begins and ends: a lone ESC inside an OSC terminates it
//! immediately (the `ESC \` is then just an escape-level no-op), BEL
//! terminates it, and CAN/SUB (`0x18`/`0x1A`) terminate it too.
//!
//! Ground-state scanning is a SIMD ESC search (`scan::find_esc`, AVX-512/
//! AVX2/`memchr` ladder) so plain text costs about one memory-bandwidth pass —
//! the spike's benchmark measured well under 0.5% end-to-end overhead against
//! the real parse pipeline.
//!
use crate::scan;

/// Default cap on the OSC payload we buffer per sequence. Sequences longer
/// than this are dropped (the event is not delivered) but still consumed so
/// their bytes keep passing through.
pub const DEFAULT_MAX_PAYLOAD: usize = 4 * 1024;

/// What OSC 133 marks mean (iTerm2 / popular shell-integration vocabulary).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptMarkKind {
    PromptStart,
    PromptEnd,
    CommandStart,
    CommandEnd,
}

/// A kitty keyboard-protocol control sequence (minimal scope). These
/// are CSI `u` forms the embedded terminal ignores, so the scanner extracts
/// them alongside the OSC events; the actor owns the actual flag stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KittyEvent {
    /// `CSI > <flags> u` — push flags (progressive enhancement). The value is
    /// the raw parameter; the actor masks it to the flags it honours.
    Push(u32),
    /// `CSI < [n] u` — pop n levels off the stack (default 1).
    Pop(u32),
    /// `CSI = <flags> [; <mode>] u` — set flags directly on the current
    /// stack entry (mode 1 = replace, 2 = union, 3 = difference; omitted
    /// mode = 1). Some clients use this instead of push/pop — Claude Code's
    /// Shift+Enter host-run failure is why it graduated into the minimal
    /// scope. The actor masks the flags it honours.
    Set(u32, u32),
    /// `CSI ? u` — query the current flags; the actor replies through the pty.
    Query,
    /// `ESC c` (RIS) — the flag stack is cleared.
    Reset,
}

/// An OSC sequence we extracted that the terminal itself ignores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OscEvent {
    /// OSC 9 / 777 `notify` / 99 (kitty-style). Title is `None` when the
    /// emitter sent an empty title field.
    Notify { title: Option<String>, body: String },
    /// OSC 133 A/B/C/D semantic prompt marks.
    PromptMark(PromptMarkKind),
    /// A kitty keyboard-protocol control sequence (`CSI ? u`, `CSI > … u`,
    /// `CSI < … u`, `ESC c`). See [`KittyEvent`].
    Kitty(KittyEvent),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    Escape,
    Osc,
    Csi,
}

/// Cap on the CSI bytes we buffer for kitty detection. Kitty sequences are
/// `ESC [ <intermediate> <params> u`; real params are a handful of digits, so
/// 32 bytes is generous. Anything longer is definitely not a kitty form.
const CSI_DETECT_CAP: usize = 32;

/// Streaming scanner. Call [`OscScanner::feed`] for every PTY chunk, in
/// order, before handing the same bytes to the terminal parser.
#[derive(Debug)]
pub struct OscScanner {
    state: State,
    /// Raw payload bytes of the in-progress OSC (code + params), capped.
    payload: Vec<u8>,
    cap: usize,
    /// True once the in-progress OSC exceeded `cap`; its event is dropped.
    dropped: bool,
    /// Buffered bytes of the in-progress CSI (after `ESC [`), used only to
    /// decide whether it is a kitty `u` form. Capped; detection aborts past
    /// the cap but the bytes still flow through to the parser untouched.
    csi: Vec<u8>,
}

impl Default for OscScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl OscScanner {
    pub fn new() -> Self {
        Self::with_cap(DEFAULT_MAX_PAYLOAD)
    }

    pub fn with_cap(cap: usize) -> Self {
        Self {
            state: State::Ground,
            payload: Vec::with_capacity(cap),
            cap,
            dropped: false,
            csi: Vec::with_capacity(CSI_DETECT_CAP),
        }
    }

    /// Scan `chunk`. Returns the OSC events whose terminator landed inside
    /// this chunk. The chunk itself is only read, never copied or modified:
    /// pass the same bytes straight on to `alacritty_terminal`.
    #[inline]
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<OscEvent> {
        let mut events = Vec::new();
        let mut pos = 0;
        while pos < chunk.len() {
            match self.state {
                State::Ground => {
                    // Fast path: plain text has no control bytes, so jump
                    // straight to the next ESC at SIMD speed (see scan.rs).
                    // For a 100MB plain-text stream this is one search pass
                    // at ~memory bandwidth — cheaper than copying the same
                    // bytes to another buffer.
                    let pos_after = match scan::find_esc(&chunk[pos..]) {
                        Some(i) => {
                            self.state = State::Escape;
                            pos + i + 1
                        }
                        None => chunk.len(),
                    };
                    pos = pos_after;
                }
                State::Escape => {
                    match chunk[pos] {
                        // `ESC ]` opens an OSC string.
                        0x5D => {
                            self.payload.clear();
                            self.dropped = false;
                            self.state = State::Osc;
                        }
                        // `ESC [` opens a CSI; buffer it to detect the kitty
                        // `u` forms. Normal CSI passes through either way.
                        0x5B => {
                            self.csi.clear();
                            self.state = State::Csi;
                        }
                        // `ESC c` is RIS — the kitty flag stack is cleared.
                        b'c' => {
                            events.push(OscEvent::Kitty(KittyEvent::Reset));
                            self.state = State::Ground;
                        }
                        // `ESC ESC ...` — the second ESC is a pad (vte
                        // ignores it and stays in Escape).
                        0x1B => {}
                        // Anything else: a normal two-byte escape, back to
                        // ground. `ESC \` with empty intermediates is a
                        // harmless no-op here.
                        _ => self.state = State::Ground,
                    }
                    pos += 1;
                }
                State::Csi => {
                    match chunk[pos] {
                        // Final byte (0x40..=0x7E): the sequence is complete.
                        // Decide whether it was a kitty `u` form.
                        b if (0x40..=0x7E).contains(&b) => {
                            if let Some(event) = parse_kitty(&self.csi, b) {
                                events.push(OscEvent::Kitty(event));
                            }
                            self.csi.clear();
                            self.state = State::Ground;
                        }
                        // Control bytes abort the sequence (vte executes
                        // CAN/SUB and other controls); never part of a kitty
                        // form, so drop the detection buffer.
                        0x00..=0x1F | 0x7F => {
                            self.csi.clear();
                            self.state = State::Ground;
                        }
                        // Intermediates + params (0x20..=0x3F): buffer for
                        // detection, capped. Past the cap the sequence cannot
                        // be a kitty form; keep consuming until the final
                        // byte without buffering.
                        b => {
                            if self.csi.len() < CSI_DETECT_CAP {
                                self.csi.push(b);
                            }
                        }
                    }
                    pos += 1;
                }
                State::Osc => {
                    match chunk[pos] {
                        0x07 => {
                            // BEL terminates the OSC.
                            self.finish(&mut events);
                            self.state = State::Ground;
                        }
                        0x18 | 0x1A => {
                            // CAN/SUB terminate it too (vte executes them
                            // after dispatch).
                            self.finish(&mut events);
                            self.state = State::Ground;
                        }
                        0x1B => {
                            // A lone ESC immediately ends the OSC (matching
                            // vte); the next byte is read in Escape state,
                            // where `\` closes the ST.
                            self.finish(&mut events);
                            self.state = State::Escape;
                        }
                        b => {
                            if !self.dropped {
                                if self.payload.len() < self.cap {
                                    self.payload.push(b);
                                } else {
                                    // Oversized: forget the payload, keep
                                    // consuming so the stream stays intact.
                                    self.dropped = true;
                                    self.payload.clear();
                                }
                            }
                        }
                    }
                    pos += 1;
                }
            }
        }
        events
    }

    /// Scan and also copy `chunk` verbatim into `out`. Convenience for a
    /// tee, and the shape the benchmark measures (scan + passthrough in one
    /// pass); the actor can equally just `feed` and hand bytes to the parser.
    #[inline]
    pub fn feed_to(&mut self, chunk: &[u8], out: &mut Vec<u8>) -> Vec<OscEvent> {
        out.extend_from_slice(chunk);
        self.feed(chunk)
    }

    fn finish(&mut self, events: &mut Vec<OscEvent>) {
        if !self.dropped && !self.payload.is_empty() {
            if let Some(event) = parse_osc(&self.payload) {
                events.push(event);
            }
        }
        self.payload.clear();
        self.dropped = false;
    }
}

/// Turn a buffered OSC payload into an event. Payloads whose first param is
/// not one of 9/99/777/133 (or that don't parse) yield `None` and are
/// silently dropped — exactly what the spike is meant to demonstrate.
fn parse_osc(payload: &[u8]) -> Option<OscEvent> {
    let mut params = payload.split(|&b| b == b';');
    let code = params.next()?;
    match code {
        b"9" => {
            let body = join_rest(&mut params);
            Some(OscEvent::Notify {
                title: None,
                body: lossy(&body),
            })
        }
        b"99" => {
            let title = params.next().unwrap_or_default();
            let body = join_rest(&mut params);
            Some(OscEvent::Notify {
                title: non_empty(title),
                body: lossy(&body),
            })
        }
        b"777" => {
            // Windows Terminal notify: `777;notify;title;body`.
            let sub = params.next().unwrap_or_default();
            if sub == b"notify" {
                let title = params.next().unwrap_or_default();
                let body = join_rest(&mut params);
                Some(OscEvent::Notify {
                    title: non_empty(title),
                    body: lossy(&body),
                })
            } else {
                None
            }
        }
        b"133" => {
            // `133;<letter>` optionally followed by more params (e.g.
            // `133;A;Id=...`); the letter is the kind.
            let kind = params.next().unwrap_or_default();
            let kind = match kind.first() {
                Some(b'A') => PromptMarkKind::PromptStart,
                Some(b'B') => PromptMarkKind::PromptEnd,
                Some(b'C') => PromptMarkKind::CommandStart,
                Some(b'D') => PromptMarkKind::CommandEnd,
                _ => return None,
            };
            Some(OscEvent::PromptMark(kind))
        }
        _ => None,
    }
}

/// Decide whether a completed CSI (buffered bytes after `ESC [`, plus the
/// final byte) is one of the kitty keyboard-protocol forms. Mirrors vte's
/// dispatch: `?`/`>`/`<` collected as the leading intermediate, final `u`.
/// Anything else yields `None` (a normal CSI the parser handles itself).
fn parse_kitty(buf: &[u8], final_byte: u8) -> Option<KittyEvent> {
    if final_byte != b'u' {
        return None;
    }
    let (intermediate, params) = buf.split_first()?;
    match *intermediate {
        b'?' => {
            // `CSI ? u` — the query carries no params.
            if params.is_empty() {
                Some(KittyEvent::Query)
            } else {
                None
            }
        }
        b'>' => {
            // `CSI > <flags> u` — flags are one positive decimal.
            let flags = digits(params)?;
            Some(KittyEvent::Push(flags))
        }
        b'<' => {
            // `CSI < u` pops 1, `CSI < n u` pops n.
            if params.is_empty() {
                Some(KittyEvent::Pop(1))
            } else {
                Some(KittyEvent::Pop(digits(params)?))
            }
        }
        b'=' => {
            // `CSI = <flags> [; <mode>] u` — direct set; mode defaults to 1.
            let mut parts = params.split(|b| *b == b';');
            let flags = digits(parts.next()?)?;
            let mode = match parts.next() {
                Some(m) => digits(m)?,
                None => 1,
            };
            if parts.next().is_some() || !(1..=3).contains(&mode) {
                return None;
            }
            Some(KittyEvent::Set(flags, mode))
        }
        _ => None,
    }
}

/// Parse a positive decimal digit run. Empty or non-digit input yields None.
fn digits(bytes: &[u8]) -> Option<u32> {
    if bytes.is_empty() || !bytes.iter().all(|b| b.is_ascii_digit()) {
        return None;
    }
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

/// Rebuild the remaining params joined by `;` (bodies may contain `;`).
fn join_rest<'a>(rest: &mut impl Iterator<Item = &'a [u8]>) -> Vec<u8> {
    let mut out = Vec::new();
    for (i, part) in rest.enumerate() {
        if i > 0 {
            out.push(b';');
        }
        out.extend_from_slice(part);
    }
    out
}

fn non_empty(bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() {
        None
    } else {
        Some(lossy(bytes))
    }
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}
