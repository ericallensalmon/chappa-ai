// The webview side of the pre-launch secret scan: reading a refused spawn's
// findings out of its error, the dialog that names them (file and rule,
// never a value) and offers to acknowledge them into the project's local
// baseline, the "Launch anyway" confirm for a scan that did not finish (a
// required note, good for that one launch, never written to the baseline),
// and the notification text for a launch that went ahead with a warning or
// past an unfinished scan.

import { confirmDialog, promptDialog } from "./dialog";
import * as ipc from "./ipc";

/** Rust's refusal prefix; the JSON after it is a `SecretScanBlockDto`. */
export const BLOCK_PREFIX = "secret-scan-blocked:";

/** The block behind a spawn error, or null when the error is something else. */
export function parseSecretScanBlock(err: unknown): ipc.SecretScanBlockDto | null {
  const text = String(err instanceof Error ? err.message : err);
  const at = text.indexOf(BLOCK_PREFIX);
  if (at < 0) return null;
  try {
    const parsed = JSON.parse(text.slice(at + BLOCK_PREFIX.length)) as Partial<ipc.SecretScanBlockDto>;
    if (!parsed || !Array.isArray(parsed.findings)) return null;
    return {
      projectRoot: typeof parsed.projectRoot === "string" ? parsed.projectRoot : "",
      cwd: typeof parsed.cwd === "string" ? parsed.cwd : "",
      findings: parsed.findings.filter((f) => f && typeof f.file === "string" && typeof f.rule === "string"),
      skipped: typeof parsed.skipped === "number" ? parsed.skipped : 0,
      ...(typeof parsed.incomplete === "string" && parsed.incomplete ? { incomplete: parsed.incomplete } : {}),
    };
  } catch {
    return null;
  }
}

/** One finding as a line: `file:line (rule)`. */
export function findingLine(f: ipc.SecretFindingDto): string {
  return `${f.file}${f.line ? `:${f.line}` : ""}  (${f.rule})`;
}

/** The dialog text for a refused launch. */
export function blockMessage(block: ipc.SecretScanBlockDto): string {
  if (block.incomplete) {
    return [
      `The agent was not launched: the secret scan of ${block.cwd} did not finish.`,
      "",
      block.incomplete,
      "",
      "The agent would read files nobody checked. Fix the scanner (Settings ▸ Security), or launch anyway: say why below. That lets this one launch through and is recorded in the agent's journal; it acknowledges nothing.",
    ].join("\n");
  }
  const n = block.findings.length;
  const lines = [
    `The agent was not launched: ${n} possible secret${n === 1 ? "" : "s"} in ${block.cwd}.`,
    "",
    ...block.findings.map(findingLine),
    "",
    "Fix them, or acknowledge them as known (they go into the project's local baseline under .work/ and stop blocking launches; the baseline is never committed).",
  ];
  if (block.skipped > 0) lines.push("", `${block.skipped} file${block.skipped === 1 ? " was" : "s were"} not scanned (too large, or a link).`);
  return lines.join("\n");
}

/** The notification for a launch that went ahead. */
export function noticeText(notice: ipc.SecretScanNoticeEvent["notice"]): { title: string; body: string } {
  if (notice.kind === "launched-anyway") {
    return { title: "Launched without a complete secret scan", body: `Launched in ${notice.cwd} anyway: ${notice.reason}. Note: ${notice.note}` };
  }
  const n = notice.findings.length;
  return {
    title: `${n} possible secret${n === 1 ? "" : "s"} in the agent's folder`,
    body: `Launched in ${notice.cwd} anyway (policy: warn). ${notice.findings.map(findingLine).join("; ")}`,
  };
}

/**
 * The "Launch anyway" confirm for a scan that did not finish: the reason and
 * a required note. Resolves the note, or null when cancelled.
 */
export async function offerLaunchAnyway(
  block: ipc.SecretScanBlockDto,
  opts: { container?: HTMLElement } = {},
): Promise<string | null> {
  const message = `${blockMessage(block)}\n\nA note is required.`;
  for (;;) {
    const note = await promptDialog(message, { okLabel: "Launch anyway", placeholder: "why it is safe to launch now", ...opts });
    if (note === null) return null;
    if (note.trim() !== "") return note.trim();
  }
}

/**
 * Show a refused launch's findings with Acknowledge / Cancel. On
 * Acknowledge the findings are written to the project's baseline and the
 * caller may launch again. Resolves true when acknowledged.
 */
export async function offerAcknowledge(
  block: ipc.SecretScanBlockDto,
  projectId: number,
  api: { acknowledge: typeof ipc.acknowledgeSecretFindings } = { acknowledge: ipc.acknowledgeSecretFindings },
  opts: { container?: HTMLElement } = {},
): Promise<boolean> {
  const ok = await confirmDialog(blockMessage(block), { okLabel: "Acknowledge", cancelLabel: "Cancel", safeDefault: true, ...opts });
  if (!ok) return false;
  await api.acknowledge(projectId, block.findings, null);
  return true;
}
