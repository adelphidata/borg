import { useCallback, useEffect, useRef, useState } from "react";
import type { BorgError, session as sessionPb } from "@borg/client";
import {
  asBorgError,
  useBorg,
  useMutation,
  useQuery,
  useRetryingServerStream,
  type QueryResult,
} from "../../lib/useDaemon";
import { splitAttachments } from "./attachments";
import { draftKeyFor, moveDraft } from "./draftStore";

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

/** One rendered block in the transcript. */
export type Cell =
  | { kind: "user"; text: string; attachments?: number }
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
    }
  | { kind: "usage"; input: number; output: number; cacheRead: number; cacheWrite: number }
  | { kind: "sub-agent"; agentId: string; nickname: string; status: string }
  | { kind: "notice"; text: string }
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

function appendAssistant(cells: Cell[], text: string): Cell[] {
  const last = cells[cells.length - 1];
  if (last && last.kind === "assistant") {
    return [...cells.slice(0, -1), { ...last, text: last.text + text }];
  }
  return [...cells, { kind: "assistant", text }];
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
  return [...cells, { kind: "thinking", text, streaming: true, startedAt: now }];
}

function patchTool(cells: Cell[], id: string, patch: (c: Cell & { kind: "tool" }) => Cell): Cell[] {
  return cells.map((c) => (c.kind === "tool" && c.id === id ? patch(c) : c));
}

/**
 * Folds one streamed AgentEvent into the transcript. Exported for tests.
 *
 * `now` is injected so the thinking timer is testable without a clock.
 */
