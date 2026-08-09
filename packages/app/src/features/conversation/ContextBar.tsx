import { BranchIcon, Pill } from "@borg/ui";

/** Only worth showing once the window is filling up. */
const SHOW_AT = 0.5;
/** Where "getting full" turns into "do something about it". */
const WARN_AT = 0.8;

export interface ContextBarProps {
  context: { used: number; window: number } | undefined;
  busy: boolean;
  onCompact: () => void;
  onFork: () => void;
  compacting: boolean;
}

/**
 * How full the context window is, and the two ways out of it.
 *
 * Hidden below half — a meter that is always on screen is just chrome. It
 * appears when the number starts to matter, which is also when Compact and
 * Fork become the actions a user actually wants.
 */
export function ContextBar({ context, busy, onCompact, onFork, compacting }: ContextBarProps) {
  if (!context || context.window === 0) return null;
  const ratio = context.used / context.window;
  if (ratio < SHOW_AT) return null;

  const percent = Math.min(100, Math.round(ratio * 100));
  const warn = ratio >= WARN_AT;

  return (
    <div className="mb-2 flex items-center gap-3 rounded-[10px] border border-line2 bg-surface2 px-3.5 py-2">
      <div className="min-w-0 flex-1">
        <div className="flex items-baseline gap-2">
          <span className="text-[11px] leading-none font-medium tracking-[0.12em] text-ink3 uppercase">
            Context
          </span>
          <span className={`text-[11.5px] leading-none ${warn ? "text-alert" : "text-ink3"}`}>
            {percent}% full
          </span>
        </div>
        <div className="mt-1.5 h-1 overflow-hidden rounded-full bg-bar-track">
          <div
            className={`h-full rounded-full ${warn ? "bg-alert" : "bg-neon"}`}
            style={{ width: `${percent}%` }}
          />
        </div>
      </div>
      <Pill square className="flex flex-none items-center gap-1.5" onClick={onFork}>
        <BranchIcon size={12} /> Fork
      </Pill>
      <Pill
        square
        className="flex-none"
        variant={warn ? "primary" : undefined}
        onClick={onCompact}
        disabled={busy || compacting}
      >
        {compacting ? "Compacting…" : "Compact"}
      </Pill>
    </div>
  );
}
