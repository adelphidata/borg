import { describe, expect, it } from "vitest";
import type { session as sessionPb } from "@borg/client";
import {
  contextUsed,
  historyCells,
  reduceEvent,
  retractTail,
  retryTarget,
  stampTurnDuration,
  type Cell,
} from "./useConversation";

/** Builds an AgentEvent with just the variant under test set. */
function event(kind: Partial<sessionPb.AgentEvent>): sessionPb.AgentEvent {
  return { eventSeq: 0, ...kind } as sessionPb.AgentEvent;
}

const fold = (events: Partial<sessionPb.AgentEvent>[], now = 0): Cell[] =>
  events.reduce<Cell[]>((cells, e) => reduceEvent(cells, event(e), now), []);

describe("reduceEvent — text", () => {
  it("merges consecutive text deltas into one assistant cell", () => {
    const cells = fold([{ textDelta: { text: "Hel" } }, { textDelta: { text: "lo" } }]);
    // The row is stamped once, at the delta that opened it — a reply does not
    // restart its clock every time another token lands.
    expect(cells).toEqual([{ kind: "assistant", text: "Hello", at: 0 }]);
  });

  it("starts a fresh assistant cell after a tool call splits the reply", () => {
    const cells = fold([
      { textDelta: { text: "one" } },
      { toolExecuting: { toolCallId: "t1", name: "fs_read", argsJson: "{}" } },
      { textDelta: { text: "two" } },
    ]);
    expect(cells.filter((c) => c.kind === "assistant")).toHaveLength(2);
  });
});

describe("reduceEvent — a retracted attempt", () => {
  it("keeps only the text of the attempt that completes", () => {
    const cells = fold([
      { textDelta: { text: "The answer is fö" } },
      { textRetracted: { bytes: 17 } },
      { textDelta: { text: "The answer is " } },
      { textDelta: { text: "forty-two." } },
    ]);
    expect(cells).toEqual([{ kind: "assistant", text: "The answer is forty-two.", at: 0 }]);
  });

  it("keeps the text that came before the failed attempt", () => {
    const cells = fold([
      { textDelta: { text: "I will check. " } },
      { textDelta: { text: "It is fo" } },
      { textRetracted: { bytes: 8 } },
      { textDelta: { text: "It is forty-two." } },
    ]);
    expect(cells).toEqual([{ kind: "assistant", text: "I will check. It is forty-two.", at: 0 }]);
  });

  it("does not change a transcript with no assistant text in progress", () => {
    const tool = { toolExecuting: { toolCallId: "t1", name: "fs_read", argsJson: "{}" } };
    expect(fold([tool, { textRetracted: { bytes: 4 } }])).toEqual(fold([tool]));
  });

  it.each([
    ["ascii", "hello world", 5, "hello "],
    ["counts bytes, not UTF-16 units", "café 😀", 4, "café "],
    ["a cut inside a character moves down", "a😀", 2, "a"],
    ["a count larger than the text", "short", 500, ""],
    ["zero", "keep", 0, "keep"],
  ])("retractTail: %s", (_name, text, bytes, want) => {
    expect(retractTail(text, bytes)).toBe(want);
  });
});

describe("reduceEvent — thinking", () => {
  it("keeps a reasoning block open while its deltas arrive", () => {
    const cells = fold([{ thinkingDelta: { text: "a" } }, { thinkingDelta: { text: "b" } }]);
    expect(cells).toEqual([{ kind: "thinking", text: "ab", streaming: true, startedAt: 0, at: 0 }]);
  });

  it("seals the block and records its duration when the answer starts", () => {
    let cells = reduceEvent([], event({ thinkingDelta: { text: "mulling" } }), 1_000);
    cells = reduceEvent(cells, event({ textDelta: { text: "done" } }), 5_400);

    expect(cells[0]).toEqual({
      kind: "thinking",
      text: "mulling",
      streaming: false,
      startedAt: 1_000,
      seconds: 4,
      // Sealing records how long it ran; it does not move when the row began.
      at: 1_000,
    });
  });

  it("does not reopen a sealed block when reasoning resumes later", () => {
    let cells = reduceEvent([], event({ thinkingDelta: { text: "first" } }), 0);
    cells = reduceEvent(cells, event({ textDelta: { text: "answer" } }), 1_000);
    cells = reduceEvent(cells, event({ thinkingDelta: { text: "second" } }), 2_000);

    const thinking = cells.filter((c) => c.kind === "thinking");
    expect(thinking).toHaveLength(2);
    expect(thinking[1]).toMatchObject({ text: "second", streaming: true });
  });
});

