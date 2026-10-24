import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { BorgError, GrpcStatus, type session as sessionPb } from "@borg/client";
import {
  asBorgError,
  useBorg,
  useMutation,
  useQuery,
  useRetryingServerStream,
  type QueryResult,
} from "../../lib/useDaemon";
import { parseArgCommand, runArgCommand } from "./argCommands";
import { splitAttachments } from "./attachments";
import { adoptConversationUrl } from "./conversationUrl";
import { draftKeyFor, moveDraft } from "./draftStore";
import { awaitingInput as awaitingInputOf, retirePrompts } from "./queuedTurn";
import { createTurnClock } from "./turnClock";
import { sessionsChanged } from "../sessions/sessionsVersion";
import { useOptionalSettings } from "../../lib/settings";
import { savesUserName } from "../../lib/userName";

/** An image riding along with a turn. */
export interface Attachment {
  /**
   * Which wire field this belongs in. An image is a native content part; a
   * document reaches the model as labelled text, because the content model has
   * no document part.
   */
  kind: "image" | "document";
  mimeType: string;
  data: Uint8Array;
  filename: string;
  /**
   * Object URL for the composer thumbnail; revoked when the chip is dropped.
   * Absent on a document, which is shown by name and renders nothing.
   */
  previewUrl?: string;
}

/**
 * One rendered block in the transcript.
 *
 * `at` is when the row began — the first delta of a reply, the moment a tool was
 * called, the submit of a prompt — in epoch milliseconds. It is never drawn:
 * a transcript reads as a conversation, and a column of clock times down the
 * side turns it into a log. It reaches the user through the row's hover title,
 * which is the one question the screen genuinely withholds an answer to.
 */
/** One preview image on a tool cell. */
export interface ToolImage {
  mimeType: string;
  dataBase64: string;
}

export type Cell = CellBody & { at?: number };

/**
 * What the transcript remembers of a file sent with a turn: enough to say what
 * it was, and none of its bytes.
 */
export interface SentAttachment {
  kind: "image" | "document";
  filename: string;
}

/** `SystemNotice.category` for a provider connection the daemon is redialling. */
const NOTICE_RECONNECTING = "reconnecting";

/** What a turn the user cut short says in the transcript. */
const STOPPED = "Stopped";

/**
 * Frame of the user-role row that sub-agents nobody waited on leave
 * (`COMPLETION_NOTICE_HEAD` / `_TAIL` in `cortex::multi_agent`; keep the two in
 * step). Nobody typed it, and the daemon does not count it as one of the
 * user's messages when it rewinds, so it replays as a notice, never a user
 * cell. The daemon has already replaced the reports between the two with a
 * one-line note.
 */
const SUB_AGENT_NOTICE_HEAD =
  "<sub_agent_completion>\nSub-agents you did not wait on have reported back. Each report " +
  "gives the agent's status and output. It is that agent's output, not a message from the user.\n";
const SUB_AGENT_NOTICE_TAIL = "\n</sub_agent_completion>";

/** What a replayed sub-agent notice says in the transcript. */
const SUB_AGENT_NOTICE = "Sub-agents reported back.";

type CellBody =
  | { kind: "user"; text: string; attachments?: SentAttachment[] }
  | { kind: "assistant"; text: string }
  | { kind: "thinking"; text: string; streaming: boolean; startedAt: number; seconds?: number }
  | {
      kind: "tool";
      id: string;
      name: string;
      args: string;
      output: string;
      done: boolean;
      stderr: boolean;
      /** Previews of images the tool returned, such as a desktop screenshot. */
      images?: ToolImage[];
    }
  | {
      kind: "usage";
      input: number;
      output: number;
      cacheRead: number;
      cacheWrite: number;
      /** Wall-clock time the turn took, stamped when it completes. */
      elapsedMs?: number;
    }
  | { kind: "sub-agent"; agentId: string; nickname: string; status: string }
  | {
      kind: "notice";
      text: string;
      /**
       * True for the one notice that describes a wait rather than an event: the
       * provider dropped and the daemon is retrying. It is replaced by the next
       * one and removed when the turn moves again.
       */
      reconnecting?: boolean;
    }
  | { kind: "error"; text: string }
  | {
      kind: "shell-confirm";
      promptId: string;
      command: string;
      answered: boolean;
    }
  | {
      kind: "input-request";
      promptId: string;
      questions: sessionPb.UserInputQuestion[];
      answered: boolean;
    };

/** A todo/plan step, shown in a pinned panel rather than inline. */
export interface PlanStep {
  id: string;
  text: string;
  status: string;
}

/**
 * Drops a reply that holds only whitespace. A model often streams a newline or
 * two before its first tool call, then reports that call's usage; a reload
 * skips such a row, so live does too.
 */
function dropBlankAssistant(cells: Cell[]): Cell[] {
  let i = cells.length - 1;
  while (i >= 0 && cells[i]!.kind === "usage") i -= 1;
  const reply = cells[i];
  if (reply?.kind !== "assistant" || reply.text.trim() !== "") return cells;
  return [...cells.slice(0, i), ...cells.slice(i + 1)];
}

function appendAssistant(cells: Cell[], text: string, now: number): Cell[] {
  const last = cells[cells.length - 1];
  if (last && last.kind === "assistant") {
    return [...cells.slice(0, -1), { ...last, text: last.text + text }];
  }
  return [...cells, { kind: "assistant", text, at: now }];
}

/**
 * Removes the last `bytes` UTF-8 bytes of `text`.
 *
 * The daemon counts a retraction in UTF-8 bytes. A JavaScript string counts
 * UTF-16 units, so the cut is made on the encoded bytes. It moves down to a
 * character boundary, and a count larger than the text gives an empty string.
 */
export function retractTail(text: string, bytes: number): string {
  const encoded = new TextEncoder().encode(text);
  let cut = Math.max(0, encoded.length - bytes);
  // A continuation byte (10xxxxxx) is inside a character.
  while (cut > 0 && cut < encoded.length && (encoded[cut]! & 0xc0) === 0x80) cut -= 1;
  return new TextDecoder().decode(encoded.subarray(0, cut));
}

/**
 * Drops the streamed text of a failed LLM attempt from the assistant cell in
 * progress. The daemon sends the full response again after it.
 */
function retractAssistant(cells: Cell[], bytes: number): Cell[] {
  const last = cells[cells.length - 1];
  if (!last || last.kind !== "assistant") return cells;
  const text = retractTail(last.text, bytes);
  if (text === "") return cells.slice(0, -1);
  return [...cells.slice(0, -1), { ...last, text }];
}

