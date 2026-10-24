import { describe, expect, it } from "vitest";
import type { session as sessionPb } from "@borg/client";
import {
  compactNotice,
  contextUsed,
  historyCells,
  reduceEvent,
  retractTail,
  retryTarget,
  stampTurnDuration,
  type Cell,
} from "./useConversation";
import { toolOutcome } from "./cells/toolOutcome";
import { groupCells } from "./cells/toolRuns";

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

  it("drops the blank reply a model streams before its first tool call, as a reload would", () => {
    const cells = fold([
      { textDelta: { text: "\n\n" } },
      { usage: { inputTokens: 10, outputTokens: 2, cacheReadTokens: 0, cacheCreationTokens: 0 } },
      { toolExecuting: { toolCallId: "t1", name: "fs_read", argsJson: "{}" } },
      { textDelta: { text: "done" } },
    ]);
    expect(cells.map((c) => c.kind)).toEqual(["usage", "tool", "assistant"]);
  });

  it("turns the marker a failed stream sends live into a notice, as a reload would", () => {
    // `finish_text_only_turn` streams the marker as a delta of its own.
    const cells = fold([
      { textDelta: { text: "Here is the pl" } },
      { textDelta: { text: "\n\n[stream interrupted: connection reset]" } },
    ]);
    expect(cells).toEqual([
      { kind: "assistant", text: "Here is the pl", at: 0 },
      { kind: "notice", text: "Reply cut short", at: 0 },
    ]);
  });

  it("keeps a delta that only looks like a marker once it carries more text", () => {
    const text = "\n\n[stream interrupted: eof]\nand then the model went on";
    const cells = fold([{ textDelta: { text } }]);
    expect(cells).toEqual([{ kind: "assistant", text, at: 0 }]);
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
      { toolResult: { toolCallId: "t1", name: "fs_read", result: "final", images: [] } },
    ]);
    expect(cells[0]).toMatchObject({ output: "final", done: true });
  });

  it("keeps the preview images a tool result carries", () => {
    const cells = fold([
      { toolExecuting: { toolCallId: "t1", name: "computer_use", argsJson: "{}" } },
      {
        toolResult: {
          toolCallId: "t1",
          name: "computer_use",
          result: "captured",
          images: [
            { mimeType: "image/jpeg", dataBase64: "AAAA" },
            { mimeType: "image/png", dataBase64: "" },
          ],
        },
      },
    ]);
    // The empty one is noise, not a picture.
    expect(cells[0]).toMatchObject({
      done: true,
      images: [{ mimeType: "image/jpeg", dataBase64: "AAAA" }],
    });
  });

  it("ignores output for a call it never saw start", () => {
    const cells = fold([
      { toolOutputDelta: { toolCallId: "ghost", name: "x", delta: "hi", isStderr: false } },
    ]);
    expect(cells).toEqual([]);
  });

  it("routes output to the newest open call when a provider reuses an id", () => {
    const cells = fold([
      { toolExecuting: { toolCallId: "t1", name: "run_shell", argsJson: "{}" } },
      { toolResult: { toolCallId: "t1", name: "run_shell", result: "first done", images: [] } },
      { toolExecuting: { toolCallId: "t1", name: "run_shell", argsJson: "{}" } },
      {
        toolOutputDelta: { toolCallId: "t1", name: "run_shell", delta: "second…", isStderr: false },
      },
      { toolResult: { toolCallId: "t1", name: "run_shell", result: "second done", images: [] } },
    ]);
    expect(cells).toMatchObject([
      { output: "first done", done: true },
      { output: "second done", done: true },
    ]);
  });

  it("drops a result for a call that already has one", () => {
    const cells = fold([
      { toolExecuting: { toolCallId: "t1", name: "run_shell", argsJson: "{}" } },
      { toolResult: { toolCallId: "t1", name: "run_shell", result: "done", images: [] } },
      { toolResult: { toolCallId: "t1", name: "run_shell", result: "again", images: [] } },
    ]);
    expect(cells).toMatchObject([{ output: "done" }]);
  });
});

