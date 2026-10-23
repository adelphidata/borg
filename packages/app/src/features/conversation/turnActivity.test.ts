import { describe, expect, it } from "vitest";
import {
  BUSY_VERBS,
  busyVerb,
  createActivityHold,
  turnActivity,
  visibleLabel,
} from "./turnActivity";
import type { Cell } from "./useConversation";

const user = (text = "go"): Cell => ({ kind: "user", text });
const assistant = (text = "here"): Cell => ({ kind: "assistant", text });
const tool = (done: boolean, name = "run_shell"): Cell => ({
  kind: "tool",
  id: "t1",
  name,
  args: "{}",
  output: "",
  done,
  stderr: false,
});
const thinking = (streaming: boolean): Cell => ({
  kind: "thinking",
  text: "hm",
  streaming,
  startedAt: 0,
});
const prompt = (answered: boolean): Cell => ({
  kind: "shell-confirm",
  promptId: "p1",
  command: "ls",
  answered,
});

describe("turnActivity", () => {
  it("names the provider wait before anything comes back", () => {
    // The long one, and the one users misread as Borg being slow.
    expect(turnActivity([user()])).toEqual({ label: "Waiting for the model", generic: true });
  });

  it("says nothing while a tool row is running, because that row narrates", () => {
    expect(turnActivity([user(), tool(false)]).label).toBeUndefined();
  });

  it("says nothing while a reasoning block streams, because its header narrates", () => {
    expect(turnActivity([user(), thinking(true)]).label).toBeUndefined();
  });

  it("defers to an open prompt — the turn is waiting on the user, not working", () => {
    expect(turnActivity([user(), prompt(false)]).label).toBeUndefined();
  });

  it("resumes once that prompt is answered", () => {
    expect(turnActivity([user(), prompt(true)])).toEqual({
      label: "Working",
      tentative: true,
      generic: true,
    });
  });

  it("reports the reply while text is arriving", () => {
    expect(turnActivity([user(), assistant()])).toEqual({ label: "Replying" });
  });

  it("still defers to a running tool when a steer lands under it", () => {
    // Steering does not stop the shell command; claiming the turn is back to
    // waiting on the model would name the wrong wait.
    expect(turnActivity([user(), tool(false), user("actually, stop")]).label).toBeUndefined();
  });

  it("only guesses in the gap between one tool and the next", () => {
    // Marked tentative so the hold can keep it off screen when it is a blink.
    expect(turnActivity([user(), tool(true)])).toEqual({
      label: "Working",
      tentative: true,
      generic: true,
    });
  });

  it("names the provider outage while the retry notice is the live state", () => {
    const retrying: Cell = { kind: "notice", text: "Retrying in 4s.", reconnecting: true };
    expect(turnActivity([user(), assistant(), retrying])).toEqual({
      label: "Provider unreachable, retrying…",
    });
  });

  it("reads an ordinary notice as an ordinary gap", () => {
    const notice: Cell = { kind: "notice", text: "Posture: reduced" };
    expect(turnActivity([user(), notice])).toEqual({
      label: "Working",
      tentative: true,
      generic: true,
    });
  });
});

describe("createActivityHold", () => {
  it("shows a confident label the moment it arrives", () => {
    const hold = createActivityHold(2_000);

    expect(hold.settle({ label: "Waiting for the model" }, 0)).toEqual({
      label: "Waiting for the model",
    });
  });

  it("keeps the last confident label through a short gap between tools", () => {
    const hold = createActivityHold(2_000);
    hold.settle({ label: "Replying" }, 0);

    // The gap opens and closes inside the hold, so the line never flickers.
    expect(hold.settle({ label: "Working", tentative: true }, 500)).toEqual({ label: "Replying" });
    expect(hold.settle({ label: "Replying" }, 900)).toEqual({ label: "Replying" });
  });

  it("shows the guess once the gap has lasted long enough to be real", () => {
    const hold = createActivityHold(2_000);
    hold.settle({ label: "Replying" }, 0);

    expect(hold.settle({ label: "Working", tentative: true }, 1_000)).toEqual({
      label: "Replying",
    });
    expect(hold.settle({ label: "Working", tentative: true }, 3_100)).toEqual({
      label: "Working",
      tentative: true,
    });
  });

  it("clears the line when the tail starts narrating for itself", () => {
    const hold = createActivityHold(2_000);
    hold.settle({ label: "Waiting for the model" }, 0);

    // A stale "Waiting for the model" sitting above a live tool row would be
    // describing a wait that already ended.
    expect(hold.settle({}, 100)).toBeUndefined();
  });

  it("restarts the hold after a reset, so a new turn cannot inherit the old line", () => {
    const hold = createActivityHold(2_000);
    hold.settle({ label: "Replying" }, 0);
    hold.reset();

    expect(hold.settle({ label: "Working", tentative: true }, 10_000)).toBeUndefined();
    expect(hold.settle({ label: "Working", tentative: true }, 12_100)).toEqual({
      label: "Working",
      tentative: true,
    });
  });
});

describe("busyVerb", () => {
  it("keeps one verb for the whole of a turn", () => {
    // Re-renders read the same start time, so the line cannot swap words
    // halfway through a wait.
    expect(busyVerb(1_729_700_000_123)).toBe(busyVerb(1_729_700_000_123));
  });

  it("moves on between turns", () => {
    const verbs = new Set(Array.from({ length: 10 }, (_, i) => busyVerb(1_729_700_000_000 + i)));
    expect(verbs.size).toBeGreaterThan(1);
  });

  it("can reach every verb in the list", () => {
    const seen = new Set(BUSY_VERBS.map((_, i) => busyVerb(i)));
    expect(seen.size).toBe(BUSY_VERBS.length);
  });

  it("has a verb before the turn has a start time", () => {
    expect(BUSY_VERBS).toContain(busyVerb(undefined));
  });
});

describe("visibleLabel", () => {
  it("shows the turn's verb for a wait that says only 'busy'", () => {
    expect(visibleLabel({ label: "Waiting for the model", generic: true }, "Pondering")).toBe(
      "Pondering…",
    );
    expect(visibleLabel({ label: "Working", tentative: true, generic: true }, "Percolating")).toBe(
      "Percolating…",
    );
  });

  it("keeps a label that says something specific", () => {
    // Text arriving, or a provider outage: a whimsical verb would hide news.
    expect(visibleLabel({ label: "Replying" }, "Pondering")).toBe("Replying");
    expect(visibleLabel({ label: "Provider unreachable, retrying…" }, "Pondering")).toBe(
      "Provider unreachable, retrying…",
    );
  });

  it("shows nothing when the tail narrates for itself", () => {
    expect(visibleLabel({}, "Pondering")).toBeUndefined();
  });
});
