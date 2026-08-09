import { contextBridge, ipcRenderer } from "electron";
import type { BorgTransport, MethodPath, StreamHandlers } from "@borg/client";
import { BorgError } from "@borg/client";
import { CHANNELS, type StreamEvent, type UnaryReply } from "./channels";

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

contextBridge.exposeInMainWorld("borg", {
  platform: process.platform,
  transport,
});
