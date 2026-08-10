/**
 * The 6px dot that carries state in a list row.
 *
 * Every surface that shows liveness uses this one, so the vocabulary stays
 * fixed: neon means healthy, red means it wants you, grey means it is not
 * running. Callers pass a tone rather than a colour class — a page inventing
 * `bg-amber-400` is how a status vocabulary stops meaning anything.
 */
export type DotTone = "ok" | "alert" | "busy" | "working" | "draft" | "paused" | "idle";

const TONES: Record<DotTone, string> = {
  ok: "bg-neon shadow-(--glow)",
  alert: "bg-alert",
  /** Working right now — the pulse is the difference from a settled `ok`. */
  busy: "animate-bpulse bg-neon",
  /**
   * A conversation with a turn in flight. Same news as `busy`, but still: a
   * list is read by scanning it, and a column of pulsing dots is a column of
   * things pulling at the eye. Filled versus hollow carries the difference
   * without any motion.
   */
  working: "bg-neon",
  /** Started and never sent. The quietest mark the vocabulary has. */
  draft: "border border-ink3 bg-transparent",
  /** Stopped, but resumable. */
  paused: "bg-bar-track",
  /** Never ran, or finished and gone. */
  idle: "bg-ink3",
};

export function StatusDot({
  tone = "ok",
  className,
  label,
}: {
  tone?: DotTone;
  className?: string;
  /**
   * What the dot means, for a reader who cannot see it. Omit where the dot only
   * repeats text that is already in the row — a screen reader announcing
   * "Running, Running" is worse than silence.
   */
  label?: string;
}) {
  return (
    <span
      role={label ? "img" : undefined}
      aria-label={label}
      className={`h-1.5 w-1.5 flex-none rounded-full ${TONES[tone]} ${className ?? ""}`}
    />
  );
}
