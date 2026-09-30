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
    let accepted: boolean | undefined;
    await act(async () => {
      accepted = await conv.current.steer("actually, stop");
    });

    expect(daemon.calls.find((c) => c.method === "Steer")?.request).toEqual({
      sessionId: "live-1",
      text: "actually, stop",
    });
    expect(accepted).toBe(true);
    expect(conv.current.cells.filter((c) => c.kind === "user")).toHaveLength(1);
  });

  it("reports a failed steer as false, so the caller can keep the text", async () => {
    const daemon = fakeDaemon({
      ...OPEN,
      Steer: new BorgError(GrpcStatus.Unavailable, "daemon restarting"),
    });
    const conv = mount(daemon);
    await ready(conv);
    let accepted: boolean | undefined;
    await act(async () => {
      accepted = await conv.current.steer("nudge");
    });
    expect(accepted).toBe(false);
    expect(daemon.streams.some((s) => s.method === "Send")).toBe(false);
  });

  it("sends a steer the turn ended before taking as the next turn", async () => {
    // Regression: the daemon answered OK to a steer that landed after the
    // turn's last check, and the text was never seen again. It now refuses
    // it, and what the user typed goes out as a turn of its own.
    const daemon = fakeDaemon({
      ...OPEN,
      Steer: new BorgError(FAILED_PRECONDITION, "steer: no turn is running to take this message"),
    });
    const conv = mount(daemon);
    await ready(conv);
    let accepted: boolean | undefined;
    await act(async () => {
      accepted = await conv.current.steer("and the sources too");
    });

    expect(accepted).toBe(true);
    expect(daemon.streams.find((s) => s.method === "Send")?.request).toMatchObject({
      sessionId: "live-1",
      text: "and the sources too",
    });
    expect(conv.current.busy).toBe(true);
    expect(conv.current.cells.filter((c) => c.kind === "user")).toHaveLength(1);
    expect(conv.current.sendError).toBeUndefined();
  });

  it("is busy again when a steer keeps a finished turn going", async () => {
    // The last steer check runs after `turnComplete` is sent; a steer it
    // finds keeps the turn going, and the composer must steer, not send.
    const daemon = fakeDaemon({ ...OPEN, Steer: {} });
    const conv = mount(daemon);
    await ready(conv);
    act(() => conv.current.send("go"));
    emit(daemon, { turnComplete: {} as never });
    expect(conv.current.busy).toBe(false);

    emit(daemon, { steerReceived: { text: "one more thing" } as never });
    emit(daemon, { preparing: {} as never });
    expect(conv.current.busy).toBe(true);
  });

  it("stays idle when a stopped turn replays the steer it had queued", async () => {
    // A stop keeps what was queued in the history, echoed after the
    // `turnComplete`; no model call follows, so nothing would end a spinner.
    const daemon = fakeDaemon({ ...OPEN, Steer: {} });
    const conv = mount(daemon);
    await ready(conv);
    act(() => conv.current.send("go"));
    emit(daemon, { turnComplete: {} as never });
    emit(daemon, { steerReceived: { text: "and the sources" } as never });

    expect(conv.current.busy).toBe(false);
    expect(conv.current.turnStartedAt).toBeUndefined();
    expect(conv.current.cells.filter((c) => c.kind === "user").map((c) => c.text)).toEqual([
      "go",
      "and the sources",
    ]);
  });

  it("refuses a steer before the session is open, without calling the daemon", async () => {
    const daemon = fakeDaemon({ ...OPEN, Open: new Promise(() => {}) });
    const conv = mount(daemon);
    await waitFor(() => expect(conv.current.ready).toBe(false));
    let accepted: boolean | undefined;
    await act(async () => {
      accepted = await conv.current.steer("nudge");
    });
    expect(accepted).toBe(false);
    expect(daemon.calls.some((c) => c.method === "Steer")).toBe(false);
  });

  it("is ready once the session id is known, and not before", async () => {
    const daemon = fakeDaemon(OPEN);
    const conv = mount(daemon);
    expect(conv.current.ready).toBe(false);
    await ready(conv);
    expect(conv.current.ready).toBe(true);
  });

  it("says Stopped once when a turn the user cancelled ends", async () => {
    const daemon = fakeDaemon({ ...OPEN, Cancel: {} });
    const conv = mount(daemon);
    await ready(conv);
    act(() => conv.current.send("go"));
    emit(daemon, { textDelta: { text: "half a" } as never });
    await act(async () => conv.current.cancel());
    emit(daemon, { turnComplete: {} as never });
    emit(daemon, { turnComplete: {} as never });

    const notices = conv.current.cells.filter((c) => c.kind === "notice");
    expect(notices).toEqual([expect.objectContaining({ text: "Stopped" })]);
    // The next turn ends on its own and must not inherit the label.
    act(() => conv.current.send("again"));
    emit(daemon, { turnComplete: {} as never });
    expect(conv.current.cells.filter((c) => c.kind === "notice")).toHaveLength(1);
  });

  it("does not say Stopped for a cancel the daemon refused", async () => {
    const daemon = fakeDaemon({
      ...OPEN,
      Cancel: new BorgError(FAILED_PRECONDITION, "no turn in flight"),
    });
    const conv = mount(daemon);
    await ready(conv);
    act(() => conv.current.send("go"));
    await act(async () => conv.current.cancel());
    await waitFor(() => expect(daemon.calls.some((c) => c.method === "Cancel")).toBe(true));
    emit(daemon, { turnComplete: {} as never });
    expect(conv.current.cells.some((c) => c.kind === "notice")).toBe(false);
  });

  it("remembers what was attached by kind and name, never by count", async () => {
    const daemon = fakeDaemon({ ...OPEN, Send: {} });
    const conv = mount(daemon);
    await ready(conv);
    const bytes = new Uint8Array([1]);
    act(() =>
      conv.current.send("", [
        { kind: "image", mimeType: "image/png", data: bytes, filename: "a.png" },
        { kind: "document", mimeType: "text/plain", data: bytes, filename: "pasted-text.txt" },
      ]),
    );
    expect(conv.current.cells[0]).toMatchObject({
      kind: "user",
      attachments: [
        { kind: "image", filename: "a.png" },
        { kind: "document", filename: "pasted-text.txt" },
      ],
    });
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
      Steer: new BorgError(GrpcStatus.Internal, "steer channel broke"),
    });
    const conv = mount(daemon);
    await ready(conv);

    await act(async () => {
      await conv.current.steer("nudge");
    });
    await waitFor(() => expect(conv.current.sendError?.message).toBe("steer channel broke"));
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