export function reduceEvent(cells: Cell[], event: sessionPb.AgentEvent, now = Date.now()): Cell[] {
  if (event.thinkingDelta) return appendThinking(cells, event.thinkingDelta.text, now);

  // Everything below ends a reasoning block if one was open.
  const base = sealThinking(cells, now);

  if (event.textDelta) return appendAssistant(base, event.textDelta.text);
  if (event.toolExecuting) {
    return [
      ...base,
      {
        kind: "tool",
        id: event.toolExecuting.toolCallId,
        name: event.toolExecuting.name,
        args: event.toolExecuting.argsJson,
        output: "",
        done: false,
        stderr: false,
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
    const { toolCallId, result } = event.toolResult;
    return patchTool(base, toolCallId, (c) => ({ ...c, output: result, done: true }));
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
  if (event.steerReceived) return [...base, { kind: "user", text: event.steerReceived.text }];
  if (event.historyCompacted) {
    return [...base, { kind: "notice", text: "Older messages were compacted to save space." }];
  }
  if (event.systemNotice) return [...base, { kind: "notice", text: event.systemNotice.text }];
  if (event.error) return [...base, { kind: "error", text: event.error.message }];
  // preparing / turnComplete / planUpdated add nothing here — planUpdated
  // drives the pinned plan panel, the other two only move `busy`.
  return base;
}

/** One row of persisted history, as `Admin.GetSession` returns it. */
export interface HistoryRow {
  role: string;
  text: string;
  /** Assistant rows: the serialized tool calls this turn issued. */
  toolCallsJson?: string;
  /** Tool rows: which call this row answers. */
  toolCallId?: string;
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

/**
 * Turns persisted history rows into transcript cells.
 *
 * A reloaded conversation should look like the live one it replaces. Tool
 * traffic is stored as an assistant row carrying the calls plus one `tool` row
 * per result, and only the pair names a call — a tool row on its own has an id
 * and nothing else. So the calls are carried forward in a map and joined as
 * their results arrive.
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
      if (m.text.trim() !== "") cells.push({ kind: "assistant", text: m.text });
      return;
    }

    if (m.role === "user") {
      if (m.text.trim() !== "") cells.push({ kind: "user", text: m.text });
      return;
    }

    if (m.role === "tool") {
      const call = m.toolCallId ? pending.get(m.toolCallId) : undefined;
      cells.push({
        kind: "tool",
        id: m.toolCallId || `history-${i}`,
        name: call?.name ?? "tool",
        args: call?.args ?? "",
        output: m.text,
        done: true,
        stderr: false,
      });
    }
  });

  return cells;
}

export interface Conversation {
  /** Open + state + history, routed through DaemonState by the page. */
  opened: QueryResult<{ title: string; model: string; agentSessionId: string }>;
  /**
   * The live daemon session, undefined until it opens. Distinct from
   * `agentSessionId`, which stays empty on a conversation nobody has sent to
   * yet — this one exists from the moment the page opens, which is what a
   * workspace-scoped call such as path completion needs.
   */
  sessionId: string | undefined;
  cells: Cell[];
  /** Current todo list, empty when the agent has not published one. */
  plan: PlanStep[];
  /** True while a turn is running. */
  busy: boolean;
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
  /** Mid-turn nudge. Only meaningful while `busy`. */
  steer: (text: string) => void;
  cancel: () => void;
  respond: (promptId: string, value: string) => void;
  /**
   * Rewrites the nth user message (0-based) and re-runs from there. Everything
   * after it is discarded, matching what the daemon does to its own history.
   */
  editAndResend: (nthUserMessage: number, text: string) => void;
  /** Summarizes older turns to reclaim context. */
  compact: () => Promise<unknown>;
  /** Branches this conversation; resolves with the new session's id. */
  fork: () => Promise<string | undefined>;
  /** How full the context window is, refreshed on demand. */
  context: { used: number; window: number } | undefined;
  refreshContext: () => void;
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
  const [plan, setPlan] = useState<PlanStep[]>([]);
  const [busy, setBusy] = useState(false);
  const [sendError, setSendError] = useState<BorgError>();
  const [streamError, setStreamError] = useState<BorgError>();
  /**
   * Highest event seq applied. Resuming from this — not from the seq captured
   * at open — is what stops a reconnect from replaying the whole turn.
   */
  const lastSeq = useRef(0);

  const opened = useQuery(
    async (borg) => {
      setSessionId(undefined);
      setCells([]);
      setPlan([]);
      setBusy(false);
      setSendError(undefined);
      setStreamError(undefined);

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
        model: state.llmModel,
        agentSessionId: state.agentSessionId,
      };
    },
    [target],
  );

  const state = useRetryingServerStream<sessionPb.AgentEvent>(
    (borg, handlers) => {
      if (!sessionId) return () => {};
      return borg.session.stream({ sessionId, sinceEventSeq: lastSeq.current }, handlers);
    },
    (event) => {
      lastSeq.current = Math.max(lastSeq.current, Number(event.eventSeq));
      setStreamError(undefined);
      if (event.turnComplete || event.error) setBusy(false);
      if (event.preparing) setBusy(true);
      if (event.planUpdated) setPlan(event.planUpdated.steps);
      setCells((prev) => reduceEvent(prev, event));
    },
    [sessionId],
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
    setStreamError(state.error ?? asBorgError("The connection to Borg ended."));
  }, [state.status, state.error, sessionId]);

  const send = useCallback(
    (text: string, attachments: Attachment[] = []) => {
      if (!sessionId) return;
      setCells((prev) => [
        ...prev,
        { kind: "user", text, attachments: attachments.length || undefined },
      ]);
      setBusy(true);
      setSendError(undefined);
      // The turn's events arrive on the Stream subscription above.
      borg.session.send(
        { sessionId, text, ...splitAttachments(attachments) },
        {
          onMessage: () => {},
          onError: (error) => {
            setSendError(asBorgError(error));
            setBusy(false);
          },
        },
      );
    },
    [borg, sessionId],
  );

  const steerMutation = useMutation((borg, text: string) =>
    sessionId ? borg.session.steer({ sessionId, text }) : Promise.resolve(undefined),
  );
  const cancelMutation = useMutation((borg) =>
    sessionId ? borg.session.cancel({ sessionId }) : Promise.resolve(undefined),
  );
  const respondMutation = useMutation((borg, vars: { promptId: string; value: string }) =>
    sessionId ? borg.session.respondToPrompt({ sessionId, ...vars }) : Promise.resolve(undefined),
  );

  const steer = useCallback(
    (text: string) => {
      // Deliberately NOT echoed locally: the daemon persists a steer as a real
      // user message and replays it as `steerReceived` on this same stream, so
      // a local echo renders the turn twice and — worse — shifts every later
      // user cell's ordinal away from the one `rewindToNthUserMessage` counts.
      void steerMutation.run(text);
    },
    [steerMutation.run],
  );
  const cancel = useCallback(() => void cancelMutation.run(undefined), [cancelMutation.run]);

  const [context, setContext] = useState<{ used: number; window: number }>();
  const contextMutation = useMutation(async (borg) => {
    if (!sessionId) return undefined;
    const breakdown = await borg.session.getContextBreakdown({ sessionId });
    // `detail` rows itemize a parent that already counts their tokens, so
    // including them would double-count and report an impossible total.
    const used = breakdown.components
      .filter((c) => !c.detail)
      .reduce((sum, c) => sum + Number(c.tokens), 0);
    setContext({ used, window: Number(breakdown.contextWindow) });
    return breakdown;
  });
  const refreshContext = useCallback(
    () => void contextMutation.run(undefined),
    [contextMutation.run],
  );

  const compactMutation = useMutation(
    (borg) => (sessionId ? borg.session.compact({ sessionId }) : Promise.resolve(undefined)),
    { onSuccess: () => refreshContext() },
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
        send(text);
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

  return {
    opened,
    sessionId,
    cells,
    plan,
    busy,
    sendError,
    streamError,
    streamOutageSustained: state.sustained,
    send,
    steer,
    cancel,
    respond,
    editAndResend,
    compact: () => compactMutation.run(undefined),
    fork: () => forkMutation.run(undefined).then((r) => r?.forkedAgentSessionId),
    context,
    refreshContext,
  };
}
