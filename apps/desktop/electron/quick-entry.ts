import { BrowserWindow, app, globalShortcut, screen } from "electron";
import path from "node:path";
import { pathToFileURL } from "node:url";
import { QUICK_ENTRY_HASH } from "@borg/app/chords";

/**
 * The global capture window.
 *
 * One frameless always-on-top composer, summoned from anywhere in the OS. It
 * is built once and hidden rather than destroyed, because the round trip of
 * creating a window and loading the renderer is long enough to feel like a
 * stutter on a shortcut people press without thinking.
 *
 * It loads the same bundle as the main window with `#/quick` in the hash, so
 * there is no second renderer build and no second transport.
 */

const WIDTH = 620;
const HEIGHT = 148;
/** Roughly a third down the screen — where a spotlight-style panel belongs. */
const VERTICAL_BIAS = 0.28;

export function registerQuickEntry(options: {
  accelerator: string;
  isDev: boolean;
  devServerUrl?: string;
  indexHtml: string;
  preload: string;
}): { dispose(): void } {
  let win: BrowserWindow | null = null;

  const place = (target: BrowserWindow) => {
    // Follow the pointer's display, not the primary one: the panel should open
    // on the screen the user is looking at.
    const cursor = screen.getCursorScreenPoint();
    const area = screen.getDisplayNearestPoint(cursor).workArea;
    target.setBounds({
      x: Math.round(area.x + (area.width - WIDTH) / 2),
      y: Math.round(area.y + area.height * VERTICAL_BIAS),
      width: WIDTH,
      height: HEIGHT,
    });
  };

  const build = () => {
    const created = new BrowserWindow({
      width: WIDTH,
      height: HEIGHT,
      show: false,
      frame: false,
      resizable: false,
      minimizable: false,
      maximizable: false,
      skipTaskbar: true,
      alwaysOnTop: true,
      transparent: true,
      backgroundColor: "#00000000",
      // Same renderer sandbox invariant as the main window — isolated context,
      // no node integration, preload bridge only.
      webPreferences: {
        preload: options.preload,
        contextIsolation: true,
        nodeIntegration: false,
      },
    });

    // Visible over full-screen apps, which is most of the point of a hotkey.
    created.setVisibleOnAllWorkspaces(true, { visibleOnFullScreen: true });
    // Clicking away dismisses, the same as Spotlight.
    created.on("blur", () => created.hide());

    const target = options.isDev
      ? `${options.devServerUrl}${QUICK_ENTRY_HASH}`
      : `${pathToFileURL(options.indexHtml).href}${QUICK_ENTRY_HASH}`;
    void created.loadURL(target);
    return created;
  };

  const toggle = () => {
    win ??= build();
    if (win.isVisible()) {
      win.hide();
      return;
    }
    place(win);
    win.show();
    win.focus();
  };

  // A shortcut another app already owns simply does not register; that is a
  // conflict to report, not a crash.
  const registered = globalShortcut.register(options.accelerator, toggle);
  if (!registered) {
    console.warn(`[borg] quick entry hotkey ${options.accelerator} is already taken`);
  }

  app.on("will-quit", () => globalShortcut.unregister(options.accelerator));

  return {
    dispose() {
      globalShortcut.unregister(options.accelerator);
      if (win && !win.isDestroyed()) win.destroy();
      win = null;
    },
  };
}
