//! Headless TermActor tests. Most tests drive the parser
//! directly through a no-pty actor (`spawn_actor_nopty`) and assert on the
//! frames it emits; the last test exercises a real pty end to end.

use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use term_core::actor::{
    spawn_actor, spawn_actor_nopty, ActorConfig, Cell, CellFlags, CursorShape, FrameData,
    FrameKind, Point, SearchNavDir, SelectionKind, SelectionOp, TermEvent,
};

use term_core::color::Rgba;
use term_core::pty::{ExitStatus, PtySpec};

/// Build a config for a no-child actor of the given dimensions.
fn cfg(cols: u16, rows: u16) -> ActorConfig {
    ActorConfig {
        spec: PtySpec {
            command: String::new(),
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            cols,
            rows,
        },
        scrollback_lines: 10_000,
        // Settings at their defaults: synthetic marks OFF,
        // wheel speed 3. Tests that need them on build on top of this.
        ..ActorConfig::default()
    }
}

/// Start a no-pty actor and open its ack gate: the initial Full frame (forced
/// by construction) is consumed and acked so subsequent feeds emit
/// immediately instead of being gated.
fn test_actor(
    cols: u16,
    rows: u16,
) -> (
    term_core::actor::TermHandle,
    Receiver<TermEvent>,
    Arc<Mutex<Vec<u8>>>,
) {
    let (events, rx) = std::sync::mpsc::channel();
    let (handle, sink) = spawn_actor_nopty(cfg(cols, rows), events);
    let first = next_frame(&rx);
    handle.ack(first.seq);
    (handle, rx, sink)
}

/// Wait for the next Frame, skipping other event kinds.
fn next_frame(rx: &Receiver<TermEvent>) -> FrameData {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(TermEvent::Frame(frame)) => return frame,
            Ok(_) => continue,
            Err(err) => panic!("timed out waiting for a frame: {err}"),
        }
    }
}

/// Assert no Frame arrives within a short window (used to prove the ack gate
/// holds an emission back).
fn assert_no_frame(rx: &Receiver<TermEvent>) {
    let deadline = std::time::Instant::now() + Duration::from_millis(150);
    loop {
        match rx.recv_timeout(Duration::from_millis(10)) {
            Ok(TermEvent::Frame(frame)) => panic!("unexpected frame {frame:?}"),
            Ok(_) => continue,
            Err(_) if std::time::Instant::now() >= deadline => return,
            Err(_) => {}
        }
    }
}

fn find_cell(frame: &FrameData, ch: char) -> Option<&Cell> {
    frame
        .rows
        .iter()
        .flat_map(|row| row.cells.iter())
        .find(|cell| cell.ch == ch)
}

fn row_text(frame: &FrameData, row: u16) -> String {
    frame
        .rows
        .iter()
        .find(|patch| patch.row == row)
        .map(|patch| patch.cells.iter().map(|cell| cell.ch).collect())
        .unwrap_or_default()
}

fn frames_contain(frame: &FrameData, ch: char) -> bool {
    find_cell(frame, ch).is_some()
}

// ---- SGR / color resolution ----------------------------------------------

#[test]
fn sgr_colors_resolve_red_bold() {
    let (handle, rx, _sink) = test_actor(20, 5);
    handle.feed(b"\x1b[31;1mX\x1b[0m");

    let frame = next_frame(&rx);
    let cell = find_cell(&frame, 'X').expect("X cell");
    assert_eq!(
        cell.fg,
        Rgba::rgb(0xAC, 0x42, 0x42),
        "SGR 31 resolves to default red"
    );
    assert!(cell.flags.contains(CellFlags::BOLD), "SGR 1 sets BOLD");
}

#[test]
fn color256_and_truecolor_resolve() {
    let (handle, rx, _sink) = test_actor(20, 5);

    // 256-color: 196 = cube r5 g0 b0 → pure red.
    handle.feed(b"\x1b[38;5;196mA\x1b[0m");
    // Truecolor.
    handle.feed(b"\x1b[38;2;10;20;30mB\x1b[0m");

    let frame = next_frame(&rx);
    let a = find_cell(&frame, 'A').expect("256-color cell");
    assert_eq!(a.fg, Rgba::rgb(0xFF, 0x00, 0x00));
    let b = find_cell(&frame, 'B').expect("truecolor cell");
    assert_eq!(b.fg, Rgba::rgb(0x0A, 0x14, 0x1E));
}

#[test]
fn default_fg_bg_resolve() {
    let (handle, rx, _sink) = test_actor(20, 5);
    handle.feed(b"T");

    let frame = next_frame(&rx);
    let cell = find_cell(&frame, 'T').expect("cell");
    assert_eq!(cell.fg, Rgba::rgb(0xD8, 0xD8, 0xD8));
    assert_eq!(cell.bg, Rgba::rgb(0x18, 0x18, 0x18));
}

// ---- cursor movement + damage bounds --------------------------------------

#[test]
fn cursor_movement_and_damage_bounds() {
    let (handle, rx, _sink) = test_actor(20, 8);

    // Home: cursor (0,0), damages the row it lands on.
    handle.feed(b"\x1b[H");
    let home = next_frame(&rx);
    handle.ack(home.seq);
    assert_eq!(home.kind, FrameKind::Delta);

    // Write X at row 5, col 5 (1-based 6;6). Delta must cover exactly the
    // touched lines: the home row and the write row.
    handle.feed(b"\x1b[6;6HX");
    let frame = next_frame(&rx);
    handle.ack(frame.seq);

    assert_eq!(frame.kind, FrameKind::Delta);
    let mut rows: Vec<u16> = frame.rows.iter().map(|patch| patch.row).collect();
    rows.sort_unstable();
    // Home row (0) plus write row (5); nothing else.
    assert_eq!(rows, vec![0, 5]);

    let patch = frame.rows.iter().find(|p| p.row == 5).expect("row 5 patch");
    assert_eq!(patch.col_start, 5);
    assert_eq!(patch.cells[0].ch, 'X');
}

// ---- scroll region + wrap -------------------------------------------------

#[test]
fn scroll_region_grows_history() {
    let (handle, rx, _sink) = test_actor(10, 4);

    let mut out = String::new();
    for i in 0..10 {
        out.push_str(&format!("line{i}\r\n"));
    }
    handle.feed(out.as_bytes());

    let frame = next_frame(&rx);
    handle.ack(frame.seq);
    assert!(
        frame.history_len > 0,
        "scrollback must grow past the screen"
    );
    assert_eq!(frame.display_offset, 0);
}

#[test]
fn scrolling_returns_older_rows_in_full_frame() {
    let (handle, rx, _sink) = test_actor(10, 4);

    // One combined feed: the actor parses it in a single drain, so the
    // emitted frame is deterministic (10 lines + trailing blank cursor line
    // on a 4-row screen => 7 lines of history).
    let mut out = String::new();
    for i in 0..10 {
        out.push_str(&format!("line{i}\r\n"));
    }
    handle.feed(out.as_bytes());
    let filled = next_frame(&rx);
    handle.ack(filled.seq);
    assert!(
        filled.history_len >= 6,
        "10 lines on a 4-row screen => >= 6 history"
    );
    assert!(
        row_text(&filled, 0).starts_with("line7"),
        "row 0 shows line7, got {:?}",
        row_text(&filled, 0)
    );

    // Scrolling up by 2 must force a Full frame with a non-zero offset.
    handle.scroll(2);
    let frame = next_frame(&rx);
    handle.ack(frame.seq);
    assert_eq!(
        frame.kind,
        FrameKind::Full,
        "display_offset change forces Full"
    );
    assert_eq!(frame.display_offset, 2);

    // Two lines further into history than before the scroll.
    assert!(
        row_text(&frame, 0).starts_with("line5"),
        "row 0 shows line5, got {:?}",
        row_text(&frame, 0)
    );

    // Absolute scrollbar positioning lands on the same rows.
    handle.set_display_offset(3);
    let frame = next_frame(&rx);
    assert_eq!(frame.kind, FrameKind::Full);
    assert_eq!(frame.display_offset, 3);
    assert!(row_text(&frame, 0).starts_with("line4"));
}

