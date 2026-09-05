//! Table-driven encoder tests. Every encoding rule appears here in both
//! mode polarities where relevant; expected bytes are written as escaped
//! string literals so failures read clearly.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use term_core::actor::{spawn_actor_nopty, ActorConfig, TermEvent};
use term_core::keys::{
    encode_key, encode_mouse, encode_paste, Key, KeyEvent, ModeSnapshot, Mods, MouseEvent,
    MouseKind, MouseProto,
};
use term_core::pty::PtySpec;

/// All-modes-off snapshot (normal terminal, no mouse, no paste wrapping).
fn snap() -> ModeSnapshot {
    ModeSnapshot {
        app_cursor: false,
        app_keypad: false,
        bracketed_paste: false,
        mouse: MouseProto::None,
        mouse_sgr: false,
        alt_screen: false,
        kitty_disambiguate: false,
        // the default (3x) — the alt-screen wheel→arrows multiplier
        // (threaded it here from settings).
        wheel_speed: 3,
    }
}

fn app_cursor() -> ModeSnapshot {
    ModeSnapshot {
        app_cursor: true,
        ..snap()
    }
}

/// Kitty disambiguate on (flag 0b1 pushed), everything else off.
fn kitty() -> ModeSnapshot {
    ModeSnapshot {
        kitty_disambiguate: true,
        ..snap()
    }
}

struct KCase {
    name: &'static str,
    ev: KeyEvent,
    mode: ModeSnapshot,
    want: Option<&'static [u8]>,
}

fn check_keys(cases: &[KCase]) {
    for (i, c) in cases.iter().enumerate() {
        let got = encode_key(&c.ev, &c.mode);
        assert_eq!(
            got.as_deref(),
            c.want,
            "key case #{i} ({}) ev={:?} mode={:?}",
            c.name,
            c.ev,
            c.mode
        );
    }
}

fn key(key: Key, mods: Mods) -> KeyEvent {
    KeyEvent { key, mods }
}

fn char(c: char, mods: Mods) -> KeyEvent {
    key(Key::Char(c), mods)
}

// ---- plain chars + UTF-8 ---------------------------------------------------

#[test]
fn keys_plain_chars() {
    check_keys(&[
        KCase {
            name: "lowercase",
            ev: char('a', Mods::EMPTY),
            mode: snap(),
            want: Some(b"a"),
        },
        KCase {
            name: "uppercase",
            ev: char('A', Mods::EMPTY),
            mode: snap(),
            want: Some(b"A"),
        },
        KCase {
            name: "space",
            ev: char(' ', Mods::EMPTY),
            mode: snap(),
            want: Some(b" "),
        },
        KCase {
            name: "digit",
            ev: char('7', Mods::EMPTY),
            mode: snap(),
            want: Some(b"7"),
        },
        KCase {
            name: "punct",
            ev: char('~', Mods::EMPTY),
            mode: snap(),
            want: Some(b"~"),
        },
        KCase {
            name: "shifted char passes through",
            ev: char('A', Mods::SHIFT),
            mode: snap(),
            want: Some(b"A"),
        },
        KCase {
            name: "latin-1 supplement utf8",
            ev: char('é', Mods::EMPTY),
            mode: snap(),
            want: Some("\u{e9}".as_bytes()),
        },
        KCase {
            name: "cjk utf8",
            ev: char('中', Mods::EMPTY),
            mode: snap(),
            want: Some("中".as_bytes()),
        },
    ]);
}

// ---- Ctrl ---------------------------------------------------------------

#[test]
fn keys_ctrl_codes() {
    check_keys(&[
        KCase {
            name: "ctrl+a",
            ev: char('a', Mods::CTRL),
            mode: snap(),
            want: Some(b"\x01"),
        },
        KCase {
            name: "ctrl+z",
            ev: char('z', Mods::CTRL),
            mode: snap(),
            want: Some(b"\x1a"),
        },
        KCase {
            name: "ctrl+A == ctrl+a",
            ev: char('A', Mods::CTRL),
            mode: snap(),
            want: Some(b"\x01"),
        },
        KCase {
            name: "ctrl+space is NUL",
            ev: char(' ', Mods::CTRL),
            mode: snap(),
            want: Some(b"\x00"),
        },
        KCase {
            name: "ctrl+@ is NUL",
            ev: char('@', Mods::CTRL),
            mode: snap(),
            want: Some(b"\x00"),
        },
        KCase {
            name: "ctrl+[ is ESC",
            ev: char('[', Mods::CTRL),
            mode: snap(),
            want: Some(b"\x1b"),
        },
        KCase {
            name: "ctrl+\\ is FS",
            ev: char('\\', Mods::CTRL),
            mode: snap(),
            want: Some(b"\x1c"),
        },
        KCase {
            name: "ctrl+] is GS",
            ev: char(']', Mods::CTRL),
            mode: snap(),
            want: Some(b"\x1d"),
        },
        KCase {
            name: "ctrl+^ is RS",
            ev: char('^', Mods::CTRL),
            mode: snap(),
            want: Some(b"\x1e"),
        },
        KCase {
            name: "ctrl+_ is US",
            ev: char('_', Mods::CTRL),
            mode: snap(),
            want: Some(b"\x1f"),
        },
        KCase {
            name: "ctrl+? is DEL",
            ev: char('?', Mods::CTRL),
            mode: snap(),
            want: Some(b"\x7f"),
        },
        KCase {
            name: "ctrl+non-ascii unencodable",
            ev: char('é', Mods::CTRL),
            mode: snap(),
            want: None,
        },
        KCase {
            name: "ctrl+alt+a: alt prefix",
            ev: char('a', Mods::CTRL | Mods::ALT),
            mode: snap(),
            want: Some(b"\x1b\x01"),
        },
    ]);
}

// ---- Alt ---------------------------------------------------------------

