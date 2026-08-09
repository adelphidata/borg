import { useEffect, useRef, useState } from "react";
import { useParams } from "react-router-dom";
import { Brackets, PageChrome, Pill, SendIcon, useIsMobile } from "@borg/ui";
import type { session as sessionPb } from "@borg/client";
import { DaemonState } from "../../components/DaemonState";
import { useConversation, type Cell } from "./useConversation";

const KICKER =
  "text-[10px] leading-none font-medium tracking-[0.16em] uppercase text-ink3 mb-[13px]";

/** Assistant reply in the bracketed-card style. */
function ReplyCard({ kicker, children }: { kicker: string; children: React.ReactNode }) {
  return (
    <div className="relative rounded-xl border border-line bg-surface2 p-5 pb-[18px] shadow-(--plateGlow)">
      <Brackets card />
      <div className={KICKER}>{kicker}</div>
      <div className="text-[14.5px] leading-[1.6] whitespace-pre-wrap text-pretty text-ink">
        {children}
      </div>
    </div>
  );
}

function ToolCell({ cell }: { cell: Cell & { kind: "tool" } }) {
  const [open, setOpen] = useState(false);
  const summary = cell.output.length > 400 ? `${cell.output.slice(0, 400)}…` : cell.output;
  return (
    <div
      className="cursor-pointer rounded-[10px] border border-line2 px-4 py-3"
      onClick={() => setOpen((v) => !v)}
    >
      <div className="flex items-center gap-2 text-[11px] leading-none font-medium tracking-[0.08em] text-ink3 uppercase">
        <span className={`h-1.5 w-1.5 rounded-full ${cell.done ? "bg-neon" : "bg-ink3"}`} />
        {cell.name}
        {!cell.done && <span className="normal-case tracking-normal">running…</span>}
      </div>
      {(open || !cell.done) && cell.output !== "" && (
        <pre className="mt-2.5 mb-0 max-h-[260px] overflow-auto text-[11.5px] leading-[1.5] whitespace-pre-wrap text-ink2">
          {open ? cell.output : summary}
        </pre>
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
  return (
    <div className="rounded-xl border border-line2 px-[18px] py-4">
      <div className={KICKER}>Borg wants to run a command</div>
      <pre className="m-0 text-[12.5px] leading-[1.5] whitespace-pre-wrap text-ink">
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

function InputRequestCell({
  cell,
  respond,
}: {
  cell: Cell & { kind: "input-request" };
  respond: (promptId: string, value: string) => void;
}) {
  const [answers, setAnswers] = useState<Record<string, string[]>>({});

  const toggle = (q: sessionPb.UserInputQuestion, value: string) => {
    setAnswers((prev) => {
      const current = prev[q.id] ?? [];
      const next = q.multiSelect
        ? current.includes(value)
          ? current.filter((v) => v !== value)
          : [...current, value]
        : [value];
      return { ...prev, [q.id]: next };
    });
  };

  const submit = () => {
    const payload = Object.fromEntries(
      cell.questions.map((q) => [q.id, { answers: answers[q.id] ?? [] }]),
    );
    respond(cell.promptId, JSON.stringify(payload));
  };

  return (
    <div className="rounded-xl border border-line2 px-[18px] py-4">
      <div className={KICKER}>Borg has a question</div>
      <div className="flex flex-col gap-4">
        {cell.questions.map((q) => (
          <div key={q.id}>
            <div className="text-[13.5px] leading-[1.5] text-ink">{q.question}</div>
            <div className="mt-2.5 flex flex-wrap gap-2">
              {q.options.map((o) => {
                const on = (answers[q.id] ?? []).includes(o.label);
                return (
                  <Pill
                    key={o.label}
                    square
                    variant={on ? "primary" : undefined}
                    onClick={() => toggle(q, o.label)}
                  >
                    {o.label}
                  </Pill>
                );
              })}
            </div>
          </div>
        ))}
      </div>
      {!cell.answered && (
        <div className="mt-4">
          <Pill variant="primary" square onClick={submit}>
            Send answer
          </Pill>
        </div>
      )}
    </div>
  );
}

function TranscriptCell({
  cell,
  respond,
}: {
  cell: Cell;
  respond: (promptId: string, value: string) => void;
}) {
  switch (cell.kind) {
    case "user":
      return (
        <div className="flex justify-end">
          <div className="max-w-[78%] rounded-[13px] rounded-br-[4px] border border-line2 bg-surface3 px-4 py-[11px] text-[13.5px] leading-[1.45] whitespace-pre-wrap text-ink">
            {cell.text}
          </div>
        </div>
      );
    case "assistant":
      return <ReplyCard kicker="Borg">{cell.text}</ReplyCard>;
    case "thinking":
      return (
        <div className="px-1 text-[12.5px] leading-[1.55] whitespace-pre-wrap text-ink3 italic">
          {cell.text}
        </div>
      );
    case "tool":
      return <ToolCell cell={cell} />;
    case "notice":
      return <div className="text-center text-[11.5px] leading-[1.5] text-ink3">{cell.text}</div>;
    case "error":
      return (
        <div className="rounded-xl border border-alert bg-alert-soft px-4 py-3 text-[12.5px] leading-[1.5] text-ink2">
          {cell.text}
        </div>
      );
    case "shell-confirm":
      return <ShellConfirmCell cell={cell} respond={respond} />;
    case "input-request":
      return <InputRequestCell cell={cell} respond={respond} />;
  }
}

function Composer({
  busy,
  onSend,
  onCancel,
}: {
  busy: boolean;
  onSend: (text: string) => void;
  onCancel: () => void;
}) {
  const isMobile = useIsMobile();
  const [draft, setDraft] = useState("");

  const submit = () => {
    const text = draft.trim();
    if (!text || busy) return;
    onSend(text);
    setDraft("");
  };

  return (
    <div className={`flex-none ${isMobile ? "px-[18px] pt-3.5 pb-3" : "px-[30px] pt-4 pb-[26px]"}`}>
      <div className={`mx-auto max-w-[660px] ${isMobile ? "px-[22px]" : ""}`}>
        <div
          className={`flex items-center gap-3 border border-line bg-surface2 py-2 pr-2 pl-[19px] ${
            isMobile ? "rounded-full" : "rounded-[13px]"
          }`}
        >
          <input
            className="min-w-0 flex-1 border-0 bg-transparent py-1.5 text-[14.5px] leading-[1.3] text-ink outline-none placeholder:text-ink3"
            placeholder={busy ? "Borg is working…" : "Ask Borg anything"}
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && submit()}
          />
          {busy ? (
            <Pill variant="danger" square onClick={onCancel}>
              Stop
            </Pill>
          ) : (
            <button
              type="button"
              aria-label="Send"
              className="flex h-[29px] w-[29px] flex-none cursor-pointer items-center justify-center rounded-[9px] border-0 bg-neon shadow-(--glow)"
              onClick={submit}
            >
              <SendIcon size={14} />
            </button>
          )}
        </div>
      </div>
    </div>
  );
}

export function ConversationPage() {
  const { sessionId: target } = useParams();
  const isMobile = useIsMobile();
  const { opened, cells, busy, sendError, send, cancel, respond } = useConversation(target);
  const bottomRef = useRef<HTMLDivElement>(null);

  // Follow the newest cell, like any chat surface.
  useEffect(() => {
    bottomRef.current?.scrollIntoView({ block: "end" });
  }, [cells.length, busy]);

  const title = opened.state === "ready" && opened.data.title ? opened.data.title : "Conversation";
  const meta = opened.state === "ready" && opened.data.model ? opened.data.model : undefined;

  return (
    <PageChrome title={title} meta={meta} mobileBrandHeader>
      <div className={`flex-1 overflow-y-auto pb-2 ${isMobile ? "pt-[22px]" : "pt-[38px]"}`}>
        <div
          className={`mx-auto flex max-w-[660px] flex-col gap-6 ${
            isMobile ? "px-[22px]" : "px-[30px]"
          }`}
        >
          <DaemonState query={opened} label="the conversation">
            {() => (
              <>
                {cells.length === 0 && (
                  <div className="pt-10 text-center text-[14.5px] leading-[1.62] text-pretty text-ink3">
                    Nothing here yet. Ask Borg anything.
                  </div>
                )}
                {cells.map((cell, i) => (
                  <TranscriptCell key={i} cell={cell} respond={respond} />
                ))}
                {busy && (
                  <div className="px-1 text-[12px] leading-none text-ink3">Borg is thinking…</div>
                )}
                {sendError && (
                  <div className="rounded-xl border border-alert bg-alert-soft px-4 py-3 text-[12.5px] leading-[1.5] text-ink2">
                    {sendError.message}
                  </div>
                )}
                <div ref={bottomRef} />
              </>
            )}
          </DaemonState>
        </div>
      </div>
      <Composer busy={busy} onSend={send} onCancel={cancel} />
    </PageChrome>
  );
}
