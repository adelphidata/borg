import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import path from "node:path";
import { pathToFileURL } from "node:url";

vi.mock("electron", () => import("./test/electron-stub"));

// The subsystems main wires together have their own suites; here they are
// stubbed so the test is about the wiring and the window's security rules.
const boot = vi.hoisted(() => ({ start: vi.fn(async () => true), dispose: vi.fn() }));
const borgIpc = vi.hoisted(() => ({ dispose: vi.fn() }));
const nativeIpc = vi.hoisted(() => ({ dispose: vi.fn() }));
const updater = vi.hoisted(() => ({ dispose: vi.fn() }));
const quickEntry = vi.hoisted(() => ({ dispose: vi.fn() }));
const spies = vi.hoisted(() => ({
  registerBoot: vi.fn(),
  registerBorgIpc: vi.fn(),
  registerNativeIpc: vi.fn(),
  registerUpdater: vi.fn(),
  registerQuickEntry: vi.fn(),
  installApplicationMenu: vi.fn(),
  trackWindowState: vi.fn(),
  loadWindowState: vi.fn(),
  raise: vi.fn(),
}));

vi.mock("./boot", () => ({
  registerBoot: (...args: unknown[]) => {
    spies.registerBoot(...args);
    return boot;
  },
}));
vi.mock("./ipc", () => ({
  registerBorgIpc: (...args: unknown[]) => {
    spies.registerBorgIpc(...args);
    return borgIpc;
  },
}));
vi.mock("./native", () => ({
  registerNativeIpc: (...args: unknown[]) => {
    spies.registerNativeIpc(...args);
    return nativeIpc;
  },
  raise: (...args: unknown[]) => spies.raise(...args),
}));
vi.mock("./updater", () => ({
  registerUpdater: (...args: unknown[]) => {
    spies.registerUpdater(...args);
    return updater;
  },
}));
vi.mock("./quick-entry", () => ({
  registerQuickEntry: (...args: unknown[]) => {
    spies.registerQuickEntry(...args);
    return quickEntry;
  },
}));
vi.mock("./menu", () => ({
  installApplicationMenu: (...args: unknown[]) => spies.installApplicationMenu(...args),
}));
vi.mock("./window-state", () => ({
  // A test can queue a saved state on the spy (e.g. a maximized one); the
  // default is a plain restored window.
  loadWindowState: (...args: unknown[]) =>
    spies.loadWindowState(...args) ?? { width: 1240, height: 800 },
  trackWindowState: (...args: unknown[]) => spies.trackWindowState(...args),
}));

type Stub = typeof import("./test/electron-stub");

/**
 * `main` runs its wiring at import time, so each scenario re-imports it with a
 * fresh module registry. The electron stub is re-imported alongside it: after a
 * reset, `main` binds to a new instance, and reading the old one would show an
 * empty registry.
 */
let stub: Stub;
let scenario = 0;

const APP_ROOT = "/repo/apps/desktop";
const INDEX_HTML = path.join(APP_ROOT, "dist", "index.html");

/** Boot the main process from scratch, as Electron does on launch. */
async function launch(
  options: {
    dev?: boolean;
    platform?: NodeJS.Platform;
    packaged?: boolean;
    singleInstanceLock?: boolean;
  } = {},
) {
  const realPlatform = process.platform;
  Object.defineProperty(process, "platform", {
    configurable: true,
    value: options.platform ?? realPlatform,
  });
  if (options.dev) process.env.VITE_DEV_SERVER_URL = "http://localhost:5173";
  else delete process.env.VITE_DEV_SERVER_URL;

  vi.resetModules();
  stub = await import("./test/electron-stub");
  stub.resetElectron();
  stub.appState.appPath = APP_ROOT;
  stub.appState.packaged = options.packaged ?? false;
  stub.appState.singleInstanceLock = options.singleInstanceLock ?? true;

  // Cache-busting query: `main` does its work at import time, and a plain
  // re-import after `resetModules` still resolves to the evaluated module.
  await import(`./main?scenario=${scenario++}`);
  // whenReady().then(...) is a microtask; let it run.
  await Promise.resolve();
  await Promise.resolve();
  return () =>
    Object.defineProperty(process, "platform", { configurable: true, value: realPlatform });
}

