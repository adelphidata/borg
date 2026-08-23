import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const fs = vi.hoisted(() => ({ readFileSync: vi.fn(), writeFileSync: vi.fn() }));
vi.mock("node:fs", () => fs);
vi.mock("electron", () => import("./test/electron-stub"));

const { displays, makeWindow, resetElectron } = await import("./test/electron-stub");
const { clampToDisplays, loadWindowState, trackWindowState } = await import("./window-state");

const DEFAULTS = { width: 1240, height: 800 };
const MAIN = { bounds: { x: 0, y: 0, width: 1920, height: 1080 } };

beforeEach(() => {
  resetElectron();
  fs.readFileSync.mockReset();
  fs.writeFileSync.mockReset();
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
});

const stored = (state: unknown) => fs.readFileSync.mockReturnValue(JSON.stringify(state));
const written = () => JSON.parse(fs.writeFileSync.mock.calls.at(-1)?.[1] as string);

describe("clampToDisplays", () => {
  it("keeps a window that sits on a display", () => {
    const bounds = { x: 100, y: 100, width: 800, height: 600 };
    expect(clampToDisplays(bounds, [MAIN])).toEqual(bounds);
  });

  it("rejects a window on a monitor that has been unplugged", () => {
    // The real failure: restored off-screen, invisible, and unrecoverable
    // without editing the file by hand.
    const onSecondScreen = { x: 2600, y: 200, width: 800, height: 600 };
    expect(clampToDisplays(onSecondScreen, [MAIN])).toBeNull();
  });

  it("accepts a window that still overlaps enough to be dragged back", () => {
    // 200px of the window is on screen — enough title bar to grab.
    const mostlyOff = { x: 1720, y: 100, width: 800, height: 600 };
    expect(clampToDisplays(mostlyOff, [MAIN])).toEqual(mostlyOff);
  });

  it("rejects a window with only a sliver on screen", () => {
    const sliver = { x: 1900, y: 100, width: 800, height: 600 };
    expect(clampToDisplays(sliver, [MAIN])).toBeNull();
  });

  it("checks every display, not just the first", () => {
    displays.push({ bounds: { x: 1920, y: 0, width: 1920, height: 1080 } });
    const onSecond = { x: 2200, y: 100, width: 800, height: 600 };
    expect(clampToDisplays(onSecond, displays)).toEqual(onSecond);
  });
});

describe("loadWindowState", () => {
  it("falls back to the default size on first run", () => {
    fs.readFileSync.mockImplementation(() => {
      throw new Error("ENOENT");
    });
    expect(loadWindowState()).toEqual(DEFAULTS);
  });

  it("falls back to the default size for a corrupt file", () => {
    fs.readFileSync.mockReturnValue("{not json");
    expect(loadWindowState()).toEqual(DEFAULTS);
  });

  it("falls back for a file holding something that is not an object", () => {
    stored("just a string");
    expect(loadWindowState()).toEqual(DEFAULTS);
    fs.readFileSync.mockReturnValue("null");
    expect(loadWindowState()).toEqual(DEFAULTS);
  });

  it("restores a position that is still on a display", () => {
    stored({ x: 40, y: 60, width: 900, height: 700, maximized: false });
    expect(loadWindowState()).toEqual({ x: 40, y: 60, width: 900, height: 700, maximized: false });
  });

  it("keeps the size but drops the position when the monitor is gone", () => {
    // Letting the OS place it is the recoverable outcome.
    stored({ x: 3000, y: 200, width: 900, height: 700, maximized: true });
    expect(loadWindowState()).toEqual({ width: 900, height: 700, maximized: true });
  });

  it("fills in missing fields from the defaults", () => {
    stored({ maximized: true });
    expect(loadWindowState()).toEqual({ ...DEFAULTS, maximized: true });
  });
});

describe("trackWindowState", () => {
  it("writes once for a burst of resize events", () => {
    // `resize` fires on every frame of a drag; a write per frame would hammer
    // the disk for the length of the gesture.
    const win = makeWindow({ bounds: { x: 10, y: 20, width: 800, height: 600 } });
    trackWindowState(win as never);
    for (let i = 0; i < 20; i++) win.emit("resize");
    vi.advanceTimersByTime(500);
    expect(fs.writeFileSync).toHaveBeenCalledTimes(1);
    expect(written()).toEqual({ x: 10, y: 20, width: 800, height: 600, maximized: false });
  });

  it("persists a move as well as a resize", () => {
    const win = makeWindow({ bounds: { x: 300, y: 120, width: 700, height: 500 } });
    trackWindowState(win as never);
    win.emit("move");
    vi.advanceTimersByTime(500);
    expect(written()).toMatchObject({ x: 300, y: 120 });
  });

  it("keeps the restored size when the window is maximized", () => {
    // getBounds() would report the whole screen, which is not where the window
    // should reopen once restored.
    stored({ x: 40, y: 60, width: 900, height: 700 });
    const win = makeWindow({ maximized: true, bounds: { x: 0, y: 0, width: 1920, height: 1080 } });
    trackWindowState(win as never);
    win.emit("maximize");
    vi.advanceTimersByTime(500);
    expect(written()).toEqual({ x: 40, y: 60, width: 900, height: 700, maximized: true });
  });

  it("writes immediately on close, without waiting for the debounce", () => {
    const win = makeWindow({ bounds: { x: 5, y: 5, width: 640, height: 480 } });
    trackWindowState(win as never);
    win.emit("resize");
    win.emit("close");
    expect(fs.writeFileSync).toHaveBeenCalledTimes(1);
    // The pending debounce must not fire a second write after close.
    vi.advanceTimersByTime(500);
    expect(fs.writeFileSync).toHaveBeenCalledTimes(1);
  });

  it("does not read a destroyed window's bounds", () => {
    const win = makeWindow();
    trackWindowState(win as never);
    win.destroy();
    win.emit("close");
    expect(fs.writeFileSync).not.toHaveBeenCalled();
  });

  it("survives an unwritable state file", () => {
    // Forgetting the window size is a papercut; crashing the close handler is not.
    fs.writeFileSync.mockImplementation(() => {
      throw new Error("EACCES");
    });
    const win = makeWindow();
    trackWindowState(win as never);
    expect(() => win.emit("close")).not.toThrow();
  });
});
