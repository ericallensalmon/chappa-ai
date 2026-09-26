// The Timers tab of the settings pane: the five idle-timer knobs. A timer
// wakes an agent when a process's output goes quiet; these tune the idle rule
// (how long silence must last, how it is re-confirmed) and the delivery side
// (timeout, de-duplication, how long fired timers stay listed).
//
// Same house rules as every other setting: commit on change (never per
// keystroke — every write is a full-struct set_settings), adopt the value
// Rust returns so a clamp visibly snaps the field back, no Save button.

import {
  IDLE_THRESHOLD_MAX,
  IDLE_THRESHOLD_MIN,
  TIMER_CONFIRM_MAX,
  TIMER_CONFIRM_MIN,
  TIMER_DEDUPE_MAX,
  TIMER_DEDUPE_MIN,
  TIMER_DELIVERY_TIMEOUT_MAX,
  TIMER_DELIVERY_TIMEOUT_MIN,
  TIMER_RETENTION_HOURS_MAX,
  TIMER_RETENTION_HOURS_MIN,
  type Settings,
  type SettingsStore,
} from "./settings";
import { INPUT_CSS, settingsRow, settingsSection } from "./settings_widgets";

type TimerSetting =
  | "idleThresholdMs"
  | "timerConfirmMs"
  | "timerDeliveryTimeoutMs"
  | "timerDedupeMs"
  | "timerRetentionHours";

/** The five timer knobs, as one table — label, settings key, range,
 *  and the hint under it. Ranges mirror `project_model::settings`; Rust clamps,
 *  so an out-of-range entry snaps back visibly. */
const TIMER_SETTINGS: readonly [string, TimerSetting, number, number, string][] = [
  [
    "Idle threshold (ms)",
    "idleThresholdMs",
    IDLE_THRESHOLD_MIN,
    IDLE_THRESHOLD_MAX,
    "How long a process's pty byte stream must be silent before an idle timer calls it idle. Derived from bytes, never from render diffs; a timer can override it with idle_ms.",
  ],
  [
    "Fire-time confirm (ms)",
    "timerConfirmMs",
    TIMER_CONFIRM_MIN,
    TIMER_CONFIRM_MAX,
    "A met idle condition is re-checked after this long instead of firing at once. A byte in the window re-arms the timer — this is what stops a flicker from waking an agent. 0 fires on the first observation.",
  ],
  [
    "Delivery timeout (ms)",
    "timerDeliveryTimeoutMs",
    TIMER_DELIVERY_TIMEOUT_MIN,
    TIMER_DELIVERY_TIMEOUT_MAX,
    "How long a firing waits for its target's ready gate before recording delivered: false. The body is never silently dropped.",
  ],
  [
    "Duplicate window (ms)",
    "timerDedupeMs",
    TIMER_DEDUPE_MIN,
    TIMER_DEDUPE_MAX,
    "An identical body to the same process inside this window is coalesced instead of typed again (timer_list shows coalesced_with). 0 disables it.",
  ],
  [
    "Fired-timer retention (h)",
    "timerRetentionHours",
    TIMER_RETENTION_HOURS_MIN,
    TIMER_RETENTION_HOURS_MAX,
    "How long fired, cancelled and expired timers stay visible to timer_list(include_fired) with their delivery receipts.",
  ],
];

export interface TimersSectionOptions {
  settings: SettingsStore;
}

export class TimersSection {
  readonly element: HTMLDivElement;
  private readonly settings: SettingsStore;
  private readonly inputs: { input: HTMLInputElement; key: TimerSetting }[] = [];
  private unsubscribe: (() => void) | null = null;

  constructor(opts: TimersSectionOptions) {
    this.settings = opts.settings;
    this.element = document.createElement("div");
    this.element.className = "chappa-timers-section";
    const section = settingsSection(
      "TIMERS",
      "Idle timers wake an agent once its output has been quiet for long enough. " +
        "These set what counts as quiet and how a fired timer is delivered.",
    );
    for (const [label, key, min, max, why] of TIMER_SETTINGS) {
      const input = document.createElement("input");
      input.type = "number";
      input.min = String(min);
      input.max = String(max);
      input.style.cssText = `${INPUT_CSS}width:90px;`;
      input.dataset.setting = key;
      input.addEventListener("change", () => {
        const n = Number(input.value);
        if (!Number.isFinite(n)) {
          input.value = String(this.settings.get()[key]);
          return;
        }
        void this.settings.update({ [key]: n } as Partial<Settings>);
      });
      section.card.appendChild(settingsRow(label, input, why));
      this.inputs.push({ input, key });
    }
    this.element.appendChild(section.element);
    this.sync(this.settings.get());
  }

  /** Called by the pane on open: subscribe to the store. */
  activate(): void {
    this.unsubscribe?.();
    this.unsubscribe = this.settings.subscribe((s) => this.sync(s));
    this.sync(this.settings.get());
  }

  deactivate(): void {
    this.unsubscribe?.();
    this.unsubscribe = null;
  }

  dispose(): void {
    this.deactivate();
    this.element.remove();
  }

  /** Push the canonical values into the fields, never touching the one the
   *  user is editing. */
  sync(s: Settings): void {
    for (const { input, key } of this.inputs) {
      if (document.activeElement !== input) input.value = String(s[key]);
    }
  }
}