const restores: Array<() => void> = [];

beforeEach(() => {
  for (const spy of Object.values(spies)) spy.mockClear();
  for (const subsystem of [boot, borgIpc, nativeIpc, updater, quickEntry]) {
    for (const fn of Object.values(subsystem)) fn.mockClear();
  }
});

afterEach(() => {
  while (restores.length) restores.pop()?.();
  delete process.env.VITE_DEV_SERVER_URL;
});

const mainWindow = () => stub.constructedWindows[0];
const openHandler = () => mainWindow().webContents.windowOpenHandler!;
const navigate = (url: string) => {
  const event = { preventDefault: vi.fn() };
  mainWindow().webContents.emit("will-navigate", event, url);
  return event;
};

describe("main process launch", () => {
  it("brings up every subsystem and opens a window", async () => {
    restores.push(await launch());
    expect(spies.registerBorgIpc).toHaveBeenCalled();
    expect(spies.registerNativeIpc).toHaveBeenCalled();
    expect(spies.installApplicationMenu).toHaveBeenCalled();
    expect(spies.registerQuickEntry).toHaveBeenCalled();
    expect(boot.start).toHaveBeenCalledTimes(1);
    expect(stub.constructedWindows).toHaveLength(1);
  });

  it("keeps the renderer out of Node", async () => {
    // The whole desktop security model rests on this pair.
    restores.push(await launch());
    expect(stub.constructedOptions[0].webPreferences).toMatchObject({
      contextIsolation: true,
      nodeIntegration: false,
      preload: path.join(APP_ROOT, "dist-electron", "preload.js"),
    });
  });

  it("creates the window hidden, with the dark chrome it flashes otherwise", async () => {
    // `show: false` avoids a blank-window flash; the background color keeps
    // pre-paint frames from flashing white over the dark theme.
    restores.push(await launch());
    expect(stub.constructedOptions[0]).toMatchObject({
      show: false,
      titleBarStyle: "hiddenInset",
      trafficLightPosition: { x: 15, y: 14 },
      backgroundColor: "#0B0E0B",
    });
  });

  it("re-maximizes a window that was maximized last session", async () => {
    spies.loadWindowState.mockReturnValueOnce({ width: 1240, height: 800, maximized: true });
    restores.push(await launch());
    const win = mainWindow();
    win.emit("ready-to-show");
    expect(win.maximized).toBe(true);
  });

  it("does not maximize a window that was not maximized before", async () => {
    restores.push(await launch());
    const win = mainWindow();
    win.emit("ready-to-show");
    expect(win.maximized).toBe(false);
    expect(win.visible).toBe(true);
  });

  it("boots the daemon against the app root, resources only when packaged", async () => {
    restores.push(await launch());
    expect(spies.registerBoot).toHaveBeenCalledWith({
      appPath: APP_ROOT,
      resourcesPath: undefined,
    });
    restores.pop()?.();

    Object.defineProperty(process, "resourcesPath", { configurable: true, value: "/res" });
    try {
      restores.push(await launch({ packaged: true }));
      expect(spies.registerBoot).toHaveBeenCalledWith({ appPath: APP_ROOT, resourcesPath: "/res" });
    } finally {
      delete (process as { resourcesPath?: string }).resourcesPath;
    }
  });

  it("wires the quick-entry panel with the global hotkey and the app bundle", async () => {
    restores.push(await launch({ dev: true }));
    expect(spies.registerQuickEntry).toHaveBeenCalledWith({
      accelerator: "CmdOrCtrl+Shift+Space",
      isDev: true,
      devServerUrl: "http://localhost:5173",
      indexHtml: INDEX_HTML,
      preload: path.join(APP_ROOT, "dist-electron", "preload.js"),
    });
  });

  it("hands the native bridge a live view of the main window, not a snapshot", async () => {
    // The window can be closed and recreated; a captured reference would send
    // reveals to a destroyed window.
    restores.push(await launch({ platform: "darwin" }));
    const getWindow = spies.registerNativeIpc.mock.calls[0][0] as () => unknown;
    expect(getWindow()).toBe(mainWindow());
    mainWindow().emit("closed");
    expect(getWindow()).toBeNull();
  });

  it("resolves the preload against the app root, not the source directory", async () => {
    // bun's bundler rewrites __dirname to electron/, where preload.js does not
    // exist — this has regressed once.
    restores.push(await launch());
    const preload = (stub.constructedOptions[0].webPreferences as { preload: string }).preload;
    expect(preload).toBe(path.join(APP_ROOT, "dist-electron", "preload.js"));
    expect(preload).not.toContain(`${path.sep}electron${path.sep}`);
  });

  it("loads the packaged bundle from disk", async () => {
    restores.push(await launch());
    expect(mainWindow().loadFile).toHaveBeenCalledWith(INDEX_HTML);
  });

  it("loads the dev server when one is running", async () => {
    restores.push(await launch({ dev: true }));
    expect(mainWindow().loadURL).toHaveBeenCalledWith("http://localhost:5173");
  });

  it("checks for updates only for a packaged build", async () => {
    // A dev build has no published feed, and an unpackaged one is a checkout.
    restores.push(await launch({ dev: true, packaged: true }));
    expect(spies.registerUpdater).toHaveBeenLastCalledWith({ enabled: false });
    restores.pop()?.();

    restores.push(await launch({ packaged: false }));
    expect(spies.registerUpdater).toHaveBeenLastCalledWith({ enabled: false });
    restores.pop()?.();

    restores.push(await launch({ packaged: true }));
    expect(spies.registerUpdater).toHaveBeenLastCalledWith({ enabled: true });
  });

  it("quits instead of fighting an instance that already holds the socket", async () => {
    restores.push(await launch({ singleInstanceLock: false }));
    expect(stub.appState.quit).toBe(1);
    expect(stub.constructedWindows).toHaveLength(0);
  });

  it("raises the existing window on a second launch", async () => {
    restores.push(await launch());
    stub.emitApp("second-instance");
    expect(spies.raise).toHaveBeenCalledWith(mainWindow());
  });

  it("raises nothing on a second launch when no window is left", async () => {
    restores.push(await launch({ platform: "darwin" }));
    mainWindow().emit("closed"); // last window gone; app keeps running on macOS
    spies.raise.mockClear();
    stub.emitApp("second-instance");
    expect(spies.raise).not.toHaveBeenCalled();
  });

  it("tears every subsystem down on quit", async () => {
    restores.push(await launch());
    stub.emitApp("will-quit");
    for (const subsystem of [boot, borgIpc, nativeIpc, updater, quickEntry]) {
      expect(subsystem.dispose).toHaveBeenCalledTimes(1);
    }
  });

  it("restores a maximized window only once it is ready to show", async () => {
    // Showing before first paint is a white flash; maximizing before show is a
    // resize the user watches happen.
    spies.loadWindowState.mockImplementationOnce(() => {});
    restores.push(await launch());
    const win = mainWindow();
    expect(win.visible).toBe(false);
    win.emit("ready-to-show");
    expect(win.visible).toBe(true);
  });

  it("remembers where the window was", async () => {
    restores.push(await launch());
    expect(spies.trackWindowState).toHaveBeenCalledWith(mainWindow());
  });
});

