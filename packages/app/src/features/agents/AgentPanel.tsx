import {
  AgentsIcon,
  CheckIcon,
  CloseIcon,
  Meter,
  Pill,
  SECTION_LABEL_TEXT,
  StatusDot,
  type DotTone,
} from "@borg/ui";
import type { admin as adminPb } from "@borg/client";
import { formatTokens } from "../../lib/format";
import { classifyStatus, failureReason, isLive, statusLabel, useAgentTree } from "./useAgentTree";

function statusTone(status: string): DotTone {
  if (status === "paused") return "paused";
  switch (classifyStatus(status)) {
    case "live":
      return "busy";
    case "done":
      return "idle";
    // A status nobody recognizes is a problem until proven otherwise.
    case "failed":
    case "unknown":
      return "alert";
  }
}

function AgentRow({
  agent,
  onPause,
  onResume,
  onKill,
}: {
  agent: adminPb.AgentEntry;
  onPause: () => void;
  onResume: () => void;
  onKill: () => void;
}) {
  const live = isLive(agent.status);
  const reason = failureReason(agent);
  const metrics = agent.metrics;
  const tokens = metrics ? Number(metrics.inputTokens) + Number(metrics.outputTokens) : 0;

  return (
    <div
      className="group flex items-start gap-2 py-1.5"
      // Depth is the tree; indenting is what makes it readable as one.
      style={{ paddingLeft: agent.depth * 14 }}
    >
      <StatusDot tone={statusTone(agent.status)} className="mt-[5px]" />
      <div className="min-w-0 flex-1">
        <div className="truncate text-[12.5px] leading-[1.35] text-ink">
          {agent.nickname || agent.role || agent.id}
        </div>
        <div className="mt-[2px] truncate text-[11px] leading-none text-ink3">
          {statusLabel(agent)}
          {tokens > 0 && <> · {formatTokens(tokens)} tok</>}
          {metrics && metrics.toolCalls > 0n && <> · {Number(metrics.toolCalls)} calls</>}
        </div>
        {/* Every way of stopping badly gets a line. An agent that went quiet
            with nothing to say is exactly the case worth naming. */}
        {reason !== undefined && (
          <div className="mt-1 text-[11px] leading-[1.4] text-alert">{reason}</div>
        )}
      </div>
      {live && agent.depth > 0 && (
        <div className="flex flex-none items-center gap-1 hover-reveal">
          <Pill
            square
            sizeClasses="px-2 py-1 text-[11px]"
            onClick={agent.status === "paused" ? onResume : onPause}
          >
            {agent.status === "paused" ? "Resume" : "Pause"}
          </Pill>
          <button
            type="button"
            aria-label="Stop this agent and its children"
            onClick={onKill}
            className="flex h-6 w-6 cursor-pointer items-center justify-center rounded border-0 bg-transparent text-ink3 hover:text-alert"
          >
            <CloseIcon size={13} />
          </button>
        </div>
      )}
    </div>
  );
}

/**
 * The shell commands that must pass before a goal can finish.
 *
 * `attempts` is the consecutive-failure count, reset to zero on a pass, so a
 * non-zero value on a gate that has run is the interesting signal: it is what
 * is currently blocking the goal.
 */
function Gates({ gates }: { gates: adminPb.GoalGate[] }) {
  if (gates.length === 0) return null;
  return (
    <div className="mt-2.5 border-t border-line2 pt-2.5">
      <div className={`${SECTION_LABEL_TEXT} mb-1.5`}>Gates</div>
      {gates.map((gate) => {
        const ran = gate.lastExitCodePresent;
        const passed = ran && Number(gate.lastExitCode) === 0;
        return (
          <div key={Number(gate.ordinal)} className="flex items-start gap-2 py-[3px]">
            <StatusDot tone={!ran ? "idle" : passed ? "ok" : "alert"} className="mt-[5px]" />
            <span className="min-w-0 flex-1 truncate font-mono text-[11px] leading-[1.4] text-ink2">
              {gate.command}
            </span>
            {!passed && Number(gate.attempts) > 0 && (
              <span className="flex-none text-[10.5px] leading-none text-alert">
                ×{Number(gate.attempts)}
              </span>
            )}
          </div>
        );
      })}
    </div>
  );
}

function GoalCard({ goal, gates }: { goal: adminPb.GoalDetail; gates: adminPb.GoalGate[] }) {
  const budget = goal.tokenBudgetPresent ? Number(goal.tokenBudget) : undefined;
  const used = Number(goal.tokensUsed);
  const percent =
    budget && budget > 0 ? Math.min(100, Math.round((used / budget) * 100)) : undefined;

  return (
    <div className="mb-3 rounded-[9px] border border-line2 bg-surface3 px-3 py-2.5">
      <div className="flex items-baseline gap-2">
        <span className={SECTION_LABEL_TEXT}>Goal</span>
        <span className="flex-1 text-right text-[11px] leading-none text-ink3">{goal.status}</span>
      </div>
      <div className="mt-2 text-[12.5px] leading-[1.45] text-ink">{goal.objective}</div>
      {percent !== undefined && (
        <>
          <Meter percent={percent} warnAt={90} className="mt-2" />
          <div className="mt-1.5 text-[11px] leading-none text-ink3">
            {formatTokens(used)} of {formatTokens(budget!)} tokens
          </div>
        </>
      )}
      <Gates gates={gates} />
    </div>
  );
}

/**
 * What Borg has running underneath this conversation.
 *
 * A panel rather than a page: sub-agents and a goal are live state of the
 * session on screen, not a place you navigate to. The panel is opened and
 * closed by the caller (⌘J, or the header button) and stays closed by default
 * — an empty tree permanently occupying a third of the window would be the
 * opposite of what this is for.
 */
export function AgentPanel({
  sessionId,
  onClose,
}: {
  sessionId: string | undefined;
  onClose: () => void;
}) {
  const tree = useAgentTree(sessionId);
  const children = tree.agents.filter((a) => a.depth > 0);

  return (
    <aside className="flex w-[280px] flex-none flex-col border-l border-line2 bg-surface2 px-3.5 pt-4 pb-3">
      <div className="mb-3 flex items-center gap-2">
        <AgentsIcon size={15} />
        <span className={`${SECTION_LABEL_TEXT} flex-1`}>Activity</span>
        <button
          type="button"
          aria-label="Hide activity"
          onClick={onClose}
          className="flex h-6 w-6 cursor-pointer items-center justify-center rounded border-0 bg-transparent text-ink3 hover:text-ink"
        >
          <CloseIcon size={13} />
        </button>
      </div>

      {tree.error && (
        <div role="alert" className="mb-2 text-[11px] leading-[1.4] text-alert">
          {tree.error}
        </div>
      )}

      <div className="min-h-0 flex-1 overflow-y-auto">
        {tree.goal && <GoalCard goal={tree.goal} gates={tree.gates} />}

        {children.length === 0 ? (
          <div className="flex items-center gap-2 py-2 text-[12px] leading-[1.5] text-ink3">
            <CheckIcon size={13} /> Nothing delegated.
          </div>
        ) : (
          children.map((agent) => (
            <AgentRow
              key={agent.id}
              agent={agent}
              onPause={() => tree.pause(agent.id)}
              onResume={() => tree.resume(agent.id)}
              onKill={() => tree.kill(agent.id)}
            />
          ))
        )}
      </div>
    </aside>
  );
}