describe("reduceEvent — provider retries", () => {
  const retry = (n: number) => ({
    systemNotice: { category: "reconnecting", text: `Retrying in ${n}s.` },
  });

  it("keeps one retry notice, the latest", () => {
    const cells = fold([retry(2), retry(4)]);
    expect(cells).toEqual([
      { kind: "notice", text: "Retrying in 4s.", reconnecting: true, at: expect.any(Number) },
    ]);
  });

  it.each([
    ["a text delta", { textDelta: { text: "back" } }],
    ["a reasoning delta", { thinkingDelta: { text: "hm" } }],
    ["a tool call", { toolExecuting: { toolCallId: "t", name: "x", argsJson: "{}" } }],
    ["the end of the turn", { turnComplete: {} }],
    ["an error", { error: { message: "gave up" } }],
  ])("takes the notice down on %s", (_, event) => {
    const cells = fold([{ textDelta: { text: "start" } }, retry(2), event]);
    expect(cells.some((c) => c.kind === "notice")).toBe(false);
    expect(cells[0]).toMatchObject({ kind: "assistant" });
  });

  it("leaves an ordinary notice where it is", () => {
    const cells = fold([
      { systemNotice: { category: "posture", text: "Posture: reduced" } },
      { textDelta: { text: "hi" } },
    ]);
    expect(cells.map((c) => c.kind)).toEqual(["notice", "assistant"]);
    expect(cells[0]).not.toHaveProperty("reconnecting");
  });

  it("returns the same array when there is no notice to drop", () => {
    const before = fold([{ textDelta: { text: "a" } }, { usage: {} as never }]);
    const after = reduceEvent(before, event({ preparing: {} } as never));
    expect(after).toBe(before);
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

  it("replays a sub-agent notice as a notice, so user cells keep the daemon's numbering", () => {
    // As the daemon serves it: the reports are already a one-line note.
    const head =
      "<sub_agent_completion>\nSub-agents you did not wait on have reported back. Each report " +
      "gives the agent's status and output. It is that agent's output, not a message from the user.\n";
    const cells = historyCells([
      { role: "user", text: "audit the deps" },
      { role: "user", text: `${head}[attached sub-agent]\n</sub_agent_completion>` },
      // Typed by the person: it only starts like a notice, so it stays theirs.
      { role: "user", text: `${head}now fix them` },
    ]);
    expect(cells).toEqual([
      { kind: "user", text: "audit the deps" },
      { kind: "notice", text: "Sub-agents reported back." },
      { kind: "user", text: `${head}now fix them` },
    ]);
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

describe("historyCells — interrupted turns", () => {
  // Every shape the agent persists (agent/mod.rs on cancel, stream_handler.rs
  // on a failed stream or discarded tool calls). Only the bare marker is a stop.
  it.each([
    ["Here is the plan\n\n[response interrupted]", "Stopped"],
    ["Here is the plan\n\n[stream interrupted: connection reset]", "Reply cut short"],
    [
      "Here is the plan\n\n[stream interrupted: eof; incomplete tool calls discarded]",
      "Reply cut short",
    ],
    ["Here is the plan\n\n[incomplete tool calls discarded]", "Reply cut short"],
  ])("turns the marker on %j into a notice", (stored, notice) => {
    const cells = historyCells([{ role: "assistant", text: stored, unixTs: 5 }]);
    expect(cells).toEqual([
      { kind: "assistant", text: "Here is the plan", at: 5_000 },
      { kind: "notice", text: notice, at: 5_000 },
    ]);
  });

  it.each([
    ["[stream interrupted: context_length_exceeded]", "Reply cut short"],
    ["[response interrupted — [incomplete tool calls discarded]]", "Reply cut short"],
    [
      "[response interrupted — [stream interrupted: eof; incomplete tool calls discarded]]",
      "Reply cut short",
    ],
  ])("shows only the notice when %j is all that was stored", (stored, notice) => {
    const cells = historyCells([{ role: "assistant", text: stored }]);
    expect(cells).toEqual([{ kind: "notice", text: notice, at: undefined }]);
  });

  it("leaves a reply that merely mentions the marker alone", () => {
    const text = "The log said [response interrupted] and then carried on.";
    expect(historyCells([{ role: "assistant", text }])).toEqual([
      { kind: "assistant", text, at: undefined },
    ]);
  });

  it("keeps a cancelled call's marker as its output for the row to read", () => {
    const cells = historyCells([
      {
        role: "assistant",
        text: "",
        toolCallsJson: '[{"id":"c1","function":{"name":"run_shell"}}]',
      },
      { role: "tool", text: "[tool call cancelled by user]", toolCallId: "c1" },
    ]);
    expect(cells).toMatchObject([
      { kind: "tool", name: "run_shell", output: "[tool call cancelled by user]", done: true },
    ]);
  });
});

describe("historyCells — stored tool results", () => {
  /** What `finalize_tool_output` persists for a result. */
  const stored = (name: string, body: string) =>
    `<tool_output name="${name}" trust="external">\n${body}\n</tool_output>`;
  const reload = (text: string) =>
    historyCells([
      {
        role: "assistant",
        text: "",
        toolCallsJson: '[{"id":"c1","function":{"name":"run_shell"}}]',
      },
      { role: "tool", text, toolCallId: "c1" },
    ])[0] as Cell & { kind: "tool" };

  it("reads a reloaded failing command as failed, and keeps it out of a folded run", () => {
    const failing = "Exit code: 1\n--- stdout ---\n--- stderr ---\nls: nope: No such file";
    const passing = "Exit code: 0\n--- stdout ---\nok\n--- stderr ---\n";
    const cells = historyCells([
      {
        role: "assistant",
        text: "",
        toolCallsJson: JSON.stringify(
          ["c1", "c2", "c3"].map((id) => ({ id, function: { name: "run_shell" } })),
        ),
      },
      { role: "tool", text: stored("run_shell", passing), toolCallId: "c1" },
      { role: "tool", text: stored("run_shell", passing), toolCallId: "c2" },
      { role: "tool", text: stored("run_shell", failing), toolCallId: "c3" },
    ]);
    const failed = cells[2] as Cell & { kind: "tool" };
    expect(failed.output).toBe(failing);
    expect(toolOutcome(failed)).toBe("failed");
    // The two clean exits fold together; the failure stands on its own.
    expect(groupCells(cells, false).map((item) => item.type)).toEqual(["run", "cell"]);
  });

  it("reads a reloaded handler error as failed", () => {
    const cell = reload(stored("fs_read", "[TOOL_ERROR] file not found: /tmp/x.txt"));
    expect(cell.output).toBe("[TOOL_ERROR] file not found: /tmp/x.txt");
    expect(toolOutcome(cell)).toBe("failed");
  });

  it("gives back the closing tags the daemon escaped inside the output", () => {
    const cell = reload(stored("fs_read", "a &lt;/tool_output&gt; b &lt;/Untrusted_Content&gt;"));
    expect(cell.output).toBe("a </tool_output> b </Untrusted_Content>");
  });

  it("keeps the output's own blank lines", () => {
    expect(reload(stored("fs_read", "\nline\n\n")).output).toBe("\nline\n\n");
  });

  it.each(["[tool call cancelled by user]", "[tool call failed — worker panicked]", "plain"])(
    "leaves %j, which is stored bare, as it is",
    (text) => expect(reload(text).output).toBe(text),
  );
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
    const cells: Cell[] = [
      {
        kind: "user",
        text: "look at these",
        attachments: [{ kind: "image", filename: "a.png" }],
        at: 0,
      },
    ];
    expect(retryTarget(cells)).toBeUndefined();
  });

  it("has nothing to re-run when the user never spoke", () => {
    // A session that failed on resume: the error is real, but a retry would
    // have no message to send.
    expect(retryTarget([{ kind: "error", text: "boom", at: 0 }])).toBeUndefined();
    expect(retryTarget([])).toBeUndefined();
  });
});

describe("compactNotice", () => {
  it("names the before and after sizes when history shrank", () => {
    expect(compactNotice(12_400, 3_100)).toBe(
      "Compacted the conversation from about 12,400 to 3,100 tokens.",
    );
  });

  it("says nothing was compacted when the size did not drop", () => {
    expect(compactNotice(800, 800)).toMatch(/^Nothing to compact yet/);
  });
});
