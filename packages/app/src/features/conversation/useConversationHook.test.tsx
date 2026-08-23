import { describe, expect, it, vi } from "vitest";
import { act, render, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { BorgError, GrpcStatus, type session as sessionPb } from "@borg/client";
import { PlatformProvider } from "../../lib/platform";
import { FAILED_PRECONDITION, fakeDaemon } from "../../test/harness";
import { useConversation, type Conversation } from "./useConversation";

const OPEN = {
  ListSessions: { sessions: [{ id: "agent-1" }] },
  Open: { sessionId: "live-1", lastEventSeq: 7 },
  GetState: {
    title: "Ship it",
    llmModel: "claude-opus-5",
    agentSessionId: "agent-1",
    turnInFlight: false,
  },
  GetSession: { messages: [] },
  GetContextBreakdown: { components: [], contextWindow: 200_000 },
};

/** The hook, exposed to the test through a ref rather than through the DOM. */
function mount(daemon: ReturnType<typeof fakeDaemon>, target?: string) {
  const seen: { current: Conversation } = { current: undefined as never };
  function Probe() {
    seen.current = useConversation(target);
    return null;
  }
  render(
    <MemoryRouter>
      <PlatformProvider
        platform={{ kind: "desktop", transport: daemon.transport, titleBarDrag: false }}
      >
        <Probe />
      </PlatformProvider>
    </MemoryRouter>,
  );
  return seen;
}

const ready = async (conv: { current: Conversation }) =>
  waitFor(() => expect(conv.current.sessionId).toBe("live-1"));

/** Push an event down the conversation's own stream. */
function emit(daemon: ReturnType<typeof fakeDaemon>, event: Partial<sessionPb.AgentEvent>) {
  const stream = daemon.streams.filter((s) => s.method === "Stream").at(-1);
  if (!stream) throw new Error("the conversation never subscribed");
  act(() => stream.handlers.onMessage({ eventSeq: 0, ...event } as never));
}

describe("useConversation opening", () => {
  it("resumes the most recent conversation when no target is named", async () => {
    const daemon = fakeDaemon(OPEN);
    const conv = mount(daemon);
    await ready(conv);
    expect(daemon.calls.find((c) => c.method === "Open")?.request).toEqual({ resumeId: "agent-1" });
    expect(conv.current.opened.data?.title).toBe("Ship it");
  });

  it("starts fresh when asked for a new conversation", async () => {
    const daemon = fakeDaemon(OPEN);
    const conv = mount(daemon, "new");
    await ready(conv);
    expect(daemon.calls.find((c) => c.method === "Open")?.request).toEqual({});
    // A brand-new conversation must not list sessions looking for one to resume.
    expect(daemon.calls.some((c) => c.method === "ListSessions")).toBe(false);
  });

  it("resumes the conversation it was pointed at", async () => {
    const daemon = fakeDaemon(OPEN);
    const conv = mount(daemon, "agent-9");
    await ready(conv);
    expect(daemon.calls.find((c) => c.method === "Open")?.request).toEqual({ resumeId: "agent-9" });
  });

  it("loads the stored history so a reload looks like the live conversation", async () => {
    const daemon = fakeDaemon({
      ...OPEN,
      GetSession: {
        messages: [
          { role: "user", text: "hello" },
          { role: "assistant", text: "hi there" },
        ],
      },
    });
    const conv = mount(daemon);
    await ready(conv);
    expect(conv.current.cells.map((c) => c.kind)).toEqual(["user", "assistant"]);
  });

  it("skips the history call for a conversation nobody has written to", async () => {
    const daemon = fakeDaemon({
      ...OPEN,
      GetState: { ...OPEN.GetState, agentSessionId: "" },
    });
    const conv = mount(daemon);
    await ready(conv);
    expect(daemon.calls.some((c) => c.method === "GetSession")).toBe(false);
    expect(conv.current.cells).toEqual([]);
  });

  it("opens busy when the daemon says a turn is already running", async () => {
    // A reload mid-turn must not offer a composer that looks idle.
    const daemon = fakeDaemon({ ...OPEN, GetState: { ...OPEN.GetState, turnInFlight: true } });
    const conv = mount(daemon);
    await ready(conv);
    expect(conv.current.busy).toBe(true);
  });

  it("reports a failed open rather than a blank transcript", async () => {
    const daemon = fakeDaemon({
      ...OPEN,
      Open: new BorgError(GrpcStatus.Unavailable, "daemon down"),
    });
    const conv = mount(daemon);
    await waitFor(() => expect(conv.current.opened.state).toBe("error"));
  });

  it("subscribes from the seq the open reported, so nothing replays", async () => {
    const daemon = fakeDaemon(OPEN);
    const conv = mount(daemon);
    await ready(conv);
    await waitFor(() => expect(daemon.streams.some((s) => s.method === "Stream")).toBe(true));
    expect(daemon.streams.find((s) => s.method === "Stream")?.request).toEqual({
      sessionId: "live-1",
      sinceEventSeq: 7,
    });
  });
});

describe("useConversation sending", () => {
  it("echoes the message, marks the turn busy and starts the clock", async () => {
    const daemon = fakeDaemon(OPEN);
    const conv = mount(daemon);
    await ready(conv);

    act(() => conv.current.send("do the thing"));
    expect(conv.current.cells.at(-1)).toMatchObject({ kind: "user", text: "do the thing" });
    expect(conv.current.busy).toBe(true);
    expect(conv.current.turnStartedAt).toBeGreaterThan(0);
    expect(daemon.streams.find((s) => s.method === "Send")?.request).toMatchObject({
      sessionId: "live-1",
      text: "do the thing",
    });
  });

  it("ignores a send before the session is open", () => {
    const daemon = fakeDaemon(OPEN);
    const conv = mount(daemon);
    act(() => conv.current.send("too early"));
    expect(daemon.streams.some((s) => s.method === "Send")).toBe(false);
  });

  it("clears busy and reports a send that failed", async () => {
    const daemon = fakeDaemon(OPEN);
    const conv = mount(daemon);
    await ready(conv);
    act(() => conv.current.send("do the thing"));

    const send = daemon.streams.find((s) => s.method === "Send")!;
    act(() => send.handlers.onError?.(new BorgError(GrpcStatus.Unavailable, "connection lost")));
    expect(conv.current.busy).toBe(false);
    expect(conv.current.sendError?.message).toBe("connection lost");
    expect(conv.current.turnStartedAt).toBeUndefined();
  });

  it("renders the turn from the stream, not from the send", async () => {
    // Both carry the same events; one consumer keeps the ordering simple.
    const daemon = fakeDaemon(OPEN);
    const conv = mount(daemon);
    await ready(conv);
    act(() => conv.current.send("hi"));

    emit(daemon, { textDelta: { text: "hel" } as never });
    emit(daemon, { textDelta: { text: "lo" } as never });
    expect(conv.current.cells.at(-1)).toMatchObject({ kind: "assistant", text: "hello" });
  });

  it("ends the turn and stamps how long it took", async () => {
    const daemon = fakeDaemon(OPEN);
    const conv = mount(daemon);
    await ready(conv);
    act(() => conv.current.send("hi"));

    emit(daemon, {
      usage: {
        inputTokens: 10,
        outputTokens: 5,
        cacheReadTokens: 0,
        cacheCreationTokens: 0,
      } as never,
    });
    emit(daemon, { turnComplete: {} as never });

    expect(conv.current.busy).toBe(false);
    expect(conv.current.turnStartedAt).toBeUndefined();
    const usage = conv.current.cells.find((c) => c.kind === "usage");
    expect(usage).toMatchObject({ input: 10, output: 5 });
    expect((usage as { elapsedMs?: number }).elapsedMs).toBeGreaterThanOrEqual(0);
  });

  it("starts the clock for a turn the daemon began on its own", async () => {
    // A resumed or scheduled turn has no submit to arm it.
    const daemon = fakeDaemon(OPEN);
    const conv = mount(daemon);
    await ready(conv);

    emit(daemon, { preparing: {} as never });
    expect(conv.current.busy).toBe(true);
    expect(conv.current.turnStartedAt).toBeGreaterThan(0);
  });

  it("keeps the plan out of the transcript", async () => {
    const daemon = fakeDaemon(OPEN);
    const conv = mount(daemon);
    await ready(conv);

    emit(daemon, {
      planUpdated: { steps: [{ id: "1", text: "write tests", status: "in_progress" }] } as never,
    });
    expect(conv.current.plan).toHaveLength(1);
    expect(conv.current.cells).toEqual([]);
  });
});

describe("useConversation retry", () => {
  it("re-runs the last message without printing it twice", async () => {
    const daemon = fakeDaemon(OPEN);
    const conv = mount(daemon);
    await ready(conv);
    act(() => conv.current.send("do the thing"));
    const send = daemon.streams.find((s) => s.method === "Send")!;
    act(() => send.handlers.onError?.(new BorgError(GrpcStatus.Unavailable, "lost")));

    act(() => conv.current.retryLast());
    expect(conv.current.cells.filter((c) => c.kind === "user")).toHaveLength(1);
    expect(daemon.streams.filter((s) => s.method === "Send")).toHaveLength(2);
  });

  it("refuses to retry once the daemon has answered", async () => {
    // The turn is already in the persisted history; resending would double it.
    const daemon = fakeDaemon(OPEN);
    const conv = mount(daemon);
    await ready(conv);
    act(() => conv.current.send("do the thing"));
    emit(daemon, { textDelta: { text: "half an answer" } as never });

    act(() => conv.current.retryLast());
    expect(daemon.streams.filter((s) => s.method === "Send")).toHaveLength(1);
  });

  it("refuses to retry mid-turn", async () => {
    const daemon = fakeDaemon(OPEN);
    const conv = mount(daemon);
    await ready(conv);
    act(() => conv.current.send("do the thing"));

    act(() => conv.current.retryLast());
    expect(daemon.streams.filter((s) => s.method === "Send")).toHaveLength(1);
  });
});

describe("useConversation side channels", () => {
  it("steers without echoing, so the daemon's own replay is the only copy", async () => {
    // A local echo would shift every later user cell's ordinal.
    const daemon = fakeDaemon({ ...OPEN, Steer: {} });
    const conv = mount(daemon);
    await ready(conv);
    act(() => conv.current.send("go"));
    act(() => conv.current.steer("actually, stop"));

    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "Steer")?.request).toEqual({
        sessionId: "live-1",
        text: "actually, stop",
      }),
    );
    expect(conv.current.cells.filter((c) => c.kind === "user")).toHaveLength(1);
  });

  it("retires open prompts when the turn is cancelled", async () => {
    // Allow/Deny pills wired to an abandoned prompt id are a dead end.
    const daemon = fakeDaemon({ ...OPEN, Cancel: {} });
    const conv = mount(daemon);
    await ready(conv);
    emit(daemon, {
      shellConfirmation: { promptId: "p1", command: "rm -rf build" } as never,
    });
    expect(conv.current.awaitingInput).toBe(true);

    act(() => conv.current.cancel());
    expect(conv.current.awaitingInput).toBe(false);
    await waitFor(() => expect(daemon.calls.some((c) => c.method === "Cancel")).toBe(true));
  });

  it("marks a prompt answered as soon as the answer is sent", async () => {
    const daemon = fakeDaemon({ ...OPEN, RespondToPrompt: {} });
    const conv = mount(daemon);
    await ready(conv);
    emit(daemon, { shellConfirmation: { promptId: "p1", command: "ls" } as never });

    act(() => conv.current.respond("p1", "allow"));
    expect(conv.current.cells.at(-1)).toMatchObject({ answered: true });
    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "RespondToPrompt")?.request).toEqual({
        sessionId: "live-1",
        promptId: "p1",
        value: "allow",
      }),
    );
  });

  it("retires prompts when the turn ends, so the composer stops queueing", async () => {
    const daemon = fakeDaemon(OPEN);
    const conv = mount(daemon);
    await ready(conv);
    emit(daemon, { shellConfirmation: { promptId: "p1", command: "ls" } as never });
    emit(daemon, { turnComplete: {} as never });
    expect(conv.current.awaitingInput).toBe(false);
  });

  it("surfaces a failed side-channel call in the transcript's error slot", async () => {
    const daemon = fakeDaemon({
      ...OPEN,
      Steer: new BorgError(FAILED_PRECONDITION, "no turn in flight"),
    });
    const conv = mount(daemon);
    await ready(conv);

    act(() => conv.current.steer("nudge"));
    await waitFor(() => expect(conv.current.sendError?.message).toBe("no turn in flight"));
  });
});

