/**
 * Capabilities only a native shell can provide.
 *
 * `packages/app` must run unchanged on both surfaces, so feature code never
 * reads `window.borg` and never branches on `platform.kind`. It calls
 * `useNative()` and gets either the real Electron bridge or a no-op stub — the
 * browser simply does less, and no page has to know which one it got.
 *
 * This sits beside the transport rather than inside it: the transport is how a
 * surface reaches borgd, and these are things borgd has no part in.
 */

/** A native OS notification. */
export interface NativeNotification {
  title: string;
  body: string;
  /** Focus this session when the notification is clicked. */
  sessionId?: string;
  /** Repeats with the same key replace rather than stack. */
  key?: string;
}

/** Where the auto-updater currently stands. */
export interface UpdateStatus {
  state: "idle" | "checking" | "available" | "downloading" | "ready" | "error";
  version?: string;
  /** Download progress, 0–100, while `state` is "downloading". */
  percent?: number;
  message?: string;
}

export interface NativeBridge {
  /**
   * Menu items and global hotkeys arrive here as keybind action ids, so a
   * shortcut has one implementation no matter how it was triggered.
   */
  onMenuCommand(handler: (action: string) => void): () => void;
  notify(notification: NativeNotification): void;
  /** The user clicked a notification, or the quick-entry window sent a turn. */
  onActivateSession(handler: (sessionId: string) => void): () => void;
  /** Brings the main window forward and opens `sessionId` there. */
  revealSession(sessionId: string): void;
  /** Dismisses the window this renderer is in. Used by the quick-entry panel. */
  hideWindow(): void;
  /** Dock/taskbar badge. 0 clears it. */
  setBadgeCount(count: number): void;
  updates: {
    subscribe(handler: (status: UpdateStatus) => void): () => void;
    check(): void;
    /** Quits and relaunches into the downloaded version. */
    install(): void;
  };
}

const noop = () => {};
const unsubscribe = () => noop;

/** What every `useNative()` caller gets on a surface with no native shell. */
export const NO_NATIVE: NativeBridge = {
  onMenuCommand: unsubscribe,
  notify: noop,
  onActivateSession: unsubscribe,
  revealSession: noop,
  hideWindow: noop,
  setBadgeCount: noop,
  updates: {
    subscribe: unsubscribe,
    check: noop,
    install: noop,
  },
};