#[test]
fn keys_alt() {
    check_keys(&[
        KCase {
            name: "alt+a",
            ev: char('a', Mods::ALT),
            mode: snap(),
            want: Some(b"\x1ba"),
        },
        KCase {
            name: "alt+space",
            ev: char(' ', Mods::ALT),
            mode: snap(),
            want: Some(b"\x1b "),
        },
        KCase {
            name: "alt+utf8",
            ev: char('é', Mods::ALT),
            mode: snap(),
            want: Some(b"\x1b\xc3\xa9"),
        },
    ]);
}

// ---- Backspace / Enter / Tab / Escape ---------------------------------------

#[test]
fn keys_editing() {
    check_keys(&[
        KCase {
            name: "backspace is DEL",
            ev: key(Key::Backspace, Mods::EMPTY),
            mode: snap(),
            want: Some(b"\x7f"),
        },
        KCase {
            name: "ctrl+backspace is BS",
            ev: key(Key::Backspace, Mods::CTRL),
            mode: snap(),
            want: Some(b"\x08"),
        },
        KCase {
            name: "alt+backspace",
            ev: key(Key::Backspace, Mods::ALT),
            mode: snap(),
            want: Some(b"\x1b\x7f"),
        },
        KCase {
            name: "enter is CR",
            ev: key(Key::Enter, Mods::EMPTY),
            mode: snap(),
            want: Some(b"\r"),
        },
        KCase {
            name: "alt+enter is ESC CR",
            ev: key(Key::Enter, Mods::ALT),
            mode: snap(),
            want: Some(b"\x1b\r"),
        },
        KCase {
            name: "ctrl+enter stays CR",
            ev: key(Key::Enter, Mods::CTRL),
            mode: snap(),
            want: Some(b"\r"),
        },
        KCase {
            name: "tab is HT",
            ev: key(Key::Tab, Mods::EMPTY),
            mode: snap(),
            want: Some(b"\t"),
        },
        KCase {
            name: "shift+tab is CSI Z",
            ev: key(Key::Tab, Mods::SHIFT),
            mode: snap(),
            want: Some(b"\x1b[Z"),
        },
        KCase {
            name: "ctrl+tab stays HT",
            ev: key(Key::Tab, Mods::CTRL),
            mode: snap(),
            want: Some(b"\t"),
        },
        KCase {
            name: "escape",
            ev: key(Key::Escape, Mods::EMPTY),
            mode: snap(),
            want: Some(b"\x1b"),
        },
        KCase {
            name: "alt+escape",
            ev: key(Key::Escape, Mods::ALT),
            mode: snap(),
            want: Some(b"\x1b\x1b"),
        },
    ]);
}

// ---- Arrows / Home / End ----------------------------------------------------

#[test]
fn keys_arrows_home_end() {
    check_keys(&[
        KCase {
            name: "up",
            ev: key(Key::Up, Mods::EMPTY),
            mode: snap(),
            want: Some(b"\x1b[A"),
        },
        KCase {
            name: "down",
            ev: key(Key::Down, Mods::EMPTY),
            mode: snap(),
            want: Some(b"\x1b[B"),
        },
        KCase {
            name: "right",
            ev: key(Key::Right, Mods::EMPTY),
            mode: snap(),
            want: Some(b"\x1b[C"),
        },
        KCase {
            name: "left",
            ev: key(Key::Left, Mods::EMPTY),
            mode: snap(),
            want: Some(b"\x1b[D"),
        },
        KCase {
            name: "up app-cursor SS3",
            ev: key(Key::Up, Mods::EMPTY),
            mode: app_cursor(),
            want: Some(b"\x1bOA"),
        },
        KCase {
            name: "down app-cursor SS3",
            ev: key(Key::Down, Mods::EMPTY),
            mode: app_cursor(),
            want: Some(b"\x1bOB"),
        },
        KCase {
            name: "right app-cursor SS3",
            ev: key(Key::Right, Mods::EMPTY),
            mode: app_cursor(),
            want: Some(b"\x1bOC"),
        },
        KCase {
            name: "left app-cursor SS3",
            ev: key(Key::Left, Mods::EMPTY),
            mode: app_cursor(),
            want: Some(b"\x1bOD"),
        },
        KCase {
            name: "shift+up modifier param",
            ev: key(Key::Up, Mods::SHIFT),
            mode: snap(),
            want: Some(b"\x1b[1;2A"),
        },
        KCase {
            name: "shift+up overrides app-cursor",
            ev: key(Key::Up, Mods::SHIFT),
            mode: app_cursor(),
            want: Some(b"\x1b[1;2A"),
        },
        KCase {
            name: "ctrl+left",
            ev: key(Key::Left, Mods::CTRL),
            mode: snap(),
            want: Some(b"\x1b[1;5D"),
        },
        KCase {
            name: "alt+up",
            ev: key(Key::Up, Mods::ALT),
            mode: snap(),
            want: Some(b"\x1b[1;3A"),
        },
        KCase {
            name: "super+up",
            ev: key(Key::Up, Mods::SUPER),
            mode: snap(),
            want: Some(b"\x1b[1;9A"),
        },
        KCase {
            name: "shift+ctrl+alt+left",
            ev: key(Key::Left, Mods::SHIFT | Mods::CTRL | Mods::ALT),
            mode: snap(),
            want: Some(b"\x1b[1;8D"),
        },
        KCase {
            name: "home",
            ev: key(Key::Home, Mods::EMPTY),
            mode: snap(),
            want: Some(b"\x1b[H"),
        },
        KCase {
            name: "home app-cursor SS3",
            ev: key(Key::Home, Mods::EMPTY),
            mode: app_cursor(),
            want: Some(b"\x1bOH"),
        },
        KCase {
            name: "end",
            ev: key(Key::End, Mods::EMPTY),
            mode: snap(),
            want: Some(b"\x1b[F"),
        },
        KCase {
            name: "end app-cursor SS3",
            ev: key(Key::End, Mods::EMPTY),
            mode: app_cursor(),
            want: Some(b"\x1bOF"),
        },
        KCase {
            name: "ctrl+home",
            ev: key(Key::Home, Mods::CTRL),
            mode: snap(),
            want: Some(b"\x1b[1;5H"),
        },
        KCase {
            name: "ctrl+end",
            ev: key(Key::End, Mods::CTRL),
            mode: snap(),
            want: Some(b"\x1b[1;5F"),
        },
    ]);
}

