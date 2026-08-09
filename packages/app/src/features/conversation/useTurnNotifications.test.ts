import { describe, expect, it } from "vitest";
import type { session as sessionPb } from "@borg/client";
import { pendingPrompts } from "./useTurnNotifications";
import type { Cell } from "./useConversation";

const shellConfirm = (answered: boolean): Cell => ({
  kind: "shell-confirm",
  promptId: "p1",
  command: "ls",
  answered,
});

const inputRequest = (answered: boolean): Cell => ({
  kind: "input-request",
  promptId: "p2",
  questions: [] as sessionPb.UserInputQuestion[],
  answered,
});

describe("pendingPrompts", () => {
  it("counts only the prompts still waiting on the user", () => {
    const cells: Cell[] = [
      { kind: "assistant", text: "hi" },
      shellConfirm(false),
      inputRequest(false),
      shellConfirm(true),
    ];
    expect(pendingPrompts(cells)).toBe(2);
  });

  it("is zero once every prompt is answered — this is what clears the badge", () => {
    expect(pendingPrompts([shellConfirm(true), inputRequest(true)])).toBe(0);
  });

  it("ignores ordinary transcript content", () => {
    const cells: Cell[] = [
      { kind: "user", text: "go" },
      { kind: "tool", id: "t", name: "x", args: "", output: "", done: true, stderr: false },
      { kind: "error", text: "boom" },
    ];
    expect(pendingPrompts(cells)).toBe(0);
  });
});
