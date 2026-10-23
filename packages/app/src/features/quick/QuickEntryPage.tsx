import { useEffect, useRef, useState } from "react";
import { BorgHead, SendIcon, isSubmitEnter } from "@borg/ui";
import { asBorgError, useBorg } from "../../lib/useDaemon";
import { useNative } from "../../lib/platform";

/**
 * The global-hotkey panel.
 *
 * One job: get a thought into Borg without leaving whatever you were doing.
 * It resumes the most recent conversation, sends, hands the session to the
 * main window, and disappears. No transcript, no history, no settings — the
 * moment it needs a second screen it stops being a capture tool.
 */
export function QuickEntryPage() {
  const borg = useBorg();
  const native = useNative();
  const [draft, setDraft] = useState("");
  const [sending, setSending] = useState(false);
  const [error, setError] = useState<string>();
  const input = useRef<HTMLTextAreaElement>(null);

  // The window is shown and hidden rather than rebuilt, so focus has to be
  // reclaimed every time it comes back rather than only on mount.
  useEffect(() => {
    const focus = () => {
      input.current?.focus();
      setError(undefined);
    };
    focus();
    window.addEventListener("focus", focus);
    return () => window.removeEventListener("focus", focus);
  }, []);

  const submit = async () => {
    const text = draft.trim();
    if (text === "" || sending) return;
    setSending(true);
    setError(undefined);
    try {
      // Resume the most recent conversation; a capture that started a brand
      // new session every time would shred the history into fragments.
      const list = await borg.admin.listSessions({});
      const resumeId = list.sessions[0]?.id;
      const open = await borg.session.open(resumeId ? { resumeId } : {});
      borg.session.send({ sessionId: open.sessionId, text, images: [] }, { onMessage: () => {} });

      setDraft("");
      native.hideWindow();
      // The reply belongs where the transcript is.
      native.revealSession(open.sessionId);
    } catch (e) {
      setError(asBorgError(e).message);
    } finally {
      setSending(false);
    }
  };

  return (
    <div className="flex h-full flex-col justify-center rounded-[14px] border border-line bg-surface2 px-4 py-3 shadow-(--frameShadow)">
      <div className="flex items-center gap-3">
        <BorgHead width={28} />
        <textarea
          ref={input}
          rows={1}
          className="min-w-0 flex-1 resize-none border-0 bg-transparent py-1 text-[15px] leading-[1.4] text-ink outline-none select-text placeholder:text-ink3"
          placeholder="Ask Borg anything"
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Escape") {
              native.hideWindow();
              return;
            }
            if (isSubmitEnter(e) && !e.shiftKey) {
              e.preventDefault();
              void submit();
            }
          }}
        />
        <button
          type="button"
          aria-label="Send"
          disabled={sending}
          onClick={() => void submit()}
          className="flex h-[29px] w-[29px] flex-none cursor-pointer items-center justify-center rounded-[9px] border-0 bg-neon shadow-(--glow)"
        >
          <SendIcon size={14} />
        </button>
      </div>
      <div className="mt-2 px-[33px] text-[10.5px] leading-none text-ink3">
        {error ? <span className="text-alert">{error}</span> : "Enter to send · Esc to dismiss"}
      </div>
    </div>
  );
}