// ---- Insert / Delete / PageUp / PageDown --------------------------------------

#[test]
fn keys_edit_keys() {
    check_keys(&[
        KCase {
            name: "insert",
            ev: key(Key::Insert, Mods::EMPTY),
            mode: snap(),
            want: Some(b"\x1b[2~"),
        },
        KCase {
            name: "delete",
            ev: key(Key::Delete, Mods::EMPTY),
            mode: snap(),
            want: Some(b"\x1b[3~"),
        },
        KCase {
            name: "pageup",
            ev: key(Key::PageUp, Mods::EMPTY),
            mode: snap(),
            want: Some(b"\x1b[5~"),
        },
        KCase {
            name: "pagedown",
            ev: key(Key::PageDown, Mods::EMPTY),
            mode: snap(),
            want: Some(b"\x1b[6~"),
        },
        KCase {
            name: "shift+delete",
            ev: key(Key::Delete, Mods::SHIFT),
            mode: snap(),
            want: Some(b"\x1b[3;2~"),
        },
        KCase {
            name: "ctrl+pageup",
            ev: key(Key::PageUp, Mods::CTRL),
            mode: snap(),
            want: Some(b"\x1b[5;5~"),
        },
        KCase {
            name: "alt+insert",
            ev: key(Key::Insert, Mods::ALT),
            mode: snap(),
            want: Some(b"\x1b[2;3~"),
        },
    ]);
}

// ---- Function keys ------------------------------------------------------------

#[test]
fn keys_function() {
    check_keys(&[
        KCase {
            name: "f1",
            ev: key(Key::F(1), Mods::EMPTY),
            mode: snap(),
            want: Some(b"\x1bOP"),
        },
        KCase {
            name: "f2",
            ev: key(Key::F(2), Mods::EMPTY),
            mode: snap(),
            want: Some(b"\x1bOQ"),
        },
        KCase {
            name: "f3",
            ev: key(Key::F(3), Mods::EMPTY),
            mode: snap(),
            want: Some(b"\x1bOR"),
        },
        KCase {
            name: "f4",
            ev: key(Key::F(4), Mods::EMPTY),
            mode: snap(),
            want: Some(b"\x1bOS"),
        },
        KCase {
            name: "f5",
            ev: key(Key::F(5), Mods::EMPTY),
            mode: snap(),
            want: Some(b"\x1b[15~"),
        },
        KCase {
            name: "f6",
            ev: key(Key::F(6), Mods::EMPTY),
            mode: snap(),
            want: Some(b"\x1b[17~"),
        },
        KCase {
            name: "f7",
            ev: key(Key::F(7), Mods::EMPTY),
            mode: snap(),
            want: Some(b"\x1b[18~"),
        },
        KCase {
            name: "f8",
            ev: key(Key::F(8), Mods::EMPTY),
            mode: snap(),
            want: Some(b"\x1b[19~"),
        },
        KCase {
            name: "f9",
            ev: key(Key::F(9), Mods::EMPTY),
            mode: snap(),
            want: Some(b"\x1b[20~"),
        },
        KCase {
            name: "f10",
            ev: key(Key::F(10), Mods::EMPTY),
            mode: snap(),
            want: Some(b"\x1b[21~"),
        },
        KCase {
            name: "f11",
            ev: key(Key::F(11), Mods::EMPTY),
            mode: snap(),
            want: Some(b"\x1b[23~"),
        },
        KCase {
            name: "f12",
            ev: key(Key::F(12), Mods::EMPTY),
            mode: snap(),
            want: Some(b"\x1b[24~"),
        },
        KCase {
            name: "shift+f5",
            ev: key(Key::F(5), Mods::SHIFT),
            mode: snap(),
            want: Some(b"\x1b[15;2~"),
        },
        KCase {
            name: "ctrl+f1",
            ev: key(Key::F(1), Mods::CTRL),
            mode: snap(),
            want: Some(b"\x1b[1;5P"),
        },
        KCase {
            name: "alt+f12",
            ev: key(Key::F(12), Mods::ALT),
            mode: snap(),
            want: Some(b"\x1b[24;3~"),
        },
        KCase {
            name: "shift+alt+f4",
            ev: key(Key::F(4), Mods::SHIFT | Mods::ALT),
            mode: snap(),
            want: Some(b"\x1b[1;4S"),
        },
        KCase {
            name: "f13 unsupported",
            ev: key(Key::F(13), Mods::EMPTY),
            mode: snap(),
            want: None,
        },
    ]);
}

// ---- vim-on-alt-screen scenario batch -----------------------------------------

#[test]
fn vim_alt_screen() {
    let vim_app = ModeSnapshot {
        alt_screen: true,
        app_cursor: true,
        ..snap()
    };
    let vim_normal = ModeSnapshot {
        alt_screen: true,
        ..snap()
    };
    check_keys(&[
        KCase {
            name: "vim ctrl+w",
            ev: char('w', Mods::CTRL),
            mode: vim_app,
            want: Some(b"\x17"),
        },
        KCase {
            name: "vim escape",
            ev: key(Key::Escape, Mods::EMPTY),
            mode: vim_app,
            want: Some(b"\x1b"),
        },
        KCase {
            name: "vim shift+f5",
            ev: key(Key::F(5), Mods::SHIFT),
            mode: vim_app,
            want: Some(b"\x1b[15;2~"),
        },
    ]);
    check_mouse(&[
        MCase {
            name: "vim wheel-up -> arrows app-cursor",
            ev: m(MouseKind::WheelUp, 0, 1, 1, Mods::EMPTY),
            mode: vim_app,
            want: Some(b"\x1bOA\x1bOA\x1bOA"),
        },
        MCase {
            name: "vim wheel-down -> arrows app-cursor",
            ev: m(MouseKind::WheelDown, 0, 1, 1, Mods::EMPTY),
            mode: vim_app,
            want: Some(b"\x1bOB\x1bOB\x1bOB"),
        },
        MCase {
            name: "wheel-up -> arrows normal cursor",
            ev: m(MouseKind::WheelUp, 0, 1, 1, Mods::EMPTY),
            mode: vim_normal,
            want: Some(b"\x1b[A\x1b[A\x1b[A"),
        },
    ]);
}

