//! Frame protocol tests.
//!
//! Three layers, all against the SAME checked-in fixtures:
//! - `fixtures_roundtrip_and_write_bin` reads every `tests/fixtures/frames/
//!   *.json`, encodes it with `encode_frame`, writes the `.bin` next to it
//!   (checked in — the TS decoder consumes the exact same bytes), and
//!   hand-rolls a decode to prove the layout round-trips. The truncated
//!   fixture's `.bin` is deliberately cut short and must FAIL to decode.
//! - `roundtrip_random_frames` property-tests the layout against seeded
//!   random `FrameData`.
//! - `bench_encode_full_200x50` prints the per-frame encode cost for
//!   reference.

use std::fs;
use std::time::Instant;

use serde::Deserialize;

use term_core::actor::{
    Cell, CellFlags, CursorShape, CursorState, FrameData, FrameKind, Point, RowPatch,
    ZerowidthEntry,
};
use term_core::color::Rgba;
use term_core::frame::{cell_flags_from_bits, encode_frame, FRAME_MAGIC, FRAME_VERSION};

use alacritty_terminal::index::{Column, Line};

/// Checked-in fixture directory, relative to the crate root (cargo runs
/// integration tests with cwd = package root).
const FIXTURE_DIR: &str = "tests/fixtures/frames";

// ---- fixture schema -------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Fixture {
    name: String,
    seq: u32,
    kind: String,
    cursor: FixtureCursor,
    #[serde(rename = "displayOffset")]
    display_offset: u32,
    #[serde(rename = "historyLen")]
    history_len: u32,
    selection: Option<FixtureRange>,
    /// A selection exists even if scrolled out of the viewport (wire flags
    /// bit 0). Defaults to `selection.is_some()` when absent.
    #[serde(rename = "selectionActive", default)]
    selection_active: Option<bool>,
    /// Any mouse tracking protocol active (wire flags bit 1).
    #[serde(rename = "mouseCapture", default)]
    mouse_capture: Option<bool>,
    /// Alternate screen active (wire flags bit 2).
    #[serde(rename = "altScreen", default)]
    alt_screen: Option<bool>,
    rows: Vec<FixtureRow>,
    zerowidth: Vec<FixtureZw>,
    matches: Vec<FixtureRange>,
    #[serde(rename = "truncateTo")]
    truncate_to: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct FixtureCursor {
    row: u16,
    col: u16,
    shape: String,
    visible: bool,
}

#[derive(Debug, Deserialize)]
struct FixtureRange {
    #[serde(rename = "startRow")]
    start_row: i32,
    #[serde(rename = "startCol")]
    start_col: u16,
    #[serde(rename = "endRow")]
    end_row: i32,
    #[serde(rename = "endCol")]
    end_col: u16,
}

#[derive(Debug, Deserialize)]
struct FixtureRow {
    row: u16,
    #[serde(rename = "colStart")]
    col_start: u16,
    cells: Vec<FixtureCell>,
}

#[derive(Debug, Deserialize)]
struct FixtureCell {
    ch: String,
    fg: String,
    bg: String,
    flags: Vec<String>,
    link: u16,
}

#[derive(Debug, Deserialize)]
struct FixtureZw {
    row: u16,
    col: u16,
    chars: Vec<String>,
}

// ---- fixture → FrameData --------------------------------------------------

fn shape_from_str(s: &str) -> CursorShape {
    match s {
        "block" => CursorShape::Block,
        "underline" => CursorShape::Underline,
        "beam" => CursorShape::Beam,
        "hollow" => CursorShape::HollowBlock,
        "hidden" => CursorShape::Hidden,
        other => panic!("fixture cursor shape {other:?}"),
    }
}