describe("useConversation edit and resend", () => {
  it("rewinds first, then resends — never the other way round", async () => {
    // Resending into a history the daemon never truncated duplicates the turn.
    const daemon = fakeDaemon({ ...OPEN, RewindToNthUserMessage: {} });
    const conv = mount(daemon);
    await ready(conv);
    act(() => conv.current.send("first"));
    emit(daemon, { turnComplete: {} as never });
    act(() => conv.current.send("second"));
    emit(daemon, { turnComplete: {} as never });

    await act(async () => {
      conv.current.editAndResend(0, "first, corrected");
    });

    await waitFor(() => expect(daemon.streams.filter((s) => s.method === "Send")).toHaveLength(3));
    expect(daemon.calls.find((c) => c.method === "RewindToNthUserMessage")?.request).toEqual({
      sessionId: "live-1",
      n: 0,
    });
    const users = conv.current.cells.filter((c) => c.kind === "user");
    expect(users).toHaveLength(1);
    expect(users[0]).toMatchObject({ text: "first, corrected" });
  });

  it("does not resend when the rewind failed", async () => {
    const daemon = fakeDaemon({
      ...OPEN,
      RewindToNthUserMessage: new BorgError(GrpcStatus.Internal, "cannot rewind"),
    });
    const conv = mount(daemon);
    await ready(conv);
    act(() => conv.current.send("first"));
    emit(daemon, { turnComplete: {} as never });

    await act(async () => {
      conv.current.editAndResend(0, "corrected");
    });
    await waitFor(() => expect(conv.current.sendError?.message).toBe("cannot rewind"));
    expect(daemon.streams.filter((s) => s.method === "Send")).toHaveLength(1);
  });
});

