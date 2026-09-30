import type { ReactNode } from "react";
import { Pill } from "@borg/ui";
import { useNative, usePlatform } from "../lib/platform";
import { signOut } from "../lib/session";
import type { Query } from "../lib/useDaemon";

/**
 * Renders the three outcomes of a daemon call so pages do not each invent their
 * own spinner and error copy.
 *
 * The two failures worth distinguishing are the two a user can act on:
 * borgd being down, and the browser's token being rejected. Everything else is
 * reported verbatim.
 */
export function DaemonState<T>({
  query,
  label,
  actions,
  children,
}: {
  /** A `useQuery` result. `refetch` is optional so a bare `Query` still fits. */
  query: Query<T> & { refetch?: () => void };
  /** What was being fetched, used in the failure line: "Could not load {label}." */
  label: string;
  /**
   * Ways out of a failure that only the page knows about, shown after the
   * remedies above. A page that fails to load is otherwise a dead end.
   */
  actions?: ReactNode;
  children: (data: T) => ReactNode;
}) {
  const { kind } = usePlatform();
  const native = useNative();

  if (query.state === "loading") {
    return <div className="text-[13px] leading-none text-ink3">Loading {label}…</div>;
  }

  if (query.state === "error") {
    const { error } = query;
    // The desktop shell starts the daemon itself, so the remedy there is a
    // button, not a command someone has to find a terminal for.
    const message = error.isUnavailable
      ? kind === "desktop"
        ? "Borg has stopped running."
        : "Borg is not running. Start it with `borg daemon`."
      : error.isUnauthenticated
        ? "This token was rejected. Issue a new one with `borg client-token issue web`."
        : error.message;

    return (
      <div className="flex flex-col items-start gap-3 rounded-xl border border-alert bg-alert-soft px-4 py-[15px]">
        <div className="text-[13px] leading-[1.3] font-medium text-ink">
          Could not load {label}.
        </div>
        <div className="text-[12.5px] leading-[1.4] text-ink2">{message}</div>
        {/* Starting Borg is only half the remedy: the call that failed has to
            run again, or the page keeps the outage on screen and the button
            looks like it did nothing. */}
        {kind === "desktop" && error.isUnavailable && (
          <Pill variant="primary" onClick={() => void native.boot.retry().then(query.refetch)}>
            Start Borg
          </Pill>
        )}
        {/* Only the browser holds a token, so only it can clear one. */}
        {kind === "web" && error.isUnauthenticated && (
          <Pill variant="danger" onClick={signOut}>
            Sign in again
          </Pill>
        )}
        {actions !== undefined && <div className="flex flex-wrap gap-2">{actions}</div>}
      </div>
    );
  }

  return <>{children(query.data)}</>;
}
