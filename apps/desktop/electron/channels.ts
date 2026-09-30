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

  // Bringing borgd up. The daemon cannot report its own absence, so the shell
  // does: one status channel, and a retry the failure screen can call.
  bootStatus: "borg:native:boot:status", // main → renderer
  bootRead: "borg:native:boot:read", // renderer → main, the status so far
  bootRetry: "borg:native:boot:retry", // renderer → main
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

/**
 * The transport as the preload exposes it: every failure arrives as data.
 *
 * The context bridge copies a thrown or passed `Error` as a bare `Error` with
 * only its message — a `BorgError` built in the preload reaches the renderer
 * with no status code, so "Borg is not running" and "token rejected" both
 * read as internal errors. The renderer turns this into a `BorgTransport`
 * with `fromWire`, where the `BorgError` class is its own.
 */
export interface WireTransport {
  unary(method: string, request: unknown): Promise<UnaryReply>;
  /** Delivers events until an error or completion; the return value cancels. */
  serverStream(method: string, request: unknown, onEvent: (event: StreamEvent) => void): () => void;
}

/** Mirrors `NativeNotification` in `@borg/app` — kept structured-cloneable. */
export interface WireNotification {
  title: string;
  body: string;
  sessionId?: string;
  key?: string;
}

/** Mirrors `BootStatus` in `@borg/app`. */
export interface WireBootStatus {
  state: "starting" | "ready" | "failed";
  /** Plain language, no paths and no codes — this is shown to the user. */
  message: string;
  /** 0–1. Monotonic within one boot attempt. */
  progress: number;
  /** True when trying again could plausibly work. */
  canRetry: boolean;
}

/** Mirrors `UpdateStatus` in `@borg/app`. */
export interface WireUpdateStatus {
  state: "idle" | "checking" | "available" | "downloading" | "ready" | "error";
  version?: string;
  percent?: number;
  message?: string;
}
