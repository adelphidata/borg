import type { ReactNode } from "react";
import { ChevronIcon } from "@borg/ui";

export const ROW_BUTTON =
  "flex w-full cursor-pointer items-center border-0 bg-transparent text-left font-[inherit]";

/** The fold's chevron; it turns to point down while the fold is open. */
export function Chevron({ open }: { open: boolean }) {
  return (
    <span
      className={`flex flex-none transition-transform duration-[180ms] ${open ? "[transform:rotate(90deg)]" : ""}`}
    >
      <ChevronIcon size={13} className={open ? "text-neon" : "text-ink3"} />
    </span>
  );
}

/** A count pill beside a fold's title: neon for state, warn for something to set up. */
export function FoldBadge({
  tone = "neon",
  children,
}: {
  tone?: "neon" | "warn";
  children: ReactNode;
}) {
  return (
    <span
      className={`flex-none rounded-full px-2 py-[3px] text-[10.5px] leading-none font-medium ${
        tone === "warn" ? "bg-warn-soft text-warn" : "bg-nav-on-bg text-neon-ink"
      }`}
    >
      {children}
    </span>
  );
}

/**
 * One category of a long list, folded: a title, any badges, how many rows sit
 * inside, and a chevron. The body is in the DOM only while open. Meant to
 * stack inside a `CategoryCard`.
 */
export function CategoryFold({
  id,
  name,
  badges,
  total,
  open,
  onToggle,
  bodyClassName = "",
  children,
}: {
  /** Unique on the page; the body's element id derives from it. */
  id: string;
  name: string;
  badges?: ReactNode;
  total: number;
  open: boolean;
  onToggle: () => void;
  bodyClassName?: string;
  children: ReactNode;
}) {
  return (
    <section className="border-b border-line2 last:border-b-0">
      <button
        type="button"
        aria-expanded={open}
        // Only while the body is in the DOM; a dangling id reference is worse than none.
        aria-controls={open ? id : undefined}
        className={`${ROW_BUTTON} gap-4 px-4 py-3.5 transition-[background] duration-150 hover:bg-surface3 ${
          open ? "bg-surface3" : ""
        }`}
        onClick={onToggle}
      >
        <span className="min-w-0 flex-1 truncate text-[13.5px] leading-[1.3] font-medium text-ink">
          {name}
        </span>
        {badges && <span className="flex flex-none items-center gap-1.5">{badges}</span>}
        <span className="w-7 flex-none text-right text-[12px] leading-none text-ink3 tabular-nums">
          {total}
        </span>
        <Chevron open={open} />
      </button>
      {open && (
        <div id={id} className={`border-t border-line2 bg-surface ${bodyClassName}`}>
          {children}
        </div>
      )}
    </section>
  );
}

/** The rounded card a stack of `CategoryFold`s sits in. */
export function CategoryCard({ children }: { children: ReactNode }) {
  return (
    <div className="overflow-hidden rounded-[12px] border border-line2 bg-surface2">{children}</div>
  );
}
