import { describe, expect, it } from "vitest";
import type { session as sessionPb } from "@borg/client";
import {
  NOTIFICATION_BODY_MAX,
  finishedBody,
  pendingPrompts,
  promptBody,
} from "./useTurnNotifications";
import type { Cell } from "./useConversation";

const shellConfirm = (answered: boolean): Cell => ({
  kind: "shell-confirm",
  promptId: "p1",
  command: "ls",
  answered,
});

const inputRequest = (answered: boolean, question?: string): Cell => ({
  kind: "input-request",
  promptId: "p2",
  questions: (question === undefined
    ? []
    : [
        { id: "q", header: "Pick", question, multiSelect: false, options: [] },
      ]) as sessionPb.UserInputQuestion[],
  answered,
});

const usage: Cell = { kind: "usage", input: 1, output: 1, cacheRead: 0, cacheWrite: 0 };

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

describe("finishedBody", () => {
  it.each<{ name: string; cells: Cell[]; want: string }>([
    {
      name: "quotes the first line of the reply",
      cells: [
        { kind: "user", text: "go" },
        { kind: "assistant", text: "Done — the parser is fixed.\n\nDetails follow." },
        usage,
      ],
      want: "Done — the parser is fixed.",
    },
    {
      name: "skips blank lines and the markup that opens a line",
      cells: [{ kind: "assistant", text: "\n\n## **Summary** of `work`\nmore" }],
      want: "Summary of work",
    },
    {
      name: "reads the last reply of the turn, not the first",
      cells: [
        { kind: "assistant", text: "Looking." },
        { kind: "tool", id: "t", name: "x", args: "", output: "", done: true, stderr: false },
        { kind: "assistant", text: "Found it." },
      ],
      want: "Found it.",
    },
    {
      name: "does not quote an older turn when this one said nothing",
      cells: [
        { kind: "assistant", text: "Yesterday's answer." },
        { kind: "user", text: "and now?" },
        { kind: "tool", id: "t", name: "x", args: "", output: "", done: true, stderr: false },
      ],
      want: "Finished.",
    },
    { name: "has a plain answer for an empty transcript", cells: [], want: "Finished." },
    {
      name: "names a failure it recognises, in plain words",
      cells: [
        { kind: "assistant", text: "Working on it." },
        { kind: "error", text: "HTTP 429 Too Many Requests" },
        usage,
      ],
      want: "Borg hit a problem: The model provider is throttling Borg",
    },
    {
      name: "keeps a failure it cannot name out of the banner",
      cells: [{ kind: "error", text: "E0499 frobnicator desync" }],
      want: "Borg hit a problem.",
    },
    {
      name: "reports the reply when the turn recovered from an earlier error",
      cells: [
        { kind: "error", text: "HTTP 429 Too Many Requests" },
        { kind: "assistant", text: "Second try worked." },
      ],
      want: "Second try worked.",
    },
  ])("$name", ({ cells, want }) => {
    expect(finishedBody(cells)).toBe(want);
  });

  it("cuts a long first line short, and marks the cut", () => {
    const body = finishedBody([{ kind: "assistant", text: "word ".repeat(100) }]);
    expect(body.length).toBeLessThanOrEqual(NOTIFICATION_BODY_MAX);
    expect(body.endsWith("…")).toBe(true);
    expect(body.startsWith("word word")).toBe(true);
  });
});

describe("promptBody", () => {
  it.each<{ name: string; cells: Cell[]; want: string }>([
    {
      name: "names the command",
      cells: [{ kind: "shell-confirm", promptId: "p", command: "rm -r build", answered: false }],
      want: "Borg wants to run: rm -r build",
    },
    {
      name: "names only the first line of a script",
      cells: [{ kind: "shell-confirm", promptId: "p", command: "cd app\nmake", answered: false }],
      want: "Borg wants to run: cd app",
    },
    {
      name: "asks the question itself",
      cells: [inputRequest(false, "Which database should I use?")],
      want: "Which database should I use?",
    },
    {
      name: "reads the newest prompt still open, not an answered one",
      cells: [shellConfirm(false), inputRequest(true, "Old question?")],
      want: "Borg wants to run: ls",
    },
    {
      name: "falls back when the prompt carries no words",
      cells: [inputRequest(false)],
      want: "Borg is waiting on an answer.",
    },
  ])("$name", ({ cells, want }) => {
    expect(promptBody(cells)).toBe(want);
  });
});
