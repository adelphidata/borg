/**
 * A stand-in for the `electron` module.
 *
 * The main process is ordinary logic — which binary to launch, which window to
 * raise, what to persist — wrapped around Electron's objects. Loading the real
 * module needs the Electron binary and a running app, so these tests mock it
 * and drive the same logic directly.
 *
 * Deliberately stateful rather than a bag of `vi.fn()`s: `ipcMain.handle`
 * registering a handler that `removeHandler` later takes away is the contract
 * the dispose paths are tested against, so the stub has to actually keep the
 * registry.
 */
import { vi, type Mock } from "vitest";

/**
 * All mutable state lives on one globally shared object.
 *
 * A test that re-imports the main process with a fresh module registry gets a
 * second copy of this module, and Vitest's mock of `electron` resolves to its
 * own copy again. Without a shared store the test would inspect one registry
 * while the code under test wrote to another — the arrays would simply look
 * empty. Keyed on `globalThis` so every copy reads and writes the same store.
 */
interface SharedState {
  windows: StubWindow[];
  constructedWindows: StubWindow[];
  constructedOptions: Record<string, unknown>[];
  focused: StubWindow | null;
  handlers: Map<string, (...args: unknown[]) => unknown>;
  notifications: StubNotification[];
  displays: { bounds: { x: number; y: number; width: number; height: number } }[];
  notificationState: { supported: boolean };
  appState: {
    name: string;
    paths: Record<string, string>;
    badge: number;
    listeners: Map<string, ((...args: unknown[]) => void)[]>;
    appPath: string;
    packaged: boolean;
    singleInstanceLock: boolean;
    quit: number;
  };
  cursor: { point: { x: number; y: number } };
  /** URLs handed to the OS browser, in order. */
  externalOpens: string[];
  menuState: { template: unknown; applied: unknown };
  shortcutState: { registered: Map<string, () => void>; available: boolean };
  contextBridgeState: { exposed: Map<string, unknown> };
  rendererState: {
    listeners: Map<string, ((...args: unknown[]) => void)[]>;
    invocations: { channel: string; args: unknown[] }[];
    replies: Map<string, (...args: unknown[]) => unknown>;
  };
}

const shared: SharedState = ((globalThis as Record<string, unknown>).__borgElectronStub ??= {
  windows: [],
  constructedWindows: [],
  constructedOptions: [],
  focused: null,
  handlers: new Map(),
  notifications: [],
  displays: [{ bounds: { x: 0, y: 0, width: 1920, height: 1080 } }],
  notificationState: { supported: true },
  appState: {
    name: "Borg",
    paths: { userData: "/tmp/borg-userdata" },
    badge: 0,
    listeners: new Map(),
    appPath: "/repo/apps/desktop",
    packaged: false,
    singleInstanceLock: true,
    quit: 0,
  },
  cursor: { point: { x: 100, y: 100 } },
  externalOpens: [],
  menuState: { template: null, applied: null },
  shortcutState: { registered: new Map(), available: true },
  contextBridgeState: { exposed: new Map() },
  rendererState: { listeners: new Map(), invocations: [], replies: new Map() },
}) as SharedState;

export interface StubWindow {
  id: number;
  destroyed: boolean;
  visible: boolean;
  minimized: boolean;
  maximized: boolean;
  bounds: { x: number; y: number; width: number; height: number };
  sent: { channel: string; args: unknown[] }[];
  listeners: Map<string, ((...args: unknown[]) => void)[]>;
  webContents: {
    isDestroyed(): boolean;
    send(channel: string, ...args: unknown[]): void;
    /** Handler installed by `setWindowOpenHandler`, if any. */
    windowOpenHandler?: (details: { url: string }) => { action: string };
    setWindowOpenHandler(handler: (details: { url: string }) => { action: string }): void;
    listeners: Map<string, ((...args: unknown[]) => void)[]>;
    on(event: string, listener: (...args: unknown[]) => void): void;
    emit(event: string, ...args: unknown[]): void;
  };
  maximize(): void;
  loadFile: Mock<(file: string) => Promise<void>>;
  isDestroyed(): boolean;
  isVisible(): boolean;
  isMinimized(): boolean;
  isMaximized(): boolean;
  getNormalBounds(): { x: number; y: number; width: number; height: number };
  getBounds(): { x: number; y: number; width: number; height: number };
  setBounds(bounds: { x: number; y: number; width: number; height: number }): void;
  show(): void;
  hide(): void;
  focus(): void;
  restore(): void;
  destroy(): void;
  /** Spies, so a test can read what the code under test loaded or set. */
  loadURL: Mock<(url: string) => Promise<void>>;
  setVisibleOnAllWorkspaces: Mock<(value: boolean, options?: unknown) => void>;
  on(event: string, listener: (...args: unknown[]) => void): StubWindow;
  once(event: string, listener: (...args: unknown[]) => void): StubWindow;
  emit(event: string, ...args: unknown[]): void;
}

