/** Connection status: neon when fine, red when it needs the user. */
export function StatusDot({ needsAttention }: { needsAttention?: boolean }) {
  return needsAttention ? <span className="alert-dot" /> : <span className="neon-dot" />;
}
