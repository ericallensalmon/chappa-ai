// Shared test plumbing for the journey tests (links/search/marks):
// a flexible wire-frame builder and a stubbed TerminalApi. Imported only by
// `*.test.ts`; never referenced by the app bundle.

import { vi } from "vitest";
import type { TerminalApi } from "./panel";
import {
  SettingsStore,
  clampSettings,
  defaultSettings,
  type Settings,
  type SettingsApi,
} from "../settings";

/** One cell's content in a row spec: a bare string is one char (or a blank
 *  cell when the row is shorter than `cols`); an object sets the char and/or
 *  OSC 8 link id explicitly. */
export type CellSpec = string | { ch?: string; link?: number };

export interface FrameSpec {
  cols: number;
  rows: number;
  seq?: number;
  kind?: 0 | 1;
  displayOffset?: number;
  historyLen?: number;
  selectionActive?: boolean;
  matches?: { startRow: number; startCol: number; endRow: number; endCol: number }[];
  /** Row-index → per-cell content (trailing cells blank). A bare string is
   *  one char per cell — shorthand for a plain-text row. Absent rows are
   *  blank on a full frame. */
  content?: Record<number, CellSpec[] | string>;
}

/** Encode one full/delta frame per protocol.ts v1. Rows not listed are blank
 *  on a full; a delta still writes every listed row (deltas apply onto the
 *  panel's retained store, so only the covered rows matter). */
export function frame(spec: FrameSpec): ArrayBuffer {
  const { cols, rows, seq = 1, kind = 0 } = spec;
  void rows; // informational — the decoded store dims come from `content`
  const rowSpecs = spec.content ?? {};
  const rowCount = Object.keys(rowSpecs).length;
  const cellCount = cols * rowCount;
  const buf = new ArrayBuffer(37 + rowCount * 6 + cellCount * 16 + 2 + 2 + (spec.matches?.length ?? 0) * 12);
  const view = new DataView(buf);
  let p = 0;
  view.setUint8(p++, 0xd7);
  view.setUint8(p++, 1);
  view.setUint32(p, seq, true);
  p += 4;
  view.setUint8(p++, kind);
  view.setUint8(p++, spec.selectionActive ? 1 : 0);
  view.setUint16(p, 0, true); // cursor row
  p += 2;
  view.setUint16(p, 0, true); // cursor col
  p += 2;
  view.setUint8(p++, 4); // cursor shape: hidden
  view.setUint8(p++, 0); // cursor visible
  view.setUint32(p, spec.displayOffset ?? 0, true);
  p += 4;
  view.setUint32(p, spec.historyLen ?? 0, true);
  p += 4;
  view.setUint8(p++, spec.selectionActive ? 1 : 0);
  view.setUint32(p, 0, true);
  p += 4;
  view.setUint16(p, 0, true);
  p += 2;
  view.setUint32(p, 0, true);
  p += 4;
  view.setUint16(p, 1, true);
  p += 2;
  view.setUint16(p, rowCount, true);
  p += 2;
  const sorted = Object.entries(rowSpecs).sort(([a], [b]) => Number(a) - Number(b));
  for (const [rowStr, rowSpec] of sorted) {
    const row = Number(rowStr);
    const cells = typeof rowSpec === "string" ? Array.from(rowSpec) : rowSpec;
    view.setUint16(p, row, true);
    p += 2;
    view.setUint16(p, 0, true); // colStart
    p += 2;
    view.setUint16(p, cols, true);
    p += 2;
    for (let c = 0; c < cols; c++) {
      const spec_ = cells[c] ?? "";
      const ch = typeof spec_ === "string" ? spec_.codePointAt(0) ?? 0x20 : spec_.ch?.codePointAt(0) ?? 0x20;
      const link = typeof spec_ === "string" ? 0 : (spec_.link ?? 0);
      view.setUint32(p, ch, true);
      p += 4;
      view.setUint32(p, 0xd8d8d8ff, true); // fg
      p += 4;
      view.setUint32(p, 0x181818ff, true); // bg
      p += 4;
      view.setUint16(p, 0, true); // flags
      p += 2;
      view.setUint16(p, link, true);
      p += 2;
    }
  }
  view.setUint16(p, 0, true); // zerowidth_count
  p += 2;
  view.setUint16(p, spec.matches?.length ?? 0, true);
  p += 2;
  for (const m of spec.matches ?? []) {
    view.setUint32(p, m.startRow, true);
    p += 4;
    view.setUint16(p, m.startCol, true);
    p += 2;
    view.setUint32(p, m.endRow, true);
    p += 4;
    view.setUint16(p, m.endCol, true);
    p += 2;
  }
  return buf;
}

export interface ApiHarness {
  api: TerminalApi;
  onFrame: () => ((buf: ArrayBuffer) => void) | null;
}

/** A stubbed TerminalApi capturing every call as a vi.fn (the journey tests
 *  assert exact wire messages). `onFrame` hands back the frames callback the
 *  last `create_terminal` registered. */
