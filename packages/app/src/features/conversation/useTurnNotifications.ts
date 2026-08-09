import { useEffect, useRef } from "react";
import { useNative } from "../../lib/platform";
import type { Cell } from "./useConversation";

/** Prompts the agent is waiting on, which are what a badge should count. */
export function pendingPrompts(cells: Cell[]): number {
  return cells.filter(
    (c) => (c.kind === "shell-confirm" || c.kind === "input-request") && !c.answered,
  ).length;
}

/**
 * Tells the OS when something needs the user.
 *
 * Only fires when the window is not focused: a notification for a turn you are
 * sitting and watching is pure noise. Two things qualify — a turn finishing and
 * the agent asking a question — and the second also lights the dock badge,
 * because that one blocks until it is answered.
 *
 * A no-op in the browser: `useNative()` returns stubs there.
 */
export function useTurnNotifications(cells: Cell[], busy: boolean, sessionId: string | undefined) {
  const native = useNative();
  const wasBusy = useRef(busy);
  const lastPromptCount = useRef(0);

  useEffect(() => {
    const finished = wasBusy.current && !busy;
    wasBusy.current = busy;
    if (!finished || document.hasFocus()) return;
    native.notify({
      title: "Borg",
      body: "Your turn is finished.",
      sessionId,
      // One pending "done" per conversation; a burst of turns should not
      // stack into a wall of identical banners.
      key: `turn-complete:${sessionId ?? "current"}`,
    });
  }, [busy, native, sessionId]);

  useEffect(() => {
    const pending = pendingPrompts(cells);
    const appeared = pending > lastPromptCount.current;
    lastPromptCount.current = pending;

    native.setBadgeCount(pending);
    if (!appeared || document.hasFocus()) return;
    native.notify({
      title: "Borg needs you",
      body: "Borg is waiting on an answer.",
      sessionId,
      key: `prompt:${sessionId ?? "current"}`,
    });
  }, [cells, native, sessionId]);

  // A badge left behind after the conversation closes would never clear.
  useEffect(() => () => native.setBadgeCount(0), [native]);
}
