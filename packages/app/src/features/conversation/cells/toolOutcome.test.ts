import { describe, expect, it } from "vitest";
import { toolFailed, toolOutcome, toolStopped } from "./toolOutcome";

const cell = (output: string, over: { done?: boolean; stderr?: boolean } = {}) => ({
  output,
  done: over.done ?? true,
  stderr: over.stderr ?? false,
});

describe("toolOutcome", () => {
  it.each([
    // The daemon's own result wording, live and reloaded alike.
    ["a clean exit", cell("Exit code: 0\n--- stdout ---\nok\n--- stderr ---\n"), "ok"],
    [
      "a clean exit that wrote to stderr",
      cell("Exit code: 0\n--- stdout ---\n--- stderr ---\nwarning", { stderr: true }),
      "ok",
    ],
    [
      "a failing exit",
      cell("Exit code: 1\n--- stdout ---\n--- stderr ---\nno such file"),
      "failed",
    ],
    ["a signal exit", cell("Exit code: -1\n--- stdout ---\n--- stderr ---\n"), "failed"],
    ["a tty exit", cell("Exit code: 2\n--- output (tty) ---\nboom"), "failed"],
    [
      "a denied command",
      cell("Shell command denied: rm -rf / matches a blocked pattern"),
      "failed",
    ],
    ["a call the loop refused", cell("Error: Invalid JSON arguments: expected value"), "failed"],
    ["a timed-out call", cell("Error: tool 'web_fetch' timed out after 30s"), "failed"],
    // What `sanitize_tool_error` makes of an error a handler returned.
    ["a handler error", cell("[TOOL_ERROR] file not found: /tmp/x.txt"), "failed"],
    ["a crashed worker", cell("[tool call failed — worker panicked]"), "failed"],
    ["a result that quotes a tool error", cell("log: [TOOL_ERROR] earlier"), "ok"],
    ["another tool's stderr, with no status to go by", cell("partial", { stderr: true }), "failed"],
    ["another tool's ordinary result", cell('{"entries":[]}'), "ok"],
    ["a result that mentions an error further in", cell("Read 3 lines.\nError: none"), "ok"],
    ["a cancelled call", cell("[tool call cancelled by user]"), "stopped"],
    [
      "a cancelled call with trailing whitespace",
      cell("[tool call cancelled by user]\n"),
      "stopped",
    ],
    [
      "a call still running, whatever it has printed",
      cell("Exit code: 1", { done: false, stderr: true }),
      "running",
    ],
  ])("reads %s", (_, input, outcome) => {
    expect(toolOutcome(input)).toBe(outcome);
    expect(toolFailed(input)).toBe(outcome === "failed");
    expect(toolStopped(input)).toBe(outcome === "stopped");
  });
});