describe("window security", () => {
  it("never opens an in-app window", async () => {
    restores.push(await launch());
    expect(openHandler()({ url: "https://example.com" })).toEqual({ action: "deny" });
    expect(stub.externalOpens).toEqual(["https://example.com"]);
  });

  it("hands only http and https to the OS", async () => {
    // file:, smb:, and custom schemes reaching openExternal is a local-code
    // execution route.
    restores.push(await launch());
    for (const url of [
      "file:///etc/passwd",
      "smb://share/x",
      "borg://run",
      "javascript:alert(1)",
    ]) {
      openHandler()({ url });
    }
    expect(stub.externalOpens).toEqual([]);

    openHandler()({ url: "http://example.com" });
    expect(stub.externalOpens).toEqual(["http://example.com"]);
  });

  it("ignores an unparseable URL rather than throwing in the handler", async () => {
    restores.push(await launch());
    expect(() => openHandler()({ url: "not a url" })).not.toThrow();
    expect(stub.externalOpens).toEqual([]);
  });

  it("allows navigation within the packaged app", async () => {
    restores.push(await launch());
    const event = navigate(`${pathToFileURL(INDEX_HTML).href}#/settings`);
    expect(event.preventDefault).not.toHaveBeenCalled();
  });

  it("blocks any other local file, not just a different app", async () => {
    // A prefix check would let every file:// path under the app root count as
    // internal.
    restores.push(await launch());
    const event = navigate(pathToFileURL(path.join(APP_ROOT, "dist", "evil.html")).href);
    expect(event.preventDefault).toHaveBeenCalled();
  });

  it("blocks an external site and sends it to the browser instead", async () => {
    restores.push(await launch());
    const event = navigate("https://evil.example.com");
    expect(event.preventDefault).toHaveBeenCalled();
    expect(stub.externalOpens).toEqual(["https://evil.example.com"]);
  });

  it("allows navigation within the dev server's origin", async () => {
    restores.push(await launch({ dev: true }));
    const event = navigate("http://localhost:5173/#/usage");
    expect(event.preventDefault).not.toHaveBeenCalled();
  });

  it("blocks another port on the same host in dev", async () => {
    // localhost:51737 is a different origin, however local it looks.
    restores.push(await launch({ dev: true }));
    const event = navigate("http://localhost:51737/");
    expect(event.preventDefault).toHaveBeenCalled();
  });

  it("blocks an unparseable navigation target", async () => {
    restores.push(await launch());
    const event = navigate("http://[::");
    expect(event.preventDefault).toHaveBeenCalled();
  });
});

