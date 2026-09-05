//! TermActor: one OS thread per terminal that owns the `Term` + VT parser
//! and produces damage snapshots.
//!
//! Ground rules:
//! - The actor thread is the ONLY thing touching `Term` — no mutex around it.
//! - Control messages (resize/scroll/selection/search/ack/shutdown) arrive on
//!   an mpsc drained with `recv_timeout(~8ms)` as the tick; the pty reader
//!   thread pushes byte chunks into a second channel that is drained and
//!   parsed immediately on arrival.
//! - `Event::PtyWrite` from the terminal (DA/DSR/CPR replies) loops back into
//!   the pty writer.
//! - A frame is emitted only when there is damage AND no un-acked frame is
//!   outstanding; damage self-coalesces between acks.
//!
//! Frames carry *viewport* coordinates throughout: row 0 is the top of the
//! screen, and `FrameData::display_offset` / `history_len` let the frontend
//! map viewport rows onto scrollback lines. Alacritty's grid lines are
//! converted at the boundary.

use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Row, Scroll};
use alacritty_terminal::index::{Boundary, Column, Direction, Line, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags as TermFlags;
use alacritty_terminal::term::search::{Match, RegexSearch};
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, LineDamageBounds, Term, TermDamage, TermMode};
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

use crate::color::{self, Rgba};
use crate::frame::encoded_len;
use crate::keys::{
    encode_key, encode_mouse, encode_paste, Key, KeyEvent, ModeSnapshot, MouseEvent, MouseProto,
};
use crate::pty::{self, ExitStatus, PtySession, PtySpec};
use crate::scanner::{KittyEvent, OscEvent, OscScanner, PromptMarkKind};

/// Re-exported so embedders never need to reach into alacritty_terminal.
pub use alacritty_terminal::index::Point;
/// Cursor shape carried in frame headers.
pub use alacritty_terminal::vte::ansi::CursorShape;

/// Poll interval of the actor's control channel — the frame tick.
const TICK: Duration = Duration::from_millis(8);

/// How long after the child exits we keep draining pty output before giving
/// up on EOF (guards platforms whose pty reader never sees EOF, e.g. ConPTY).
const EXIT_FLUSH_TIMEOUT: Duration = Duration::from_millis(500);

/// Size of the pty reader's per-read buffer.
const READ_BUF: usize = 4096;

/// Everything needed to build one actor. `spec` supplies the child command,
/// environment and initial dimensions; `scrollback_lines` caps history. The
/// last two are the settings an actor needs at SPAWN; both are also
/// live-settable ([`TermHandle::set_synthetic_marks`] /
/// [`TermHandle::set_wheel_speed`]) so a settings change never requires
/// restarting terminals.
#[derive(Debug, Clone)]
pub struct ActorConfig {
    pub spec: PtySpec,
    pub scrollback_lines: usize,
    /// Plant a synthetic `PromptStart` mark on unmodified Enter.
    /// DEFAULT FALSE, permanently: it guesses. Never enable it implicitly.
    pub synthetic_prompt_marks: bool,
    /// Scroll wheel speed, `1x…6x`, default 3x. Applies ONLY to
    /// the alt-screen wheel→arrows path in `keys::alt_screen_wheel`; normal
    /// scrollback stays at system speed.
    pub wheel_speed: u8,
}

impl Default for ActorConfig {
    fn default() -> Self {
        Self {
            spec: PtySpec::default(),
            scrollback_lines: 10_000,
            synthetic_prompt_marks: false,
            wheel_speed: 3,
        }
    }
}

/// Semantic prompt mark kinds (OSC 133), surfaced verbatim from the scanner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkKind {
    PromptStart,
    PromptEnd,
    CommandStart,
    CommandEnd,
}

/// What a frame covers: the whole viewport, or only damaged row spans.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    Full,
    Delta,
}

/// Which direction `search_nav` moves the viewport: down the scrollback
/// towards newer output (`Next`) or up towards the oldest (`Prev`). The
/// "next match" is relative to the current viewport and wraps around.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchNavDir {
    Next,
    Prev,
}

/// The kitty keyboard-protocol flag we honour: 0b1 = disambiguate escape
/// codes. Everything else (key-release events, alternate keys, all-keys-as-
/// esc, associated text) is deliberately NOT advertised or honoured — three
/// regressions lived in the key-release machinery.
pub const KITTY_SUPPORTED_FLAGS: u32 = 0b1;

/// Kitty flag-stack depth cap (the kitty spec's limit).
pub const KITTY_STACK_MAX: usize = 128;

/// Cursor position + appearance in viewport coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorState {
    /// Viewport row of the cursor (0 = top of screen).
    pub row: u16,
    pub col: u16,
    pub shape: CursorShape,
    pub visible: bool,
}

/// Cell flags that survive the trip to the renderer. Mirrors the PLAN wire
/// flags; underline kinds ride the low three bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellFlags(u16);

impl CellFlags {
    pub const BOLD: Self = Self(1 << 0);
    pub const ITALIC: Self = Self(1 << 1);
    pub const DIM: Self = Self(1 << 2);
    pub const UNDERLINE: Self = Self(1 << 3);
    pub const DOUBLE_UNDERLINE: Self = Self(1 << 4);
    pub const UNDERCURL: Self = Self(1 << 5);
    pub const DOTTED_UNDERLINE: Self = Self(1 << 6);
    pub const DASHED_UNDERLINE: Self = Self(1 << 7);
    pub const INVERSE: Self = Self(1 << 8);
    pub const STRIKEOUT: Self = Self(1 << 9);
    pub const HIDDEN: Self = Self(1 << 10);
    pub const WIDE: Self = Self(1 << 11);
    pub const WIDE_SPACER: Self = Self(1 << 12);
    pub const EMPTY: Self = Self(0);

    #[inline]
    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    #[inline]
    pub fn from_alacritty(flags: TermFlags) -> Self {
        let mut out = Self::EMPTY;
        if flags.contains(TermFlags::BOLD) {
            out.0 |= Self::BOLD.0;
        }
        if flags.contains(TermFlags::ITALIC) {
            out.0 |= Self::ITALIC.0;
        }
        if flags.contains(TermFlags::DIM) {
            out.0 |= Self::DIM.0;
        }
        if flags.contains(TermFlags::UNDERLINE) {
            out.0 |= Self::UNDERLINE.0;
        }
        if flags.contains(TermFlags::DOUBLE_UNDERLINE) {
            out.0 |= Self::DOUBLE_UNDERLINE.0;
        }
        if flags.contains(TermFlags::UNDERCURL) {
            out.0 |= Self::UNDERCURL.0;
        }
        if flags.contains(TermFlags::DOTTED_UNDERLINE) {
            out.0 |= Self::DOTTED_UNDERLINE.0;
        }
        if flags.contains(TermFlags::DASHED_UNDERLINE) {
            out.0 |= Self::DASHED_UNDERLINE.0;
        }
        if flags.contains(TermFlags::INVERSE) {
            out.0 |= Self::INVERSE.0;
        }
        if flags.contains(TermFlags::STRIKEOUT) {
            out.0 |= Self::STRIKEOUT.0;
        }
        if flags.contains(TermFlags::HIDDEN) {
            out.0 |= Self::HIDDEN.0;
        }
        if flags.contains(TermFlags::WIDE_CHAR) {
            out.0 |= Self::WIDE.0;
        }
        if flags.contains(TermFlags::WIDE_CHAR_SPACER) {
            out.0 |= Self::WIDE_SPACER.0;
        }
        out
    }
}

impl std::ops::BitOr for CellFlags {
    type Output = Self;
    #[inline]
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

/// One resolved cell for the renderer. `fg`/`bg` are fully resolved here
/// (palette/named lookups happen in `crate::color`); `link_id` is an index
/// into the actor's hyperlink table (0 = none).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub fg: Rgba,
    pub bg: Rgba,
    pub flags: CellFlags,
    pub link_id: u16,
}

/// A horizontal span of cells at one viewport row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowPatch {
    /// Viewport row (0 = top of screen).
    pub row: u16,
    /// First column covered by `cells` (for deltas; always 0 on fulls).
    pub col_start: u16,
    pub cells: Vec<Cell>,
}

/// A damage snapshot the embedder can draw. All coordinates are viewport
/// relative; `display_offset` / `history_len` map them onto scrollback.
#[derive(Debug, Clone)]
pub struct FrameData {
    pub seq: u32,
    pub kind: FrameKind,
    pub cursor: CursorState,
    pub display_offset: usize,
    pub history_len: usize,
    /// Active selection as (start, end) viewport points. None when nothing
    /// is selected OR the selection is scrolled fully out of the viewport —
    /// this drives the overlay only; gate copy on `selection_active`.
    pub selection: Option<(Point, Point)>,
    /// A non-empty selection exists somewhere (viewport or scrollback).
    /// `copy_selection` would return text — the frontend's copy gate. Rides
    /// wire flags bit 0.
    pub selection_active: bool,
    /// Any mouse tracking protocol is active (xterm 1000/1002/1003). The
    /// frontend suppresses its local drag-selection and wheel→scroll when
    /// this is set. Rides wire flags bit 1.
    pub mouse_capture: bool,
    /// The alternate screen is active (xterm 1049). Rides wire flags bit 2.
    pub alt_screen: bool,
    /// Regex matches intersecting the viewport, as (start, end) points.
    pub search_matches: Vec<(Point, Point)>,
    pub rows: Vec<RowPatch>,
    /// Zero-width combiners: (viewport row, column, chars).
    pub zerowidth: Vec<ZerowidthEntry>,
}

