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
const { registerBorgIpc } = await import("./ipc");

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