// ---- kitty disambiguate ----------------------------------------------

#[test]
fn kitty_disambiguate_encodings() {
    check_keys(&[
        // Esc always rides CSI-u when the flag is on, even unmodified.
        KCase {
            name: "kitty escape",
            ev: key(Key::Escape, Mods::EMPTY),
            mode: kitty(),
            want: Some(b"\x1b[27u"),
        },
        KCase {
            name: "kitty alt+escape",
            ev: key(Key::Escape, Mods::ALT),
            mode: kitty(),
            want: Some(b"\x1b[27;3u"),
        },
        // Modified Enter/Tab/Backspace ride CSI-u; unmodified keep xterm.
        KCase {
            name: "kitty shift+enter",
            ev: key(Key::Enter, Mods::SHIFT),
            mode: kitty(),
            // Kitty modifiers are the bitmask + 1 (spec) — `13;1u` (the
            // verbatim-bitmask bug) decodes as UNMODIFIED enter and Claude
            // Code submitted on Shift+Enter (host-run).
            want: Some(b"\x1b[13;2u"),
        },
        KCase {
            name: "kitty ctrl+enter",
            ev: key(Key::Enter, Mods::CTRL),
            mode: kitty(),
            want: Some(b"\x1b[13;5u"),
        },
        KCase {
            name: "kitty enter unmodified stays CR",
            ev: key(Key::Enter, Mods::EMPTY),
            mode: kitty(),
            want: Some(b"\r"),
        },
        KCase {
            name: "kitty shift+tab",
            ev: key(Key::Tab, Mods::SHIFT),
            mode: kitty(),
            want: Some(b"\x1b[9;2u"),
        },
        KCase {
            name: "kitty tab unmodified stays HT",
            ev: key(Key::Tab, Mods::EMPTY),
            mode: kitty(),
            want: Some(b"\t"),
        },
        KCase {
            name: "kitty ctrl+backspace",
            ev: key(Key::Backspace, Mods::CTRL),
            mode: kitty(),
            want: Some(b"\x1b[127;5u"),
        },
        KCase {
            name: "kitty backspace unmodified stays DEL",
            ev: key(Key::Backspace, Mods::EMPTY),
            mode: kitty(),
            want: Some(b"\x7f"),
        },
        // Modified printables use their codepoint + full mod bitmask.
        KCase {
            name: "kitty ctrl+a",
            ev: char('a', Mods::CTRL),
            mode: kitty(),
            want: Some(b"\x1b[97;5u"),
        },
        KCase {
            name: "kitty ctrl+shift+a keeps case + shift bit",
            ev: char('A', Mods::CTRL | Mods::SHIFT),
            mode: kitty(),
            want: Some(b"\x1b[65;6u"),
        },
        KCase {
            name: "kitty alt+a",
            ev: char('a', Mods::ALT),
            mode: kitty(),
            want: Some(b"\x1b[97;3u"),
        },
        KCase {
            name: "kitty shift+printable stays the shifted char",
            ev: char('A', Mods::SHIFT),
            mode: kitty(),
            want: Some(b"A"),
        },
        KCase {
            name: "kitty plain char stays plain",
            ev: char('a', Mods::EMPTY),
            mode: kitty(),
            want: Some(b"a"),
        },
        // Everything else keeps xterm encoding with the stack active.
        KCase {
            name: "kitty up app-cursor SS3",
            ev: key(Key::Up, Mods::EMPTY),
            mode: ModeSnapshot {
                app_cursor: true,
                ..kitty()
            },
            want: Some(b"\x1bOA"),
        },
        KCase {
            name: "kitty shift+up xterm modifier form",
            ev: key(Key::Up, Mods::SHIFT),
            mode: kitty(),
            want: Some(b"\x1b[1;2A"),
        },
        KCase {
            name: "kitty f5 unchanged",
            ev: key(Key::F(5), Mods::EMPTY),
            mode: kitty(),
            want: Some(b"\x1b[15~"),
        },
    ]);
}

// ---- actor integration: kitty negotiation journey (Claude Code fixture) ----------

#[test]
fn actor_kitty_negotiation_claude_code() {
    // Claude Code's real negotiation: query → reply advertising disambiguate
    // → push flag 1 → Shift+Enter emits CSI-u → pop → Shift+Enter back to the
    // xterm bytes. The kitty sequence bytes pass through to the parser (which
    // ignores them) and the reply lands on the writer sink.
    let (handle, rx, sink) = test_actor();

    // Query: expect a reply advertising exactly flag 0b1 — never the
    // key-release/alternate-key/etc. bits.
    handle.feed(b"\x1b[?u");
    wait_for_sink(&sink, b"\x1b[?1u");
    let frame = wait_frame(&rx);
    handle.ack(frame.seq);

    // Push flag 1 (disambiguate).
    handle.feed(b"\x1b[>1u");
    let frame = wait_frame(&rx);
    handle.ack(frame.seq);

    // Shift+Enter is now distinguishable: CSI-u with mods = bitmask+1.
    handle.write_key(key(Key::Enter, Mods::SHIFT));
    wait_for_sink(&sink, b"\x1b[13;2u");

    // Pop the stack: Shift+Enter reverts to the xterm bytes.
    handle.feed(b"\x1b[<u");
    let frame = wait_frame(&rx);
    handle.ack(frame.seq);
    handle.write_key(key(Key::Enter, Mods::SHIFT));
    wait_for_sink(&sink, b"\r");
}