/// Rolling frame counters for the debug HUD, queryable via
/// [`TermHandle::stats`]. All values are actor-side facts; the frontend's
/// HUD overlays them with its own render-side counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FrameStats {
    /// Frames emitted since the actor started.
    pub frames_sent: u64,
    /// Total wire bytes emitted (`frame::encoded_len`, the same bytes
    /// `encode_frame` produces — the HUD's MB/s input).
    pub bytes_sent: u64,
    /// Rows covered by the last emitted frame (damaged span count for a
    /// delta, full screen height for a full).
    pub damage_rows_last: u32,
    /// Ticks where damage existed but emission was held by the ack gate —
    /// how much coalescing actually happened (gate pressure).
    pub coalesced_ticks: u64,
    /// Whether a frame is currently un-acked (the ack gate is closed).
    pub outstanding: bool,
}

/// Everything the actor pushes to the embedder-owned event channel.
#[derive(Debug)]
pub enum TermEvent {
    Frame(FrameData),
    Title(String),
    Bell,
    Notify {
        title: String,
        body: String,
    },
    PromptMark {
        kind: MarkKind,
        row: i64,
    },
    /// Link ids assigned this frame: (id, uri).
    HyperlinkTable(Vec<(u16, String)>),
    /// Search state changed: `total` matches exist across the whole grid
    /// (scrollback + viewport). Emitted on a new search, a resize, and a
    /// requested full only — the count is deliberately NOT recounted on every
    /// frame, so it can drift stale as scrollback scrolls. The frontend shows
    /// it with that caveat.
    SearchStatus {
        total: usize,
    },
    Clipboard(String),
    Exited(ExitStatus),
}

/// Kinds of selection the frontend can start/update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionKind {
    Simple,
    Block,
    Lines,
    Semantic,
}

/// Selection commands. Points are viewport coordinates (row 0 = top).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionOp {
    Start { point: Point, kind: SelectionKind },
    Update { point: Point, kind: SelectionKind },
    Clear,
}

/// Byte-stream liveness counters: what the pty READER
/// saw, independent of rendering. Deriving "idle" from rendered-frame
/// diffs both flickered and wedged; these are bumped on the raw byte
/// path (reader thread / `feed`), so `last_output_ms` answers "is anything
/// still flowing" and `output_bytes == 0` answers "still booting" — the
/// empty-output trap where a booting process looked dead.
///
/// Lock-free atomics so a status probe never contends with parsing; the
/// mutex/condvar pair is touched only to WAKE a parked
/// [`IoCounters::wait_for_output`] (the control surface's send receipt), never
/// on the read path.
#[derive(Debug, Default)]
pub struct IoCounters {
    /// Total bytes the reader delivered (monotonic; a cheap activity seq).
    output_bytes: AtomicU64,
    /// Unix-epoch ms of the last delivered chunk; 0 = nothing yet.
    last_output_ms: AtomicU64,
    /// Cleared the moment the actor observes the child's exit.
    child_alive: AtomicBool,
    /// Held by a waiter across "read the counter → park", so a `record_output`
    /// between those two steps cannot lose the wakeup.
    gate: Mutex<()>,
    /// Signalled by `record_output`.
    output_signal: Condvar,
}

impl IoCounters {
    fn new(child_alive: bool) -> Self {
        Self {
            output_bytes: AtomicU64::new(0),
            last_output_ms: AtomicU64::new(0),
            child_alive: AtomicBool::new(child_alive),
            gate: Mutex::new(()),
            output_signal: Condvar::new(),
        }
    }

    fn record_output(&self, n: usize) {
        self.output_bytes.fetch_add(n as u64, Ordering::Relaxed);
        self.last_output_ms.store(epoch_ms(), Ordering::Relaxed);
        // Taking the gate is what makes the wakeup un-loseable: a waiter that
        // has already read the counter but has not parked yet still holds it,
        // so this blocks until that waiter is inside `wait_timeout`. Nothing
        // can panic while the gate is held, so a poisoned lock is impossible
        // — and is ignored rather than unwrapped anyway (this runs on the pty
        // reader thread).
        drop(self.gate.lock());
        self.output_signal.notify_all();
    }

    /// Park until `output_bytes` moves away from `since`, or `timeout`
    /// elapses; returns the counter as observed. ONE parked wait instead of a
    /// sleep-poll loop — the control surface's `send_input` receipt asks
    /// exactly this question ("did the write produce terminal activity").
    pub fn wait_for_output(&self, since: u64, timeout: Duration) -> u64 {
        let deadline = Instant::now() + timeout;
        let mut gate = match self.gate.lock() {
            Ok(gate) => gate,
            Err(poisoned) => poisoned.into_inner(),
        };
        loop {
            let seen = self.output_bytes();
            if seen != since {
                return seen;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return seen;
            }
            gate = match self.output_signal.wait_timeout(gate, remaining) {
                Ok((gate, _)) => gate,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
    }

    pub fn output_bytes(&self) -> u64 {
        self.output_bytes.load(Ordering::Relaxed)
    }

    /// `None` until the first byte arrived (`has_output == false`).
    pub fn last_output_ms(&self) -> Option<u64> {
        match self.last_output_ms.load(Ordering::Relaxed) {
            0 => None,
            ms => Some(ms),
        }
    }

    pub fn has_output(&self) -> bool {
        self.output_bytes() > 0
    }

    pub fn child_alive(&self) -> bool {
        self.child_alive.load(Ordering::Relaxed)
    }
}

/// Unix-epoch milliseconds — the ONE clock reading in the workspace (the Tauri
/// registry's input journal and the project lifecycle both call this).
///
/// Never returns 0: `IoCounters::last_output_ms` stores 0 as the "nothing has
/// arrived yet" sentinel, so a real reading must not collide with it. The
/// clamp costs a millisecond of accuracy only for timestamps at the epoch
/// itself (or a clock that cannot be read at all).
pub fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(1)
        .max(1)
}

/// Cheap clonable handle. Every method just sends on the control mpsc, except
/// `write_input` which goes straight to the pty writer.
pub struct TermHandle {
    tx: Sender<Control>,
    feed_tx: Sender<ByteMsg>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    join: Arc<Mutex<Option<JoinHandle<()>>>>,
    io: Arc<IoCounters>,
    /// OS pid of the pty child, captured at spawn. `None` for a
    /// no-pty actor. Recorded rather than queried through the control channel
    /// on purpose: the stats poller must be able to read it after the actor
    /// thread has ended, and a pid never changes for the life of the handle.
    pid: Option<u32>,
}

impl Clone for TermHandle {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            feed_tx: self.feed_tx.clone(),
            writer: self.writer.clone(),
            join: self.join.clone(),
            io: self.io.clone(),
            pid: self.pid,
        }
    }
}

impl TermHandle {
    /// The byte-flow liveness counters (see [`IoCounters`]). Lock-free; safe
    /// to poll from any thread at any rate.
    pub fn io(&self) -> &IoCounters {
        &self.io
    }

    /// Pre-encoded input bytes (the encoder produces these). Bypasses
    /// the control channel entirely — never queued behind parsing.
    pub fn write_input(&self, bytes: &[u8]) {
        if let Ok(mut writer) = self.writer.lock() {
            let _ = writer.write_all(bytes);
            let _ = writer.flush();
        }
    }

    /// Encode and send a semantic key event. The terminal modes that decide
    /// the encoding live on the actor thread, so the snapshot happens there
    /// and only the resulting bytes touch the writer.
    pub fn write_key(&self, ev: KeyEvent) {
        let _ = self.tx.send(Control::WriteKey(ev));
    }

    /// Encode and send pasted text (bracketed-paste wrapping + injection
    /// guard applied against the live mode snapshot).
    pub fn paste(&self, text: &str) {
        let _ = self.tx.send(Control::Paste(text.to_owned()));
    }

    /// Encode and send a mouse event against the live mouse-mode snapshot.
    pub fn mouse(&self, ev: MouseEvent) {
        let _ = self.tx.send(Control::Mouse(ev));
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        let _ = self.tx.send(Control::Resize { cols, rows });
    }

