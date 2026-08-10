/**
 * The 6px dot that carries state in a list row.
 *
 * Every surface that shows liveness uses this one, so the vocabulary stays
 * fixed: neon means healthy, red means it wants you, grey means it is not
 * running. Callers pass a tone rather than a colour class — a page inventing
 * `bg-amber-400` is how a status vocabulary stops meaning anything.
 */
export type DotTone = "ok" | "alert" | "busy" | "paused" | "idle";

const TONES: Record<DotTone, string> = {
  ok: "bg-neon shadow-(--glow)",
  alert: "bg-alert",
  /** Working right now — the pulse is the difference from a settled `ok`. */
  busy: "animate-bpulse bg-neon",
  /** Stopped, but resumable. */
  paused: "bg-bar-track",
  /** Never ran, or finished and gone. */
  idle: "bg-ink3",
};

export function StatusDot({ tone = "ok", className }: { tone?: DotTone; className?: string }) {
  return (
    <span className={`h-1.5 w-1.5 flex-none rounded-full ${TONES[tone]} ${className ?? ""}`} />
  );
}
