/**
 * The chord table — action id to key combination.
 *
 * Deliberately free of React (and of every other import) so the Electron main
 * process can read it when it builds the application menu. A menu accelerator
 * and a typed chord must resolve to the same action, and the only way to
 * guarantee that is for both to be generated from this one table.
 *
 * `mod` is ⌘ on macOS and Ctrl everywhere else.
 */
export const CHORDS: Readonly<Record<string, string>> = {
  "command-palette": "mod+k",
  "new-conversation": "mod+n",
  "search-sessions": "mod+shift+f",
  "open-settings": "mod+,",
  "toggle-activity": "mod+j",
  "cancel-turn": "escape",
};

/** True on macOS, where `mod` means ⌘ rather than Ctrl. */
export function isMacPlatform(): boolean {
  return /macintosh|mac os x/i.test(globalThis.navigator?.userAgent ?? "");
}

const MAC_SYMBOLS: Record<string, string> = { mod: "⌘", shift: "⇧", alt: "⌥", ctrl: "⌃" };

/**
 * Renders a chord the way it should appear next to a menu or palette entry:
 * `mod+shift+f` → `⌘⇧F` on macOS, `Ctrl+Shift+F` elsewhere. Undefined when the
 * action has no chord, so a caller can omit the hint entirely.
 */
export function hintFor(action: string, isMac: boolean = isMacPlatform()): string | undefined {
  const chord = CHORDS[action];
  if (!chord) return undefined;
  const parts = chord.split("+");
  if (isMac) {
    return parts.map((p) => MAC_SYMBOLS[p] ?? (p.length === 1 ? p.toUpperCase() : p)).join("");
  }
  return parts
    .map((p) =>
      p === "mod" ? "Ctrl" : p.length === 1 ? p.toUpperCase() : p[0]!.toUpperCase() + p.slice(1),
    )
    .join("+");
}

/**
 * Renders a chord as an Electron accelerator: `mod+shift+f` →
 * `CmdOrCtrl+Shift+F`. Returns undefined for an action with no chord, which is
 * what a menu item with no accelerator wants.
 */
export function acceleratorFor(action: string): string | undefined {
  const chord = CHORDS[action];
  if (!chord) return undefined;
  return chord
    .split("+")
    .map((part) => {
      if (part === "mod") return "CmdOrCtrl";
      if (part.length === 1) return part.toUpperCase();
      return part[0]!.toUpperCase() + part.slice(1);
    })
    .join("+");
}
