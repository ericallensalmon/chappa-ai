// @vitest-environment jsdom
import { describe, expect, it, vi } from "vitest";
import { BLOCK_PREFIX, blockMessage, findingLine, noticeText, offerAcknowledge, offerLaunchAnyway, parseSecretScanBlock } from "./secret_scan";

const block = {
  projectRoot: "C:\\code\\app",
  cwd: "C:\\code\\app-wt\\feature",
  findings: [
    { file: ".env", rule: "github-pat", line: 1 },
    { file: "config/settings.json", rule: "generic-api-key", line: null },
  ],
  skipped: 2,
};

describe("parseSecretScanBlock", () => {
  it("reads the block out of a refusal and ignores other errors", () => {
    expect(parseSecretScanBlock(`${BLOCK_PREFIX}${JSON.stringify(block)}`)).toEqual(block);
    expect(parseSecretScanBlock(new Error(`spawn failed: ${BLOCK_PREFIX}${JSON.stringify(block)}`))).toEqual(block);
    expect(parseSecretScanBlock("busy guard: 2 agents are busy")).toBeNull();
    expect(parseSecretScanBlock(`${BLOCK_PREFIX}not json`)).toBeNull();
    expect(parseSecretScanBlock(`${BLOCK_PREFIX}{"findings": "nope"}`)).toBeNull();
    // Malformed findings are dropped, the rest kept with defaults.
    const loose = parseSecretScanBlock(`${BLOCK_PREFIX}{"findings": [{"file": ".env", "rule": "r"}, {"file": 3}]}`);
    expect(loose).toEqual({ projectRoot: "", cwd: "", findings: [{ file: ".env", rule: "r" }], skipped: 0 });
  });
});

describe("messages", () => {
  it("name file, line and rule and never claim a value", () => {
    expect(findingLine(block.findings[0])).toBe(".env:1  (github-pat)");
    expect(findingLine(block.findings[1])).toBe("config/settings.json  (generic-api-key)");
    const text = blockMessage(block);
    expect(text).toContain("2 possible secrets in C:\\code\\app-wt\\feature");
    expect(text).toContain(".env:1  (github-pat)");
    expect(text).toContain("acknowledge");
    expect(text).toContain("2 files were not scanned");
    expect(blockMessage({ ...block, findings: [block.findings[0]], skipped: 0 })).toContain("1 possible secret in");
    expect(blockMessage({ ...block, skipped: 0 })).not.toContain("not scanned");
  });

  it("describe a warn launch and an incomplete scan", () => {
    const warn = noticeText({ kind: "warn", cwd: "/w", findings: block.findings, skipped: 0 });
    expect(warn.title).toBe("2 possible secrets in the agent's folder");
    expect(warn.body).toContain("policy: warn");
    expect(warn.body).toContain(".env:1  (github-pat)");
    const anyway = noticeText({ kind: "launched-anyway", cwd: "/w", reason: "the secret scan did not finish within 60 s", note: "scanner being replaced" });
    expect(anyway.title).toBe("Launched without a complete secret scan");
    expect(anyway.body).toContain("did not finish within 60 s");
    expect(anyway.body).toContain("scanner being replaced");
  });

  it("read and explain a refusal for a scan that did not finish", () => {
    const incomplete = { projectRoot: "/p", cwd: "/p/wt", findings: [], skipped: 0, incomplete: "gitleaks was not found" };
    expect(parseSecretScanBlock(`${BLOCK_PREFIX}${JSON.stringify(incomplete)}`)).toEqual(incomplete);
    const text = blockMessage(incomplete);
    expect(text).toContain("did not finish");
    expect(text).toContain("gitleaks was not found");
    expect(text).toContain("launch anyway");
    expect(text).not.toContain("acknowledge them");
  });
});

describe("offerAcknowledge", () => {
  const okButton = () => document.querySelector<HTMLButtonElement>(".chappa-dialog-ok")!;

  it("acknowledges only on the Acknowledge button and writes the findings", async () => {
    const acknowledge = vi.fn(async () => 2);
    const cancelled = offerAcknowledge(block, 4, { acknowledge });
    await Promise.resolve();
    expect(okButton().textContent).toBe("Acknowledge");
    expect(document.querySelector(".chappa-dialog-message")!.textContent).toContain("github-pat");
    document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    expect(await cancelled).toBe(false);
    expect(acknowledge).not.toHaveBeenCalled();
    const accepted = offerAcknowledge(block, 4, { acknowledge });
    await Promise.resolve();
    okButton().click();
    expect(await accepted).toBe(true);
    expect(acknowledge).toHaveBeenCalledWith(4, block.findings, null);
  });
});

describe("offerLaunchAnyway", () => {
  const ok = () => document.querySelector<HTMLButtonElement>(".chappa-dialog-ok")!;
  const input = () => document.querySelector<HTMLInputElement>(".chappa-dialog-input")!;
  const incomplete = { projectRoot: "/p", cwd: "/p", findings: [], skipped: 0, incomplete: "the scan did not finish within 60 s" };

  it("needs a note, answers it, and answers null when cancelled", async () => {
    const asked = offerLaunchAnyway(incomplete);
    await Promise.resolve();
    expect(ok().textContent).toBe("Launch anyway");
    input().value = "   ";
    ok().click();
    await Promise.resolve();
    await Promise.resolve();
    // An empty note asks again.
    expect(input()).not.toBeNull();
    input().value = "the scanner is being reinstalled";
    ok().click();
    await expect(asked).resolves.toBe("the scanner is being reinstalled");
    const cancelled = offerLaunchAnyway(incomplete);
    await Promise.resolve();
    document.querySelector<HTMLButtonElement>(".chappa-dialog-cancel")!.click();
    await expect(cancelled).resolves.toBeNull();
  });
});
