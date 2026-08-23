import { describe, expect, it, vi } from "vitest";
import { act, screen, waitFor } from "@testing-library/react";
import { BorgError, GrpcStatus } from "@borg/client";
import { fakeDaemon, renderApp } from "../../test/harness";
import { AgentPanel } from "./AgentPanel";
import { failureReason, statusLabel } from "./useAgentTree";

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
  metrics: {
    inputTokens: 900,
    outputTokens: 300,
    reasoningTokens: 0,
    costUsd: 0,
    toolCalls: 4n,
    filesTouched: [],
  },
  ...over,
});

const tree = (agents: unknown[], extra: Record<string, unknown> = {}) => ({
  ListAgents: { agents },
  GetGoal: { present: false },
  ...extra,
});

function mount(daemon: ReturnType<typeof fakeDaemon>) {
  const onClose = vi.fn();
  const view = renderApp(<AgentPanel sessionId="s1" onClose={onClose} />, { daemon });
  return { ...view, onClose };
}

describe("failureReason", () => {
  it("says nothing about an agent that is fine", () => {
    expect(failureReason(agent() as never)).toBeUndefined();
    expect(failureReason(agent({ status: "completed" }) as never)).toBeUndefined();
  });

  it("names the case where an agent went quiet with nothing to say", () => {
    expect(failureReason(agent({ status: "errored" }) as never)).toBe(
      "Stopped without reporting a reason.",
    );
  });

  it("distinguishes Borg stopping an agent from an agent failing", () => {
    expect(failureReason(agent({ status: "shutdown" }) as never)).toBe("Stopped by Borg.");
  });

  it("leads a stall with what it means, not with the watchdog's prefix", () => {
    expect(
      failureReason(agent({ status: "errored", error: "stalled: no progress for 450s" }) as never),
    ).toBe("Stalled — no progress for 450s");
  });

  it("treats a status this build never heard of as a problem", () => {
    // Treating the unknown as benign is how a stalled agent comes to look
    // identical to a finished one.
    expect(failureReason(agent({ status: "quantum" }) as never)).toBe(
      'Unrecognized status "quantum" — treating it as stopped.',
    );
  });
});

describe("statusLabel", () => {
  it("reads a stall apart from an ordinary failure", () => {
    expect(statusLabel(agent({ status: "errored", error: "stalled: x" }) as never)).toBe("Stalled");
    expect(statusLabel(agent({ status: "errored", error: "boom" }) as never)).toBe("Failed");
  });

  it("names every status the daemon sends", () => {
    const labels = ["running", "pending_init", "paused", "completed", "shutdown", "wat"].map((s) =>
      statusLabel(agent({ status: s }) as never),
    );
    expect(labels).toEqual(["Running", "Queued", "Paused", "Done", "Stopped", "Unknown"]);
  });
});

