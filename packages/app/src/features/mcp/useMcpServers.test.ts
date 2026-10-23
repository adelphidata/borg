import { describe, expect, it } from "vitest";
import {
  formatKeyValues,
  joinCommandLine,
  parseKeyValues,
  splitCommandLine,
  stateTone,
  stateWords,
} from "./useMcpServers";

describe("splitCommandLine", () => {
  it("splits on whitespace and keeps quoted arguments whole", () => {
    expect(splitCommandLine('npx -y "@scope/pkg" --root "/My Files"')).toEqual([
      "npx",
      "-y",
      "@scope/pkg",
      "--root",
      "/My Files",
    ]);
    expect(splitCommandLine("  uvx   mcp-server-time ")).toEqual(["uvx", "mcp-server-time"]);
    expect(splitCommandLine("")).toEqual([]);
  });

  it("honours single quotes, escapes, and empty quoted arguments", () => {
    expect(splitCommandLine(`echo 'a "b"' c\\ d ""`)).toEqual(["echo", 'a "b"', "c d", ""]);
    // An escaped quote inside double quotes stays in the argument.
    expect(splitCommandLine(`x "say \\"hi\\""`)).toEqual(["x", 'say "hi"']);
  });

  it("round-trips through joinCommandLine", () => {
    const argv = ["npx", "-y", "@scope/pkg", "--root", "/My Files", "", 'q"uote'];
    expect(splitCommandLine(joinCommandLine(argv))).toEqual(argv);
  });
});

describe("parseKeyValues", () => {
  it("reads KEY=value lines, skipping blanks and comments", () => {
    expect(parseKeyValues("A=1\n\n# note\nB = two=parts\n")).toEqual({
      map: { A: "1", B: "two=parts" },
    });
  });

  it("names the first line that is not KEY=value", () => {
    expect(parseKeyValues("A=1\nnot a pair\nB=2").bad).toBe("not a pair");
    expect(parseKeyValues("=novalue").bad).toBe("=novalue");
  });

  it("round-trips through formatKeyValues", () => {
    const map = { A: "1", B: "x=y" };
    expect(parseKeyValues(formatKeyValues(map)).map).toEqual(map);
  });
});

/** Every lifecycle label the daemon documents on `McpServerEntry.state`. */
const DAEMON_STATES = [
  "connected",
  "starting",
  "reconnecting",
  "auth-failed",
  "degraded",
  "not-started",
  "disabled",
  "live-disabled",
];

describe("state vocabulary", () => {
  it("has words and a dot tone for every label the daemon can send", () => {
    for (const state of DAEMON_STATES) {
      // A label with no words would show up raw in the list.
      expect(stateWords(state), state).not.toBe(state);
      expect(["ok", "alert", "busy", "idle"]).toContain(stateTone({ state }));
    }
    // Only a healthy server lights the dot; anything still moving pulses,
    // anything broken is red, and off or not running is grey.
    expect(DAEMON_STATES.filter((s) => stateTone({ state: s }) === "ok")).toEqual(["connected"]);
    expect(DAEMON_STATES.filter((s) => stateTone({ state: s }) === "alert")).toEqual([
      "auth-failed",
      "degraded",
    ]);
    // An unknown label is shown as the daemon sent it rather than hidden.
    expect(stateWords("something-new")).toBe("something-new");
    expect(stateTone({ state: "something-new" })).toBe("idle");
  });
});
