/**
 * Hands keyboard focus back to the composer from anywhere in the conversation.
 *
 * Controls that unmount on use (an answered question, a closed menu) drop focus
 * on `<body>`, where the next keystrokes go nowhere. The composer registers its
 * textarea here; those controls call `focusComposer()` once they are done.
 */

let target: (() => HTMLTextAreaElement | null) | undefined;

/** Called by the composer on mount. Returns the unregister function. */
export function registerComposer(get: () => HTMLTextAreaElement | null): () => void {
  target = get;
  return () => {
    if (target === get) target = undefined;
  };
}

function isEditable(el: Element | null): boolean {
  if (!(el instanceof HTMLElement)) return false;
  return el.tagName === "INPUT" || el.tagName === "TEXTAREA" || el.isContentEditable;
}

/**
 * Focuses the composer unless the user is typing somewhere else.
 * Returns whether focus moved.
 */
export function focusComposer(): boolean {
  const el = target?.();
  if (!el) return false;
  const active = document.activeElement;
  if (active !== el && isEditable(active)) return false;
  el.focus();
  return true;
}
