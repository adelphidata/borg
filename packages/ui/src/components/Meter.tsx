/**
 * A thin proportion bar.
 *
 * Three surfaces show "how much of a budget is gone" — context window, goal
 * tokens, per-model spend — and each one wants the same 4px track. The clamp
 * lives here so a caller passing 130% renders a full bar rather than one that
 * overflows its track.
 *
 * `warnAt` is opt-in: a share-of-total bar (this model is 60% of spend) has no
 * danger threshold, while a budget bar does.
 */
export function Meter({
  percent,
  warnAt,
  className,
}: {
  percent: number;
  /** Above this percentage the fill turns red. Omit for a neutral bar. */
  warnAt?: number;
  className?: string;
}) {
  const clamped = Math.max(0, Math.min(100, percent));
  const warn = warnAt !== undefined && clamped >= warnAt;
  return (
    <div className={`h-1 overflow-hidden rounded-full bg-bar-track ${className ?? ""}`}>
      <div
        className={`h-full rounded-full ${warn ? "bg-alert" : "bg-neon"}`}
        style={{ width: `${clamped}%` }}
      />
    </div>
  );
}
