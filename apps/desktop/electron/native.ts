import { app, BrowserWindow, Notification, ipcMain } from "electron";
import { CHANNELS, type WireNotification } from "./channels";

/**
 * The native half of the `NativeBridge` the renderer sees.
 *
 * Notifications, the dock badge, and the menu→renderer command path. The
 * updater registers its own handlers on the same channels (see `updater.ts`);
 * this module owns everything that needs no external dependency.
 */

/** Notification keys already on screen, so a repeat replaces instead of stacking. */
const live = new Map<string, Notification>();

/** Sends a keybind action id to the focused window — the menu's only job. */
export function sendMenuCommand(action: string): void {
  const win = BrowserWindow.getFocusedWindow() ?? BrowserWindow.getAllWindows()[0];
  if (win && !win.isDestroyed()) win.webContents.send(CHANNELS.menuCommand, action);
}

/** Brings `window` forward and asks the renderer to open `sessionId`. */
export function activateSession(window: BrowserWindow, sessionId: string): void {
  if (window.isDestroyed()) return;
  if (window.isMinimized()) window.restore();
  window.show();
  window.focus();
  window.webContents.send(CHANNELS.activateSession, sessionId);
}

/**
 * Registers the native handlers.
 *
 * `mainWindow` is read lazily rather than captured: the window can be closed
 * and recreated (macOS dock activate), and a stale reference would send the
 * reveal to a destroyed window.
 */
export function registerNativeIpc(mainWindow: () => BrowserWindow | null): { dispose(): void } {
  ipcMain.handle(CHANNELS.notify, (event, notification: WireNotification) => {
    if (!Notification.isSupported()) return;

    const key = notification.key;
    if (key) live.get(key)?.close();

    const shown = new Notification({ title: notification.title, body: notification.body });
    if (key) {
      live.set(key, shown);
      shown.on("close", () => {
        if (live.get(key) === shown) live.delete(key);
      });
    }

    // Clicking a notification should land the user on the conversation it is
    // about, not just anywhere in the app.
    shown.on("click", () => {
      const win = BrowserWindow.fromWebContents(event.sender);
      if (!win) return;
      if (notification.sessionId) activateSession(win, notification.sessionId);
      else {
        if (win.isMinimized()) win.restore();
        win.show();
        win.focus();
      }
    });

    shown.show();
  });

  // The quick-entry panel starts a turn and then gets out of the way; the
  // conversation itself belongs in the main window.
  ipcMain.handle(CHANNELS.revealSession, (_event, sessionId: string) => {
    const win = mainWindow();
    if (win) activateSession(win, sessionId);
  });

  ipcMain.handle(CHANNELS.hideWindow, (event) => {
    BrowserWindow.fromWebContents(event.sender)?.hide();
  });

  ipcMain.handle(CHANNELS.setBadge, (_event, count: number) => {
    // Linux support depends on the desktop environment; Electron no-ops there.
    if (typeof app.setBadgeCount === "function") app.setBadgeCount(Math.max(0, count));
  });

  return {
    dispose() {
      for (const notification of live.values()) notification.close();
      live.clear();
      ipcMain.removeHandler(CHANNELS.notify);
      ipcMain.removeHandler(CHANNELS.revealSession);
      ipcMain.removeHandler(CHANNELS.hideWindow);
      ipcMain.removeHandler(CHANNELS.setBadge);
    },
  };
}
