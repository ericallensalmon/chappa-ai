//! Semantic input events → terminal escape bytes.
//!
//! Pure functions driven by a live terminal-mode snapshot. Encoding follows
//! xterm: unmodified arrows/Home/End take the application-cursor (DECCKM)
//! form when set; any modifier sends the `CSI 1;<mods+1>` form instead. Mouse
//! paths honour the active tracking protocol (1000/1002/1003) and SGR vs
//! legacy X10, and wheel-on-alt-screen falls back to repeated arrows when no
//! mouse protocol is active (the alt-screen scroll story).
//! `app_keypad` is snapshot for the mode plumbing but unused in v1 (we do not
//! advertise the kitty keyboard protocol; the mode leaves room for it).

/// Modifier bitmask. Bit values match xterm's modifier parameter encoding
/// (shift=1, alt=2, ctrl=4, super=8), so `bits() + 1` is the `CSI 1;<n>`
/// parameter and `bits() << 2` the SGR/X10 mouse modifier field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mods(u8);

impl Mods {
    pub const SHIFT: Mods = Mods(1 << 0);
    pub const ALT: Mods = Mods(1 << 1);
    pub const CTRL: Mods = Mods(1 << 2);
    pub const SUPER: Mods = Mods(1 << 3);
    pub const EMPTY: Mods = Mods(0);

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub const fn bits(self) -> u8 {
        self.0
    }
}

impl std::ops::BitOr for Mods {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

/// A physical/semantic key, independent of terminal modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Tab,
    Backspace,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    F(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyEvent {
    pub key: Key,
    pub mods: Mods,
}

/// Terminal state read off `Term` by the actor at encode time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeSnapshot {
    /// DECCKM: unmodified arrows/Home/End use SS3 when set.
    pub app_cursor: bool,
    /// DECKPAM: reserved for keypad encoding (unused in v1).
    pub app_keypad: bool,
    /// DEC 2004: wrap pastes in `ESC[200~ … ESC[201~`.
    pub bracketed_paste: bool,
    /// Active mouse tracking protocol (1000 / 1002 / 1003).
    pub mouse: MouseProto,
    /// DEC 1006: SGR mouse encoding instead of legacy X10.
    pub mouse_sgr: bool,
    /// ALTSHM (xterm alternate screen): wheel → arrows when no mouse proto.
    pub alt_screen: bool,
    /// Kitty keyboard-protocol flag 0b1 (disambiguate escape codes) active
    ///. The affected keys — Esc, modified Enter/Tab/Backspace,
    /// modified printables — switch to the `CSI code;mods u` form.
    pub kitty_disambiguate: bool,
    /// Scroll wheel speed, `1x…6x`, default 3x: how many arrow
    /// repeats one wheel notch becomes on the alt-screen wheel→arrows path
    /// (threaded it through from `ActorConfig`/settings; it used to
    /// be the literal 3). "The multiplier applies ONLY when a TUI app
    /// captures scroll input … normal scrollback scrolls at system speed",
    /// so NOTHING else in this module reads it.
    pub wheel_speed: u8,
}

impl Default for ModeSnapshot {
    /// All modes off, wheel speed at the settings default (3).
    fn default() -> Self {
        Self {
            app_cursor: false,
            app_keypad: false,
            bracketed_paste: false,
            mouse: MouseProto::None,
            mouse_sgr: false,
            alt_screen: false,
            kitty_disambiguate: false,
            wheel_speed: 3,
        }
    }
}

/// Mouse tracking protocols, mutually exclusive per xterm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseProto {
    None,
    /// 1000 — click tracking only.
    Normal,
    /// 1002 — button-event tracking (motion while a button is held).
    ButtonDrag,
    /// 1003 — any-motion tracking.
    AnyMotion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseKind {
    Press,
    Release,
    Drag,
    Move,
    WheelUp,
    WheelDown,
}

/// A mouse event in viewport coordinates (0-based `col`/`row`, as everywhere
/// else in the app; encoders add 1 for the wire). `button` is 0/1/2 for
/// left/middle/right on Press/Drag; ignored for wheel kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseEvent {
    pub kind: MouseKind,
    pub button: u8,
    pub col: u16,
    pub row: u16,
    pub mods: Mods,
}

