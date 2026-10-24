import { admin, type BorgClient } from "@borg/client";
import { MODE_KEY } from "../settings/catalog";

/**
 * Slash commands that take words after them: `/goal …`, `/heartbeat …` and
 * `/mode …`.
 *
 * The `/` menu runs one-word commands; these carry an objective or a prompt,
 * so the menu never matches them and they used to go to the model as prose —
 * which looks like it worked and sets nothing. They run here instead, through
 * the same Admin RPCs the terminal uses, and answer with one line in the
 * transcript.
 */
export type ArgCommand =
  | { kind: "goal-show" }
  | { kind: "goal-set"; objective: string }
  | { kind: "goal-status"; status: "paused" | "active" }
  | { kind: "goal-clear" }
  | { kind: "goal-terminal-only" }
  | {
      kind: "heartbeat";
      op: admin.SessionHeartbeatMutation_Op;
      prompt: string;
      intervalSecs: number;
    }
  | { kind: "mode"; mode: "plan" | "execute" }
  | { kind: "usage"; text: string };

export const GOAL_USAGE =
  "Use /goal <what to finish> to set a goal, or /goal pause, resume or clear to manage it.";

export const MODE_USAGE =
  "Use /mode plan to have Borg propose changes and make none, or /mode execute to let it act.";

export const HEARTBEAT_USAGE =
  "Use /heartbeat every <interval> <prompt>, for example: /heartbeat every 15m Check the deploy. " +
  "Intervals are 90s, 10m, 2h or 1d, with a 60s minimum. /heartbeat pause, resume or clear manages it.";

/** `90s`, `10m`, `2h`, `1d` or bare seconds; undefined for anything else. */
export function parseIntervalSecs(raw: string): number | undefined {
  const m = /^(\d+)\s*([smhd]?)$/i.exec(raw.trim());
  if (!m) return undefined;
  const n = Number(m[1]);
  const unit = { "": 1, s: 1, m: 60, h: 3600, d: 86_400 }[m[2]!.toLowerCase()]!;
  return n > 0 ? n * unit : undefined;
}

/**
 * The command in `text`, or undefined when it is not one of these — in which
 * case it is ordinary text and is sent as usual.
 */
export function parseArgCommand(text: string): ArgCommand | undefined {
  const m = /^\/(goal|heartbeat|hb|mode)(?:\s+([\s\S]*))?$/.exec(text.trim());
  if (!m) return undefined;
  const rest = (m[2] ?? "").trim();
  if (m[1] === "mode") return parseMode(rest);
  return m[1] === "goal" ? parseGoal(rest) : parseHeartbeat(rest);
}

/**
 * `/mode plan` and `/mode execute`: the spelling chat channels use, and the one
 * Borg itself suggests, so it works here too rather than reaching the model.
 */
function parseMode(rest: string): ArgCommand {
  const mode = rest.toLowerCase();
  return mode === "plan" || mode === "execute"
    ? { kind: "mode", mode }
    : { kind: "usage", text: MODE_USAGE };
}

function parseGoal(rest: string): ArgCommand {
  if (rest === "" || rest === "status") return { kind: "goal-show" };
  if (rest === "pause") return { kind: "goal-status", status: "paused" };
  if (rest === "resume") return { kind: "goal-status", status: "active" };
  if (rest === "clear") return { kind: "goal-clear" };
  const [verb, ...tail] = rest.split(/\s+/);
  // Budgets and quality gates are power-user knobs; the terminal has them.
  if (verb === "gate" || verb === "budget" || verb === "--budget") {
    return { kind: "goal-terminal-only" };
  }
  const objective = verb === "edit" ? tail.join(" ").trim() : rest;
  return objective === "" ? { kind: "usage", text: GOAL_USAGE } : { kind: "goal-set", objective };
}

function parseHeartbeat(rest: string): ArgCommand {
  const Op = admin.SessionHeartbeatMutation_Op;
  const simple = {
    "": Op.OP_STATUS,
    status: Op.OP_STATUS,
    pause: Op.OP_PAUSE,
    resume: Op.OP_RESUME,
    clear: Op.OP_CLEAR,
  }[rest];
  if (simple !== undefined) return { kind: "heartbeat", op: simple, prompt: "", intervalSecs: 0 };
  const m = /^every\s+(\S+)\s+([\s\S]+)$/.exec(rest);
  const intervalSecs = m ? parseIntervalSecs(m[1]!) : undefined;
  if (!m || intervalSecs === undefined) return { kind: "usage", text: HEARTBEAT_USAGE };
  // The prompt is prose: sent exactly as written.
  return { kind: "heartbeat", op: Op.OP_SET, prompt: m[2]!.trim(), intervalSecs };
}