// ---- wide chars + zero-width ----------------------------------------------

#[test]
fn wide_char_sets_wide_and_spacer() {
    let (handle, rx, _sink) = test_actor(20, 5);
    handle.feed("你ab".as_bytes());

    let frame = next_frame(&rx);
    let wide = find_cell(&frame, '你').expect("wide cell");
    assert!(
        wide.flags.contains(CellFlags::WIDE),
        "fullwidth char sets WIDE"
    );

    // The spacer cell follows the wide glyph.
    let patch = frame.rows.iter().find(|p| p.row == 0).expect("row 0");
    let spacer = &patch.cells[1];
    assert_eq!(spacer.ch, ' ');
    assert!(spacer.flags.contains(CellFlags::WIDE_SPACER));
}

#[test]
fn zero_width_combiner_lands_in_zerowidth_table() {
    let (handle, rx, _sink) = test_actor(20, 5);
    // e + combining acute accent (U+0301) → the accent is zero-width.
    handle.feed("e\u{301}".as_bytes());

    let frame = next_frame(&rx);
    let entry = frame
        .zerowidth
        .iter()
        .find(|(_, _, chars)| chars.contains(&'\u{301}'))
        .expect("combining accent must ride the zerowidth table");
    assert_eq!(entry.0, 0, "row 0");
    assert_eq!(entry.1, 0, "column 0 (the 'e')");
    assert_eq!(entry.2, vec!['\u{301}']);
}

// ---- ack gating -----------------------------------------------------------

#[test]
fn ack_gating_holds_emission_until_acked() {
    let (handle, rx, _sink) = test_actor(20, 5);

    // First write is emitted immediately.
    handle.feed(b"AAAA");
    let first = next_frame(&rx);
    assert!(frames_contain(&first, 'A'));

    // Second write while the first frame is outstanding: nothing is emitted.
    handle.feed(b"BBBB");
    assert_no_frame(&rx);

    // After the ack, the coalesced pending damage is delivered.
    handle.ack(first.seq);
    let second = next_frame(&rx);
    assert!(
        frames_contain(&second, 'B'),
        "pending damage must not be lost"
    );
}

#[test]
fn ack_gating_coalesces_multiple_writes() {
    let (handle, rx, _sink) = test_actor(20, 5);

    handle.feed(b"A");
    let first = next_frame(&rx);

    // Two writes while gated: both coalesce into one later frame.
    handle.feed(b"B");
    handle.feed(b"C");
    assert_no_frame(&rx);

    handle.ack(first.seq);
    let second = next_frame(&rx);
    assert!(frames_contain(&second, 'B'), "second frame carries B");
    assert!(frames_contain(&second, 'C'), "second frame carries C");
}

#[test]
fn stale_ack_is_ignored() {
    let (handle, rx, _sink) = test_actor(20, 5);

    handle.feed(b"A");
    let first = next_frame(&rx);
    handle.feed(b"B");
    assert_no_frame(&rx);

    // Ack a stale seq (below the emitted one): gate stays closed.
    handle.ack(first.seq.wrapping_sub(1));
    assert_no_frame(&rx);

    // Correct ack opens it.
    handle.ack(first.seq);
    let second = next_frame(&rx);
    assert!(frames_contain(&second, 'B'));
}

#[test]
fn request_full_bypasses_unacked_frame_gate() {
    // Regression (Phase-1 gate): the frontend requests a full exactly when it
    // FAILED to decode the outstanding frame — it can never ack it. A
    // request_full while a frame is un-acked must clear the gate and emit,
    // or the stream deadlocks forever.
    let (handle, rx, _sink) = test_actor(20, 5);

    handle.feed(b"Z");
    let first = next_frame(&rx);
    // No ack: the gate is closed on further damage…
    handle.feed(b"Q");
    assert_no_frame(&rx);

    // …but a request_full must still produce a Full frame.
    handle.request_full();
    std::thread::sleep(Duration::from_millis(30));
    let full = next_frame(&rx);
    assert_eq!(full.kind, FrameKind::Full);
    assert!(full.seq > first.seq);
    assert!(frames_contain(&full, 'Q'));
}

#[test]
fn request_full_forces_full_frame() {
    let (handle, rx, _sink) = test_actor(20, 5);

    handle.feed(b"Z");
    let delta = next_frame(&rx);
    assert_eq!(delta.kind, FrameKind::Delta);
    handle.ack(delta.seq);

    handle.request_full();
    // request_full alone forces a Full frame even without new damage; the
    // 30ms sleep is only to let the control message land.
    std::thread::sleep(Duration::from_millis(30));
    let full = next_frame(&rx);
    assert_eq!(full.kind, FrameKind::Full);
    assert!(frames_contain(&full, 'Z'));
}

// ---- FULL-trigger matrix -------------------------------

/// The five FULL triggers, each verified to yield a Full frame:
/// 1. first frame (forced by construction),
/// 2. resize,
/// 3. display_offset change (scroll / set_display_offset),
/// 4. request_full,
/// 5. ack-seq mismatch — the frontend reports it via request_full (PLAN:
///    "Frontend may request FULL anytime: seq gap, context loss, panel
///    reveal"), which is exactly the broken-state case from the regression
///    test above.
#[test]
fn full_trigger_matrix() {
    // Case 1: the very first frame is always Full.
    {
        let (events, rx) = std::sync::mpsc::channel();
        let (handle, _sink) = spawn_actor_nopty(cfg(20, 5), events);
        let first = next_frame(&rx);
        assert_eq!(first.kind, FrameKind::Full, "first frame must be Full");
        handle.ack(first.seq);
    }

    // Case 2: resize forces Full.
    {
        let (handle, rx, _sink) = test_actor(20, 5);
        handle.feed(b"x");
        let delta = next_frame(&rx);
        assert_eq!(delta.kind, FrameKind::Delta);
        handle.ack(delta.seq);

        handle.resize(10, 3);
        let full = next_frame(&rx);
        assert_eq!(full.kind, FrameKind::Full, "resize must force Full");
        assert!(full.seq > delta.seq);
    }

    // Case 3: display_offset change (scroll up) forces Full. The 20-row
    // buffer keeps the offset change meaningful without hitting history.
    {
        let (handle, rx, _sink) = test_actor(20, 5);
        let mut out = String::new();
        for i in 0..10 {
            out.push_str(&format!("line{i}\r\n"));
        }
        handle.feed(out.as_bytes());
        let frame = next_frame(&rx);
        handle.ack(frame.seq);
        assert!(frame.history_len > 0);

        handle.scroll(2);
        let full = next_frame(&rx);
        assert_eq!(
            full.kind,
            FrameKind::Full,
            "display_offset change must force Full"
        );
        assert_eq!(full.display_offset, 2);
        handle.ack(full.seq);

        handle.set_display_offset(4);
        let full = next_frame(&rx);
        assert_eq!(full.kind, FrameKind::Full, "absolute offset change -> Full");
        assert_eq!(full.display_offset, 4);
    }

    // Case 4: request_full forces Full even without any new damage.
    {
        let (handle, rx, _sink) = test_actor(20, 5);
        handle.feed(b"Z");
        let delta = next_frame(&rx);
        handle.ack(delta.seq);

        handle.request_full();
        std::thread::sleep(Duration::from_millis(30));
        let full = next_frame(&rx);
        assert_eq!(full.kind, FrameKind::Full, "request_full must force Full");
        assert!(full.seq > delta.seq);
    }

    // Case 5: ack-seq mismatch. The frontend decoded a frame out of the
    // expected sequence (or failed to decode it — same broken state, same
    // recovery): it can never ack the in-flight frame, and the resync FULL
    // must not be held hostage by the outstanding-ack gate.
    {
        let (handle, rx, _sink) = test_actor(20, 5);
        handle.feed(b"A");
        let first = next_frame(&rx);
        // No ack: gate closed, subsequent damage coalesces.
        handle.feed(b"B");
        assert_no_frame(&rx);

        // Frontend notices the seq it expected never arrived → request_full.
        handle.request_full();
        std::thread::sleep(Duration::from_millis(30));
        let full = next_frame(&rx);
        assert_eq!(
            full.kind,
            FrameKind::Full,
            "ack-seq mismatch resync -> Full"
        );
        assert!(full.seq > first.seq);
        assert!(
            frames_contain(&full, 'B'),
            "coalesced damage survives resync"
        );
    }
}