/**
 * Closes any still-streaming reasoning block and stamps how long it ran.
 *
 * Called whenever a non-thinking event arrives: the daemon has no
 * "thinking finished" event, so the next thing to happen is the signal.
 */
function sealThinking(cells: Cell[], now: number): Cell[] {
  const last = cells[cells.length - 1];
  if (!last || last.kind !== "thinking" || !last.streaming) return cells;
  return [
    ...cells.slice(0, -1),
    { ...last, streaming: false, seconds: Math.round((now - last.startedAt) / 1000) },
  ];
}

function appendThinking(cells: Cell[], text: string, now: number): Cell[] {
  const last = cells[cells.length - 1];
  if (last && last.kind === "thinking" && last.streaming) {
    return [...cells.slice(0, -1), { ...last, text: last.text + text }];
  }
  return [...cells, { kind: "thinking", text, streaming: true, startedAt: now, at: now }];
}

/**
 * Patches the call an event is about: the newest one with this id that is
 * still running.
 *
 * Ids are the provider's, and some reuse them within a conversation. Matching
 * on the id alone would write the new call's output over every finished call
 * that once carried it.
 */
function patchTool(cells: Cell[], id: string, patch: (c: Cell & { kind: "tool" }) => Cell): Cell[] {
  for (let i = cells.length - 1; i >= 0; i -= 1) {
    const cell = cells[i]!;
    if (cell.kind === "tool" && cell.id === id && !cell.done) {
      return [...cells.slice(0, i), patch(cell), ...cells.slice(i + 1)];
    }
  }
  return cells;
}

/** Takes the retry notice down. The array is untouched when there is none. */
function dropReconnecting(cells: Cell[]): Cell[] {
  const isLive = (c: Cell) => c.kind === "notice" && c.reconnecting === true;
  return cells.some(isLive) ? cells.filter((c) => !isLive(c)) : cells;
}

/** True for an event that proves the turn is moving again, or is over. */
function endsReconnecting(event: sessionPb.AgentEvent): boolean {
  return Boolean(
    event.textDelta ||
    event.thinkingDelta ||
    event.toolExecuting ||
    event.toolOutputDelta ||
    event.toolResult ||
    event.turnComplete ||
    event.error ||
    event.systemNotice?.category === NOTICE_RECONNECTING,
  );
}

/**
 * Folds one streamed AgentEvent into the transcript. Exported for tests.
 *
 * `now` is injected so the thinking timer is testable without a clock.
 */
export function reduceEvent(
  transcript: Cell[],
  event: sessionPb.AgentEvent,
  now = Date.now(),
): Cell[] {
  // A retry notice describes a wait, so it lasts only as long as the wait does.
  const cells = endsReconnecting(event) ? dropReconnecting(transcript) : transcript;
  if (event.thinkingDelta) return appendThinking(cells, event.thinkingDelta.text, now);

  // Everything below ends a reasoning block if one was open.
  const base = sealThinking(cells, now);

  if (event.textRetracted) return retractAssistant(base, event.textRetracted.bytes);
  if (event.textDelta) {
    // A stream that fails part-way sends its marker as one last delta of its
    // own, `\n\n[stream interrupted: …]`, so it reads the same as on reload.
    const delta = event.textDelta.text;
    const marker = delta.startsWith("\n\n") && !delta.slice(2).includes("\n");
    const notice = marker ? interruptionNotice(delta) : undefined;
    if (notice) return [...base, { kind: "notice", text: notice, at: now }];
    return appendAssistant(base, delta, now);
  }
  if (event.toolExecuting) {
    return [
      ...dropBlankAssistant(base),
      {
        kind: "tool",
        id: event.toolExecuting.toolCallId,
        name: event.toolExecuting.name,
        args: event.toolExecuting.argsJson,
        output: "",
        done: false,
        stderr: false,
        at: now,
      },
    ];
  }
  if (event.toolOutputDelta) {
    const { toolCallId, delta, isStderr } = event.toolOutputDelta;
    return patchTool(base, toolCallId, (c) => ({
      ...c,
      output: c.output + delta,
      stderr: c.stderr || isStderr,
    }));
  }
  if (event.toolResult) {
    const { toolCallId, result, images } = event.toolResult;
    const previews = images
      .filter((i) => i.dataBase64 !== "")
      .map((i) => ({ mimeType: i.mimeType, dataBase64: i.dataBase64 }));
    return patchTool(base, toolCallId, (c) => ({
      ...c,
      output: result,
      done: true,
      ...(previews.length ? { images: previews } : {}),
    }));
  }
  if (event.usage) {
    const { inputTokens, outputTokens, cacheReadTokens, cacheCreationTokens } = event.usage;
    const last = base[base.length - 1];
    // A turn can report usage more than once (one per model call); show the
    // turn's total rather than a run of near-identical lines.
    if (last && last.kind === "usage") {
      return [
        ...base.slice(0, -1),
        {
          ...last,
          input: last.input + Number(inputTokens),
          output: last.output + Number(outputTokens),
          cacheRead: last.cacheRead + Number(cacheReadTokens),
          cacheWrite: last.cacheWrite + Number(cacheCreationTokens),
        },
      ];
    }
    return [
      ...base,
      {
        kind: "usage",
        input: Number(inputTokens),
        output: Number(outputTokens),
        cacheRead: Number(cacheReadTokens),
        cacheWrite: Number(cacheCreationTokens),
      },
    ];
  }
  if (event.subAgentUpdate) {
    const { agentId, nickname, status } = event.subAgentUpdate;
    const existing = base.some((c) => c.kind === "sub-agent" && c.agentId === agentId);
    if (existing) {
      return base.map((c) =>
        c.kind === "sub-agent" && c.agentId === agentId ? { ...c, nickname, status } : c,
      );
    }
    return [...base, { kind: "sub-agent", agentId, nickname, status }];
  }
  if (event.shellConfirmation) {
    return [
      ...base,
      {
        kind: "shell-confirm",
        promptId: event.shellConfirmation.promptId,
        command: event.shellConfirmation.command,
        answered: false,
      },
    ];
  }
  if (event.userInputRequest) {
    return [
      ...base,
      {
        kind: "input-request",
        promptId: event.userInputRequest.promptId,
        questions: event.userInputRequest.questions,
        answered: false,
      },
    ];
  }
  if (event.steerReceived)
    return [...base, { kind: "user", text: event.steerReceived.text, at: now }];
  if (event.historyCompacted) {
    return [
      ...base,
      { kind: "notice", text: "Older messages were compacted to save space.", at: now },
    ];
  }
  if (event.systemNotice) {
    const { category, text } = event.systemNotice;
    if (category === NOTICE_RECONNECTING) {
      return [...base, { kind: "notice", text, reconnecting: true, at: now }];
    }
    return [...base, { kind: "notice", text, at: now }];
  }
  if (event.error) return [...base, { kind: "error", text: event.error.message, at: now }];
  // preparing / turnComplete / planUpdated add nothing here — planUpdated
  // drives the pinned plan panel, the other two only move `busy`.
  return base;
}

