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
/**
 * Hash the quick-entry window loads. A cross-process contract: the main
 * process appends it to the bundle URL and the renderer tests for it to skip
 * the full shell, so the two spellings must never drift.
 */
export const QUICK_ENTRY_HASH = "#/quick";

/**
 * The desktop app's system-wide key for the quick-entry panel. It lives here
 * so the settings page can warn when the dictation key is set to the same
 * chord: the panel holds it system-wide, so dictation would never see it.
 */
export const QUICK_ENTRY_CHORD = "mod+shift+space";

export const CHORDS: Readonly<Record<string, string>> = {
  "command-palette": "mod+k",
  "new-conversation": "mod+n",
  "search-sessions": "mod+shift+f",
  "find-in-conversation": "mod+f",
  "open-settings": "mod+,",
  "toggle-activity": "mod+j",
  "toggle-sidebar": "mod+b",
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
  return hintForChord(chord, isMac);
}

/** `hintFor` over a chord string rather than an action id. */
export function hintForChord(chord: string, isMac: boolean = isMacPlatform()): string {
  const parts = chord.split("+");
  if (isMac) {
    return parts
      .map((p) => MAC_SYMBOLS[p] ?? (p.length === 1 ? p.toUpperCase() : capitalize(p)))
      .join("");
  }
  return parts.map((p) => (p === "mod" ? "Ctrl" : capitalize(p))).join("+");
}

/** `space` → `Space`; a single letter is simply uppercased. */
function capitalize(part: string): string {
  return part[0]!.toUpperCase() + part.slice(1);
}

/**
 * Renders a chord as an Electron accelerator: `mod+shift+f` →
 * `CmdOrCtrl+Shift+F`. Returns undefined for an action with no chord, which is
 * what a menu item with no accelerator wants.
 */
export function acceleratorFor(action: string): string | undefined {
  const chord = CHORDS[action];
  if (!chord) return undefined;
  return acceleratorForChord(chord);
}

/** `acceleratorFor` over a chord string rather than an action id. */
export function acceleratorForChord(chord: string): string {
  return chord
    .split("+")
    .map((part) => {
      if (part === "mod") return "CmdOrCtrl";
      if (part.length === 1) return part.toUpperCase();
      return part[0]!.toUpperCase() + part.slice(1);
    })
    .join("+");
}
