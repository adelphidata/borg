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