    /// Winsize poke (bridge probe): wiggle the PTY's window size
    /// (rows-1, then back) WITHOUT touching the grid, so the child gets a
    /// SIGWINCH / ConPTY resize pair and a healthy TUI repaints. A plain
    /// `resize` to the current size is deliberately dropped by the actor
    /// (identical dimensions = no-op), which is why this exists. No frame is
    /// emitted; the only observable is the child's own output.
    pub fn poke(&self) {
        let _ = self.tx.send(Control::Poke);
    }

    /// A no-pty actor fronting a PIPED child (the json
    /// agent transport) reports the child's byte flow here so the liveness
    /// counters (`has_output`, `last_output_ms`, `output_bytes`) stay true
    /// without pushing anything through the VT parser.
    pub fn note_output(&self, n: usize) {
        self.io.record_output(n);
    }

    /// The piped child behind a no-pty actor exited. Flips
    /// `child_alive`, and the actor reports `Exited(status)` through the
    /// normal event path (so the registry's status/exit_code follow).
    pub fn note_child_exit(&self, status: ExitStatus) {
        self.io.child_alive.store(false, Ordering::Relaxed);
        let _ = self.tx.send(Control::ChildExited(status));
    }

    /// Scroll by `delta_lines` viewport lines; positive scrolls up into
    /// history.
    pub fn scroll(&self, delta_lines: i32) {
        let _ = self.tx.send(Control::Scroll(delta_lines));
    }

    /// Set the absolute display offset (e.g. from a scrollbar thumb).
    pub fn set_display_offset(&self, offset: usize) {
        let _ = self.tx.send(Control::SetDisplayOffset(offset));
    }

    pub fn selection(&self, op: SelectionOp) {
        let _ = self.tx.send(Control::Selection(op));
    }

    /// Synchronous: ask the actor to serialize the current selection.
    pub fn copy_selection(&self) -> Option<String> {
        let (tx, rx) = channel();
        if self.tx.send(Control::CopySelection(SyncReply(tx))).is_err() {
            return None;
        }
        rx.recv().ok().flatten()
    }

    /// Synchronous: serialize the current viewport plus the last `lines`
    /// scrollback rows, oldest first, trailing whitespace trimmed. Same
    /// control-channel round trip as `copy_selection`; backs the debug HTTP
    /// `/text` route. Falls back to an empty dump if the actor has already
    /// exited (the actor thread ends after `Exited`, so the reply would never
    /// come — `recv_timeout` turns that into a fast empty result instead of a
    /// hang).
    pub fn dump_text(&self, lines: usize) -> Vec<String> {
        let (tx, rx) = channel();
        if self
            .tx
            .send(Control::DumpText {
                lines,
                reply: TextReply(tx),
            })
            .is_err()
        {
            return Vec::new();
        }
        rx.recv_timeout(Duration::from_secs(2)).unwrap_or_default()
    }

    /// Current terminal modes (alt screen, kitty flags) — debug surface only.
    /// Same round-trip shape as [`TermHandle::dump_text`]; `None` when the
    /// actor is gone or slow.
    pub fn modes(&self) -> Option<ModeDump> {
        let (tx, rx) = channel();
        if self.tx.send(Control::GetModes(ModesReply(tx))).is_err() {
            return None;
        }
        rx.recv_timeout(Duration::from_secs(2)).ok()
    }

    /// Set (Some) or clear (None) the regex search. Takes effect on the next
    /// frame, which is forced to Full so matches repaint everywhere.
    pub fn search(&self, regex: Option<String>) {
        let _ = self.tx.send(Control::Search(regex));
    }

    /// Live-toggle the synthetic prompt marks on this terminal. The
    /// settings pane broadcasts to every open actor: "a toggle must not
    /// require restarting terminals".
    pub fn set_synthetic_marks(&self, on: bool) {
        let _ = self.tx.send(Control::SetSyntheticMarks(on));
    }

    /// Live-set the alt-screen wheel multiplier (1..=6). Same broadcast path
    /// as [`TermHandle::set_synthetic_marks`].
    pub fn set_wheel_speed(&self, speed: u8) {
        let _ = self.tx.send(Control::SetWheelSpeed(speed));
    }

    /// Move the viewport to the next/previous match (relative to the current
    /// viewport), centered-ish. Wraps around the whole grid; no-op when no
    /// matches exist. Forces a Full frame so the fresh matches repaint.
    pub fn search_nav(&self, dir: SearchNavDir) {
        let _ = self.tx.send(Control::SearchNav(dir));
    }

    /// The embedder consumed the frame with this sequence number; the next
    /// pending damage may now be emitted.
    pub fn ack(&self, seq: u32) {
        let _ = self.tx.send(Control::Ack(seq));
    }

    /// Synchronous: the actor's current [`FrameStats`] (debug HUD input).
    /// Falls back to all-zero stats if the actor is already gone. Uses
    /// `recv_timeout` for the same reason `dump_text` does: the caller is a
    /// synchronous Tauri command on the main thread, and a reply that never
    /// comes (actor ended after `Exited`) must degrade, not hang the UI.
    pub fn stats(&self) -> FrameStats {
        let (tx, rx) = channel();
        if self.tx.send(Control::GetStats(StatsReply(tx))).is_err() {
            return FrameStats::default();
        }
        rx.recv_timeout(Duration::from_secs(2)).unwrap_or_default()
    }

    /// OS pid of the pty child (the stats root), `None` for a no-pty
    /// actor. Cheap and lock-free — the value was captured at spawn.
    pub fn child_pid(&self) -> Option<u32> {
        self.pid
    }

    /// Force the next frame to be Full (seq gap, context loss, panel reveal).
    pub fn request_full(&self) {
        let _ = self.tx.send(Control::RequestFull);
    }

    /// Inject bytes straight into the parser. For tests and embedders that
    /// source terminal bytes from somewhere other than the pty reader.
    pub fn feed(&self, bytes: &[u8]) {
        self.io.record_output(bytes.len());
        let _ = self.feed_tx.send(ByteMsg::Chunk(bytes.to_vec()));
    }

    /// Send a key event to the pty writer, then hard-kill the child and join
    /// the actor thread. The app-quit path: a straggler that ignores the
    /// normal close must not outlive the app ("hard-kill after 2s").
    pub fn kill(self) {
        let _ = self.tx.send(Control::Kill);
        drop(self.tx);
        drop(self.feed_tx);
        if let Some(join) = self.join.lock().unwrap().take() {
            if let Err(panic) = join.join() {
                std::panic::resume_unwind(panic);
            }
        }
    }

    /// Shut the child down, join the actor thread. Re-raises an actor panic (a
    /// panicking engine is a bug the embedder should see).
    pub fn shutdown(self) {
        let _ = self.tx.send(Control::Shutdown);
        drop(self.tx);
        drop(self.feed_tx);
        if let Some(join) = self.join.lock().unwrap().take() {
            if let Err(panic) = join.join() {
                std::panic::resume_unwind(panic);
            }
        }
    }
}

/// Control-channel messages (the only way to touch `Term` from outside).
enum Control {
    Resize { cols: u16, rows: u16 },
    /// PTY-level winsize wiggle; the grid is untouched.
    Poke,
    Scroll(i32),
    SetDisplayOffset(usize),
    Selection(SelectionOp),
    CopySelection(SyncReply),
    DumpText { lines: usize, reply: TextReply },
    Search(Option<String>),
    SearchNav(SearchNavDir),
    SetSyntheticMarks(bool),
    SetWheelSpeed(u8),
    GetModes(ModesReply),
    Ack(u32),
    GetStats(StatsReply),
    RequestFull,
    WriteKey(KeyEvent),
    Paste(String),
    Mouse(MouseEvent),
    Kill,
    Shutdown,
    /// A no-pty actor's external child exited.
    ChildExited(ExitStatus),
}

/// One-shot reply channel for `copy_selection`.
struct SyncReply(Sender<Option<String>>);

/// One-shot reply channel for `dump_text`.
struct TextReply(Sender<Vec<String>>);

/// Point-in-time terminal-mode dump for the debug surface (marks
/// forensics): which screen is active and what kitty flags are set — the two
/// mode bits that gate/alter input-side behavior an HTTP probe can't
/// otherwise see.
#[derive(Debug, Clone, Copy)]
pub struct ModeDump {
    pub alt_screen: bool,
    pub kitty_flags: u32,
}

/// One-shot reply channel for `modes`.
struct ModesReply(Sender<ModeDump>);

/// One-shot reply channel for `stats`.
struct StatsReply(Sender<FrameStats>);

/// Byte-channel messages: either a pty chunk or the reader-EOF marker.
enum ByteMsg {
    Chunk(Vec<u8>),
    Eof,
}