describe("useConversation context and branching", () => {
  it("measures the context once the turn settles", async () => {
    const daemon = fakeDaemon({
      ...OPEN,
      GetContextBreakdown: {
        components: [{ tokens: 40_000 }, { tokens: 10_000 }],
        contextWindow: 200_000,
      },
    });
    const conv = mount(daemon);
    await ready(conv);
    await waitFor(() => expect(conv.current.context).toEqual({ used: 50_000, window: 200_000 }));
  });

  it("ignores a measurement that belongs to the conversation just left", async () => {
    // One page instance serves every /c/:id, so a slow answer can outlive its
    // session.
    const daemon = fakeDaemon(OPEN);
    const conv = mount(daemon);
    await ready(conv);
    await waitFor(() => expect(conv.current.context).toBeDefined());
    expect(daemon.calls.find((c) => c.method === "GetContextBreakdown")?.request).toEqual({
      sessionId: "live-1",
    });
  });

  it("compacts and re-measures", async () => {
    const daemon = fakeDaemon({ ...OPEN, Compact: {} });
    const conv = mount(daemon);
    await ready(conv);
    const before = daemon.calls.filter((c) => c.method === "GetContextBreakdown").length;

    await act(async () => {
      await conv.current.compact();
    });
    await waitFor(() =>
      expect(daemon.calls.filter((c) => c.method === "GetContextBreakdown").length).toBeGreaterThan(
        before,
      ),
    );
  });

  it("forks and hands back the new conversation's id", async () => {
    const daemon = fakeDaemon({ ...OPEN, Fork: { forkedAgentSessionId: "agent-2" } });
    const conv = mount(daemon);
    await ready(conv);

    let forked: string | undefined;
    await act(async () => {
      forked = await conv.current.fork();
    });
    expect(forked).toBe("agent-2");
  });
});