// ---- FrameStats ----------------------------------------

#[test]
fn frame_stats_counters_track_emission() {
    let (handle, rx, _sink) = test_actor(20, 5);

    // The initial Full (acked in test_actor) is frames_sent #1.
    let mut stats = handle.stats();
    assert_eq!(stats.frames_sent, 1);
    assert!(
        stats.bytes_sent > 0,
        "a 20x5 full frame must account wire bytes"
    );
    assert_eq!(stats.damage_rows_last, 5, "full frame covers all rows");
    assert!(!stats.outstanding, "gate open after the ack");
    assert_eq!(stats.coalesced_ticks, 0);

    // A delta: one more frame, one damaged row. LEFT un-acked so the gate
    // stays closed for the coalescing phase below.
    handle.feed(b"X");
    let delta = next_frame(&rx);
    stats = handle.stats();
    assert_eq!(stats.frames_sent, 2);
    assert_eq!(stats.damage_rows_last, 1, "single-row delta");
    assert!(stats.bytes_sent > 0);
    assert!(stats.outstanding, "un-acked delta reported outstanding");

    // Gate the stream: with the delta un-acked, new damage coalesces and the
    // counters reflect the pressure.
    handle.feed(b"Y");
    assert_no_frame(&rx);
    stats = handle.stats();
    assert!(
        stats.coalesced_ticks > 0,
        "withheld ticks must be counted (got {})",
        stats.coalesced_ticks
    );

    // Ack: the coalesced frame lands and the gate re-opens.
    let before = stats.frames_sent;
    handle.ack(delta.seq);
    let coalesced = next_frame(&rx);
    assert!(frames_contain(&coalesced, 'Y'));
    // Ack the coalesced frame too, then the gate is demonstrably open again.
    handle.ack(coalesced.seq);
    stats = handle.stats();
    assert_eq!(stats.frames_sent, before + 1);
    assert!(!stats.outstanding);
}

/// The ack gate must emit nothing while damage piles up across multiple
/// writes, then exactly ONE coalesced frame after the ack (the
/// hidden-panel story: a hidden webview acks nothing and the actor coalesces
/// all of it). Also asserted via FrameStats: coalesced_ticks grows while
/// gated, frames_sent stays put.
#[test]
fn unacked_actor_emits_nothing_then_one_frame_after_ack() {
    let (handle, rx, _sink) = test_actor(20, 5);

    handle.feed(b"AAAA");
    let first = next_frame(&rx);
    let sent_before = handle.stats().frames_sent;

    // A burst of writes while the first frame is un-acked.
    for i in 0..20 {
        handle.feed(format!("line{i}\r\n").as_bytes());
    }
    assert_no_frame(&rx);
    assert_eq!(
        handle.stats().frames_sent,
        sent_before,
        "no emission while gated"
    );

    // One frame after the ack carries everything.
    handle.ack(first.seq);
    let frame = next_frame(&rx);
    assert!(
        frames_contain(&frame, '9'),
        "coalesced content must survive (line9/line19 contain '9')"
    );
    assert_eq!(handle.stats().frames_sent, sent_before + 1);
    handle.shutdown();
}

// ---- PtyWrite loopback ----------------------------------------------------

#[test]
fn pty_write_loopback_answers_dsr() {
    let (handle, _rx, sink) = test_actor(20, 5);

    // Device Status Report: the terminal answers with a CPR escape.
    handle.feed(b"\x1b[6n");

    // The reply must be written to the pty writer (the loopback sink here).
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut found = false;
    while std::time::Instant::now() < deadline {
        let bytes = sink.lock().unwrap().clone();
        if bytes.windows(6).any(|w| w == b"\x1b[1;1R") {
            found = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(found, "DSR must be answered with a CPR reply on the writer");
}

// ---- selection + copy ------------------------------------------------------

#[test]
fn selection_and_copy_selection() {
    let (handle, rx, _sink) = test_actor(20, 5);
    handle.feed(b"hello world");
    let frame = next_frame(&rx);
    handle.ack(frame.seq);

    // Select columns 0..=4 of row 0 ("hello").
    handle.selection(SelectionOp::Start {
        point: Point::new(0.into(), 0.into()),
        kind: SelectionKind::Simple,
    });
    handle.selection(SelectionOp::Update {
        point: Point::new(0.into(), 4.into()),
        kind: SelectionKind::Simple,
    });

    let copied = handle.copy_selection();
    assert_eq!(copied.as_deref(), Some("hello"));

    // Clear: the frame header reflects no selection.
    handle.selection(SelectionOp::Clear);
    let frame = next_frame(&rx);
    assert_eq!(frame.selection, None);
}

// ---- text dump -------------------------------------------------------------

#[test]
fn dump_text_returns_last_n_history_plus_viewport() {
    let (handle, rx, _sink) = test_actor(10, 4);

    // 9 lines on a 4-row screen: 5 in history (line0..line4), the last 4
    // visible (line5..line8). No trailing newline so the last line stays put.
    let mut out = String::new();
    for i in 0..8 {
        out.push_str(&format!("line{i}\r\n"));
    }
    out.push_str("line8");
    handle.feed(out.as_bytes());
    let frame = next_frame(&rx);
    handle.ack(frame.seq);

    let lines = handle.dump_text(3);
    assert_eq!(
        lines,
        vec![
            "line2", "line3", "line4", // last 3 history lines
            "line5", "line6", "line7", "line8", // full viewport
        ]
    );
}

#[test]
fn dump_text_keeps_screen_height_and_trims_padding() {
    let (handle, rx, _sink) = test_actor(10, 4);
    handle.feed(b"hi");
    // Wait until the bytes are parsed (frame emitted) so the dump reads the
    // updated grid; the dump itself is a point-in-time snapshot.
    let frame = next_frame(&rx);
    handle.ack(frame.seq);

    // 0 requested history lines: exactly the 4 viewport rows, empty rows kept
    // as empty strings, "hi" row trimmed of its trailing pad cells.
    let lines = handle.dump_text(0);
    assert_eq!(lines.len(), 4);
    assert_eq!(lines[0], "hi");
    assert_eq!(lines[1], "");
}

// ---- regex search ----------------------------------------------------------
#[test]
fn search_reports_viewport_matches() {
    let (handle, rx, _sink) = test_actor(30, 5);
    handle.feed(b"the quick brown fox");
    let frame = next_frame(&rx);
    handle.ack(frame.seq);

    handle.search(Some("fox".into()));
    let frame = next_frame(&rx);
    handle.ack(frame.seq);
    assert_eq!(frame.kind, FrameKind::Full, "search forces a repaint");
    assert!(!frame.search_matches.is_empty(), "expected a match");

    let (start, end) = frame.search_matches[0];
    assert_eq!(start.column.0, 16, "fox starts at col 16");
    assert_eq!(end.column.0, 18, "fox ends at col 18");

    handle.search(None);
    let frame = next_frame(&rx);
    assert!(frame.search_matches.is_empty());
}

// ---- search navigation + status ----------------------------------

/// Wait for the next `SearchStatus` event, skipping frames.
fn next_search_status(rx: &Receiver<TermEvent>) -> usize {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(TermEvent::SearchStatus { total }) => return total,
            Ok(_) => continue,
            Err(err) => panic!("timed out waiting for SearchStatus: {err}"),
        }
    }
}