/// Spawn a real actor: launches `cfg.spec` in a pty, starts the reader
/// thread, and returns a handle. Spawn failures propagate loudly.
pub fn spawn_actor(cfg: ActorConfig, events: Sender<TermEvent>) -> pty::Result<TermHandle> {
    let spec = cfg.spec.clone();
    let mut session = PtySession::spawn(&spec)?;
    let pid = session.process_id();
    let reader = session.take_reader()?;
    let writer = Arc::new(Mutex::new(session.writer()?));

    let io = Arc::new(IoCounters::new(true));
    let (byte_tx, byte_rx) = channel::<ByteMsg>();
    spawn_reader(reader, byte_tx.clone(), io.clone());

    let (ctrl_tx, ctrl_rx) = channel::<Control>();
    let handle = TermHandle {
        tx: ctrl_tx,
        feed_tx: byte_tx,
        writer: writer.clone(),
        join: Arc::new(Mutex::new(None)),
        io,
        pid,
    };

    spawn_actor_thread(
        cfg,
        Some(session),
        events,
        ctrl_rx,
        byte_rx,
        writer,
        &handle,
    );
    Ok(handle)
}

/// Actor without a child process: bytes are injected via
/// [`TermHandle::feed`], and anything the terminal writes back (DSR replies,
/// DA) lands in the returned sink. Used by the headless test suite and by
/// embedders that drive the parser from their own byte source.
pub fn spawn_actor_nopty(
    cfg: ActorConfig,
    events: Sender<TermEvent>,
) -> (TermHandle, Arc<Mutex<Vec<u8>>>) {
    let sink = Arc::new(Mutex::new(Vec::new()));
    let writer: Arc<Mutex<Box<dyn Write + Send>>> =
        Arc::new(Mutex::new(Box::new(SinkWriter(sink.clone()))));

    let (byte_tx, byte_rx) = channel::<ByteMsg>();
    let (ctrl_tx, ctrl_rx) = channel::<Control>();
    let handle = TermHandle {
        tx: ctrl_tx,
        feed_tx: byte_tx,
        writer: writer.clone(),
        join: Arc::new(Mutex::new(None)),
        // No child: "alive" means the actor thread is still running.
        io: Arc::new(IoCounters::new(true)),
        // No pty, no child: nothing for the stats poller to walk.
        pid: None,
    };

    spawn_actor_thread(cfg, None, events, ctrl_rx, byte_rx, writer, &handle);
    (handle, sink)
}

fn spawn_actor_thread(
    cfg: ActorConfig,
    session: Option<PtySession>,
    events: Sender<TermEvent>,
    ctrl_rx: Receiver<Control>,
    byte_rx: Receiver<ByteMsg>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    handle: &TermHandle,
) {
    let actor = Actor::new(
        cfg,
        session,
        events,
        ctrl_rx,
        byte_rx,
        writer,
        handle.io.clone(),
    );
    let join = std::thread::spawn(move || actor.run());
    *handle.join.lock().unwrap() = Some(join);
}

/// Reads the pty to EOF, forwarding chunks to the actor. The empty `Eof`
/// marker tells the actor the child is gone and its output is drained.
fn spawn_reader(
    mut reader: Box<dyn std::io::Read + Send>,
    tx: Sender<ByteMsg>,
    io: Arc<IoCounters>,
) {
    std::thread::spawn(move || {
        let mut buf = [0u8; READ_BUF];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => {
                    let _ = tx.send(ByteMsg::Eof);
                    break;
                }
                Ok(n) => {
                    // Byte-flow liveness: counted HERE, before parsing, so a
                    // status probe sees activity even while the parser is
                    // busy ("actor byte flow, NOT render").
                    io.record_output(n);
                    if tx.send(ByteMsg::Chunk(buf[..n].to_vec())).is_err() {
                        break;
                    }
                }
                Err(_) => {
                    let _ = tx.send(ByteMsg::Eof);
                    break;
                }
            }
        }
    });
}

/// A `Write` that appends into a shared `Vec<u8>` (no-pty actor sink).
struct SinkWriter(Arc<Mutex<Vec<u8>>>);
impl Write for SinkWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Hyperlink id assignment, kept separate from `Term` so row building can
/// borrow them disjointly.
#[derive(Default)]
struct LinkState {
    ids: HashMap<String, u16>,
    next: u16,
    new: Vec<(u16, String)>,
}

/// Acknowledge frame `acked` against the actor's emission state. Runs in the
/// wrapping u32 sequence space: `build_frame` increments `seq` with
/// `wrapping_add`, so a plain `<`/`>=` comparison would read a fresh post-wrap
/// ack (seq 0 after u32::MAX) as stale and stall the ack gate forever. The
/// subtraction wraps in the correct direction: `0 - u32::MAX == 1`, so the
/// post-wrap ack reads as newer. Assumes the two seqs are within 2^31 steps
/// (true here — seqs only advance on emission).
/// Strip trailing whitespace from EVERY line, keeping the line structure
/// (copy path). `selection_to_string` joins rows with `\n`, so a
/// plain split/`trim_end`/rejoin is exact; a wholly blank selection collapses
/// to the empty string, which is what the `skip_whitespace_only` gate in the
/// `copy_selection` command tests for.
fn rtrim_lines(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(line.trim_end());
    }
    out
}

fn apply_ack(acked: u32, last_acked: &mut u32, emitted_seq: u32, outstanding: &mut bool) {
    if acked.wrapping_sub(*last_acked) as i32 > 0 {
        *last_acked = acked;
        if acked == emitted_seq || acked.wrapping_sub(emitted_seq) as i32 > 0 {
            *outstanding = false;
        }
    }
}

impl LinkState {
    fn id_for(&mut self, uri: &str) -> u16 {
        if let Some(id) = self.ids.get(uri) {
            return *id;
        }
        self.next = self.next.wrapping_add(1);
        let id = self.next;
        self.ids.insert(uri.to_owned(), id);
        self.new.push((id, uri.to_owned()));
        id
    }
}

/// The listener handed to `Term`; forwards interesting `Event`s outward and
/// loops `Event::PtyWrite` back into the pty writer.
struct ActorListener {
    events: Sender<TermEvent>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
}

impl EventListener for ActorListener {
    fn send_event(&self, event: Event) {
        match event {
            Event::Title(title) => {
                let _ = self.events.send(TermEvent::Title(title));
            }
            Event::ResetTitle => {
                let _ = self.events.send(TermEvent::Title(String::new()));
            }
            Event::Bell => {
                let _ = self.events.send(TermEvent::Bell);
            }
            Event::PtyWrite(text) => {
                if let Ok(mut writer) = self.writer.lock() {
                    let _ = writer.write_all(text.as_bytes());
                    let _ = writer.flush();
                }
            }
            Event::ClipboardStore(_, text) => {
                let _ = self.events.send(TermEvent::Clipboard(text));
            }
            // ClipboardLoad / ColorRequest / TextAreaSizeRequest / ChildExit
            // are the embedder's (or a wider harness's) job, not the actor's.
            _ => {}
        }
    }
}

/// The per-terminal state, owned exclusively by the actor thread.
struct Actor {
    term: Term<ActorListener>,
    processor: Processor<StdSyncHandler>,
    scanner: OscScanner,
    session: Option<PtySession>,
    ctrl_rx: Receiver<Control>,
    byte_rx: Receiver<ByteMsg>,
    events: Sender<TermEvent>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,

    seq: u32,
    emitted_seq: u32,
    last_acked_seq: u32,
    frame_outstanding: bool,
    force_full: bool,
    /// Set whenever anything display-relevant happens (bytes parsed, control
    /// commands). `Term::damage()` always marks the current cursor cell, so
    /// it can't be used as the emission trigger on its own — without this
    /// flag every ack would emit a spurious cursor-only frame.
    dirty: bool,
    stats: FrameStats,

    search: Option<RegexSearch>,
    /// The match the last `search_nav` selected — the anchor the next nav
    /// steps from. Without it the anchor re-derives from the viewport center
    /// every press, and a match that cannot be centered (bottom half of the
    /// buffer: the offset clamps at 0) is re-selected forever, wedging the
    /// walk before it ever wraps (host-run bug). Cleared when the
    /// pattern changes or the user scrolls/resizes, so the anchor re-derives
    /// from the fresh viewport.
    search_hit: Option<Match>,
    links: LinkState,

    /// Kitty keyboard-protocol flag stack (minimal scope): current
    /// flags + the saved levels. Only bit 0b1 (disambiguate) is ever set —
    /// pushes are masked, the query reply can therefore never advertise the
    /// other flags. Cleared on RIS; depth-capped per the kitty spec.
    kitty_flags: u32,
    kitty_stack: Vec<u32>,

    /// Settings, seeded from `ActorConfig` and live-updated by
    /// `Control::SetSyntheticMarks` / `Control::SetWheelSpeed`.
    synthetic_prompt_marks: bool,
    wheel_speed: u8,

    child_exited: Option<ExitStatus>,
    eof_received: bool,
    exit_deadline: Option<Instant>,
    shutting_down: bool,
    /// Shared with the handle; this side only flips `child_alive`.
    io: Arc<IoCounters>,
}

