//! Binary frame protocol encoder.
//!
//! Turns a `FrameData` damage snapshot into the v1 wire layout the frontend
//! decodes (`ui/src/term/protocol.ts`). The layout here is the spec — the TS
//! decoder is a faithful mirror, and both sides validate against the same
//! checked-in fixtures (`term-core/tests/fixtures/frames/*.json` + `*.bin`).
//!
//! ## Wire layout (v1) — all integers little-endian
//!
//! ```text
//! magic u8 = 0xD7
//! version u8 = 1
//! seq u32
//! kind u8                 0 = full, 1 = delta
//! flags u8                bit 0 = selection_active (a non-empty selection
//!                         exists, even scrolled out of the viewport — the
//!                         frontend copy gate)
//!                         bit 1 = mouse_capture (any mouse tracking protocol
//!                         active — the frontend suppresses local drag
//!                         selection and wheel→scroll)
//!                         bit 2 = alt_screen (xterm 1049 active)
//!                         bits 3..=7 reserved, 0 in v1
//! cursor_row u16  cursor_col u16  cursor_shape u8  cursor_visible u8
//! display_offset u32      history_len u32
//! selection: present u8, start_row i32, start_col u16, end_row i32, end_col u16
//! row_count u16
//! rows: [row u16 | col_start u16 | cell_count u16 | cell_count × 16B]
//! zerowidth_count u16 | [row u16 | col u16 | n u8 | n × ch u32]
//! match_count u16 | [start_row i32 | start_col u16 | end_row i32 | end_col u16]
//! ```
//!
//! Wire cell = 16 bytes: `ch u32 | fg u32 | bg u32 | flags u16 | link_id u16`.
//! `ch` is a Unicode scalar value. Colors are the resolved RGBA packed
//! `r<<24 | g<<16 | b<<8 | a` so the bytes on the wire read back as the
//! familiar `RRGGBBAA` hex. `link_id` is an index into the actor's hyperlink
//! table (0 = none).
//!
//! Cursor shapes: 0 block, 1 underline, 2 beam, 3 hollow, 4 hidden (matches
//! `alacritty_terminal::vte::ansi::CursorShape`'s variant order).
//!
//! ## CellFlags bit assignments
//!
//! The bits are defined in `actor::CellFlags`; `cell_flags_bits`
//! below exports them onto the wire and `ui/src/term/protocol.ts` mirrors
//! them exactly. Underline kinds ride bits 3..=7.
//!
//! | bit | name            | | bit | name            |
//! |-----|-----------------|-|-----|-----------------|
//! | 0   | bold            | | 7   | dashed_underline|
//! | 1   | italic          | | 8   | inverse         |
//! | 2   | dim             | | 9   | strikeout       |
//! | 3   | underline       | | 10  | hidden          |
//! | 4   | double_underline| | 11  | wide            |
//! | 5   | undercurl       | | 12  | wide_spacer     |
//! | 6   | dotted_underline| |     |                 |

use crate::actor::{CellFlags, CursorShape, FrameData, FrameKind};

/// First byte of every frame; lets the decoder reject foreign byte streams
/// (e.g. an unframed JSON channel) before walking anything.
pub const FRAME_MAGIC: u8 = 0xD7;

/// Version of the frame layout. Bump on any layout change; the decoder
/// refuses versions it does not know.
pub const FRAME_VERSION: u8 = 1;

/// Bytes in the fixed header (through `row_count`): see the module doc.
pub const FRAME_HEADER_LEN: usize = 37;

/// Bytes per wire cell: ch u32 | fg u32 | bg u32 | flags u16 | link_id u16.
pub const CELL_BYTES: usize = 16;

/// Wire bytes in one (start,end) range: start_row i32 | start_col u16 |
/// end_row i32 | end_col u16.
pub const RANGE_BYTES: usize = 12;

