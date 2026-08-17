import { useEffect, useRef } from "react";
import { ChevronDownIcon, CloseIcon, SearchIcon } from "./icons";

export interface FindBarProps {
  query: string;
  onQueryChange: (query: string) => void;
  /** Total matches found. */
  count: number;
  /** Which match is active, 1-based; 0 when there are none. */
  position: number;
  onNext: () => void;
  onPrevious: () => void;
  onClose: () => void;
  /**
   * Offered when older history is mounted out of the search's reach. Omit and
   * the bar says nothing about it — on a short conversation there is nothing to
   * say.
   */
  onSearchAll?: () => void;
}

function Step({
  label,
  onClick,
  disabled,
  up = false,
}: {
  label: string;
  onClick: () => void;
  disabled: boolean;
  up?: boolean;
}) {
  return (
    <button
      type="button"
      aria-label={label}
      disabled={disabled}
      onClick={onClick}
      className="flex h-6 w-6 flex-none cursor-pointer items-center justify-center rounded border-0 bg-transparent text-ink3 hover:text-ink disabled:cursor-default disabled:text-line"
    >
      <span className={up ? "rotate-180" : undefined}>
        <ChevronDownIcon size={13} />
      </span>
    </button>
  );
}

/**
 * Search within what is on screen.
 *
 * Floats over the top of the surface it searches rather than pushing it down: a
 * bar that reflowed the transcript would move the very line the user is looking
 * for. It carries no state — the count, the position, and the query all arrive
 * as props, so the same bar can search a transcript here and a log elsewhere.
 */
export function FindBar({
  query,
  onQueryChange,
  count,
  position,
  onNext,
  onPrevious,
  onClose,
  onSearchAll,
}: FindBarProps) {
  const box = useRef<HTMLInputElement>(null);
  // Opening a search box that is not focused makes the user click the thing
  // they just asked for.
  useEffect(() => box.current?.focus(), []);

  const empty = query !== "" && count === 0;

  return (
    <div
      role="search"
      className="absolute top-3 right-4 z-10 flex items-center gap-1.5 rounded-[10px] border border-line bg-surface2 py-1.5 pr-1.5 pl-3 shadow-(--plateGlow)"
    >
      <SearchIcon size={13} />
      <input
        ref={box}
        aria-label="Find in conversation"
        placeholder="Find"
        className="w-[150px] min-w-0 border-0 bg-transparent text-[12.5px] text-ink outline-none select-text placeholder:text-ink3"
        value={query}
        onChange={(e) => onQueryChange(e.target.value)}
        onKeyDown={(e) => {
          // Enter walks the results, which is what every find bar does.
          if (e.key === "Enter") {
            e.preventDefault();
            if (e.shiftKey) onPrevious();
            else onNext();
          }
          // Escape closes the bar from here rather than only through the app's
          // keyboard layer: that layer runs whichever handler registered last,
          // and a turn starting while the bar is open re-registers the
          // composer's cancel above it. Escape would then stop the turn and
          // leave the bar sitting open. Preventing the default here is what
          // stops the layer seeing it at all.
          if (e.key === "Escape") {
            e.preventDefault();
            onClose();
          }
        }}
      />
      <span
        className={`min-w-[46px] text-right text-[11px] leading-none tabular-nums ${
          empty ? "text-alert" : "text-ink3"
        }`}
      >
        {query === "" ? "" : empty ? "None" : `${position}/${count}`}
      </span>
      <Step label="Previous match" onClick={onPrevious} disabled={count === 0} up />
      <Step label="Next match" onClick={onNext} disabled={count === 0} />
      {onSearchAll && (
        <button
          type="button"
          onClick={onSearchAll}
          className="cursor-pointer rounded border-0 bg-transparent px-1.5 text-[11px] leading-none text-ink3 hover:text-ink"
        >
          Older too
        </button>
      )}
      <button
        type="button"
        aria-label="Close find"
        onClick={onClose}
        className="flex h-6 w-6 flex-none cursor-pointer items-center justify-center rounded border-0 bg-transparent text-ink3 hover:text-ink"
      >
        <CloseIcon size={13} />
      </button>
    </div>
  );
}
