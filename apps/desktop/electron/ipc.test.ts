import { beforeEach, describe, expect, it, vi } from "vitest";
import { CHANNELS, type StreamEvent, type UnaryReply } from "./channels";

vi.mock("electron", () => import("./test/electron-stub"));

/** The node transport, replaced so no unix socket is involved. */
const transport = vi.hoisted(() => ({
  unary: vi.fn(),
  serverStream: vi.fn(),
  close: vi.fn(),
}));
vi.mock("@borg/client/node", () => ({ createNodeTransport: () => transport }));

const { handlers, invoke, makeWindow, resetElectron, windows } =
  await import("./test/electron-stub");
const { BorgError } = await import("@borg/client");
const { registerBorgIpc, loadOf } = await import("./ipc");

let win: ReturnType<typeof makeWindow>;

beforeEach(() => {
  resetElectron();
  transport.unary.mockReset();
  transport.serverStream.mockReset();
  transport.close.mockReset();
  win = makeWindow();
  windows.push(win);
});

const event = () => ({ sender: win.webContents });
const streamed = () =>
  win.sent
    .filter((s) => s.channel === CHANNELS.streamEvent)
    .map((s) => ({ id: s.args[0] as string, payload: s.args[1] as StreamEvent }));

describe("registerBorgIpc unary", () => {
  it("returns the daemon's reply", async () => {
    transport.unary.mockResolvedValue({ sessions: [] });
    const ipc = registerBorgIpc();

    const reply = (await invoke(CHANNELS.unary, event(), "Sessions/List", {
      limit: 5,
    })) as UnaryReply;
    expect(transport.unary).toHaveBeenCalledWith("Sessions/List", { limit: 5 });
    expect(reply).toEqual({ ok: true, value: { sessions: [] } });
    ipc.dispose();
  });

  it("flattens a gRPC failure to data, keeping its status code", async () => {
    // An Error instance loses its gRPC status through structured clone, so the
    // renderer would see a generic failure and could not branch on the code.
    transport.unary.mockRejectedValue(new BorgError(7, "permission denied", "Sessions/List"));
    const ipc = registerBorgIpc();

    const reply = (await invoke(CHANNELS.unary, event(), "Sessions/List", {})) as UnaryReply;
    expect(reply).toEqual({ ok: false, error: { code: 7, message: "permission denied" } });
    ipc.dispose();
  });

  it("reports a non-gRPC failure as internal rather than losing it", async () => {
    transport.unary.mockRejectedValue(new Error("socket closed"));
    const ipc = registerBorgIpc();

    const reply = (await invoke(CHANNELS.unary, event(), "Sessions/List", {})) as UnaryReply;
    expect(reply).toEqual({ ok: false, error: { code: 13, message: "socket closed" } });
    ipc.dispose();
  });

  it("survives a thrown non-Error", async () => {
    transport.unary.mockRejectedValue("kaboom");
    const ipc = registerBorgIpc();

    const reply = (await invoke(CHANNELS.unary, event(), "Sessions/List", {})) as UnaryReply;
    expect(reply).toEqual({ ok: false, error: { code: 13, message: "kaboom" } });
    ipc.dispose();
  });
});

