import { useState, type ReactNode } from "react";
import { useNavigate } from "react-router-dom";
import { SendIcon, isSubmitEnter } from "@borg/ui";
import { asBorgError, useBorg } from "../../lib/useDaemon";
import { askAboutMeeting } from "./askAboutMeeting";

/**
 * The pieces a meeting's page and a calendar event's page share: the big
 * title, the row of chips under it, and the "Ask anything" bar. Granola's
 * document layout, drawn with Borg's own type and colours.
 */

/** The page column: wide enough to read as a document, narrow enough to scan. */
export const DOC_COL = "mx-auto flex min-h-full w-full max-w-[680px] flex-col gap-6";

const TITLE = "m-0 text-[26px] leading-[1.2] font-medium tracking-[-0.01em] text-ink";

/**
 * The page's title, set large: the page reads as a document, not a form.
 * With `onRename`, a double-click (or Enter) edits it in place.
 */
export function DocTitle({
  title,
  onRename,
}: {
  title: string;
  /** Resolves false to keep the box open, as when the rename failed. */
  onRename?: (next: string) => Promise<boolean>;
}) {
  const [draft, setDraft] = useState<string>();
  const [saving, setSaving] = useState(false);
  if (!onRename) return <h1 className={`${TITLE} break-words select-text`}>{title}</h1>;
  const commit = () => {
    if (draft === undefined || saving) return;
    const next = draft.trim();
    if (next === "" || next === title) {
      setDraft(undefined);
      return;
    }
    setSaving(true);
    void onRename(next)
      .then((ok) => {
        if (ok) setDraft(undefined);
      })
      .finally(() => setSaving(false));
  };
  if (draft !== undefined) {
    return (
      <input
        autoFocus
        aria-label="Meeting name"
        className={`${TITLE} -mx-2 w-[calc(100%+16px)] rounded-[8px] border border-focus bg-surface2 px-2 py-1 outline-none`}
        value={draft}
        readOnly={saving}
        onChange={(e) => setDraft(e.target.value)}
        onBlur={commit}
        onKeyDown={(e) => {
          if (e.key === "Escape") {
            e.preventDefault();
            setDraft(undefined);
          } else if (isSubmitEnter(e)) {
            e.preventDefault();
            commit();
          }
        }}
      />
    );
  }
  return (
    <h1
      tabIndex={0}
      title="Double-click to rename"
      className={`${TITLE} -mx-2 cursor-text rounded-[8px] border border-transparent px-2 py-1 break-words hover:bg-surface2 focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-focus`}
      onDoubleClick={() => setDraft(title)}
      onKeyDown={(e) => {
        if (e.key === "Enter") setDraft(title);
      }}
    >
      {title}
    </h1>
  );
}

const CHIP =
  "inline-flex h-7 items-center gap-1.5 rounded-full border border-line2 px-3 text-[12px] leading-none text-ink2";

/** One fact about the meeting: when, who, how long. */
export function Chip({
  icon,
  title,
  children,
}: {
  icon?: ReactNode;
  /** The longer version, on hover: everyone's name behind "3". */
  title?: string;
  children: ReactNode;
}) {
  return (
    <span className={CHIP} title={title}>
      {icon && <span className="flex text-ink3">{icon}</span>}
      {children}
    </span>
  );
}

/** A chip that does something, like joining the call. */
export function ChipButton({
  icon,
  onClick,
  children,
}: {
  icon?: ReactNode;
  onClick: () => void;
  children: ReactNode;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      className={`${CHIP} cursor-pointer bg-transparent hover:border-neon hover:text-ink`}
    >
      {icon && <span className="flex text-ink3">{icon}</span>}
      {children}
    </button>
  );
}

export function ChipRow({ children }: { children: ReactNode }) {
  return <div className="flex flex-wrap items-center gap-2">{children}</div>;
}

/** One-press questions offered beside the box. */
const SUGGESTIONS = ["Write a follow-up email", "List the action items"];

/**
 * "Ask anything": floats at the foot of the page and starts a conversation
 * that knows which meeting it is about.
 */
export function AskBar({ meeting }: { meeting: { id: string; title: string } }) {
  const borg = useBorg();
  const navigate = useNavigate();
  const [question, setQuestion] = useState("");
  const [asking, setAsking] = useState(false);
  const [error, setError] = useState<string>();
  const ask = async (text: string) => {
    if (text.trim() === "" || asking) return;
    setAsking(true);
    setError(undefined);
    try {
      const sessionId = await askAboutMeeting(borg, text, meeting);
      navigate(`/c/${sessionId}`);
    } catch (e) {
      setError(`Couldn't ask Borg: ${asBorgError(e).message}`);
      setAsking(false);
    }
  };
  return (
    <div className="sticky bottom-0 mt-auto bg-gradient-to-t from-surface from-60% to-transparent pt-6 pb-5">
      <div className="flex items-center gap-2 rounded-full border border-line bg-surface2 py-1.5 pr-1.5 pl-5 shadow-(--frameShadow) focus-within:border-focus">
        <input
          aria-label="Ask about this meeting"
          className="min-w-0 flex-1 border-0 bg-transparent text-[13.5px] leading-[1.4] text-ink outline-none placeholder:text-ink3"
          placeholder="Ask anything"
          value={question}
          disabled={asking}
          onChange={(e) => setQuestion(e.target.value)}
          onKeyDown={(e) => {
            if (isSubmitEnter(e)) {
              e.preventDefault();
              void ask(question);
            }
          }}
        />
        {question.trim() === "" ? (
          SUGGESTIONS.map((s) => (
            <button
              key={s}
              type="button"
              disabled={asking}
              onClick={() => void ask(s)}
              className="hidden h-8 flex-none cursor-pointer items-center rounded-full border border-line2 bg-transparent px-3 text-[12px] leading-none whitespace-nowrap text-ink2 hover:border-neon hover:text-ink disabled:opacity-50 sm:flex"
            >
              {s}
            </button>
          ))
        ) : (
          <button
            type="button"
            aria-label="Ask"
            disabled={asking}
            onClick={() => void ask(question)}
            className="flex h-8 w-8 flex-none cursor-pointer items-center justify-center rounded-full border-0 bg-neon disabled:opacity-50"
          >
            <SendIcon size={13} />
          </button>
        )}
      </div>
      {error && <div className="mt-2 px-5 text-[12px] leading-[1.45] text-alert">{error}</div>}
    </div>
  );
}