/**
 * Records how long the turn that just ended took.
 *
 * The time joins the footer the turn already has rather than arriving as a
 * badge of its own — one quiet line per turn, not two. A turn whose usage never
 * reported (a cancel, an error, a provider that omits it) grows the footer for
 * the time alone, and `UsageCell` then prints no token counts.
 *
 * Only a footer belonging to *this* turn is stamped: anything after the last
 * user cell is this turn's, and an earlier one is left alone.
 */
export function stampTurnDuration(cells: Cell[], elapsedMs: number): Cell[] {
  const lastIndexOf = (kind: Cell["kind"]) => {
    for (let i = cells.length - 1; i >= 0; i -= 1) if (cells[i]?.kind === kind) return i;
    return -1;
  };
  const turnStart = lastIndexOf("user");
  const at = lastIndexOf("usage");
  if (at > turnStart) return cells.map((c, i) => (i === at ? { ...c, elapsedMs } : c));
  // Nothing came back at all — a send that failed before the agent spoke. Time
  // spent waiting for nothing is not worth a line of its own.
  if (turnStart === cells.length - 1) return cells;
  return [...cells, { kind: "usage", input: 0, output: 0, cacheRead: 0, cacheWrite: 0, elapsedMs }];
}

/**
 * The message a retry may re-run, if any.
 *
 * A retry is only ever offered for a turn that produced *nothing*: the user's
 * message is still the last thing in the transcript, so sending it again adds
 * one turn where there were none. That narrowness is the whole point. Once the
 * daemon has answered — even half a sentence, even only to report the error —
 * it has already written the turn into the session's history, and a client-side
 * "retry" would send the same message a second time. The transcript would look
 * clean while the persisted conversation held the message twice, and the model
 * would read both. Recovering *that* is a rewind or a resume, which is the
 * daemon's job and not this button's.
 *
 * "Produced nothing" is read off client state, so it is strong evidence rather
 * than proof: a send that failed *after* the daemon accepted the turn, on a
 * connection too dead to deliver the events, still looks like this. That window
 * is the residue. Closing it means asking the daemon what it has, which is the
 * rewind this deliberately is not.
 *
 * Attachments also close the door: the transcript keeps their names, not their
 * bytes, so a resend would carry none while the cell above went on claiming
 * they were sent.
 */
export function retryTarget(cells: Cell[]): string | undefined {
  const last = cells[cells.length - 1];
  if (!last || last.kind !== "user" || last.attachments?.length) return undefined;
  return last.text;
}

/** One row of persisted history, as `Admin.GetSession` returns it. */
export interface HistoryRow {
  role: string;
  text: string;
  /** Assistant rows: the serialized tool calls this turn issued. */
  toolCallsJson?: string;
  /** Tool rows: which call this row answers. */
  toolCallId?: string;
  /**
   * When the row was written, in epoch *seconds* — the daemon's own unit. Zero
   * on rows persisted before it was recorded, which is why the cell keeps `at`
   * optional rather than inventing a time.
   */
  unixTs?: number;
}

/** The `{id, function: {name, arguments}}` shape stored in `tool_calls_json`. */
interface StoredToolCall {
  id?: string;
  function?: { name?: string; arguments?: string };
}

function parseToolCalls(json: string | undefined): StoredToolCall[] {
  if (!json || json.trim() === "") return [];
  try {
    const parsed: unknown = JSON.parse(json);
    return Array.isArray(parsed) ? (parsed as StoredToolCall[]) : [];
  } catch {
    return [];
  }
}

/** What a reply that failed part-way says in the transcript. */
const CUT_SHORT = "Reply cut short";

/** The marker the daemon stores on a reply the user stopped. */
const USER_STOP_MARKER = "[response interrupted]";

/**
 * How every "this turn did not finish" marker the daemon writes begins. Only
 * the exact user-stop marker is a stop; the rest name a stream that failed or
 * tool calls it had to throw away, sometimes wrapped as
 * `[response interrupted — [stream interrupted: …]]`.
 */
const INTERRUPTION_PREFIXES = [
  "[response interrupted",
  "[stream interrupted",
  "[incomplete tool calls discarded",
];

/** The notice an interruption marker line stands for, or undefined for prose. */
function interruptionNotice(line: string): string | undefined {
  const marker = line.trim();
  if (!marker.endsWith("]") || !INTERRUPTION_PREFIXES.some((p) => marker.startsWith(p))) {
    return undefined;
  }
  return marker === USER_STOP_MARKER ? STOPPED : CUT_SHORT;
}

/**
 * Splits the daemon's "this reply did not finish" marker off a stored reply.
 *
 * The daemon appends the marker as the last line of what it persists, as a
 * record for itself. On screen it is a bracketed string the user never wrote
 * and the model never said, so it becomes a notice instead.
 */
function splitInterruption(text: string): { text: string; notice?: string } {
  const trimmed = text.trimEnd();
  const lineStart = trimmed.lastIndexOf("\n") + 1;
  const notice = interruptionNotice(trimmed.slice(lineStart));
  if (!notice) return { text };
  return { text: trimmed.slice(0, lineStart).trimEnd(), notice };
}

/**
 * The daemon's boundary tag names, whose closing tags it escapes inside a
 * stored tool result so the output cannot close its own wrapper.
 */
const BOUNDARY_CLOSE =
  /&lt;\/(tool_output|system_instructions|long_term_memory|working_memory|user_memory|memory_file|compaction_summary|mcp_tool_result|untrusted_content|prior_run_output)&gt;/gi;

/** `<tool_output name="…" trust="external">\n…\n</tool_output>`, whole. */
const TOOL_OUTPUT_WRAPPER = /^<tool_output(?:\s[^>]*)?>\n?([\s\S]*?)\n?<\/tool_output>$/;

