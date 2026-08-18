import { describe, expect, it } from "vitest";
import { awaitingInput, mergeQueued, retirePrompts } from "./queuedTurn";
import type { Cell } from "./useConversation";

const shellConfirm = (answered: boolean): Cell => ({
  kind: "shell-confirm",
  promptId: "p1",
  command: "rm build/",
  answered,
});
const inputRequest = (answered: boolean): Cell => ({
  kind: "input-request",
  promptId: "p2",
  questions: [],
  answered,
});
const transcript: Cell[] = [
  { kind: "user", text: "clean the build" },
  { kind: "assistant", text: "on it" },
];

describe("awaitingInput", () => {
  it("is true while a shell confirmation is unanswered", () => {
    expect(awaitingInput([...transcript, shellConfirm(false)])).toBe(true);
  });

  it("is true while a structured question is unanswered", () => {
    expect(awaitingInput([...transcript, inputRequest(false)])).toBe(true);
  });

  it("goes false once every prompt is answered", () => {
    expect(awaitingInput([...transcript, shellConfirm(true), inputRequest(true)])).toBe(false);
  });

  it("is false for a transcript with no prompts", () => {
    expect(awaitingInput(transcript)).toBe(false);
  });

  it("stays true while a second prompt in the same turn is open", () => {
    expect(awaitingInput([shellConfirm(true), inputRequest(false)])).toBe(true);
  });
});

describe("retirePrompts", () => {
  it("closes a prompt the turn ended without answering", () => {
    // The bug this guards: the daemon can end a turn with a confirmation still
    // open — it timed out, or it was answered from the TUI, which this surface
    // never hears about. The cell then kept `awaitingInput` true forever, so
    // every later message queued instead of steering the running turn.
    const stranded = [...transcript, shellConfirm(false), inputRequest(false)];

    expect(awaitingInput(retirePrompts(stranded))).toBe(false);
  });

  it("leaves everything else exactly as it was", () => {
    const retired = retirePrompts([...transcript, shellConfirm(false)]);

    expect(retired.slice(0, 2)).toEqual(transcript);
    expect(retired[2]).toEqual(shellConfirm(true));
  });

  it("hands back the same array when no prompt is open", () => {
    // Called on every completed turn, so a transcript with nothing to retire
    // must not churn a new array and re-render the whole conversation.
    const settled = [...transcript, shellConfirm(true)];

    expect(retirePrompts(settled)).toBe(settled);
  });
});

describe("mergeQueued", () => {
  it("joins two submissions with a newline", () => {
    expect(mergeQueued("first", "second")).toBe("first\nsecond");
  });

  it("keeps merging into one entry", () => {
    expect(mergeQueued(mergeQueued("a", "b"), "c")).toBe("a\nb\nc");
  });

  it("does not add a stray newline around an empty side", () => {
    expect(mergeQueued("", "only")).toBe("only");
    expect(mergeQueued("only", "")).toBe("only");
  });
});
