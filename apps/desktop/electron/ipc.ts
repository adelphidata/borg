import { ipcMain, type WebContents } from "electron";
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
/**
 * The page load a stream id belongs to. The preload names each stream
 * `<load>:s<n>`, with a fresh `<load>` on every page load, so ids from two
 * loads of one window can never meet.
 */
export function loadOf(id: string): string {
  const colon = id.lastIndexOf(":");
  return colon < 0 ? "" : id.slice(0, colon);
}

export function registerBorgIpc(): { dispose(): void } {
  const transport = createNodeTransport();

  /**
   * In-flight server streams per window, and the page load that opened them,
   * so the renderer can cancel and unload cleanly.
   *
   * A page that dies never cancels its own streams. The first stream a new
   * load opens (its id carries the load, see `loadOf`) closes whatever the
   * previous load left behind; a crash or a closed window closes them at once.
   * Navigation events are not used: a navigation the window then blocks
   * (a file dropped on the page) would close the streams of a page that lives
   * on, and nothing would tell it.
   */
  const pages = new Map<WebContents, { load: string; streams: Map<string, Unsubscribe> }>();

  /** Closes every stream a window's current page opened. */
  const closePage = (sender: WebContents) => {
    const page = pages.get(sender);
    if (!page) return;
    pages.delete(sender);
    for (const cancel of page.streams.values()) cancel();
  };

  const streamsOf = (sender: WebContents, load: string) => {
    const known = pages.get(sender);
    if (known?.load === load) return known.streams;
    if (known) {
      // A new page load: the previous page is gone. IPC from one window
      // arrives in order, so nothing the old page sent can come after this.
      closePage(sender);
    } else {
      const close = () => {
        sender.off("render-process-gone", close);
        sender.off("destroyed", close);
        closePage(sender);
      };
      sender.on("render-process-gone", close);
      sender.on("destroyed", close);
    }
    const streams = new Map<string, Unsubscribe>();
    pages.set(sender, { load, streams });
    return streams;
  };

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

    const streams = streamsOf(sender, loadOf(id));
    const finish = (payload: StreamEvent) => {
      if (streams.get(id) === cancel) streams.delete(id);
      send(payload);
    };

    const cancel = transport.serverStream(method, request, {
      onMessage: (message) => send({ kind: "message", message }),
      onError: (error) => finish({ kind: "error", error: toWire(error) }),
      onComplete: () => finish({ kind: "complete" }),
    });
    streams.set(id, cancel);
  });

  ipcMain.handle(CHANNELS.streamCancel, (event, id: string) => {
    const streams = pages.get(event.sender)?.streams;
    streams?.get(id)?.();
    streams?.delete(id);
  });

  return {
    dispose() {
      for (const sender of [...pages.keys()]) closePage(sender);
      transport.close();
      for (const channel of Object.values(CHANNELS)) ipcMain.removeHandler(channel);
    },
  };
}
