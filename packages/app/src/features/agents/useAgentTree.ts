import { useCallback, useEffect, useRef, useState } from "react";
import { GrpcStatus, type admin as adminPb } from "@borg/client";
import { showNotice } from "../../lib/notice";
import { asBorgError, useBorg, useMutation, useServerStream } from "../../lib/useDaemon";

/** What an agent's wire status means for the reader. */
export type AgentLiveness = "live" | "done" | "failed" | "unknown";

/**
 * Sorts a wire status into the four things a reader can do about it.
 *
 * Whitelisted rather than defaulted: a status this build has never heard of
 * reads as a problem, never as a quiet finished row. Treating the unknown as
 * benign is how a stalled agent comes to look identical to a completed one.
 */
export function classifyStatus(status: string): AgentLiveness {
  switch (status) {
    case "running":
    case "pending_init":
    case "paused":
      return "live";
    case "completed":
      return "done";
    case "errored":
    case "shutdown":
      return "failed";
    default:
      return "unknown";
  }
}

/** Statuses that mean the agent is still doing something. */
export function isLive(status: string): boolean {
  return classifyStatus(status) === "live";
}

/** What went wrong, in words, or undefined while nothing has. */
export function failureReason(agent: adminPb.AgentEntry): string | undefined {
  const reason = agent.error?.trim() ?? "";
  switch (classifyStatus(agent.status)) {
    case "live":
    case "done":
      return undefined;
    case "failed":
      if (agent.status === "shutdown" && reason === "") return "Stopped by Borg.";
      if (reason === "") return "Stopped without reporting a reason.";
      // The stall watchdog writes a machine-shaped line; lead with what it
      // means rather than with the word "stalled:".
      return reason.startsWith("stalled:")
        ? `Stalled — ${reason.slice("stalled:".length).trim()}`
        : reason;
    case "unknown":
      return `Unrecognized status "${agent.status}" — treating it as stopped.`;
  }
}

/** How a status reads in the panel. */
export function statusLabel(agent: adminPb.AgentEntry): string {
  switch (agent.status) {
    case "running":
      return "Running";
    case "pending_init":
      return "Queued";
    case "paused":
      return "Paused";
    case "completed":
      return "Done";
    case "errored":
      return agent.error?.startsWith("stalled:") ? "Stalled" : "Failed";
    case "shutdown":
      return "Stopped";
    default:
      return "Unknown";
  }
}

/** How often to re-ask for the tree while the delta stream is unavailable. */
export const POLL_ACTIVE_MS = 3_000;
export const POLL_IDLE_MS = 15_000;

export interface AgentTree {
  agents: adminPb.AgentEntry[];
  goal: adminPb.GoalDetail | undefined;
  gates: adminPb.GoalGate[];
  /** True while anything below the root is still working. */
  active: boolean;
  pause: (id: string) => void;
  resume: (id: string) => void;
  kill: (id: string) => void;
  error: string | undefined;
}

/**
 * The live agent tree and goal for one session.
 *
 * Fifteen daemon RPCs across the two subsystems, none of which either client
 * has ever called. The tree arrives as a snapshot plus a stream of deltas, and
 * the stream carries a monotonic `seq` precisely so a subscriber can notice a
 * gap — the daemon's broadcast channel drops messages under lag — and re-issue
 * `ListAgents` instead of quietly rendering a stale tree.
 */