describe("useConversation title", () => {
  it("re-reads the title when a turn ends, so a new conversation gets its name", async () => {
    const daemon = fakeDaemon({ ...OPEN, GetState: { ...OPEN.GetState, title: "" } });
    const conv = mount(daemon, "new");
    await ready(conv);
    expect(conv.current.opened.data?.title).toBe("");

    daemon.answer("GetState", { ...OPEN.GetState, title: "Ship it" });
    const before = daemon.calls.filter((c) => c.method === "GetState").length;
    emit(daemon, { turnComplete: {} as never });
    await waitFor(() => expect(conv.current.opened.data?.title).toBe("Ship it"));
    // One read per turn end, not one per event.
    expect(daemon.calls.filter((c) => c.method === "GetState")).toHaveLength(before + 1);
  });

  it("keeps the model and session id the open reported", async () => {
    const daemon = fakeDaemon(OPEN);
    const conv = mount(daemon);
    await ready(conv);
    daemon.answer("GetState", { ...OPEN.GetState, title: "Renamed" });
    emit(daemon, { turnComplete: {} as never });
    await waitFor(() => expect(conv.current.opened.data?.title).toBe("Renamed"));
    expect(conv.current.opened.data).toMatchObject({
      model: "claude-opus-5",
      agentSessionId: "agent-1",
    });
  });
});

