import { beforeEach, describe, expect, it } from "vitest";
import { STORAGE_KEYS } from "@borg/ui";
import { byRecency, noteSkillUse, readSkillRecency } from "./skillRecency";

/** A Storage that throws on write, which is what Safari private mode does. */
function refusingStore(): Storage {
  const inner = new Map<string, string>();
  return {
    getItem: (k: string) => inner.get(k) ?? null,
    setItem: () => {
      throw new DOMException("QuotaExceededError");
    },
    removeItem: (k: string) => void inner.delete(k),
    clear: () => inner.clear(),
    key: () => null,
    get length() {
      return inner.size;
    },
  } as Storage;
}

beforeEach(() => localStorage.clear());

describe("skill recency", () => {
  it("leads with the skill used last", () => {
    noteSkillUse(localStorage, "git");
    noteSkillUse(localStorage, "weather");
    expect(readSkillRecency(localStorage)).toEqual(["weather", "git"]);
  });

  it("moves a repeat use to the front instead of listing it twice", () => {
    noteSkillUse(localStorage, "git");
    noteSkillUse(localStorage, "weather");
    noteSkillUse(localStorage, "git");
    expect(readSkillRecency(localStorage)).toEqual(["git", "weather"]);
  });

  it("forgets the long tail rather than growing without limit", () => {
    for (let i = 0; i < 20; i++) noteSkillUse(localStorage, `skill-${i}`);
    const remembered = readSkillRecency(localStorage);
    expect(remembered).toHaveLength(12);
    expect(remembered[0]).toBe("skill-19");
    expect(remembered).not.toContain("skill-0");
  });

  it("survives junk under the key rather than taking the menu down", () => {
    // Another tab, an older build, or anyone with devtools open.
    localStorage.setItem(STORAGE_KEYS.skillRecency, "{not json");
    expect(readSkillRecency(localStorage)).toEqual([]);

    localStorage.setItem(STORAGE_KEYS.skillRecency, JSON.stringify({ git: 1 }));
    expect(readSkillRecency(localStorage)).toEqual([]);

    localStorage.setItem(STORAGE_KEYS.skillRecency, JSON.stringify(["git", 7, null]));
    expect(readSkillRecency(localStorage)).toEqual(["git"]);
  });

  it("keeps working when the store refuses to be written", () => {
    const store = refusingStore();
    expect(() => noteSkillUse(store, "git")).not.toThrow();
    expect(noteSkillUse(store, "git")).toEqual(["git"]);
  });

  it("puts used skills first and leaves the rest in the order given", () => {
    const rows = [{ name: "alpha" }, { name: "bravo" }, { name: "charlie" }];
    const ordered = byRecency(rows, (r) => r.name, ["charlie"]);
    expect(ordered.map((r) => r.name)).toEqual(["charlie", "alpha", "bravo"]);
  });

  it("does not reorder anything when nothing has been used", () => {
    const rows = [{ name: "alpha" }, { name: "bravo" }];
    expect(byRecency(rows, (r) => r.name, []).map((r) => r.name)).toEqual(["alpha", "bravo"]);
  });
});
