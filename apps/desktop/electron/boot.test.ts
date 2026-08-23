import { afterEach, describe, expect, it, vi } from "vitest";
import { CHANNELS, type WireBootStatus } from "./channels";

/** The handlers `registerBoot` installs, captured from the fake ipcMain. */
const handlers = new Map<string, (...args: unknown[]) => unknown>();
const sent: { channel: string; payload: unknown }[] = [];
let destroyed = false;

vi.mock("electron", () => ({
  ipcMain: {
    handle: (channel: string, handler: (...args: unknown[]) => unknown) =>
      handlers.set(channel, handler),
    removeHandler: (channel: string) => handlers.delete(channel),
  },
  BrowserWindow: {
    getAllWindows: () => [
      {
        isDestroyed: () => destroyed,
        webContents: {
          isDestroyed: () => destroyed,
          send: (channel: string, payload: unknown) => sent.push({ channel, payload }),
        },
      },
    ],
  },
}));

const { registerBoot } = await import("./boot");

afterEach(() => {
  handlers.clear();
  sent.length = 0;
  destroyed = false;
});

const deps = {
  appPath: "/repo/apps/desktop",
  env: { PATH: "/usr/bin", HOME: "/home/me" },
  platform: "linux" as NodeJS.Platform,
  socketPath: "/home/me/.borg/borgd.sock",
  resolve: () => "/usr/bin/borgd",
  launch: () => null,
  waitFor: async () => true,
};

const read = () => handlers.get(CHANNELS.bootRead)!() as WireBootStatus;

describe("registerBoot", () => {
  it("pushes every step to the window that is open", async () => {
    const boot = registerBoot({ ...deps, reachable: async () => true });
    await boot.start();
    expect(sent.map((s) => s.channel)).toEqual([CHANNELS.bootStatus, CHANNELS.bootStatus]);
    expect((sent.at(-1)!.payload as WireBootStatus).state).toBe("ready");
    boot.dispose();
  });

  it("replays the last status to a window that attaches late", async () => {
    // A reloaded renderer missed every broadcast; without the replay it would
    // sit on an empty screen with a daemon that is already up.
    const boot = registerBoot({ ...deps, reachable: async () => true });
    await boot.start();
    expect(read().state).toBe("ready");
    boot.dispose();
  });

  it("does not send to a window that has gone away", async () => {
    destroyed = true;
    const boot = registerBoot({ ...deps, reachable: async () => true });
    await boot.start();
    expect(sent).toEqual([]);
    boot.dispose();
  });

  it("joins the attempt already running rather than starting a second daemon", async () => {
    let release: (() => void) | undefined;
    const launch = vi.fn(() => null);
    const boot = registerBoot({
      ...deps,
      launch,
      reachable: async () => false,
      waitFor: () => new Promise<boolean>((resolve) => (release = () => resolve(true))),
    });
    const first = boot.start();
    const second = handlers.get(CHANNELS.bootRetry)!() as Promise<boolean>;
    // Let the probe/spawn microtasks run so the wait is actually in flight.
    while (!release) await new Promise((resolve) => setTimeout(resolve, 0));
    release();
    await Promise.all([first, second]);
    expect(launch).toHaveBeenCalledTimes(1);
    boot.dispose();
  });

  it("retries after a failure instead of latching it forever", async () => {
    vi.spyOn(console, "error").mockImplementation(() => {});
    let found = false;
    const boot = registerBoot({
      ...deps,
      reachable: async () => false,
      resolve: () => (found ? "/usr/bin/borgd" : null),
    });
    await boot.start();
    expect(read().state).toBe("failed");
    found = true;
    await (handlers.get(CHANNELS.bootRetry)!() as Promise<boolean>);
    expect(read().state).toBe("ready");
    boot.dispose();
  });

  it("ends a boot that throws in a state the user can act on", async () => {
    // Anything unexpected still has to land on the failure screen: a silent
    // rejection would leave the window on a step that never finishes.
    vi.spyOn(console, "error").mockImplementation(() => {});
    const boot = registerBoot({
      ...deps,
      reachable: async () => false,
      resolve: () => {
        throw new Error("boom");
      },
    });
    await expect(boot.start()).resolves.toBe(false);
    expect(read()).toMatchObject({ state: "failed", canRetry: true });
    boot.dispose();
  });

  it("leaves no handlers behind when disposed", () => {
    const boot = registerBoot(deps);
    boot.dispose();
    expect(handlers.has(CHANNELS.bootRead)).toBe(false);
    expect(handlers.has(CHANNELS.bootRetry)).toBe(false);
  });
});
