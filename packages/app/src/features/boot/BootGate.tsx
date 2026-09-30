import { useEffect, useState, type ReactNode } from "react";
import { Meter, Pill } from "@borg/ui";
import { useNative } from "../../lib/platform";
import type { BootStatus } from "../../lib/native";

/** How long a boot may take before it is worth showing at all. */
const FLASH_GRACE_MS = 400;

const TITLE =
  "mx-0 mb-0 text-[30px] leading-[1.2] font-light tracking-[-0.03em] text-pretty text-ink";
const BODY = "mt-3.5 mb-0 text-[14.5px] leading-[1.62] text-pretty text-ink2";

/** Subscribes to the shell's boot status. Web surfaces get "ready" at once. */
export function useBootStatus(): BootStatus {
  const native = useNative();
  const [status, setStatus] = useState<BootStatus>({
    state: "starting",
    message: "Starting Borg…",
    progress: 0.1,
    canRetry: false,
  });
  useEffect(() => native.boot.subscribe(setStatus), [native]);
  return status;
}

/**
 * Holds the app back until Borg itself is running.
 *
 * The daemon is the product; the window is a view onto it. Opening straight
 * into a shell whose every page says "could not reach Borg" tells the user a
 * subsystem failed and leaves them to guess the remedy. This waits instead,
 * says what it is doing in one line, and — if it cannot — offers the two
 * things a person can actually do: try again, or look around anyway.
 *
 * A boot that finishes quickly is never seen: the screen only appears once the
 * wait outlasts the grace period, so a warm start goes straight to the app.
 */
export function BootGate({ children }: { children: ReactNode }) {
  const native = useNative();
  const status = useBootStatus();
  const [waited, setWaited] = useState(false);
  const [dismissed, setDismissed] = useState(false);

  useEffect(() => {
    const timer = setTimeout(() => setWaited(true), FLASH_GRACE_MS);
    return () => clearTimeout(timer);
  }, []);

  if (status.state === "ready" || dismissed) return <>{children}</>;
  if (status.state === "starting" && !waited) return null;

  const failed = status.state === "failed";

  return (
    <div className="flex h-full items-center justify-center bg-surface [-webkit-app-region:drag]">
      <div className="flex w-[420px] max-w-[calc(100vw-48px)] flex-col px-[38px] [-webkit-app-region:no-drag]">
        <h1 className={TITLE}>{failed ? "Borg could not start." : "Starting Borg…"}</h1>
        <p className={BODY}>
          {failed ? status.message : "Waking the part of me that thinks. One moment."}
        </p>
        {!failed && <Meter className="mt-6" percent={Math.round(status.progress * 100)} />}
        {failed && (
          <div className="mt-[26px] flex items-center gap-3">
            {status.canRetry && (
              <Pill
                variant="primary"
                sizeClasses="px-[22px] py-[11px] text-[13px]"
                onClick={() => native.boot.retry()}
              >
                Try again
              </Pill>
            )}
            <button
              type="button"
              className="cursor-pointer border-0 bg-transparent p-0 text-[12.5px] leading-none text-ink3 hover:text-ink"
              onClick={() => setDismissed(true)}
            >
              Look around anyway
            </button>
          </div>
        )}
      </div>
    </div>
  );
}