function describeGoal(goal: admin.GoalDetail): string {
  if (!goal.present) return `No goal is set for this conversation. ${GOAL_USAGE}`;
  const state =
    {
      active: "Active",
      paused: "Paused",
      budget_limited: "Stopped at its budget",
      complete: "Complete",
    }[goal.status] ?? goal.status;
  return `Goal (${state}): ${goal.objective}`;
}

/** Plain words for a wait; whole units so "every 15m" reads back as 15 minutes. */
export function formatInterval(secs: number): string {
  const units: [number, string][] = [
    [86_400, "day"],
    [3600, "hour"],
    [60, "minute"],
  ];
  for (const [size, name] of units) {
    if (secs >= size && secs % size === 0) {
      const n = secs / size;
      return `${n} ${name}${n === 1 ? "" : "s"}`;
    }
  }
  return `${secs} seconds`;
}

/**
 * The heartbeat as it now stands. The daemon's own `report` is a terse
 * terminal echo ("set", "paused"); the state below says the rest.
 */
function describeHeartbeat(
  op: admin.SessionHeartbeatMutation_Op,
  hb: admin.SessionHeartbeatDetail,
): string {
  if (op === admin.SessionHeartbeatMutation_Op.OP_CLEAR) return "Heartbeat cleared.";
  if (!hb.present) return `No heartbeat is set for this conversation. ${HEARTBEAT_USAGE}`;
  // The next fire rounds up to whole minutes: "next in 47 seconds" is noise.
  const next = Math.max(60, Math.ceil(hb.secondsUntilDue / 60) * 60);
  const when = hb.paused
    ? "paused"
    : `every ${formatInterval(hb.intervalSecs)}, next in ${formatInterval(next)}`;
  return `Heartbeat (${when}): ${hb.prompt}`;
}

/**
 * Runs `cmd` against the conversation's agent session and returns the line to
 * show. A daemon refusal comes back as text too: the user asked for something
 * and needs to see that it did not happen.
 *
 * `save` writes a setting; the app passes its settings store so the mode
 * switch beside the model follows at once.
 */
export async function runArgCommand(
  borg: BorgClient,
  agentSessionId: string,
  cmd: ArgCommand,
  save: (key: string, value: string) => Promise<boolean> = async (key, value) => {
    await borg.admin.setSetting({ key, value });
    return true;
  },
): Promise<string> {
  const sessionId = agentSessionId;
  try {
    switch (cmd.kind) {
      case "usage":
        return cmd.text;
      case "mode":
        if (!(await save(MODE_KEY, cmd.mode))) return "That did not work: the mode did not change.";
        return cmd.mode === "plan"
          ? "Plan mode is on. Borg proposes changes and makes none."
          : "Execute mode is on. Borg acts.";
      case "goal-terminal-only":
        return "Goal budgets and quality gates are set from the terminal: run `borg`, then /goal.";
      case "goal-show":
        return describeGoal(await borg.admin.getGoal({ sessionId }));
      case "goal-clear":
        await borg.admin.clearGoal({ sessionId });
        return "Goal cleared.";
      case "goal-set": {
        const goal = await borg.admin.setGoal({
          sessionId,
          op: admin.GoalMutation_Op.OP_REPLACE,
          objective: cmd.objective,
          tokenBudgetPresent: false,
          tokenBudget: 0,
          status: "",
        });
        const stop = /[.!?]$/.test(goal.objective) ? "" : ".";
        return `Goal set: ${goal.objective}${stop} Borg works toward it after each of your messages.`;
      }
      case "goal-status": {
        // The daemon refuses a status change with no goal, in its own words.
        const current = await borg.admin.getGoal({ sessionId });
        if (!current.present)
          return `There is no goal to ${cmd.status === "paused" ? "pause" : "resume"}.`;
        const goal = await borg.admin.setGoal({
          sessionId,
          op: admin.GoalMutation_Op.OP_SET_STATUS,
          objective: "",
          tokenBudgetPresent: false,
          tokenBudget: 0,
          status: cmd.status,
        });
        return `Goal ${cmd.status === "paused" ? "paused" : "resumed"}: ${goal.objective}`;
      }
      case "heartbeat":
        return describeHeartbeat(
          cmd.op,
          await borg.admin.mutateSessionHeartbeat({
            sessionId,
            op: cmd.op,
            prompt: cmd.prompt,
            intervalSecs: cmd.intervalSecs,
          }),
        );
    }
  } catch (e) {
    const detail = e instanceof Error ? e.message : String(e);
    return `That did not work: ${detail}`;
  }
}
