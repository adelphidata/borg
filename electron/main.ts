import { app, BrowserWindow, shell } from "electron";
import path from "node:path";

const isDev = !!process.env.VITE_DEV_SERVER_URL;

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
  win.webContents.on("will-navigate", (event, url) => {
    const internal = isDev
      ? url.startsWith(process.env.VITE_DEV_SERVER_URL!)
      : url.startsWith("file:");
    if (!internal) {
      event.preventDefault();
      openExternally(url);
    }
  });

  if (isDev) {
    win.loadURL(process.env.VITE_DEV_SERVER_URL!);
  } else {
    win.loadFile(path.join(__dirname, "..", "dist", "index.html"));
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
