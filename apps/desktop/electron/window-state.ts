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
/** The smallest window the layout still works in. */
export const MIN_WIDTH = 360;
export const MIN_HEIGHT = 600;
/** Enough of the title bar must stay on a display to drag the window back. */
const MIN_VISIBLE = 80;

type Size = Pick<Rectangle, "width" | "height">;

function statePath(): string {
  return path.join(app.getPath("userData"), "window-state.json");
}

/**
 * Caps a size to a work area, but never below the minimum window size: on a
 * display smaller than the minimum, Electron would enforce it anyway.
 */
export function fitSize(size: Size, area: Size): Size {
  return {
    width: Math.max(MIN_WIDTH, Math.min(size.width, area.width)),
    height: Math.max(MIN_HEIGHT, Math.min(size.height, area.height)),
  };
}

/**
 * Fits `bounds` onto the displays that exist right now.
 *
 * A window restored onto a monitor that has since been unplugged is invisible
 * and unrecoverable without editing the file by hand, so a rectangle that no
 * display shows enough of is rejected. One that is still reachable moves onto
 * the display that shows most of it, and into that display's work area: a
 * window saved on a large monitor must not open larger than a laptop screen,
 * or with its title bar under the menu bar.
 */
export function clampToDisplays(
  bounds: Rectangle,
  displays: { workArea: Rectangle }[],
): Rectangle | null {
  let best: { area: Rectangle; overlap: number } | null = null;
  for (const { workArea: d } of displays) {
    const overlapX = Math.min(bounds.x + bounds.width, d.x + d.width) - Math.max(bounds.x, d.x);
    const overlapY = Math.min(bounds.y + bounds.height, d.y + d.height) - Math.max(bounds.y, d.y);
    if (overlapX < MIN_VISIBLE || overlapY < MIN_VISIBLE) continue;
    const overlap = overlapX * overlapY;
    if (best === null || overlap > best.overlap) best = { area: d, overlap };
  }
  if (best === null) return null;
  const { area } = best;
  const { width, height } = fitSize(bounds, area);
  // The outer max wins when the minimum size is larger than the work area:
  // the top-left corner, and so the title bar, stays on the display.
  return {
    x: Math.max(area.x, Math.min(bounds.x, area.x + area.width - width)),
    y: Math.max(area.y, Math.min(bounds.y, area.y + area.height - height)),
    width,
    height,
  };
}

export function loadWindowState(): WindowState {
  // The OS places a window that has no position, and it starts on the primary
  // display, so that is the work area its size must fit.
  const unplaced = (size: Partial<Size>): Size =>
    fitSize({ ...DEFAULTS, ...size }, screen.getPrimaryDisplay().workArea);
  try {
    const parsed: unknown = JSON.parse(readFileSync(statePath(), "utf8"));
    if (parsed === null || typeof parsed !== "object") return unplaced({});
    const raw = parsed as Record<string, unknown>;
    // Whitelist and type-check: these values go straight into the
    // BrowserWindow constructor, and Electron does not forgive a string `x`
    // from a hand-edited file. A wrong-typed field is dropped, not forwarded.
    const state: WindowState = {};
    // Finite, not just number: JSON `1e999` parses to Infinity, and NaN or
    // Infinity in the BrowserWindow constructor is as bad as a string.
    const finite = (v: unknown): v is number => Number.isFinite(v);
    if (finite(raw.width)) state.width = raw.width;
    if (finite(raw.height)) state.height = raw.height;
    if (typeof raw.maximized === "boolean") state.maximized = raw.maximized;
    // A position only means something as a pair, and only with the size it
    // was saved with: clamping needs the full rectangle. A partial or
    // off-display rectangle keeps the size and lets the OS place the window.
    const placed =
      finite(raw.x) &&
      finite(raw.y) &&
      typeof state.width === "number" &&
      typeof state.height === "number"
        ? clampToDisplays(
            { x: raw.x, y: raw.y, width: state.width, height: state.height },
            screen.getAllDisplays(),
          )
        : null;
    return { ...state, ...(placed ?? unplaced(state)) };
  } catch {
    // No file yet on first run, or someone corrupted it — neither is worth a log.
    return unplaced({});
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