describe("useConversation session gone", () => {
  const notFound = () => new BorgError(GrpcStatus.NotFound, "session live-1 not found");
  const lastStream = (daemon: ReturnType<typeof fakeDaemon>) =>
    daemon.streams.filter((s) => s.method === "Stream").at(-1)!;
  const opens = (daemon: ReturnType<typeof fakeDaemon>) =>
    daemon.calls.filter((c) => c.method === "Open");
  // The daemon resumes a session under the live id it already had, so every
  // reopen here answers with "live-1"; only the seq tells the streams apart.
  const resubscribed = (daemon: ReturnType<typeof fakeDaemon>, sinceEventSeq: number) =>
    waitFor(() =>
      expect(lastStream(daemon).request).toEqual({ sessionId: "live-1", sinceEventSeq }),
    );

  it("opens the conversation again by its agent session id, not by the route", async () => {
    // A daemon restart forgets every live session. Re-running the open for
    // "new" would start an empty conversation and abandon this one.
    const daemon = fakeDaemon({
      ...OPEN,
      GetSession: { messages: [{ role: "user", text: "hello" }] },
    });
    const conv = mount(daemon, "new");
    await ready(conv);
    act(() => conv.current.send("go"));
    expect(conv.current.busy).toBe(true);

    daemon.answer("Open", { sessionId: "live-1", lastEventSeq: 3 });
    act(() => lastStream(daemon).handlers.onError?.(notFound()));

    await resubscribed(daemon, 3);
    expect(opens(daemon).map((c) => c.request)).toEqual([{}, { resumeId: "agent-1" }]);
    expect(conv.current.sessionId).toBe("live-1");
    expect(conv.current.cells.map((c) => c.kind)).toEqual(["user"]);
    expect(conv.current.busy).toBe(false);
    expect(conv.current.turnStartedAt).toBeUndefined();
    expect(conv.current.streamError).toBeUndefined();
  });

  it("resubscribes the moment the reopen lands, without waiting out the backoff", async () => {
    // The live id is unchanged, so nothing but the reopen itself can restart
    // the stream. Pin the backoff draw near its ceiling: a retry timer could
    // then not fire inside the short window this waits.
    const random = vi.spyOn(Math, "random").mockReturnValue(0.99);
    const daemon = fakeDaemon(OPEN);
    const conv = mount(daemon);
    await ready(conv);
    daemon.answer("Open", { sessionId: "live-1", lastEventSeq: 4 });
    act(() => lastStream(daemon).handlers.onError?.(notFound()));

    await waitFor(
      () => expect(lastStream(daemon).request).toEqual({ sessionId: "live-1", sinceEventSeq: 4 }),
      { timeout: 300 },
    );
    expect(conv.current.sendError).toBeUndefined();
    random.mockRestore();
  });

  it("stays busy when the reopened session still has a turn running", async () => {
    const daemon = fakeDaemon(OPEN);
    const conv = mount(daemon);
    await ready(conv);
    daemon.answer("Open", { sessionId: "live-1", lastEventSeq: 0 });
    daemon.answer("GetState", { ...OPEN.GetState, turnInFlight: true });
    act(() => lastStream(daemon).handlers.onError?.(notFound()));
    await resubscribed(daemon, 0);
    expect(conv.current.busy).toBe(true);
  });

  it("gives up after one failed reopen instead of redialling forever", async () => {
    const daemon = fakeDaemon(OPEN);
    const conv = mount(daemon);
    await ready(conv);
    act(() => conv.current.send("go"));
    daemon.answer("Open", notFound());
    act(() => lastStream(daemon).handlers.onError?.(notFound()));

    await waitFor(() => expect(conv.current.sendError).toBeDefined());
    expect(conv.current.sendError?.message).toMatch(/no longer open/);
    expect(conv.current.busy).toBe(false);
    expect(conv.current.streamError).toBeUndefined();
    expect(opens(daemon)).toHaveLength(2);
    // A later error from the retry loop must not start another attempt.
    act(() => lastStream(daemon).handlers.onError?.(notFound()));
    await act(async () => {});
    expect(opens(daemon)).toHaveLength(2);
  });

  it("gives up when the reopened session is missing at once, rather than looping", async () => {
    const daemon = fakeDaemon(OPEN);
    const conv = mount(daemon);
    await ready(conv);
    daemon.answer("Open", { sessionId: "live-1", lastEventSeq: 0 });
    act(() => lastStream(daemon).handlers.onError?.(notFound()));
    await resubscribed(daemon, 0);

    act(() => lastStream(daemon).handlers.onError?.(notFound()));
    await waitFor(() => expect(conv.current.sendError?.message).toMatch(/no longer open/));
    expect(opens(daemon)).toHaveLength(2);
  });

  it("gives an idle conversation a fresh reopen after a second restart", async () => {
    // No event arrives after the first reopen, so only the outage that opens
    // the second restart can say the first try belongs to an older outage.
    const daemon = fakeDaemon(OPEN);
    const conv = mount(daemon);
    await ready(conv);
    daemon.answer("Open", { sessionId: "live-1", lastEventSeq: 4 });
    act(() => lastStream(daemon).handlers.onError?.(notFound()));
    await resubscribed(daemon, 4);

    daemon.answer("Open", { sessionId: "live-1", lastEventSeq: 5 });
    const stream = lastStream(daemon);
    act(() => stream.handlers.onError?.(new BorgError(GrpcStatus.Unavailable, "restarting")));
    act(() => stream.handlers.onError?.(notFound()));

    await resubscribed(daemon, 5);
    expect(opens(daemon)).toHaveLength(3);
    expect(conv.current.sendError).toBeUndefined();
  });

  it("reports the reopen's own failure when it is not a missing session", async () => {
    const daemon = fakeDaemon(OPEN);
    const conv = mount(daemon);
    await ready(conv);
    daemon.answer("Open", new BorgError(GrpcStatus.Unavailable, "daemon down"));
    act(() => lastStream(daemon).handlers.onError?.(notFound()));
    await waitFor(() => expect(conv.current.sendError?.message).toBe("daemon down"));
  });

  it("does not reopen a conversation it has no id for, once there is something to lose", async () => {
    // Nobody has sent yet, so the daemon issued no agent session id; but a
    // steer replay put a row on screen. A fresh session would drop it.
    const daemon = fakeDaemon({ ...OPEN, GetState: { ...OPEN.GetState, agentSessionId: "" } });
    const conv = mount(daemon, "new");
    await ready(conv);
    emit(daemon, { steerReceived: { text: "hi" } as never });
    act(() => lastStream(daemon).handlers.onError?.(notFound()));
    await waitFor(() => expect(conv.current.sendError?.message).toMatch(/no longer open/));
    expect(opens(daemon)).toHaveLength(1);
  });

  it("ignores a NotFound raised before the reopen landed, under the same live id", async () => {
    // The retry loop keeps dialling until the reopen lands. Its fresh NotFound
    // must not turn a reopen that worked into "no longer open", though the
    // session it names has the same id as the one now on screen.
    const daemon = fakeDaemon(OPEN);
    const conv = mount(daemon);
    await ready(conv);
    let finishOpen: (value: unknown) => void = () => {};
    daemon.answer("Open", () => new Promise((resolve) => (finishOpen = resolve)));
    const oldStream = lastStream(daemon);
    act(() => oldStream.handlers.onError?.(notFound()));
    await waitFor(() => expect(opens(daemon)).toHaveLength(2));

    act(() => oldStream.handlers.onError?.(notFound()));
    await act(async () => finishOpen({ sessionId: "live-1", lastEventSeq: 4 }));

    await resubscribed(daemon, 4);
    await act(async () => {});
    expect(conv.current.sendError).toBeUndefined();
    expect(opens(daemon)).toHaveLength(2);
  });

  it("keeps an event that arrived during the reopen exactly once", async () => {
    // A redial can succeed while the history loads. What it delivers belongs
    // after the snapshot, and the resubscribe from the reopen's seq replays it.
    // Applied early as well, its turn end would read the title twice.
    const daemon = fakeDaemon(OPEN);
    const conv = mount(daemon);
    await ready(conv);
    let finishOpen: (value: unknown) => void = () => {};
    daemon.answer("Open", () => new Promise((resolve) => (finishOpen = resolve)));
    daemon.answer("GetSession", { messages: [{ role: "user", text: "hello" }] });
    act(() => lastStream(daemon).handlers.onError?.(notFound()));
    await waitFor(() => expect(opens(daemon)).toHaveLength(2));
    const reads = () => daemon.calls.filter((c) => c.method === "GetState").length;
    const turn = () => {
      emit(daemon, { eventSeq: 5, textDelta: { text: "early" } as never });
      emit(daemon, { eventSeq: 6, turnComplete: {} as never });
    };

    turn();
    await act(async () => finishOpen({ sessionId: "live-1", lastEventSeq: 4 }));
    await resubscribed(daemon, 4);
    const afterReopen = reads();
    turn();
    await act(async () => {});

    expect(conv.current.cells.map((c) => c.kind)).toEqual(["user", "assistant"]);
    expect(conv.current.cells[1]).toMatchObject({ text: "early" });
    // The open, the reopen, and one title read for the one turn end.
    expect(afterReopen).toBe(2);
    expect(reads()).toBe(3);
  });

  it("carries on with the reopened session when only the history read fails", async () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    const daemon = fakeDaemon({
      ...OPEN,
      GetSession: { messages: [{ role: "user", text: "hello" }] },
    });
    const conv = mount(daemon);
    await ready(conv);
    await waitFor(() => expect(conv.current.cells).toHaveLength(1));
    daemon.answer("Open", { sessionId: "live-1", lastEventSeq: 0 });
    daemon.answer("GetSession", new BorgError(GrpcStatus.Unavailable, "db locked"));
    act(() => lastStream(daemon).handlers.onError?.(notFound()));

    await resubscribed(daemon, 0);
    expect(conv.current.cells).toEqual([{ kind: "user", text: "hello" }]);
    expect(conv.current.sendError).toBeUndefined();
    expect(warn).toHaveBeenCalledWith(
      "reloading the conversation history failed",
      expect.objectContaining({ message: "db locked" }),
    );
    warn.mockRestore();
  });

  it("learns a new conversation's id during its first turn, so a restart can resume it", async () => {
    // At "new" the daemon has issued no agent session id when the page opens,
    // and the turn-end read that would learn it never comes if the daemon
    // restarts mid-turn.
    const daemon = fakeDaemon({ ...OPEN, GetState: { ...OPEN.GetState, agentSessionId: "" } });
    const conv = mount(daemon, "new");
    await ready(conv);
    act(() => conv.current.send("go"));
    daemon.answer("GetState", { ...OPEN.GetState, agentSessionId: "agent-7" });
    daemon.answer("GetSession", { messages: [{ role: "user", text: "go" }] });
    const reads = daemon.calls.filter((c) => c.method === "GetState").length;
    emit(daemon, { preparing: {} as never });
    emit(daemon, { textDelta: { text: "work" } as never });
    await waitFor(() =>
      expect(daemon.calls.filter((c) => c.method === "GetState")).toHaveLength(reads + 1),
    );
    await act(async () => {});

    daemon.answer("Open", { sessionId: "live-1", lastEventSeq: 0 });
    act(() => lastStream(daemon).handlers.onError?.(notFound()));
    await resubscribed(daemon, 0);
    expect(opens(daemon).map((c) => c.request)).toEqual([{}, { resumeId: "agent-7" }]);
    expect(conv.current.sendError).toBeUndefined();
  });

  it("treats a lost connection as an outage, not a missing session", async () => {
    const daemon = fakeDaemon(OPEN);
    const conv = mount(daemon);
    await ready(conv);
    act(() => lastStream(daemon).handlers.onError?.(new BorgError(GrpcStatus.Unavailable, "x")));
    await waitFor(() => expect(conv.current.streamError).toBeDefined());
    expect(daemon.calls.filter((c) => c.method === "Open")).toHaveLength(1);
  });
});
