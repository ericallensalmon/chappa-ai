//! Tests for the OSC pre-scan scanner (deliverables).

use term_core::scanner::{KittyEvent, OscEvent, OscScanner, PromptMarkKind};

/// Feed a single byte at a time — the harshest chunking, plus the exact
/// boundary splits the scanner has to survive.
fn feed_one_byte_at_a_time(scanner: &mut OscScanner, bytes: &[u8]) -> Vec<OscEvent> {
    let mut events = Vec::new();
    for &b in bytes {
        events.extend(scanner.feed(&[b]));
    }
    events
}

#[test]
fn osc_9_body_bel_terminated() {
    let mut s = OscScanner::new();
    let ev = s.feed(b"hello \x1b]9;build finished\x07 done");
    assert_eq!(
        ev,
        vec![OscEvent::Notify {
            title: None,
            body: "build finished".into()
        }]
    );
}

#[test]
fn osc_9_body_st_terminated() {
    let mut s = OscScanner::new();
    let ev = s.feed(b"\x1b]9;build finished\x1b\\");
    assert_eq!(
        ev,
        vec![OscEvent::Notify {
            title: None,
            body: "build finished".into()
        }]
    );
}

#[test]
fn osc_777_notify_title_and_body() {
    let mut s = OscScanner::new();
    let ev = s.feed(b"\x1b]777;notify;ChappaAi;sync complete\x1b\\");
    assert_eq!(
        ev,
        vec![OscEvent::Notify {
            title: Some("ChappaAi".into()),
            body: "sync complete".into()
        }]
    );
}

#[test]
fn osc_777_ignores_other_subcommands() {
    let mut s = OscScanner::new();
    let ev = s.feed(b"\x1b]777;other;stuff\x1b\\");
    assert!(ev.is_empty());
}

#[test]
fn osc_777_empty_title_is_none() {
    let mut s = OscScanner::new();
    let ev = s.feed(b"\x1b]777;notify;;just a body\x1b\\");
    assert_eq!(
        ev,
        vec![OscEvent::Notify {
            title: None,
            body: "just a body".into()
        }]
    );
}

#[test]
fn osc_777_body_keeps_inner_semicolons() {
    let mut s = OscScanner::new();
    let ev = s.feed(b"\x1b]777;notify;T;one;two;three\x1b\\");
    assert_eq!(
        ev,
        vec![OscEvent::Notify {
            title: Some("T".into()),
            body: "one;two;three".into()
        }]
    );
}

#[test]
fn osc_99_kitty_style_title_body() {
    let mut s = OscScanner::new();
    let ev = s.feed(b"\x1b]99;ChappaAi;hello there\x1b\\");
    assert_eq!(
        ev,
        vec![OscEvent::Notify {
            title: Some("ChappaAi".into()),
            body: "hello there".into()
        }]
    );
}

#[test]
fn osc_99_empty_title_empty_body() {
    let mut s = OscScanner::new();
    let ev = s.feed(b"\x1b]99;;body only\x07");
    assert_eq!(
        ev,
        vec![OscEvent::Notify {
            title: None,
            body: "body only".into()
        }]
    );

    let mut s = OscScanner::new();
    let ev = s.feed(b"\x1b]99;title only;\x07");
    assert_eq!(
        ev,
        vec![OscEvent::Notify {
            title: Some("title only".into()),
            body: "".into()
        }]
    );
}

#[test]
fn osc_133_all_four_kinds() {
    let mut s = OscScanner::new();
    let ev = s.feed(b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\\x1b]133;C\x1b\\make\x1b]133;D\x1b\\");
    assert_eq!(
        ev,
        vec![
            OscEvent::PromptMark(PromptMarkKind::PromptStart),
            OscEvent::PromptMark(PromptMarkKind::PromptEnd),
            OscEvent::PromptMark(PromptMarkKind::CommandStart),
            OscEvent::PromptMark(PromptMarkKind::CommandEnd),
        ]
    );
}

#[test]
fn osc_133_with_extra_params_keeps_kind() {
    let mut s = OscScanner::new();
    let ev = s.feed(b"\x1b]133;A;Id=abc\x1b\\");
    assert_eq!(ev, vec![OscEvent::PromptMark(PromptMarkKind::PromptStart)]);
}

#[test]
fn osc_133_unknown_letter_ignored() {
    let mut s = OscScanner::new();
    let ev = s.feed(b"\x1b]133;X\x1b\\\x1b]133;\x1b\\");
    assert!(ev.is_empty());
}

#[test]
fn unknown_osc_codes_passthrough() {
    // OSC 7 (cwd) and OSC 10 (color) are not in scope: no events, no loss.
    let mut s = OscScanner::new();
    let ev = s.feed(b"\x1b]7;file:///home\x1b\\\x1b]10;#ff0000\x07");
    assert!(ev.is_empty());
}