/// Encode one key event. Returns `None` when the terminal shouldn't see it
/// (unencodable Ctrl combos, F-keys past F12) so the frontend keeps it local.
pub fn encode_key(ev: &KeyEvent, m: &ModeSnapshot) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(8);
    match ev.key {
        Key::Char(c) => {
            // Kitty disambiguate: a printable with any non-shift
            // modifier rides `CSI <codepoint>;<mods> u` so the terminal can
            // tell Ctrl+Shift+A from Ctrl+A. Shift-only printables stay plain
            // (the shifted char already encodes them). Non-ASCII keeps the
            // legacy alt-prefix form (kitty's UTF-8 parameter encoding is out
            // of the minimal scope).
            if m.kitty_disambiguate && ev.mods.bits() & !Mods::SHIFT.bits() != 0 && c.is_ascii() {
                return Some(encode_csi_u(c as u32, ev.mods));
            }
            if ev.mods.contains(Mods::CTRL) {
                let code = ctrl_code(c)?;
                if ev.mods.contains(Mods::ALT) {
                    out.push(0x1B);
                }
                out.push(code);
            } else {
                if ev.mods.contains(Mods::ALT) {
                    out.push(0x1B);
                }
                out.extend(c.encode_utf8(&mut [0; 4]).as_bytes());
            }
            Some(out)
        }
        Key::Enter => {
            if m.kitty_disambiguate && !ev.mods.is_empty() {
                return Some(encode_csi_u(13, ev.mods));
            }
            if ev.mods.contains(Mods::ALT) {
                out.push(0x1B);
            }
            out.push(b'\r');
            Some(out)
        }
        Key::Tab => {
            if m.kitty_disambiguate && !ev.mods.is_empty() {
                return Some(encode_csi_u(9, ev.mods));
            }
            if ev.mods.contains(Mods::SHIFT) {
                out.extend_from_slice(b"\x1b[Z");
                Some(out)
            } else {
                if ev.mods.contains(Mods::ALT) {
                    out.push(0x1B);
                }
                out.push(b'\t');
                Some(out)
            }
        }
        Key::Backspace => {
            if m.kitty_disambiguate && !ev.mods.is_empty() {
                return Some(encode_csi_u(127, ev.mods));
            }
            if ev.mods.contains(Mods::ALT) {
                out.push(0x1B);
            }
            out.push(if ev.mods.contains(Mods::CTRL) {
                0x08
            } else {
                0x7F
            });
            Some(out)
        }
        Key::Escape => {
            if m.kitty_disambiguate {
                return Some(encode_csi_u(27, ev.mods));
            }
            if ev.mods.contains(Mods::ALT) {
                out.push(0x1B);
            }
            out.push(0x1B);
            Some(out)
        }
        Key::Up => arrow(b'A', ev.mods, m),
        Key::Down => arrow(b'B', ev.mods, m),
        Key::Right => arrow(b'C', ev.mods, m),
        Key::Left => arrow(b'D', ev.mods, m),
        Key::Home => arrow(b'H', ev.mods, m),
        Key::End => arrow(b'F', ev.mods, m),
        Key::Insert => tilde_key(2, ev.mods),
        Key::Delete => tilde_key(3, ev.mods),
        Key::PageUp => tilde_key(5, ev.mods),
        Key::PageDown => tilde_key(6, ev.mods),
        Key::F(n) => f_key(n, ev.mods),
    }
}

/// `CSI <code>;<mods> u` — the kitty disambiguate encoding. The modifier
/// parameter is the `Mods` bitmask (shift=1 alt=2 ctrl=4 super=8) PLUS ONE,
/// exactly like xterm's modifyOtherKeys: shift+enter is `13;2u`. An earlier
/// version sent the bitmask verbatim ("minus the +1") — a kitty parser
/// reads `13;1u` as UNMODIFIED enter, so Shift+Enter still submitted
/// (host-run bug; spec: "modifiers … with an added 1").
fn encode_csi_u(code: u32, mods: Mods) -> Vec<u8> {
    if mods.is_empty() {
        format!("\x1b[{code}u").into_bytes()
    } else {
        format!("\x1b[{code};{}u", mods.bits() + 1).into_bytes()
    }
}