/// Feed `lines` rows of `"plain {i}"`, except `start..end` are `"error {i}"`,
/// so the match-count and grid positions are deterministic. The oldest rows
/// scroll off as usual. The last line carries no trailing CRLF so the grid
/// holds exactly `lines` rows (no dangling blank cursor line).
fn seed_with_errors(
    handle: &term_core::actor::TermHandle,
    rx: &Receiver<TermEvent>,
    lines: u32,
    start: u32,
    end: u32,
) {
    let mut out = Vec::new();
    for i in 0..lines {
        let text = if i >= start && i < end {
            format!("error {i}")
        } else {
            format!("plain {i}")
        };
        if i + 1 < lines {
            out.extend_from_slice(text.as_bytes());
            out.extend_from_slice(b"\r\n");
        } else {
            out.extend_from_slice(text.as_bytes());
        }
    }
    handle.feed(&out);
    let frame = next_frame(rx);
    handle.ack(frame.seq);
}

#[test]
fn search_status_reports_total_once_per_search() {
    let (handle, rx, _sink) = test_actor(30, 5);
    seed_with_errors(&handle, &rx, 20, 0, 20);

    handle.search(Some("error".into()));
    assert_eq!(next_search_status(&rx), 20);

    // No further SearchStatus from ordinary output — the count is a scan-time
    // snapshot, not a per-frame recount.
    handle.feed(b"error more output");
    handle.ack(next_frame(&rx).seq);
    let deadline = std::time::Instant::now() + Duration::from_millis(150);
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
            Ok(TermEvent::SearchStatus { .. }) => panic!("unexpected recount on plain output"),
            Ok(_) => continue,
            Err(_) => break,
        }
    }

    // Clearing the search emits nothing; a new search recounts from scratch
    // over the 21-line grid.
    handle.search(None);
    handle.search(Some("error".into()));
    assert_eq!(next_search_status(&rx), 21);
}

#[test]
fn search_nav_lands_match_centered() {
    // 5-row viewport, 30 rows with "error" only at written lines 5..9. Grid
    // lines: written N at grid N-25, so the errors sit at grid -20..-16.
    let (handle, rx, _sink) = test_actor(30, 5);
    seed_with_errors(&handle, &rx, 30, 5, 10);

    handle.search(Some("error".into()));
    assert_eq!(next_search_status(&rx), 5);
    handle.ack(next_frame(&rx).seq);

    // Next from the live viewport (nothing below center) wraps to the oldest
    // error (grid -20) and centers it at viewport row 2 → offset 22.
    handle.search_nav(SearchNavDir::Next);
    let frame = next_frame(&rx);
    handle.ack(frame.seq);
    assert_eq!(
        frame.display_offset, 22,
        "wrapped next lands on the oldest error"
    );
    assert!(
        frame
            .search_matches
            .iter()
            .any(|(s, _)| s.line == 2 && s.column == 0),
        "oldest error centered at row 2: {:?}",
        frame.search_matches
    );

    // Next again steps to the next error (grid -19) → offset 21.
    handle.search_nav(SearchNavDir::Next);
    let frame = next_frame(&rx);
    handle.ack(frame.seq);
    assert_eq!(frame.display_offset, 21);
}

#[test]
fn search_nav_wraps_both_directions() {
    // Errors at grid -20..-16. Next past the newest wraps to the oldest;
    // Prev past the oldest wraps to the newest. Each nav is acked so the
    // actor's gate opens and every step lands its own frame.
    let (handle, rx, _sink) = test_actor(30, 5);
    seed_with_errors(&handle, &rx, 30, 5, 10);

    handle.search(Some("error".into()));
    assert_eq!(next_search_status(&rx), 5);
    handle.ack(next_frame(&rx).seq);

    // Walk next down through all five errors.
    let mut offsets = Vec::new();
    for _ in 0..5 {
        handle.search_nav(SearchNavDir::Next);
        let frame = next_frame(&rx);
        handle.ack(frame.seq);
        offsets.push(frame.display_offset);
    }
    assert_eq!(
        offsets,
        vec![22, 21, 20, 19, 18],
        "next steps through the errors"
    );

    // Next past the newest wraps to the oldest error.
    handle.search_nav(SearchNavDir::Next);
    let frame = next_frame(&rx);
    handle.ack(frame.seq);
    assert_eq!(frame.display_offset, 22, "next wraps to the oldest");

    // Prev past the oldest wraps to the newest error.
    handle.search_nav(SearchNavDir::Prev);
    let frame = next_frame(&rx);
    handle.ack(frame.seq);
    assert_eq!(frame.display_offset, 18, "prev wraps to the newest");
}

#[test]
fn search_nav_moves_previous_up_scrollback() {
    // Errors at grid -20..-16. Prev from the live viewport (offset 0) jumps
    // to the newest error (grid -16) → offset 18; Prev again up one → 19.
    let (handle, rx, _sink) = test_actor(30, 5);
    seed_with_errors(&handle, &rx, 30, 5, 10);

    handle.search(Some("error".into()));
    assert_eq!(next_search_status(&rx), 5);
    handle.ack(next_frame(&rx).seq);

    handle.search_nav(SearchNavDir::Prev);
    let frame = next_frame(&rx);
    handle.ack(frame.seq);
    assert_eq!(frame.display_offset, 18);

    handle.search_nav(SearchNavDir::Prev);
    let frame = next_frame(&rx);
    handle.ack(frame.seq);
    assert_eq!(frame.display_offset, 19);

    // Next returns to the newer error.
    handle.search_nav(SearchNavDir::Next);
    let frame = next_frame(&rx);
    handle.ack(frame.seq);
    assert_eq!(frame.display_offset, 18);
}

#[test]
fn search_nav_no_op_without_matches() {
    let (handle, rx, _sink) = test_actor(30, 5);
    seed_with_errors(&handle, &rx, 10, 0, 10);

    handle.search(Some("nomatch".into()));
    assert_eq!(next_search_status(&rx), 0);
    handle.ack(next_frame(&rx).seq);

    // No matches → nav must not move the viewport or emit a frame.
    handle.search_nav(SearchNavDir::Next);
    handle.search_nav(SearchNavDir::Prev);
    let deadline = std::time::Instant::now() + Duration::from_millis(120);
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
            Ok(TermEvent::Frame(_)) => panic!("nav with no matches must not emit a frame"),
            Ok(_) => continue,
            Err(_) => break,
        }
    }
}