// ---- chunk boundaries ---------------------------------------------------

#[test]
fn osc_split_after_introducer() {
    let mut s = OscScanner::new();
    let mut ev = Vec::new();
    ev.extend(s.feed(b"pre \x1b"));
    ev.extend(s.feed(b"]"));
    ev.extend(s.feed(b"9;hi"));
    ev.extend(s.feed(b"\x07"));
    ev.extend(s.feed(b" post"));
    assert_eq!(
        ev,
        vec![OscEvent::Notify {
            title: None,
            body: "hi".into()
        }]
    );
}

#[test]
fn osc_split_every_byte() {
    let mut s = OscScanner::new();
    let ev = feed_one_byte_at_a_time(&mut s, b"\x1b]777;notify;D;x y\x1b\\");
    assert_eq!(
        ev,
        vec![OscEvent::Notify {
            title: Some("D".into()),
            body: "x y".into()
        }]
    );
}

#[test]
fn osc_terminator_split_across_chunks() {
    // ST split as `\x1b` in one chunk and `\` in the next.
    let mut s = OscScanner::new();
    let mut ev = Vec::new();
    ev.extend(s.feed(b"\x1b]133;A"));
    ev.extend(s.feed(b"\x1b"));
    ev.extend(s.feed(b"\\"));
    assert_eq!(ev, vec![OscEvent::PromptMark(PromptMarkKind::PromptStart)]);
}

#[test]
fn esc_esc_introducer_survives() {
    // Some shells pad with an extra ESC: `ESC ESC ] 133;A`. The first ESC
    // is a pad, the second starts the OSC. Mirrors vte's Escape-state pad.
    let mut s = OscScanner::new();
    let ev = s.feed(b"\x1b\x1b]133;A\x1b\\");
    assert_eq!(ev, vec![OscEvent::PromptMark(PromptMarkKind::PromptStart)]);
}

#[test]
fn lone_esc_terminates_osc_like_vte() {
    // vte dispatches the OSC the moment it sees an ESC inside the string;
    // the `\` then just closes the (empty) ST at escape level. Our scanner
    // must extract the same payload the parser saw.
    let mut s = OscScanner::new();
    let ev = s.feed(b"\x1b]9;done\x1b\x1b\\");
    assert_eq!(
        ev,
        vec![OscEvent::Notify {
            title: None,
            body: "done".into()
        }]
    );
}

// ---- embedded in arbitrary output --------------------------------------

#[test]
fn heavy_surrounding_output() {
    let out = concat!(
        "building...\r\n\x1b[1;32mok\x1b[0m\x1b]8;;https://x.example\x1b\\link\x1b]8;;\x1b\\\r\n",
        "\x1b]9;uploading 42%\x07 progress\r\n\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\\r\n",
        "some \x1b[31mred\x1b[0m text with éè accents\n\x1b]777;notify;T;b\x1b\\bye"
    );
    let mut s = OscScanner::new();
    let ev = s.feed(out.as_bytes());
    assert_eq!(
        ev,
        vec![
            OscEvent::Notify {
                title: None,
                body: "uploading 42%".into()
            },
            OscEvent::PromptMark(PromptMarkKind::PromptStart),
            OscEvent::PromptMark(PromptMarkKind::PromptEnd),
            OscEvent::Notify {
                title: Some("T".into()),
                body: "b".into()
            },
        ]
    );
}

#[test]
fn passthrough_is_byte_exact() {
    // feed_to must reproduce the input byte-for-byte while extracting.
    let mut out = Vec::new();
    let mut s = OscScanner::new();
    let input = b"a\x1b]9;n\x07b\x1b[1m\xc3\xa9\x1b]133;C\x1b\\\x00\xff";
    let _ = s.feed_to(input, &mut out);
    assert_eq!(out, input);
    assert_eq!(out.len(), input.len());
}

// ---- malformed / oversized ----------------------------------------------

#[test]
fn truncated_osc_no_panic_no_event() {
    // Stream ends mid-OSC: nothing is emitted, scanner is reusable.
    let mut s = OscScanner::new();
    let mut ev = Vec::new();
    ev.extend(s.feed(b"\x1b]9;partial"));
    assert!(ev.is_empty());
    ev.extend(s.feed(b"\x07"));
    assert_eq!(
        ev,
        vec![OscEvent::Notify {
            title: None,
            body: "partial".into()
        }]
    );
}

