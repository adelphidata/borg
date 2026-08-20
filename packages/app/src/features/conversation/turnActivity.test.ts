import { describe, expect, it } from "vitest";
import { createActivityHold, turnActivity } from "./turnActivity";
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
    expect(turnActivity([user()])).toEqual({ label: "Waiting for the model" });
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
    expect(turnActivity([user(), tool(true)])).toEqual({ label: "Working", tentative: true });
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
