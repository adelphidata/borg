import { beforeEach, describe, expect, it, vi } from "vitest";
import type { BorgTransport } from "@borg/client";
import type { NativeBridge } from "@borg/app";
import { CHANNELS, type StreamEvent, type WireBootStatus } from "./channels";

vi.mock("electron", () => import("./test/electron-stub"));

const { contextBridgeState, pushToRenderer, rendererState, resetElectron } =
  await import("./test/electron-stub");
const { BorgError } = await import("@borg/client");

interface ExposedBorg {
  platform: string;
  transport: BorgTransport;
  native: NativeBridge;
}

// The preload exposes its bridge once, as a side effect of being imported —
// exactly as it does in a real renderer. Captured here, then reused: the bridge
// itself holds no per-window state beyond its stream counter.
await import("./preload");
const bridge = contextBridgeState.exposed.get("borg") as ExposedBorg;
const loadPreload = async (): Promise<ExposedBorg> => bridge;

beforeEach(() => {
  resetElectron();
});

const invocations = (channel: string) =>
  rendererState.invocations.filter((i) => i.channel === channel);

describe("preload bridge", () => {
  it("exposes exactly the platform, transport and native bridge", async () => {
    // `window.borg` is the whole renderer surface; anything else here would be
    // reach the browser build cannot have.
    const borg = await loadPreload();
    expect(Object.keys(borg).sort()).toEqual(["native", "platform", "transport"]);
    expect(borg.platform).toBe(process.platform);
  });
});

describe("transport.unary", () => {
  it("returns the value from a successful reply", async () => {
    rendererState.replies.set(CHANNELS.unary, () => ({ ok: true, value: { id: "s1" } }));
    const borg = await loadPreload();

    await expect(borg.transport.unary("Sessions/Open" as never, {})).resolves.toEqual({ id: "s1" });
  });

  it("rehydrates a failure into a BorgError carrying the status code", async () => {
    // The renderer branches on the code; a plain Error would erase it.
    rendererState.replies.set(CHANNELS.unary, () => ({
      ok: false,
      error: { code: 7, message: "permission denied" },
    }));
    const borg = await loadPreload();

    const failure = borg.transport.unary("Sessions/Open" as never, {});
    await expect(failure).rejects.toBeInstanceOf(BorgError);
    await expect(failure).rejects.toMatchObject({ code: 7, message: "permission denied" });
  });

  it("refuses to send a call whose signal is already aborted", async () => {
    const borg = await loadPreload();
    const aborted = AbortSignal.abort();

    await expect(borg.transport.unary("Sessions/Open" as never, {}, aborted)).rejects.toThrow();
    expect(invocations(CHANNELS.unary)).toHaveLength(0);
  });
});

describe("transport.serverStream", () => {
  const open = async (borg: ExposedBorg, handlers: Record<string, unknown> = {}) => {
    const onMessage = vi.fn();
    const cancel = borg.transport.serverStream("Sessions/Watch" as never, {}, {
      onMessage,
      ...handlers,
    } as never);
    const id = invocations(CHANNELS.streamOpen).at(-1)?.args[0] as string;
    return { cancel, onMessage, id };
  };

  const deliver = (id: string, payload: StreamEvent) =>
    pushToRenderer(CHANNELS.streamEvent, id, payload);

  it("opens the stream and delivers its messages", async () => {
    const borg = await loadPreload();
    const { onMessage, id } = await open(borg);

    deliver(id, { kind: "message", message: { turn: 1 } });
    expect(onMessage).toHaveBeenCalledWith({ turn: 1 });
  });

  it("ignores events belonging to another stream", async () => {
    // Every stream on this window shares one channel.
    const borg = await loadPreload();
    const { onMessage, id } = await open(borg);

    deliver(`${id}-other`, { kind: "message", message: { turn: 99 } });
    expect(onMessage).not.toHaveBeenCalled();
  });

  it("gives concurrent streams distinct ids", async () => {
    const borg = await loadPreload();
    const first = await open(borg);
    const second = await open(borg);
    expect(first.id).not.toBe(second.id);
  });

  it("rehydrates a stream error and stops listening", async () => {
    const borg = await loadPreload();
    const onError = vi.fn();
    const { onMessage, id } = await open(borg, { onError });

    deliver(id, { kind: "error", error: { code: 14, message: "unavailable" } });
    expect(onError).toHaveBeenCalledWith(expect.any(BorgError));
    expect(onError.mock.calls[0][0]).toMatchObject({ code: 14, message: "unavailable" });

    // Detached: a late message must not reach a handler that already errored.
    deliver(id, { kind: "message", message: { turn: 2 } });
    expect(onMessage).not.toHaveBeenCalled();
  });

  it("completes and stops listening", async () => {
    const borg = await loadPreload();
    const onComplete = vi.fn();
    const { onMessage, id } = await open(borg, { onComplete });

    deliver(id, { kind: "complete" });
    expect(onComplete).toHaveBeenCalledTimes(1);

    deliver(id, { kind: "message", message: { turn: 2 } });
    expect(onMessage).not.toHaveBeenCalled();
  });

  it("survives a stream with no error or completion handler", async () => {
    const borg = await loadPreload();
    const { id } = await open(borg);
    expect(() => deliver(id, { kind: "error", error: { code: 1, message: "x" } })).not.toThrow();
    expect(() => deliver(id, { kind: "complete" })).not.toThrow();
  });

  it("cancelling detaches the listener and tells the daemon", async () => {
    const borg = await loadPreload();
    const { cancel, onMessage, id } = await open(borg);

    cancel();
    expect(invocations(CHANNELS.streamCancel).at(-1)?.args).toEqual([id]);
    deliver(id, { kind: "message", message: { turn: 1 } });
    expect(onMessage).not.toHaveBeenCalled();
  });
});