#[test]
fn search_nav_no_op_without_active_search() {
    let (handle, rx, _sink) = test_actor(30, 5);
    seed_with_errors(&handle, &rx, 10, 0, 10);

    handle.search_nav(SearchNavDir::Next);
    let deadline = std::time::Instant::now() + Duration::from_millis(120);
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
            Ok(TermEvent::Frame(_)) => panic!("nav without a search must not emit a frame"),
            Ok(_) => continue,
            Err(_) => break,
        }
    }
}

#[test]
fn search_nav_walks_past_uncenterable_bottom_matches() {
    // Host-run regression: a match in the BOTTOM half of the live
    // viewport can never be centered — the offset clamps at 0 — so an anchor
    // re-derived from the viewport center re-selected it forever and the walk
    // never wrapped. The nav must instead step from the last selected match.
    //
    // 30 written lines, 5-row viewport: written N sits at grid N-25. Errors
    // at written 0 (grid -25, deep in scrollback), 27 (grid 2, the center
    // line) and 28 (grid 3, below center — the uncenterable one).
    let (handle, rx, _sink) = test_actor(30, 5);
    let mut out = Vec::new();
    for i in 0..30 {
        let text = if i == 0 || i == 27 || i == 28 {
            format!("error {i}")
        } else {
            format!("plain {i}")
        };
        out.extend_from_slice(text.as_bytes());
        if i + 1 < 30 {
            out.extend_from_slice(b"\r\n");
        }
    }
    handle.feed(&out);
    handle.ack(next_frame(&rx).seq);

    handle.search(Some("error".into()));
    assert_eq!(next_search_status(&rx), 3);
    handle.ack(next_frame(&rx).seq);

    // First Next selects "error 28" (grid 3). Centering it wants offset -1,
    // which clamps to the current 0 — correctly NO frame.
    handle.search_nav(SearchNavDir::Next);
    let deadline = std::time::Instant::now() + Duration::from_millis(120);
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
            Ok(TermEvent::Frame(_)) => panic!("clamped nav must not emit a frame"),
            Ok(_) => continue,
            Err(_) => break,
        }
    }

    // Second Next must ADVANCE past the clamped match and wrap to "error 0"
    // at the top of scrollback (offset 27 clamped to the 25-line history).
    // The center-anchored bug re-selected "error 28" here forever.
    handle.search_nav(SearchNavDir::Next);
    let frame = next_frame(&rx);
    handle.ack(frame.seq);
    assert_eq!(
        frame.display_offset, 25,
        "nav must step off the uncenterable match and wrap to scrollback"
    );
}

#[test]
fn search_status_terminates_on_corner_matches() {
    // Host-run regression: `.*` matches the bottom line through its
    // last column, and `Point::add(Boundary::None)` WRAPS at the bottom-right
    // corner — the count loop restarted from the top of scrollback and spun
    // the actor thread forever (frozen terminal, dead search, queued Esc).
    let (handle, rx, _sink) = test_actor(30, 5);
    seed_with_errors(&handle, &rx, 10, 0, 10);

    handle.search(Some(".*".into()));
    let total = next_search_status(&rx); // hangs (5s panic) on the bug
    assert!(total >= 1, "full-line matches must be counted, got {total}");
    handle.ack(next_frame(&rx).seq);

    // The actor must still be responsive after the corner-case count.
    handle.search(Some("error".into()));
    assert_eq!(next_search_status(&rx), 10);
}

#[test]
fn typed_input_scrolls_back_to_bottom() {
    // Host-run nit: typing while scrolled up must snap the view
    // back to the live screen — you type blind otherwise. Mouse events and
    // unencodable keys must NOT scroll.
    use term_core::keys::{Key, KeyEvent, Mods, MouseEvent, MouseKind};

    let (handle, rx, _sink) = test_actor(30, 5);
    seed_with_errors(&handle, &rx, 30, 0, 0);

    handle.set_display_offset(20);
    let frame = next_frame(&rx);
    handle.ack(frame.seq);
    assert_eq!(frame.display_offset, 20);

    // A mouse move while scrolled up: no mouse mode is active, nothing is
    // written, nothing scrolls — no frame.
    handle.mouse(MouseEvent {
        kind: MouseKind::Move,
        button: 0,
        col: 1,
        row: 1,
        mods: Mods::empty(),
    });
    let deadline = std::time::Instant::now() + Duration::from_millis(120);
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
            Ok(TermEvent::Frame(_)) => panic!("a mouse event must not scroll the view"),
            Ok(_) => continue,
            Err(_) => break,
        }
    }

    // A typed printable snaps the view to the bottom.
    handle.write_key(KeyEvent {
        key: Key::Char('a'),
        mods: Mods::empty(),
    });
    let frame = next_frame(&rx);
    handle.ack(frame.seq);
    assert_eq!(frame.display_offset, 0, "typing snaps to the live screen");
}

#[test]
fn search_nav_reanchors_after_user_scroll() {
    // A user scroll (scrollbar/wheel → SetDisplayOffset) invalidates the nav
    // anchor: the next nav derives from the fresh viewport, not the stale
    // last-selected match. Errors at grid -20..-16 (seed 30 lines, 5..10).
    let (handle, rx, _sink) = test_actor(30, 5);
    seed_with_errors(&handle, &rx, 30, 5, 10);

    handle.search(Some("error".into()));
    assert_eq!(next_search_status(&rx), 5);
    handle.ack(next_frame(&rx).seq);

    // Walk two matches in: offsets 22 then 21 (anchor now "error 6").
    handle.search_nav(SearchNavDir::Next);
    handle.ack(next_frame(&rx).seq);
    handle.search_nav(SearchNavDir::Next);
    let frame = next_frame(&rx);
    handle.ack(frame.seq);
    assert_eq!(frame.display_offset, 21);

    // User jumps back to the live view.
    handle.set_display_offset(0);
    let frame = next_frame(&rx);
    handle.ack(frame.seq);
    assert_eq!(frame.display_offset, 0);

    // Next must re-anchor on the viewport (wrap to the oldest error, offset
    // 22), NOT continue from the stale anchor (which would give offset 20).
    handle.search_nav(SearchNavDir::Next);
    let frame = next_frame(&rx);
    handle.ack(frame.seq);
    assert_eq!(
        frame.display_offset, 22,
        "nav after a user scroll must re-anchor on the viewport"
    );
}

// ---- OSC (mechanism) ----------------------------------------------

#[test]
fn osc_notify_surfaces() {
    let (handle, rx, _sink) = test_actor(20, 5);
    handle.feed(b"\x1b]9;build done\x07");

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
            Ok(TermEvent::Notify { title, body }) => {
                assert_eq!(title, "");
                assert_eq!(body, "build done");
                break;
            }
            Ok(_) => continue,
            Err(err) => panic!("no notify event: {err}"),
        }
    }
}

#[test]
fn osc_777_notify_with_title() {
    let (handle, rx, _sink) = test_actor(20, 5);
    handle.feed(b"\x1b]777;notify;ChappaAi;sync complete\x1b\\");

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
            Ok(TermEvent::Notify { title, body }) => {
                assert_eq!(title, "ChappaAi");
                assert_eq!(body, "sync complete");
                break;
            }
            Ok(_) => continue,
            Err(err) => panic!("no notify event: {err}"),
        }
    }
}

#[test]
fn osc_prompt_marks_surface_all_kinds() {
    let (handle, rx, _sink) = test_actor(20, 5);
    handle.feed(b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\\x1b]133;C\x1b\\make\x1b]133;D\x1b\\");

    let mut kinds = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while kinds.len() < 4 {
        match rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
            Ok(TermEvent::PromptMark { kind, .. }) => kinds.push(kind),
            Ok(_) => continue,
            Err(err) => panic!("expected 4 prompt marks, got {kinds:?}: {err}"),
        }
    }
    use term_core::actor::MarkKind;
    assert_eq!(
        kinds,
        vec![
            MarkKind::PromptStart,
            MarkKind::PromptEnd,
            MarkKind::CommandStart,
            MarkKind::CommandEnd
        ]
    );
}