/// Parse an "RRGGBBAA" hex color into an `Rgba`.
fn color_from_hex(hex: &str) -> Rgba {
    let v = u32::from_str_radix(hex, 16).unwrap_or_else(|_| panic!("bad hex color {hex:?}"));
    Rgba {
        r: (v >> 24) as u8,
        g: (v >> 16) as u8,
        b: (v >> 8) as u8,
        a: v as u8,
    }
}

fn flag_bits(name: &str) -> u16 {
    match name {
        "bold" => 1 << 0,
        "italic" => 1 << 1,
        "dim" => 1 << 2,
        "underline" => 1 << 3,
        "double_underline" => 1 << 4,
        "undercurl" => 1 << 5,
        "dotted_underline" => 1 << 6,
        "dashed_underline" => 1 << 7,
        "inverse" => 1 << 8,
        "strikeout" => 1 << 9,
        "hidden" => 1 << 10,
        "wide" => 1 << 11,
        "wide_spacer" => 1 << 12,
        _ => panic!("fixture flag {name:?}"),
    }
}

fn flags_from_names(names: &[String]) -> CellFlags {
    let mut flags = CellFlags::EMPTY;
    for name in names {
        flags = flags | cell_flags_from_bits(flag_bits(name));
    }
    flags
}

fn point(start_row: i32, start_col: u16) -> Point {
    Point::new(Line(start_row), Column(start_col as usize))
}

fn build_frame(f: &Fixture) -> FrameData {
    FrameData {
        seq: f.seq,
        kind: match f.kind.as_str() {
            "full" => FrameKind::Full,
            "delta" => FrameKind::Delta,
            other => panic!("fixture kind {other:?}"),
        },
        cursor: CursorState {
            row: f.cursor.row,
            col: f.cursor.col,
            shape: shape_from_str(&f.cursor.shape),
            visible: f.cursor.visible,
        },
        display_offset: f.display_offset as usize,
        history_len: f.history_len as usize,
        selection: f.selection.as_ref().map(|s| {
            (
                point(s.start_row, s.start_col),
                point(s.end_row, s.end_col),
            )
        }),
        selection_active: f.selection_active.unwrap_or(f.selection.is_some()),
        mouse_capture: f.mouse_capture.unwrap_or(false),
        alt_screen: f.alt_screen.unwrap_or(false),
        search_matches: f
            .matches
            .iter()
            .map(|m| (point(m.start_row, m.start_col), point(m.end_row, m.end_col)))
            .collect(),
        rows: f
            .rows
            .iter()
            .map(|r| RowPatch {
                row: r.row,
                col_start: r.col_start,
                cells: r
                    .cells
                    .iter()
                    .map(|c| Cell {
                        // `""` in a fixture means "no glyph" (wide spacer); it
                        // rides the wire as a space, matching alacritty.
                        ch: c.ch.chars().next().unwrap_or(' '),
                        fg: color_from_hex(&c.fg),
                        bg: color_from_hex(&c.bg),
                        flags: flags_from_names(&c.flags),
                        link_id: c.link,
                    })
                    .collect(),
            })
            .collect(),
        zerowidth: f
            .zerowidth
            .iter()
            .map(|z| {
                (
                    z.row,
                    z.col,
                    z.chars
                        .iter()
                        .map(|s| s.chars().next().expect("fixture zw char"))
                        .collect(),
                )
            })
            .collect(),
    }
}

fn assert_frame_eq(a: &FrameData, b: &FrameData, what: &str) {
    assert_eq!(a.seq, b.seq, "{what}: seq");
    assert_eq!(a.kind, b.kind, "{what}: kind");
    assert_eq!(a.cursor, b.cursor, "{what}: cursor");
    assert_eq!(a.display_offset, b.display_offset, "{what}: display_offset");
    assert_eq!(a.history_len, b.history_len, "{what}: history_len");
    assert_eq!(a.selection, b.selection, "{what}: selection");
    assert_eq!(
        a.selection_active, b.selection_active,
        "{what}: selection_active"
    );
    assert_eq!(a.mouse_capture, b.mouse_capture, "{what}: mouse_capture");
    assert_eq!(a.alt_screen, b.alt_screen, "{what}: alt_screen");
    assert_eq!(a.search_matches, b.search_matches, "{what}: matches");
    assert_eq!(a.rows, b.rows, "{what}: rows");
    assert_eq!(a.zerowidth, b.zerowidth, "{what}: zerowidth");
}