describe("useConversation stream health", () => {
  it("says the connection ended rather than freezing the transcript", async () => {
    const daemon = fakeDaemon(OPEN);
    const conv = mount(daemon);
    await ready(conv);
    const stream = daemon.streams.filter((s) => s.method === "Stream").at(-1)!;

    act(() => stream.handlers.onError?.(new BorgError(GrpcStatus.Unavailable, "socket closed")));
    await waitFor(() => expect(conv.current.streamError?.message).toBe("socket closed"));
  });

  it("treats a clean close as an outage too", async () => {
    // A turn stream ends when the daemon goes away as readily as when it
    // errors, and the silent case is the worse one.
    const daemon = fakeDaemon(OPEN);
    const conv = mount(daemon);
    await ready(conv);
    const stream = daemon.streams.filter((s) => s.method === "Stream").at(-1)!;

    act(() => stream.handlers.onComplete?.());
    await waitFor(() =>
      expect(conv.current.streamError?.message).toBe("The connection to Borg ended."),
    );
  });

  it("clears the outage once events flow again", async () => {
    const daemon = fakeDaemon(OPEN);
    const conv = mount(daemon);
    await ready(conv);
    const stream = daemon.streams.filter((s) => s.method === "Stream").at(-1)!;
    act(() => stream.handlers.onError?.(new BorgError(GrpcStatus.Unavailable, "socket closed")));
    await waitFor(() => expect(conv.current.streamError).toBeDefined());

    emit(daemon, { textDelta: { text: "back" } as never });
    expect(conv.current.streamError).toBeUndefined();
  });
});
