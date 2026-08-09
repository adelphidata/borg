import { ipcMain } from "electron";
import { createNodeTransport } from "@borg/client/node";
import { BorgError, type Unsubscribe } from "@borg/client";
import { CHANNELS, type StreamEvent, type WireError } from "./channels";

/**
 * Daemon access for the desktop app.
 *
 * The renderer is context-isolated with no Node APIs, so the gRPC channel to
 * borgd's unix socket lives here. The renderer sends a method path and a plain
 * request object; this forwards it and sends the reply back. That keeps the
 * socket — and the filesystem permission that guards it — out of web content.
 */
export function registerBorgIpc(): { dispose(): void } {
  const transport = createNodeTransport();

  /** In-flight server streams, so the renderer can cancel and unload cleanly. */
  const streams = new Map<string, Unsubscribe>();

  const toWire = (error: unknown): WireError =>
    error instanceof BorgError
      ? { code: error.code, message: error.message }
      : { code: 13, message: error instanceof Error ? error.message : String(error) };

  ipcMain.handle(CHANNELS.unary, async (_event, method: string, request: unknown) => {
    try {
      return { ok: true as const, value: await transport.unary(method, request) };
    } catch (error) {
      // Errors cross IPC as data: an Error instance would lose its gRPC status.
      return { ok: false as const, error: toWire(error) };
    }
  });

  ipcMain.handle(CHANNELS.streamOpen, (event, id: string, method: string, request: unknown) => {
    const sender = event.sender;
    const send = (payload: StreamEvent) => {
      if (!sender.isDestroyed()) sender.send(CHANNELS.streamEvent, id, payload);
    };

    const finish = (payload: StreamEvent) => {
      streams.delete(id);
      send(payload);
    };

    streams.set(
      id,
      transport.serverStream(method, request, {
        onMessage: (message) => send({ kind: "message", message }),
        onError: (error) => finish({ kind: "error", error: toWire(error) }),
        onComplete: () => finish({ kind: "complete" }),
      }),
    );
  });

  ipcMain.handle(CHANNELS.streamCancel, (_event, id: string) => {
    streams.get(id)?.();
    streams.delete(id);
  });

  return {
    dispose() {
      for (const cancel of streams.values()) cancel();
      streams.clear();
      transport.close();
      for (const channel of Object.values(CHANNELS)) ipcMain.removeHandler(channel);
    },
  };
}