describe("registerBorgIpc streams", () => {
  /** Captures the handlers the transport was given for the open stream. */
  function openStream(id = "s0") {
    let captured!: {
      onMessage: (m: unknown) => void;
      onError: (e: unknown) => void;
      onComplete: () => void;
    };
    const cancel = vi.fn();
    transport.serverStream.mockImplementation((_m, _r, handlers) => {
      captured = handlers;
      return cancel;
    });
    invoke(CHANNELS.streamOpen, event(), id, "Sessions/Watch", {});
    return { handlers: captured, cancel };
  }

  it("forwards each message to the window that opened the stream", () => {
    const ipc = registerBorgIpc();
    const { handlers: stream } = openStream();

    stream.onMessage({ turn: 1 });
    stream.onMessage({ turn: 2 });
    expect(streamed()).toEqual([
      { id: "s0", payload: { kind: "message", message: { turn: 1 } } },
      { id: "s0", payload: { kind: "message", message: { turn: 2 } } },
    ]);
    ipc.dispose();
  });

  it("carries the stream id, so two streams on one window stay apart", () => {
    const ipc = registerBorgIpc();
    const first = openStream("s0");
    const second = openStream("s1");

    first.handlers.onMessage({ from: "first" });
    second.handlers.onMessage({ from: "second" });
    expect(streamed().map((s) => s.id)).toEqual(["s0", "s1"]);
    ipc.dispose();
  });

  it("sends the error as data and ends the stream", () => {
    const ipc = registerBorgIpc();
    const { handlers: stream } = openStream();

    stream.onError(new BorgError(14, "unavailable", "Sessions/Watch"));
    expect(streamed().at(-1)?.payload).toEqual({
      kind: "error",
      error: { code: 14, message: "unavailable" },
    });
    ipc.dispose();
  });

  it("sends completion", () => {
    const ipc = registerBorgIpc();
    const { handlers: stream } = openStream();

    stream.onComplete();
    expect(streamed().at(-1)?.payload).toEqual({ kind: "complete" });
    ipc.dispose();
  });

  it("says nothing to a window that has already gone", () => {
    // A reload destroys the sender while the daemon is mid-stream.
    const ipc = registerBorgIpc();
    const { handlers: stream } = openStream();
    win.destroy();

    expect(() => stream.onMessage({ turn: 1 })).not.toThrow();
    expect(streamed()).toEqual([]);
    ipc.dispose();
  });

  it("cancels the stream the renderer asks it to", () => {
    const ipc = registerBorgIpc();
    const { cancel } = openStream();

    invoke(CHANNELS.streamCancel, event(), "s0");
    expect(cancel).toHaveBeenCalledTimes(1);
    ipc.dispose();
  });

  it("ignores a cancel for a stream that already ended", () => {
    const ipc = registerBorgIpc();
    const { handlers: stream, cancel } = openStream();

    stream.onComplete();
    invoke(CHANNELS.streamCancel, event(), "s0");
    expect(cancel).not.toHaveBeenCalled();
    ipc.dispose();
  });

  it("does not cancel again on dispose a stream the renderer already cancelled", () => {
    // streamCancel must also drop the map entry; a leftover entry would be
    // cancelled a second time on quit.
    const ipc = registerBorgIpc();
    const { cancel } = openStream();

    invoke(CHANNELS.streamCancel, event(), "s0");
    ipc.dispose();
    expect(cancel).toHaveBeenCalledTimes(1);
  });

  it("dispose is idempotent: a second dispose cancels nothing again", () => {
    // Window reload and app quit can both tear the bridge down; the map must
    // be emptied by the first pass so the second finds nothing to cancel.
    const ipc = registerBorgIpc();
    const { cancel } = openStream();

    ipc.dispose();
    ipc.dispose();
    expect(cancel).toHaveBeenCalledTimes(1);
  });

  it("does not cancel a finished stream on dispose", () => {
    // The map is the leak guard: an entry left behind after completion would be
    // cancelled again on quit.
    const ipc = registerBorgIpc();
    const { handlers: stream, cancel } = openStream();
    stream.onError(new Error("gone"));

    ipc.dispose();
    expect(cancel).not.toHaveBeenCalled();
  });

  it("cancels what is still live, closes the transport, and drops every handler", () => {
    // Without this, a window reload leaks subscriptions into the daemon.
    const ipc = registerBorgIpc();
    const { cancel } = openStream();

    ipc.dispose();
    expect(cancel).toHaveBeenCalledTimes(1);
    expect(transport.close).toHaveBeenCalledTimes(1);
    for (const channel of Object.values(CHANNELS)) expect(handlers.has(channel)).toBe(false);
  });
});