impl Actor {
    fn new(
        cfg: ActorConfig,
        session: Option<PtySession>,
        events: Sender<TermEvent>,
        ctrl_rx: Receiver<Control>,
        byte_rx: Receiver<ByteMsg>,
        writer: Arc<Mutex<Box<dyn Write + Send>>>,
        io: Arc<IoCounters>,
    ) -> Self {
        let listener = ActorListener {
            events: events.clone(),
            writer: writer.clone(),
        };
        let size = TermSize::new(cfg.spec.cols as usize, cfg.spec.rows as usize);
        let config = Config {
            scrolling_history: cfg.scrollback_lines,
            ..Default::default()
        };
        let term = Term::new(config, &size, listener);

        Self {
            term,
            processor: Processor::new(),
            scanner: OscScanner::new(),
            session,
            ctrl_rx,
            byte_rx,
            events,
            writer,
            seq: 0,
            emitted_seq: 0,
            last_acked_seq: 0,
            frame_outstanding: false,
            force_full: true,
            dirty: true,
            stats: FrameStats::default(),
            search: None,
            search_hit: None,
            links: LinkState::default(),
            kitty_flags: 0,
            kitty_stack: Vec::new(),
            synthetic_prompt_marks: cfg.synthetic_prompt_marks,
            wheel_speed: cfg.wheel_speed,
            child_exited: None,
            eof_received: false,
            exit_deadline: None,
            shutting_down: false,
            io,
        }
    }

    fn run(mut self) {
        while !self.shutting_down {
            // Parse whatever arrived before we block on the control channel.
            self.drain_bytes();

            match self.ctrl_rx.recv_timeout(TICK) {
                Ok(control) => self.on_control(control),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    // Every handle was dropped: shut the child down.
                    self.shutting_down = true;
                }
            }

            // Bytes that arrived while we were blocked on control.
            self.drain_bytes();
            self.poll_child_exit();

            if self.shutting_down {
                break;
            }

            // Child gone and output flushed (or we gave up waiting on EOF):
            // paint the final screen and report the exit.
            if self.child_exited.is_some() && self.output_flushed() {
                self.finish_exit();
                break;
            }

            self.maybe_emit_frame();
        }