let nextWindowId = 1;

export function makeWindow(overrides: Partial<StubWindow> = {}): StubWindow {
  const win: StubWindow = {
    id: nextWindowId++,
    destroyed: false,
    visible: true,
    minimized: false,
    maximized: false,
    bounds: { x: 0, y: 0, width: 1240, height: 800 },
    sent: [],
    listeners: new Map(),
    webContents: {
      isDestroyed: () => win.destroyed,
      send: (channel: string, ...args: unknown[]) => win.sent.push({ channel, args }),
      setWindowOpenHandler: (handler) => {
        win.webContents.windowOpenHandler = handler;
      },
      listeners: new Map(),
      on: (event, listener) => {
        const wc = win.webContents;
        wc.listeners.set(event, [...(wc.listeners.get(event) ?? []), listener]);
      },
      emit: (event, ...args) => {
        for (const listener of win.webContents.listeners.get(event) ?? []) listener(...args);
      },
    },
    maximize: () => {
      win.maximized = true;
    },
    loadFile: vi.fn(async () => {}),
    isDestroyed: () => win.destroyed,
    isVisible: () => win.visible,
    isMinimized: () => win.minimized,
    isMaximized: () => win.maximized,
    getNormalBounds: () => win.bounds,
    getBounds: () => win.bounds,
    setBounds: (bounds) => {
      win.bounds = bounds;
    },
    show: () => {
      win.visible = true;
    },
    hide: () => {
      win.visible = false;
    },
    focus: () => {},
    restore: () => {
      win.minimized = false;
    },
    destroy: () => {
      win.destroyed = true;
    },
    loadURL: vi.fn(async () => {}),
    setVisibleOnAllWorkspaces: vi.fn(),
    on: (event, listener) => {
      win.listeners.set(event, [...(win.listeners.get(event) ?? []), listener]);
      return win;
    },
    once: (event, listener) => win.on(event, listener),
    emit: (event, ...args) => {
      for (const listener of win.listeners.get(event) ?? []) listener(...args);
    },
    ...overrides,
  };
  return win;
}

/** Every window `BrowserWindow.getAllWindows()` should report. */
export const windows = shared.windows;
/** The window `getFocusedWindow()` returns; falls back to the first window. */
export function setFocused(win: StubWindow | null) {
  shared.focused = win;
}

/** Handlers registered through `ipcMain.handle`, by channel. */
export const handlers = shared.handlers;

export const ipcMain = {
  handle: vi.fn((channel: string, handler: (...args: unknown[]) => unknown) => {
    handlers.set(channel, handler);
  }),
  removeHandler: vi.fn((channel: string) => {
    handlers.delete(channel);
  }),
};

/** Invoke a registered handler the way the renderer would. */
export function invoke(channel: string, ...args: unknown[]): unknown {
  const handler = handlers.get(channel);
  if (!handler) throw new Error(`no handler registered for ${channel}`);
  return handler(...args);
}

/** Notifications constructed by the code under test. */
export interface StubNotification {
  options: { title: string; body: string };
  shown: boolean;
  closed: boolean;
  listeners: Map<string, (() => void)[]>;
  show(): void;
  close(): void;
  on(event: string, listener: () => void): StubNotification;
  emit(event: string): void;
}

export const notifications = shared.notifications;
export const notificationState = shared.notificationState;

export class Notification {
  constructor(options: { title: string; body: string }) {
    const record: StubNotification = {
      options,
      shown: false,
      closed: false,
      listeners: new Map(),
      show: () => {
        record.shown = true;
      },
      close: () => {
        record.closed = true;
        record.emit("close");
      },
      on: (event, listener) => {
        record.listeners.set(event, [...(record.listeners.get(event) ?? []), listener]);
        return record;
      },
      emit: (event) => {
        for (const listener of record.listeners.get(event) ?? []) listener();
      },
    };
    notifications.push(record);
    return record as unknown as Notification;
  }

  static isSupported() {
    return notificationState.supported;
  }
}

/** Windows built with `new BrowserWindow(...)`, and the options each was given. */
export const constructedWindows = shared.constructedWindows;
export const constructedOptions = shared.constructedOptions;

/**
 * Constructible, because `quick-entry` builds its panel with `new`. The statics
 * are the same registry every other module reads.
 */
export class BrowserWindow {
  constructor(options: Record<string, unknown> = {}) {
    const win = makeWindow({ visible: false });
    constructedWindows.push(win);
    constructedOptions.push(options);
    windows.push(win);
    return win as unknown as BrowserWindow;
  }

  static getAllWindows = () => windows;
  static getFocusedWindow = () => shared.focused ?? null;
  static fromWebContents = (sender: unknown) =>
    windows.find((w) => w.webContents === sender) ?? null;
}

export const appState = shared.appState;

