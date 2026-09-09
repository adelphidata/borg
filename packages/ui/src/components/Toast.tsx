import { useEffect, useRef } from "react";

export interface ToastProps {
  /**
   * Identity of the notice on screen. A new id restarts the dismissal clock,
   * so two notices that happen to read the same do not share one deadline.
   */
  id?: string | number;
  /** What happened, in the user's words. Absent means nothing is showing. */
  message: string | undefined;
  /** The one way to take it back, e.g. "Undo". Omit for a bare notice. */
  actionLabel?: string;
  onAction?: () => void;
  onDismiss: () => void;
  /** How long the notice stands before it fades on its own. */
  durationMs?: number;
  /**
   * Clearance below the notice, as a CSS length. Surfaces with their own
   * bottom chrome — the mobile tab bar — pass enough to sit above it.
   */
  bottomInset?: string;
}

/**
 * A single transient notice at the foot of the window.
 *
 * One at a time on purpose: a stack of toasts is a notification centre, and
 * this exists for the opposite reason — to let a reversible action happen
 * *without* a confirmation dialog in front of it. The action goes through
 * immediately, and the way back sits here for a few seconds.
 *
 * Presentational, like everything in this package: it owns the dismissal clock
 * and nothing else.
 */
export function Toast({
  id,
  message,
  actionLabel,
  onAction,
  onDismiss,
  durationMs = 7_000,
  bottomInset = "1.5rem",
}: ToastProps) {
  // The latest callback, read when the timer fires. Depending on `onDismiss`
  // directly would restart the clock on every render that passed a fresh
  // arrow, and the notice would never leave.
  const onDismissRef = useRef(onDismiss);
  onDismissRef.current = onDismiss;

  useEffect(() => {
    if (message === undefined) return;
    const timer = setTimeout(() => onDismissRef.current(), durationMs);
    return () => clearTimeout(timer);
  }, [id, message, durationMs]);

  if (message === undefined) return null;

  return (
    // `status`, not `alert`: this reports something the user just did, and
    // should not cut across whatever a screen reader is already saying.
    <div
      role="status"
      aria-live="polite"
      style={{ bottom: bottomInset }}
      className="pointer-events-none fixed inset-x-0 z-50 flex justify-center px-4"
    >
      <div className="pointer-events-auto flex max-w-full items-center gap-3 rounded-[10px] border border-line2 bg-surface2 py-2 pr-2 pl-3.5 shadow-(--glow)">
        <span className="min-w-0 truncate text-[12.5px] leading-none text-ink2">{message}</span>
        {actionLabel && onAction && (
          <button
            type="button"
            onClick={onAction}
            className="flex-none cursor-pointer rounded-md border-0 bg-transparent px-2 py-1.5 text-[12.5px] leading-none font-medium text-neon-ink hover:bg-nav-on-bg"
          >
            {actionLabel}
          </button>
        )}
      </div>
    </div>
  );
}
