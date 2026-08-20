import { describe, expect, it } from "vitest";
import { readSidebarCollapsed, writeSidebarCollapsed } from "./sidebarCollapsed";
import { STORAGE_KEYS } from "@borg/ui";

/** A store that refuses every operation, as a full or blocked one does. */
const brokenStore = {
  getItem() {
    throw new Error("access denied");
  },
  setItem() {
    throw new Error("quota exceeded");
  },
} as unknown as Storage;

function store(initial?: string): Storage {
  const map = new Map<string, string>();
  if (initial !== undefined) map.set(STORAGE_KEYS.sidebarCollapsed, initial);
  return {
    getItem: (k: string) => map.get(k) ?? null,
    setItem: (k: string, v: string) => void map.set(k, v),
  } as unknown as Storage;
}

describe("sidebar collapse preference", () => {
  it("round-trips the choice so a reload keeps it", () => {
    const s = store();
    writeSidebarCollapsed(s, true);
    expect(readSidebarCollapsed(s)).toBe(true);

    writeSidebarCollapsed(s, false);
    expect(readSidebarCollapsed(s)).toBe(false);
  });

  it("starts expanded on a first run", () => {
    expect(readSidebarCollapsed(store())).toBe(false);
  });

  it("expands rather than trusting a value it does not recognise", () => {
    // Another tab, an older build, or someone with devtools open. Anything but
    // our own "true" must not leave a person staring at a rail.
    expect(readSidebarCollapsed(store("yes"))).toBe(false);
    expect(readSidebarCollapsed(store(""))).toBe(false);
  });

  it("survives a store that refuses to be read or written", () => {
    expect(readSidebarCollapsed(brokenStore)).toBe(false);
    // Losing the preference is acceptable; refusing to collapse is not.
    expect(() => writeSidebarCollapsed(brokenStore, true)).not.toThrow();
  });
});