// ---- hand-rolled decode (mirrors the wire layout from frame.rs) -----------

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self.pos.checked_add(n).ok_or("cursor overflow")?;
        if end > self.bytes.len() {
            return Err(format!(
                "truncated: need {n} bytes at {:#x}, only {} left",
                self.pos,
                self.bytes.len().saturating_sub(self.pos)
            ));
        }
        let slice = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, String> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn i32(&mut self) -> Result<i32, String> {
        Ok(self.u32()? as i32)
    }

    fn at_end(&self) -> bool {
        self.pos == self.bytes.len()
    }
}

fn decode_frame(bytes: &[u8]) -> Result<FrameData, String> {
    let mut r = Reader::new(bytes);
    if r.u8()? != FRAME_MAGIC {
        return Err("bad magic".into());
    }
    if r.u8()? != FRAME_VERSION {
        return Err("bad version".into());
    }
    let seq = r.u32()?;
    let kind = match r.u8()? {
        0 => FrameKind::Full,
        1 => FrameKind::Delta,
        k => return Err(format!("bad kind {k}")),
    };
    let flags = r.u8()?;
    if flags & !0b111 != 0 {
        return Err(format!("bad flags {flags:#x} (only bits 0..=2 defined in v1)"));
    }
    let selection_active = flags & 1 != 0;
    let mouse_capture = flags & 2 != 0;
    let alt_screen = flags & 4 != 0;
    let cursor = CursorState {
        row: r.u16()?,
        col: r.u16()?,
        shape: match r.u8()? {
            0 => CursorShape::Block,
            1 => CursorShape::Underline,
            2 => CursorShape::Beam,
            3 => CursorShape::HollowBlock,
            4 => CursorShape::Hidden,
            s => return Err(format!("bad cursor shape {s}")),
        },
        visible: r.u8()? != 0,
    };
    let display_offset = r.u32()? as usize;
    let history_len = r.u32()? as usize;
    // The selection slot is always 13 bytes on the wire (present + two
    // ranges); a present=0 carries zeroed padding that must still be walked.
    let present = r.u8()?;
    let start = point(r.i32()?, r.u16()?);
    let end = point(r.i32()?, r.u16()?);
    let selection = match present {
        0 => None,
        1 => Some((start, end)),
        p => return Err(format!("bad selection present {p}")),
    };

    let row_count = r.u16()? as usize;
    let mut rows = Vec::with_capacity(row_count);
    for _ in 0..row_count {
        let row = r.u16()?;
        let col_start = r.u16()?;
        let cell_count = r.u16()? as usize;
        let mut cells = Vec::with_capacity(cell_count);
        for _ in 0..cell_count {
            let ch = char::from_u32(r.u32()?).ok_or("bad ch")?;
            let fg = color_from_wire(r.u32()?);
            let bg = color_from_wire(r.u32()?);
            let flags = cell_flags_from_bits(r.u16()?);
            let link_id = r.u16()?;
            cells.push(Cell {
                ch,
                fg,
                bg,
                flags,
                link_id,
            });
        }
        rows.push(RowPatch {
            row,
            col_start,
            cells,
        });
    }

    let zw_count = r.u16()? as usize;
    let mut zerowidth: Vec<ZerowidthEntry> = Vec::with_capacity(zw_count);
    for _ in 0..zw_count {
        let row = r.u16()?;
        let col = r.u16()?;
        let n = r.u8()? as usize;
        let mut chars = Vec::with_capacity(n);
        for _ in 0..n {
            chars.push(char::from_u32(r.u32()?).ok_or("bad zw char")?);
        }
        zerowidth.push((row, col, chars));
    }

    let match_count = r.u16()? as usize;
    let mut search_matches = Vec::with_capacity(match_count);
    for _ in 0..match_count {
        let start = point(r.i32()?, r.u16()?);
        let end = point(r.i32()?, r.u16()?);
        search_matches.push((start, end));
    }

    if !r.at_end() {
        return Err(format!("trailing bytes after match section (pos {:#x})", r.pos));
    }

    Ok(FrameData {
        seq,
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
    })
}