#[test]
fn prompt_mark_rows_are_buffer_absolute() {
    // Host-run regression: the mark row must be the line's index
    // from the TOP of scrollback (history + cursor row), not the raw cursor
    // line — the cursor pins to the bottom once the screen fills, so the
    // viewport-relative rows collapsed to ~one value and mark nav went
    // nowhere. Eight prompts, two lines apart, on a 5-row screen: the rows
    // must keep climbing 0,2,4,… long after the viewport saturated.
    let (handle, rx, _sink) = test_actor(30, 5);

    for _ in 0..8 {
        // The prompt chunk ends ON the prompt line (no trailing newline),
        // like a real shell; the next chunk echoes a command + output.
        handle.feed(b"\x1b]133;A\x1b\\PS> ");
        handle.feed(b"cmd\r\nout\r\n");
    }

    let mut rows = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while rows.len() < 8 {
        match rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
            Ok(TermEvent::PromptMark { row, .. }) => rows.push(row),
            Ok(_) => continue,
            Err(err) => panic!("expected 8 prompt marks, got {rows:?}: {err}"),
        }
    }
    assert_eq!(rows, vec![0, 2, 4, 6, 8, 10, 12, 14]);
}

#[test]
fn osc8_link_gets_id_and_hyperlink_table() {
    let (handle, rx, _sink) = test_actor(20, 5);
    handle.feed(b"\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\");

    // The frame's cells reference a link id...
    let frame = next_frame(&rx);
    let cell = find_cell(&frame, 'l').expect("linked cell");
    assert!(cell.link_id != 0, "cell under an OSC 8 link gets an id");

    // ...and a HyperlinkTable entry announces the uri for that id.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
            Ok(TermEvent::HyperlinkTable(links)) => {
                assert!(
                    links
                        .iter()
                        .any(|(id, uri)| *id == cell.link_id && uri == "https://example.com"),
                    "table must map {:#?}",
                    links
                );
                break;
            }
            Ok(_) => continue,
            Err(err) => panic!("no hyperlink table: {err}"),
        }
    }
}

// ---- resize ----------------------------------------------------------------

#[test]
fn resize_reflows_and_forces_full() {
    let (handle, rx, _sink) = test_actor(10, 5);

    handle.resize(6, 3);
    let frame = next_frame(&rx);
    handle.ack(frame.seq);
    assert_eq!(frame.kind, FrameKind::Full, "resize forces Full");
    // 6 columns, 3 rows.
    assert!(frame.rows.len() <= 3);
    assert!(frame.rows.iter().all(|patch| patch.cells.len() <= 6));
}

// ---- cursor state in frames ------------------------------------------------

#[test]
fn frame_carries_cursor_and_shape() {
    let (handle, rx, _sink) = test_actor(20, 5);

    handle.feed(b"\x1b[3;3HZ");
    let frame = next_frame(&rx);
    handle.ack(frame.seq);

    assert_eq!(frame.cursor.row, 2, "viewport row of grid row 2");
    assert_eq!(frame.cursor.col, 3, "cursor advances past Z");
    assert!(frame.cursor.visible, "cursor visible by default");
    assert_eq!(frame.cursor.shape, CursorShape::Block);
}

// ---- fuzz ------------------------------------------------------------------

#[test]
fn one_megabyte_random_bytes_no_panic() {
    let (handle, rx, _sink) = test_actor(80, 24);

    // Deterministic xorshift; 128 × 8 KB = 1 MB.
    let mut rng = 0xDEAD_BEEF_1234_5678u64;
    let mut chunk = vec![0u8; 8192];
    for _ in 0..128 {
        for byte in chunk.iter_mut() {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            *byte = rng as u8;
        }
        handle.feed(&chunk);
    }

    // Liveness probe: a clean SGR sequence right after the garbage must still
    // parse and produce a frame. If the actor panicked mid-fuzz, no frame
    // arrives and `next_frame` times out.
    handle.feed(b"\x1b[31mFZZ");
    let frame = next_frame(&rx);
    assert!(
        frames_contain(&frame, 'F'),
        "actor alive and parsing after 1MB of garbage"
    );

    handle.shutdown();
}

// ---- wire mode flags --------------------------------------------

/// The frame header carries the live mouse/alt-screen mode so the frontend
/// can suppress local selection/wheel handling when a TUI owns the mouse.
#[test]
fn frame_carries_mouse_capture_and_alt_screen_flags() {
    let (handle, rx, _sink) = test_actor(20, 5);

    // No mouse protocol, no alt screen by default.
    handle.feed(b"x");
    let initial = next_frame(&rx);
    handle.ack(initial.seq);
    assert!(!initial.mouse_capture);
    assert!(!initial.alt_screen);

    // Enable mouse tracking + the alternate screen.
    handle.feed(b"\x1b[?1000h\x1b[?1049h");
    let frame = next_frame(&rx);
    handle.ack(frame.seq);
    assert!(frame.mouse_capture, "DECSET 1000 sets mouse_capture");
    assert!(frame.alt_screen, "DECSET 1049 sets alt_screen");

    // Disable both again.
    handle.feed(b"\x1b[?1000l\x1b[?1049l");
    let frame = next_frame(&rx);
    assert!(!frame.mouse_capture);
    assert!(!frame.alt_screen);
}

/// A TUI killed while mouse tracking
/// is on never sends its reset sequences — the final frame must not export
/// the stale capture, or the dead pane demands Shift-selection forever
/// (panel.ts takes `frameMouseCapture` from every frame, so a false bit in
/// the final frame is what clears the pane-side flag).
#[test]
fn final_frame_after_exit_clears_stale_mouse_capture() {
    let (handle, rx, _sink) = test_actor(20, 5);

    // The TUI enables any-motion + SGR mouse tracking, then dies without
    // resetting (the kill-mid-vim scenario).
    handle.feed(b"\x1b[?1003h\x1b[?1006hvim");
    let frame = next_frame(&rx);
    handle.ack(frame.seq);
    assert!(frame.mouse_capture, "precondition: capture is on");

    handle.note_child_exit(ExitStatus {
        code: Some(137),
        success: false,
    });

    // finish_exit paints one final Full frame, then reports the exit.
    let last = next_frame(&rx);
    assert_eq!(last.kind, FrameKind::Full);
    assert!(
        !last.mouse_capture,
        "exit must clear stale mouse capture before the final frame"
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(TermEvent::Exited(status)) => {
                assert_eq!(status.code, Some(137));
                break;
            }
            Ok(_) => continue,
            Err(err) => panic!("no Exited event after the final frame: {err}"),
        }
    }
}

// ---- hard-kill (quit) ---------------------------------------------

/// `TermHandle::kill` is the app-quit path: a straggler that ignores the
/// normal close must be hard-killed after 2 s and the actor must join
/// promptly.
#[test]
#[cfg(unix)]
fn kill_terminates_running_child_promptly() {
    let (events, rx) = std::sync::mpsc::channel();
    let cfg = ActorConfig {
        spec: PtySpec {
            command: "sh".into(),
            args: vec!["-c".into(), "sleep 30".into()],
            cwd: None,
            env: Vec::new(),
            cols: 20,
            rows: 5,
        },
        scrollback_lines: 10_000,
        ..ActorConfig::default()
    };
    let handle = spawn_actor(cfg, events).expect("pty spawn");

    // Wait until the child is demonstrably up (first frame).
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(TermEvent::Frame(_)) => break,
            Ok(_) => continue,
            Err(err) => panic!("no frame from spawned child: {err}"),
        }
    }

    // Kill must return (child dead + actor joined) well within 2s.
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        handle.kill();
        let _ = done_tx.send(());
    });
    done_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("kill() must join the actor within 2s");
}