/// Arrows/Home/End. Unmodified: SS3 under DECCKM, CSI otherwise. Any modifier
/// overrides to the modifyOtherKeys-style `CSI 1;<mods+1><final>`.
fn arrow(final_byte: u8, mods: Mods, m: &ModeSnapshot) -> Option<Vec<u8>> {
    let final_byte = final_byte as char;
    if mods.is_empty() {
        if m.app_cursor {
            Some(format!("\x1bO{final_byte}").into_bytes())
        } else {
            Some(format!("\x1b[{final_byte}").into_bytes())
        }
    } else {
        Some(format!("\x1b[1;{}{final_byte}", mods.bits() + 1).into_bytes())
    }
}

/// Insert/Delete/PageUp/PageDown: `CSI <code>~` with the modifier parameter
/// inserted before the tilde when set.
fn tilde_key(code: u8, mods: Mods) -> Option<Vec<u8>> {
    if mods.is_empty() {
        Some(format!("\x1b[{code}~").into_bytes())
    } else {
        Some(format!("\x1b[{code};{}~", mods.bits() + 1).into_bytes())
    }
}

/// F1–F12. Unmodified F1–F4 use SS3 P/Q/R/S; F5+ use tilde sequences
/// (15/17/18/19/20/21/23/24). Modifiers switch F1–F4 to `CSI 1;<n><final>`
/// and insert the parameter in the tilde form. F13+ have no v1 mapping.
fn f_key(n: u8, mods: Mods) -> Option<Vec<u8>> {
    match n {
        1..=4 => {
            let final_byte = (b'P' + n - 1) as char;
            if mods.is_empty() {
                Some(format!("\x1bO{final_byte}").into_bytes())
            } else {
                Some(format!("\x1b[1;{}{final_byte}", mods.bits() + 1).into_bytes())
            }
        }
        5..=12 => {
            let code = match n {
                5 => 15,
                6 => 17,
                7 => 18,
                8 => 19,
                9 => 20,
                10 => 21,
                11 => 23,
                12 => 24,
                _ => unreachable!(),
            };
            tilde_key(code, mods)
        }
        _ => None,
    }
}

/// Ctrl+char → control byte per xterm. Letters mask to 0x01..=0x1A; the
/// punctuation set is mapped explicitly (Ctrl+Space/`@` = NUL, `[`=ESC,
/// `\`=FS, `]`=GS, `^`=RS, `_`=US, `?`=DEL). Unmappable chars (e.g. non-ASCII
/// under Ctrl) return `None`.
fn ctrl_code(c: char) -> Option<u8> {
    if c.is_ascii_alphabetic() {
        Some((c.to_ascii_lowercase() as u8) & 0x1F)
    } else {
        match c {
            ' ' | '@' => Some(0x00),
            '[' => Some(0x1B),
            '\\' => Some(0x1C),
            ']' => Some(0x1D),
            '^' => Some(0x1E),
            '_' => Some(0x1F),
            '?' => Some(0x7F),
            _ => None,
        }
    }
}

