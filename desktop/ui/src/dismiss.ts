// The shared capture-phase dismiss helper.
//
// Extracted from the FIVE hand-rolled copies of the same pattern (
// review debt): the panel context menu (term/panel.ts), and app.ts's project
// menu, new-terminal menu, project-terminals menu and command-row menu. Every
// copy shared the core:
//
//   - a CAPTURE-phase document `mousedown` listener — it fires BEFORE the
//     menu items' click can land, so a press INSIDE the surface must not hide
//     it (hiding the button mid-gesture swallows its click — the host
//     gate: Copy/Paste were unclickable);
//   - an inside-guard: `contains(e.target)` on the surface itself;
//
// …but the copies had DRIFTED on two axes, both preserved here as parameters
// rather than papered over:
//
//   - ANCHOR EXCLUSION: the new-terminal ▾ and project-terminals ▾ exclude
//     their own toggle button (without it, pressing the toggle while open
//     would dismiss on mousedown and then re-open on the very same click).
//     The project menu, row menu and panel menu exclude nothing beyond the
//     surface — context menus have no toggle, and the project switcher's
//     press-while-open close-then-reopen is its long-standing behaviour.
//   - ESCAPE: only the panel menu and the command-row menu bind a capture
//     keydown that dismisses on Escape; the three ▾/dropdown menus never did.
//
// The two surfaces (notification drawer, banner overflow) are built
// on this same helper with their own parameter choices.

export interface DismissOptions {
  /** Whether the surface is currently open. The dismiss callback only fires
   *  while this is true (the copies all guarded on `display !== "none"`). */
  isOpen: () => boolean;
  /** Hide the surface. */
  dismiss: () => void;
  /** Nodes whose containment of the press target BLOCKS dismissal: the
   *  surface itself, plus any anchor/toggle to exclude. */
  inside: readonly Node[];
  /** Also dismiss on Escape (capture-phase keydown). Default false — only
   *  the panel menu and the command-row menu ever had it. */
  escape?: boolean;
}

/**
 * Register the capture-phase outside-press dismiss (and, opted in, Escape)
 * for one surface. Returns the unbind function; callers own calling it at
 * teardown exactly where they removed the hand-rolled listeners before.
 */
export function registerDismiss(opts: DismissOptions): () => void {
  const onMousedown = (e: MouseEvent): void => {
    const target = e.target as Node;
    for (const node of opts.inside) {
      if (node.contains(target)) return;
    }
    if (opts.isOpen()) opts.dismiss();
  };
  document.addEventListener("mousedown", onMousedown, true);
  let onKeydown: ((e: KeyboardEvent) => void) | null = null;
  if (opts.escape) {
    onKeydown = (e: KeyboardEvent): void => {
      if (e.key === "Escape" && opts.isOpen()) opts.dismiss();
    };
    document.addEventListener("keydown", onKeydown, true);
  }
  return () => {
    document.removeEventListener("mousedown", onMousedown, true);
    if (onKeydown) document.removeEventListener("keydown", onKeydown, true);
  };
}
