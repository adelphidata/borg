import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { CHANNELS, type WireUpdateStatus } from "./channels";

vi.mock("electron", () => import("./test/electron-stub"));

/** A stand-in for electron-updater's event-emitting singleton. */
const autoUpdater = vi.hoisted(() => {
  const listeners = new Map<string, ((...args: unknown[]) => void)[]>();
  return {
    autoDownload: false,
    autoInstallOnAppQuit: false,
    checkForUpdates: vi.fn(async () => ({})),
    quitAndInstall: vi.fn(),
    on(event: string, listener: (...args: unknown[]) => void) {
      listeners.set(event, [...(listeners.get(event) ?? []), listener]);
      return this;
    },
    removeAllListeners: vi.fn(() => listeners.clear()),
    emit(event: string, ...args: unknown[]) {
      for (const listener of listeners.get(event) ?? []) listener(...args);
    },
    listenerCount: (event: string) => (listeners.get(event) ?? []).length,
  };
});
vi.mock("electron-updater", () => ({ autoUpdater }));

const { handlers, invoke, makeWindow, resetElectron, windows } =
  await import("./test/electron-stub");
const { registerUpdater } = await import("./updater");

let win: ReturnType<typeof makeWindow>;

beforeEach(() => {
  resetElectron();
  autoUpdater.removeAllListeners();
  autoUpdater.checkForUpdates.mockReset().mockResolvedValue({});
  autoUpdater.quitAndInstall.mockReset();
  vi.useFakeTimers();
  win = makeWindow();
  windows.push(win);
});

afterEach(() => {
  vi.useRealTimers();
});

const statuses = () =>
  win.sent
    .filter((s) => s.channel === CHANNELS.updateStatus)
    .map((s) => s.args[0] as WireUpdateStatus);

describe("registerUpdater", () => {
  it("never installs behind the user's back", () => {
    // Downloading quietly is fine; restarting mid-turn is not.
    const updater = registerUpdater({ enabled: true });
    expect(autoUpdater.autoDownload).toBe(true);
    expect(autoUpdater.autoInstallOnAppQuit).toBe(true);
    updater.dispose();
  });

  it("reports each stage of a real update to the renderer", () => {
    const updater = registerUpdater({ enabled: true });
    autoUpdater.emit("checking-for-update");
    autoUpdater.emit("update-available", { version: "1.2.0" });
    autoUpdater.emit("download-progress", { percent: 41.6 });
    autoUpdater.emit("update-downloaded", { version: "1.2.0" });

    expect(statuses()).toEqual([
      { state: "checking" },
      { state: "available", version: "1.2.0" },
      { state: "downloading", percent: 42 },
      { state: "ready", version: "1.2.0" },
    ]);
    updater.dispose();
  });

  it("reports being up to date", () => {
    const updater = registerUpdater({ enabled: true });
    autoUpdater.emit("update-not-available");
    expect(statuses()).toEqual([{ state: "idle" }]);
    updater.dispose();
  });

  it("passes an unreachable feed on as an error rather than throwing", () => {
    // Offline, or a private repo — normal, and the renderer stays quiet unless
    // the user asked for the check.
    const updater = registerUpdater({ enabled: true });
    autoUpdater.emit("error", new Error("net::ERR_INTERNET_DISCONNECTED"));
    expect(statuses()).toEqual([{ state: "error", message: "net::ERR_INTERNET_DISCONNECTED" }]);
    updater.dispose();
  });

  it("does not send to a window that has gone away", () => {
    win.destroy();
    const updater = registerUpdater({ enabled: true });
    autoUpdater.emit("update-not-available");
    expect(win.sent).toEqual([]);
    updater.dispose();
  });

  it("checks shortly after launch and then daily", () => {
    const updater = registerUpdater({ enabled: true });
    expect(autoUpdater.checkForUpdates).not.toHaveBeenCalled();

    vi.advanceTimersByTime(8_000);
    expect(autoUpdater.checkForUpdates).toHaveBeenCalledTimes(1);

    vi.advanceTimersByTime(24 * 60 * 60 * 1000);
    expect(autoUpdater.checkForUpdates).toHaveBeenCalledTimes(2);
    updater.dispose();
  });

  it("turns a rejected check into a reported error", () => {
    const updater = registerUpdater({ enabled: true });
    autoUpdater.checkForUpdates.mockRejectedValueOnce(new Error("403 forbidden"));
    invoke(CHANNELS.updateCheck);
    return Promise.resolve().then(() => {
      expect(statuses().at(-1)).toEqual({ state: "error", message: "403 forbidden" });
      updater.dispose();
    });
  });

  it("checks nothing in development, and says so instead of staying silent", () => {
    // There is no published feed to check against; the pill must not spin.
    const updater = registerUpdater({ enabled: false });
    vi.advanceTimersByTime(24 * 60 * 60 * 1000);
    expect(autoUpdater.checkForUpdates).not.toHaveBeenCalled();

    invoke(CHANNELS.updateCheck);
    expect(statuses()).toEqual([{ state: "idle" }]);
    updater.dispose();
  });

  it("restarts only when the renderer asks, and never in development", () => {
    const dev = registerUpdater({ enabled: false });
    invoke(CHANNELS.updateInstall);
    expect(autoUpdater.quitAndInstall).not.toHaveBeenCalled();
    dev.dispose();

    const prod = registerUpdater({ enabled: true });
    invoke(CHANNELS.updateInstall);
    expect(autoUpdater.quitAndInstall).toHaveBeenCalledTimes(1);
    prod.dispose();
  });

  it("fires no check after being disposed", () => {
    // The startup check is a timeout, not the interval: leaving it armed would
    // fire into removed handlers.
    const updater = registerUpdater({ enabled: true });
    updater.dispose();
    vi.advanceTimersByTime(24 * 60 * 60 * 1000);
    expect(autoUpdater.checkForUpdates).not.toHaveBeenCalled();
    expect(handlers.has(CHANNELS.updateCheck)).toBe(false);
    expect(handlers.has(CHANNELS.updateInstall)).toBe(false);
  });

  it("detaches its listeners on dispose, so a second registration does not double-report", () => {
    const first = registerUpdater({ enabled: true });
    first.dispose();
    const second = registerUpdater({ enabled: true });
    autoUpdater.emit("update-not-available");
    expect(statuses()).toEqual([{ state: "idle" }]);
    second.dispose();
  });
});
