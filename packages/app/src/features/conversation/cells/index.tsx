import { memo, useState } from "react";
import { Brackets, Markdown, PencilIcon, Pill, StatusDot } from "@borg/ui";
import type { Cell } from "../useConversation";
import { classifyStatus } from "../../agents/useAgentTree";
import { FailureCard } from "../FailureCard";
import { InputRequestCell } from "./InputRequestCell";
import { ThinkingCell } from "./ThinkingCell";
import { formatElapsed, formatTimestamp, formatTokens } from "../../../lib/format";
import { KICKER } from "./styles";
import { ToolCell } from "./ToolCell";

/** Assistant reply in the bracketed-card style. */
function ReplyCard({ kicker, children }: { kicker: string; children: React.ReactNode }) {
  return (
    <div className="relative rounded-xl border border-line bg-surface2 p-5 pb-[18px] shadow-(--plateGlow)">
      <Brackets card />
      <div className={KICKER}>{kicker}</div>
      {children}
    </div>
  );
}

function ShellConfirmCell({
  cell,
  respond,
}: {
  cell: Cell & { kind: "shell-confirm" };
  respond: (promptId: string, value: string) => void;
}) {
  return (
    <div className="rounded-xl border border-line2 px-[18px] py-4">
      <div className={KICKER}>Borg wants to run a command</div>
      <pre className="m-0 text-[12.5px] leading-[1.5] whitespace-pre-wrap text-ink select-text">
        {cell.command}
      </pre>
      {!cell.answered && (
        <div className="mt-3.5 flex flex-wrap gap-2">
          <Pill variant="primary" square onClick={() => respond(cell.promptId, "true")}>
            Allow
          </Pill>
          <Pill square onClick={() => respond(cell.promptId, "false")}>
            Deny
          </Pill>
        </div>
      )}
    </div>
  );
}

/**
 * Per-turn time and token accounting, deliberately quiet.
 *
 * The elapsed time leads, because "how long did that take" is the question a
 * user asks without being taught to. Token counts are omitted when the turn
 * never reported any, so the line never claims a turn cost zero.
 */
function UsageCell({ cell }: { cell: Cell & { kind: "usage" } }) {
  const cached = cell.cacheRead + cell.cacheWrite;
  const tokens = cell.input + cell.output > 0;
  // Nothing to report is nothing to render: the transcript lays its cells out
  // with a gap, so an empty footer is a blank band with no line in it.
  if (cell.elapsedMs === undefined && !tokens) return null;
  return (
    <div className="px-1 text-[11px] leading-none text-ink3">
      {cell.elapsedMs !== undefined && formatElapsed(cell.elapsedMs)}
      {cell.elapsedMs !== undefined && tokens && " · "}
      {tokens && (
        <>
          {formatTokens(cell.input)} in · {formatTokens(cell.output)} out
          {cached > 0 && <> · {formatTokens(cached)} cached</>}
        </>
      )}
    </div>
  );
}

/**
 * A sub-agent's progress, inline.
 *
 * Nothing in the daemon emits `SubAgentUpdate` today, so this is unreachable in
 * practice; the activity panel is where sub-agents are actually watched. It
 * classifies its status the same way regardless, so the two can never disagree
 * about what a status means once the event does start arriving.
 */
function SubAgentCell({ cell }: { cell: Cell & { kind: "sub-agent" } }) {
  const liveness = classifyStatus(cell.status.toLowerCase());
  return (
    <div className="flex items-center gap-2 rounded-[10px] border border-line2 px-4 py-2.5">
      <StatusDot tone={liveness === "live" ? "busy" : liveness === "done" ? "idle" : "alert"} />
      <span className="text-[11px] leading-none font-medium tracking-[0.08em] text-ink3 uppercase">
        Agent
      </span>
      <span className="min-w-0 flex-1 truncate text-[12.5px] leading-none text-ink2">
        {cell.nickname || cell.agentId}
      </span>
      <span className="text-[11.5px] leading-none text-ink3">{cell.status}</span>
    </div>
  );
}

/**
 * A user turn, rewritable in place.
 *
 * Editing truncates the conversation from this message onward — the daemon
 * has no way to splice a turn in the middle — so the button says so before it
 * does it.
 */