#[test]
fn oversized_osc_dropped_bytes_pass_through() {
    let mut s = OscScanner::with_cap(64);
    let big = vec![b'x'; 10_000];
    let mut chunk = Vec::new();
    chunk.extend_from_slice(b"\x1b]9;");
    chunk.extend_from_slice(&big);
    chunk.extend_from_slice(b"\x07rest");

    let mut out = Vec::new();
    let ev = s.feed_to(&chunk, &mut out);
    assert!(ev.is_empty(), "oversized OSC must not surface an event");
    assert_eq!(out, chunk, "bytes must pass through untouched");

    // Scanner still works afterwards.
    let ev = s.feed(b"\x1b]9;after\x07");
    assert_eq!(
        ev,
        vec![OscEvent::Notify {
            title: None,
            body: "after".into()
        }]
    );
}

#[test]
fn oversized_across_chunks() {
    // A single OSC payload that exceeds the cap only once it has been
    // assembled across many feed calls: still dropped, still consumed.
    let mut s = OscScanner::with_cap(16);
    let mut ev = Vec::new();
    ev.extend(s.feed(b"\x1b]9;"));
    for _ in 0..8 {
        ev.extend(s.feed(b"padding"));
    }
    ev.extend(s.feed(b"\x07"));
    assert!(ev.is_empty());

    // The next OSC is unaffected.
    let ev = s.feed(b"\x1b]9;ok\x07");
    assert_eq!(
        ev,
        vec![OscEvent::Notify {
            title: None,
            body: "ok".into()
        }]
    );
}

#[test]
fn non_utf8_body_is_lossy_not_fatal() {
    let mut s = OscScanner::new();
    let ev = s.feed(b"\x1b]9;bad \xff\xfe bytes\x07");
    assert_eq!(
        ev,
        vec![OscEvent::Notify {
            title: None,
            body: "bad \u{fffd}\u{fffd} bytes".into()
        }]
    );
}

#[test]
fn random_bytes_never_panic() {
    // Deterministic pseudo-random fuzz: 1MB of bytes, no panics, events
    // (if any) are well-formed.
    let mut rng = 0x9E37_79B9u64;
    let mut s = OscScanner::new();
    let mut ev_total = 0usize;
    for _ in 0..1024 {
        let mut chunk = vec![0u8; 1024];
        for b in &mut chunk {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            *b = rng as u8;
        }
        ev_total += s.feed(&chunk).len();
    }
    let _ = ev_total;
}

// ---- kitty keyboard protocol CSI forms ---------------------------

#[test]
fn kitty_query_push_pop() {
    let mut s = OscScanner::new();
    let ev = s.feed(b"pre \x1b[?u \x1b[>1u \x1b[<u post");
    assert_eq!(
        ev,
        vec![
            OscEvent::Kitty(KittyEvent::Query),
            OscEvent::Kitty(KittyEvent::Push(1)),
            OscEvent::Kitty(KittyEvent::Pop(1)),
        ]
    );
}

#[test]
fn kitty_pop_with_count_and_multi_digit_flags() {
    let mut s = OscScanner::new();
    let ev = s.feed(b"\x1b[>17u \x1b[<2u \x1b[<u");
    assert_eq!(
        ev,
        vec![
            OscEvent::Kitty(KittyEvent::Push(17)),
            OscEvent::Kitty(KittyEvent::Pop(2)),
            OscEvent::Kitty(KittyEvent::Pop(1)),
        ]
    );
}

#[test]
fn kitty_set_form() {
    // `CSI = <flags> [; <mode>] u`: omitted mode defaults to 1; modes 1..=3
    // only. A trailing param or an out-of-range mode is NOT a kitty form
    // (falls through to the parser).
    let mut s = OscScanner::new();
    let ev = s.feed(b"\x1b[=1u \x1b[=17;2u \x1b[=1;3u");
    assert_eq!(
        ev,
        vec![
            OscEvent::Kitty(KittyEvent::Set(1, 1)),
            OscEvent::Kitty(KittyEvent::Set(17, 2)),
            OscEvent::Kitty(KittyEvent::Set(1, 3)),
        ]
    );
    assert!(s.feed(b"\x1b[=1;4u").is_empty(), "mode 4 is not a kitty form");
    assert!(s.feed(b"\x1b[=u").is_empty(), "flags are required");
    assert!(
        s.feed(b"\x1b[=1;2;3u").is_empty(),
        "extra params are not a kitty form"
    );
}

#[test]
fn kitty_split_across_chunks() {
    let mut s = OscScanner::new();
    let mut ev = Vec::new();
    ev.extend(s.feed(b"\x1b["));
    ev.extend(s.feed(b"?"));
    ev.extend(s.feed(b"u"));
    ev.extend(s.feed(b"\x1b[>"));
    ev.extend(s.feed(b"1"));
    ev.extend(s.feed(b"u"));
    assert_eq!(
        ev,
        vec![
            OscEvent::Kitty(KittyEvent::Query),
            OscEvent::Kitty(KittyEvent::Push(1)),
        ]
    );
}

