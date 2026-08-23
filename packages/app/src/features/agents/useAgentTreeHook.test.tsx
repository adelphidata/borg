import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, render, waitFor } from "@testing-library/react";
import { BorgError, GrpcStatus } from "@borg/client";
import { PlatformProvider } from "../../lib/platform";
import { fakeDaemon } from "../../test/harness";
import { POLL_ACTIVE_MS, POLL_IDLE_MS, useAgentTree, type AgentTree } from "./useAgentTree";

const agent = (over: Record<string, unknown> = {}) => ({
  id: "a1",
  nickname: "scout",
  role: "researcher",
  model: "claude-opus-5",
  parentSessionId: "s1",
  sessionId: "child-1",
  depth: 1,
  status: "running",
  error: "",
  result: "",
  ...over,
});

const BASE = {
  ListAgents: { agents: [agent()] },
  GetGoal: { present: false },
};

function mount(daemon: ReturnType<typeof fakeDaemon>, sessionId?: string | undefined) {
  const target = arguments.length > 1 ? sessionId : "s1";
  const seen: { current: AgentTree } = { current: undefined as never };
  function Probe() {
    seen.current = useAgentTree(target);
    return null;
  }
  render(
    <PlatformProvider
      platform={{ kind: "desktop", transport: daemon.transport, titleBarDrag: false }}
    >
      <Probe />
    </PlatformProvider>,
  );
  return seen;
}

/** Push a tree delta down the subscription. */
function emit(daemon: ReturnType<typeof fakeDaemon>, event: Record<string, unknown>) {
  const stream = daemon.streams.filter((s) => s.method === "SubscribeAgentTreeEvents").at(-1);
  if (!stream) throw new Error("the panel never subscribed");
  act(() => stream.handlers.onMessage(event as never));
}

/** Report the stream as unavailable, which is what makes the hook poll. */
function unimplemented(daemon: ReturnType<typeof fakeDaemon>) {
  const stream = daemon.streams.filter((s) => s.method === "SubscribeAgentTreeEvents").at(-1)!;
  act(() => stream.handlers.onError?.(new BorgError(GrpcStatus.Unimplemented, "not served")));
}

/** Let every pending promise settle; used where fake timers rule out waitFor. */
const flush = async () => {
  for (let i = 0; i < 5; i++) await act(async () => {});
};

afterEach(() => {
  vi.useRealTimers();
});

