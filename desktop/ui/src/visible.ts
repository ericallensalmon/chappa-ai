// Text made visible for approval prompts.
//
// What a person approves has to read exactly like what will happen. A line
// break inside a value pushes the rest out of view, a carriage return or an
// escape sequence can redraw what came before, and the bidi and zero-width
// formatting characters make text read differently from its bytes. Every
// approval prompt (project trust, clipboard writes) shows those characters
// as glyphs instead of rendering them, with one mapping, the same one the
// Rust side uses for the trust prompt's text.

/** One code point made visible: C0 controls (line breaks and tabs
 *  included) and DEL as their Control Pictures glyph, C1 controls, the
 *  Unicode line separators and the bidi / zero-width formatting characters
 *  as `\u{…}`. Anything else comes back as itself. */
export function visibleChar(cp: number): string {
  if (cp < 0x20) return String.fromCodePoint(0x2400 + cp);
  if (cp === 0x7f) return "␡";
  if (
    (cp >= 0x80 && cp <= 0x9f) ||
    (cp >= 0x200b && cp <= 0x200f) ||
    (cp >= 0x2028 && cp <= 0x202e) ||
    (cp >= 0x2066 && cp <= 0x2069) ||
    cp === 0xfeff
  ) {
    return `\\u{${cp.toString(16)}}`;
  }
  return String.fromCodePoint(cp);
}

/** The glyph a space is shown as where whitespace has to be visible. */
export const VISIBLE_SPACE = "␣";

/** `text` with every character {@link visibleChar} names made visible;
 *  with `spaces`, a space too (as {@link VISIBLE_SPACE}). Idempotent: the
 *  glyphs it produces are themselves printable. */
export function visibleText(text: string, opts: { spaces?: boolean } = {}): string {
  let out = "";
  for (const ch of text) {
    if (opts.spaces && ch === " ") out += VISIBLE_SPACE;
    else out += visibleChar(ch.codePointAt(0)!);
  }
  return out;
}