describe("reduceEvent — tools", () => {
  it("keeps the arguments so the UI can show what was requested", () => {
    const cells = fold([
      { toolExecuting: { toolCallId: "t1", name: "run_shell", argsJson: '{"command":"ls"}' } },
    ]);
    expect(cells[0]).toMatchObject({ kind: "tool", name: "run_shell", args: '{"command":"ls"}' });
  });

  it("appends output deltas and latches the stderr flag", () => {
    const cells = fold([
      { toolExecuting: { toolCallId: "t1", name: "run_shell", argsJson: "{}" } },
      { toolOutputDelta: { toolCallId: "t1", name: "run_shell", delta: "oops", isStderr: true } },
      { toolOutputDelta: { toolCallId: "t1", name: "run_shell", delta: "!", isStderr: false } },
    ]);
    expect(cells[0]).toMatchObject({ output: "oops!", stderr: true, done: false });
  });

  it("marks the call done and replaces streamed output with the result", () => {
    const cells = fold([
      { toolExecuting: { toolCallId: "t1", name: "fs_read", argsJson: "{}" } },
      { toolOutputDelta: { toolCallId: "t1", name: "fs_read", delta: "partial", isStderr: false } },
      { toolResult: { toolCallId: "t1", name: "fs_read", result: "final" } },
    ]);
    expect(cells[0]).toMatchObject({ output: "final", done: true });
  });

  it("ignores output for a call it never saw start", () => {
    const cells = fold([
      { toolOutputDelta: { toolCallId: "ghost", name: "x", delta: "hi", isStderr: false } },
    ]);
    expect(cells).toEqual([]);
  });
});

describe("reduceEvent — usage", () => {
  it("sums repeated usage reports within a turn into one line", () => {
    const cells = fold([
      { usage: { inputTokens: 100, outputTokens: 10, cacheReadTokens: 5, cacheCreationTokens: 0 } },
      { usage: { inputTokens: 50, outputTokens: 20, cacheReadTokens: 0, cacheCreationTokens: 7 } },
    ]);
    expect(cells).toEqual([{ kind: "usage", input: 150, output: 30, cacheRead: 5, cacheWrite: 7 }]);
  });

  it("starts a new line once other content separates the reports", () => {
    const cells = fold([
      { usage: { inputTokens: 1, outputTokens: 1, cacheReadTokens: 0, cacheCreationTokens: 0 } },
      { textDelta: { text: "more" } },
      { usage: { inputTokens: 2, outputTokens: 2, cacheReadTokens: 0, cacheCreationTokens: 0 } },
    ]);
    expect(cells.filter((c) => c.kind === "usage")).toHaveLength(2);
  });
});

describe("stampTurnDuration", () => {
  const usage = (input: number): Cell => ({
    kind: "usage",
    input,
    output: 0,
    cacheRead: 0,
    cacheWrite: 0,
  });

  it("stamps the usage line this turn produced", () => {
    const cells: Cell[] = [
      { kind: "user", text: "hi" },
      { kind: "assistant", text: "hello" },
      usage(9),
    ];
    expect(stampTurnDuration(cells, 1_500)[2]).toEqual({ ...usage(9), elapsedMs: 1_500 });
  });

  it("leaves a previous turn's footer alone", () => {
    // The bug this guards: stamping the last usage line anywhere in the
    // transcript would re-time an old turn whenever a new one reported none.
    const cells: Cell[] = [
      { kind: "user", text: "first" },
      usage(1),
      { kind: "user", text: "second" },
      { kind: "assistant", text: "answer" },
    ];
    const stamped = stampTurnDuration(cells, 900);
    expect(stamped[1]).toEqual(usage(1));
    expect(stamped[4]).toEqual({ ...usage(0), elapsedMs: 900 });
  });

  it("adds no footer to a send that never came back", () => {
    // Time spent waiting for nothing is not worth a line of its own.
    const cells: Cell[] = [{ kind: "user", text: "hi" }];
    expect(stampTurnDuration(cells, 4_000)).toEqual(cells);
  });
});