function UserCell({
  cell,
  onEdit,
}: {
  cell: Cell & { kind: "user" };
  onEdit: ((text: string) => void) | undefined;
}) {
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(cell.text);

  if (editing) {
    return (
      <div className="flex flex-col items-end gap-2">
        <textarea
          autoFocus
          rows={Math.min(10, draft.split("\n").length + 1)}
          className="w-full resize-none rounded-[13px] border border-line bg-surface2 px-4 py-[11px] text-[13.5px] leading-[1.45] text-ink outline-none select-text"
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Escape") setEditing(false);
            if (e.key === "Enter" && !e.shiftKey) {
              e.preventDefault();
              if (draft.trim() !== "") {
                setEditing(false);
                onEdit?.(draft.trim());
              }
            }
          }}
        />
        <div className="flex items-center gap-2">
          <span className="text-[11px] leading-none text-ink3">
            Replies after this are discarded
          </span>
          <Pill square onClick={() => setEditing(false)}>
            Cancel
          </Pill>
          <Pill
            variant="primary"
            square
            onClick={() => {
              if (draft.trim() === "") return;
              setEditing(false);
              onEdit?.(draft.trim());
            }}
          >
            Resend
          </Pill>
        </div>
      </div>
    );
  }

  return (
    <div className="group flex items-start justify-end gap-1.5">
      {onEdit && (
        <button
          type="button"
          aria-label="Edit and resend"
          onClick={() => {
            setDraft(cell.text);
            setEditing(true);
          }}
          className="mt-2 flex h-6 w-6 flex-none cursor-pointer items-center justify-center rounded border-0 bg-transparent text-ink3 hover-reveal hover:text-ink"
        >
          <PencilIcon size={13} />
        </button>
      )}
      <div className="max-w-[78%] rounded-[13px] rounded-br-[4px] border border-line2 bg-surface3 px-4 py-[11px] text-[13.5px] leading-[1.45] whitespace-pre-wrap text-ink select-text">
        {cell.text}
        {cell.attachments !== undefined && (
          <div className="mt-1.5 text-[11px] leading-none text-ink3">
            {cell.attachments} image{cell.attachments === 1 ? "" : "s"} attached
          </div>
        )}
      </div>
    </div>
  );
}

/**
 * One row of the transcript.
 *
 * Memoised: the reducer replaces only the cell a stream event touches and
 * leaves the rest of the array identity-stable, so a token arriving mid-turn
 * re-renders one cell instead of the whole conversation. Callers must pass
 * stable `respond`/`onEdit` callbacks for this to hold.
 */
export const TranscriptCell = memo(function TranscriptCell({
  cell,
  respond,
  ordinal,
  editable,
  onEdit,
}: {
  cell: Cell;
  respond: (promptId: string, value: string) => void;
  /**
   * This cell's position among *user* cells, which is what the daemon rewinds
   * by. -1 for every other kind.
   */
  ordinal: number;
  /** False mid-turn, when rewriting a turn would race the one in flight. */
  editable: boolean;
  onEdit: (nthUserMessage: number, text: string) => void;
}) {
  const body = renderCell(cell, respond, ordinal, editable, onEdit);
  // Only the kinds the reducer stamps carry a time, so the presence of `at`
  // selects the rows worth asking "when" about — a usage footer or an open
  // prompt is not one of them. The wrapper is a plain block so the cell inside
  // lays out exactly as it did before.
  if (cell.at === undefined) return body;
  return <div title={formatTimestamp(cell.at)}>{body}</div>;
});

function renderCell(
  cell: Cell,
  respond: (promptId: string, value: string) => void,
  ordinal: number,
  editable: boolean,
  onEdit: (nthUserMessage: number, text: string) => void,
) {
  switch (cell.kind) {
    case "user":
      return (
        <UserCell cell={cell} onEdit={editable ? (text) => onEdit(ordinal, text) : undefined} />
      );
    case "assistant":
      return (
        <ReplyCard kicker="Borg">
          <Markdown>{cell.text}</Markdown>
        </ReplyCard>
      );
    case "thinking":
      return <ThinkingCell cell={cell} />;
    case "tool":
      return <ToolCell cell={cell} />;
    case "usage":
      return <UsageCell cell={cell} />;
    case "sub-agent":
      return <SubAgentCell cell={cell} />;
    case "notice":
      return <div className="text-center text-[11.5px] leading-[1.5] text-ink3">{cell.text}</div>;
    // No retry here, deliberately. A turn the daemon reported an error for is a
    // turn it has already written into the session's history, so re-sending the
    // message from the client would duplicate it there. The card explains and
    // hands back the raw text; re-running it is the composer's job.
    case "error":
      return <FailureCard error={{ message: cell.text }} />;
    case "shell-confirm":
      return <ShellConfirmCell cell={cell} respond={respond} />;
    case "input-request":
      return <InputRequestCell cell={cell} respond={respond} />;
  }
}
