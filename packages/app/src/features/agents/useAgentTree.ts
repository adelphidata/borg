import { useCallback, useEffect, useRef, useState } from "react";
import type { admin as adminPb } from "@borg/client";
import { asBorgError, useBorg, useMutation, useServerStream } from "../../lib/useDaemon";

/** Statuses that mean the agent is still doing something. */
export function isLive(status: string): boolean {
  return status === "running" || status === "pending_init" || status === "paused";
}

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

  useServerStream<adminPb.AgentTreeEvent>(
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

  const pauseMutation = useMutation((borg, id: string) => borg.admin.pauseAgent({ agentId: id }));
  const resumeMutation = useMutation((borg, id: string) => borg.admin.resumeAgent({ agentId: id }));
  // Kill the subtree, not the node: orphaned children keep burning tokens with
  // nothing left to report to.
  const killMutation = useMutation((borg, id: string) =>
    borg.admin.killAgentSubtree({ agentId: id }),
  );

  return {
    agents,
    goal,
    gates,
    active: agents.some((a) => a.depth > 0 && isLive(a.status)),
    pause: (id) => void pauseMutation.run(id),
    resume: (id) => void resumeMutation.run(id),
    kill: (id) => void killMutation.run(id),
    error: error ?? (pauseMutation.error ?? resumeMutation.error ?? killMutation.error)?.message,
  };
}
