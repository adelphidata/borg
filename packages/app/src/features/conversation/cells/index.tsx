import { memo, useId, useMemo, useState } from "react";
import {
  Brackets,
  InfoIcon,
  isSubmitEnter,
  Markdown,
  PencilIcon,
  Pill,
  SpeakerIcon,
  StatusDot,
  StopIcon,
} from "@borg/ui";
import type { Cell, SentAttachment } from "../useConversation";
import { useSpeaker } from "../../../lib/speech";
import { classifyStatus } from "../../agents/useAgentTree";
import { focusComposer } from "../composerFocus";
import { FailureCard } from "../FailureCard";
import { useAutoGrow } from "../../../lib/useAutoGrow";
import { InputRequestCell } from "./InputRequestCell";
import { ThinkingCell } from "./ThinkingCell";
import { formatClock, formatTimestamp } from "../../../lib/format";
import { KICKER } from "./styles";
import { ToolCell } from "./ToolCell";
import { parseUserTurn, type AttachedBlock } from "../userTurn";

/** Assistant reply in the bracketed-card style. */
function ReplyCard({
  kicker,
  action,
  children,
}: {
  kicker: string;
  /** A control in the card's top-right corner, shown on hover. */
  action?: React.ReactNode;
  children: React.ReactNode;
}) {
  return (
    <div className="group relative rounded-xl border border-line bg-surface2 p-5 pb-[18px] shadow-(--plateGlow)">
      <Brackets card />
      <div className={KICKER}>{kicker}</div>
      {action && <div className="absolute top-3 right-3">{action}</div>}
      {children}
    </div>
  );
}

/**
 * The id a reply is spoken under. Cells carry no id of their own; the send
 * time is unique per reply and stable once the turn has ended, which is the
 * only time anyone reads one aloud.
 */
export function replyId(cell: Cell & { kind: "assistant" }): string {
  return `reply:${cell.at ?? 0}:${cell.text.length}`;
}

/**
 * Reads a reply aloud, or stops it. Hidden until hover, like the edit pencil;
 * absent entirely where nothing on this device or in the daemon can speak.
 */
function SpeakButton({ cell }: { cell: Cell & { kind: "assistant" } }) {
  const [problem, setProblem] = useState<string>();
  const speaker = useSpeaker(setProblem);
  if (!speaker.canSpeak) return null;
  const id = replyId(cell);
  const active = speaker.state?.id === id;
  const loading = active && speaker.state?.phase === "loading";
  return (
    <span className="flex items-center gap-2">
      {problem && <span className="text-[11px] leading-none text-alert">{problem}</span>}
      <button
        type="button"
        aria-label={active ? "Stop reading" : "Read aloud"}
        aria-pressed={active}
        aria-busy={loading}
        title={active ? "Stop" : "Read aloud"}
        onClick={() => {
          setProblem(undefined);
          speaker.speak(id, cell.text);
        }}
        className={`flex h-6 w-6 cursor-pointer items-center justify-center rounded border-0 bg-transparent ${
          active ? "text-neon-ink" : "text-ink3 hover-reveal hover:text-ink"
        } ${loading ? "animate-pulse" : ""}`}
      >
        {active ? <StopIcon size={12} /> : <SpeakerIcon size={14} />}
      </button>
    </span>
  );
}

/**
 * A turn's time and tokens, behind an info mark that shows only on hover or
 * focus. The numbers are for the curious; in the transcript they were a line
 * under every reply that most people never read.
 *
 * `title=""` stops the row's own date tip from showing over this one. Escape
 * puts the tip away without moving the pointer or focus.
 */
function UsageInfo({ label, reveal }: { label: string; reveal: boolean }) {
  const [open, setOpen] = useState(false);
  const id = useId();
  return (
    <span
      title=""
      className={`relative inline-flex ${reveal ? "hover-reveal" : ""}`}
      onPointerEnter={() => setOpen(true)}
      onPointerLeave={() => setOpen(false)}
    >
      <button
        type="button"
        aria-label="Time and tokens"
        aria-describedby={id}
        onFocus={() => setOpen(true)}
        onBlur={() => setOpen(false)}
        onKeyDown={(e) => e.key === "Escape" && setOpen(false)}
        className="flex cursor-default items-center rounded-sm border-0 bg-transparent p-0 text-ink3 hover:text-ink2"
      >
        <InfoIcon size={12} />
      </button>
      <span
        role="tooltip"
        id={id}
        hidden={!open}
        className="absolute right-0 bottom-full z-30 mb-1.5 rounded-[8px] border border-line bg-surface2 px-2.5 py-1.5 text-[11px] leading-none whitespace-nowrap text-ink2 tabular-nums shadow-(--frameShadow)"
      >
        {label}
      </span>
    </span>
  );
}

/**
 * The local time a message was sent, under its bottom-right corner — the same
 * place for the user's messages and Borg's. Hover gives the full date, and a
 * reply that ends its turn carries the turn's usage beside it.
 */
