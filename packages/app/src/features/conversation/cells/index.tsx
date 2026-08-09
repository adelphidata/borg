import { useState } from "react";
import { Brackets, Markdown, PencilIcon, Pill } from "@borg/ui";
import type { Cell } from "../useConversation";
import { InputRequestCell } from "./InputRequestCell";
import { ThinkingCell } from "./ThinkingCell";
import { ToolCell } from "./ToolCell";

const KICKER =
  "text-[10px] leading-none font-medium tracking-[0.16em] uppercase text-ink3 mb-[13px]";

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

const COMPACT = new Intl.NumberFormat(undefined, { notation: "compact", maximumFractionDigits: 1 });

/** Per-turn token accounting, deliberately quiet. */
function UsageCell({ cell }: { cell: Cell & { kind: "usage" } }) {
  const cached = cell.cacheRead + cell.cacheWrite;
  return (
    <div className="px-1 text-[11px] leading-none text-ink3">
      {COMPACT.format(cell.input)} in · {COMPACT.format(cell.output)} out
      {cached > 0 && <> · {COMPACT.format(cached)} cached</>}
    </div>
  );
}

function SubAgentCell({ cell }: { cell: Cell & { kind: "sub-agent" } }) {
  const running = cell.status.toLowerCase() === "running";
  return (
    <div className="flex items-center gap-2 rounded-[10px] border border-line2 px-4 py-2.5">
      <span
        className={`h-1.5 w-1.5 flex-none rounded-full ${running ? "animate-bpulse bg-neon" : "bg-ink3"}`}
      />
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
          className="mt-2 flex h-6 w-6 flex-none cursor-pointer items-center justify-center rounded border-0 bg-transparent text-ink3 opacity-0 transition-opacity group-hover:opacity-100 focus-visible:opacity-100 hover:text-ink"
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

export function TranscriptCell({
  cell,
  respond,
  onEdit,
}: {
  cell: Cell;
  respond: (promptId: string, value: string) => void;
  /** Absent when this turn cannot be rewritten — mid-turn, or not a user cell. */
  onEdit?: (text: string) => void;
}) {
  switch (cell.kind) {
    case "user":
      return <UserCell cell={cell} onEdit={onEdit} />;
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
    case "error":
      return (
        <div className="rounded-xl border border-alert bg-alert-soft px-4 py-3 text-[12.5px] leading-[1.5] text-ink2 select-text">
          {cell.text}
        </div>
      );
    case "shell-confirm":
      return <ShellConfirmCell cell={cell} respond={respond} />;
    case "input-request":
      return <InputRequestCell cell={cell} respond={respond} />;
  }
}