// ---- real pty smoke test ---------------------------------------------------

#[test]
#[cfg(unix)]
fn real_pty_smoke() {
    let (events, rx) = std::sync::mpsc::channel();
    let cfg = ActorConfig {
        spec: PtySpec {
            command: "sh".into(),
            args: vec!["-c".into(), "printf 'hi\\n'; exit 0".into()],
            cwd: None,
            env: Vec::new(),
            cols: 20,
            rows: 5,
        },
        scrollback_lines: 10_000,
        ..ActorConfig::default()
    };
    let handle = spawn_actor(cfg, events).expect("pty spawn");

    let mut saw_text = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
            Ok(TermEvent::Frame(frame)) => {
                if frames_contain(&frame, 'h') {
                    saw_text = true;
                }
            }
            Ok(TermEvent::Exited(status)) => {
                assert!(saw_text, "child output must arrive as frames before Exited");
                assert_eq!(status.code, Some(0));
                assert!(status.success);
                break;
            }
            Ok(_) => continue,
            Err(err) => panic!("no Exited event: {err}"),
        }
    }

    handle.shutdown();
}

// ---- synthetic prompt marks --------------------------------------
//
// Rule:
//   "When `synthetic_prompt_marks` is ON, the ACTOR records a synthetic mark
//    on every UNMODIFIED Enter `WriteKey` while the terminal is NOT in the
//    alt screen: emit the existing `TermEvent::PromptMark { PromptStart, row }`
//    with the buffer-absolute row (history + cursor), same as the scanner
//    path. No new event shapes."
// Every `row` asserted below is BUFFER-ABSOLUTE: the line's index from the
// TOP of scrollback (history_size + cursor line), never viewport-relative.

use term_core::actor::MarkKind;
use term_core::keys::{Key, KeyEvent, Mods};

/// A no-pty actor with the settings set explicitly.
fn marks_actor(
    cols: u16,
    rows: u16,
    synthetic_prompt_marks: bool,
) -> (
    term_core::actor::TermHandle,
    Receiver<TermEvent>,
    Arc<Mutex<Vec<u8>>>,
) {
    let (events, rx) = std::sync::mpsc::channel();
    let cfg = ActorConfig {
        synthetic_prompt_marks,
        ..cfg(cols, rows)
    };
    let (handle, sink) = spawn_actor_nopty(cfg, events);
    let first = next_frame(&rx);
    handle.ack(first.seq);
    (handle, rx, sink)
}

fn enter(handle: &term_core::actor::TermHandle, mods: Mods) {
    handle.write_key(KeyEvent {
        key: Key::Enter,
        mods,
    });
}

/// Next `PromptMark` within 5s, or panic.
fn next_prompt_mark(rx: &Receiver<TermEvent>) -> (MarkKind, i64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
            Ok(TermEvent::PromptMark { kind, row }) => return (kind, row),
            Ok(_) => continue,
            Err(err) => panic!("expected a prompt mark: {err}"),
        }
    }
}

/// No `PromptMark` arrives within a short window. The actor's tick is 8ms, so
/// 250ms is ~30 ticks of headroom.
fn assert_no_prompt_mark(rx: &Receiver<TermEvent>, why: &str) {
    let deadline = std::time::Instant::now() + Duration::from_millis(250);
    loop {
        match rx.recv_timeout(Duration::from_millis(10)) {
            Ok(TermEvent::PromptMark { kind, row }) => {
                panic!("unexpected {kind:?} mark at row {row}: {why}")
            }
            Ok(_) => continue,
            Err(_) if std::time::Instant::now() >= deadline => return,
            Err(_) => {}
        }
    }
}

/// Do not flip: default OFF, permanently. It guesses — an Enter that cancels
/// a dialog or answers a y/n still plants a mark — so it is never enabled
/// implicitly. Both the config default AND the resulting behavior.
#[test]
fn synthetic_prompt_marks_default_off_permanently() {
    assert!(
        !ActorConfig::default().synthetic_prompt_marks,
        "ActorConfig::default().synthetic_prompt_marks must stay FALSE — the \
         opt-in is deliberate"
    );
    let (handle, rx, _sink) = test_actor(20, 5);
    enter(&handle, Mods::EMPTY);
    assert_no_prompt_mark(&rx, "the default actor must never plant marks");
    handle.shutdown();
}

/// The boundary regime that matters: NONZERO scrollback. The row must be
/// history + cursor line, not the viewport-relative cursor row (the
/// host-run bug the OSC 133 path already fixed).
#[test]
fn synthetic_mark_row_is_buffer_absolute_with_history() {
    let (handle, rx, _sink) = marks_actor(20, 5, true);

    // Scroll the screen: 10 newlines on a 5-row screen leaves 11 lines total
    // (10 + the cursor's own), so history_size = 11 - 5 = 6 and the cursor
    // sits on the last viewport row (line 4) => buffer-absolute row 10.
    let mut out = String::new();
    for i in 0..10 {
        out.push_str(&format!("line{i}\r\n"));
    }
    handle.feed(out.as_bytes());
    let frame = next_frame(&rx);
    handle.ack(frame.seq);
    assert!(frame.history_len > 0, "the test needs real scrollback");

    enter(&handle, Mods::EMPTY);
    let (kind, row) = next_prompt_mark(&rx);
    assert_eq!(kind, MarkKind::PromptStart, "no new event shapes");
    assert_eq!(
        row,
        frame.history_len as i64 + frame.cursor.row as i64,
        "row must be history + cursor line (buffer-absolute)"
    );
    assert_eq!(row, 10, "6 history lines + cursor on viewport row 4");
    handle.shutdown();
}

/// "Alt screen is excluded (no scrollback to jump in — vim Enter spam would
/// be pure noise)." Also the enter/exit BOUNDARY: leaving the alt screen must
/// restore marking without a restart.
#[test]
fn synthetic_marks_are_suppressed_on_the_alt_screen() {
    let (handle, rx, _sink) = marks_actor(20, 5, true);

    handle.feed(b"\x1b[?1049h");
    let frame = next_frame(&rx);
    handle.ack(frame.seq);
    assert!(frame.alt_screen, "alt screen must be active");

    enter(&handle, Mods::EMPTY);
    assert_no_prompt_mark(&rx, "Enter in the alt screen must emit NOTHING");

    // Back to the main screen: marking resumes on the very next Enter.
    handle.feed(b"\x1b[?1049l");
    let frame = next_frame(&rx);
    handle.ack(frame.seq);
    assert!(!frame.alt_screen);
    enter(&handle, Mods::EMPTY);
    let (kind, _row) = next_prompt_mark(&rx);
    assert_eq!(kind, MarkKind::PromptStart);
    handle.shutdown();
}

/// "a toggle must not require restarting terminals" — the live control has to
/// take effect MID-STREAM, in both directions.
#[test]
fn synthetic_marks_toggle_live_mid_stream() {
    let (handle, rx, _sink) = marks_actor(20, 5, true);

    enter(&handle, Mods::EMPTY);
    assert_eq!(next_prompt_mark(&rx).0, MarkKind::PromptStart);

    handle.set_synthetic_marks(false);
    enter(&handle, Mods::EMPTY);
    assert_no_prompt_mark(&rx, "toggling off must stop emission immediately");

    handle.set_synthetic_marks(true);
    enter(&handle, Mods::EMPTY);
    assert_eq!(next_prompt_mark(&rx).0, MarkKind::PromptStart);
    handle.shutdown();
}