/// `(flag, wire bit)` table — the actor's `CellFlags` assignments, exported
/// onto the wire. Shared by the encoder and the decode helper.
pub const CELL_FLAG_WIRE_BITS: [(CellFlags, u16); 13] = [
    (CellFlags::BOLD, 1 << 0),
    (CellFlags::ITALIC, 1 << 1),
    (CellFlags::DIM, 1 << 2),
    (CellFlags::UNDERLINE, 1 << 3),
    (CellFlags::DOUBLE_UNDERLINE, 1 << 4),
    (CellFlags::UNDERCURL, 1 << 5),
    (CellFlags::DOTTED_UNDERLINE, 1 << 6),
    (CellFlags::DASHED_UNDERLINE, 1 << 7),
    (CellFlags::INVERSE, 1 << 8),
    (CellFlags::STRIKEOUT, 1 << 9),
    (CellFlags::HIDDEN, 1 << 10),
    (CellFlags::WIDE, 1 << 11),
    (CellFlags::WIDE_SPACER, 1 << 12),
];

/// Wire value for a `CellFlags`. `CellFlags` keeps its u16 private (the
/// engine owns the type), so the export goes through the public flag constants.
#[inline]
fn cell_flags_bits(flags: CellFlags) -> u16 {
    let mut bits = 0u16;
    for (flag, bit) in CELL_FLAG_WIRE_BITS {
        if flags.contains(flag) {
            bits |= bit;
        }
    }
    bits
}

/// Rebuild a `CellFlags` from wire bits. The inverse of `cell_flags_bits`;
/// the round-trip tests decode through it.
pub fn cell_flags_from_bits(bits: u16) -> CellFlags {
    let mut flags = CellFlags::EMPTY;
    for (flag, bit) in CELL_FLAG_WIRE_BITS {
        if bits & bit != 0 {
            flags = flags | flag;
        }
    }
    flags
}

#[inline]
fn cursor_shape_code(shape: CursorShape) -> u8 {
    match shape {
        CursorShape::Block => 0,
        CursorShape::Underline => 1,
        CursorShape::Beam => 2,
        CursorShape::HollowBlock => 3,
        CursorShape::Hidden => 4,
    }
}

/// Total wire length of one frame, given `f`. Kept in lockstep with the
/// write order in `encode_frame`. Public so embedders can account wire bytes
/// without encoding (the actor's `FrameStats::bytes_sent` uses it).
pub fn encoded_len(f: &FrameData) -> usize {
    let mut len = FRAME_HEADER_LEN;
    for row in &f.rows {
        len += 6 + row.cells.len() * CELL_BYTES;
    }
    len += 2; // zerowidth_count
    for (_, _, chars) in &f.zerowidth {
        len += 5 + chars.len() * 4; // row u16 | col u16 | n u8 | n × ch u32
    }
    len += 2; // match_count
    len += f.search_matches.len() * RANGE_BYTES;
    len
}

#[inline]
fn put_u8(out: &mut [u8], off: &mut usize, v: u8) {
    out[*off] = v;
    *off += 1;
}

#[inline]
fn put_u16(out: &mut [u8], off: &mut usize, v: u16) {
    out[*off..*off + 2].copy_from_slice(&v.to_le_bytes());
    *off += 2;
}

#[inline]
fn put_u32(out: &mut [u8], off: &mut usize, v: u32) {
    out[*off..*off + 4].copy_from_slice(&v.to_le_bytes());
    *off += 4;
}

#[inline]
fn put_i32(out: &mut [u8], off: &mut usize, v: i32) {
    put_u32(out, off, v as u32);
}

/// Pack an `Rgba` into the wire color: `r<<24 | g<<16 | b<<8 | a`, so the
/// LE bytes on the wire spell `RRGGBBAA` when read as a u32.
#[inline]
fn color_u32(color: crate::color::Rgba) -> u32 {
    (u32::from(color.r) << 24)
        | (u32::from(color.g) << 16)
        | (u32::from(color.b) << 8)
        | u32::from(color.a)
}