export function useAgentTree(sessionId: string | undefined): AgentTree {
  const borg = useBorg();
  const [agents, setAgents] = useState<adminPb.AgentEntry[]>([]);
  const [goal, setGoal] = useState<adminPb.GoalDetail>();
  const [gates, setGates] = useState<adminPb.GoalGate[]>([]);
  // A ref, not state: the sequence number is only ever compared, never
  // rendered. In state it forced a second render per event and — worse — the
  // stream callback would compare against whatever value its closure captured.
  const lastSeq = useRef(0);
  const [error, setError] = useState<string>();

  const refresh = useCallback(async () => {
    // An empty string is meaningful: the daemon reads it as "the active
    // session". Only `undefined` means there is nothing to ask about.
    if (sessionId === undefined) return;
    try {
      const [list, goalDetail] = await Promise.all([
        borg.admin.listAgents({ sessionId }),
        borg.admin.getGoal({ sessionId }),
      ]);
      setAgents(list.agents);
      setGoal(goalDetail.present ? goalDetail : undefined);
      // Quality gates have no getter of their own — the mutation's LIST op is
      // the read path. Only worth asking for when a goal actually exists.
      if (goalDetail.present) {
        const listed = await borg.admin.mutateGoalGates({
          sessionId,
          op: 1, // OP_LIST
          command: "",
          ordinal: 0,
        });
        setGates(listed.gates);
      } else {
        setGates([]);
      }
      setError(undefined);
    } catch (e) {
      setError(asBorgError(e).message);
    }
  }, [borg, sessionId]);

  useEffect(() => {
    setAgents([]);
    setGoal(undefined);
    setGates([]);
    lastSeq.current = 0;
    void refresh();
  }, [refresh]);

  const stream = useServerStream<adminPb.AgentTreeEvent>(
    (borg, handlers) => {
      if (sessionId === undefined) return () => {};
      return borg.admin.subscribeAgentTreeEvents({ id: sessionId }, handlers);
    },
    (event) => {
      const seq = Number(event.seq);
      // A gap means the broadcast lagged and events were dropped; patching
      // deltas onto a tree that missed some would silently diverge.
      if (lastSeq.current !== 0 && seq !== lastSeq.current + 1) {
        lastSeq.current = seq;
        void refresh();
        return;
      }
      lastSeq.current = seq;
      const agent = event.agent;
      if (!agent) return;
      setAgents((prev) => {
        const at = prev.findIndex((a) => a.id === agent.id);
        if (at === -1) return [...prev, agent];
        const next = [...prev];
        next[at] = agent;
        return next;
      });
    },
    [sessionId],
  );

  const active = agents.some((a) => a.depth > 0 && isLive(a.status));

  // The daemon does not serve the delta stream yet, so without this the panel
  // shows one snapshot taken at mount and never moves again — an agent that
  // stalls looks exactly like one still working. Polling covers the gap and
  // should go the day `SubscribeAgentTreeEvents` starts answering.
  const streamServed = stream.status === "open";
  useEffect(() => {
    if (streamServed || sessionId === undefined) return;
    const id = setInterval(() => void refresh(), active ? POLL_ACTIVE_MS : POLL_IDLE_MS);
    return () => clearInterval(id);
  }, [streamServed, sessionId, active, refresh]);

  // An unimplemented stream is not worth a red banner on every session — it is
  // the signal to poll instead, which is what the effect above does.
  const streamError =
    stream.status === "error" && stream.error?.code !== GrpcStatus.Unimplemented
      ? stream.error?.message
      : undefined;

  /** A control the daemon has not implemented is a gap, not a gRPC fault. */
  const controlError = (failure: { code: number; message: string } | undefined) =>
    failure === undefined
      ? undefined
      : failure.code === GrpcStatus.Unimplemented
        ? "Borg's daemon cannot do this yet."
        : failure.message;

  // The row's status changes only once the tree catches up, which can be a
  // poll away; the toast says at once that the control was heard.
  const nameOf = (id: string) => {
    const agent = agents.find((a) => a.id === id);
    return agent?.nickname || agent?.role || "the agent";
  };
  const pauseMutation = useMutation((borg, id: string) => borg.admin.pauseAgent({ agentId: id }), {
    onSuccess: (_data, id) => showNotice(`Paused ${nameOf(id)}`),
  });
  const resumeMutation = useMutation(
    (borg, id: string) => borg.admin.resumeAgent({ agentId: id }),
    { onSuccess: (_data, id) => showNotice(`Resumed ${nameOf(id)}`) },
  );
  // Kill the subtree, not the node: orphaned children keep burning tokens with
  // nothing left to report to.
  const killMutation = useMutation(
    (borg, id: string) => borg.admin.killAgentSubtree({ agentId: id }),
    { onSuccess: (_data, id) => showNotice(`Stopped ${nameOf(id)} and everything it started`) },
  );

  return {
    agents,
    goal,
    gates,
    active,
    pause: (id) => void pauseMutation.run(id),
    resume: (id) => void resumeMutation.run(id),
    kill: (id) => void killMutation.run(id),
    error:
      error ??
      streamError ??
      controlError(pauseMutation.error ?? resumeMutation.error ?? killMutation.error),
  };
}