describe("useAgentTree", () => {
  it("loads the tree for its session", async () => {
    const daemon = fakeDaemon(BASE);
    const tree = mount(daemon);
    await waitFor(() => expect(tree.current.agents).toHaveLength(1));
    expect(daemon.calls.find((c) => c.method === "ListAgents")?.request).toEqual({
      sessionId: "s1",
    });
  });

  it("treats an empty session id as the daemon's active session", async () => {
    // Only `undefined` means there is nothing to ask about.
    const daemon = fakeDaemon(BASE);
    mount(daemon, "");
    await waitFor(() => expect(daemon.calls.some((c) => c.method === "ListAgents")).toBe(true));
  });

  it("asks nothing at all with no session", async () => {
    const daemon = fakeDaemon(BASE);
    const tree = mount(daemon, undefined);
    await waitFor(() => expect(tree.current.agents).toEqual([]));
    expect(daemon.calls).toEqual([]);
  });

  it("loads the goal's quality gates only when a goal exists", async () => {
    const daemon = fakeDaemon({
      ...BASE,
      GetGoal: { present: true, objective: "ship it", status: "active" },
      MutateGoalGates: { gates: [{ ordinal: 1, command: "cargo test" }] },
    });
    const tree = mount(daemon);
    await waitFor(() => expect(tree.current.gates).toHaveLength(1));
    expect(daemon.calls.find((c) => c.method === "MutateGoalGates")?.request).toMatchObject({
      sessionId: "s1",
      op: 1,
    });
  });

  it("does not ask for gates when there is no goal", async () => {
    const daemon = fakeDaemon(BASE);
    const tree = mount(daemon);
    await waitFor(() => expect(tree.current.agents).toHaveLength(1));
    expect(daemon.calls.some((c) => c.method === "MutateGoalGates")).toBe(false);
    expect(tree.current.goal).toBeUndefined();
  });

  it("applies a delta to the agent it names", async () => {
    const daemon = fakeDaemon(BASE);
    const tree = mount(daemon);
    await waitFor(() => expect(tree.current.agents).toHaveLength(1));

    emit(daemon, { seq: 1, agent: agent({ status: "completed" }) });
    expect(tree.current.agents[0].status).toBe("completed");
  });

  it("appends an agent it has not seen before", async () => {
    const daemon = fakeDaemon(BASE);
    const tree = mount(daemon);
    await waitFor(() => expect(tree.current.agents).toHaveLength(1));

    emit(daemon, { seq: 1, agent: agent({ id: "a2", nickname: "scribe" }) });
    expect(tree.current.agents.map((a) => a.id)).toEqual(["a1", "a2"]);
  });

  it("re-reads the whole tree when the sequence gaps", async () => {
    // The daemon's broadcast channel drops under lag; patching deltas onto a
    // tree that missed some diverges silently.
    const daemon = fakeDaemon(BASE);
    const tree = mount(daemon);
    await waitFor(() => expect(tree.current.agents).toHaveLength(1));
    emit(daemon, { seq: 1, agent: agent() });
    const before = daemon.calls.filter((c) => c.method === "ListAgents").length;

    daemon.answer("ListAgents", { agents: [agent({ status: "completed" })] });
    emit(daemon, { seq: 5, agent: agent({ status: "running" }) });

    await waitFor(() =>
      expect(daemon.calls.filter((c) => c.method === "ListAgents").length).toBe(before + 1),
    );
    await waitFor(() => expect(tree.current.agents[0].status).toBe("completed"));
  });

  it("ignores an event carrying no agent", async () => {
    const daemon = fakeDaemon(BASE);
    const tree = mount(daemon);
    await waitFor(() => expect(tree.current.agents).toHaveLength(1));

    emit(daemon, { seq: 1 });
    expect(tree.current.agents).toHaveLength(1);
  });

  it("reports the tree as active only while a child is working", async () => {
    const daemon = fakeDaemon({
      ...BASE,
      ListAgents: { agents: [agent({ id: "root", depth: 0 }), agent({ status: "running" })] },
    });
    const tree = mount(daemon);
    await waitFor(() => expect(tree.current.active).toBe(true));

    emit(daemon, { seq: 1, agent: agent({ status: "completed" }) });
    expect(tree.current.active).toBe(false);
  });

  it("polls while the delta stream is unavailable", async () => {
    // Without this the panel shows one snapshot and never moves, so a stalled
    // agent looks exactly like a working one.
    vi.useFakeTimers();
    const daemon = fakeDaemon(BASE);
    mount(daemon);
    await flush();
    unimplemented(daemon);
    const before = daemon.calls.filter((c) => c.method === "ListAgents").length;

    await act(async () => {
      vi.advanceTimersByTime(POLL_ACTIVE_MS);
    });
    expect(daemon.calls.filter((c) => c.method === "ListAgents").length).toBe(before + 1);
  });

  it("polls more slowly when nothing is running", async () => {
    vi.useFakeTimers();
    const daemon = fakeDaemon({
      ...BASE,
      ListAgents: { agents: [agent({ status: "completed" })] },
    });
    const tree = mount(daemon);
    await flush();
    expect(tree.current.active).toBe(false);
    unimplemented(daemon);
    const before = daemon.calls.filter((c) => c.method === "ListAgents").length;

    await act(async () => {
      vi.advanceTimersByTime(POLL_ACTIVE_MS);
    });
    expect(daemon.calls.filter((c) => c.method === "ListAgents").length).toBe(before);

    await act(async () => {
      vi.advanceTimersByTime(POLL_IDLE_MS - POLL_ACTIVE_MS);
    });
    expect(daemon.calls.filter((c) => c.method === "ListAgents").length).toBe(before + 1);
  });

  it("does not put a red banner on a stream the daemon simply does not serve", async () => {
    const daemon = fakeDaemon(BASE);
    const tree = mount(daemon);
    await waitFor(() => expect(tree.current.agents).toHaveLength(1));

    unimplemented(daemon);
    expect(tree.current.error).toBeUndefined();
  });

  it("reports a stream that failed for a real reason", async () => {
    const daemon = fakeDaemon(BASE);
    const tree = mount(daemon);
    await waitFor(() => expect(tree.current.agents).toHaveLength(1));

    const stream = daemon.streams.filter((s) => s.method === "SubscribeAgentTreeEvents").at(-1)!;
    act(() => stream.handlers.onError?.(new BorgError(GrpcStatus.Internal, "bus closed")));
    await waitFor(() => expect(tree.current.error).toBe("bus closed"));
  });

  it("reports a failed load", async () => {
    const daemon = fakeDaemon({
      ...BASE,
      ListAgents: new BorgError(GrpcStatus.Unavailable, "daemon down"),
    });
    const tree = mount(daemon);
    await waitFor(() => expect(tree.current.error).toBe("daemon down"));
  });

  it("pauses, resumes, and kills the whole subtree", async () => {
    // Killing the node alone leaves orphaned children burning tokens.
    const daemon = fakeDaemon({
      ...BASE,
      PauseAgent: {},
      ResumeAgent: {},
      KillAgentSubtree: {},
    });
    const tree = mount(daemon);
    await waitFor(() => expect(tree.current.agents).toHaveLength(1));

    await act(async () => {
      tree.current.pause("a1");
      tree.current.resume("a1");
      tree.current.kill("a1");
    });

    await waitFor(() =>
      expect(daemon.calls.some((c) => c.method === "KillAgentSubtree")).toBe(true),
    );
    expect(daemon.calls.find((c) => c.method === "PauseAgent")?.request).toEqual({ agentId: "a1" });
    expect(daemon.calls.find((c) => c.method === "KillAgentSubtree")?.request).toEqual({
      agentId: "a1",
    });
  });

  it("says a control is not built yet rather than showing a gRPC fault", async () => {
    const daemon = fakeDaemon({
      ...BASE,
      PauseAgent: new BorgError(GrpcStatus.Unimplemented, "PauseAgent is not implemented"),
    });
    const tree = mount(daemon);
    await waitFor(() => expect(tree.current.agents).toHaveLength(1));

    await act(async () => {
      tree.current.pause("a1");
    });
    await waitFor(() => expect(tree.current.error).toBe("Borg's daemon cannot do this yet."));
  });

  it("reports a control that failed for a real reason verbatim", async () => {
    const daemon = fakeDaemon({
      ...BASE,
      KillAgentSubtree: new BorgError(GrpcStatus.NotFound, "no such agent"),
    });
    const tree = mount(daemon);
    await waitFor(() => expect(tree.current.agents).toHaveLength(1));

    await act(async () => {
      tree.current.kill("gone");
    });
    await waitFor(() => expect(tree.current.error).toBe("no such agent"));
  });
});
