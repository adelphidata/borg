import { describe, expect, it } from "vitest";
import { SERVICES, WEB_SERVICES } from "./services.js";

describe("SERVICES", () => {
  it("keys each service by a definition whose fullName is a borg proto", () => {
    for (const [name, definition] of Object.entries(SERVICES)) {
      expect(definition.fullName, name).toMatch(/^borg\.[a-z]+\.v1\./);
      expect(Object.keys(definition.methods).length, name).toBeGreaterThan(0);
    }
  });

  it("excludes the agent-to-agent capability channel", () => {
    const fullNames = Object.values(SERVICES).map((d) => d.fullName);
    expect(fullNames).not.toContain("borg.capability.v1.Capability");
  });
});

describe("WEB_SERVICES", () => {
  it("is a subset of SERVICES", () => {
    for (const name of WEB_SERVICES) {
      expect(Object.keys(SERVICES)).toContain(name);
    }
  });

  it("omits pki — pairing bootstraps over the socket only, never the browser", () => {
    expect(WEB_SERVICES).not.toContain("pki");
  });

  it("carries every other UI service, so the browser loses nothing else", () => {
    const everythingButPki = Object.keys(SERVICES)
      .filter((name) => name !== "pki")
      .sort();
    expect([...WEB_SERVICES].sort()).toEqual(everythingButPki);
  });
});
