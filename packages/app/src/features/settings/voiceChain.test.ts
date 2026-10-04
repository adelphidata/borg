import { describe, expect, it } from "vitest";
import { parseChain, primaryRow, withPrimaryFields, withPrimaryProvider } from "./voiceChain";

const CHAIN = JSON.stringify([
  { provider: "elevenlabs", voice: "adam", timeout_ms: 9000 },
  { provider: "edge" },
]);

describe("parseChain", () => {
  it("reads missing, malformed, and non-list values as no chain", () => {
    for (const raw of [undefined, "", "not json", "{}", '"x"']) {
      expect(parseChain(raw), String(raw)).toEqual([]);
    }
  });

  it("skips rows that name no provider", () => {
    expect(parseChain('[{"voice":"x"},{"provider":"edge"}]')).toEqual([{ provider: "edge" }]);
  });
});

describe("withPrimaryFields", () => {
  it("edits the first row and keeps its other fields and the fallbacks", () => {
    expect(JSON.parse(withPrimaryFields(CHAIN, { voice: "rachel" }))).toEqual([
      { provider: "elevenlabs", voice: "rachel", timeout_ms: 9000 },
      { provider: "edge" },
    ]);
  });

  it("clears a field given an empty string, so the provider default applies", () => {
    expect(primaryRow(withPrimaryFields(CHAIN, { voice: "" }))).toEqual({
      provider: "elevenlabs",
      timeout_ms: 9000,
    });
  });
});

describe("withPrimaryProvider", () => {
  it("starts a fresh first row on switch, keeping the fallbacks", () => {
    expect(JSON.parse(withPrimaryProvider(CHAIN, "openai"))).toEqual([
      { provider: "openai" },
      { provider: "edge" },
    ]);
  });

  it("promotes a fallback for the same provider instead of listing it twice", () => {
    expect(JSON.parse(withPrimaryProvider(CHAIN, "edge"))).toEqual([{ provider: "edge" }]);
  });

  it("leaves the chain alone when the provider already answers first", () => {
    expect(withPrimaryProvider(CHAIN, "elevenlabs")).toBe(JSON.stringify(JSON.parse(CHAIN)));
  });

  it("creates the chain when there was none", () => {
    expect(JSON.parse(withPrimaryProvider(undefined, "edge"))).toEqual([{ provider: "edge" }]);
  });
});