/// "Modified Enters are excluded … Shift+Enter is the
/// in-composer newline — marking it would plant a mark per continuation
/// line of a single prompt." Ctrl/Alt+Enter are excluded by the same rule.
#[test]
fn modified_enter_never_plants_a_synthetic_mark() {
    let (handle, rx, _sink) = marks_actor(20, 5, true);
    for mods in [Mods::SHIFT, Mods::CTRL, Mods::ALT, Mods::SHIFT | Mods::CTRL] {
        enter(&handle, mods);
        assert_no_prompt_mark(&rx, "only UNMODIFIED Enter marks");
    }
    // …and the unmodified press right after still works.
    enter(&handle, Mods::EMPTY);
    assert_eq!(next_prompt_mark(&rx).0, MarkKind::PromptStart);
    handle.shutdown();
}

/// "Plain shells with real OSC 133 marks double up harmlessly: `marks.add`
/// already ignores same-row duplicates." Dedup is the FRONTEND's job — the
/// actor emits both, in the same event shape, on the same buffer-absolute row.
#[test]
fn real_osc133_and_synthetic_marks_coexist_on_one_row() {
    let (handle, rx, _sink) = marks_actor(20, 5, true);

    // A real shell prompt: OSC 133;A then the prompt text, no newline.
    handle.feed(b"\x1b]133;A\x1b\\PS> ");
    let (osc_kind, osc_row) = next_prompt_mark(&rx);
    assert_eq!(osc_kind, MarkKind::PromptStart);

    // The user submits: the synthetic mark lands on the SAME row.
    enter(&handle, Mods::EMPTY);
    let (syn_kind, syn_row) = next_prompt_mark(&rx);
    assert_eq!(syn_kind, MarkKind::PromptStart, "no new event shapes");
    assert_eq!(syn_row, osc_row, "same buffer-absolute row, twice");

    // Exactly two marks total — no third shape, no duplicate storm.
    assert_no_prompt_mark(&rx, "only the two marks may be emitted");
    handle.shutdown();
}

/// Byte-flow liveness counters live on the raw byte
/// path, not the render path. `feed` is that path for a no-pty actor: bytes
/// count the moment they are handed over, `last_output_ms` is set, and
/// `has_output` flips exactly once from "booting" to "alive and talking".
/// `child_alive` drops when the actor thread ends.
#[test]
fn io_counters_track_byte_flow_not_render() {
    let (handle, _rx, _sink) = test_actor(20, 5);
    assert!(!handle.io().has_output(), "fresh actor has no output yet");
    assert_eq!(handle.io().output_bytes(), 0);
    assert_eq!(handle.io().last_output_ms(), None);
    assert!(handle.io().child_alive());

    handle.feed(b"hello");
    assert_eq!(handle.io().output_bytes(), 5);
    assert!(handle.io().has_output());
    let first = handle.io().last_output_ms().expect("timestamp set");
    assert!(first > 0);

    handle.feed(b"\x1b[2J"); // escape bytes count too: this is byte flow
    assert_eq!(handle.io().output_bytes(), 9);
    assert!(handle.io().last_output_ms().unwrap() >= first);

    let probe = handle.clone();
    handle.shutdown();
    assert!(!probe.io().child_alive(), "joined actor is not alive");
}

/// Cleanup: the send receipt waits on a condvar instead of a 5ms
/// sleep-poll. A write that produces output wakes the waiter as soon as the
/// bytes land (well inside the timeout), a quiet stream costs exactly the
/// timeout and no CPU, and a counter that has ALREADY moved never parks.
#[test]
fn wait_for_output_wakes_on_bytes_and_honours_the_deadline() {
    use std::time::{Duration, Instant};

    let (handle, _rx, _sink) = test_actor(20, 5);
    let io = handle.io();

    // Nothing flowing: the wait costs its full timeout and reports no change.
    let started = Instant::now();
    assert_eq!(io.wait_for_output(0, Duration::from_millis(120)), 0);
    let quiet = started.elapsed();
    assert!(quiet >= Duration::from_millis(100), "waited {quiet:?}");
    assert!(quiet < Duration::from_secs(3), "must not overshoot: {quiet:?}");

    // Bytes from another thread wake it immediately (the timeout is 10s: a
    // sleep-poll or a lost wakeup would blow the assert below).
    let writer = handle.clone();
    let feeder = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(30));
        writer.feed(b"late output");
    });
    let started = Instant::now();
    let seen = handle.io().wait_for_output(0, Duration::from_secs(10));
    let woke = started.elapsed();
    feeder.join().unwrap();
    assert_eq!(seen, 11, "returns the counter it observed");
    assert!(woke < Duration::from_secs(5), "woke on the signal, not the deadline: {woke:?}");

    // Already moved: returns without parking at all.
    let started = Instant::now();
    assert_eq!(handle.io().wait_for_output(0, Duration::from_secs(10)), 11);
    assert!(started.elapsed() < Duration::from_secs(1), "no park when the answer is already there");

    handle.shutdown();
}

// ---- winsize poke (bridge probe) ------------------------------------

/// An IDLE but healthy shell must answer a winsize poke with bytes (ConPTY
/// repaints on a resize; readline redraws its prompt on SIGWINCH) — that is
/// what keeps a quiet docker-exec agent from being called `stale`. A resize
/// to the CURRENT size is dropped by the actor as a no-op, so the poke must
/// be its own primitive.
#[test]
fn poke_makes_an_idle_shell_talk() {
    let (events, _rx) = std::sync::mpsc::channel();
    #[cfg(windows)]
    let (command, args): (String, Vec<String>) = ("cmd".into(), vec!["/K".into()]);
    #[cfg(not(windows))]
    let (command, args): (String, Vec<String>) = (
        "bash".into(),
        vec!["--norc".into(), "--noprofile".into(), "-i".into()],
    );
    let cfg = ActorConfig {
        spec: PtySpec {
            command,
            args,
            cwd: None,
            env: Vec::new(),
            cols: 80,
            rows: 24,
        },
        scrollback_lines: 100,
        ..ActorConfig::default()
    };
    let handle = match spawn_actor(cfg, events) {
        Ok(h) => h,
        // No bash on this box: nothing to assert against.
        Err(_) if cfg!(not(windows)) => return,
        Err(e) => panic!("pty spawn: {e}"),
    };
    // Let the shell boot and go quiet.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !handle.io().has_output() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(handle.io().has_output(), "shell never printed a prompt");
    let mut last = handle.io().output_bytes();
    loop {
        std::thread::sleep(Duration::from_millis(300));
        let now = handle.io().output_bytes();
        if now == last {
            break;
        }
        last = now;
    }
    // Quiet now. A same-size resize is a no-op (must NOT produce bytes)…
    handle.resize(80, 24);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(handle.io().output_bytes(), last, "same-size resize must be a no-op");
    // …and the poke does.
    handle.poke();
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while handle.io().output_bytes() == last && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    // ConPTY re-renders on EVERY resize, so Windows asserts hard. Unix
    // readline only redraws when it decides the prompt needs it — bash in
    // some containers stay silent on SIGWINCH (found
    // 2026-08-31, first Linux run of this suite), so there the poke's
    // delivery is exercised but the byte assertion cannot hold.
    if cfg!(windows) {
        assert_ne!(handle.io().output_bytes(), last, "poke produced no output from an idle shell");
    }
    handle.kill();
}