export function stubApi(): ApiHarness {
  let onFrame: ((buf: ArrayBuffer) => void) | null = null;
  const api: TerminalApi = {
    createTerminal: vi.fn(async (opts) => {
      onFrame = opts.onFrame;
      return 42;
    }),
    writeKey: vi.fn(async () => {}),
    paste: vi.fn(async () => {}),
    mouse: vi.fn(async () => {}),
    resize: vi.fn(async () => {}),
    scroll: vi.fn(async () => {}),
    setDisplayOffset: vi.fn(async () => {}),
    ack: vi.fn(async () => {}),
    requestFull: vi.fn(async () => {}),
    selection: vi.fn(async () => {}),
    // The second arg is `skipWhitespaceOnly`. The default stub answers
    // null (nothing copied) — the copy-on-select tests override it per case.
    copySelection: vi.fn(async (_id: number, _skipWhitespaceOnly?: boolean) => null as string | null),
    search: vi.fn(async () => {}),
    searchNav: vi.fn(async () => {}),
    closeTerminal: vi.fn(async () => null),
    debugStats: vi.fn(async () => null),
    listTerminals: vi.fn(async () => []),
    attachTerminal: vi.fn(async (_id: number, _frames: unknown) => ({
      id: 0,
      name: "",
      status: "running",
      exit_code: null,
      cols: 80,
      rows: 24,
      seq: 1,
      kind: "terminal" as const,
      project_id: null,
      agent_tool_id: null,
    })),
    listProjects: vi.fn(async () => []),
    addProject: vi.fn(async () => ({ id: 1, name: "p", path: "/p", icon: null, notificationLevel: null })),
    renameProject: vi.fn(async () => {}),
    removeProject: vi.fn(async () => {}),
    pickDirectory: vi.fn(async () => null),
    openProject: vi.fn(async () => ({
      project: { id: 1, name: "p", path: "/p", icon: null, notificationLevel: null },
      trustPending: false,
      trustCommands: [],
      processes: [],
    })),
    confirmProjectTrust: vi.fn(async () => {}),
    listProjectProcesses: vi.fn(async () => []),
    startProjectProcess: vi.fn(async () => 42),
    stopProjectProcess: vi.fn(async () => {}),
    restartProjectProcess: vi.fn(async () => 42),
    setNotificationLevel: vi.fn(async () => {}),
    osNotify: vi.fn(async () => {}),
    saveProjectProcess: vi.fn(async () => []),
    deleteProjectProcess: vi.fn(async () => []),
    duplicateProjectProcesses: vi.fn(async () => []),
    setProcessFavorite: vi.fn(async () => {}),
    setProcessAutoRename: vi.fn(async () => {}),
    listWorkspaceCommands: vi.fn(async () => []),
    saveWorkspaceCommand: vi.fn(async () => []),
    deleteWorkspaceCommand: vi.fn(async () => []),
    startWorkspaceCommand: vi.fn(async () => 42),
    stopWorkspaceCommand: vi.fn(async () => {}),
    restartWorkspaceCommand: vi.fn(async () => 42),
    listAgentTools: vi.fn(async () => ({ tools: [], machine_mode_types: ["claude", "opencode"] as const })),
    upsertAgentTool: vi.fn(async (t) => t),
    deleteAgentTool: vi.fn(async () => {}),
    parseAgentCommand: vi.fn(async () => ({
      tool: emptyAgentTool(),
      docker_detected: false,
      warnings: [],
    })),
    spawnAgent: vi.fn(async () => {
      throw new Error("spawnAgent not stubbed");
    }),
    sendAgentInput: vi.fn(async () => ({ delivered: true, waited_ms: 0, seq_before: 0 })),
    getAgentEvents: vi.fn(async () => []),
  };
  return { api, onFrame: () => onFrame };
}

/** A blank custom Host tool (the `parseAgentCommand` stub's answer). */
export function emptyAgentTool(): import("../ipc").AgentToolDto {
  return {
    id: 0,
    name: "",
    tool_type: "custom",
    program: "",
    args: [],
    model: null,
    runtime: { kind: "host" },
    env: {},
    enabled: true,
    max_busy: null,
    transport: "tty",
  };
}

export interface SettingsHarness {
  store: SettingsStore;
  api: { getSettings: ReturnType<typeof vi.fn>; setSettings: ReturnType<typeof vi.fn> } & SettingsApi;
  /** The last value `set_settings` was asked to store. */
  saved: () => Settings | null;
}

/**
 * A settings store over a fake IPC. `stored` seeds the "Rust side";
 * `canonical` lets a test model Rust's clamping — whatever it returns is what
 * the store must adopt, which is the point of the full-struct set contract.
 */
export function stubSettings(
  stored: Partial<Settings> = {},
  canonical: (s: Settings) => Settings = (s) => s,
): SettingsHarness {
  let current = clampSettings({ ...defaultSettings(), ...stored });
  let saved: Settings | null = null;
  const api = {
    getSettings: vi.fn(async () => current),
    setSettings: vi.fn(async (s: Settings) => {
      saved = s;
      current = canonical(s);
      return current;
    }),
  };
  return { store: new SettingsStore(api as SettingsApi), api: api as SettingsHarness["api"], saved: () => saved };
}

/** Give the host a real rect so the panel's ResizeObserver path sizes the
 *  grid (jsdom cannot lay out). */
export function mockHostRect(host: HTMLElement, w: number, h: number): void {
  host.getBoundingClientRect = () =>
    ({
      left: 0,
      top: 0,
      right: w,
      bottom: h,
      width: w,
      height: h,
      x: 0,
      y: 0,
      toJSON: () => ({}),
    }) as DOMRect;
}

export const FLUSH = (): Promise<void> => new Promise((resolve) => setTimeout(resolve, 0));

/** Paint the panel's rAF queue once (frames are rendered + acked in rAF). */
export const paint = (): Promise<void> =>
  new Promise((resolve) => requestAnimationFrame(() => resolve()));