describe("reduceEvent — sub-agents", () => {
  it("upserts by agent id instead of stacking a row per update", () => {
    const cells = fold([
      { subAgentUpdate: { agentId: "a1", nickname: "scout", status: "running" } },
      { subAgentUpdate: { agentId: "a1", nickname: "scout", status: "done" } },
      { subAgentUpdate: { agentId: "a2", nickname: "writer", status: "running" } },
    ]);
    expect(cells).toEqual([
      { kind: "sub-agent", agentId: "a1", nickname: "scout", status: "done" },
      { kind: "sub-agent", agentId: "a2", nickname: "writer", status: "running" },
    ]);
  });
});

describe("reduceEvent — pass-through kinds", () => {
  it("adds nothing for progress-only events", () => {
    const cells = fold([
      { preparing: {} },
      { turnComplete: {} },
      { planUpdated: { steps: [{ id: "1", text: "do it", status: "pending" }] } },
    ]);
    expect(cells).toEqual([]);
  });

  it("renders a steer as a user turn", () => {
    expect(fold([{ steerReceived: { text: "actually, stop" } }])).toEqual([
      { kind: "user", text: "actually, stop", at: 0 },
    ]);
  });

  it("seals an open reasoning block before an error", () => {
    let cells = reduceEvent([], event({ thinkingDelta: { text: "hm" } }), 0);
    cells = reduceEvent(cells, event({ error: { message: "boom" } }), 3_000);
    expect(cells[0]).toMatchObject({ streaming: false, seconds: 3 });
    expect(cells[1]).toEqual({ kind: "error", text: "boom", at: 3_000 });
  });
});

describe("transcript timestamps", () => {
  it("keeps a reply's own start time when a tool splits it into two rows", () => {
    let cells = reduceEvent([], event({ textDelta: { text: "one" } }), 1_000);
    cells = reduceEvent(
      cells,
      event({ toolExecuting: { toolCallId: "t1", name: "fs_read", argsJson: "{}" } }),
      2_000,
    );
    cells = reduceEvent(cells, event({ textDelta: { text: "two" } }), 3_000);

    // Three rows, three distinct times: the second half of the reply is not
    // backdated to when the first half started.
    expect(cells.map((c) => c.at)).toEqual([1_000, 2_000, 3_000]);
  });

  it("carries a persisted row's time across a reload, in milliseconds", () => {
    const cells = historyCells([
      { role: "user", text: "hello", unixTs: 1_755_000_000 },
      { role: "tool", text: "out", toolCallId: "c1", unixTs: 1_755_000_005 },
    ]);

    // The daemon counts seconds; every other time in this module is a
    // millisecond, and mixing them would date the row to 1970.
    expect(cells.map((c) => c.at)).toEqual([1_755_000_000_000, 1_755_000_005_000]);
  });

  it("leaves a row with no recorded time unstamped rather than dating it to 1970", () => {
    const cells = historyCells([
      { role: "user", text: "old row" },
      { role: "assistant", text: "older still", unixTs: 0 },
    ]);

    expect(cells.map((c) => c.at)).toEqual([undefined, undefined]);
  });
});