describe("registerBorgIpc when the page goes away", () => {
  /** Opens a stream and keeps its handlers, so late daemon events can be sent. */
  function open(id: string) {
    const cancel = vi.fn();
    let captured!: { onMessage: (m: unknown) => void; onComplete: () => void };
    transport.serverStream.mockImplementationOnce((_m, _r, h) => {
      captured = h;
      return cancel;
    });
    invoke(CHANNELS.streamOpen, event(), id, "Sessions/Watch", {});
    return { cancel, handlers: captured };
  }

  it.each([
    ["the renderer crashes", () => win.webContents.emit("render-process-gone", {}, {})],
    ["the window's page is destroyed", () => win.webContents.emit("destroyed")],
  ])("closes every stream the page opened when %s", (_name, leave) => {
    // A page that dies never cancels its own streams: without this the
    // daemon subscriptions stay open until the app quits.
    const ipc = registerBorgIpc();
    const a = open("load-1:s0");
    const b = open("load-1:s1");

    leave();
    expect(a.cancel).toHaveBeenCalledTimes(1);
    expect(b.cancel).toHaveBeenCalledTimes(1);
    ipc.dispose();
    expect(a.cancel).toHaveBeenCalledTimes(1);
  });

  it("closes the previous page's streams when a reloaded page opens its first", () => {
    const ipc = registerBorgIpc();
    const a = open("load-1:s0");
    const b = open("load-1:s1");

    const fresh = open("load-2:s0");
    expect(a.cancel).toHaveBeenCalledTimes(1);
    expect(b.cancel).toHaveBeenCalledTimes(1);
    expect(fresh.cancel).not.toHaveBeenCalled();
    ipc.dispose();
    expect(fresh.cancel).toHaveBeenCalledTimes(1);
  });

  it("keeps the streams of a page whose navigation was blocked", () => {
    // A file dropped on the page starts a navigation the window then refuses;
    // the page lives on, so its streams must too.
    const ipc = registerBorgIpc();
    const a = open("load-1:s0");

    win.webContents.emit("did-start-navigation", { isMainFrame: true, isSameDocument: false });
    open("load-1:s1");
    expect(a.cancel).not.toHaveBeenCalled();
    ipc.dispose();
  });

  it("never hands the new page an event or a cancel meant for the old one", () => {
    const ipc = registerBorgIpc();
    const old = open("load-1:s0");
    const fresh = open("load-2:s0");

    // A late event from the old subscription goes out under the old id, which
    // the new page does not listen for.
    old.handlers.onMessage({ stale: true });
    expect(streamed().at(-1)?.id).toBe("load-1:s0");

    // The old stream's late completion must not remove the new entry…
    old.handlers.onComplete();
    invoke(CHANNELS.streamCancel, event(), "load-2:s0");
    expect(fresh.cancel).toHaveBeenCalledTimes(1);
    expect(old.cancel).toHaveBeenCalledTimes(1);
    ipc.dispose();
  });

  it("does not pile up page listeners across reloads and crashes", () => {
    const ipc = registerBorgIpc();
    const count = () =>
      [...win.webContents.listeners.values()].reduce((n, list) => n + list.length, 0);
    open("load-0:s0");
    const perWindow = count();
    for (let i = 1; i <= 5; i++) {
      open(`load-${i}:s0`);
      if (i % 2 === 0) win.webContents.emit("render-process-gone", {}, {});
    }
    open("load-9:s0");
    expect(count()).toBe(perWindow);
    ipc.dispose();
  });
});

describe("loadOf", () => {
  it.each([
    ["3f2a:s0", "3f2a"],
    ["a:b:s12", "a:b"],
    ["s0", ""],
  ])("%s belongs to load %j", (id, load) => {
    expect(loadOf(id)).toBe(load);
  });
});
