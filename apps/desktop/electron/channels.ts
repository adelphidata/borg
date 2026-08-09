/**
 * The IPC contract between `electron/main` and `electron/preload`.
 *
 * Shared by both sides so a renamed channel is a type error rather than a
 * silent no-op at runtime.
 */

export const CHANNELS = {
  unary: "borg:unary",
  streamOpen: "borg:stream:open",
  streamCancel: "borg:stream:cancel",
  streamEvent: "borg:stream:event",

  // Native shell. Everything borgd has no part in: menus, notifications,
  // the dock badge, and the updater.
  menuCommand: "borg:native:menu", // main → renderer, carries a keybind action id
  activateSession: "borg:native:activate", // main → renderer, focus this session
  notify: "borg:native:notify", // renderer → main
  revealSession: "borg:native:reveal", // renderer → main, raise the main window
  hideWindow: "borg:native:hide", // renderer → main, dismiss this window
  setBadge: "borg:native:badge", // renderer → main
  updateStatus: "borg:native:update:status", // main → renderer
  updateCheck: "borg:native:update:check", // renderer → main
  updateInstall: "borg:native:update:install", // renderer → main
} as const;

/** A gRPC failure flattened to structured-cloneable data. */
export interface WireError {
  code: number;
  message: string;
}

export type UnaryReply<T = unknown> = { ok: true; value: T } | { ok: false; error: WireError };

export type StreamEvent =
  | { kind: "message"; message: unknown }
  | { kind: "error"; error: WireError }
  | { kind: "complete" };

/** Mirrors `NativeNotification` in `@borg/app` — kept structured-cloneable. */
export interface WireNotification {
  title: string;
  body: string;
  sessionId?: string;
  key?: string;
}

/** Mirrors `UpdateStatus` in `@borg/app`. */
export interface WireUpdateStatus {
  state: "idle" | "checking" | "available" | "downloading" | "ready" | "error";
  version?: string;
  percent?: number;
  message?: string;
}
