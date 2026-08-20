import { describe, expect, it } from "vitest";
import { CHORDS, acceleratorFor, hintFor } from "./chords";

describe("acceleratorFor", () => {
  it("renders a chord in Electron's accelerator grammar", () => {
    expect(acceleratorFor("new-conversation")).toBe("CmdOrCtrl+N");
    expect(acceleratorFor("search-sessions")).toBe("CmdOrCtrl+Shift+F");
    expect(acceleratorFor("open-settings")).toBe("CmdOrCtrl+,");
  });

  it("has no accelerator for an action with no chord", () => {
    // "check-for-updates" is dispatchable from the menu but owns no key.
    expect(acceleratorFor("check-for-updates")).toBeUndefined();
  });

  it("produces an accelerator for every chord, so no menu item silently loses its key", () => {
    for (const action of Object.keys(CHORDS)) {
      expect(acceleratorFor(action)).toBeTruthy();
    }
  });

  it("gives no two actions the same chord", () => {
    // A duplicate is silent: the key still fires, just for whichever action the
    // registry happens to resolve last, and the other one is simply gone.
    const chords = Object.values(CHORDS);
    expect(new Set(chords).size).toBe(chords.length);
  });
});

describe("hintFor", () => {
  it("uses the modifier glyphs on macOS", () => {
    expect(hintFor("new-conversation", true)).toBe("⌘N");
    expect(hintFor("search-sessions", true)).toBe("⌘⇧F");
  });

  it("spells the modifiers out everywhere else", () => {
    expect(hintFor("new-conversation", false)).toBe("Ctrl+N");
    expect(hintFor("search-sessions", false)).toBe("Ctrl+Shift+F");
  });

  it("has no hint for an action with no chord", () => {
    expect(hintFor("check-for-updates", true)).toBeUndefined();
  });
});