/// Unpack a wire color (r<<24 | g<<16 | b<<8 | a) back into `Rgba`.
fn color_from_wire(v: u32) -> Rgba {
    Rgba {
        r: (v >> 24) as u8,
        g: (v >> 16) as u8,
        b: (v >> 8) as u8,
        a: v as u8,
    }
}

// ---- fixture tests --------------------------------------------------------

#[test]
fn fixtures_roundtrip_and_write_bin() {
    let dir = fs::read_dir(FIXTURE_DIR).expect("fixture directory exists");
    let mut files: Vec<_> = dir
        .map(|entry| entry.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    files.sort();

    assert!(
        !files.is_empty(),
        "no fixture JSONs in {FIXTURE_DIR} — run the generator"
    );

    for path in files {
        let text = fs::read_to_string(&path).expect("fixture readable");
        let fixture: Fixture = serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let frame = build_frame(&fixture);

        let mut buf = Vec::new();
        encode_frame(&frame, &mut buf);

        let name = path.file_name().unwrap().to_string_lossy();
        let stem = path.file_stem().unwrap().to_string_lossy();
        assert_eq!(fixture.name, stem, "{name}: fixture name must match filename");
        let bin_path = path.with_extension("bin");
        match fixture.truncate_to {
            Some(n) => {
                // The shipped .bin is the deliberately-cut short version; both
                // decoders must reject it.
                assert!(n < buf.len(), "{name}: truncateTo must cut into data");
                let cut = buf[..n].to_vec();
                fs::write(&bin_path, &cut).unwrap();
                let err = decode_frame(&cut).unwrap_err();
                assert!(
                    err.contains("truncated") || err.contains("magic") || err.contains("version"),
                    "{name}: unexpected error {err}"
                );
            }
            None => {
                fs::write(&bin_path, &buf).unwrap();
                let decoded = decode_frame(&buf).expect("decodes");
                assert_frame_eq(&frame, &decoded, &name);
            }
        }
    }
}

// ---- random property test -------------------------------------------------

/// Deterministic xorshift64 so the property test is reproducible.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }
}

const RANDOM_CHARS: &[char] = &[
    'a', 'Z', '0', ' ', '!', '界', '\u{0301}', '\u{20dd}', '🔥', 'x', 'y', '\u{200b}',
];

const SHAPES: &[CursorShape] = &[
    CursorShape::Block,
    CursorShape::Underline,
    CursorShape::Beam,
    CursorShape::HollowBlock,
    CursorShape::Hidden,
];

fn random_point(rng: &mut Rng, rows: i32, cols: i32) -> Point {
    Point::new(
        Line(rng.below(rows as u64) as i32 - rng.below(5) as i32),
        Column(rng.below(cols as u64) as usize),
    )
}

fn random_cell(rng: &mut Rng) -> Cell {
    let flags = cell_flags_from_bits(rng.below(1 << 13) as u16);
    Cell {
        ch: RANDOM_CHARS[rng.below(RANDOM_CHARS.len() as u64) as usize],
        fg: Rgba::new(
            rng.below(256) as u8,
            rng.below(256) as u8,
            rng.below(256) as u8,
            0xFF,
        ),
        bg: Rgba::new(
            rng.below(256) as u8,
            rng.below(256) as u8,
            rng.below(256) as u8,
            rng.below(2) as u8,
        ),
        flags,
        link_id: rng.below(1 << 12) as u16,
    }
}