function SentAt({ at, usage }: { at?: number; usage?: string }) {
  return (
    <div className="mt-1.5 flex items-center justify-end gap-1.5">
      {usage && <UsageInfo label={usage} reveal />}
      {at !== undefined && (
        <time
          dateTime={new Date(at).toISOString()}
          className="block text-[11px] leading-none text-ink3 tabular-nums"
        >
          {formatClock(at)}
        </time>
      )}
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
 * The usage of a turn that ended without a reply — it failed, or was stopped —
 * so the numbers have no timestamp to sit beside. Shown whole, not on hover:
 * there is no reply to hover. A usage cell whose turn has a reply renders
 * nothing; its numbers are on that reply.
 */
function UsageCell({ usage }: { usage: string | undefined }) {
  if (!usage) return null;
  return (
    <div className="flex justify-end px-1">
      <UsageInfo label={usage} reveal={false} />
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
      aria-label="Edit message"
      className="w-full resize-none rounded-[13px] border border-field bg-surface2 px-4 py-[11px] text-[13.5px] leading-[1.45] text-ink outline-none select-text focus:border-focus"
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
 * One block Borg attached for the model, folded behind its name and size.
 *
 * Closed by default: it is the brief the model read, not a message to the
 * user, and the reply right after it is where the answer is. Open, it reads as
 * the formatted list it already is rather than as tagged source.
 */
function AttachedSection({ block }: { block: AttachedBlock }) {
  return (
    <details className="mt-2.5 first:mt-0">
      <summary className="cursor-pointer text-[12px] leading-[1.4] text-ink3 select-none hover:text-ink2">
        {block.label}
        {block.items > 0 && <span className="tabular-nums"> · {block.items}</span>}
      </summary>
      <div className="mt-2 border-l border-line2 pl-3.5 text-[13px] leading-[1.55] text-ink2 select-text">
        <Markdown>{block.body}</Markdown>
      </div>
    </details>
  );
}

/**
 * A turn Borg wrote into the user's role: a heartbeat check-in or a session
 * greeting. Drawn as a quiet event across the column, not a bubble on the
 * user's side, because the user did not say it.
 */
function SystemTurnCell({
  label,
  text,
  blocks,
  at,
}: {
  label: string;
  text: string;
  blocks: AttachedBlock[];
  at: number | undefined;
}) {
  return (
    <div className="rounded-xl border border-dashed border-line2 px-[18px] py-3">
      <div className="flex items-baseline gap-2">
        <span className={`${KICKER} mb-0 flex-1`}>{label}</span>
        {at !== undefined && (
          <time
            dateTime={new Date(at).toISOString()}
            className="text-[11px] leading-none text-ink3 tabular-nums"
          >
            {formatClock(at)}
          </time>
        )}
      </div>
      {text !== "" && (
        <div className="mt-2.5 text-[13.5px] leading-[1.45] whitespace-pre-wrap text-ink select-text">
          {text}
        </div>
      )}
      {blocks.length > 0 && (
        <div className="mt-2">
          {blocks.map((b, i) => (
            <AttachedSection key={`${b.tag}-${i}`} block={b} />
          ))}
        </div>
      )}
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
  const turn = useMemo(() => parseUserTurn(cell.text), [cell.text]);
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

  if (turn.system) {
    return (
      <SystemTurnCell label={turn.system} text={turn.text} blocks={turn.blocks} at={cell.at} />
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
          {turn.text !== "" && <div>{turn.text}</div>}
          {turn.blocks.length > 0 && (
            <div className={`whitespace-normal ${turn.text !== "" ? "mt-2.5" : ""}`}>
              {turn.blocks.map((b, i) => (
                <AttachedSection key={`${b.tag}-${i}`} block={b} />
              ))}
            </div>
          )}
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
  usage,
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
  /** The turn's time and tokens, on the cell that shows them (see `placeTurnUsage`). */
  usage?: string;
}) {
  const body = renderCell(cell, respond, ordinal, editable, onEdit, usage);
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
  usage: string | undefined,
) {
  switch (cell.kind) {
    case "user":
      return (
        <UserCell cell={cell} onEdit={editable ? (text) => onEdit(ordinal, text) : undefined} />
      );
    case "assistant":
      return (
        // A group, so hovering anywhere on the reply shows its usage mark.
        <div className="group">
          <ReplyCard kicker="Borg" action={<SpeakButton cell={cell} />}>
            <Markdown>{cell.text}</Markdown>
          </ReplyCard>
          {(cell.at !== undefined || usage) && <SentAt at={cell.at} usage={usage} />}
        </div>
      );
    case "thinking":
      return <ThinkingCell cell={cell} />;
    case "tool":
      return <ToolCell cell={cell} />;
    case "usage":
      return <UsageCell usage={usage} />;
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
