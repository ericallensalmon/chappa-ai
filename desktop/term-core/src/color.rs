//! Cell color resolution: turn `alacritty_terminal`'s symbolic `Color`
//! (named ANSI, 256-color index, or truecolor) into a flat RGBA the frame
//! protocol can ship to the renderer.
//!
//! The term keeps colors symbolic — `Cell.fg` is a `Color::Named(...)` /
//! `Color::Indexed(...)` / `Color::Spec(...)`, and the RGB values live in
//! `Term::colors()` (a 269-slot table, `None` = "use the default palette").
//! Resolution happens here, in the actor, exactly once per cell per frame,
//! so the frontend never has to know about palettes.
//!
//! The default palette mirrors the xterm-256color layout: 16 base ANSI
//! colors (alacritty's defaults), the 6×6×6 color cube, the 24-step gray
//! ramp, and the fg/bg/cursor/dim slots. When a program sets a color via
//! OSC 10/11/104… the `Some` value in the table wins over the default.

use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color, Rgb};

/// Flat RGBA color used on the wire. `a` is always 255 for terminal cell
/// colors (the renderer applies selection/search/bg-alpha on its side);
/// the channel exists so the wire encoding has a uniform shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    #[inline]
    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    #[inline]
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 0xFF }
    }

    #[inline]
    pub const fn from_rgb(rgb: Rgb) -> Self {
        Self::rgb(rgb.r, rgb.g, rgb.b)
    }
}

/// Resolve a symbolic cell color to RGBA, consulting the term's live color
/// table first and falling back to the default palette.
#[inline]
pub fn resolve(color: Color, colors: &Colors) -> Rgba {
    match color {
        Color::Spec(rgb) => Rgba::from_rgb(rgb),
        Color::Indexed(index) => table_or_default(colors, index as usize),
        Color::Named(named) => table_or_default(colors, named as usize),
    }
}

#[inline]
fn table_or_default(colors: &Colors, index: usize) -> Rgba {
    match colors[index] {
        Some(rgb) => Rgba::from_rgb(rgb),
        None => DEFAULT_PALETTE[index],
    }
}

/// Levels of the xterm 6×6×6 color cube (indices 16..=231).
const CUBE_LEVELS: [u8; 6] = [0x00, 0x5F, 0x87, 0xAF, 0xD7, 0xFF];

/// Default palette, laid out exactly like `alacritty_terminal::term::color`
/// (`Colors`)'s index map: 0..16 base ANSI, 16..232 cube, 232..256 gray
/// ramp, 256 fg, 257 bg, 258 cursor, 259..267 dims, 267 bright fg, 268 dim
/// fg. Values are the alacritty defaults (its own config would populate the
/// table; these only serve when the table slot is unset).
#[rustfmt::skip]
pub const DEFAULT_PALETTE: [Rgba; 269] = {
    let mut p = [Rgba::new(0, 0, 0, 0); 269];

    // Base 16 ANSI colors.
    p[0] = Rgba::rgb(0x18, 0x18, 0x18); // black
    p[1] = Rgba::rgb(0xAC, 0x42, 0x42); // red
    p[2] = Rgba::rgb(0x90, 0xA9, 0x59); // green
    p[3] = Rgba::rgb(0xF4, 0xBF, 0x75); // yellow
    p[4] = Rgba::rgb(0x6A, 0x9F, 0xB5); // blue
    p[5] = Rgba::rgb(0xAA, 0x75, 0x9F); // magenta
    p[6] = Rgba::rgb(0x75, 0xB5, 0xAA); // cyan
    p[7] = Rgba::rgb(0xD8, 0xD8, 0xD8); // white
    p[8] = Rgba::rgb(0x6B, 0x6B, 0x6B); // bright black
    p[9] = Rgba::rgb(0xC5, 0x55, 0x55); // bright red
    p[10] = Rgba::rgb(0xAA, 0xC4, 0x74); // bright green
    p[11] = Rgba::rgb(0xFE, 0xCA, 0x88); // bright yellow
    p[12] = Rgba::rgb(0x82, 0xB8, 0xC8); // bright blue
    p[13] = Rgba::rgb(0xC2, 0x8C, 0xB8); // bright magenta
    p[14] = Rgba::rgb(0x93, 0xD3, 0xC3); // bright cyan
    p[15] = Rgba::rgb(0xF8, 0xF8, 0xF8); // bright white

    // Color cube: index 16 + 36r + 6g + b.
    let mut i = 16;
    let mut r = 0;
    while r < 6 {
        let mut g = 0;
        while g < 6 {
            let mut b = 0;
            while b < 6 {
                p[i] = Rgba::rgb(CUBE_LEVELS[r], CUBE_LEVELS[g], CUBE_LEVELS[b]);
                i += 1;
                b += 1;
            }
            g += 1;
        }
        r += 1;
    }

    // Grayscale ramp: 8 + 10*k for k in 0..24.
    let mut k = 0;
    while k < 24 {
        let v = 8 + 10 * k;
        p[i] = Rgba::rgb(v, v, v);
        i += 1;
        k += 1;
    }

    // Special slots.
    p[256] = Rgba::rgb(0xD8, 0xD8, 0xD8); // foreground
    p[257] = Rgba::rgb(0x18, 0x18, 0x18); // background
    p[258] = Rgba::rgb(0xD8, 0xD8, 0xD8); // cursor
    p[259] = Rgba::rgb(0x0F, 0x0F, 0x0F); // dim black
    p[260] = Rgba::rgb(0x71, 0x2B, 0x2B); // dim red
    p[261] = Rgba::rgb(0x5F, 0x6F, 0x3A); // dim green
    p[262] = Rgba::rgb(0xA1, 0x7E, 0x4D); // dim yellow
    p[263] = Rgba::rgb(0x45, 0x68, 0x77); // dim blue
    p[264] = Rgba::rgb(0x70, 0x4D, 0x68); // dim magenta
    p[265] = Rgba::rgb(0x4D, 0x77, 0x70); // dim cyan
    p[266] = Rgba::rgb(0x8E, 0x8E, 0x8E); // dim white
    p[267] = Rgba::rgb(0xD8, 0xD8, 0xD8); // bright foreground
    p[268] = Rgba::rgb(0x8E, 0x8E, 0x8E); // dim foreground

    p
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        assert_eq!(DEFAULT_PALETTE[0], Rgba::rgb(0x18, 0x18, 0x18));
        assert_eq!(DEFAULT_PALETTE[1], Rgba::rgb(0xAC, 0x42, 0x42));
        // Cube: index 196 = 16 + 5*36 → r at level 5 (0xFF), g/b at level 0.
        assert_eq!(DEFAULT_PALETTE[196], Rgba::rgb(0xFF, 0x00, 0x00));
        assert_eq!(DEFAULT_PALETTE[231], Rgba::rgb(0xFF, 0xFF, 0xFF));
        assert_eq!(DEFAULT_PALETTE[232], Rgba::rgb(0x08, 0x08, 0x08));
        assert_eq!(DEFAULT_PALETTE[255], Rgba::rgb(0xEE, 0xEE, 0xEE));
        assert_eq!(DEFAULT_PALETTE[256], Rgba::rgb(0xD8, 0xD8, 0xD8));
        assert_eq!(DEFAULT_PALETTE[257], Rgba::rgb(0x18, 0x18, 0x18));
    }
}
