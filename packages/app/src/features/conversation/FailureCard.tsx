import { useState } from "react";
import { useNavigate } from "react-router-dom";
import { Pill } from "@borg/ui";
import { classifyFailure } from "./sendFailure";

/**
 * A failed turn, told as a layer and a next action.
 *
 * The old card was the error string in a red box: true, and useless to anyone
 * who did not write the daemon. This one leads with which part of the stack
 * broke — the provider, the network, the disk — and then offers only the
 * actions that could actually help. A key the provider refuses gets Settings,
 * not Retry; a dropped socket gets Retry, not Settings.
 *
 * The raw text never leaves, it only steps back: a classified failure folds it
 * into a closed disclosure. Copying is the convenience, not the way through —
 * `navigator.clipboard` is absent on an insecure origin, which is exactly the
 * gateway-served SPA on a LAN address, and a misfiled guess must never be the
 * only thing a maintainer can read.
 */
export function FailureCard({
  error,
  onRetry,
  live = false,
}: {
  error: { message: string } | undefined;
  /** Re-runs the failed turn. Omit where nothing can safely be re-run. */
  onRetry?: () => void;
  /**
   * True for the one card reporting a failure as it happens. Settled failures
   * in the transcript stay quiet — reopening a conversation with three old
   * errors in it must not fire three announcements at a screen reader.
   */
  live?: boolean;
}) {
  const navigate = useNavigate();
  // Keyed by the message it was copied from. The live card never unmounts, so a
  // plain boolean would leave the next, different failure already reading
  // "Copied" — a receipt for something the user has not done.
  const [copyResult, setCopyResult] = useState<{ of: string; state: true | "failed" }>();
  const failure = classifyFailure(error);
  if (!failure || !error) return null;
  const copied = copyResult?.of === error.message ? copyResult.state : false;

  const copy = () => {
    const clipboard = navigator.clipboard;
    if (!clipboard) {
      // Not a silent no-op: the button says so, and the text is in the
      // disclosure below either way.
      setCopyResult({ of: error.message, state: "failed" });
      return;
    }
    void clipboard
      .writeText(error.message)
      .then(() => setCopyResult({ of: error.message, state: true }))
      .catch((reason: unknown) => {
        console.warn("copying the error details failed", reason);
        setCopyResult({ of: error.message, state: "failed" });
      });
  };

  return (
    <div
      role={live ? "alert" : undefined}
      className="rounded-xl border border-alert bg-alert-soft px-4 py-3 text-[12.5px] leading-[1.5] text-ink2 select-text"
    >
      {failure.title && (
        <div className="pb-1 text-[12.5px] leading-[1.4] font-medium text-ink">{failure.title}</div>
      )}
      {failure.message}
      <div className="mt-3 flex flex-wrap gap-2">
        {/* Retry leads only where a repeat could work; elsewhere the fix does. */}
        {failure.retryable && onRetry && (
          <Pill variant="primary" square onClick={onRetry}>
            Try again
          </Pill>
        )}
        {failure.settings && (
          <Pill square onClick={() => navigate("/settings")}>
            Open settings
          </Pill>
        )}
        <Pill square onClick={copy}>
          {copied === true ? "Copied" : copied === "failed" ? "Could not copy" : "Copy error"}
        </Pill>
      </div>
      {/* Only when the card is telling its own story — an unclassified failure
          already shows this exact text above, and repeating it teaches nothing. */}
      {failure.layer !== "unknown" && (
        <details className="mt-2.5">
          <summary className="cursor-pointer text-[11.5px] leading-none text-ink3">Details</summary>
          <pre className="mt-2 mb-0 overflow-x-auto text-[11.5px] leading-[1.45] whitespace-pre-wrap text-ink3">
            {error.message}
          </pre>
        </details>
      )}
    </div>
  );
}
