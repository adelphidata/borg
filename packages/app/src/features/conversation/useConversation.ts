import { useCallback, useEffect, useState } from "react";
import type { BorgError, session as sessionPb } from "@borg/client";
import {
  asBorgError,
  useBorg,
  useMutation,
  useQuery,
  useServerStream,
  type QueryResult,
} from "../../lib/useDaemon";

/** One rendered block in the transcript. */
export type Cell =
  | { kind: "user"; text: string }
  | { kind: "assistant"; text: string }
  | { kind: "thinking"; text: string }
  | { kind: "tool"; id: string; name: string; args: string; output: string; done: boolean }
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

function appendText(cells: Cell[], kind: "assistant" | "thinking", text: string): Cell[] {
  const last = cells[cells.length - 1];
  if (last && last.kind === kind) {
    return [...cells.slice(0, -1), { ...last, text: last.text + text }];
  }
  return [...cells, { kind, text }];
}

function patchTool(cells: Cell[], id: string, patch: (c: Cell & { kind: "tool" }) => Cell): Cell[] {
  return cells.map((c) => (c.kind === "tool" && c.id === id ? patch(c) : c));
}

/** Folds one streamed AgentEvent into the transcript. Exported for tests. */
export function reduceEvent(cells: Cell[], event: sessionPb.AgentEvent): Cell[] {
  if (event.textDelta) return appendText(cells, "assistant", event.textDelta.text);
  if (event.thinkingDelta) return appendText(cells, "thinking", event.thinkingDelta.text);
  if (event.toolExecuting) {
    return [
      ...cells,
      {
        kind: "tool",
        id: event.toolExecuting.toolCallId,
        name: event.toolExecuting.name,
        args: event.toolExecuting.argsJson,
        output: "",
        done: false,
      },
    ];
  }
  if (event.toolOutputDelta) {
    const { toolCallId, delta } = event.toolOutputDelta;
    return patchTool(cells, toolCallId, (c) => ({ ...c, output: c.output + delta }));
  }
  if (event.toolResult) {
    const { toolCallId, result } = event.toolResult;
    return patchTool(cells, toolCallId, (c) => ({ ...c, output: result, done: true }));
  }
  if (event.shellConfirmation) {
    return [
      ...cells,
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
      ...cells,
      {
        kind: "input-request",
        promptId: event.userInputRequest.promptId,
        questions: event.userInputRequest.questions,
        answered: false,
      },
    ];
  }
  if (event.steerReceived) return [...cells, { kind: "user", text: event.steerReceived.text }];
  if (event.historyCompacted) {
    return [...cells, { kind: "notice", text: "Older messages were compacted to save space." }];
  }
  if (event.systemNotice) return [...cells, { kind: "notice", text: event.systemNotice.text }];
  if (event.error) return [...cells, { kind: "error", text: event.error.message }];
  // Progress-only events (preparing, usage, plan, sub-agents, turnComplete)
  // add nothing to the transcript.
  return cells;
}

/** Turns persisted history rows into transcript cells. */
export function historyCells(messages: { role: string; text: string }[]): Cell[] {
  return messages.flatMap((m): Cell[] => {
    if (m.text.trim() === "") return [];
    if (m.role === "user") return [{ kind: "user", text: m.text }];
    if (m.role === "assistant") return [{ kind: "assistant", text: m.text }];
    return [];
  });
}

export interface Conversation {
  /** Open + state + history, routed through DaemonState by the page. */
  opened: QueryResult<{ title: string; model: string }>;
  cells: Cell[];
  /** True while a turn is running. */
  busy: boolean;
  sendError: BorgError | undefined;
  send: (text: string) => void;
  cancel: () => void;
  respond: (promptId: string, value: string) => void;
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
  const [sinceSeq, setSinceSeq] = useState(0);
  const [cells, setCells] = useState<Cell[]>([]);
  const [busy, setBusy] = useState(false);
  const [sendError, setSendError] = useState<BorgError>();

  const opened = useQuery(
    async (borg) => {
      setSessionId(undefined);
      setCells([]);
      setBusy(false);
      setSendError(undefined);

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

      setSessionId(open.sessionId);
      setSinceSeq(open.lastEventSeq);
      setBusy(state.turnInFlight);
      return { title: state.title, model: state.llmModel };
    },
    [target],
  );

  useServerStream<sessionPb.AgentEvent>(
    (borg, handlers) => {
      if (!sessionId) return () => {};
      return borg.session.stream({ sessionId, sinceEventSeq: sinceSeq }, handlers);
    },
    (event) => {
      if (event.turnComplete || event.error) setBusy(false);
      if (event.preparing) setBusy(true);
      setCells((prev) => reduceEvent(prev, event));
    },
    [sessionId],
  );

  const send = useCallback(
    (text: string) => {
      if (!sessionId) return;
      setCells((prev) => [...prev, { kind: "user", text }]);
      setBusy(true);
      setSendError(undefined);
      // The turn's events arrive on the Stream subscription above.
      borg.session.send(
        { sessionId, text },
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

  const cancelMutation = useMutation((borg) =>
    sessionId ? borg.session.cancel({ sessionId }) : Promise.resolve(undefined),
  );
  const respondMutation = useMutation((borg, vars: { promptId: string; value: string }) =>
    sessionId ? borg.session.respondToPrompt({ sessionId, ...vars }) : Promise.resolve(undefined),
  );

  const cancel = useCallback(() => void cancelMutation.run(undefined), [cancelMutation.run]);
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

  // Surface prompt-response failures in the transcript error slot.
  useEffect(() => {
    if (respondMutation.error) setSendError(respondMutation.error);
  }, [respondMutation.error]);

  return { opened, cells, busy, sendError, send, cancel, respond };
}
