import { app, screen, type BrowserWindow, type Rectangle } from "electron";
import { readFileSync, writeFileSync } from "node:fs";
import path from "node:path";

/**
 * Remembers where the window was.
 *
 * Stored in `userData` rather than the settings database: this is a property of
 * this machine's window manager, not of the agent, and borgd has no business
 * knowing it. A missing or corrupt file simply falls back to the default size.
 */

export interface WindowState extends Partial<Rectangle> {
  maximized?: boolean;
}

const DEFAULTS = { width: 1240, height: 800 };
/** Enough of the title bar must stay on a display to drag the window back. */
const MIN_VISIBLE = 80;

function statePath(): string {
  return path.join(app.getPath("userData"), "window-state.json");
}

/**
 * Keeps `bounds` reachable on the displays that exist right now.
 *
 * A window restored onto a monitor that has since been unplugged is invisible
 * and unrecoverable without editing the file by hand.
 */
export function clampToDisplays(
  bounds: Rectangle,
  displays: { bounds: Rectangle }[],
): Rectangle | null {
  const visible = displays.some((display) => {
    const d = display.bounds;
    const overlapX = Math.min(bounds.x + bounds.width, d.x + d.width) - Math.max(bounds.x, d.x);
    const overlapY = Math.min(bounds.y + bounds.height, d.y + d.height) - Math.max(bounds.y, d.y);
    return overlapX >= MIN_VISIBLE && overlapY >= MIN_VISIBLE;
  });
  return visible ? bounds : null;
}

export function loadWindowState(): WindowState {
  try {
    const parsed: unknown = JSON.parse(readFileSync(statePath(), "utf8"));
    if (parsed === null || typeof parsed !== "object") return DEFAULTS;
    const raw = parsed as Record<string, unknown>;
    // Whitelist and type-check: these values go straight into the
    // BrowserWindow constructor, and Electron does not forgive a string `x`
    // from a hand-edited file. A wrong-typed field is dropped, not forwarded.
    const state: WindowState = {};
    if (typeof raw.width === "number") state.width = raw.width;
    if (typeof raw.height === "number") state.height = raw.height;
    if (typeof raw.maximized === "boolean") state.maximized = raw.maximized;
    // A position only means something as a pair, and only with the size it
    // was saved with: clamping needs the full rectangle. A partial or
    // off-display rectangle keeps the size and lets the OS place the window.
    if (
      typeof raw.x === "number" &&
      typeof raw.y === "number" &&
      typeof state.width === "number" &&
      typeof state.height === "number" &&
      clampToDisplays(
        { x: raw.x, y: raw.y, width: state.width, height: state.height },
        screen.getAllDisplays(),
      ) !== null
    ) {
      state.x = raw.x;
      state.y = raw.y;
    }
    return { ...DEFAULTS, ...state };
  } catch {
    // No file yet on first run, or someone corrupted it — neither is worth a log.
    return DEFAULTS;
  }
}

/**
 * Saves position and size as the user moves the window.
 *
 * Debounced because `resize` fires on every frame of a drag, and writes are
 * skipped while maximized or full-screen — `getBounds()` would report the
 * screen, which is not where the window should reopen when restored.
 */
export function trackWindowState(win: BrowserWindow): void {
  let timer: ReturnType<typeof setTimeout> | undefined;

  const persist = () => {
    if (win.isDestroyed()) return;
    const state: WindowState = win.isMaximized()
      ? { ...loadWindowState(), maximized: true }
      : { ...win.getNormalBounds(), maximized: false };
    try {
      writeFileSync(statePath(), JSON.stringify(state));
    } catch {
      // A window that cannot remember its size is a papercut, not a failure;
      // there is nowhere useful to surface this to the user.
    }
  };

  const schedule = () => {
    clearTimeout(timer);
    timer = setTimeout(persist, 400);
  };

  win.on("resize", schedule);
  win.on("move", schedule);
  win.on("maximize", schedule);
  win.on("unmaximize", schedule);
  win.once("close", () => {
    clearTimeout(timer);
    persist();
  });
}