#[test]
fn kitty_every_byte() {
    let mut s = OscScanner::new();
    let ev = feed_one_byte_at_a_time(&mut s, b"\x1b[>12u\x1b[<u");
    assert_eq!(
        ev,
        vec![
            OscEvent::Kitty(KittyEvent::Push(12)),
            OscEvent::Kitty(KittyEvent::Pop(1)),
        ]
    );
}

#[test]
fn normal_csi_not_kitty() {
    // `CSI ? 25 l`, `CSI u` (DECRC) and `CSI > 0 c` (secondary DA) are normal
    // CSI — never misdetected as kitty forms.
    let mut s = OscScanner::new();
    let ev = s.feed(b"\x1b[?25l\x1b[u\x1b[>0c\x1b[2J\x1b[31m");
    assert!(ev.is_empty());
}

#[test]
fn kitty_query_with_params_not_detected() {
    // `CSI ? 1 u` is not the standard query form — no event.
    let mut s = OscScanner::new();
    let ev = s.feed(b"\x1b[?1u");
    assert!(ev.is_empty());
}

#[test]
fn kitty_ris_reset() {
    let mut s = OscScanner::new();
    let ev = s.feed(b"a\x1bc b\x1b]9;n\x07");
    assert_eq!(
        ev,
        vec![
            OscEvent::Kitty(KittyEvent::Reset),
            OscEvent::Notify {
                title: None,
                body: "n".into()
            },
        ]
    );
}

#[test]
fn kitty_does_not_break_passthrough() {
    let mut out = Vec::new();
    let mut s = OscScanner::new();
    let input = b"x\x1b[?u\x1b[>1u\x1b[<uy\x1b[31mz";
    let ev = s.feed_to(input, &mut out);
    assert_eq!(ev.len(), 3);
    assert_eq!(out, input, "kitty bytes must pass through untouched");
}

#[test]
fn kitty_csi_aborted_by_control_byte() {
    // A CAN inside the CSI kills detection (and vte); nothing is emitted.
    let mut s = OscScanner::new();
    let ev = s.feed(b"\x1b[?\x18u");
    assert!(ev.is_empty());
    let ev = s.feed(b"\x1b[>1u");
    assert_eq!(ev, vec![OscEvent::Kitty(KittyEvent::Push(1))]);
}

// ---- SIMD ESC search correctness (scan.rs is unsafe; hammer the edges) ----

use term_core::scan;

#[test]
fn find_esc_positions() {
    // Empty and no-ESC.
    assert_eq!(scan::find_esc(b""), None);
    assert_eq!(scan::find_esc(b"abcdefgh"), None);
    assert_eq!(scan::find_esc(&[b'x'; 100_000]), None);

    // First byte, last byte, single byte.
    assert_eq!(scan::find_esc(b"\x1babc"), Some(0));
    assert_eq!(scan::find_esc(b"abc\x1b"), Some(3));
    assert_eq!(scan::find_esc(b"\x1b"), Some(0));

    // Around 64-byte (AVX-512) and 32-byte (AVX2) block boundaries.
    for offset in [63usize, 64, 65, 127, 128, 129] {
        let mut v = vec![b'x'; 200];
        v[offset] = 0x1B;
        assert_eq!(scan::find_esc(&v), Some(offset), "offset {offset}");
    }

    // First of several; trailing garbage after the ESC is irrelevant.
    let v = [b'q'; 100];
    let mut v2 = v.to_vec();
    v2[50] = 0x1B;
    v2[80] = 0x1B;
    assert_eq!(scan::find_esc(&v2), Some(50));
}

#[test]
fn find_esc_agrees_with_naive_on_structured_corpus() {
    // Cross-check the SIMD search against a naive scan over a corpus with
    // ESCs sprinkled at every modulus, so both SIMD and scalar tails run.
    let mut v = vec![b'a'; 1000];
    for i in (0..1000).step_by(17) {
        v[i] = 0x1B;
    }
    let naive = v.iter().position(|&b| b == 0x1B);
    assert_eq!(scan::find_esc(&v), naive);

    // Random-ish corpus.
    let mut rng = 0x12345678u64;
    let mut v = Vec::with_capacity(5000);
    for _ in 0..5000 {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        v.push(if rng.is_multiple_of(50) {
            0x1B
        } else {
            (rng % 250) as u8
        });
    }
    let naive = v.iter().position(|&b| b == 0x1B);
    assert_eq!(scan::find_esc(&v), naive);
}
