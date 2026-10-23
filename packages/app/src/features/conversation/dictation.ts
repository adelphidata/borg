import { hintForChord } from "../../lib/chords";

/**
 * The pure half of dictation: how a shortcut setting is read, how a press is
 * told from a hold, and how spoken words join what was already typed.
 */

/** Held past this, a press is a hold: release stops the recording. */
export const HOLD_MS = 350;

/**
 * Modifier keys that can be a shortcut on their own, by `KeyboardEvent.code`
 * (lowercased). Hold the key to talk, tap it to toggle — what a single
 * modifier is for, since it cannot type anything by itself.
 */
export const MODIFIER_CODES: Readonly<Record<string, string>> = {
  altleft: "Left ⌥",
  altright: "Right ⌥",
  metaleft: "Left ⌘",
  metaright: "Right ⌘",
  controlleft: "Left ⌃",
  controlright: "Right ⌃",
  shiftleft: "Left ⇧",
  shiftright: "Right ⇧",
};

/** On Windows and Linux the same keys have other names. */
const MODIFIER_CODES_PC: Readonly<Record<string, string>> = {
  altleft: "Left Alt",
  altright: "Right Alt",
  metaleft: "Left Win",
  metaright: "Right Win",
  controlleft: "Left Ctrl",
  controlright: "Right Ctrl",
  shiftleft: "Left Shift",
  shiftright: "Right Shift",
};

export type DictationShortcut =
  { kind: "chord"; chord: string } | { kind: "modifier"; code: string };

/**
 * Reads `voice.dictation_shortcut`. A value naming a lone modifier is a
 * hold-to-talk key; anything with a `+` is a chord in the keybinds syntax.
 * Undefined for a value that is neither, so a typo cannot bind every key.
 */
export function parseShortcut(value: string): DictationShortcut | undefined {
  const v = value.trim().toLowerCase();
  if (v === "") return undefined;
  if (v in MODIFIER_CODES) return { kind: "modifier", code: v };
  const parts = v.split("+");
  const key = parts[parts.length - 1];
  // A chord is modifiers plus one key; a bare letter would swallow typing.
  if (parts.length < 2 || !key) return undefined;
  const mods = parts.slice(0, -1);
  if (!mods.every((m) => m === "mod" || m === "ctrl" || m === "alt" || m === "shift")) {
    return undefined;
  }
  return { kind: "chord", chord: v };
}

/** The shortcut as a tooltip or a settings row shows it. */
export function shortcutLabel(value: string, isMac: boolean): string {
  const parsed = parseShortcut(value);
  if (!parsed) return "";
  if (parsed.kind === "modifier") {
    return (isMac ? MODIFIER_CODES : MODIFIER_CODES_PC)[parsed.code] ?? parsed.code;
  }
  return hintForChord(parsed.chord, isMac);
}

/**
 * Whether a keydown is this shortcut. For a chord that means the whole chord;
 * for a modifier, that key alone. `chordOf` is passed in so the matching
 * follows the same platform rules as every other shortcut.
 */
export function matchesShortcut(
  shortcut: DictationShortcut,
  event: KeyboardEvent,
  chordOf: (event: KeyboardEvent) => string,
): boolean {
  if (shortcut.kind === "modifier") return event.code.toLowerCase() === shortcut.code;
  return chordOf(event) === shortcut.chord;
}

/**
 * Whether a keyup ends the hold. Releasing any key in the chord counts: a
 * thumb that leaves the modifier first should stop the recording as surely
 * as a finger leaving the letter.
 */
export function releasesShortcut(shortcut: DictationShortcut, event: KeyboardEvent): boolean {
  if (shortcut.kind === "modifier") return event.code.toLowerCase() === shortcut.code;
  const key = event.key === " " ? "space" : event.key.toLowerCase();
  const parts = shortcut.chord.split("+");
  const main = parts[parts.length - 1];
  if (key === main) return true;
  const mods = new Set(parts.slice(0, -1));
  if (key === "meta" || key === "control") return mods.has("mod") || mods.has("ctrl");
  if (key === "alt") return mods.has("alt");
  if (key === "shift") return mods.has("shift");
  return false;
}

/**
 * Spoken words after what was already typed, exactly one space apart. An
 * empty draft takes the words as they are.
 */
export function mergeTranscript(draft: string, spoken: string): string {
  const words = spoken.trim();
  if (draft.trim() === "") return words;
  return `${draft.trimEnd()} ${words}`;
}

/** `m:ss` for the recording clock. */
export function formatRecordingClock(seconds: number): string {
  const m = Math.floor(seconds / 60);
  const s = seconds % 60;
  return `${m}:${s < 10 ? "0" : ""}${s}`;
}