/**
 * A stored tool result as the live stream showed it.
 *
 * The daemon stores a result wrapped for the model — the wrapper marks it as
 * untrusted — and escapes any boundary closing tags inside it; the live event
 * carries the text bare. Undoing both is what lets the row read its outcome,
 * and keeps markup the user never saw out of the transcript. A result stored
 * without the wrapper (a cancelled or crashed call) comes back unchanged.
 */
function unwrapToolOutput(text: string): string {
  const wrapped = TOOL_OUTPUT_WRAPPER.exec(text.trim());
  if (!wrapped) return text;
  return wrapped[1]!.replace(BOUNDARY_CLOSE, "</$1>");
}

/**
 * Turns persisted history rows into transcript cells.
 *
 * A reloaded conversation should look like the live one it replaces. Tool
 * traffic is stored as an assistant row carrying the calls plus one `tool` row
 * per result, and only the pair names a call — a tool row on its own has an id
 * and nothing else. So the calls are carried forward in a map and joined as
 * their results arrive.
 *
 * A turn that did not finish says so: the marker the daemon stores on a
 * cut-off reply becomes a notice — "Stopped" when the user stopped it, "Reply
 * cut short" when something failed — and a cancelled call keeps its marker as
 * its output, which is what the tool row reads its state from. A tool result
 * loses the wrapper it was stored in, so the row reads the text the live
 * stream carried.
 *
 * Two things genuinely cannot come back: reasoning blocks, which the daemon
 * never persists, and per-turn usage. System rows are skipped because that
 * role doubles as the carrier for compaction checkpoints, whose payload is a
 * whole summary rather than a notice.
 */
export function historyCells(messages: HistoryRow[]): Cell[] {
  const pending = new Map<string, { name: string; args: string }>();
  const cells: Cell[] = [];

  messages.forEach((m, i) => {
    // Seconds on the wire, milliseconds everywhere in this module. A zero means
    // the row predates the daemon recording it, and carries no time at all.
    const at = m.unixTs ? m.unixTs * 1000 : undefined;

    if (m.role === "assistant") {
      for (const call of parseToolCalls(m.toolCallsJson)) {
        if (call.id) {
          pending.set(call.id, {
            name: call.function?.name ?? "tool",
            args: call.function?.arguments ?? "",
          });
        }
      }
      // An assistant row can be tool calls only, with no prose to show.
      const reply = splitInterruption(m.text);
      if (reply.text.trim() !== "") cells.push({ kind: "assistant", text: reply.text, at });
      if (reply.notice) cells.push({ kind: "notice", text: reply.notice, at });
      return;
    }

    if (m.role === "user") {
      if (m.text.startsWith(SUB_AGENT_NOTICE_HEAD) && m.text.endsWith(SUB_AGENT_NOTICE_TAIL)) {
        cells.push({ kind: "notice", text: SUB_AGENT_NOTICE, at });
        return;
      }
      if (m.text.trim() !== "") cells.push({ kind: "user", text: m.text, at });
      return;
    }

    if (m.role === "tool") {
      const call = m.toolCallId ? pending.get(m.toolCallId) : undefined;
      cells.push({
        kind: "tool",
        id: m.toolCallId || `history-${i}`,
        name: call?.name ?? "tool",
        args: call?.args ?? "",
        output: unwrapToolOutput(m.text),
        done: true,
        stderr: false,
        at,
      });
    }
  });

  return cells;
}

/**
 * Total context tokens in use.
 *
 * `detail` rows itemize a parent that already counts their tokens, so including
 * them would double-count and report an impossible total.
 */
export function contextUsed(
  components: readonly { tokens: bigint | number; detail: boolean }[],
): number {
  return components.filter((c) => !c.detail).reduce((sum, c) => sum + Number(c.tokens), 0);
}

export interface Conversation {
  /** Open + state + history, routed through DaemonState by the page. */
  opened: QueryResult<{ title: string; agentSessionId: string }>;
  /**
   * The live daemon session, undefined until it opens. Distinct from
   * `agentSessionId`, which stays empty on a conversation nobody has sent to
   * yet — this one exists from the moment the page opens, which is what a
   * workspace-scoped call such as path completion needs.
   */
  sessionId: string | undefined;
  /** True once the session is open, which is when a send can go anywhere. */
  ready: boolean;
  cells: Cell[];
  /** Current todo list, empty when the agent has not published one. */
  plan: PlanStep[];
  /** True while a turn is running. */
  busy: boolean;
  /**
   * True while the turn is parked on a prompt only the user can answer. The
   * composer queues what it is given instead of steering it into a channel
   * nobody is draining.
   */
  awaitingInput: boolean;
  /**
   * When the turn in flight started, for a live elapsed counter. Undefined
   * between turns.
   */
  turnStartedAt: number | undefined;
  sendError: BorgError | undefined;
  /** Set when the event stream dropped; the hook retries on its own. */
  streamError: BorgError | undefined;
  /**
   * True when the retries have gone on long enough that the daemon is probably
   * down rather than blipping — the point at which the notice should stop
   * saying "reconnecting" and tell the user what to check.
   */
  streamOutageSustained: boolean;
  send: (text: string, attachments?: Attachment[]) => void;
  /**
   * Runs `/goal …`, `/heartbeat …` or `/mode …` here and answers in the transcript.
   * False when the text is not such a command.
   */
  runCommand: (text: string) => boolean;
  /**
   * Runs the last user message again, in place of the turn that failed on it.
   * A no-op mid-turn, and when nothing has been sent yet.
   */
  retryLast: () => void;
  /**
   * Mid-turn nudge. Only meaningful while `busy`. Resolves false when the
   * daemon refused it or the call failed, so the caller can keep the text.
   */
  steer: (text: string) => Promise<boolean>;
  cancel: () => void;
  respond: (promptId: string, value: string) => void;
  /**
   * Rewrites the nth user message (0-based) and re-runs from there. Everything
   * after it is discarded, matching what the daemon does to its own history.
   */
  editAndResend: (nthUserMessage: number, text: string) => void;
  /** Gives the conversation a new name. False when it did not save. */
  rename: (title: string) => Promise<boolean>;
  /** Summarizes older turns to reclaim context. */
  compact: () => Promise<unknown>;
  /** Branches this conversation; resolves with the new session's id. */
  fork: () => Promise<string | undefined>;
  /** How full the context window is, refreshed on demand. */
  context: { used: number; window: number } | undefined;
  refreshContext: () => void;
}

