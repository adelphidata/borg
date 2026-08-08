/** Connection status: neon when fine, red when it needs the user. */
export function StatusDot({ needsAttention }: { needsAttention?: boolean }) {
  return needsAttention ? (
    <span className="h-1.5 w-1.5 flex-none rounded-full bg-alert" />
  ) : (
    <span className="h-1.5 w-1.5 rounded-full bg-neon shadow-(--glow)" />
  );
}
