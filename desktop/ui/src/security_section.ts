// The Security tab of the settings pane: the secret scan that runs before an
// agent launches. A credential in the folder an agent is about to read (a
// committed key, an untracked .env, a token in a config file) is one `cat`
// away from its context and its provider's logs, so the folder is scanned
// first — untracked and ignored files included, values redacted.
//
// Three controls, all committed on change like every other setting:
// - the switch, three-valued: Automatic (on when gitleaks is installed, the
//   default), Always on, Off — a project can override it from its own row;
// - the policy: block the launch until fixed or acknowledged (default), or
//   launch and warn;
// - the gitleaks path, empty for the one on PATH.
// The hint under the switch says what Automatic resolves to right now,
// which `secret_scan_info` answers from Rust (the webview cannot see PATH).

import * as ipc from "./ipc";
import { SECRET_SCAN_POLICIES, type Settings, type SettingsStore } from "./settings";
import { INPUT_CSS, SELECT_CSS, settingsRow, settingsSection } from "./settings_widgets";

export type ScanSwitch = "auto" | "on" | "off";

/** The switch's three positions in picker order, with their labels. */
export const SCAN_SWITCH_OPTIONS: ReadonlyArray<{ value: ScanSwitch; label: string }> = [
  { value: "auto", label: "Automatic (when gitleaks is installed)" },
  { value: "on", label: "Always" },
  { value: "off", label: "Off" },
];

/** The stored tri-state → the picker value. */
export function switchOf(setting: boolean | null): ScanSwitch {
  return setting === null ? "auto" : setting ? "on" : "off";
}

/** The picker value → the stored tri-state. */
export function settingOf(choice: ScanSwitch): boolean | null {
  return choice === "auto" ? null : choice === "on";
}

/** The sentence under the switch: what the scan does right now, given the
 *  setting and whether a scanner was found. */
export function switchHint(setting: boolean | null, info: ipc.ScannerInfoDto | null): string {
  const where = info?.program ? ` (${info.program})` : "";
  if (setting === true) {
    return info && !info.found
      ? "Always on, but gitleaks was not found: launches go ahead with a warning that nothing was scanned."
      : `Always on${where}.`;
  }
  if (setting === false) return "Off: agent launches are not scanned.";
  if (!info) return "Automatic.";
  return info.found
    ? `Automatic: on, gitleaks was found${where}.`
    : "Automatic: off, gitleaks is not installed. Install it or name its path below to turn the scan on.";
}

/** Why a gitleaks config typed into Settings was not saved: the one reason
 *  the app refuses it. */
export const CONFIG_REFUSED =
  "Not saved: this file is inside an open project folder. Use the project's own config through chappa.yml (secretScan: { config: <path> }), which is trusted like a command.";

export const POLICY_LABELS: Record<ipc.SecretScanPolicy, string> = {
  "block-agents": "Block the launch until fixed or acknowledged",
  warn: "Launch and warn",
};

export interface SecuritySectionOptions {
  settings: SettingsStore;
  /** Where the scanner is looked up. Defaults to the Tauri command; tests
   *  inject a fake. */
  scannerInfo?: () => Promise<ipc.ScannerInfoDto>;
}

export class SecuritySection {
  readonly element: HTMLDivElement;
  private readonly settings: SettingsStore;
  private readonly scannerInfo: () => Promise<ipc.ScannerInfoDto>;
  private readonly switchSelect: HTMLSelectElement;
  private readonly switchHint: HTMLDivElement;
  private readonly policySelect: HTMLSelectElement;
  private readonly programInput: HTMLInputElement;
  private readonly configInput: HTMLInputElement;
  private readonly configHint: HTMLDivElement;
  /** Why the last config typed here was not saved, until the next change. */
  private configRefusedNow: string | null = null;
  private info: ipc.ScannerInfoDto | null = null;
  private unsubscribe: (() => void) | null = null;