#[test]
fn actor_kitty_push_is_masked_to_honoured_flag() {
    // Pushing extra flags must not advertise or honour them: `CSI > 17 u`
    // (disambiguate + associated text) only ever engages disambiguate.
    let (handle, rx, sink) = test_actor();

    handle.feed(b"\x1b[?u");
    wait_for_sink(&sink, b"\x1b[?1u");
    let frame = wait_frame(&rx);
    handle.ack(frame.seq);

    handle.feed(b"\x1b[>17u");
    let frame = wait_frame(&rx);
    handle.ack(frame.seq);

    // Query again: still only 0b1 advertised.
    handle.feed(b"\x1b[?u");
    wait_for_sink(&sink, b"\x1b[?1u");
    let frame = wait_frame(&rx);
    handle.ack(frame.seq);

    // Disambiguate IS active (the one honoured flag), so Enter+mods is CSI-u.
    handle.write_key(key(Key::Enter, Mods::CTRL));
    wait_for_sink(&sink, b"\x1b[13;5u");
}

#[test]
fn actor_kitty_reset_clears_stack() {
    // RIS (`ESC c`) clears the stack: after push + RIS, Shift+Enter is back to
    // xterm bytes and the query reply is the baseline.
    let (handle, rx, sink) = test_actor();

    handle.feed(b"\x1b[>1u");
    let frame = wait_frame(&rx);
    handle.ack(frame.seq);
    handle.write_key(key(Key::Enter, Mods::SHIFT));
    let bytes = wait_for_sink(&sink, b"\x1b[13;2u");
    assert_eq!(&bytes[..], b"\x1b[13;2u");

    handle.feed(b"\x1bc");
    let frame = wait_frame(&rx);
    handle.ack(frame.seq);
    handle.write_key(key(Key::Enter, Mods::SHIFT));
    let bytes = wait_for_sink(&sink, b"\r");
    // RIS writes nothing to the pty; the only new byte is the plain CR.
    assert_eq!(&bytes[..], b"\x1b[13;2u\r");
}

#[test]
fn actor_kitty_reply_beats_da1_answer() {
    // The kitty detection pattern is "CSI ? u, then CSI c (DA1); a DA1
    // answer arriving without the kitty reply first means unsupported".
    // Alacritty answers DA1 during the parser advance, so the kitty reply
    // must be written BEFORE the advance — Claude Code read the old
    // after-advance ordering as "no kitty support" and Shift+Enter submitted
    // (host-run bug).
    let (handle, _rx, sink) = test_actor();

    // The two replies are separate locked writes, so wait for the LATER one
    // (alacritty's DA1 answer is `CSI ? 6 c`) and then check the order.
    // Waiting for the kitty reply alone let the poller land in the gap
    // between the two writes, which is where the Windows CI runner failed.
    handle.feed(b"\x1b[?u\x1b[c");
    let bytes = wait_for_sink(&sink, b"\x1b[?6c");
    let kitty = find(&bytes, b"\x1b[?1u");
    let da1 = find(&bytes, b"\x1b[?6c");
    assert!(
        matches!((kitty, da1), (Some(k), Some(d)) if k < d),
        "kitty reply must be written before the DA1 answer, sink: {:?}",
        String::from_utf8_lossy(&bytes)
    );
}

