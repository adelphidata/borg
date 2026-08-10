import { app, BrowserWindow, shell } from "electron";
import path from "node:path";
import { pathToFileURL } from "node:url";
import { registerBorgIpc } from "./ipc";
import { installApplicationMenu } from "./menu";
import { raise, registerNativeIpc } from "./native";
import { registerQuickEntry } from "./quick-entry";
import { registerUpdater } from "./updater";
import { loadWindowState, trackWindowState } from "./window-state";

const isDev = !!process.env.VITE_DEV_SERVER_URL;
// Resolve against the app root (the directory holding package.json), not
// __dirname: bun's bundler rewrites __dirname to the *source* directory
// (electron/), which pointed the preload path at a file that only exists
// in dist-electron/.
const appRoot = app.getAppPath();
const indexHtml = path.join(appRoot, "dist", "index.html");
const preloadJs = path.join(appRoot, "dist-electron", "preload.js");

/** Summons the quick-entry panel from anywhere in the OS. */
const QUICK_ENTRY_HOTKEY = "CmdOrCtrl+Shift+Space";

let mainWindow: BrowserWindow | null = null;

function createWindow() {
  const saved = loadWindowState();
  const win = new BrowserWindow({
    width: saved.width,
    height: saved.height,
    x: saved.x,
    y: saved.y,
    minWidth: 360,
    minHeight: 600,
    show: false, // shown on ready-to-show to avoid a blank-window flash
    titleBarStyle: "hiddenInset",
    trafficLightPosition: { x: 15, y: 14 },
    backgroundColor: "#0B0E0B", // matches dark theme so pre-paint frames don't flash white
    // Renderer sandbox invariant: context isolation on, node integration off.
    // The typed preload bridge is the renderer's only route to the daemon.
    webPreferences: {
      preload: preloadJs,
      contextIsolation: true,
      nodeIntegration: false,
    },
  });

  win.once("ready-to-show", () => {
    if (saved.maximized) win.maximize();
    win.show();
  });
  trackWindowState(win);

  // Security: only hand http(s) URLs to the OS — file:, smb:, custom schemes stay blocked.
  const openExternally = (url: string) => {
    let protocol = "";
    try {
      protocol = new URL(url).protocol;
    } catch {
      return;
    }
    if (protocol === "https:" || protocol === "http:") void shell.openExternal(url);
  };

  // Security: never spawn in-app windows — external links go to the system browser.
  win.webContents.setWindowOpenHandler(({ url }) => {
    openExternally(url);
    return { action: "deny" };
  });

  // Security: keep top-level navigation inside the app; send anything else to the browser.
  // Exact origin/file comparison — prefix checks would let localhost:51737 (dev) or any
  // local file:// path (prod) count as internal.
  win.webContents.on("will-navigate", (event, url) => {
    let internal = false;
    try {
      const target = new URL(url);
      target.search = "";
      target.hash = "";
      internal = isDev
        ? target.origin === new URL(process.env.VITE_DEV_SERVER_URL!).origin
        : target.href === pathToFileURL(indexHtml).href;
    } catch {
      // unparseable URL → treated as external below
    }
    if (!internal) {
      event.preventDefault();
      openExternally(url);
    }
  });

  if (isDev) {
    win.loadURL(process.env.VITE_DEV_SERVER_URL!);
  } else {
    win.loadFile(indexHtml);
  }

  win.on("closed", () => {
    if (mainWindow === win) mainWindow = null;
    // `window-all-closed` never fires while the hidden quick-entry panel
    // exists, so closing the last real window would otherwise leave an
    // invisible app running on Windows and Linux.
    if (process.platform !== "darwin") app.quit();
  });
  mainWindow = win;
  return win;
}

// A second launch — from the dock, a file open, or a stray CLI invocation —
// must raise the window that already exists rather than start a rival instance
// fighting it for the same daemon socket.
if (!app.requestSingleInstanceLock()) {
  app.quit();
} else {
  app.on("second-instance", () => {
    if (mainWindow) raise(mainWindow);
  });

  app.whenReady().then(() => {
    const borg = registerBorgIpc();
    const native = registerNativeIpc(() => mainWindow);
    // In dev there is no published feed, so checking would only log failures.
    const updater = registerUpdater({ enabled: !isDev && app.isPackaged });
    const quickEntry = registerQuickEntry({
      accelerator: QUICK_ENTRY_HOTKEY,
      isDev,
      devServerUrl: process.env.VITE_DEV_SERVER_URL,
      indexHtml,
      preload: preloadJs,
    });

    app.once("will-quit", () => {
      borg.dispose();
      native.dispose();
      updater.dispose();
      quickEntry.dispose();
    });

    installApplicationMenu();
    createWindow();
    app.on("activate", () => {
      // Must test the main window, not the window count: the hidden
      // quick-entry panel is a BrowserWindow too, so once it has been summoned
      // `getAllWindows()` is never empty and clicking the dock icon after
      // closing the last real window would do nothing at all.
      if (mainWindow && !mainWindow.isDestroyed()) {
        raise(mainWindow);
        return;
      }
      createWindow();
    });
  });
}

app.on("window-all-closed", () => {
  if (process.platform !== "darwin") app.quit();
});