/// Encode pasted text. Wrapped in bracketed-paste markers iff the mode is on.
/// Regardless of mode, any embedded `ESC[200~`/`ESC[201~` is stripped to a
/// fixed point (paste injection guard: a pasted terminator would close the
/// wrapper early and let the rest be read as live terminal input). A single
/// `str::replace` pass is bypassable by reassembly — e.g. `"\x1b[20" +
/// "\x1b[201~" + "1~"` rejoins into a live `ESC[201~` once the inner marker is
/// removed — so the loop reruns until neither marker occurs anywhere. The loop
/// terminates because every iteration strictly shrinks the string.
/// `\r\n`/`\n` then become `\r`.
pub fn encode_paste(text: &str, m: &ModeSnapshot) -> Vec<u8> {
    let mut sanitized = text.to_owned();
    loop {
        let stripped = sanitized.replace("\x1b[201~", "").replace("\x1b[200~", "");
        if stripped == sanitized {
            break;
        }
        sanitized = stripped;
    }
    let sanitized = sanitized.replace("\r\n", "\r").replace('\n', "\r");
    if m.bracketed_paste {
        let mut out = Vec::with_capacity(sanitized.len() + 8);
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(sanitized.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        sanitized.into_bytes()
    }
}

/// Encode a mouse event. Returns `None` when the terminal shouldn't see it:
/// motion without AnyMotion, drag below ButtonDrag, any tracking event with
/// no protocol, and wheel with no protocol off the alt screen (the frontend
/// scrolls scrollback itself there).
pub fn encode_mouse(ev: &MouseEvent, m: &ModeSnapshot) -> Option<Vec<u8>> {
    match ev.kind {
        MouseKind::Move => {
            if m.mouse != MouseProto::AnyMotion {
                return None;
            }
        }
        MouseKind::Drag => {
            if m.mouse != MouseProto::AnyMotion && m.mouse != MouseProto::ButtonDrag {
                return None;
            }
        }
        MouseKind::WheelUp | MouseKind::WheelDown => {
            if m.mouse == MouseProto::None {
                if m.alt_screen {
                    return Some(alt_screen_wheel(ev.kind, m));
                }
                return None;
            }
        }
        MouseKind::Press | MouseKind::Release => {
            if m.mouse == MouseProto::None {
                return None;
            }
        }
    }
    Some(if m.mouse_sgr {
        encode_mouse_sgr(ev)
    } else {
        encode_mouse_x10(ev)
    })
}

/// Alt-screen wheel with no mouse protocol: the TUI sees arrow keys, repeated
/// `m.wheel_speed` times (the multiplier used to be the literal 3
/// here, "the ×3 multiplier is NOT applied in `input.ts` — it is baked
/// Rust-side in `term-core/src/keys.rs::alt_screen_wheel`"). This is the ONLY
/// reader of `wheel_speed`: the TUI-only rule stands.
fn alt_screen_wheel(kind: MouseKind, m: &ModeSnapshot) -> Vec<u8> {
    let key = match kind {
        MouseKind::WheelUp => Key::Up,
        MouseKind::WheelDown => Key::Down,
        _ => unreachable!(),
    };
    let ev = KeyEvent {
        key,
        mods: Mods::EMPTY,
    };
    // Settings clamps to 1..=6 before this ever runs; the floor guards a
    // hand-built snapshot with 0, which would silently make the wheel dead.
    let repeats = m.wheel_speed.max(1);
    let mut out = Vec::with_capacity(4 * repeats as usize);
    for _ in 0..repeats {
        out.extend(encode_key(&ev, m).expect("arrow always encodable"));
    }
    out
}

/// `CSI < b;x;y (M|m)` — no coordinate clamping. `b` is the button code plus
/// motion bit (32) for Drag, plus 3 for no-button motion, plus modifiers
/// shifted left two bits.
fn encode_mouse_sgr(ev: &MouseEvent) -> Vec<u8> {
    let mut b = match ev.kind {
        MouseKind::Press => ev.button,
        MouseKind::Release => 3,
        MouseKind::Drag => 32 + ev.button,
        MouseKind::Move => 35,
        MouseKind::WheelUp => 64,
        MouseKind::WheelDown => 65,
    };
    b |= ev.mods.bits() << 2;
    let final_byte = match ev.kind {
        MouseKind::Release => 'm',
        _ => 'M',
    };
    format!("\x1b[<{b};{};{}{final_byte}", ev.col + 1, ev.row + 1).into_bytes()
}

/// Legacy X10: `CSI M <b> <col+33> <row+33>` with the coordinate bytes clamped
/// at 223 (0xFF − 0x20) per xterm.
fn encode_mouse_x10(ev: &MouseEvent) -> Vec<u8> {
    let mut b = match ev.kind {
        MouseKind::Press => 32 + ev.button,
        MouseKind::Release => 35,
        MouseKind::Drag => 32 + ev.button,
        MouseKind::Move => 35,
        MouseKind::WheelUp => 96,
        MouseKind::WheelDown => 97,
    };
    b |= ev.mods.bits() << 2;
    let mut out = Vec::with_capacity(5);
    out.extend_from_slice(b"\x1b[M");
    out.push(b);
    out.push(x10_coord(ev.col));
    out.push(x10_coord(ev.row));
    out
}

/// X10 coordinate byte: 1-based coordinate + 0x20, clamped at 223.
fn x10_coord(v: u16) -> u8 {
    (u32::from(v) + 0x21).min(223) as u8
}