/// Byte offset of the first occurrence of `needle` in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[test]
fn actor_kitty_set_form() {
    // `CSI = flags ; mode u` — direct set, the other way clients enable the
    // protocol. Mode 1 replaces, mode 3 clears; the supported-flags mask
    // still applies. Each phase asserts exactly the NEW pty bytes (the sink
    // accumulates, so contains-checks would race across phases).
    fn new_bytes_after(sink: &Arc<Mutex<Vec<u8>>>, from: usize) -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let bytes = sink.lock().unwrap().clone();
            if bytes.len() > from {
                return bytes[from..].to_vec();
            }
            assert!(Instant::now() < deadline, "no new pty bytes after {from}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    let (handle, rx, sink) = test_actor();

    // Mode 1 (replace): disambiguate engages.
    handle.feed(b"\x1b[=1u");
    let frame = wait_frame(&rx);
    handle.ack(frame.seq);
    let before = sink.lock().unwrap().len();
    handle.write_key(key(Key::Enter, Mods::SHIFT));
    assert_eq!(new_bytes_after(&sink, before), b"\x1b[13;2u");

    // Mode 3 (clear): back to the xterm bytes.
    handle.feed(b"\x1b[=1;3u");
    let frame = wait_frame(&rx);
    handle.ack(frame.seq);
    let before = sink.lock().unwrap().len();
    handle.write_key(key(Key::Enter, Mods::SHIFT));
    assert_eq!(new_bytes_after(&sink, before), b"\r");

    // Unsupported bits are masked: 16 (associated text) alone must not
    // engage disambiguate.
    handle.feed(b"\x1b[=16u");
    let frame = wait_frame(&rx);
    handle.ack(frame.seq);
    let before = sink.lock().unwrap().len();
    handle.write_key(key(Key::Enter, Mods::SHIFT));
    assert_eq!(new_bytes_after(&sink, before), b"\r");
}

// ---- paste ---------------------------------------------------------------------

#[test]
fn paste_wrapping_and_sanitizing() {
    let bracketed = ModeSnapshot {
        bracketed_paste: true,
        ..snap()
    };

    // (text, mode, expected)
    let cases: &[(&str, ModeSnapshot, &[u8])] = &[
        ("hello", bracketed, b"\x1b[200~hello\x1b[201~"),
        ("hello", snap(), b"hello"),
        ("a\nb", bracketed, b"\x1b[200~a\rb\x1b[201~"),
        ("a\r\nb", bracketed, b"\x1b[200~a\rb\x1b[201~"),
        ("a\nb", snap(), b"a\rb"),
        ("x\x1b[201~y", bracketed, b"\x1b[200~xy\x1b[201~"),
        ("x\x1b[201~y", snap(), b"xy"),
        ("x\x1b[200~y", bracketed, b"\x1b[200~xy\x1b[201~"),
        (
            "héllo\r\n\x1b[201~wörld",
            bracketed,
            b"\x1b[200~h\xc3\xa9llo\rw\xc3\xb6rld\x1b[201~",
        ),
    ];

    for (i, (text, mode, want)) in cases.iter().enumerate() {
        let got = encode_paste(text, mode);
        assert_eq!(&got[..], *want, "paste case #{i}: {text:?}");
    }
}

#[test]
fn paste_reassembly_bypass_closed() {
    // Single-pass str::replace is bypassable: removing the inner marker of
    // "\x1b[20" + "\x1b[201~" + "1~" rejoins the halves into a live ESC[201~.
    // The fixed-point strip must converge so no marker survives.
    let cases: &[(&str, &str)] = &[
        ("x\x1b[20\x1b[201~1~y", "xy"),
        ("x\x1b[200\x1b[200~~y", "xy"),
        (
            "\x1b[20\x1b[200\x1b[201~\x1b[200~1~\x1b[201~0~",
            "\x1b[20\x1b[2001~0~",
        ),
        ("a\r\n\x1b[20\x1b[201~1~b", "a\rb"),
    ];
    for (i, (input, want)) in cases.iter().enumerate() {
        let got = encode_paste(input, &snap());
        let got_str = std::str::from_utf8(&got).unwrap();
        assert!(
            !got_str.contains("\x1b[201~") && !got_str.contains("\x1b[200~"),
            "paste case #{i} left a live marker in {got_str:?}"
        );
        assert_eq!(got_str, *want, "paste case #{i}: {input:?}");
    }

    // Bracketed on: only the legitimate wrapper may carry the markers.
    let bracketed = ModeSnapshot {
        bracketed_paste: true,
        ..snap()
    };
    let got = encode_paste("x\x1b[20\x1b[201~1~y", &bracketed);
    let got_str = std::str::from_utf8(&got).unwrap();
    assert!(got_str.starts_with("\x1b[200~"));
    assert!(got_str.ends_with("\x1b[201~"));
    let inner = &got_str[6..got_str.len() - 6];
    assert!(
        !inner.contains("\x1b[201~") && !inner.contains("\x1b[200~"),
        "reassembled marker leaked into bracketed body: {inner:?}"
    );
    assert_eq!(inner, "xy");
}

// ---- mouse -----------------------------------------------------------------------

struct MCase {
    name: &'static str,
    ev: MouseEvent,
    mode: ModeSnapshot,
    want: Option<&'static [u8]>,
}

fn m(kind: MouseKind, button: u8, col: u16, row: u16, mods: Mods) -> MouseEvent {
    MouseEvent {
        kind,
        button,
        col,
        row,
        mods,
    }
}

fn sgr() -> ModeSnapshot {
    ModeSnapshot {
        mouse: MouseProto::Normal,
        mouse_sgr: true,
        ..snap()
    }
}

fn x10() -> ModeSnapshot {
    ModeSnapshot {
        mouse: MouseProto::Normal,
        ..snap()
    }
}

fn check_mouse(cases: &[MCase]) {
    for (i, c) in cases.iter().enumerate() {
        let got = encode_mouse(&c.ev, &c.mode);
        assert_eq!(
            got.as_deref(),
            c.want,
            "mouse case #{i} ({}) ev={:?} mode={:?}",
            c.name,
            c.ev,
            c.mode
        );
    }
}

#[test]
fn mouse_sgr() {
    check_mouse(&[
        MCase {
            name: "sgr press left",
            ev: m(MouseKind::Press, 0, 0, 0, Mods::EMPTY),
            mode: sgr(),
            want: Some(b"\x1b[<0;1;1M"),
        },
        MCase {
            name: "sgr press right ctrl",
            ev: m(MouseKind::Press, 2, 4, 3, Mods::CTRL),
            mode: sgr(),
            want: Some(b"\x1b[<18;5;4M"),
        },
        MCase {
            name: "sgr release",
            ev: m(MouseKind::Release, 0, 2, 2, Mods::EMPTY),
            mode: sgr(),
            want: Some(b"\x1b[<3;3;3m"),
        },
        MCase {
            name: "sgr move any-motion",
            ev: m(MouseKind::Move, 0, 1, 1, Mods::EMPTY),
            mode: ModeSnapshot {
                mouse: MouseProto::AnyMotion,
                ..sgr()
            },
            want: Some(b"\x1b[<35;2;2M"),
        },
        MCase {
            name: "sgr drag left",
            ev: m(MouseKind::Drag, 0, 5, 5, Mods::EMPTY),
            mode: ModeSnapshot {
                mouse: MouseProto::ButtonDrag,
                ..sgr()
            },
            want: Some(b"\x1b[<32;6;6M"),
        },
        MCase {
            name: "sgr drag right shift",
            ev: m(MouseKind::Drag, 2, 0, 0, Mods::SHIFT),
            mode: ModeSnapshot {
                mouse: MouseProto::ButtonDrag,
                ..sgr()
            },
            want: Some(b"\x1b[<38;1;1M"),
        },
        MCase {
            name: "sgr wheel up",
            ev: m(MouseKind::WheelUp, 0, 9, 9, Mods::EMPTY),
            mode: sgr(),
            want: Some(b"\x1b[<64;10;10M"),
        },
        MCase {
            name: "sgr wheel down alt",
            ev: m(MouseKind::WheelDown, 0, 0, 0, Mods::ALT),
            mode: sgr(),
            want: Some(b"\x1b[<73;1;1M"),
        },
        MCase {
            name: "sgr no coord clamp",
            ev: m(MouseKind::Press, 0, 300, 100, Mods::EMPTY),
            mode: sgr(),
            want: Some(b"\x1b[<0;301;101M"),
        },
        MCase {
            name: "sgr press super",
            ev: m(MouseKind::Press, 0, 0, 0, Mods::SUPER),
            mode: sgr(),
            want: Some(b"\x1b[<32;1;1M"),
        },
    ]);
}

#[test]
fn mouse_x10() {
    check_mouse(&[
        MCase {
            name: "x10 press left",
            ev: m(MouseKind::Press, 0, 0, 0, Mods::EMPTY),
            mode: x10(),
            want: Some(b"\x1b[M !!"),
        },
        MCase {
            name: "x10 press middle alt",
            ev: m(MouseKind::Press, 1, 0, 0, Mods::ALT),
            mode: x10(),
            want: Some(b"\x1b[M)!!"),
        },
        MCase {
            name: "x10 release",
            ev: m(MouseKind::Release, 0, 0, 0, Mods::EMPTY),
            mode: x10(),
            want: Some(b"\x1b[M#!!"),
        },
        MCase {
            name: "x10 drag",
            ev: m(MouseKind::Drag, 0, 0, 0, Mods::EMPTY),
            mode: ModeSnapshot {
                mouse: MouseProto::ButtonDrag,
                ..x10()
            },
            want: Some(b"\x1b[M !!"),
        },
        MCase {
            name: "x10 wheel up",
            ev: m(MouseKind::WheelUp, 0, 0, 0, Mods::EMPTY),
            mode: x10(),
            want: Some(b"\x1b[M`!!"),
        },
        MCase {
            name: "x10 wheel down shift",
            ev: m(MouseKind::WheelDown, 0, 0, 0, Mods::SHIFT),
            mode: x10(),
            want: Some(b"\x1b[Me!!"),
        },
        MCase {
            name: "x10 coord clamp at 223",
            ev: m(MouseKind::Press, 0, 300, 0, Mods::EMPTY),
            mode: x10(),
            want: Some(b"\x1b[M \xdf!"),
        },
    ]);
}

#[test]
fn mouse_protocol_gating() {
    check_mouse(&[
        MCase {
            name: "move proto none",
            ev: m(MouseKind::Move, 0, 1, 1, Mods::EMPTY),
            mode: snap(),
            want: None,
        },
        MCase {
            name: "move proto normal",
            ev: m(MouseKind::Move, 0, 1, 1, Mods::EMPTY),
            mode: sgr(),
            want: None,
        },
        MCase {
            name: "move proto drag",
            ev: m(MouseKind::Move, 0, 1, 1, Mods::EMPTY),
            mode: ModeSnapshot {
                mouse: MouseProto::ButtonDrag,
                ..sgr()
            },
            want: None,
        },
        MCase {
            name: "move proto any-motion",
            ev: m(MouseKind::Move, 0, 1, 1, Mods::EMPTY),
            mode: ModeSnapshot {
                mouse: MouseProto::AnyMotion,
                ..sgr()
            },
            want: Some(b"\x1b[<35;2;2M"),
        },
        MCase {
            name: "drag proto normal",
            ev: m(MouseKind::Drag, 0, 1, 1, Mods::EMPTY),
            mode: sgr(),
            want: None,
        },
        MCase {
            name: "drag proto drag",
            ev: m(MouseKind::Drag, 0, 1, 1, Mods::EMPTY),
            mode: ModeSnapshot {
                mouse: MouseProto::ButtonDrag,
                ..sgr()
            },
            want: Some(b"\x1b[<32;2;2M"),
        },
        MCase {
            name: "drag proto any-motion",
            ev: m(MouseKind::Drag, 0, 1, 1, Mods::EMPTY),
            mode: ModeSnapshot {
                mouse: MouseProto::AnyMotion,
                ..sgr()
            },
            want: Some(b"\x1b[<32;2;2M"),
        },
        MCase {
            name: "press proto none",
            ev: m(MouseKind::Press, 0, 1, 1, Mods::EMPTY),
            mode: snap(),
            want: None,
        },
        MCase {
            name: "release proto none",
            ev: m(MouseKind::Release, 0, 1, 1, Mods::EMPTY),
            mode: snap(),
            want: None,
        },
        MCase {
            name: "wheel proto none not alt",
            ev: m(MouseKind::WheelUp, 0, 1, 1, Mods::EMPTY),
            mode: snap(),
            want: None,
        },
        MCase {
            name: "wheel proto none alt app-cursor",
            ev: m(MouseKind::WheelUp, 0, 1, 1, Mods::EMPTY),
            mode: ModeSnapshot {
                alt_screen: true,
                app_cursor: true,
                ..snap()
            },
            want: Some(b"\x1bOA\x1bOA\x1bOA"),
        },
        MCase {
            name: "wheel down proto none alt",
            ev: m(MouseKind::WheelDown, 0, 1, 1, Mods::EMPTY),
            mode: ModeSnapshot {
                alt_screen: true,
                ..snap()
            },
            want: Some(b"\x1b[B\x1b[B\x1b[B"),
        },
        MCase {
            name: "wheel proto active",
            ev: m(MouseKind::WheelUp, 0, 9, 9, Mods::EMPTY),
            mode: ModeSnapshot {
                alt_screen: true,
                ..sgr()
            },
            want: Some(b"\x1b[<64;10;10M"),
        },
        MCase {
            name: "press proto normal x10",
            ev: m(MouseKind::Press, 0, 0, 0, Mods::EMPTY),
            mode: x10(),
            want: Some(b"\x1b[M !!"),
        },
    ]);
}

// ---- actor integration: modes snapshot at encode time -----------------------------

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
        scrollback_lines: 1_000,
        // Settings at their defaults (marks OFF, wheel 3x).
        ..ActorConfig::default()
    }
}