describe("AgentPanel", () => {
  it("says nothing is delegated rather than showing an empty tree", async () => {
    mount(fakeDaemon(tree([])));
    await waitFor(() => expect(screen.getByText("Nothing delegated.")).toBeTruthy());
  });

  it("lists the children, not the root", async () => {
    // The root is the conversation itself; listing it says nothing.
    mount(fakeDaemon(tree([agent({ id: "root", depth: 0, nickname: "root" }), agent()])));
    await waitFor(() => expect(screen.getByText("scout")).toBeTruthy());
    expect(screen.queryByText("root")).toBeNull();
  });

  it("reports each agent's work so far", async () => {
    mount(fakeDaemon(tree([agent()])));
    await waitFor(() => expect(screen.getByText(/Running/)).toBeTruthy());
    expect(screen.getByText(/1.2K tok/)).toBeTruthy();
    expect(screen.getByText(/4 calls/)).toBeTruthy();
  });

  it("names an agent by role when it has no nickname", async () => {
    mount(fakeDaemon(tree([agent({ nickname: "" })])));
    await waitFor(() => expect(screen.getByText("researcher")).toBeTruthy());
  });

  it("spells out why an agent stopped badly", async () => {
    mount(fakeDaemon(tree([agent({ status: "errored", error: "stalled: no progress" })])));
    await waitFor(() => expect(screen.getByText("Stalled — no progress")).toBeTruthy());
  });

  it("offers controls only for a live child", async () => {
    mount(fakeDaemon(tree([agent({ status: "completed" })])));
    await waitFor(() => expect(screen.getByText(/Done/)).toBeTruthy());
    expect(screen.queryByText("Pause")).toBeNull();
  });

  it("pauses, resumes and stops the subtree", async () => {
    const daemon = fakeDaemon(
      tree([agent()], { PauseAgent: {}, ResumeAgent: {}, KillAgentSubtree: {} }),
    );
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Pause")).toBeTruthy());

    await act(async () => screen.getByText("Pause").click());
    await waitFor(() => expect(daemon.calls.some((c) => c.method === "PauseAgent")).toBe(true));

    await act(async () =>
      screen.getByRole("button", { name: "Stop this agent and its children" }).click(),
    );
    await waitFor(() =>
      expect(daemon.calls.some((c) => c.method === "KillAgentSubtree")).toBe(true),
    );
  });

  it("offers Resume for a paused agent", async () => {
    const daemon = fakeDaemon(tree([agent({ status: "paused" })], { ResumeAgent: {} }));
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Resume")).toBeTruthy());

    await act(async () => screen.getByText("Resume").click());
    await waitFor(() => expect(daemon.calls.some((c) => c.method === "ResumeAgent")).toBe(true));
  });

  it("shows the goal, its budget and its gates", async () => {
    const daemon = fakeDaemon(
      tree([], {
        GetGoal: {
          present: true,
          objective: "ship the daemon",
          status: "active",
          tokenBudgetPresent: true,
          tokenBudget: 100_000,
          tokensUsed: 45_000,
        },
        MutateGoalGates: {
          gates: [
            {
              ordinal: 1,
              command: "cargo test",
              lastExitCodePresent: true,
              lastExitCode: 0,
              attempts: 0,
            },
            {
              ordinal: 2,
              command: "cargo clippy",
              lastExitCodePresent: true,
              lastExitCode: 1,
              attempts: 3,
            },
          ],
        },
      }),
    );
    mount(daemon);
    await waitFor(() => expect(screen.getByText("ship the daemon")).toBeTruthy());
    expect(screen.getByText("45K of 100K tokens")).toBeTruthy();
    expect(screen.getByText("cargo test")).toBeTruthy();
    // The consecutive-failure count is what says which gate blocks the goal.
    expect(screen.getByText("×3")).toBeTruthy();
  });

  it("shows no budget meter for a goal that has no budget", async () => {
    const daemon = fakeDaemon(
      tree([], {
        GetGoal: {
          present: true,
          objective: "ship it",
          status: "active",
          tokenBudgetPresent: false,
          tokensUsed: 10,
        },
        MutateGoalGates: { gates: [] },
      }),
    );
    mount(daemon);
    await waitFor(() => expect(screen.getByText("ship it")).toBeTruthy());
    expect(screen.queryByText(/tokens$/)).toBeNull();
  });

  it("surfaces a failure from the tree", async () => {
    const daemon = fakeDaemon({
      ...tree([]),
      ListAgents: new BorgError(GrpcStatus.Unavailable, "daemon down"),
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByRole("alert").textContent).toBe("daemon down"));
  });

  it("closes on request", async () => {
    const { onClose } = mount(fakeDaemon(tree([])));
    await waitFor(() => expect(screen.getByText("Nothing delegated.")).toBeTruthy());
    act(() => screen.getByRole("button", { name: "Hide activity" }).click());
    expect(onClose).toHaveBeenCalledTimes(1);
  });
});
