// URL opener for OSC 8 / autolink ctrl+click.
//
// The tauri-plugin-opener guest binding is a single invoke call to the
// plugin's `open_url` command; we call it directly instead of importing
// `@tauri-apps/plugin-opener` so the container (which cannot fetch the npm
// package) still type-checks and tests. Scheme gating lives in links.ts —
// this module never filters.

import { invoke } from "@tauri-apps/api/core";

/** Open a URL with the OS default handler (tauri-plugin-opener). */
export function openUrl(url: string): Promise<void> {
  return invoke("plugin:opener|open_url", { url });
}
