import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { AttachIcon, CloseIcon, CompletionList, Pill, SendIcon, useIsMobile } from "@borg/ui";
import { useKeybind } from "../../lib/keybinds";
import { rank } from "../palette/score";
import { useCommands } from "../palette/useCommands";
import { collectImages } from "./attachments";
import { publishDraft } from "./draftStore";
import { ModelPill } from "./ModelPill";
import { slashQuery } from "./slashToken";
import type { Attachment } from "./useConversation";

/** Grows with its content up to a ceiling, then scrolls. */
const MAX_ROWS_PX = 200;

/**
 * How many rows the `/` menu offers. Short on purpose: it is there to teach
 * what Borg can do, and a wall of options teaches nothing. ⌘K is the full list.
 */
const SLASH_RESULT_LIMIT = 8;

function useAutoGrow(value: string) {
  const ref = useRef<HTMLTextAreaElement>(null);
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    // Collapse first: scrollHeight only shrinks if the box is not already tall.
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, MAX_ROWS_PX)}px`;
  }, [value]);
  return ref;
}

interface ComposerProps {
  busy: boolean;
  /** Persists the draft per session so switching conversations does not lose it. */
  draftKey: string;
  /**
   * Identifies the conversation itself. Unlike `draftKey` it does not change
   * when a new conversation is issued its id, so attachments survive that.
   */
  conversationKey: string;
  onSend: (text: string, attachments: Attachment[]) => void;
  onSteer: (text: string) => void;
  onCancel: () => void;
}

/**
 * The input.
 *
 * A textarea rather than an `<input>`, so a multi-line prompt is possible at
 * all: Enter sends, Shift+Enter breaks the line. It stays enabled during a
 * turn — that path steers the running agent instead of queueing, which is the
 * one thing the daemon supports and the UI never exposed.
 */
export function Composer({
  busy,
  draftKey,
  conversationKey,
  onSend,
  onSteer,
  onCancel,
}: ComposerProps) {
  const isMobile = useIsMobile();
  const [draft, setDraft] = useState("");
  const [attachments, setAttachments] = useState<Attachment[]>([]);
  const [error, setError] = useState<string>();
  const [dragging, setDragging] = useState(false);
  const textarea = useAutoGrow(draft);
  /** Sent messages, newest last — recalled with ArrowUp on an empty composer. */
  const history = useRef<string[]>([]);
  const historyAt = useRef<number | null>(null);

  // Object URLs are only freed by hand. Tracking them in a ref rather than an
  // effect on `attachments` matters: an effect keyed on the array would revoke
  // the previous images every time a new one is added, blanking the thumbnails
  // still on screen.
  const objectUrls = useRef<string[]>([]);
  useEffect(
    () => () => {
      for (const url of objectUrls.current) URL.revokeObjectURL(url);
      objectUrls.current = [];
    },
    [],
  );

  // Restore this session's draft whenever the key changes — including the
  // provisional → real migration a new conversation makes once the daemon
  // answers, where the text has already been moved across for us.
  useEffect(() => {
    setDraft(sessionStorage.getItem(draftKey) ?? "");
  }, [draftKey]);

  // Attachments belong to the conversation, not to the key naming it. Clearing
  // them on any key change would silently bin an image attached in the moment
  // between opening a new conversation and the daemon issuing its id.
  //
  // Dropping the chips is not enough: the blobs behind them stay alive until
  // the document unloads, so hopping between conversations would pile up a
  // full copy of every abandoned image.
  useEffect(() => {
    for (const url of objectUrls.current) URL.revokeObjectURL(url);
    objectUrls.current = [];
    setAttachments([]);
    setError(undefined);
  }, [conversationKey]);

  useEffect(() => {
    if (draft === "") sessionStorage.removeItem(draftKey);
    else sessionStorage.setItem(draftKey, draft);
    // Session lists render beside this box and name unsent rows after it;
    // same-tab writes raise no storage event, so tell them directly.
    publishDraft(draftKey, draft);
  }, [draft, draftKey]);

  useKeybind("cancel-turn", onCancel, { enabled: busy, inInputs: true });

  // `/` opens the same command list ⌘K renders. The chord is the shortcut for
  // people who know it exists; the slash is how everyone else finds out.
  const token = slashQuery(draft);
  const [slashDismissed, setSlashDismissed] = useState(false);
  const [slashCursor, setSlashCursor] = useState(0);
  const slashOpen = token !== null && !slashDismissed;

  // A refusal applies to the token that earned it. Once the token is gone the
  // next `/` is a fresh request, not a continuation of the dismissed one.
  useEffect(() => {
    if (token === null) setSlashDismissed(false);
    setSlashCursor(0);
  }, [token]);

  const { commands } = useCommands({ active: slashOpen, searching: (token ?? "") !== "" });
  const slashResults = useMemo(
    () => (slashOpen ? rank(commands, token ?? "").slice(0, SLASH_RESULT_LIMIT) : []),
    [slashOpen, commands, token],
  );

  // Registered after the cancel-turn binding above, so while the menu is open
  // Escape closes it and on close the running turn gets Escape back. A raw
  // listener here would do both at once.
  useKeybind("cancel-turn", () => setSlashDismissed(true), { enabled: slashOpen, inInputs: true });

  const runCommand = (index: number) => {
    const command = slashResults[index];
    if (!command) return;
    // The token was the query, never prompt text. Clearing it first means a
    // command that navigates cannot leave a stray `/` behind in the draft.
    setDraft("");
    setSlashDismissed(false);
    command.run();
  };

  const addFiles = useCallback(async (files: readonly File[]) => {
    if (files.length === 0) return;
    const { attachments: added, errors } = await collectImages(files);
    if (added.length > 0) {
      objectUrls.current.push(...added.map((a) => a.previewUrl));
      setAttachments((prev) => [...prev, ...added]);
    }
    setError(errors[0]);
  }, []);

  const forget = (urls: string[]) => {
    for (const url of urls) URL.revokeObjectURL(url);
    objectUrls.current = objectUrls.current.filter((u) => !urls.includes(u));
  };

  const removeAttachment = (index: number) => {
    setAttachments((prev) => {
      const target = prev[index];
      if (target) forget([target.previewUrl]);
      return prev.filter((_, i) => i !== index);
    });
  };

  const submit = () => {
    // A slash token is a menu query. The daemon has no slash handler, so
    // sending it as prompt text would look like it worked and do nothing.
    if (slashOpen) {
      runCommand(slashCursor);
      return;
    }
    const text = draft.trim();
    if (text === "" && attachments.length === 0) return;
    if (busy) {
      onSteer(text);
    } else {
      onSend(text, attachments);
    }
    history.current.push(text);
    historyAt.current = null;
    setDraft("");
    // The bytes are already read and on their way; only the previews are left
    // to release, and the sent turn renders a count rather than thumbnails.
    forget(attachments.map((a) => a.previewUrl));
    setAttachments([]);
    setError(undefined);
  };

  const onKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    // While the menu is open the arrows belong to it, not to the caret or to
    // history recall.
    if (slashOpen && (e.key === "ArrowDown" || e.key === "ArrowUp")) {
      e.preventDefault();
      const last = Math.max(0, slashResults.length - 1);
      setSlashCursor((c) => Math.max(0, Math.min(last, e.key === "ArrowDown" ? c + 1 : c - 1)));
      return;
    }
    if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) {
      e.preventDefault();
      submit();
      return;
    }
    // ArrowUp recalls previous prompts, but only from an empty box — otherwise
    // it would fight ordinary cursor movement.
    if (e.key === "ArrowUp" && draft === "" && history.current.length > 0) {
      e.preventDefault();
      const next =
        historyAt.current === null
          ? history.current.length - 1
          : Math.max(0, historyAt.current - 1);
      historyAt.current = next;
      setDraft(history.current[next] ?? "");
    }
  };

  return (
    <div className={`flex-none ${isMobile ? "px-[18px] pt-3.5 pb-3" : "px-[30px] pt-4 pb-[26px]"}`}>
      <div className={`mx-auto max-w-[660px] ${isMobile ? "px-[22px]" : ""}`}>
        {error && <div className="mb-2 px-1 text-[11.5px] leading-[1.5] text-alert">{error}</div>}

        {attachments.length > 0 && (
          <div className="mb-2 flex flex-wrap gap-2">
            {attachments.map((a, i) => (
              <div
                key={a.previewUrl}
                className="relative h-[52px] w-[52px] overflow-hidden rounded-[8px] border border-line2"
              >
                <img src={a.previewUrl} alt={a.filename} className="h-full w-full object-cover" />
                <button
                  type="button"
                  aria-label={`Remove ${a.filename}`}
                  onClick={() => removeAttachment(i)}
                  className="absolute top-0 right-0 flex h-[18px] w-[18px] cursor-pointer items-center justify-center rounded-bl-[6px] border-0 bg-surface text-ink2 hover:text-ink"
                >
                  <CloseIcon size={11} />
                </button>
              </div>
            ))}
          </div>
        )}

        {slashOpen && (
          <CompletionList
            label="Commands"
            emptyText="No command matches. Press Escape to keep typing."
            rows={slashResults.map((c) => ({
              id: c.id,
              label: c.label,
              detail: c.detail,
              hint: c.hint,
            }))}
            cursor={slashCursor}
            onHover={setSlashCursor}
            onChoose={runCommand}
          />
        )}

        <div
          onDragOver={(e) => {
            e.preventDefault();
            setDragging(true);
          }}
          onDragLeave={() => setDragging(false)}
          onDrop={(e) => {
            e.preventDefault();
            setDragging(false);
            void addFiles(Array.from(e.dataTransfer.files));
          }}
          className={`flex items-end gap-3 border bg-surface2 py-2 pr-2 pl-[19px] ${
            isMobile ? "rounded-[22px]" : "rounded-[13px]"
          } ${dragging ? "border-neon" : "border-line"}`}
        >
          <label
            aria-label="Attach an image"
            className="flex h-[29px] flex-none cursor-pointer items-center text-ink3 hover:text-ink2"
          >
            <AttachIcon size={15} />
            <input
              type="file"
              accept="image/png,image/jpeg,image/gif,image/webp"
              multiple
              className="hidden"
              onChange={(e) => {
                void addFiles(Array.from(e.target.files ?? []));
                e.target.value = "";
              }}
            />
          </label>
          <textarea
            ref={textarea}
            rows={1}
            className="min-w-0 flex-1 resize-none border-0 bg-transparent py-1.5 text-[14.5px] leading-[1.4] text-ink outline-none select-text placeholder:text-ink3"
            placeholder={busy ? "Steer Borg…" : "Ask Borg anything"}
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            onKeyDown={onKeyDown}
            onPaste={(e) => {
              const files = Array.from(e.clipboardData.files);
              if (files.length === 0) return;
              e.preventDefault();
              void addFiles(files);
            }}
          />
          {busy && draft.trim() === "" ? (
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

        {/* Under the box, not in it: choosing the model is part of asking the
            question, but it is not the question. */}
        <div className="mt-1.5 flex items-center px-1">
          <ModelPill />
        </div>
      </div>
    </div>
  );
}
