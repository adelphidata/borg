import { useEffect, useRef, useState } from "react";
import type { SidebarUpdate } from "@borg/ui";
import { useNative } from "./platform";
import { useKeybind } from "./keybinds";
import { showNotice } from "./notice";
import type { UpdateStatus } from "./native";

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
 * The part of the updater's status the sidebar shows, or nothing.
 *
 * Idle, checking and a failed automatic check all map to nothing: the sidebar
 * speaks only when there is a build on its way, and says so until the user
 * restarts into it.
 */
export function sidebarUpdate(status: UpdateStatus): SidebarUpdate | undefined {
  switch (status.state) {
    case "available":
    case "downloading":
    case "ready":
      return { state: status.state, version: status.version, percent: status.percent };
    default:
      return undefined;
  }
}

/**
 * Follows the native shell's auto-updater.
 *
 * Automatic checks stay silent. A check the user asked for, through the menu
 * item or its chord, answers — up to date, or why it could not tell — through
 * the shell's notice, because a menu item that does nothing visible reads as
 * broken. A build that was found needs no notice: the sidebar shows it.
 */
export function useUpdater(): { status: UpdateStatus; install: () => void } {
  const native = useNative();
  const [status, setStatus] = useState<UpdateStatus>({ state: "idle" });
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
          // The sidebar is the answer.
          asked.current = false;
        }
      }),
    [native],
  );
  useKeybind("check-for-updates", () => {
    asked.current = true;
    found.current = false;
    native.updates.check();
  });

  return { status, install: native.updates.install };
}