/// Start a no-pty actor and consume its initial Full frame.
fn test_actor() -> (
    term_core::actor::TermHandle,
    std::sync::mpsc::Receiver<TermEvent>,
    Arc<Mutex<Vec<u8>>>,
) {
    let (events, rx) = std::sync::mpsc::channel();
    let (handle, sink) = spawn_actor_nopty(cfg(20, 5), events);
    let first = wait_frame(&rx);
    handle.ack(first.seq);
    (handle, rx, sink)
}

fn wait_frame(rx: &std::sync::mpsc::Receiver<TermEvent>) -> term_core::actor::FrameData {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(TermEvent::Frame(frame)) => return frame,
            Ok(_) => continue,
            Err(err) => panic!("timed out waiting for a frame: {err}"),
        }
    }
}

/// Poll the sink until `needle` appears; returns the bytes accumulated so far.
fn wait_for_sink(sink: &Arc<Mutex<Vec<u8>>>, needle: &[u8]) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let bytes = sink.lock().unwrap().clone();
        if bytes.windows(needle.len()).any(|w| w == needle) {
            return bytes;
        }
        assert!(
            Instant::now() < deadline,
            "sink never contained {needle:?}; got {bytes:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn actor_write_key_honors_decckm() {
    let (handle, rx, sink) = test_actor();

    handle.feed(b"\x1b[?1hX");
    let frame = wait_frame(&rx);
    handle.ack(frame.seq);

    handle.write_key(key(Key::Up, Mods::EMPTY));
    wait_for_sink(&sink, b"\x1bOA");
}

