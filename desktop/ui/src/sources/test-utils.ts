// Shared by the source views' tests: the fixture source's JSON payloads and
// small builders for listings and payloads.
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import type * as ipc from "../ipc";
import type { ViewPayload } from "./prs_model";

// Resolved by path, not `new URL(…, import.meta.url)`: under jsdom `URL` is
// the DOM's, which node's fileURLToPath does not take.
const DATA_DIR = join(dirname(fileURLToPath(import.meta.url)), "../../../sources/fixture/data");

/** A fixture payload, parsed fresh each call (tests may mutate it). */
export function fixture(name: "prs" | "attention" | "work" | "manifest" | "panel"): Record<string, unknown> & { items: Record<string, unknown>[] } {
  return JSON.parse(readFileSync(`${DATA_DIR}/${name}.json`, "utf8"));
}

/** One source's payload as the data layer hands it over. */
export function payload(sourceId: string, body: unknown, over: Partial<ViewPayload> = {}): ViewPayload {
  return { sourceId, sourceLabel: sourceId, payload: body, freshness: { asOf: null, refreshing: false, errors: [] }, ...over };
}

/** A PR item with the required fields filled. */
export function pr(number: string, over: Record<string, unknown> = {}): Record<string, unknown> {
  return {
    forge: "github",
    repo: "owner/repo",
    number,
    url: `https://registry.example/owner/repo/pull/${number.replace("#", "")}`,
    title: `Change ${number}`,
    author: { name: "Alex", isMe: true },
    state: "open",
    role: "mine",
    bucket: "action",
    reasons: [],
    ...over,
  };
}

/** A body of the PRs view. */
export function prsBody(items: unknown[], asOf: string | null = "2026-01-15T10:00:00Z", extra: Record<string, unknown> = {}): Record<string, unknown> {
  return { freshness: { asOf, ...extra }, items };
}

/** A listed source serving `views`. */
export function source(id: string, views: string[], over: Partial<ipc.SourceDto> = {}): ipc.SourceDto {
  return {
    id,
    label: id,
    origin: { kind: "launched", project_id: 1, project_path: "C:\\code\\x", command: `${id}-source` },
    status: "up",
    lastError: null,
    trusted: true,
    trust: null,
    manifestId: id,
    version: "0.1.0",
    views: views.map((name) => ({ name, pollSeconds: 60, asOf: null, nextUtc: null, refreshing: false, lastError: null, hasPayload: true })),
    warning: null,
    promptPolicy: "first_time",
    promptsShown: [],
    jobs: [],
    ...over,
  };
}