        // The actor thread is ending: whatever the reason (exit observed,
        // shutdown, kill), nothing is alive behind this handle any more.
        self.io.child_alive.store(false, Ordering::Relaxed);
        // Dropping `self` drops `session`, whose Drop kills and reaps the
        // child if it is still running (see pty.rs).
    }

    fn drain_bytes(&mut self) {
        while let Ok(msg) = self.byte_rx.try_recv() {
            match msg {
                ByteMsg::Chunk(bytes) => self.on_bytes(&bytes),
                ByteMsg::Eof => self.eof_received = true,
            }
        }
    }

    fn on_bytes(&mut self, bytes: &[u8]) {
        self.dirty = true;
        // Mechanism: pre-scan for OSC 9/99/777/133 (alacritty drops
        // them) *before* handing the same bytes to the parser.
        let events = self.scanner.feed(bytes);

        // Kitty replies must be written BEFORE the parser advances: alacritty
        // answers DA1 (`CSI c`) during `advance`, and the kitty detection
        // pattern (query, then DA1, "no kitty reply before the DA1 answer ⇒
        // unsupported") would always see our reply lose that race — Claude
        // Code fell back to plain Enter for Shift+Enter when run on the host.
        // Kitty handling touches only our own flag stack, never `Term`,
        // so pre-advance processing is safe.
        for event in &events {
            if let OscEvent::Kitty(kitty) = event {
                self.on_kitty(*kitty);
            }
        }

        self.processor.advance(&mut self.term, bytes);

        for event in events {
            match event {
                OscEvent::Notify { title, body } => {
                    let _ = self.events.send(TermEvent::Notify {
                        title: title.unwrap_or_default(),
                        body,
                    });
                }
                OscEvent::PromptMark(kind) => {
                    let kind = match kind {
                        PromptMarkKind::PromptStart => MarkKind::PromptStart,
                        PromptMarkKind::PromptEnd => MarkKind::PromptEnd,
                        PromptMarkKind::CommandStart => MarkKind::CommandStart,
                        PromptMarkKind::CommandEnd => MarkKind::CommandEnd,
                    };
                    // Buffer-absolute row: the line's index from the TOP of
                    // scrollback (history above the screen + cursor row), the
                    // one coordinate that stays put as output scrolls. The
                    // raw cursor line (viewport-relative) pinned every mark
                    // to ~the prompt row and mark nav went nowhere (host-run
                    // bug). Once the scrollback cap starts dropping
                    // top lines these indices drift; such marks are ancient
                    // and their jumps clamp to the buffer top.
                    let row = self.term.grid().history_size() as i64
                        + self.term.grid().cursor.point.line.0 as i64;
                    let _ = self.events.send(TermEvent::PromptMark { kind, row });
                }
                OscEvent::Kitty(_) => {} // handled pre-advance above
            }
        }
    }

    /// The kitty keyboard-protocol flag stack (minimal scope). Pushes
    /// are masked to the single flag we honour (0b1); the query reply is the
    /// masked current flags so it can never advertise key-release/alternate-
    /// key/associated-text support. The stack is per-terminal, cleared on RIS,
    /// depth-capped per the kitty spec.
    fn on_kitty(&mut self, event: KittyEvent) {
        match event {
            KittyEvent::Push(flags) => {
                // Depth-capped per the kitty spec: a full stack ignores pushes.
                if self.kitty_stack.len() >= KITTY_STACK_MAX {
                    return;
                }
                self.kitty_stack.push(self.kitty_flags);
                self.kitty_flags |= flags & KITTY_SUPPORTED_FLAGS;
            }
            KittyEvent::Pop(n) => {
                for _ in 0..n.min(KITTY_STACK_MAX as u32) {
                    match self.kitty_stack.pop() {
                        Some(prev) => self.kitty_flags = prev,
                        None => break,
                    }
                }
            }
            KittyEvent::Set(flags, mode) => {
                // `CSI = flags ; mode u` — direct set of the current entry,
                // no stack traffic. Only the supported bits ever change.
                let flags = flags & KITTY_SUPPORTED_FLAGS;
                match mode {
                    2 => self.kitty_flags |= flags,
                    3 => self.kitty_flags &= !flags,
                    // Mode 1 (the parser admits nothing else): replace the
                    // supported bits with the given values.
                    _ => {
                        self.kitty_flags =
                            (self.kitty_flags & !KITTY_SUPPORTED_FLAGS) | flags;
                    }
                }
            }
            KittyEvent::Query => {
                // Report the current flags OR our baseline support, masked to
                // what we actually honour — a fresh program querying before
                // any push must still learn that disambiguate is available.
                let reply = self.kitty_flags | KITTY_SUPPORTED_FLAGS;
                self.write_pty(format!("\x1b[?{reply}u").as_bytes());
            }
            KittyEvent::Reset => {
                self.kitty_stack.clear();
                self.kitty_flags = 0;
            }
        }
    }

    fn on_control(&mut self, control: Control) {
        match control {
            Control::Resize { cols, rows } => {
                // Only resize when the dimensions actually change: a no-op
                // resize would force a Full (and, per PLAN, resizes always
                // do) for nothing. Resizes always emit a Full frame.
                if cols as usize != self.term.columns() || rows as usize != self.term.screen_lines()
                {
                    if let Some(session) = &self.session {
                        let _ = session.resize(cols, rows);
                    }
                    self.term
                        .resize(TermSize::new(cols as usize, rows as usize));
                    self.force_full = true;
                    self.dirty = true;
                    // A resize can trim the scrollback / change the grid the
                    // matches are counted over — recount,
                    // and drop the nav anchor (its points just reshuffled).
                    self.search_hit = None;
                    self.emit_search_status();
                }
            }
            Control::Poke => {
                if let Some(session) = &self.session {
                    let cols = self.term.columns() as u16;
                    let rows = self.term.screen_lines() as u16;
                    let _ = session.resize(cols, rows.saturating_sub(1).max(1));
                    let _ = session.resize(cols, rows);
                }
            }
            Control::Scroll(delta) => {
                if delta != 0 {
                    self.term.scroll_display(Scroll::Delta(delta));
                    // A display_offset change always repaints the whole
                    // viewport (FULL-trigger matrix: case 3). The user moved
                    // the view — the next search_nav re-anchors on it.
                    self.search_hit = None;
                    self.force_full = true;
                    self.dirty = true;
                }
            }
            Control::SetDisplayOffset(offset) => {
                let current = self.term.grid().display_offset() as i32;
                let target = offset as i32;
                if target != current {
                    self.term.scroll_display(Scroll::Delta(target - current));
                    self.search_hit = None;
                    self.force_full = true;
                    self.dirty = true;
                }
            }
            Control::Selection(op) => self.apply_selection(op),
            Control::CopySelection(reply) => {
                // "rtrim trailing cell padding". A block/line
                // selection picks up each row's blank tail from the grid;
                // those spaces are padding, not content, and pasting them
                // back re-indents. Line structure is preserved.
                let text = self
                    .term
                    .selection_to_string()
                    .map(|text| rtrim_lines(&text));
                let _ = reply.0.send(text);
            }
            Control::DumpText { lines, reply } => {
                let _ = reply.0.send(self.dump_text(lines));
            }
            Control::Search(pattern) => {
                self.search = pattern.as_deref().and_then(|re| RegexSearch::new(re).ok());
                self.search_hit = None;
                self.force_full = true;
                self.dirty = true;
                self.emit_search_status();
            }
            Control::SearchNav(dir) => {
                // Move the viewport so the next/previous match is centered,
                // wrapping around the whole grid. No-op when there is no
                // active search or no matches at all. The anchor is the last
                // selected match when one is held (see `search_hit`) — the
                // centering clamp means "where the viewport is" and "which
                // match is selected" can disagree, so the selection must not
                // be re-derived from the viewport between presses.
                let target = match &mut self.search {
                    Some(regex) => {
                        search_nav_target(&self.term, regex, dir, self.search_hit.as_ref())
                    }
                    None => None,
                };
                if let Some(matched) = target {
                    let target_line = matched.start().line.0;
                    self.search_hit = Some(matched);
                    let history = self.term.grid().history_size() as i32;
                    let rows = self.term.screen_lines() as i32;
                    let offset = (rows / 2 - target_line).clamp(0, history);
                    let current = self.term.grid().display_offset() as i32;
                    if offset != current {
                        self.term.scroll_display(Scroll::Delta(offset - current));
                        self.force_full = true;
                        self.dirty = true;
                    }
                }
            }
            // Live settings broadcast. Both are plain state swaps —
            // nothing to repaint, nothing to force-full.
            Control::SetSyntheticMarks(on) => self.synthetic_prompt_marks = on,
            Control::SetWheelSpeed(speed) => self.wheel_speed = speed,
            Control::GetModes(reply) => {
                let snapshot = self.mode_snapshot();
                let _ = reply.0.send(ModeDump {
                    alt_screen: snapshot.alt_screen,
                    kitty_flags: self.kitty_flags,
                });
            }
            Control::Ack(seq) => apply_ack(
                seq,
                &mut self.last_acked_seq,
                self.emitted_seq,
                &mut self.frame_outstanding,
            ),
            Control::GetStats(reply) => {
                // `outstanding` mirrors `frame_outstanding`; refresh it at
                // query time so the reply is always current regardless of
                // where the gate last changed.
                self.stats.outstanding = self.frame_outstanding;
                let _ = reply.0.send(self.stats);
            }
            Control::RequestFull => {
                // The requester asks for a full precisely when its state is
                // broken (decode failure, visibility regain) — it may never
                // ack the frame currently in flight, so the outstanding gate
                // must not hold the resync hostage.
                self.frame_outstanding = false;
                self.force_full = true;
                self.dirty = true;
                self.emit_search_status();
            }
            Control::WriteKey(ev) => {
                let snapshot = self.mode_snapshot();
                if let Some(bytes) = encode_key(&ev, &snapshot) {
                    // Synthetic prompt marks (opt-in, default off).
                    // Rule: "When `synthetic_prompt_marks` is ON, the
                    // ACTOR records a synthetic mark on every UNMODIFIED
                    // Enter `WriteKey` while the terminal is NOT in the alt
                    // screen: emit the existing `TermEvent::PromptMark
                    // { PromptStart, row }` with the buffer-absolute row
                    // (history + cursor), same as the scanner path. No new
                    // event shapes." Modified Enters are excluded:
                    // "Shift+Enter is the in-composer
                    // newline — marking it would plant a mark per
                    // continuation line of a single prompt."
                    //
                    // COORDINATE SPACE: `row` is BUFFER-ABSOLUTE — the
                    // line's index from the TOP of scrollback
                    // (history_size + cursor.point.line), identical to the
                    // OSC 133 scanner path in `on_bytes`. It is computed
                    // BEFORE `write_pty` so the mark lands on the line the
                    // user pressed Enter on, not on whatever the shell
                    // echoes back afterwards.
                    if self.synthetic_prompt_marks
                        && ev.key == Key::Enter
                        && ev.mods.is_empty()
                        && !snapshot.alt_screen
                    {
                        let row = self.term.grid().history_size() as i64
                            + self.term.grid().cursor.point.line.0 as i64;
                        let _ = self.events.send(TermEvent::PromptMark {
                            kind: MarkKind::PromptStart,
                            row,
                        });
                    }
                    self.write_pty(&bytes);
                    self.scroll_to_bottom_on_input();
                }
            }
            Control::Paste(text) => {
                let snapshot = self.mode_snapshot();
                self.write_pty(&encode_paste(&text, &snapshot));
                self.scroll_to_bottom_on_input();
            }
            Control::Mouse(ev) => {
                let snapshot = self.mode_snapshot();
                if let Some(bytes) = encode_mouse(&ev, &snapshot) {
                    self.write_pty(&bytes);
                }
            }
            Control::Shutdown => self.shutting_down = true,
            Control::ChildExited(status) => {
                if self.child_exited.is_none() {
                    self.child_exited = Some(status);
                    self.io.child_alive.store(false, Ordering::Relaxed);
                    // No pty reader will ever send Eof: the output is flushed
                    // by definition (the embedder reports the exit only after
                    // draining its own pipe).
                    self.eof_received = true;
                }
            }
            Control::Kill => {
                // Hard-kill the child and exit immediately: the app-quit
                // "straggler" path. Dropping the session at thread end also
                // reaps it, so no zombie is left behind.
                if let Some(session) = &mut self.session {
                    let _ = session.kill();
                }
                self.shutting_down = true;
            }
        }
    }

    /// Read the terminal modes the encoders key off. Must run on the
    /// actor thread: `Term` has no lock and its modes are actor-owned.
    fn mode_snapshot(&self) -> ModeSnapshot {
        let mode = self.term.mode();
        let mouse = if mode.contains(TermMode::MOUSE_MOTION) {
            MouseProto::AnyMotion
        } else if mode.contains(TermMode::MOUSE_DRAG) {
            MouseProto::ButtonDrag
        } else if mode.contains(TermMode::MOUSE_REPORT_CLICK) {
            MouseProto::Normal
        } else {
            MouseProto::None
        };
        ModeSnapshot {
            app_cursor: mode.contains(TermMode::APP_CURSOR),
            app_keypad: mode.contains(TermMode::APP_KEYPAD),
            bracketed_paste: mode.contains(TermMode::BRACKETED_PASTE),
            mouse,
            mouse_sgr: mode.contains(TermMode::SGR_MOUSE),
            alt_screen: mode.contains(TermMode::ALT_SCREEN),
            // Our own kitty flag stack, not alacritty's TermMode:
            // alacritty only tracks the mode when its `kitty_keyboard` config
            // is on (we keep it off) and its pushes are unmasked.
            kitty_disambiguate: self.kitty_flags & KITTY_SUPPORTED_FLAGS != 0,
            // The wheel multiplier is Rust-side, and the ONLY path
            // that reads it is `keys::alt_screen_wheel` (TUI wheel→arrows).
            wheel_speed: self.wheel_speed,
        }
    }

    /// Recount the matches across the whole grid and emit a `SearchStatus`
    /// event. Called on new search / resize / request_full only — the count
    /// is a scan-time snapshot and deliberately not refreshed every frame.
    fn emit_search_status(&mut self) {
        if self.search.is_none() {
            return;
        }
        let total = count_search_matches(&self.term, self.search.as_mut().unwrap());
        let _ = self.events.send(TermEvent::SearchStatus { total });
    }

    /// Typed or pasted input jumps the view back to the live screen —
    /// standard terminal behavior, you must see what you type (a host-run
    /// nit). Mouse events and protocol replies (kitty query, DA/DSR)
    /// deliberately do NOT scroll. Also drops the search-nav anchor, like
    /// every other user-driven scroll.
    fn scroll_to_bottom_on_input(&mut self) {
        if self.term.grid().display_offset() != 0 {
            self.term.scroll_display(Scroll::Bottom);
            self.search_hit = None;
            self.force_full = true;
            self.dirty = true;
        }
    }

    /// Encoded input bytes hit the same writer the listener uses, so replies
    /// and user input stay in one ordered stream.
    fn write_pty(&self, bytes: &[u8]) {
        if let Ok(mut writer) = self.writer.lock() {
            let _ = writer.write_all(bytes);
            let _ = writer.flush();
        }
    }

    fn apply_selection(&mut self, op: SelectionOp) {
        match op {
            SelectionOp::Clear => {
                self.term.selection = None;
            }
            SelectionOp::Start { point, kind } => {
                let kind = match kind {
                    SelectionKind::Simple => SelectionType::Simple,
                    SelectionKind::Block => SelectionType::Block,
                    SelectionKind::Lines => SelectionType::Lines,
                    SelectionKind::Semantic => SelectionType::Semantic,
                };
                let point = self.to_grid_point(point);
                self.term.selection = Some(Selection::new(kind, point, Side::Left));
            }
            SelectionOp::Update { point, kind } => {
                let kind = match kind {
                    SelectionKind::Simple => SelectionType::Simple,
                    SelectionKind::Block => SelectionType::Block,
                    SelectionKind::Lines => SelectionType::Lines,
                    SelectionKind::Semantic => SelectionType::Semantic,
                };
                let point = self.to_grid_point(point);
                if let Some(selection) = &mut self.term.selection {
                    selection.ty = kind;
                    selection.update(point, Side::Right);
                } else {
                    self.term.selection = Some(Selection::new(kind, point, Side::Left));
                }
            }
        }
        // Selection state rides in the frame header; repaint everything so
        // the overlay appears/clears even without cell damage.
        self.force_full = true;
        self.dirty = true;
    }

    /// Viewport point (row 0 = top of screen) → grid line.
    fn to_grid_point(&self, point: Point) -> Point {
        let display_offset = self.term.grid().display_offset() as i32;
        Point::new(Line(point.line.0 - display_offset), point.column)
    }

    /// Serialize a bottom-anchored slice of the grid: `lines` scrollback rows
    /// plus the full current viewport, oldest first, trailing whitespace
    /// trimmed. Must run on the actor thread (reads `Term` directly). Empty
    /// rows are kept as empty strings so the harness sees the exact screen
    /// height.
    fn dump_text(&self, lines: usize) -> Vec<String> {
        let grid = self.term.grid();
        let screen_lines = self.term.screen_lines() as i64;
        let display_offset = grid.display_offset() as i64;
        let history_len = grid.history_size() as i64;
        let lines = (lines as i64).min(history_len);

        let bottom = screen_lines - 1 - display_offset;
        let top = (bottom - lines - screen_lines + 1).max(-history_len);
        let mut out = Vec::with_capacity((bottom - top + 1).max(0) as usize);
        for line in top..=bottom {
            out.push(row_to_text(&grid[Line(line as i32)]));
        }
        out
    }

    fn poll_child_exit(&mut self) {
        if self.child_exited.is_some() {
            return;
        }
        if let Some(session) = &mut self.session {
            if let Ok(Some(status)) = session.try_wait() {
                self.child_exited = Some(status);
                self.io.child_alive.store(false, Ordering::Relaxed);
                self.exit_deadline = Some(Instant::now() + EXIT_FLUSH_TIMEOUT);
            }
        }
    }

    fn output_flushed(&self) -> bool {
        self.eof_received
            || self
                .exit_deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
    }

    fn finish_exit(&mut self) {
        // A child killed mid-TUI never sends its mode resets, and the final
        // frame would export mouse_capture=true forever — the dead pane then
        // demands Shift-selection (PLAN bugs; 0.10.0: "exited
        // processes clear stale mouse and keyboard modes so stopped panes
        // remain selectable"). `Term`'s modes are parser-owned, so push the
        // reset sequences through it; the kitty stack is ours to drop.
        self.processor.advance(
            &mut self.term,
            b"\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?1006l\x1b[?1005l",
        );
        self.kitty_flags = 0;
        self.kitty_stack.clear();
        // One last full frame so the final screen state is painted, then the
        // exit status. The ack gate is opened deliberately: nothing further
        // will be emitted, and the frontend must see the final pixels even
        // if it never acked the previous frame.
        self.force_full = true;
        self.dirty = true;
        self.frame_outstanding = false;
        self.maybe_emit_frame();
        let status = self.child_exited.unwrap_or(ExitStatus {
            code: None,
            success: false,
        });
        let _ = self.events.send(TermEvent::Exited(status));
    }

    /// The frame emission rule: emit iff the terminal is
    /// dirty AND no un-acked frame outstanding. `dirty` is actor-owned:
    /// `Term::damage()` alone can't gate, because it always damages the
    /// current cursor cell and would fire on every idle tick.
    fn maybe_emit_frame(&mut self) {
        if self.frame_outstanding {
            // A frame is un-acked: any damage that piled up coalesces into
            // the post-ack emission. Count the withheld tick so the HUD can
            // show gate pressure (coalesced_ticks).
            if self.dirty {
                self.stats.coalesced_ticks += 1;
            }
            return;
        }
        if !self.dirty {
            return;
        }

        let mut damaged: Vec<LineDamageBounds> = Vec::new();
        let kind = match self.term.damage() {
            TermDamage::Full => FrameKind::Full,
            TermDamage::Partial(lines) => {
                let lines: Vec<_> = lines.collect();
                // In practice the cursor cell is always damaged, so this is
                // near-dead; keep dirty set and retry next tick regardless.
                if lines.is_empty() && !self.force_full {
                    return;
                }
                if self.force_full {
                    FrameKind::Full
                } else {
                    damaged = lines;
                    FrameKind::Delta
                }
            }
        };

        self.dirty = false;
        self.force_full = false;
        self.term.reset_damage();
        self.build_frame(kind, &damaged);
    }

    fn build_frame(&mut self, kind: FrameKind, damaged: &[LineDamageBounds]) {
        self.seq = self.seq.wrapping_add(1);
        let display_offset = self.term.grid().display_offset();
        let history_len = self.term.grid().history_size();

        let cursor = build_cursor(&self.term, display_offset);
        let selection = build_selection(&self.term, display_offset);
        // Active even when scrolled out of the viewport (selection above is
        // viewport-clipped): the copy gate must survive streaming output.
        let selection_active = self
            .term
            .selection
            .as_ref()
            .and_then(|s| s.to_range(&self.term))
            .is_some();
        // Wire flags bits 1/2: mouse capture + alt screen, exported from the
        // live mode snapshot so the frontend can suppress local selection/
        // scroll when a TUI owns the wheel.
        let mode = self.mode_snapshot();
        let mouse_capture = mode.mouse != MouseProto::None;
        let alt_screen = mode.alt_screen;
        let (rows, zerowidth) =
            build_rows(&self.term, &mut self.links, kind, damaged, display_offset);
        let search_matches = build_search_matches(&mut self.search, &self.term, display_offset);

        let frame = FrameData {
            seq: self.seq,
            kind,
            cursor,
            display_offset,
            history_len,
            selection,
            selection_active,
            mouse_capture,
            alt_screen,
            search_matches,
            rows,
            zerowidth,
        };
        // HUD counters: wire bytes via `encoded_len` (the same bytes
        // `encode_frame` produces), damage rows from the row patches actually
        // built this frame.
        self.stats.frames_sent += 1;
        self.stats.bytes_sent += encoded_len(&frame) as u64;
        self.stats.damage_rows_last = frame.rows.len() as u32;
        let _ = self.events.send(TermEvent::Frame(frame));
        self.emitted_seq = self.seq;
        self.frame_outstanding = true;

        if !self.links.new.is_empty() {
            let links = std::mem::take(&mut self.links.new);
            let _ = self.events.send(TermEvent::HyperlinkTable(links));
        }
    }
}

