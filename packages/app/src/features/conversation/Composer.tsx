import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  AttachIcon,
  CloseIcon,
  CompletionList,
  isSubmitEnter,
  Pill,
  rowDomId,
  SendIcon,
  useIsMobile,
} from "@borg/ui";
import { useKeybind } from "../../lib/keybinds";
import { rank } from "../palette/score";
import { useCommands } from "../palette/useCommands";
import { useAutoGrow } from "./useAutoGrow";
import { collectAttachments } from "./attachments";
import { focusComposer, registerComposer } from "./composerFocus";
import { appendPastedText, composerOwnsPaste, pastedTextFile } from "./documentPaste";
import { draftTitle, publishDraft, readDraft } from "./draftStore";
import { carriesFiles, carriesText, insertAt, readDrop } from "./dropContent";
import { ModelPill } from "./ModelPill";
import { VoiceInputButton } from "./VoiceInputButton";
import { backspacePathToken, pathToken, replacePathToken } from "./pathToken";
import { mergeQueued } from "./queuedTurn";
import { slashQuery } from "./slashToken";
import type { Attachment } from "./useConversation";
import { usePathCompletions } from "./usePathCompletions";

/** Grows with its content up to a ceiling, then scrolls. */
const MAX_ROWS_PX = 200;

/**
 * How many rows the `/` menu offers. Short on purpose: it is there to teach
 * what Borg can do, and a wall of options teaches nothing. ⌘K is the full list.
 */
const SLASH_RESULT_LIMIT = 8;

/** Ties the textarea's `aria-controls` to the list it drives. */
const SLASH_LIST_ID = "composer-slash-menu";

/** Same, for the `@` path menu. Only one of the two is ever open. */
const PATH_LIST_ID = "composer-path-menu";

