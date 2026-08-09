import { contextBridge, ipcRenderer } from "electron";
import type { BorgTransport, MethodPath, StreamHandlers } from "@borg/client";
import { BorgError } from "@borg/client";
import type { NativeBridge } from "@borg/app";
import {
  CHANNELS,
  type StreamEvent,
  type UnaryReply,
  type WireNotification,
  type WireUpdateStatus,
} from "./channels";

/**
 * The renderer's `BorgTransport`, implemented over IPC.
 *
 * Satisfies exactly the same interface as the browser's gRPC-Web transport, so
 * `@borg/app` runs unchanged on both surfaces.
 */
let nextStreamId = 0;

const transport: BorgTransport = {
  async unary(method, request, signal) {
    signal?.throwIfAborted();
    const reply: UnaryReply = await ipcRenderer.invoke(CHANNELS.unary, method, request);
    if (!reply.ok) throw new BorgError(reply.error.code, reply.error.message, method);
    return reply.value as never;
  },

  serverStream(method: MethodPath, request, handlers: StreamHandlers<never>) {
    const id = `s${nextStreamId++}`;

    const onEvent = (_e: unknown, streamId: string, payload: StreamEvent) => {
      if (streamId !== id) return;
      switch (payload.kind) {
        case "message":
          handlers.onMessage(payload.message as never);
          break;
        case "error":
          detach();
          handlers.onError?.(new BorgError(payload.error.code, payload.error.message, method));
          break;
        case "complete":
          detach();
          handlers.onComplete?.();
          break;
      }
    };

    const detach = () => ipcRenderer.off(CHANNELS.streamEvent, onEvent);

    ipcRenderer.on(CHANNELS.streamEvent, onEvent);
    void ipcRenderer.invoke(CHANNELS.streamOpen, id, method, request);

    return () => {
      detach();
      void ipcRenderer.invoke(CHANNELS.streamCancel, id);
    };
  },
};

/** Subscribes to a main→renderer push channel and returns its unsubscribe. */
function subscribe<T>(channel: string, handler: (payload: T) => void): () => void {
  const listener = (_event: unknown, payload: T) => handler(payload);
  ipcRenderer.on(channel, listener);
  return () => ipcRenderer.off(channel, listener);
}

/**
 * The native shell, exposed alongside the transport rather than inside it.
 *
 * This is the only sanctioned widening of `window.borg`: it carries no Node
 * APIs and no filesystem reach, just the handful of OS affordances a browser
 * tab cannot offer. `@borg/app` reaches it through `useNative()` and degrades
 * to no-ops when it is absent.
 */
const native: NativeBridge = {
  onMenuCommand: (handler) => subscribe<string>(CHANNELS.menuCommand, handler),
  onActivateSession: (handler) => subscribe<string>(CHANNELS.activateSession, handler),
  notify: (notification) => {
    void ipcRenderer.invoke(CHANNELS.notify, notification satisfies WireNotification);
  },
  setBadgeCount: (count) => {
    void ipcRenderer.invoke(CHANNELS.setBadge, count);
  },
  updates: {
    subscribe: (handler) => subscribe<WireUpdateStatus>(CHANNELS.updateStatus, handler),
    check: () => void ipcRenderer.invoke(CHANNELS.updateCheck),
    install: () => void ipcRenderer.invoke(CHANNELS.updateInstall),
  },
};

contextBridge.exposeInMainWorld("borg", {
  platform: process.platform,
  transport,
  native,
});