fn build_cursor(term: &Term<ActorListener>, display_offset: usize) -> CursorState {
    let point = term.grid().cursor.point;
    let row = point.line.0 + display_offset as i32;
    let screen_lines = term.screen_lines() as i32;
    let visible = term.mode().contains(TermMode::SHOW_CURSOR) && (0..screen_lines).contains(&row);
    CursorState {
        row: row.clamp(0, screen_lines.saturating_sub(1)) as u16,
        col: point.column.0 as u16,
        shape: term.cursor_style().shape,
        visible,
    }
}

fn build_selection(term: &Term<ActorListener>, display_offset: usize) -> Option<(Point, Point)> {
    let range = term.selection.as_ref()?.to_range(term)?;
    let start = alacritty_terminal::term::point_to_viewport(display_offset, range.start)?;
    let end = alacritty_terminal::term::point_to_viewport(display_offset, range.end)?;
    Some((
        Point::new(Line(start.line as i32), start.column),
        Point::new(Line(end.line as i32), end.column),
    ))
}

fn build_search_matches(
    search: &mut Option<RegexSearch>,
    term: &Term<ActorListener>,
    display_offset: usize,
) -> Vec<(Point, Point)> {
    let mut matches = Vec::new();
    let regex = match search {
        Some(regex) => regex,
        None => return matches,
    };

    let screen_lines = term.screen_lines() as i32;
    let start = Point::new(Line(-(display_offset as i32)), Column(0));
    let end_line = screen_lines - 1 - display_offset as i32;
    if end_line < start.line.0 {
        return matches;
    }
    let end = Point::new(Line(end_line), term.last_column());

    let mut origin = start;
    while let Some(range) = term.regex_search_right(regex, origin, end) {
        let start_point = *range.start();
        let end_point = *range.end();
        if let (Some(start_vp), Some(end_vp)) = (
            alacritty_terminal::term::point_to_viewport(display_offset, start_point),
            alacritty_terminal::term::point_to_viewport(display_offset, end_point),
        ) {
            matches.push((
                Point::new(Line(start_vp.line as i32), start_vp.column),
                Point::new(Line(end_vp.line as i32), end_vp.column),
            ));
        }
        // Same corner trap as `count_search_matches`: `Boundary::None` WRAPS
        // at the bottom-right cell, and a match ending exactly there (`.*`
        // with the viewport at the buffer bottom) would restart this scan
        // from the top of scrollback and grow `matches` forever on the frame
        // path. Stop at the corner / any non-advancing step.
        if end_point >= end {
            break;
        }
        let next = end_point.add(term, Boundary::None, 1);
        if next <= end_point || next > end {
            break;
        }
        origin = next;
    }

    matches
}

