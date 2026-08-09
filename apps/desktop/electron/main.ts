import { app, BrowserWindow, shell } from "electron";
import path from "node:path";
import { pathToFileURL } from "node:url";

const isDev = !!process.env.VITE_DEV_SERVER_URL;
const indexHtml = path.join(__dirname, "..", "dist", "index.html");

function createWindow() {
  const win = new BrowserWindow({
    width: 1240,
    height: 800,
    minWidth: 360,
    minHeight: 600,
    show: false, // shown on ready-to-show to avoid a blank-window flash
    titleBarStyle: "hiddenInset",
    trafficLightPosition: { x: 15, y: 14 },
    backgroundColor: "#0B0E0B", // matches dark theme so pre-paint frames don't flash white
    webPreferences: {
      preload: path.join(__dirname, "preload.js"),
      contextIsolation: true,
      nodeIntegration: false,
    },
  });

  win.once("ready-to-show", () => win.show());

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
}

app.whenReady().then(() => {
  createWindow();
  app.on("activate", () => {
    if (BrowserWindow.getAllWindows().length === 0) createWindow();
  });
});

app.on("window-all-closed", () => {
  if (process.platform !== "darwin") app.quit();
});
