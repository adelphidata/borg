import { Component, type ErrorInfo, type ReactNode } from "react";
import { Pill } from "@borg/ui";

interface Props {
  /** A new value clears the failure: moving to another page is a fresh try. */
  resetKey: string;
  children: ReactNode;
}

interface State {
  error?: Error;
  /** The `resetKey` the failure happened under. */
  failedAt?: string;
}

/**
 * Keeps one page's crash inside that page.
 *
 * Without it a single render error unmounts the whole tree and leaves an empty
 * window — no sidebar to leave by, and on desktop no visible way to reload. The
 * boundary sits around the routed page only, so navigation still works, and a
 * route change is a fresh attempt.
 */
export class PageErrorBoundary extends Component<Props, State> {
  state: State = {};

  static getDerivedStateFromError(error: Error): Partial<State> {
    return { error };
  }

  static getDerivedStateFromProps(props: Props, state: State): Partial<State> | null {
    if (state.error === undefined) return null;
    if (state.failedAt === undefined) return { failedAt: props.resetKey };
    return props.resetKey === state.failedAt ? null : { error: undefined, failedAt: undefined };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    console.error("page crashed", error, info.componentStack);
  }

  render() {
    const { error } = this.state;
    if (!error) return this.props.children;
    return (
      <div className="flex min-w-0 flex-1 items-center justify-center p-8">
        <div className="flex max-w-[420px] flex-col items-start gap-3 rounded-xl border border-alert bg-alert-soft px-4 py-[15px]">
          <div className="text-[13px] leading-[1.3] font-medium text-ink">
            This page ran into a problem.
          </div>
          <div className="text-[12.5px] leading-[1.4] text-ink2">
            Your conversations are safe. Reload to try again, or pick another page.
          </div>
          <Pill variant="primary" onClick={() => window.location.reload()}>
            Reload
          </Pill>
          <details className="text-[11.5px] leading-[1.4] text-ink3">
            <summary className="cursor-pointer">Details</summary>
            <div className="mt-1.5 break-all">{error.message}</div>
          </details>
        </div>
      </div>
    );
  }
}