fn random_frame(rng: &mut Rng) -> FrameData {
    let rows = 1 + rng.below(6) as usize;
    let cols = 1 + rng.below(40) as i32;

    let mut frame_rows = Vec::new();
    for _ in 0..rng.below(4) {
        let n = rng.below(30);
        frame_rows.push(RowPatch {
            row: rng.below(rows as u64) as u16,
            col_start: rng.below(cols as u64) as u16,
            cells: (0..n).map(|_| random_cell(rng)).collect(),
        });
    }

    let mut zerowidth = Vec::new();
    for _ in 0..rng.below(3) {
        zerowidth.push((
            rng.below(rows as u64) as u16,
            rng.below(cols as u64) as u16,
            (0..rng.below(3))
                .map(|_| RANDOM_CHARS[rng.below(RANDOM_CHARS.len() as u64) as usize])
                .collect(),
        ));
    }

    let mut matches = Vec::new();
    for _ in 0..rng.below(3) {
        matches.push((random_point(rng, rows as i32, cols), random_point(rng, rows as i32, cols)));
    }

    FrameData {
        seq: rng.next() as u32,
        kind: if rng.chance(50) {
            FrameKind::Full
        } else {
            FrameKind::Delta
        },
        cursor: CursorState {
            row: rng.below(rows as u64) as u16,
            col: rng.below(cols as u64) as u16,
            shape: SHAPES[rng.below(SHAPES.len() as u64) as usize],
            visible: rng.chance(80),
        },
        display_offset: rng.below(10_000) as usize,
        history_len: rng.below(100_000) as usize,
        selection: if rng.chance(50) {
            Some((random_point(rng, rows as i32, cols), random_point(rng, rows as i32, cols)))
        } else {
            None
        },
        // Independent of `selection` on purpose: active-but-scrolled-out is a
        // real state (streaming output) and the bit must round-trip alone.
        selection_active: rng.chance(50),
        mouse_capture: rng.chance(50),
        alt_screen: rng.chance(50),
        search_matches: matches,
        rows: frame_rows,
        zerowidth,
    }
}

#[test]
fn roundtrip_random_frames() {
    let mut rng = Rng(0x0DDC_0FFE_5EED_CAFE);
    for i in 0..300 {
        let frame = random_frame(&mut rng);
        let mut buf = Vec::new();
        encode_frame(&frame, &mut buf);
        let decoded = decode_frame(&buf).unwrap_or_else(|e| panic!("case {i}: {e}"));
        assert_frame_eq(&frame, &decoded, &format!("random case {i}"));
    }
}

#[test]
fn malformed_input_rejected() {
    // Zero-length, wrong magic, wrong version.
    assert!(decode_frame(&[]).is_err());
    let mut buf = Vec::new();
    encode_frame(&sample_frame(), &mut buf);
    buf[0] = 0x00;
    assert!(decode_frame(&buf).unwrap_err().contains("magic"));
    buf[0] = FRAME_MAGIC;
    buf[1] = 0x7F;
    assert!(decode_frame(&buf).unwrap_err().contains("version"));

    // Trailing garbage after a valid frame.
    let mut buf = Vec::new();
    encode_frame(&sample_frame(), &mut buf);
    buf.extend_from_slice(&[0xDE, 0xAD]);
    assert!(decode_frame(&buf).is_err());
}