#[test]
fn actor_write_key_normal_mode() {
    let (handle, rx, sink) = test_actor();

    handle.feed(b"X");
    let frame = wait_frame(&rx);
    handle.ack(frame.seq);

    handle.write_key(key(Key::Up, Mods::EMPTY));
    handle.write_key(char('q', Mods::EMPTY));
    handle.write_key(key(Key::Backspace, Mods::EMPTY));
    let bytes = wait_for_sink(&sink, b"\x1b[Aq\x7f");
    assert_eq!(&bytes[..], b"\x1b[Aq\x7f");
}

#[test]
fn actor_paste_honors_bracketed_paste() {
    let (handle, rx, sink) = test_actor();

    handle.feed(b"\x1b[?2004hX");
    let frame = wait_frame(&rx);
    handle.ack(frame.seq);

    handle.paste("a\nb");
    wait_for_sink(&sink, b"\x1b[200~a\rb\x1b[201~");
}

#[test]
fn actor_paste_without_bracketed() {
    let (handle, rx, sink) = test_actor();

    handle.feed(b"X");
    let frame = wait_frame(&rx);
    handle.ack(frame.seq);

    handle.paste("a\nb");
    wait_for_sink(&sink, b"a\rb");
}

#[test]
fn actor_mouse_honors_sgr_protocol() {
    let (handle, rx, sink) = test_actor();

    handle.feed(b"\x1b[?1000h\x1b[?1006hX");
    let frame = wait_frame(&rx);
    handle.ack(frame.seq);

    handle.mouse(m(MouseKind::Press, 0, 3, 2, Mods::EMPTY));
    wait_for_sink(&sink, b"\x1b[<0;4;3M");
}

#[test]
fn actor_mouse_no_protocol_suppressed() {
    let (handle, rx, sink) = test_actor();

    handle.feed(b"X");
    let frame = wait_frame(&rx);
    handle.ack(frame.seq);

    handle.mouse(m(MouseKind::Press, 0, 3, 2, Mods::EMPTY));
    std::thread::sleep(Duration::from_millis(50));
    let bytes = sink.lock().unwrap().clone();
    assert!(
        bytes.is_empty(),
        "no-protocol mouse must not reach the pty: {bytes:?}"
    );
}

// ---- wheel speed -------------------------------------------------

/// Scroll wheel speed `1x…6x` (default 3x): the multiplier applies ONLY when
/// a TUI app captures scroll input (mouse protocols active / alt screen
/// wheel→arrows); normal scrollback scrolls at system speed.
/// The multiplier is Rust-side, so it is
/// `ModeSnapshot::wheel_speed` that decides the arrow repeat count.
#[test]
fn wheel_speed_sets_the_alt_screen_arrow_repeat_count() {
    for (speed, want) in [
        (1u8, &b"\x1b[B"[..]),
        (3, &b"\x1b[B\x1b[B\x1b[B"[..]),
        (6, &b"\x1b[B\x1b[B\x1b[B\x1b[B\x1b[B\x1b[B"[..]),
    ] {
        let mode = ModeSnapshot {
            alt_screen: true,
            wheel_speed: speed,
            ..snap()
        };
        let got = encode_mouse(&m(MouseKind::WheelDown, 0, 1, 1, Mods::EMPTY), &mode);
        assert_eq!(got.as_deref(), Some(want), "wheel_speed {speed}");
    }
}

/// The TUI-only rule stands: off the alt screen with no mouse protocol the
/// wheel still produces NOTHING regardless of speed (the frontend scrolls
/// scrollback itself, at system speed).
#[test]
fn wheel_speed_never_leaks_into_normal_scrollback() {
    for speed in 1u8..=6 {
        let mode = ModeSnapshot {
            wheel_speed: speed,
            ..snap()
        };
        assert_eq!(
            encode_mouse(&m(MouseKind::WheelUp, 0, 1, 1, Mods::EMPTY), &mode),
            None,
            "wheel_speed {speed} must not encode anything off the alt screen"
        );
    }
}

/// A protocol-active TUI gets ONE report per notch: the repeat is the
/// wheel→arrows fallback only, and doubling reports would double-scroll.
#[test]
fn wheel_speed_does_not_repeat_protocol_reports() {
    let mode = ModeSnapshot {
        alt_screen: true,
        wheel_speed: 6,
        ..sgr()
    };
    let got = encode_mouse(&m(MouseKind::WheelUp, 0, 9, 9, Mods::EMPTY), &mode);
    assert_eq!(got.as_deref(), Some(&b"\x1b[<64;10;10M"[..]));
}
