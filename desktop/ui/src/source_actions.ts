// The user-facing half of source actions: what a button shows and whether
// it is enabled, and the two dialogs that stand between a click and a run —
// the confirm text of a mutating endpoint action (with its optional or
// required note, and the inputs it asks for) and the full prompt of an agent
// action, shown before the first run (or every run, per the source's prompt
// policy).
//
// Rust judges every action (`source_actions`) and judges it again when it
// runs (`source_run_action`); this module only turns those judgements into
// labels, tooltips and questions. No view renders the buttons yet; the
// views that do read `actionButtonState`, then `confirmEndpoint` /
// `confirmAgentPrompt`, then `runAction`.

import { confirmDialog, fieldsDialog, promptDialog } from "./dialog";
import * as ipc from "./ipc";

export type SourceAction = ipc.SourceActionShape;
export type ActionVerdict = ipc.ActionVerdictDto;
export type ActionOutcome = ipc.ActionOutcomeDto;

/** What a view draws for one action. */
export interface ActionButtonState {
  label: string;
  disabled: boolean;
  /** The tooltip: the disabled reason, or what the button does. */
  title: string;
  /** Never drawn at all (a mutating endpoint without a confirm text). */
  hidden: boolean;
  /** `agent` whose prompt will be shown before this run. */
  asksFirst: boolean;
}

const KIND_TITLES: Record<SourceAction["kind"], string> = {
  agent: "Launch an agent here",
  terminal: "Open a terminal here",
  endpoint: "Ask the source to do this",
  link: "Open in the browser",
  panel: "Open a page from the source",
};

/** The button for `entry`: label from the action, disabled + reason from
 *  the verdict, a hint of what happens on click. Pure. */
export function actionButtonState(entry: ipc.SourceActionDto): ActionButtonState {
  const { action, verdict } = entry;
  const label = action.label.trim() || action.id;
  if (verdict.hidden) {
    return { label, disabled: true, title: verdict.reason ?? "not offered", hidden: true, asksFirst: false };
  }
  if (!verdict.enabled) {
    return { label, disabled: true, title: verdict.reason ?? "not available", hidden: false, asksFirst: false };
  }
  // Rust's verdict already carries a source's own `disabled`; read here
  // too so the button can never be drawn live while the source refuses it.
  if (action.kind === "endpoint" && action.disabled) {
    return { label, disabled: true, title: action.disabled, hidden: false, asksFirst: false };
  }
  let title = KIND_TITLES[action.kind];
  if (action.kind === "agent" && verdict.tool) title = `Launch ${verdict.tool[1]} in ${verdict.cwd ?? action.cwd}`;
  if (action.kind === "terminal") title = `Open a terminal in ${verdict.cwd ?? action.cwd}`;
  if (action.kind === "link") title = `Open ${action.url}`;
  if (action.kind === "endpoint" && action.mutates) title = "Changes something; asks first";
  return { label, disabled: false, title, hidden: false, asksFirst: entry.needsPrompt };
}

/** The confirm question of a mutating endpoint action plus, when the
 *  action takes a note, where the note is asked. Pure. */
export function endpointQuestion(action: Extract<SourceAction, { kind: "endpoint" }>): {
  confirm: string | null;
  note: "optional" | "required" | null;
} {
  return { confirm: action.mutates ? action.confirm : null, note: action.note };
}

/** What the agent-prompt dialog shows: the whole prompt, the tool, the
 *  folder and every env entry, so nothing a source wrote reaches an agent
 *  unseen. Pure. */
export function agentPromptMessage(
  action: Extract<SourceAction, { kind: "agent" }>,
  verdict: ActionVerdict,
  sourceLabel: string,
): string {
  const tool = verdict.tool ? verdict.tool[1] : (action.tool ?? "the default agent tool");
  const cwd = verdict.cwd ?? action.cwd;
  const env = Object.entries(action.env);
  const lines = [
    `${sourceLabel} wants to launch ${tool}`,
    `in ${cwd}`,
    ...(env.length ? ["with env:", ...env.map(([k, v]) => `  ${k}=${v}`)] : []),
    "",
    "and hand it this prompt:",
    "",
    action.prompt,
  ];
  return lines.join("\n");
}

export interface ConfirmedEndpoint {
  confirmed: boolean;
  note: string | null;
  /** The inputs' values, when the action asks for any. */
  values?: Record<string, unknown>;
}

/** Free text is capped here when an input declares no `max` (as in Rust). */
export const TEXT_INPUT_MAX = 200;

const isEmpty = (v: unknown): boolean => v === null || v === undefined || (typeof v === "string" && v.trim() === "") || (Array.isArray(v) && v.length === 0);

/** Whether `values` are what `inputs` take: the first problem, or null.
 *  The same rules Rust applies before the call (which refuses whatever
 *  this lets through); the dialog uses it to keep asking. Pure. */
export function checkInputValues(inputs: readonly ipc.EndpointInputDto[], values: Record<string, unknown>): string | null {
  for (const input of inputs) {
    const v = values[input.name];
    if (isEmpty(v)) {
      if (input.required) return `${input.label} is required.`;
      continue;
    }
    const known = (x: unknown): boolean => typeof x === "string" && (input.options ?? []).some((o) => o.value === x);
    switch (input.kind) {
      case "choice":
        if (!known(v)) return `${input.label}: choose one of the options.`;
        break;
      case "multi":
        if (!Array.isArray(v) || !v.every(known)) return `${input.label}: choose among the options.`;
        if (new Set(v).size !== v.length) return `${input.label}: each option once.`;
        break;
      case "text": {
        const cap = input.max ?? TEXT_INPUT_MAX;
        if (typeof v !== "string") return `${input.label} must be text.`;
        if ([...v].length > cap) return `${input.label} is longer than ${cap} characters.`;
        // eslint-disable-next-line no-control-regex
        if (/[\u0000-\u001f\u007f-\u009f]/.test(v)) return `${input.label} must be one line of printable text.`;
        break;
      }
      case "number":
        if (typeof v !== "number" || !Number.isFinite(v)) return `${input.label} must be a number.`;
        if (input.max !== null && v > input.max) return `${input.label} must be at most ${input.max}.`;
        break;
    }
  }
  return null;
}

