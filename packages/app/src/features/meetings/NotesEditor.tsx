import { useAutoGrow } from "../../lib/useAutoGrow";
import type { NotesDraft } from "./useNotesDraft";

/**
 * Where the user writes during and after a meeting. The summary folds these
 * in, so it is a plain box with no formatting bar: what matters is getting
 * the words down.
 */
export function NotesEditor({ draft, maxPx = 640 }: { draft: NotesDraft; maxPx?: number }) {
  const ref = useAutoGrow(draft.text, maxPx);
  return (
    <div className="flex flex-col gap-1.5">
      <textarea
        ref={ref}
        aria-label="Your notes"
        rows={3}
        value={draft.text}
        placeholder="Add your thoughts…"
        onChange={(e) => draft.change(e.target.value)}
        onBlur={draft.flush}
        className="w-full resize-none border-0 bg-transparent p-0 text-[14.5px] leading-[1.6] text-ink outline-none select-text placeholder:text-ink3"
      />
      {draft.error && <div className="text-[12px] leading-[1.45] text-alert">{draft.error}</div>}
    </div>
  );
}