/// Append one encoded frame to `buf`. `buf` is the caller's (typically a
/// `clear()`ed, capacity-warmed buffer reused every frame).
///
/// Allocation behaviour: `reserve` grows capacity only when needed, and the
/// length is bumped with `set_len` instead of `resize` so a reused buffer is
/// not zero-filled on the hot path (160KB of memset would miss the µs target).
/// Every byte in the extended range is written deterministically below and the
/// trailing `debug_assert_eq!(off, total)` guards against an under-filled gap
/// in debug builds (where this encoder is tested).
pub fn encode_frame(f: &FrameData, buf: &mut Vec<u8>) {
    let base = buf.len();
    let total = encoded_len(f);
    buf.reserve(total);
    let new_len = base + total;
    // SAFETY: every byte in `base..new_len` is written below; `debug_assert_eq!`
    // at the end of the function checks the write cursor reached `total`.
    unsafe {
        buf.set_len(new_len);
    }
    let out = &mut buf[base..new_len];
    let mut off = 0;

    put_u8(out, &mut off, FRAME_MAGIC);
    put_u8(out, &mut off, FRAME_VERSION);
    put_u32(out, &mut off, f.seq);
    put_u8(
        out,
        &mut off,
        match f.kind {
            FrameKind::Full => 0,
            FrameKind::Delta => 1,
        },
    );
    // flags: bit 0 = selection_active, bit 1 = mouse_capture, bit 2 =
    // alt_screen; bits 3..=7 reserved. A layout change bumps the version byte,
    // so unknown bits are malformed, not future.
    let flags = (u8::from(f.selection_active))
        | if f.mouse_capture { 1 << 1 } else { 0 }
        | if f.alt_screen { 1 << 2 } else { 0 };
    put_u8(out, &mut off, flags);

    put_u16(out, &mut off, f.cursor.row);
    put_u16(out, &mut off, f.cursor.col);
    put_u8(out, &mut off, cursor_shape_code(f.cursor.shape));
    put_u8(out, &mut off, f.cursor.visible as u8);
    put_u32(out, &mut off, f.display_offset as u32);
    put_u32(out, &mut off, f.history_len as u32);

    match &f.selection {
        Some((start, end)) => {
            put_u8(out, &mut off, 1);
            put_i32(out, &mut off, start.line.0);
            put_u16(out, &mut off, start.column.0 as u16);
            put_i32(out, &mut off, end.line.0);
            put_u16(out, &mut off, end.column.0 as u16);
        }
        None => {
            put_u8(out, &mut off, 0);
            put_i32(out, &mut off, 0);
            put_u16(out, &mut off, 0);
            put_i32(out, &mut off, 0);
            put_u16(out, &mut off, 0);
        }
    }

    debug_assert!(f.rows.len() <= u16::MAX as usize);
    put_u16(out, &mut off, f.rows.len() as u16);
    for row in &f.rows {
        put_u16(out, &mut off, row.row);
        put_u16(out, &mut off, row.col_start);
        debug_assert!(row.cells.len() <= u16::MAX as usize);
        put_u16(out, &mut off, row.cells.len() as u16);
        for cell in &row.cells {
            put_u32(out, &mut off, cell.ch as u32);
            put_u32(out, &mut off, color_u32(cell.fg));
            put_u32(out, &mut off, color_u32(cell.bg));
            put_u16(out, &mut off, cell_flags_bits(cell.flags));
            put_u16(out, &mut off, cell.link_id);
        }
    }

    debug_assert!(f.zerowidth.len() <= u16::MAX as usize);
    put_u16(out, &mut off, f.zerowidth.len() as u16);
    for (row, col, chars) in &f.zerowidth {
        put_u16(out, &mut off, *row);
        put_u16(out, &mut off, *col);
        debug_assert!(chars.len() <= u8::MAX as usize);
        put_u8(out, &mut off, chars.len() as u8);
        for c in chars {
            put_u32(out, &mut off, *c as u32);
        }
    }

    debug_assert!(f.search_matches.len() <= u16::MAX as usize);
    put_u16(out, &mut off, f.search_matches.len() as u16);
    for (start, end) in &f.search_matches {
        put_i32(out, &mut off, start.line.0);
        put_u16(out, &mut off, start.column.0 as u16);
        put_i32(out, &mut off, end.line.0);
        put_u16(out, &mut off, end.column.0 as u16);
    }

    debug_assert_eq!(off, total, "encoder length must match frame_len");
}