describe("historyCells", () => {
  it("names a replayed tool call by pairing it with the assistant row", () => {
    const cells = historyCells([
      { role: "user", text: "read the file" },
      {
        role: "assistant",
        text: "",
        toolCallsJson: JSON.stringify([
          { id: "call_1", function: { name: "fs_read", arguments: '{"path":"a.txt"}' } },
        ]),
      },
      { role: "tool", text: "file body", toolCallId: "call_1" },
    ]);

    expect(cells).toEqual([
      { kind: "user", text: "read the file" },
      {
        kind: "tool",
        id: "call_1",
        name: "fs_read",
        args: '{"path":"a.txt"}',
        output: "file body",
        done: true,
        stderr: false,
      },
    ]);
  });

  it("keeps an assistant row that has both prose and tool calls", () => {
    const cells = historyCells([
      {
        role: "assistant",
        text: "Let me look.",
        toolCallsJson: JSON.stringify([
          { id: "c1", function: { name: "fs_read", arguments: "{}" } },
        ]),
      },
      { role: "tool", text: "ok", toolCallId: "c1" },
    ]);
    expect(cells.map((c) => c.kind)).toEqual(["assistant", "tool"]);
  });

  it("still shows a tool result whose call it cannot resolve", () => {
    const cells = historyCells([{ role: "tool", text: "orphaned", toolCallId: "missing" }]);
    expect(cells[0]).toMatchObject({ kind: "tool", name: "tool", args: "", output: "orphaned" });
  });

  it("survives malformed tool_calls_json rather than losing the transcript", () => {
    const cells = historyCells([
      { role: "assistant", text: "hi", toolCallsJson: "{not json" },
      { role: "tool", text: "result", toolCallId: "c1" },
    ]);
    expect(cells.map((c) => c.kind)).toEqual(["assistant", "tool"]);
  });

  it("skips system rows, which double as compaction-checkpoint carriers", () => {
    const cells = historyCells([
      { role: "system", text: "a long compaction summary" },
      { role: "user", text: "hello" },
    ]);
    expect(cells).toEqual([{ kind: "user", text: "hello" }]);
  });

  it("drops empty rows but keeps their tool calls available for pairing", () => {
    const cells = historyCells([
      { role: "user", text: "   " },
      {
        role: "assistant",
        text: "",
        toolCallsJson: JSON.stringify([{ id: "c9", function: { name: "web_search" } }]),
      },
      { role: "tool", text: "hits", toolCallId: "c9" },
    ]);
    expect(cells).toEqual([
      {
        kind: "tool",
        id: "c9",
        name: "web_search",
        args: "",
        output: "hits",
        done: true,
        stderr: false,
      },
    ]);
  });
});

describe("contextUsed", () => {
  it("counts only the top-level components", () => {
    expect(
      contextUsed([
        { tokens: 1_000, detail: false },
        { tokens: 400, detail: true },
        { tokens: 600, detail: true },
        { tokens: 250, detail: false },
      ]),
    ).toBe(1_250);
  });

  it("sums the wire's bigint token counts", () => {
    expect(
      contextUsed([
        { tokens: 9_007_199n, detail: false },
        { tokens: 1n, detail: false },
      ]),
    ).toBe(9_007_200);
  });

  it("is zero when every component itemizes a parent", () => {
    expect(contextUsed([{ tokens: 500, detail: true }])).toBe(0);
  });
});

describe("retryTarget", () => {
  const user = (text: string): Cell => ({ kind: "user", text, at: 0 });

  it("re-runs the message when the send never reached the daemon", () => {
    // The whole retryable shape: the user's message is still the last thing in
    // the transcript, so nothing came back and nothing was persisted.
    const cells: Cell[] = [user("first"), { kind: "assistant", text: "ok", at: 1 }, user("second")];
    expect(retryTarget(cells)).toBe("second");
  });

  it("refuses once the daemon has answered, even to report the failure", () => {
    // The daemon writes a turn into session history as it runs it. Re-sending
    // the same text from here would put the message in that history twice — the
    // transcript would look clean while the model read it twice over. Undoing
    // that is a rewind, not this button.
    const cells: Cell[] = [user("do the thing"), { kind: "error", text: "429 rate limit", at: 2 }];
    expect(retryTarget(cells)).toBeUndefined();
  });

  it("refuses when the turn got as far as a partial reply", () => {
    const cells: Cell[] = [user("do the thing"), { kind: "assistant", text: "half a rep", at: 1 }];
    expect(retryTarget(cells)).toBeUndefined();
  });

  it("refuses to resend a message that carried attachments", () => {
    // The transcript keeps the count, never the bytes. A resend would carry no
    // files while the cell above went on claiming it sent two.
    const cells: Cell[] = [{ kind: "user", text: "look at these", attachments: 2, at: 0 }];
    expect(retryTarget(cells)).toBeUndefined();
  });

  it("has nothing to re-run when the user never spoke", () => {
    // A session that failed on resume: the error is real, but a retry would
    // have no message to send.
    expect(retryTarget([{ kind: "error", text: "boom", at: 0 }])).toBeUndefined();
    expect(retryTarget([])).toBeUndefined();
  });
});
