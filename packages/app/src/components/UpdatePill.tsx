import { useEffect, useRef, useState } from "react";
import { Pill } from "@borg/ui";
import { useNative } from "../lib/platform";
import { useKeybind } from "../lib/keybinds";
import { showNotice } from "../lib/notice";
import type { UpdateStatus } from "../lib/native";

/**
 * Why a check failed, in words a person can act on. The updater's own message
 * is a network error or an HTTP status, and neither says what to do next.
 */
export function checkFailureReason(message: string | undefined): string {
  const offline =
    (typeof navigator !== "undefined" && navigator.onLine === false) ||
    /net::|ENOTFOUND|ECONN|ETIMEDOUT|EAI_AGAIN|network|offline|internet/i.test(message ?? "");
  return offline
    ? "Check your internet connection, then try again."
    : "Try again in a little while.";
}

/**
 * The only thing an update is allowed to do to the user.
 *
 * No modal, no forced restart, no progress bar competing with the transcript.
 * The download happens silently and this appears once a build is ready, in the
 * corner, until it is convenient. Everything else about updating is invisible.
 *
 * The one exception is a check the user asked for. A menu item that does
 * nothing visible reads as broken, so that check answers — up to date, or why
 * it could not tell — through the shell's notice. Automatic checks stay silent.
 */
export function UpdatePill() {
  const native = useNative();
  const [status, setStatus] = useState<UpdateStatus>({ state: "idle" });
  const [dismissed, setDismissed] = useState(false);
  /** True from the user's request until the updater has answered it. */
  const asked = useRef(false);
  /**
   * True once the asked-for check found a build. A failure after that is the
   * download failing, and "couldn't check" would be false: the check worked.
   */
  const found = useRef(false);

  useEffect(
    () =>
      native.updates.subscribe((next) => {
        setStatus(next);
        if (!asked.current) return;
        if (next.state === "available" || next.state === "downloading") {
          found.current = true;
        } else if (next.state === "idle") {
          asked.current = false;
          showNotice("Borg is up to date");
        } else if (next.state === "error") {
          asked.current = false;
          const what = found.current
            ? "Couldn't download the update."
            : "Couldn't check for updates.";
          showNotice(`${what} ${checkFailureReason(next.message)}`);
        } else if (next.state === "ready") {
          // The pill below is the answer.
          asked.current = false;
        }
      }),
    [native],
  );
  useKeybind("check-for-updates", () => {
    setDismissed(false);
    asked.current = true;
    found.current = false;
    native.updates.check();
  });

  if (status.state !== "ready" || dismissed) return null;

  return (
    <div className="pointer-events-auto fixed right-5 bottom-5 z-40 flex items-center gap-2.5 rounded-[11px] border border-line bg-surface2 py-2 pr-2 pl-3.5 shadow-(--frameShadow)">
      <span className="text-[12.5px] leading-none text-ink2">
        Borg {status.version ?? ""} is ready
      </span>
      <Pill square variant="primary" onClick={() => native.updates.install()}>
        Restart
      </Pill>
      <button
        type="button"
        aria-label="Dismiss"
        onClick={() => setDismissed(true)}
        className="cursor-pointer border-0 bg-transparent px-1 text-[13px] leading-none text-ink3 hover:text-ink"
      >
        ×
      </button>
    </div>
  );
}