/** What a compaction someone asked for did, in one line. */
export function compactNotice(beforeTokens: number, afterTokens: number): string {
  if (afterTokens >= beforeTokens) {
    return "Nothing to compact yet. The conversation is still short enough to keep whole.";
  }
  const fmt = (n: number) => n.toLocaleString("en-US");
  return `Compacted the conversation from about ${fmt(beforeTokens)} to ${fmt(afterTokens)} tokens.`;
}

/**
 * One conversation with the daemon.
 *
 * `target` picks the session: an agent session id resumes it, `"new"` starts
 * fresh, and `undefined` resumes the most recent (or starts fresh when none).
 *
 * Rendering listens to `Session.Stream` only; `Session.Send` starts the turn
 * and its duplicate per-turn stream is discarded. Both carry the same events
 * from the daemon's broadcast bus, so one consumer keeps ordering simple.
 */
export function useConversation(target: string | undefined): Conversation {
  const borg = useBorg();
  const [sessionId, setSessionId] = useState<string>();
  const [cells, setCells] = useState<Cell[]>([]);
  const awaitingInput = useMemo(() => awaitingInputOf(cells), [cells]);
  const [plan, setPlan] = useState<PlanStep[]>([]);
  const [busy, setBusy] = useState(false);
  const [sendError, setSendError] = useState<BorgError>();
  const [streamError, setStreamError] = useState<BorgError>();
  /**
   * Highest event seq applied. Resuming from this — not from the seq captured
   * at open — is what stops a reconnect from replaying the whole turn.
   */
  const lastSeq = useRef(0);
  /**
   * When the turn in flight started, or undefined between turns.
   *
   * Started at submit rather than at the first event: the wait for the provider
   * to answer is part of what the user sat through, and it is often most of it.
   * The ref is the source of truth — the stream callback reads it synchronously
   * — and the state mirror exists only so the page can render a live counter.
   */
  const clock = useRef(createTurnClock());
  const [turnStartedAt, setTurnStartedAt] = useState<number | undefined>(undefined);
  /** Read by callbacks that must see the current value, not the rendered one. */
  const busyRef = useRef(busy);
  busyRef.current = busy;
  const cellsRef = useRef(cells);
  cellsRef.current = cells;
  /**
   * The agent saves the user's name through `write_memory`, and the sidebar
   * reads it from the settings snapshot, which nothing else would renew
   * mid-conversation. The ids are those calls still running.
   */
  const refetchSettings = useRef<(() => void) | undefined>(undefined);
  const settingsStore = useOptionalSettings();
  refetchSettings.current = settingsStore?.settings.refetch;
  // `/mode` writes through the store, so the mode switch follows at once.
  const saveSetting = settingsStore?.save;
  const nameSaves = useRef(new Set<string>());
  /**
   * Counts opens. Work that outlives a render — a title read, a reopen —
   * compares it before writing, so an answer about the conversation just left
   * never lands on the one now on screen.
   */
  const generation = useRef(0);
  /** The id a session resumes by. Empty until somebody has sent to it. */
  const agentSessionId = useRef("");
  /** The title as of the last finished turn; the open's own title until then. */
  const [title, setTitle] = useState<string>();
  /** True between the user pressing stop and the turn reporting that it ended. */
  const stopRequested = useRef(false);
  /**
   * True once the daemon has no such session and opening it again did not
   * help. The stream stops redialling: there is nothing to dial.
   */
  const [gone, setGone] = useState(false);
  const reopening = useRef(false);
  /**
   * Set by a reopen; cleared by the first event after it, or by an outage that
   * is not a missing session (a restart starts with the daemon unreachable).
   * One try per outage, so a reopen that is missing at once still gives up.
   */
  const reopenSpent = useRef(false);
  /**
   * Counts reopens that worked. The daemon resumes a session under the same
   * live id, so the id alone cannot restart the stream or date an error.
   */
  const [reopens, setReopens] = useState(0);
  /**
   * The stream error last acted on. The stream's state outlives the session
   * it reports on by a render — the new session's subscription resets it a
   * commit later — so the same error object seen again is old news.
   */
  const handledError = useRef<BorgError | undefined>(undefined);
  /**
   * Which subscription each stream error was raised for: the open, the reopen
   * and the live id. While a reopen is in flight the retry loop still dials
   * and collects fresh NotFounds; once the reopen lands, those are old news —
   * even under the same live id.
   */
  const errorSession = useRef(new WeakMap<BorgError, string>());
  const subscriptionKey = (id: string, reopen: number) => `${generation.current}:${reopen}:${id}`;
  /**
   * Set by a send while the conversation has no agent session id yet, cleared
   * by the next event, which asks the daemon for it. A daemon restart during
   * the very first turn otherwise has nothing to reopen by.
   */
  const learnAgentId = useRef(false);
  const startTurnClock = useCallback((at: number = Date.now()) => {
    clock.current.start(at);
    setTurnStartedAt(clock.current.startedAt());
  }, []);
  const armTurnClock = useCallback((at: number = Date.now()) => {
    clock.current.arm(at);
    setTurnStartedAt(clock.current.startedAt());
  }, []);
  const stopTurnClock = useCallback((at: number = Date.now()) => {
    const elapsedMs = clock.current.stop(at);
    setTurnStartedAt(undefined);
    return elapsedMs;
  }, []);

  /**
   * Reads the title again once a turn has ended.
   *
   * The daemon names a conversation after its first exchange, which is after
   * the open that fetched the title. One call per turn end, and a failure costs
   * nothing but a stale heading.
   */
  const refreshTitle = () => {
    if (!sessionId) return;
    const forGeneration = generation.current;
    borg.session
      .getState({ sessionId })
      .then((live) => {
        if (forGeneration !== generation.current) return;
        if (live.agentSessionId) learnedAgentSessionId(live.agentSessionId);
        if (live.title) setTitle(live.title);
      })
      .catch((error: unknown) => console.warn("reading the conversation title failed", error));
  };

  /**
   * Asks the daemon, once, which agent session the first turn created.
   *
   * A conversation opened at "new" has no agent session id until somebody
   * sends, and the turn-end title read that would learn it comes too late for a
   * daemon that restarts mid-turn: the reopen would have nothing to resume by.
   */
  const learnedAgentSessionId = (id: string) => {
    agentSessionId.current = id;
    if (target === "new") adoptConversationUrl(id);
  };

  const learnAgentSessionId = () => {
    if (!sessionId || agentSessionId.current) return;
    const forGeneration = generation.current;
    borg.session
      .getState({ sessionId })
      .then((live) => {
        if (forGeneration !== generation.current || agentSessionId.current) return;
        learnedAgentSessionId(live.agentSessionId);
      })
      .catch((error: unknown) => console.warn("reading the conversation id failed", error));
  };

  const opened = useQuery(
    async (borg) => {
      setSessionId(undefined);
      setCells([]);
      setPlan([]);
      setBusy(false);
      setSendError(undefined);
      setStreamError(undefined);
      setContext(undefined);
      setTitle(undefined);
      setGone(false);
      stopTurnClock();
      generation.current += 1;
      agentSessionId.current = "";
      stopRequested.current = false;
      reopenSpent.current = false;
      learnAgentId.current = false;

      let resumeId = target === "new" ? undefined : target;
      if (resumeId === undefined && target === undefined) {
        const list = await borg.admin.listSessions({});
        resumeId = list.sessions[0]?.id;
      }

      const open = await borg.session.open(resumeId ? { resumeId } : {});
      const state = await borg.session.getState({ sessionId: open.sessionId });
      if (state.agentSessionId) {
        const history = await borg.admin.getSession({ id: state.agentSessionId });
        setCells(historyCells(history.messages));
      }

      lastSeq.current = Number(open.lastEventSeq);
      agentSessionId.current = state.agentSessionId;
      setSessionId(open.sessionId);
      setBusy(state.turnInFlight);
      // A conversation opened at `/c/new` has no id to file a draft under, so
      // the composer types into a key named for the route. Carry that text over
      // now the real id has landed, before anything re-reads it.
      //
      // This belongs here rather than in the page: `moveDraft` notifies the
      // session lists, and a store write during render would update a sibling
      // React had already rendered. It cannot run in an effect either — those
      // fire child-first, so the composer would be handed the new key a frame
      // before the text arrived under it, blanking the box mid-sentence.
      if (state.agentSessionId) {
        moveDraft(draftKeyFor(target ?? "latest"), draftKeyFor(state.agentSessionId));
      }
      // `agentSessionId` is the id the session lists key on. The page needs it
      // to file this conversation's draft where those lists will look.
      return {
        title: state.title,
        agentSessionId: state.agentSessionId,
      };
    },
    [target],
  );

  const state = useRetryingServerStream<sessionPb.AgentEvent>(
    (borg, handlers) => {
      if (!sessionId || gone) return () => {};
      const key = subscriptionKey(sessionId, reopens);
      return borg.session.stream(
        { sessionId, sinceEventSeq: lastSeq.current },
        {
          ...handlers,
          onError: (error) => {
            const tagged = asBorgError(error);
            errorSession.current.set(tagged, key);
            handlers.onError?.(tagged);
          },
        },
      );
    },
    (event) => {
      // A reopen replaces the transcript with the stored history and rewinds
      // `lastSeq` to the reopen's seq. An event applied meanwhile would be
      // wiped by that snapshot; dropped here, the resubscribe replays it.
      if (reopening.current) return;
      lastSeq.current = Math.max(lastSeq.current, Number(event.eventSeq));
      if (learnAgentId.current) {
        learnAgentId.current = false;
        learnAgentSessionId();
      }
      // An event is proof the session exists, so a later loss earns a new try.
      reopenSpent.current = false;
      setStreamError(undefined);
      if (event.turnComplete || event.error) setBusy(false);
      // A turn the daemon started on its own — a resumed one, or a scheduled
      // wake — has no submit to arm the clock, so the first event does it. So
      // does a turn that a steer taken at its last check keeps going past the
      // `turnComplete` already sent: its next model call starts with
      // `preparing`. A bare steer echo is not enough — a stopped turn replays
      // what it had queued after its `turnComplete`, and nothing follows it.
      if (event.preparing) {
        setBusy(true);
        armTurnClock();
      }
      if (event.planUpdated) setPlan(event.planUpdated.steps);
      if (
        event.toolExecuting &&
        savesUserName(event.toolExecuting.name, event.toolExecuting.argsJson)
      ) {
        nameSaves.current.add(event.toolExecuting.toolCallId);
      }
      if (event.toolResult && nameSaves.current.delete(event.toolResult.toolCallId)) {
        refetchSettings.current?.();
      }
      setCells((prev) => reduceEvent(prev, event));
      if (event.turnComplete && stopRequested.current) {
        setCells((prev) => [...prev, { kind: "notice", text: STOPPED, at: Date.now() }]);
      }
      if (event.turnComplete || event.error) {
        stopRequested.current = false;
        if (event.turnComplete) refreshTitle();
        // The turn that raised any open prompt is over, so retire them: a cell
        // left waiting keeps the composer queueing instead of steering.
        setCells(retirePrompts);
        const elapsedMs = stopTurnClock();
        if (elapsedMs !== undefined) {
          setCells((prev) => stampTurnDuration(prev, elapsedMs));
        }
      }
    },
    [sessionId, gone, reopens],
  );

  // A dropped stream leaves the transcript frozen with no explanation. Surface
  // it; `useRetryingServerStream` schedules the resubscribe, and resuming from
  // `lastSeq` means nothing is missed or doubled.
  //
  // A clean `closed` counts as dropped: a turn stream ends when the daemon goes
  // away as readily as when it errors, and the silent case is the worse one —
  // the reply simply stops mid-sentence with nothing on screen to say why.
  useEffect(() => {
    if (state.status === "open" || !sessionId) return;
    // A missing session is not a lost connection, and is handled below.
    if (state.error?.code === GrpcStatus.NotFound) return;
    // An idle session sends no event after a reopen, so this is what earns the
    // next restart its own try. A reopen that is missing at once never passes
    // through here, which is what stops that case from looping.
    if (state.error && !reopening.current) reopenSpent.current = false;
    setStreamError(state.error ?? asBorgError("The connection to Borg ended."));
  }, [state.status, state.error, sessionId]);

  // The daemon answers, but no longer has this session: it restarted, or the
  // conversation was archived from another surface. Redialling the same id can
  // never succeed, so open the conversation again instead.
  //
  // It resumes by the agent session id rather than re-running the open above.
  // For a conversation that began at "new" that query would start a second,
  // empty one and leave this transcript attached to nothing.
  useEffect(() => {
    if (state.error?.code !== GrpcStatus.NotFound || !sessionId || gone) return;
    if (reopening.current || handledError.current === state.error) return;
    // An error about a session this conversation has already left — raised by
    // the retry loop while a reopen was in flight — says nothing about now.
    if (errorSession.current.get(state.error) !== subscriptionKey(sessionId, reopens)) return;
    handledError.current = state.error;
    const forGeneration = generation.current;
    const current = () => forGeneration === generation.current;
    const giveUp = (error: BorgError) => {
      if (!current()) return;
      setGone(true);
      setStreamError(undefined);
      setSendError(
        error.code === GrpcStatus.NotFound
          ? new BorgError(
              GrpcStatus.NotFound,
              "This conversation is no longer open in Borg. Open it again from the conversation list.",
            )
          : error,
      );
      setBusy(false);
      stopTurnClock();
    };

    const resumeId = agentSessionId.current;
    // One try per outage, and nothing to resume by is the same dead end: a
    // fresh session would silently drop what is on screen.
    if (reopenSpent.current || (!resumeId && cellsRef.current.length > 0)) {
      giveUp(state.error);
      return;
    }
    reopenSpent.current = true;
    reopening.current = true;
    void (async () => {
      try {
        const open = await borg.session.open(resumeId ? { resumeId } : {});
        const live = await borg.session.getState({ sessionId: open.sessionId });
        // The session is back either way; a failed history read only means the
        // transcript on screen stays as it is, which beats giving up on it.
        const history = live.agentSessionId
          ? await borg.admin.getSession({ id: live.agentSessionId }).catch((error: unknown) => {
              console.warn("reloading the conversation history failed", error);
              return undefined;
            })
          : undefined;
        if (!current()) return;
        if (history) setCells(historyCells(history.messages));
        lastSeq.current = Number(open.lastEventSeq);
        agentSessionId.current = live.agentSessionId;
        stopRequested.current = false;
        setStreamError(undefined);
        setSessionId(open.sessionId);
        // The live id usually comes back unchanged, so the id alone would not
        // resubscribe: the stream would wait out its backoff timer instead.
        setReopens((n) => n + 1);
        setBusy(live.turnInFlight);
        if (live.turnInFlight) armTurnClock();
        else stopTurnClock();
      } catch (error) {
        giveUp(asBorgError(error));
      } finally {
        reopening.current = false;
      }
    })();
  }, [state.error, sessionId, gone, reopens, borg, armTurnClock, stopTurnClock]);

  const send = useCallback(
    // `startedAt` lets a caller that had work to do first — the rewind an
    // edit-and-resend waits on — bill that wait to the turn the user is
    // watching. `echo` is how a retry re-runs a turn whose user cell is already
    // on screen, without printing the same message twice. The public signature
    // takes two arguments; the rest are internal.
    (
      text: string,
      attachments: Attachment[] = [],
      startedAt: number = Date.now(),
      echo: boolean = true,
    ) => {
      if (!sessionId) return;
      if (echo) {
        const sent = attachments.map(({ kind, filename }) => ({ kind, filename }));
        setCells((prev) => [
          ...prev,
          { kind: "user", text, attachments: sent.length ? sent : undefined, at: startedAt },
        ]);
      }
      stopRequested.current = false;
      if (!agentSessionId.current) learnAgentId.current = true;
      setBusy(true);
      setSendError(undefined);
      startTurnClock(startedAt);
      // The turn's events arrive on the Stream subscription above.
      borg.session.send(
        { sessionId, text, ...splitAttachments(attachments) },
        {
          onMessage: () => {},
          onError: (error) => {
            setSendError(asBorgError(error));
            setBusy(false);
            stopTurnClock();
          },
        },
      );
    },
    [borg, sessionId, startTurnClock, stopTurnClock],
  );

  /**
   * Runs `/goal …`, `/heartbeat …` or `/mode …` against this conversation and answers
   * with a notice. False when `text` is not one of them, so the caller sends
   * it as an ordinary message.
   */
  const runCommand = useCallback(
    (text: string): boolean => {
      const cmd = parseArgCommand(text);
      if (!cmd || !sessionId) return false;
      const forGeneration = generation.current;
      const answer = (reply: string) => {
        if (forGeneration !== generation.current) return;
        setCells((prev) => [...prev, { kind: "notice", text: reply, at: Date.now() }]);
      };
      void (async () => {
        // Asked fresh rather than read from the ref: an empty id makes the
        // daemon pick its most recent session, which can be another
        // conversation entirely.
        const agentId = await borg.session.getState({ sessionId }).then(
          (s) => s.agentSessionId || agentSessionId.current,
          () => agentSessionId.current,
        );
        if (!agentId) {
          answer("Borg could not find this conversation. Try again in a moment.");
          return;
        }
        answer(await runArgCommand(borg, agentId, cmd, saveSetting));
      })();
      return true;
    },
    [borg, sessionId, saveSetting],
  );

  /**
   * Sends the last message again, when nothing came back the first time.
   *
   * `retryTarget` decides whether there is anything safe to re-run; see it for
   * why "nothing came back" is the only case. The user's cell is already on
   * screen and is not echoed again — they wrote the message once, and a retry
   * is the same message, not a new one.
   */
  const retryLast = useCallback(() => {
    if (busy || !sessionId) return;
    const text = retryTarget(cells);
    if (text === undefined) return;
    setSendError(undefined);
    send(text, [], Date.now(), false);
  }, [busy, cells, send, sessionId]);

  const steerMutation = useMutation(async (borg, text: string) => {
    if (!sessionId) return undefined;
    try {
      await borg.session.steer({ sessionId, text });
      return "steered" as const;
    } catch (e) {
      // The turn made its last check before this arrived. Not a failure: the
      // text is simply a turn of its own now.
      if (asBorgError(e).code === GrpcStatus.FailedPrecondition) return "turn-ended" as const;
      throw e;
    }
  });
  const cancelMutation = useMutation((borg) =>
    sessionId ? borg.session.cancel({ sessionId }) : Promise.resolve(undefined),
  );
  const respondMutation = useMutation((borg, vars: { promptId: string; value: string }) =>
    sessionId ? borg.session.respondToPrompt({ sessionId, ...vars }) : Promise.resolve(undefined),
  );

  const steer = useCallback(
    async (text: string) => {
      if (!sessionId) return false;
      // Deliberately NOT echoed locally: the daemon persists a steer as a real
      // user message and replays it as `steerReceived` on this same stream, so
      // a local echo renders the turn twice and — worse — shifts every later
      // user cell's ordinal away from the one `rewindToNthUserMessage` counts.
      //
      // `arm`, not `start`: a mid-turn steer must not restart the turn's clock.
      armTurnClock();
      const outcome = await steerMutation.run(text);
      // The daemon refuses a steer that reached it after the turn's last
      // check, rather than keep it where no turn reads it. Send it as the
      // next turn, which is what the user meant by pressing Enter.
      if (outcome === "turn-ended") send(text);
      // `run` reports any other refusal as undefined, and the error itself
      // reaches the transcript through the effect below.
      return outcome !== undefined;
    },
    [armTurnClock, send, sessionId, steerMutation.run],
  );
  const cancel = useCallback(() => {
    // A cancelled turn leaves nobody waiting on its prompts, so retire them —
    // live Allow/Deny pills wired to an abandoned prompt id are a dead end.
    setCells(retirePrompts);
    // Only a running turn can be stopped; the flag is what makes its end read
    // as "Stopped" rather than as a reply that simply ran out.
    stopRequested.current = busyRef.current;
    void cancelMutation.run(undefined).then((result) => {
      // A stop the daemon never heard did not stop anything.
      if (result === undefined) stopRequested.current = false;
    });
  }, [cancelMutation.run]);

  /**
   * The context meter, tagged with the session it describes.
   *
   * One ConversationPage instance serves every `/c/:id`, so switching
   * conversations changes `target` without remounting — and `getContextBreakdown`
   * for the session just left can resolve after the next one is on screen. The
   * tag is what stops that answer from rendering.
   */
  const [context, setContext] = useState<{
    sessionId: string;
    used: number;
    window: number;
  }>();
  const contextMutation = useMutation(async (borg) => {
    if (!sessionId) return undefined;
    const forSession = sessionId;
    const breakdown = await borg.session.getContextBreakdown({ sessionId: forSession });
    setContext({
      sessionId: forSession,
      used: contextUsed(breakdown.components),
      window: Number(breakdown.contextWindow),
    });
    return breakdown;
  });
  const refreshContext = useCallback(
    () => void contextMutation.run(undefined),
    [contextMutation.run],
  );

  const compactMutation = useMutation(
    (borg) => (sessionId ? borg.session.compact({ sessionId }) : Promise.resolve(undefined)),
    {
      onSuccess: (result) => {
        refreshContext();
        // The daemon raises no event for a compaction someone asked for; the
        // counts it answers with are the only word that it happened.
        if (result) {
          const text = compactNotice(Number(result.beforeTokens), Number(result.afterTokens));
          setCells((prev) => [...prev, { kind: "notice", text, at: Date.now() }]);
        }
      },
    },
  );
  const forkMutation = useMutation((borg) =>
    sessionId ? borg.session.fork({ sessionId }) : Promise.resolve(undefined),
  );
  const rewindMutation = useMutation((borg, n: number) =>
    sessionId ? borg.session.rewindToNthUserMessage({ sessionId, n }) : Promise.resolve(undefined),
  );

  const editAndResend = useCallback(
    (nthUserMessage: number, text: string) => {
      if (!sessionId) return;
      // The rewind round-trip is time the user spends watching a resent turn do
      // nothing, so the clock starts here rather than after it resolves.
      const startedAt = Date.now();
      // Rewind FIRST. `useMutation.run` reports failure as `undefined` rather
      // than rejecting, so resending unconditionally would append the rewritten
      // turn to a history the daemon never truncated — a duplicate turn on a
      // transcript the UI had already cut short. Only once the daemon confirms
      // is it safe to drop the cells and send.
      void rewindMutation.run(nthUserMessage).then((result) => {
        if (result === undefined) return;
        setCells((prev) => {
          let seen = 0;
          const cut = prev.findIndex((c) => c.kind === "user" && seen++ === nthUserMessage);
          return cut === -1 ? prev : prev.slice(0, cut);
        });
        send(text, [], startedAt);
      });
    },
    [rewindMutation.run, send, sessionId],
  );
  const respond = useCallback(
    (promptId: string, value: string) => {
      setCells((prev) =>
        prev.map((c) =>
          (c.kind === "shell-confirm" || c.kind === "input-request") && c.promptId === promptId
            ? { ...c, answered: true }
            : c,
        ),
      );
      void respondMutation.run({ promptId, value });
    },
    [respondMutation.run],
  );

  // Surface every side-channel failure in the transcript error slot; these
  // paths have no other place to report, and a silent no-op reads as a bug.
  useEffect(() => {
    const failure =
      respondMutation.error ??
      steerMutation.error ??
      compactMutation.error ??
      forkMutation.error ??
      rewindMutation.error;
    if (failure) setSendError(failure);
  }, [
    respondMutation.error,
    steerMutation.error,
    compactMutation.error,
    forkMutation.error,
    rewindMutation.error,
  ]);

  // The context meter is only meaningful once a session is open, and it should
  // reflect the turn that just finished rather than the one before it.
  useEffect(() => {
    if (sessionId && !busy) refreshContext();
  }, [sessionId, busy, refreshContext]);

  // Renaming is the one edit the title takes from the user; the sidebar lists
  // the same titles from a query of its own, so the shared counter renews it.
  const renameMutation = useMutation(
    (borg, vars: { id: string; title: string }) => borg.admin.setSessionTitle(vars),
    { onSuccess: () => sessionsChanged() },
  );
  const rename = useCallback(
    async (next: string): Promise<boolean> => {
      const id =
        agentSessionId.current || (opened.state === "ready" ? opened.data.agentSessionId : "");
      const title = next.trim();
      if (!id || title === "") return false;
      const ok = (await renameMutation.run({ id, title })) !== undefined;
      if (ok) setTitle(title);
      return ok;
    },
    [renameMutation.run, opened],
  );

  // The title the last turn left behind outranks what the open fetched.
  const titled = useMemo(
    () =>
      opened.state === "ready" && title !== undefined
        ? { ...opened, data: { ...opened.data, title } }
        : opened,
    [opened, title],
  );

  return {
    opened: titled,
    sessionId,
    ready: sessionId !== undefined,
    cells,
    plan,
    busy,
    awaitingInput,
    turnStartedAt,
    sendError,
    streamError,
    streamOutageSustained: state.sustained,
    send,
    runCommand,
    retryLast,
    steer,
    cancel,
    respond,
    editAndResend,
    rename,
    compact: () => compactMutation.run(undefined),
    fork: () => forkMutation.run(undefined).then((r) => r?.forkedAgentSessionId),
    // Never hand back a meter belonging to a session that is no longer shown.
    context:
      context && context.sessionId === sessionId
        ? { used: context.used, window: context.window }
        : undefined,
    refreshContext,
  };
}
