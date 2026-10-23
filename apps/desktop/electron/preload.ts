import { contextBridge, ipcRenderer } from "electron";
import type { NativeBridge } from "@borg/app";
import {
  CHANNELS,
  type StreamEvent,
  type UnaryReply,
  type WireBootStatus,
  type WireNotification,
  type WireTransport,
  type WireUpdateStatus,
} from "./channels";

/**
 * The transport over IPC, with every failure kept as data.
 *
 * Nothing here throws or builds an `Error`: the context bridge would strip it
 * to a bare message. The renderer wraps this with `fromWire`, which is what
 * gives `@borg/app` the same `BorgTransport` the browser's gRPC-Web one is.
 */
let nextStreamId = 0;

/**
 * Names this page load in every stream id, so the main process can tell a
 * reloaded page's `s0` from the one the dead page opened, and close the dead
 * page's streams when the new page opens its first.
 */
const pageLoad = crypto.randomUUID();

const transport: WireTransport = {
  unary(method, request) {
    return ipcRenderer.invoke(CHANNELS.unary, method, request) as Promise<UnaryReply>;
  },

  serverStream(method, request, deliver) {
    const id = `${pageLoad}:s${nextStreamId++}`;

    const onEvent = (_e: unknown, streamId: string, payload: StreamEvent) => {
      if (streamId !== id) return;
      // An error or completion ends the stream; nothing after it is delivered.
      if (payload.kind !== "message") detach();
      deliver(payload);
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
  revealSession: (sessionId) => {
    void ipcRenderer.invoke(CHANNELS.revealSession, sessionId);
  },
  hideWindow: () => {
    void ipcRenderer.invoke(CHANNELS.hideWindow);
  },
  setBadgeCount: (count) => {
    void ipcRenderer.invoke(CHANNELS.setBadge, count);
  },
  setDictationHotkey: (accelerator) => {
    void ipcRenderer.invoke(CHANNELS.dictationHotkey, accelerator);
  },
  boot: {
    subscribe: (handler) => {
      let live = true;
      const detach = subscribe<WireBootStatus>(CHANNELS.bootStatus, handler);
      // Replay whatever the shell has already decided: a window created after
      // the daemon came up would otherwise never hear a thing. The reply can
      // land after the caller has gone (a reload mid-flight) or never come at
      // all (handlers torn down on quit), so neither outcome may escape here.
      void ipcRenderer
        .invoke(CHANNELS.bootRead)
        .then((status: WireBootStatus) => {
          if (live) handler(status);
        })
        .catch(() => {});
      return () => {
        live = false;
        detach();
      };
    },
    retry: () =>
      ipcRenderer.invoke(CHANNELS.bootRetry).then(
        () => {},
        () => {},
      ),
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