/// The next/previous match relative to the anchor, searching the whole grid
/// with wrap-around (`Term::search_next` wraps internally, and the explicit
/// `Boundary::None` add/sub below wrap at the buffer corners — exactly the
/// nav semantic). The anchor is one cell past the last selected match when
/// the caller holds one; otherwise both directions anchor on the viewport's
/// CENTER line (kept inside grid bounds, so no out-of-range grid access):
/// `Next` takes the first match starting at-or-after the line just below
/// center, `Prev` the first match ending at-or-above the center. A search
/// whose only match is the current hit returns that hit again — jumping to
/// the sole match is the right answer there.
fn search_nav_target(
    term: &Term<ActorListener>,
    regex: &mut RegexSearch,
    dir: SearchNavDir,
    hit: Option<&Match>,
) -> Option<Match> {
    let display_offset = term.grid().display_offset() as i32;
    let screen_lines = term.screen_lines() as i32;
    let center = screen_lines / 2 - display_offset;
    let (origin, direction, side) = match dir {
        SearchNavDir::Next => {
            let origin = match hit {
                Some(m) => m.end().add(term, Boundary::None, 1),
                None => {
                    // One line below center, clamped to the last grid line.
                    let line = (center + 1).min(screen_lines - 1);
                    Point::new(Line(line), Column(0))
                }
            };
            (origin, Direction::Right, Side::Left)
        }
        SearchNavDir::Prev => {
            let origin = match hit {
                Some(m) => m.start().sub(term, Boundary::None, 1),
                None => Point::new(Line(center), Column(0)),
            };
            (origin, Direction::Left, Side::Right)
        }
    };
    term.search_next(regex, origin, direction, side, None)
}

/// Count every regex match across the whole grid (scrollback + viewport):
/// the `SearchStatus` total. `regex_search_right` includes its origin, so
/// the walk resumes just past each match's end; alacritty suppresses empty
/// matches. The subtle part is the resume step: `Point::add` with
/// `Boundary::None` WRAPS at the bottom-right corner cell, so a match ending
/// exactly there (`.*` on the bottom line did it on the host)
/// would restart the walk from the top of scrollback and spin the actor
/// thread forever — the corner and non-advancing guards below break instead.
/// Runs on new search / resize / request_full only — never on the frame path.
fn count_search_matches(term: &Term<ActorListener>, regex: &mut RegexSearch) -> usize {
    let mut count = 0;
    let start = Point::new(Line(term.grid().topmost_line().0), Column(0));
    let end = Point::new(Line(term.screen_lines() as i32 - 1), term.last_column());
    let mut origin = start;
    while let Some(range) = term.regex_search_right(regex, origin, end) {
        count += 1;
        if *range.end() >= end {
            break;
        }
        let next = range.end().add(term, Boundary::None, 1);
        if next <= *range.end() || next > end {
            break;
        }
        origin = next;
    }
    count
}

/// A zero-width combiner, (viewport row, column, characters).
pub type ZerowidthEntry = (u16, u16, Vec<char>);

fn build_rows(
    term: &Term<ActorListener>,
    links: &mut LinkState,
    kind: FrameKind,
    damaged: &[LineDamageBounds],
    display_offset: usize,
) -> (Vec<RowPatch>, Vec<ZerowidthEntry>) {
    let mut rows = Vec::new();
    let mut zerowidth = Vec::new();

    match kind {
        FrameKind::Full => {
            let screen_lines = term.screen_lines();
            let last_column = term.last_column().0;
            rows.reserve(screen_lines);
            for viewport_row in 0..screen_lines {
                let grid_line = Line(viewport_row as i32 - display_offset as i32);
                rows.push(build_row(
                    term,
                    links,
                    grid_line,
                    viewport_row,
                    0,
                    last_column,
                    &mut zerowidth,
                ));
            }
        }
        FrameKind::Delta => {
            rows.reserve(damaged.len());
            for damage in damaged {
                let viewport_row = damage.line;
                let grid_line = Line(viewport_row as i32 - display_offset as i32);
                rows.push(build_row(
                    term,
                    links,
                    grid_line,
                    viewport_row,
                    damage.left,
                    damage.right,
                    &mut zerowidth,
                ));
            }
        }
    }

    (rows, zerowidth)
}

#[allow(clippy::too_many_arguments)]
fn build_row(
    term: &Term<ActorListener>,
    links: &mut LinkState,
    grid_line: Line,
    viewport_row: usize,
    left: usize,
    right: usize,
    zerowidth: &mut Vec<ZerowidthEntry>,
) -> RowPatch {
    let row = &term.grid()[grid_line];
    let mut cells = Vec::with_capacity(right.saturating_sub(left) + 1);
    for col in left..=right {
        let tcell = &row[Column(col)];
        let uri = tcell
            .hyperlink()
            .map(|hyperlink| hyperlink.uri().to_owned());
        let link_id = uri.as_deref().map_or(0, |uri| links.id_for(uri));

        cells.push(Cell {
            ch: tcell.c,
            fg: color::resolve(tcell.fg, term.colors()),
            bg: color::resolve(tcell.bg, term.colors()),
            flags: CellFlags::from_alacritty(tcell.flags),
            link_id,
        });

        if let Some(zw) = tcell.zerowidth() {
            if !zw.is_empty() {
                zerowidth.push((viewport_row as u16, col as u16, zw.to_vec()));
            }
        }
    }
    RowPatch {
        row: viewport_row as u16,
        col_start: left as u16,
        cells,
    }
}

/// One grid row flattened to text with trailing whitespace removed. Terminal
/// rows are always padded to the full column count; the harness wants the
/// visible content only, so the trailing pad cells are trimmed.
fn row_to_text(row: &Row<alacritty_terminal::term::cell::Cell>) -> String {
    let mut text = String::with_capacity(row.len());
    for col in 0..row.len() {
        text.push(row[Column(col)].c);
    }
    let trimmed = text.trim_end().len();
    text.truncate(trimmed);
    text
}

#[cfg(test)]
mod tests {
    use super::apply_ack;

    /// `apply_ack` semantics at the u32 wrap boundary: the actor's seq is
    /// `wrapping_add`ed, so a fresh ack of seq 0 after u32::MAX must clear
    /// the gate, and a stale pre-wrap ack must not.
    #[test]
    fn apply_ack_wrap_boundary() {
        // Fresh post-wrap ack: last acked = u32::MAX, emitted wrapped to 0,
        // frontend acks 0 → the gate opens.
        let mut last_acked = u32::MAX;
        let mut outstanding = true;
        apply_ack(0, &mut last_acked, 0, &mut outstanding);
        assert!(!outstanding, "ack of wrapped seq 0 must clear the gate");
        assert_eq!(last_acked, 0);

        // Stale pre-wrap ack: emitted is 0 (post-wrap), ack of u32::MAX is
        // older than everything emitted since — must not clear.
        let mut last_acked = 0;
        let mut outstanding = true;
        apply_ack(u32::MAX, &mut last_acked, 0, &mut outstanding);
        assert!(outstanding, "stale pre-wrap ack must not clear");
        assert_eq!(last_acked, 0);

        // Two seqs at the very end of the space, one apart.
        let mut last_acked = u32::MAX - 1;
        let mut outstanding = true;
        apply_ack(u32::MAX, &mut last_acked, u32::MAX, &mut outstanding);
        assert!(!outstanding);
        assert_eq!(last_acked, u32::MAX);
    }

    #[test]
    fn apply_ack_ordering() {
        // Normal forward acks.
        let mut last_acked = 0;
        let mut outstanding = true;
        // An ack below the emitted seq but above last_acked is not stale in
        // itself, yet must not open the gate for a later un-acked frame.
        apply_ack(1, &mut last_acked, 5, &mut outstanding);
        assert!(outstanding);
        assert_eq!(last_acked, 1);
        apply_ack(5, &mut last_acked, 5, &mut outstanding);
        assert!(!outstanding);

        // Re-acking the same seq (frontend acks every painted frame) is a
        // no-op, not a regression.
        let mut last_acked = 5;
        let mut outstanding = false;
        apply_ack(5, &mut last_acked, 5, &mut outstanding);
        assert_eq!(last_acked, 5);
    }
}
