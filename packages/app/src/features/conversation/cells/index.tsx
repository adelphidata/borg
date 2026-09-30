import { memo, useState } from "react";
import { Brackets, isSubmitEnter, Markdown, PencilIcon, Pill, StatusDot } from "@borg/ui";
import type { Cell, SentAttachment } from "../useConversation";
import { classifyStatus } from "../../agents/useAgentTree";
import { focusComposer } from "../composerFocus";
import { FailureCard } from "../FailureCard";
import { useAutoGrow } from "../useAutoGrow";
import { InputRequestCell } from "./InputRequestCell";
import { ThinkingCell } from "./ThinkingCell";
import { formatClock, formatElapsed, formatTimestamp, formatTokens } from "../../../lib/format";
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

/**
 * The local time a message was sent, under its bottom-right corner — the same
 * place for the user's messages and Borg's. Hover gives the full date.
 */
function SentAt({ at }: { at: number }) {
  return (
    <time
      dateTime={new Date(at).toISOString()}
      className="mt-1.5 block text-right text-[11px] leading-none text-ink3 tabular-nums"
    >
      {formatClock(at)}
    </time>
  );
}

function ShellConfirmCell({
  cell,
  respond,
}: {
  cell: Cell & { kind: "shell-confirm" };
  respond: (promptId: string, value: string) => void;
}) {
  // The pills unmount on answer, dropping focus on <body>; the composer is
  // where the next keystrokes belong.
  const answer = (value: string) => {
    respond(cell.promptId, value);
    focusComposer();
  };
  return (
    <div className="rounded-xl border border-line2 px-[18px] py-4">
      <div className={KICKER}>Borg wants to run a command</div>
      <pre className="m-0 text-[12.5px] leading-[1.5] whitespace-pre-wrap text-ink select-text">
        {cell.command}
      </pre>
      {!cell.answered && (
        <div className="mt-3.5 flex flex-wrap gap-2">
          <Pill variant="primary" square onClick={() => answer("true")}>
            Allow
          </Pill>
          <Pill square onClick={() => answer("false")}>
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

/** A long paste made from the clipboard, named by `pastedTextFile`. */
const PASTED_TEXT = /^pasted-text(-\d+)?\.txt$/;

/**
 * What went with the message, in as few words as a chip allows.
 *
 * Documents are named because the name is what distinguishes them; images are
 * counted because "IMG_4021.HEIC" tells nobody anything. A pasted block of text
 * is called what the user did, not what the file was named on its behalf.
 */
function AttachmentLine({ attachments }: { attachments: SentAttachment[] }) {
  const documents = attachments.filter((a) => a.kind === "document");
  const images = attachments.length - documents.length;
  return (
    <div className="mt-1.5 flex flex-wrap items-center gap-1.5 text-[11px] leading-none text-ink3">
      {documents.map((doc, i) => (
        <span
          key={`${doc.filename}:${i}`}
          className="rounded-md border border-line2 px-1.5 py-1 leading-none"
        >
          {PASTED_TEXT.test(doc.filename) ? "Pasted text" : doc.filename}
        </span>
      ))}
      {images > 0 && (
        <span>
          {images} image{images === 1 ? "" : "s"} attached
        </span>
      )}
    </div>
  );
}

/** Tall enough for a long rewrite; the composer stops at the same height. */
const EDIT_BOX_MAX_PX = 200;

/**
 * The rewrite box. Its own component so the auto-grow measures on the mount
 * of the textarea itself, not on the mount of the turn it belongs to.
 */
function EditBox({
  draft,
  onChange,
  onSubmit,
  onCancel,
}: {
  draft: string;
  onChange: (text: string) => void;
  onSubmit: () => void;
  onCancel: () => void;
}) {
  const box = useAutoGrow(draft, EDIT_BOX_MAX_PX);
  return (
    <textarea
      autoFocus
      ref={box}
      rows={1}
      className="w-full resize-none rounded-[13px] border border-line bg-surface2 px-4 py-[11px] text-[13.5px] leading-[1.45] text-ink outline-none select-text"
      value={draft}
      onChange={(e) => onChange(e.target.value)}
      onKeyDown={(e) => {
        if (e.key === "Escape") onCancel();
        // Never an input method's Enter: that one picks a character, and
        // reading it as a submit would rewind the conversation mid-word.
        if (isSubmitEnter(e) && !e.shiftKey) {
          e.preventDefault();
          onSubmit();
        }
      }}
    />
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
  const resend = () => {
    if (draft.trim() === "") return;
    setEditing(false);
    onEdit?.(draft.trim());
  };

  if (editing) {
    return (
      <div className="flex flex-col items-end gap-2">
        <EditBox
          draft={draft}
          onChange={setDraft}
          onSubmit={resend}
          onCancel={() => setEditing(false)}
        />
        <div className="flex items-center gap-2">
          <span className="text-[11px] leading-none text-ink3">
            Replies after this are discarded
          </span>
          <Pill square onClick={() => setEditing(false)}>
            Cancel
          </Pill>
          <Pill variant="primary" square onClick={resend}>
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
      <div className="max-w-[78%]">
        <div className="rounded-[13px] rounded-br-[4px] border border-line2 bg-surface3 px-4 py-[11px] text-[13.5px] leading-[1.45] whitespace-pre-wrap text-ink select-text">
          {/* A turn that was only its attachments has no text to make room for. */}
          {cell.text !== "" && <div>{cell.text}</div>}
          {cell.attachments && cell.attachments.length > 0 && (
            <AttachmentLine attachments={cell.attachments} />
          )}
        </div>
        {cell.at !== undefined && <SentAt at={cell.at} />}
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
        <div>
          <ReplyCard kicker="Borg">
            <Markdown>{cell.text}</Markdown>
          </ReplyCard>
          {cell.at !== undefined && <SentAt at={cell.at} />}
        </div>
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