describe("native bridge", () => {
  it("forwards a notification unchanged", async () => {
    const borg = await loadPreload();
    borg.native.notify({ title: "Done", body: "Turn finished", sessionId: "s1", key: "s1" });
    expect(invocations(CHANNELS.notify).at(-1)?.args[0]).toEqual({
      title: "Done",
      body: "Turn finished",
      sessionId: "s1",
      key: "s1",
    });
  });

  it("forwards reveal, hide and badge", async () => {
    const borg = await loadPreload();
    borg.native.revealSession("s2");
    borg.native.hideWindow();
    borg.native.setBadgeCount(3);
    expect(invocations(CHANNELS.revealSession).at(-1)?.args).toEqual(["s2"]);
    expect(invocations(CHANNELS.hideWindow)).toHaveLength(1);
    expect(invocations(CHANNELS.setBadge).at(-1)?.args).toEqual([3]);
  });

  it("delivers menu commands and session activations, and unsubscribes", async () => {
    const borg = await loadPreload();
    const onMenu = vi.fn();
    const onActivate = vi.fn();
    const stopMenu = borg.native.onMenuCommand(onMenu);
    borg.native.onActivateSession(onActivate);

    pushToRenderer(CHANNELS.menuCommand, "new-conversation");
    pushToRenderer(CHANNELS.activateSession, "s5");
    expect(onMenu).toHaveBeenCalledWith("new-conversation");
    expect(onActivate).toHaveBeenCalledWith("s5");

    stopMenu();
    pushToRenderer(CHANNELS.menuCommand, "open-settings");
    expect(onMenu).toHaveBeenCalledTimes(1);
  });
});

describe("native boot bridge", () => {
  it("replays the status the shell already decided", async () => {
    // A window created after the daemon came up hears no broadcast at all.
    const ready: WireBootStatus = {
      state: "ready",
      message: "Ready",
      progress: 1,
      canRetry: false,
    };
    rendererState.replies.set(CHANNELS.bootRead, () => ready);
    const borg = await loadPreload();

    const onStatus = vi.fn();
    borg.native.boot.subscribe(onStatus);
    await Promise.resolve();
    await Promise.resolve();
    expect(onStatus).toHaveBeenCalledWith(ready);
  });

  it("delivers later broadcasts too", async () => {
    const borg = await loadPreload();
    const onStatus = vi.fn();
    borg.native.boot.subscribe(onStatus);

    const starting: WireBootStatus = {
      state: "starting",
      message: "Starting Borg…",
      progress: 0.45,
      canRetry: false,
    };
    pushToRenderer(CHANNELS.bootStatus, starting);
    expect(onStatus).toHaveBeenCalledWith(starting);
  });

  it("drops a replay that lands after the caller unsubscribed", async () => {
    // A reload mid-flight: the handler belongs to a component that is gone.
    let release!: (status: WireBootStatus) => void;
    rendererState.replies.set(
      CHANNELS.bootRead,
      () => new Promise<WireBootStatus>((resolve) => (release = resolve)),
    );
    const borg = await loadPreload();

    const onStatus = vi.fn();
    const stop = borg.native.boot.subscribe(onStatus);
    stop();
    release({ state: "ready", message: "Ready", progress: 1, canRetry: false });
    await Promise.resolve();
    await Promise.resolve();
    expect(onStatus).not.toHaveBeenCalled();
  });

  it("swallows a replay that never comes, rather than rejecting into the renderer", async () => {
    // Handlers are torn down on quit; an unhandled rejection would surface as a
    // console error in a window that is already closing.
    rendererState.replies.set(CHANNELS.bootRead, () => {
      throw new Error("no handler registered");
    });
    const borg = await loadPreload();

    const onStatus = vi.fn();
    expect(() => borg.native.boot.subscribe(onStatus)).not.toThrow();
    await Promise.resolve();
    await Promise.resolve();
    expect(onStatus).not.toHaveBeenCalled();
  });

  it("resolves a retry either way, so the failure screen never hangs", async () => {
    const borg = await loadPreload();
    await expect(borg.native.boot.retry()).resolves.toBeUndefined();

    rendererState.replies.set(CHANNELS.bootRetry, () => {
      throw new Error("gone");
    });
    await expect(borg.native.boot.retry()).resolves.toBeUndefined();
  });
});

describe("native updates bridge", () => {
  it("subscribes, checks and installs", async () => {
    const borg = await loadPreload();
    const onStatus = vi.fn();
    const stop = borg.native.updates.subscribe(onStatus);

    pushToRenderer(CHANNELS.updateStatus, { state: "checking" });
    expect(onStatus).toHaveBeenCalledWith({ state: "checking" });

    borg.native.updates.check();
    borg.native.updates.install();
    expect(invocations(CHANNELS.updateCheck)).toHaveLength(1);
    expect(invocations(CHANNELS.updateInstall)).toHaveLength(1);

    stop();
    pushToRenderer(CHANNELS.updateStatus, { state: "idle" });
    expect(onStatus).toHaveBeenCalledTimes(1);
  });
});