  constructor(opts: SecuritySectionOptions) {
    this.settings = opts.settings;
    this.scannerInfo = opts.scannerInfo ?? ipc.secretScanInfo;
    this.element = document.createElement("div");
    this.element.className = "chappa-security-section";
    const section = settingsSection(
      "SECRETS",
      "Before an agent launches, the folder it will work in is scanned for credentials — untracked and " +
        "ignored files included, values never shown. A finding names the file and the rule.",
    );

    this.switchSelect = document.createElement("select");
    this.switchSelect.style.cssText = SELECT_CSS;
    this.switchSelect.dataset.setting = "secretScanBeforeAgents";
    for (const { value, label } of SCAN_SWITCH_OPTIONS) {
      const opt = document.createElement("option");
      opt.value = value;
      opt.textContent = label;
      this.switchSelect.appendChild(opt);
    }
    this.switchSelect.addEventListener("change", () => {
      void this.settings.update({ secretScanBeforeAgents: settingOf(this.switchSelect.value as ScanSwitch) });
    });
    const switchRow = settingsRow(
      "Scan for secrets before launching an agent",
      this.switchSelect,
      "A project can override this from its row under Projects.",
    );
    this.switchHint = document.createElement("div");
    this.switchHint.className = "chappa-security-scan-hint";
    this.switchHint.style.cssText = "font:12px system-ui,sans-serif;color:#8b949e;line-height:1.45;flex:1 1 100%;";
    this.switchHint.dataset.role = "scanHint";
    switchRow.appendChild(this.switchHint);
    section.card.appendChild(switchRow);

    this.policySelect = document.createElement("select");
    this.policySelect.style.cssText = SELECT_CSS;
    this.policySelect.dataset.setting = "secretScanPolicy";
    for (const policy of SECRET_SCAN_POLICIES) {
      const opt = document.createElement("option");
      opt.value = policy;
      opt.textContent = POLICY_LABELS[policy];
      this.policySelect.appendChild(opt);
    }
    this.policySelect.addEventListener("change", () => {
      void this.settings.update({ secretScanPolicy: this.policySelect.value as ipc.SecretScanPolicy });
    });
    section.card.appendChild(
      settingsRow(
        "When something is found",
        this.policySelect,
        "Blocking refuses the launch and offers to acknowledge the finding into the project's local baseline " +
          "(.work/, never committed). Warning launches anyway and raises a notification.",
      ),
    );

    this.programInput = document.createElement("input");
    this.programInput.type = "text";
    this.programInput.spellcheck = false;
    this.programInput.placeholder = "gitleaks on PATH";
    this.programInput.style.cssText = `${INPUT_CSS}width:260px;`;
    this.programInput.dataset.setting = "secretScanProgram";
    this.programInput.addEventListener("change", () => {
      void this.settings.update({ secretScanProgram: this.programInput.value.trim() }).then(() => this.refreshInfo());
    });
    section.card.appendChild(
      settingsRow("gitleaks executable", this.programInput, "Empty uses the gitleaks found on PATH."),
    );

    this.configInput = document.createElement("input");
    this.configInput.type = "text";
    this.configInput.spellcheck = false;
    this.configInput.placeholder = "the app's own rules";
    this.configInput.style.cssText = `${INPUT_CSS}width:260px;`;
    this.configInput.dataset.setting = "secretScanConfig";
    this.configInput.addEventListener("change", () => {
      const typed = this.configInput.value.trim();
      void this.settings.update({ secretScanConfig: typed }).then((stored) => {
        // The one refusal for this field: a file inside an open project.
        this.configRefusedNow = typed !== "" && stored.secretScanConfig !== typed ? CONFIG_REFUSED : null;
        return this.refreshInfo();
      });
    });
    const configRow = settingsRow(
      "gitleaks config",
      this.configInput,
      "Empty uses gitleaks' default rules plus rules for passwords, tokens and keys written inline in commands. " +
        "A .gitleaks.toml or .gitleaksignore inside a scanned folder is never used. " +
        "A project keeps its own config through chappa.yml (secretScan: { config: <path> }), trusted like a command; it is used ahead of this one.",
    );
    this.configHint = document.createElement("div");
    this.configHint.className = "chappa-security-config-hint";
    this.configHint.style.cssText = "font:12px system-ui,sans-serif;color:#f0883e;line-height:1.45;flex:1 1 100%;";
    this.configHint.dataset.role = "configHint";
    configRow.appendChild(this.configHint);
    section.card.appendChild(configRow);

    this.element.appendChild(section.element);
    this.sync(this.settings.get());
  }

  /** Called by the pane on open: subscribe and look the scanner up. */
  activate(): void {
    this.unsubscribe?.();
    this.unsubscribe = this.settings.subscribe((s) => this.sync(s));
    this.sync(this.settings.get());
    void this.refreshInfo();
  }

  deactivate(): void {
    this.unsubscribe?.();
    this.unsubscribe = null;
  }

  dispose(): void {
    this.deactivate();
    this.element.remove();
  }

  /** The scanner lookup's last answer (tests read it). */
  get scanner(): ipc.ScannerInfoDto | null {
    return this.info;
  }

  private async refreshInfo(): Promise<void> {
    try {
      this.info = await this.scannerInfo();
    } catch {
      // Outside Tauri there is no scanner to ask about; the hint stays generic.
      this.info = null;
    }
    this.sync(this.settings.get());
  }

  sync(s: Settings): void {
    this.switchSelect.value = switchOf(s.secretScanBeforeAgents);
    this.policySelect.value = s.secretScanPolicy;
    if (document.activeElement !== this.programInput) this.programInput.value = s.secretScanProgram;
    if (document.activeElement !== this.configInput) this.configInput.value = s.secretScanConfig;
    this.switchHint.textContent = switchHint(s.secretScanBeforeAgents, this.info);
    this.configHint.textContent =
      this.configRefusedNow ?? (this.info?.configRefused ? `Not used: ${this.info.configRefused}.` : "");
  }
}