export const app = {
  get name() {
    return appState.name;
  },
  get isPackaged() {
    return appState.packaged;
  },
  getPath: (key: string) => appState.paths[key] ?? `/tmp/${key}`,
  getAppPath: () => appState.appPath,
  setBadgeCount: vi.fn((count: number) => {
    appState.badge = count;
    return true;
  }),
  on: vi.fn((event: string, listener: (...args: unknown[]) => void) => {
    appState.listeners.set(event, [...(appState.listeners.get(event) ?? []), listener]);
  }),
  once: vi.fn((event: string, listener: (...args: unknown[]) => void) => {
    appState.listeners.set(event, [...(appState.listeners.get(event) ?? []), listener]);
  }),
  requestSingleInstanceLock: vi.fn(() => appState.singleInstanceLock),
  whenReady: vi.fn(async () => {}),
  quit: vi.fn(() => {
    appState.quit += 1;
  }),
};

/** Fire an app-level event the way Electron would. */
export function emitApp(event: string, ...args: unknown[]) {
  for (const listener of appState.listeners.get(event) ?? []) listener(...args);
}

export const displays = shared.displays;
export const cursor = shared.cursor;

export const screen = {
  getAllDisplays: () => displays,
  getCursorScreenPoint: () => cursor.point,
  // Picks the display the point actually falls in, so a test can put the
  // cursor on a second monitor and get that monitor's work area back.
  getDisplayNearestPoint: (point: { x: number; y: number }) => {
    const found = displays.find(
      (d) =>
        point.x >= d.bounds.x &&
        point.x < d.bounds.x + d.bounds.width &&
        point.y >= d.bounds.y &&
        point.y < d.bounds.y + d.bounds.height,
    );
    return { workArea: (found ?? displays[0]).bounds };
  },
};

/** URLs `shell.openExternal` was given. Shared, so a re-imported main process
 * records into the same list the test reads. */
export const externalOpens = shared.externalOpens;

export const shell = {
  openExternal: vi.fn(async (url: string) => {
    shared.externalOpens.push(url);
  }),
};

export const menuState = shared.menuState;

export const Menu = {
  buildFromTemplate: vi.fn((template: unknown) => {
    menuState.template = template;
    return { template };
  }),
  setApplicationMenu: vi.fn((menu: unknown) => {
    menuState.applied = menu;
  }),
};

export const shortcutState = shared.shortcutState;

export const globalShortcut = {
  register: vi.fn((accelerator: string, callback: () => void) => {
    if (!shortcutState.available) return false;
    shortcutState.registered.set(accelerator, callback);
    return true;
  }),
  unregister: vi.fn((accelerator: string) => {
    shortcutState.registered.delete(accelerator);
  }),
};

export const contextBridgeState = shared.contextBridgeState;
export const contextBridge = {
  exposeInMainWorld: vi.fn((key: string, value: unknown) => {
    contextBridgeState.exposed.set(key, value);
  }),
};

export const rendererState = shared.rendererState;

export const ipcRenderer = {
  invoke: vi.fn(async (channel: string, ...args: unknown[]) => {
    rendererState.invocations.push({ channel, args });
    const reply = rendererState.replies.get(channel);
    return reply ? await reply(...args) : undefined;
  }),
  on: vi.fn((channel: string, listener: (...args: unknown[]) => void) => {
    rendererState.listeners.set(channel, [
      ...(rendererState.listeners.get(channel) ?? []),
      listener,
    ]);
  }),
  off: vi.fn((channel: string, listener: (...args: unknown[]) => void) => {
    rendererState.listeners.set(
      channel,
      (rendererState.listeners.get(channel) ?? []).filter((l) => l !== listener),
    );
  }),
};

/** Deliver a main→renderer push to whatever the preload subscribed. */
export function pushToRenderer(channel: string, ...args: unknown[]) {
  for (const listener of rendererState.listeners.get(channel) ?? []) listener({}, ...args);
}

/** Reset every piece of shared state between tests. */
export function resetElectron() {
  windows.length = 0;
  constructedWindows.length = 0;
  constructedOptions.length = 0;
  shared.focused = null;
  handlers.clear();
  notifications.length = 0;
  notificationState.supported = true;
  appState.badge = 0;
  appState.listeners.clear();
  appState.packaged = false;
  appState.singleInstanceLock = true;
  appState.quit = 0;
  displays.length = 0;
  displays.push({ bounds: { x: 0, y: 0, width: 1920, height: 1080 } });
  shared.cursor.point = { x: 100, y: 100 };
  shared.externalOpens.length = 0;
  menuState.template = null;
  menuState.applied = null;
  shortcutState.registered.clear();
  shortcutState.available = true;
  contextBridgeState.exposed.clear();
  rendererState.listeners.clear();
  rendererState.invocations.length = 0;
  rendererState.replies.clear();
  vi.clearAllMocks();
}