interface ComposerProps {
  busy: boolean;
  /**
   * True while the turn is parked on a prompt only the user can answer. What is
   * typed then is held back and sent as the next turn, because the daemon
   * drains steers at tool boundaries and the agent is not at one — it is
   * waiting. Absent means no prompt is open.
   */
  awaitingInput?: boolean;
  /**
   * False until the conversation is open and can take a message. What is sent
   * before then is held the same way and goes out once it is. Absent means
   * ready.
   */
  ready?: boolean;
  /** Persists the draft per session so switching conversations does not lose it. */
  draftKey: string;
  /**
   * Identifies the conversation itself. Unlike `draftKey` it does not change
   * when a new conversation is issued its id, so attachments survive that.
   */
  conversationKey: string;
  /**
   * The live daemon session, whose workspace `@` completions are scoped to.
   * Undefined before it opens, and the menu simply does not appear — a file
   * picker with no workspace to pick from has nothing to offer.
   */
  sessionId?: string;
  onSend: (text: string, attachments: Attachment[]) => void;
  /** Resolves to false when the running turn did not take the text. */
  onSteer: (text: string) => Promise<boolean> | boolean | void;
  onCancel: () => void;
  /**
   * Takes a command with words after it (`/goal …`) before it can be sent or
   * steered as prose. Returns false for anything that is not one.
   */
  onCommand?: (text: string) => boolean;
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
  awaitingInput = false,
  ready = true,
  draftKey,
  conversationKey,
  sessionId,
  onSend,
  onSteer,
  onCancel,
  onCommand,
}: ComposerProps) {
  const isMobile = useIsMobile();
  const [draft, setDraft] = useState("");
  const [attachments, setAttachments] = useState<Attachment[]>([]);
  /**
   * What was typed while a prompt was open, waiting for the turn to end.
   *
   * One entry, never a stack — a second submission merges into it. Its previews
   * stay minted, because returning it to the box has to return the chips too.
   */
  const [queued, setQueued] = useState<{ text: string; attachments: Attachment[] }>();
  const [error, setError] = useState<string>();
  const [dragging, setDragging] = useState(false);
  const textarea = useAutoGrow(draft, MAX_ROWS_PX);
  /** Sent messages, newest last — recalled with ArrowUp on an empty composer. */
  const history = useRef<string[]>([]);
  const historyAt = useRef<number | null>(null);

  // Object URLs are only freed by hand. Tracking them in a ref rather than an
  // effect on `attachments` matters: an effect keyed on the array would revoke
  // the previous images every time a new one is added, blanking the thumbnails
  // still on screen.
  const objectUrls = useRef<string[]>([]);
  /** The conversation on screen right now, for work that outlives a render. */
  const conversationKeyRef = useRef(conversationKey);
  conversationKeyRef.current = conversationKey;
  /** Read by effects that must not re-run when the queue changes. */
  const queuedRef = useRef<{ text: string; attachments: Attachment[] } | undefined>(undefined);
  queuedRef.current = queued;
  // Read by the paste handler, which is registered once on the document and
  // would otherwise see the attachment list as it was when it was registered.
  const attachmentsRef = useRef<Attachment[]>(attachments);
  attachmentsRef.current = attachments;
  const draftKeyRef = useRef(draftKey);
  draftKeyRef.current = draftKey;
  const isMobileRef = useRef(isMobile);
  isMobileRef.current = isMobile;

  useEffect(() => registerComposer(() => textarea.current), [textarea]);

  // Opening a conversation is a request to type in it. Not on a phone, where
  // focus raises the keyboard over the conversation that was just opened.
  useEffect(() => {
    if (!isMobileRef.current) focusComposer();
  }, [conversationKey]);

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
    const restored = sessionStorage.getItem(draftKey) ?? "";
    // Queued text is mirrored into the stored draft so a reload cannot lose it.
    // On the one key change that happens mid-conversation — provisional id to
    // real id — restoring it would put the same text in the box *and* leave it
    // queued, and it would send twice.
    setDraft(restored === queuedRef.current?.text ? "" : restored);
    // A restored draft is not a keystroke. A stored `/new` that popped a menu
    // on session switch would put a command under Enter without anyone asking.
    setSlashDismissed(slashQuery(restored) !== null);
    // Same for `@`: a caret left over from the last conversation would sit at
    // an offset into text nobody has looked at yet, and a draft ending in a
    // half-typed path would open a menu on arrival.
    setCaret(0);
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
    // The queue belongs to the turn it was typed into. Nothing is lost by
    // dropping it here: its text is mirrored into that conversation's stored
    // draft below, so coming back restores it into the box.
    setQueued(undefined);
    setError(undefined);
  }, [conversationKey]);

  useEffect(() => {
    // Queued text is still this conversation's draft as far as everything
    // outside the composer is concerned — the session lists name unsent rows
    // after it, and a reload should hand it back rather than swallow it.
    const stored = draft !== "" ? draft : (queued?.text ?? "");
    if (stored === "") sessionStorage.removeItem(draftKey);
    else sessionStorage.setItem(draftKey, stored);
    // Session lists render beside this box and name unsent rows after it;
    // same-tab writes raise no storage event, so tell them directly.
    publishDraft(draftKey, stored);
  }, [draft, draftKey, queued?.text]);

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

  const { commands, saveError, clearSaveError } = useCommands({
    active: slashOpen,
    searching: (token ?? "") !== "",
    // This surface has the prompt box a skill row writes into.
    withSkills: true,
  });

  // The failure belongs to the menu that caused it. The composer outlives the
  // menu by the whole conversation, so without this a toggle that failed once
  // leaves a red line sitting above the input through every message after it.
  useEffect(() => {
    if (!slashOpen) clearSaveError();
  }, [slashOpen, clearSaveError]);
  const slashResults = useMemo(
    () => (slashOpen ? rank(commands, token ?? "").slice(0, SLASH_RESULT_LIMIT) : []),
    [slashOpen, commands, token],
  );

  // The list changes under a stable token — the settings snapshot and the
  // session listing both arrive after the menu opens. A cursor left past the
  // new end would take Enter to a row that is not there.
  useEffect(() => {
    setSlashCursor((c) => Math.min(c, Math.max(0, slashResults.length - 1)));
  }, [slashResults.length]);

  // `@` names a file. Unlike `/` it has to work mid-sentence, so it needs to
  // know where the caret is — the token being completed is the one the caret
  // sits in, not the last one in the box.
  const [caret, setCaret] = useState(0);
  const syncCaret = () => setCaret(textarea.current?.selectionStart ?? 0);

  /**
   * Puts the caret at `to` once React has painted the new text, then reads it
   * back.
   *
   * Reading it back rather than trusting `to` is what keeps the state honest:
   * changing a textarea's value sends the browser's own caret to the end, and
   * the `keyup` that follows an edit would otherwise record that position as
   * the truth — leaving the menu completing a token the caret is not in.
   */
  const restoreCaret = (to: number) => {
    requestAnimationFrame(() => {
      textarea.current?.focus();
      textarea.current?.setSelectionRange(to, to);
      syncCaret();
    });
  };
  const [pathDismissed, setPathDismissed] = useState(false);
  const [pathCursor, setPathCursor] = useState(0);
  // A conversation with no session open yet has no workspace to scope to.
  const at = sessionId === undefined ? null : pathToken(draft, caret);
  const pathOpen = at !== null && !pathDismissed;
  const pathRows = usePathCompletions(sessionId, pathOpen ? at.query : null);

  useEffect(() => {
    if (at === null) setPathDismissed(false);
    setPathCursor(0);
  }, [at?.query, at === null]);

  useEffect(() => {
    setPathCursor((c) => Math.min(c, Math.max(0, pathRows.length - 1)));
  }, [pathRows.length]);

  // One handler decides who owns Escape, rather than three registrations racing
  // for the top of the stack: `useKeybind` re-registers whenever `enabled`
  // flips, so a turn that starts *after* a menu opened would otherwise take
  // Escape back and cancel while the menu stayed on screen.
  useKeybind(
    "cancel-turn",
    () => {
      if (pathOpen) setPathDismissed(true);
      else if (slashOpen) setSlashDismissed(true);
      else stop();
    },
    { enabled: busy || slashOpen || pathOpen, inInputs: true },
  );

  /** Writes the chosen path over the token that asked for it. */
  const choosePath = (index: number) => {
    const row = pathRows[index];
    if (!row || at === null) return;
    const next = replacePathToken(draft, at.start, caret, row.path);
    setDraft(next.text);
    setCaret(next.caret);
    // Leaving the caret at the end of the box would put the next character
    // after text the user has not looked at.
    restoreCaret(next.caret);
  };

  const runCommand = (index: number) => {
    const command = slashResults[index];
    if (!command) return;
    // A skill row writes what you would have typed and stops there — choosing
    // the skill is the agent's call, and sending is yours. The trailing space
    // in `insert` is where the caret lands, so the sentence carries on.
    if (command.insert !== undefined) {
      setDraft(command.insert);
      setSlashDismissed(true);
      command.run();
      textarea.current?.focus();
      return;
    }
    // A toggle or a theme row is picked by watching it land, so it keeps the
    // menu open — same rule ⌘K follows, because it is the same row. Otherwise
    // the token was the query and never prompt text: clearing it first means a
    // command that navigates cannot leave a stray `/` behind in the draft.
    if (!command.keepOpen) setDraft("");
    command.run();
  };

  const addFiles = useCallback(
    async (files: readonly File[]): Promise<number> => {
      if (files.length === 0) return 0;
      const forConversation = conversationKey;
      const { attachments: added, errors } = await collectAttachments(files);
      // Reading and downscaling take long enough for the user to have moved on.
      // The cleanup that runs on a conversation change has already been and
      // gone, so anything arriving now would never be revoked.
      if (forConversation !== conversationKeyRef.current) {
        for (const a of added) if (a.previewUrl) URL.revokeObjectURL(a.previewUrl);
        return 0;
      }
      if (added.length > 0) {
        // Only images mint one; a document has nothing to preview.
        objectUrls.current.push(...added.flatMap((a) => (a.previewUrl ? [a.previewUrl] : [])));
        setAttachments((prev) => [...prev, ...added]);
      }
      setError(errors[0]);
      return added.length;
    },
    [conversationKey],
  );

  /**
   * Pasted text lands in the box, unless there is enough of it to be a file.
   *
   * A long paste becomes a chip so the question stays visible above it. If the
   * chip cannot be made — the paste is past the document limit, or the reader
   * failed — the text goes inline after all: cluttering the box is a nuisance,
   * losing the paste is not.
   */
  const pasteText = useCallback(
    (text: string) => {
      const pasted = attachmentsRef.current.filter((a) => a.filename.startsWith("pasted-text"));
      const file = pastedTextFile(text, pasted.length + 1);
      if (file === null) {
        setDraft((prev) => appendPastedText(prev, text));
        return;
      }
      // Same guard as the attachment path: a paste that resolves after the
      // user moved on must not land in the next conversation's box.
      const forConversation = conversationKeyRef.current;
      void addFiles([file]).then((added) => {
        if (added > 0 || forConversation !== conversationKeyRef.current) return;
        setDraft((prev) => appendPastedText(prev, text));
        // Only this file's refusal is moot now; an image pasted alongside
        // keeps its own error.
        setError((prev) => (prev?.startsWith(file.name) ? undefined : prev));
      });
    },
    [addFiles],
  );

  /**
   * Paste anywhere on the page and it lands in the composer.
   *
   * Registered on the document because the target is, by definition, not this
   * box. Text is appended and the caret follows it to the end, so the next
   * keystroke continues the pasted line rather than landing in front of it.
   */
  useEffect(() => {
    const onPaste = (e: ClipboardEvent) => {
      if (!e.clipboardData || !composerOwnsPaste(e.target)) return;
      const files = Array.from(e.clipboardData.files);
      const text = e.clipboardData.getData("text/plain");
      if (files.length === 0 && text === "") return;
      e.preventDefault();
      if (files.length > 0) void addFiles(files);
      if (text !== "") pasteText(text);
      // After the state commit, or the caret would be placed in the old value.
      requestAnimationFrame(() => {
        const el = textarea.current;
        if (!el) return;
        el.focus();
        el.setSelectionRange(el.value.length, el.value.length);
        setCaret(el.value.length);
      });
    };
    document.addEventListener("paste", onPaste);
    return () => document.removeEventListener("paste", onPaste);
  }, [addFiles, pasteText, textarea]);

  /** Writes dropped text at the caret, as the browser would have. */
  const dropText = (text: string) => {
    const el = textarea.current;
    const start = el?.selectionStart ?? draft.length;
    const next = insertAt(draft, text, start, el?.selectionEnd ?? start);
    setDraft(next.text);
    setCaret(next.caret);
    restoreCaret(next.caret);
  };

  const forget = (urls: (string | undefined)[]) => {
    const real = urls.filter((u): u is string => u !== undefined);
    for (const url of real) URL.revokeObjectURL(url);
    objectUrls.current = objectUrls.current.filter((u) => !real.includes(u));
  };

  const removeAttachment = (index: number) => {
    setAttachments((prev) => {
      const target = prev[index];
      if (target) forget([target.previewUrl]);
      return prev.filter((_, i) => i !== index);
    });
  };

  const submit = () => {
    // A path on offer is the thing being asked for, so Enter writes it. The
    // token itself is ordinary text either way, so nothing is lost by sending
    // it — it just would not be the path the user was half-way through naming.
    if (pathOpen && pathRows.length > 0) {
      choosePath(pathCursor);
      return;
    }
    // A command Borg runs itself wins over the menu: a bare `/goal` asks for
    // the goal, and must not run whichever row the query happens to rank
    // first (a settings toggle). It runs even mid-turn, as an instruction to
    // Borg rather than prose to steer in.
    const typed = draft.trim();
    if (attachments.length === 0 && typed !== "" && onCommand?.(typed)) {
      history.current.push(typed);
      historyAt.current = null;
      setDraft("");
      return;
    }
    // While a row is on offer the token is a menu query, not prompt text: the
    // daemon has no slash handler, so sending it would look like it worked and
    // do nothing. With nothing on offer it is just text — otherwise `/tmp` and
    // `/etc/hosts` would be unsendable, and Enter a silent no-op.
    if (slashOpen && slashResults.length > 0) {
      runCommand(slashCursor);
      return;
    }
    const text = typed;
    if (text === "" && attachments.length === 0) return;
    // Before the conversation is open there is nowhere to send to, so the
    // message waits for it rather than vanishing.
    const hold = awaitingInput || !ready;
    if (hold) {
      // Held, not steered: the agent is parked on the prompt above, so a steer
      // would sit in a channel nobody is draining and surface much later as a
      // synthetic mid-turn message.
      setQueued((prev) =>
        prev
          ? {
              text: mergeQueued(prev.text, text),
              attachments: [...prev.attachments, ...attachments],
            }
          : { text, attachments },
      );
    } else if (busy) {
      steer(text);
    } else {
      onSend(text, attachments);
    }
    history.current.push(text);
    historyAt.current = null;
    setDraft("");
    // The bytes are already read and on their way; only the previews are left
    // to release, and the sent turn renders a count rather than thumbnails.
    // Queued previews are the exception — they have to survive to be handed
    // back if the queue is returned to the box.
    if (!hold) forget(attachments.map((a) => a.previewUrl));
    setAttachments([]);
    setError(undefined);
  };

  /**
   * Steers the running turn, and hands the text back if the turn did not take
   * it.
   *
   * Added to whatever is in the box by then, never written over it: the answer
   * arrives after the user has had time to start the next sentence.
   */
  const steer = (text: string) => {
    const forConversation = conversationKeyRef.current;
    const forDraft = draftKeyRef.current;
    void Promise.resolve(onSteer(text))
      .catch((err: unknown) => {
        console.warn("steer failed", err);
        return false;
      })
      .then((taken) => {
        if (taken !== false) return;
        if (forConversation === conversationKeyRef.current) {
          setDraft((prev) => appendPastedText(prev, text));
          return;
        }
        // The user has moved on. The text goes back to the conversation it was
        // typed in, where its draft is waiting on their return.
        const stored = appendPastedText(readDraft(forDraft), text);
        sessionStorage.setItem(forDraft, stored);
        publishDraft(forDraft, stored);
      });
  };

  /**
   * Sends the queue once the turn ends.
   *
   * Gated on `busy` and `ready`, deliberately not on `awaitingInput`: while a
   * prompt is parked `busy` is true, so this cannot fire early, and a turn that
   * errors out leaving an unanswered prompt cell behind cannot strand the queue
   * forever. `onSend` sets `busy` synchronously, so it cannot fire twice.
   */
  useEffect(() => {
    if (busy || !ready || !queued) return;
    onSend(queued.text, queued.attachments);
    forget(queued.attachments.map((a) => a.previewUrl));
    setQueued(undefined);
  }, [busy, ready, queued, onSend]);

  /** Hands the queue back to the box. Cancelling must not bin what was typed. */
  const restoreQueued = () => {
    const pending = queuedRef.current;
    if (!pending) return;
    setDraft((prev) => appendPastedText(prev, pending.text));
    setAttachments((prev) => [...prev, ...pending.attachments]);
    setQueued(undefined);
  };

  /** Stop: drop the turn, and give back anything waiting behind it. */
  const stop = () => {
    restoreQueued();
    onCancel();
  };

  const onKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    // While a menu is open the arrows belong to it, not to the caret or to
    // history recall.
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      const step = e.key === "ArrowDown" ? 1 : -1;
      if (pathOpen && pathRows.length > 0) {
        e.preventDefault();
        const last = pathRows.length - 1;
        setPathCursor((c) => Math.max(0, Math.min(last, c + step)));
        return;
      }
      if (slashOpen && slashResults.length > 0) {
        e.preventDefault();
        const last = slashResults.length - 1;
        setSlashCursor((c) => Math.max(0, Math.min(last, c + step)));
        return;
      }
    }
    // A written path is one thing, so Backspace takes it away as one thing —
    // unpicking `@crates/cortex/src/agent` a character at a time is nobody's
    // idea of editing.
    if (e.key === "Backspace" && !e.shiftKey) {
      const el = e.currentTarget;
      const whole =
        el.selectionStart === el.selectionEnd ? backspacePathToken(draft, el.selectionStart) : null;
      if (whole) {
        e.preventDefault();
        setDraft(whole.text);
        setCaret(whole.caret);
        restoreCaret(whole.caret);
        return;
      }
    }
    if (isSubmitEnter(e) && !e.shiftKey) {
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

        {/* Where the held text waits. Same chip vocabulary as an attachment,
            because it is the same idea: something staged, not yet sent. It
            exists only while something is queued — no toggle, no setting. */}
        {queued && (
          <div className="mb-2 flex items-center px-1">
            <span className="flex h-[26px] max-w-full items-center gap-1.5 rounded-[8px] border border-line2 bg-surface px-2 text-[12px] text-ink2">
              <span className="truncate">Sends next · {draftTitle(queued.text)}</span>
              <button
                type="button"
                aria-label="Return to composer"
                onClick={restoreQueued}
                className="flex flex-none cursor-pointer items-center border-0 bg-transparent p-0 text-ink3 hover:text-ink"
              >
                <CloseIcon size={11} />
              </button>
            </span>
          </div>
        )}

        {attachments.length > 0 && (
          <div className="mb-2 flex flex-wrap items-center gap-2">
            {attachments.map((a, i) =>
              // An image is its own label, so it shows as a thumbnail. A
              // document has nothing to look at, so it shows as its name —
              // which is the part anyone needs to check anyway.
              // Branching on `kind` rather than on `previewUrl`: the kind is
              // what was decided about the file, the URL is one consequence.
              a.kind === "image" && a.previewUrl ? (
                <div
                  key={`${a.filename}-${i}`}
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
              ) : (
                <span
                  key={`${a.filename}-${i}`}
                  className="flex h-[26px] items-center gap-1.5 rounded-[8px] border border-line2 bg-surface px-2 text-[12px] text-ink2"
                >
                  {a.filename}
                  <button
                    type="button"
                    aria-label={`Remove ${a.filename}`}
                    onClick={() => removeAttachment(i)}
                    className="flex cursor-pointer items-center border-0 bg-transparent p-0 text-ink3 hover:text-ink"
                  >
                    <CloseIcon size={11} />
                  </button>
                </span>
              ),
            )}
          </div>
        )}

        {/* A row that changes a setting is picked by watching it land, so a
            failed write has to say so here too — the menu closes on the way
            out, taking the row's own feedback with it. */}
        {saveError && (
          <div className="mb-2 px-1 text-[11.5px] leading-[1.5] text-alert">{saveError}</div>
        )}

        {pathOpen && pathRows.length > 0 && (
          <CompletionList
            id={PATH_LIST_ID}
            label="Files"
            rows={pathRows.map((row) => ({
              id: row.path,
              label: row.path,
              hint: row.isDir ? "Folder" : undefined,
            }))}
            cursor={pathCursor}
            onHover={setPathCursor}
            onChoose={choosePath}
          />
        )}

        {slashOpen && (
          <CompletionList
            id={SLASH_LIST_ID}
            label="Commands"
            emptyText="No command matches. Press Enter to send it as a message."
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
            if (carriesFiles(e.dataTransfer)) {
              e.preventDefault();
              setDragging(true);
              return;
            }
            // Text over the box itself is the browser's to place. Anywhere else
            // on the frame a drop is only delivered if it is asked for here.
            if (carriesText(e.dataTransfer) && e.target !== textarea.current) e.preventDefault();
          }}
          onDragLeave={() => setDragging(false)}
          onDrop={(e) => {
            setDragging(false);
            const { files, text } = readDrop(e.dataTransfer);
            if (files.length > 0) {
              e.preventDefault();
              void addFiles(files);
              return;
            }
            if (text === "") return;
            // A plain text drag onto the box lands at the caret natively. One
            // that came with files would be opened as a file instead.
            if (e.target === textarea.current && !carriesFiles(e.dataTransfer)) return;
            e.preventDefault();
            dropText(text);
          }}
          className={`flex items-end gap-3 border bg-surface2 py-2 pr-2 pl-[19px] ${
            isMobile ? "rounded-[22px]" : "rounded-[13px]"
          } ${dragging ? "border-focus" : "border-field focus-within:border-focus"}`}
        >
          {/* The input is visually hidden but still focusable, so the paperclip
              is reachable by Tab; the label draws the focus ring for it. */}
          <label className="flex h-[29px] flex-none cursor-pointer items-center rounded-[6px] text-ink3 hover:text-ink2 has-focus-visible:text-ink has-focus-visible:outline-2 has-focus-visible:outline-offset-2 has-focus-visible:outline-focus">
            <AttachIcon size={15} />
            <input
              type="file"
              // Text first, because the picker shows the first entry as its
              // filter. `text/*` covers most of it; the rest are the types a
              // browser labels `application/…` despite being text.
              accept="text/*,.md,.json,.yaml,.yml,.toml,.csv,.log,.rs,.ts,.tsx,.js,.jsx,.py,.go,.sh,.sql,image/png,image/jpeg,image/gif,image/webp"
              multiple
              aria-label="Attach a file"
              className="sr-only"
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
            // Says what Enter will do before it is pressed — the whole reason
            // holding the text back does not read as swallowing it.
            placeholder={
              awaitingInput ? "Send after this…" : busy ? "Steer Borg…" : "Ask Borg anything"
            }
            // Focus never leaves the box, so the row Enter would take is only
            // announced if the box names it. No `role="combobox"`: that would
            // replace the textarea's own role, and it is a text box first —
            // which is also why there is no `aria-expanded`, a state a text box
            // does not support.
            aria-controls={
              pathOpen && pathRows.length > 0 ? PATH_LIST_ID : slashOpen ? SLASH_LIST_ID : undefined
            }
            aria-activedescendant={
              pathOpen && pathRows.length > 0
                ? rowDomId(PATH_LIST_ID, pathCursor)
                : slashOpen && slashResults.length > 0
                  ? rowDomId(SLASH_LIST_ID, slashCursor)
                  : undefined
            }
            value={draft}
            onChange={(e) => {
              setDraft(e.target.value);
              setCaret(e.target.selectionStart ?? e.target.value.length);
            }}
            onKeyUp={syncCaret}
            onClick={syncCaret}
            onSelect={syncCaret}
            onKeyDown={onKeyDown}
            onPaste={(e) => {
              const files = Array.from(e.clipboardData.files);
              const text = e.clipboardData.getData("text/plain");
              // A short text paste is the browser's to insert at the caret;
              // only a long one is taken out of its hands.
              const asFile = files.length === 0 && pastedTextFile(text) !== null;
              if (files.length === 0 && !asFile) return;
              e.preventDefault();
              if (files.length > 0) void addFiles(files);
              if (asFile) pasteText(text);
            }}
          />
          <VoiceInputButton
            // Another conversation gets a new microphone, so a recording never
            // lands in another draft. Not `draftKey`: it changes when a new
            // conversation is issued its id, which would drop the recording.
            key={conversationKey}
            onText={(spoken) => {
              setDraft((prev) => (prev.trim() === "" ? spoken : `${prev.trimEnd()} ${spoken}`));
              focusComposer();
            }}
            onError={setError}
          />
          {busy && draft.trim() === "" ? (
            <Pill variant="danger" square onClick={stop}>
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