/// Wire rule: bits 0..=2 are selection_active / mouse_capture /
/// alt_screen; ANY reserved bit above bit 2 makes the frame malformed, not
/// silently future-proof.
#[test]
fn flag_bits_above_2_are_malformed() {
    let mut buf = Vec::new();
    encode_frame(&sample_frame(), &mut buf);
    // Flags byte offset: magic(0) version(1) seq(2..6) kind(6) flags(7).
    for bit in [0x08u8, 0x10, 0x40, 0x80] {
        let mut buf = buf.clone();
        buf[7] |= bit;
        assert!(decode_frame(&buf).is_err(), "flags bit {bit:#x} must be rejected");
    }
    // The three defined bits still round-trip.
    let mut buf = Vec::new();
    encode_frame(&sample_frame(), &mut buf);
    buf[7] = 0b111;
    let decoded = decode_frame(&buf).unwrap();
    assert!(decoded.selection_active && decoded.mouse_capture && decoded.alt_screen);
}

/// A tiny deterministic frame used by the malformed-input test.
fn sample_frame() -> FrameData {
    FrameData {
        seq: 1,
        kind: FrameKind::Full,
        cursor: CursorState {
            row: 0,
            col: 1,
            shape: CursorShape::Block,
            visible: true,
        },
        display_offset: 0,
        history_len: 0,
        selection: None,
        selection_active: false,
        mouse_capture: true,
        alt_screen: false,
        search_matches: vec![],
        rows: vec![RowPatch {
            row: 0,
            col_start: 0,
            cells: vec![
                Cell {
                    ch: 'h',
                    fg: Rgba::rgb(0xD8, 0xD8, 0xD8),
                    bg: Rgba::rgb(0x18, 0x18, 0x18),
                    flags: CellFlags::BOLD,
                    link_id: 0,
                },
                Cell {
                    ch: 'i',
                    fg: Rgba::rgb(0xD8, 0xD8, 0xD8),
                    bg: Rgba::rgb(0x18, 0x18, 0x18),
                    flags: CellFlags::EMPTY,
                    link_id: 1,
                },
            ],
        }],
        zerowidth: vec![],
    }
}

// ---- bench note -----------------------------------------------------------

/// Encode a full 200×50 frame (10k cells, the worst realistic case) and
/// report the steady-state cost per frame. Prints with `--nocapture`.
#[test]
fn bench_encode_full_200x50() {
    let frame = build_200x50();
    let mut buf = Vec::new();

    // Warm the buffer's capacity once.
    encode_frame(&frame, &mut buf);

    const ITERS: u32 = 2000;
    let start = Instant::now();
    for _ in 0..ITERS {
        buf.clear();
        encode_frame(&frame, &mut buf);
    }
    let per_frame_us = start.elapsed().as_nanos() as f64 / ITERS as f64 / 1000.0;
    println!(
        "bench_encode_full_200x50: {per_frame_us:.1} µs/frame, {} bytes ({:.0} MiB/s)",
        buf.len(),
        buf.len() as f64 / per_frame_us as f64
    );
}

fn build_200x50() -> FrameData {
    let mut rows = Vec::with_capacity(50);
    for r in 0..50u16 {
        rows.push(RowPatch {
            row: r,
            col_start: 0,
            cells: (0..200)
                .map(|c| Cell {
                    ch: if (r as usize + c) % 2 == 0 { 'a' } else { ' ' },
                    fg: Rgba::new((c % 251) as u8, 0x80, 0x40, 0xFF),
                    bg: Rgba::new(0x18, 0x18, 0x18, 0xFF),
                    flags: if c % 7 == 0 {
                        CellFlags::BOLD | CellFlags::UNDERLINE
                    } else {
                        CellFlags::EMPTY
                    },
                    link_id: (c % 5) as u16,
                })
                .collect(),
        });
    }
    FrameData {
        seq: u32::MAX,
        kind: FrameKind::Full,
        cursor: CursorState {
            row: 49,
            col: 199,
            shape: CursorShape::Block,
            visible: true,
        },
        display_offset: 0,
        history_len: 0,
        selection: None,
        selection_active: false,
        mouse_capture: true,
        alt_screen: true,
        search_matches: vec![],
        rows,
        zerowidth: vec![],
    }
}