describe("window lifecycle", () => {
  it("keeps running on macOS when the last window closes", async () => {
    // The daemon is the product; closing a window is not a request to stop.
    restores.push(await launch({ platform: "darwin" }));
    mainWindow().emit("closed");
    expect(stub.appState.quit).toBe(0);
  });

  it("quits elsewhere, so no invisible app is left behind", async () => {
    // `window-all-closed` never fires while the hidden quick-entry panel exists.
    restores.push(await launch({ platform: "win32" }));
    mainWindow().emit("closed");
    expect(stub.appState.quit).toBeGreaterThan(0);
  });

  it("raises the existing window when the dock icon is clicked", async () => {
    restores.push(await launch({ platform: "darwin" }));
    stub.emitApp("activate");
    expect(spies.raise).toHaveBeenCalledWith(mainWindow());
    expect(stub.constructedWindows).toHaveLength(1);
  });

  it("rebuilds the window after the last one was closed", async () => {
    // Testing the main window rather than the window count is the point: the
    // hidden quick-entry panel is a BrowserWindow too.
    restores.push(await launch({ platform: "darwin" }));
    mainWindow().emit("closed");
    stub.emitApp("activate");
    expect(stub.constructedWindows).toHaveLength(2);
  });

  it("quits on window-all-closed away from macOS", async () => {
    restores.push(await launch({ platform: "linux" }));
    const before = stub.appState.quit;
    stub.emitApp("window-all-closed");
    expect(stub.appState.quit).toBeGreaterThan(before);
  });

  it("stays alive on window-all-closed on macOS", async () => {
    restores.push(await launch({ platform: "darwin" }));
    stub.emitApp("window-all-closed");
    expect(stub.appState.quit).toBe(0);
  });

  it("keeps the current window when a stale closed event arrives from an old one", async () => {
    restores.push(await launch({ platform: "darwin" }));
    const first = mainWindow();
    first.emit("closed");
    stub.emitApp("activate"); // rebuilds — the second window is now the main one
    const second = stub.constructedWindows[1];
    first.emit("closed"); // a late event from the already-closed window
    stub.emitApp("activate");
    expect(spies.raise).toHaveBeenCalledWith(second);
    expect(stub.constructedWindows).toHaveLength(2);
  });
});
