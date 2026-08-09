import { useEffect, useRef } from "react";

/**
 * The one keyboard layer.
 *
 * Every shortcut is an *action id* with a handler. A chord (typed in the
 * window) and a native menu item (clicked in the Electron menu bar) both
 * resolve to the same action, so a shortcut has exactly one implementation and
 * the two can never drift apart.
 *
 * Handlers live in a module-level registry rather than React state: the
 * listener is installed once by `installKeybinds`, and components register and
 * unregister as they mount. Last registration wins, which is what a modal
 * wants — it takes Escape while it is open and hands it back on close.
 */

/** Actions with a chord. Anything else is dispatchable but has no key. */
export const CHORDS: Readonly<Record<string, string>> = {
  "command-palette": "mod+k",
  "new-conversation": "mod+n",
  "search-sessions": "mod+shift+f",
  "open-settings": "mod+,",
  "cancel-turn": "escape",
  // No chord: reachable from the Help menu only. Dispatchable all the same.
};

type Handler = () => void;

interface Registration {
  handler: Handler;
  /** Fire even when focus sits in a text field. */
  inInputs: boolean;
}

/** Stacked per action so an overlay can shadow a page and restore it on close. */
const registry = new Map<string, Registration[]>();

/** Runs the frontmost handler for `action`. Returns false when nothing is bound. */
export function dispatchKeybind(action: string): boolean {
  const stack = registry.get(action);
  const top = stack?.[stack.length - 1];
  if (!top) return false;
  top.handler();
  return true;
}

function register(action: string, entry: Registration): () => void {
  const stack = registry.get(action) ?? [];
  stack.push(entry);
  registry.set(action, stack);
  return () => {
    const current = registry.get(action);
    if (!current) return;
    const at = current.indexOf(entry);
    if (at !== -1) current.splice(at, 1);
    if (current.length === 0) registry.delete(action);
  };
}

/** Normalizes a keyboard event to the chord syntax used in `CHORDS`. */
export function chordOf(event: KeyboardEvent, isMac: boolean): string {
  const parts: string[] = [];
  // One "mod" token keeps call sites platform-free: ⌘ on macOS, Ctrl elsewhere.
  if (isMac ? event.metaKey : event.ctrlKey) parts.push("mod");
  // The non-mod control key is still expressible, e.g. "alt+ctrl+p" on a Mac.
  if (isMac ? event.ctrlKey : event.metaKey) parts.push("ctrl");
  if (event.altKey) parts.push("alt");
  if (event.shiftKey) parts.push("shift");
  // "K" and "k" are the same chord; "Escape" keeps its name, lowercased.
  parts.push(event.key.toLowerCase());
  return parts.join("+");
}

/** Reverse of `CHORDS`, so a keydown is one lookup rather than a scan. */
const byChord = new Map(Object.entries(CHORDS).map(([action, chord]) => [chord, action]));

/** True for targets where a bare key should type instead of triggering an action. */
export function isEditable(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false;
  const tag = target.tagName;
  return (
    tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT" || target.isContentEditable === true
  );
}

/**
 * Installs the global listener. Called once, by `BorgApp`.
 *
 * Returns the teardown so the caller can drop it on unmount.
 */
export function installKeybinds(target: Window, isMac: boolean): () => void {
  const onKeyDown = (event: KeyboardEvent) => {
    if (event.defaultPrevented) return;
    const chord = chordOf(event, isMac);
    const action = byChord.get(chord);
    if (!action) return;

    const stack = registry.get(action);
    const top = stack?.[stack.length - 1];
    if (!top) return;
    // A chord with no modifier would otherwise swallow ordinary typing.
    if (!top.inInputs && isEditable(event.target) && !/\+/.test(chord)) return;

    event.preventDefault();
    top.handler();
  };

  target.addEventListener("keydown", onKeyDown);
  return () => target.removeEventListener("keydown", onKeyDown);
}

/**
 * Binds `action` to `handler` for the lifetime of the component.
 *
 * The handler is held on a ref, so a fresh closure per render does not churn
 * the registry — only `action` and `enabled` re-register.
 */
export function useKeybind(
  action: string,
  handler: Handler,
  options: { enabled?: boolean; inInputs?: boolean } = {},
): void {
  const { enabled = true, inInputs = false } = options;
  const handlerRef = useRef(handler);
  handlerRef.current = handler;

  useEffect(() => {
    if (!enabled) return;
    return register(action, { handler: () => handlerRef.current(), inInputs });
  }, [action, enabled, inInputs]);
}

/** Test seam: drops every registration. */
export function resetKeybinds(): void {
  registry.clear();
}
