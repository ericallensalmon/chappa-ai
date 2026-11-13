// Opening a ctrl+clicked file path. The panels detect and resolve the path
// purely (term/paths.ts); this is the click-time half: ask Rust whether the
// target exists, then have it open the file in the configured editor (or a
// directory in the file manager). Every failure comes back as the one line
// the panel flashes, never as a thrown error.

import * as ipc from "./ipc";
import type { OpenPathTarget } from "./term/panel";

/** The IPC seam; tests inject a fake. */
export interface FileOpenerApi {
  homeDir(): Promise<string | null>;
  resolvePath(base: string | null, text: string): Promise<ipc.ResolvedPathDto>;
  openInEditor(path: string, line?: number, col?: number): Promise<void>;
}

/** The real adapter. Outside Tauri there is no home and nothing opens. */
export const tauriFileOpener: FileOpenerApi = {
  homeDir: () => (ipc.inTauri() ? ipc.homeDir() : Promise.resolve(null)),
  resolvePath: (base, text) => ipc.resolvePath(base, text),
  openInEditor: (path, line, col) => ipc.openInEditor(path, line, col),
};

function reason(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}

/**
 * Open `target`: resolve it on disk first, so a missing file is reported as
 * `not found: <path>` instead of being handed to an editor that would create
 * it. Resolves to the message to show, or null when the open went ahead.
 * The target is already absolute when the panel had a base; a path it could
 * not resolve (no base) is reported missing rather than guessed at.
 */
export async function openFileTarget(api: FileOpenerApi, target: OpenPathTarget): Promise<string | null> {
  let resolved: ipc.ResolvedPathDto;
  try {
    resolved = await api.resolvePath(null, target.path);
  } catch (err) {
    return reason(err);
  }
  if (!resolved.exists) return `not found: ${resolved.path}`;
  try {
    // A directory has no location to jump to; Rust opens it in the file
    // manager either way.
    if (resolved.kind === "dir") await api.openInEditor(resolved.path);
    else await api.openInEditor(resolved.path, target.line, target.col);
  } catch (err) {
    return reason(err);
  }
  return null;
}
