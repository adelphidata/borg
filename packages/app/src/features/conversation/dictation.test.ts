import { describe, expect, it } from "vitest";
import {
  HOLD_MS,
  formatRecordingClock,
  matchesShortcut,
  mergeTranscript,
  parseShortcut,
  releasesShortcut,
  shortcutLabel,
} from "./dictation";

const key = (init: KeyboardEventInit & { key: string; code?: string }) =>
  new KeyboardEvent("keydown", { code: "", ...init });

describe("parseShortcut", () => {
  it("reads a chord with a modifier, and a lone modifier code", () => {
    expect(parseShortcut("mod+shift+space")).toEqual({ kind: "chord", chord: "mod+shift+space" });
    expect(parseShortcut("AltRight")).toEqual({ kind: "modifier", code: "altright" });
  });

  it("refuses what would swallow typing or binds nothing", () => {
    // A bare letter would start recording on every "d" typed into the box.
    expect(parseShortcut("d")).toBeUndefined();
    expect(parseShortcut("")).toBeUndefined();
    expect(parseShortcut("hyper+d")).toBeUndefined();
    expect(parseShortcut("mod+")).toBeUndefined();
  });
});

describe("shortcutLabel", () => {
  it("renders a chord in the platform's glyphs and a modifier by its side", () => {
    expect(shortcutLabel("mod+shift+space", true)).toBe("⌘⇧Space");
    expect(shortcutLabel("mod+shift+space", false)).toBe("Ctrl+Shift+Space");
    expect(shortcutLabel("altright", true)).toBe("Right ⌥");
    expect(shortcutLabel("altright", false)).toBe("Right Alt");
    expect(shortcutLabel("nonsense", true)).toBe("");
  });
});

describe("matchesShortcut and releasesShortcut", () => {
  const chord = parseShortcut("mod+shift+space")!;
  const mod = parseShortcut("altright")!;
  const chordOf = (e: KeyboardEvent) =>
    [e.metaKey && "mod", e.shiftKey && "shift", e.key === " " ? "space" : e.key.toLowerCase()]
      .filter(Boolean)
      .join("+");

  it("a chord matches the whole chord, not the key alone", () => {
    expect(matchesShortcut(chord, key({ key: " ", metaKey: true, shiftKey: true }), chordOf)).toBe(
      true,
    );
    expect(matchesShortcut(chord, key({ key: " " }), chordOf)).toBe(false);
  });

  it("a lone modifier matches by physical key, so the left one stays free", () => {
    expect(matchesShortcut(mod, key({ key: "Alt", code: "AltRight" }), chordOf)).toBe(true);
    expect(matchesShortcut(mod, key({ key: "Alt", code: "AltLeft" }), chordOf)).toBe(false);
  });

  it("letting go of any key in the chord ends the hold", () => {
    expect(releasesShortcut(chord, key({ key: " " }))).toBe(true);
    expect(releasesShortcut(chord, key({ key: "Shift" }))).toBe(true);
    expect(releasesShortcut(chord, key({ key: "Meta" }))).toBe(true);
    // Not a key in this chord.
    expect(releasesShortcut(chord, key({ key: "Alt" }))).toBe(false);
    expect(releasesShortcut(chord, key({ key: "a" }))).toBe(false);
    expect(releasesShortcut(mod, key({ key: "Alt", code: "AltRight" }))).toBe(true);
    expect(releasesShortcut(mod, key({ key: "Alt", code: "AltLeft" }))).toBe(false);
  });

  it("a hold is longer than a tap", () => {
    // The threshold is a UX contract: a deliberate press-and-speak must not
    // register as a tap that leaves the mic running.
    expect(HOLD_MS).toBeGreaterThanOrEqual(250);
    expect(HOLD_MS).toBeLessThanOrEqual(600);
  });
});

describe("mergeTranscript", () => {
  it("joins spoken words to the draft with exactly one space", () => {
    expect(mergeTranscript("", "  hello ")).toBe("hello");
    expect(mergeTranscript("   ", "hello")).toBe("hello");
    expect(mergeTranscript("Remind me", "hello")).toBe("Remind me hello");
    expect(mergeTranscript("Remind me   ", "hello")).toBe("Remind me hello");
  });
});

describe("formatRecordingClock", () => {
  it("reads as minutes and zero-padded seconds", () => {
    expect(formatRecordingClock(0)).toBe("0:00");
    expect(formatRecordingClock(7)).toBe("0:07");
    expect(formatRecordingClock(65)).toBe("1:05");
  });
});
