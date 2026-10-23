import { app, BrowserWindow, shell } from "electron";
import { QUICK_ENTRY_CHORD, acceleratorForChord } from "@borg/app/chords";
import path from "node:path";
import { pathToFileURL } from "node:url";
import { registerBoot } from "./boot";
import { registerBorgIpc } from "./ipc";
import { installApplicationMenu } from "./menu";
import { raise, registerNativeIpc } from "./native";
import { registerQuickEntry } from "./quick-entry";
import { isRecoveryReload, RECOVERY_PAGE_URL } from "./recovery";
import { createReloadBudget } from "./reload-budget";
import { registerUpdater } from "./updater";
import { loadWindowState, MIN_HEIGHT, MIN_WIDTH, trackWindowState } from "./window-state";

const isDev = !!process.env.VITE_DEV_SERVER_URL;
// Resolve against the app root (the directory holding package.json), not
// __dirname: bun's bundler rewrites __dirname to the *source* directory
// (electron/), which pointed the preload path at a file that only exists
// in dist-electron/.
const appRoot = app.getAppPath();
const indexHtml = path.join(appRoot, "dist", "index.html");
const preloadJs = path.join(appRoot, "dist-electron", "preload.js");

/** Summons the quick-entry panel from anywhere in the OS. */
const QUICK_ENTRY_HOTKEY = acceleratorForChord(QUICK_ENTRY_CHORD);

let mainWindow: BrowserWindow | null = null;
/** Set once a quit is under way, when a renderer that goes away is expected. */
let quitting = false;

function createWindow() {
  const saved = loadWindowState();
  const win = new BrowserWindow({
    width: saved.width,
    height: saved.height,
    x: saved.x,
    y: saved.y,
    minWidth: MIN_WIDTH,
    minHeight: MIN_HEIGHT,
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

  const loadApp = () => {
    if (isDev) {
      win.loadURL(process.env.VITE_DEV_SERVER_URL!);
    } else {
      win.loadFile(indexHtml);
    }
  };
  let reloads = createReloadBudget();
  /** True while the recovery page is on screen in place of the app. */
  let recovering = false;

  win.once("ready-to-show", () => {
    if (saved.maximized) win.maximize();
    win.show();
  });
  trackWindowState(win);

  // Security: only hand http(s) URLs to the OS — file:, smb:, custom schemes stay blocked.
  const openExternally = (url: string) => {
    try {
      const { protocol } = new URL(url);
      if (protocol === "https:" || protocol === "http:") void shell.openExternal(url);
    } catch {
      // unparseable URL → not something to hand to the OS
    }
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
    // The recovery page's Reload link. Never a real destination, so it is
    // stopped here whether or not the page is showing.
    if (isRecoveryReload(url)) {
      event.preventDefault();
      if (recovering) {
        recovering = false;
        reloads = createReloadBudget();
        loadApp();
      }
      return;
    }
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

  // A renderer that was killed or ran out of memory leaves a blank window
  // that only a quit clears. Reload it, within a budget: a page that dies on
  // every load must not reload forever. Once the budget is spent the window
  // shows the recovery page instead, whose Reload starts a fresh budget.
  win.webContents.on("render-process-gone", (_event, details) => {
    if (details.reason === "clean-exit" || quitting || win.isDestroyed()) return;
    if (!reloads.take()) {
      console.warn(`renderer keeps failing (${details.reason}); showing the recovery page`);
      // Once only: a recovery page that itself crashes must not loop.
      if (!recovering) {
        recovering = true;
        win.loadURL(RECOVERY_PAGE_URL).catch((error: unknown) => {
          console.error("could not show the recovery page", error);
        });
      }
      return;
    }
    win.reload();
  });

  loadApp();

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
  app.on("before-quit", () => {
    quitting = true;
  });
  app.on("second-instance", () => {
    if (mainWindow) raise(mainWindow);
  });

  app.whenReady().then(() => {
    const borg = registerBorgIpc();
    const native = registerNativeIpc(() => mainWindow);
    // Start the daemon before anything asks it a question. The window opens
    // immediately either way and shows the wait; nothing here blocks it.
    const boot = registerBoot({
      appPath: appRoot,
      resourcesPath: app.isPackaged ? process.resourcesPath : undefined,
    });
    void boot.start();
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
      boot.dispose();
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
