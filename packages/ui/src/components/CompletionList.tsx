/**
 * The list a composer trigger opens above the input.
 *
 * Fully controlled: the caller owns the cursor, because the keys that move it
 * arrive at the textarea, not here. This package only knows what the list looks
 * like — which row is highlighted, and what a row says.
 *
 * Distinct from `Menu`, which is a pointer-driven row menu with no cursor. The
 * two look similar on purpose and behave differently on purpose.
 */

import { useEffect, useRef } from "react";
import { Truncated } from "./Truncated";

export interface CompletionRow {
  id: string;
  label: string;
  /** Live state in muted text after the label — `on` / `off` for a toggle. */
  detail?: string;
  /** Right-aligned group or category. */
  hint?: string;
}

export interface CompletionListProps {
  rows: CompletionRow[];
  /** Index of the highlighted row; Enter in the input takes this one. */
  cursor: number;
  onHover: (index: number) => void;
  onChoose: (index: number) => void;
  /** Names the list for a screen reader, e.g. "Commands". */
  label: string;
  /**
   * Shown in place of the rows when nothing matched. Omit it and an empty list
   * renders nothing at all — right for a menu whose rows arrive over the wire,
   * where "no rows yet" and "no rows" look the same for the first moment.
   */
  emptyText?: string;
  /**
   * Id of the listbox. The input that owns the keys points at it with
   * `aria-controls`, and names the highlighted row with `aria-activedescendant`
   * built from [`rowDomId`] — focus stays in the input, so that is the only way
   * a screen reader learns what Enter will take.
   */
  id: string;
}

const MAX_HEIGHT_PX = 232;

/** The DOM id of row `index` in the list with the given `id`. */
export function rowDomId(id: string, index: number): string {
  return `${id}-row-${index}`;
}

export function CompletionList({
  rows,
  cursor,
  onHover,
  onChoose,
  label,
  emptyText,
  id,
}: CompletionListProps) {
  const listRef = useRef<HTMLDivElement>(null);
  const empty = rows.length === 0;

  // Arrowing past the fold has to bring the row with it, or the highlight moves
  // somewhere the user cannot see and Enter takes a row they never read.
  useEffect(() => {
    listRef.current?.children[cursor]?.scrollIntoView({ block: "nearest" });
  }, [cursor, rows.length]);

  // An empty box with nothing to say is worse than no box: it takes space above
  // the input and reads as a list that failed to load.
  if (empty && emptyText === undefined) return null;

  return (
    <div
      ref={listRef}
      id={id}
      role="listbox"
      aria-label={label}
      style={{ maxHeight: MAX_HEIGHT_PX }}
      className="mb-2 overflow-y-auto rounded-[11px] border border-line bg-surface2 py-1 shadow-(--frameShadow)"
    >
      {empty && (
        <div className="px-3.5 py-3 text-center text-[12px] leading-[1.5] text-ink3">
          {emptyText}
        </div>
      )}
      {rows.map((row, i) => (
        <button
          key={row.id}
          id={rowDomId(id, i)}
          type="button"
          role="option"
          aria-selected={i === cursor}
          // Pointer-down rather than click: the textarea keeps focus, so the
          // trigger token is still there to clear when the row runs.
          onPointerDown={(e) => {
            e.preventDefault();
            onChoose(i);
          }}
          onMouseEnter={() => onHover(i)}
          className={`flex w-full cursor-pointer items-center gap-2.5 border-0 px-3.5 py-[6px] text-left ${
            i === cursor ? "bg-surface3" : "bg-transparent"
          }`}
        >
          <Truncated className="min-w-0 text-[13px] leading-[1.35] text-ink">{row.label}</Truncated>
          {row.detail && (
            <span className="flex-none text-[11px] leading-none text-ink3">{row.detail}</span>
          )}
          <span className="min-w-0 flex-1" />
          {row.hint && (
            <span className="flex-none text-[11px] leading-none text-ink3">{row.hint}</span>
          )}
        </button>
      ))}
    </div>
  );
}