/** The values to send: empty ones left out (Rust treats them as not given). */
function sentValues(values: Record<string, unknown>): Record<string, unknown> {
  return Object.fromEntries(Object.entries(values).filter(([, v]) => !isEmpty(v)));
}

/**
 * The dialog(s) before a mutating endpoint action runs. A non-mutating
 * action with no note needs none and resolves at once. The confirm text is
 * the question; a note is asked in the same dialog when the action takes
 * one (and refused empty when required). Resolves null when the user backs
 * out.
 */
export async function confirmEndpoint(
  action: Extract<SourceAction, { kind: "endpoint" }>,
  opts: { container?: HTMLElement } = {},
): Promise<ConfirmedEndpoint | null> {
  const { confirm, note } = endpointQuestion(action);
  const inputs = action.inputs ?? [];
  if (inputs.length) {
    // One dialog: the question, every input with its default, the note.
    const answer = await fieldsDialog(
      confirm ?? `${action.label}?`,
      inputs.map((i) => ({ name: i.name, label: i.label, kind: i.kind, options: i.options, required: i.required, value: i.default, max: i.max })),
      { okLabel: action.label, note, check: (values) => checkInputValues(inputs, values), ...opts },
    );
    if (answer === null) return null;
    return { confirmed: confirm !== null, note: answer.note, values: sentValues(answer.values) };
  }
  if (!confirm && !note) return { confirmed: false, note: null };
  const question = confirm ?? `${action.label}?`;
  if (note) {
    const prompt = note === "required" ? `${question}\n\nA note is required.` : `${question}\n\nAdd a note (optional).`;
    // Loop until a usable answer: a required note refuses the empty string
    // and asks again rather than silently sending without it.
    for (;;) {
      const text = await promptDialog(prompt, { okLabel: action.label, placeholder: "note", ...opts });
      if (text === null) return null;
      if (note === "required" && text.trim() === "") continue;
      return { confirmed: confirm !== null, note: text.trim() === "" ? null : text.trim() };
    }
  }
  const ok = await confirmDialog(question, { okLabel: action.label, safeDefault: true, ...opts });
  return ok ? { confirmed: true, note: null } : null;
}

/** The dialog before an agent action runs: the full prompt, tool, folder
 *  and env. Resolves true on "Launch". */
export function confirmAgentPrompt(
  action: Extract<SourceAction, { kind: "agent" }>,
  verdict: ActionVerdict,
  sourceLabel: string,
  opts: { container?: HTMLElement } = {},
): Promise<boolean> {
  return confirmDialog(agentPromptMessage(action, verdict, sourceLabel), { okLabel: "Launch", safeDefault: true, ...opts });
}

/** One line for the user about what `source_run_action` answered. Pure. */
export function outcomeText(outcome: ActionOutcome): string {
  switch (outcome.kind) {
    case "done":
      return outcome.message ?? "Done.";
    case "job":
      return `Started (job ${outcome.job_id}); the result arrives when the source reports it.`;
    case "run":
      return `Started (job ${outcome.job_id}) in a pane here; the source gets the result when it exits.`;
    case "uncertain":
      return outcome.message;
    case "opened":
      return `Opened ${outcome.url}.`;
    case "panel":
      return "";
    case "launched":
      return "Launched.";
  }
}

/**
 * The whole click: ask what the action needs asked, then run it. The
 * `agent` prompt dialog is shown when Rust says it must be (`needsPrompt`),
 * and `confirmed: true` goes out only after the user accepted; an agent run
 * always carries the launch digest of the entry it was drawn from, so Rust
 * can refuse one whose prompt changed in the meantime. Resolves
 * null when the user backed out. `opts.owner`: the action is one of
 * `sourceId`'s item actions on the row `key` of that other source (its
 * confirm text already names the row; Rust filled it).
 */
export async function runAction(
  sourceId: string,
  sourceLabel: string,
  view: string,
  key: string | null,
  entry: ipc.SourceActionDto,
  api: { run: typeof ipc.sourceRunAction } = { run: ipc.sourceRunAction },
  opts: { container?: HTMLElement; owner?: string } = {},
): Promise<ActionOutcome | null> {
  const { action, verdict } = entry;
  let confirmed = false;
  let note: string | null = null;
  let values: Record<string, unknown> | undefined;
  const dialog = opts.container ? { container: opts.container } : {};
  if (action.kind === "endpoint") {
    const answer = await confirmEndpoint(action, dialog);
    if (answer === null) return null;
    confirmed = answer.confirmed;
    note = answer.note;
    values = answer.values;
  } else if (action.kind === "agent") {
    if (entry.needsPrompt) {
      if (!(await confirmAgentPrompt(action, verdict, sourceLabel, dialog))) return null;
      confirmed = true;
    }
    // Not shown now: Rust runs it only if this exact launch was accepted before.
    return api.run(sourceId, view, key, action.id, confirmed, note, opts.owner, undefined, entry.promptHash);
  }
  if (values) return api.run(sourceId, view, key, action.id, confirmed, note, opts.owner, values);
  return opts.owner
    ? api.run(sourceId, view, key, action.id, confirmed, note, opts.owner)
    : api.run(sourceId, view, key, action.id, confirmed, note);
}
