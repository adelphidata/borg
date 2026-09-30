import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const fs = vi.hoisted(() => ({ readFileSync: vi.fn(), writeFileSync: vi.fn() }));
vi.mock("node:fs", () => fs);
vi.mock("electron", () => import("./test/electron-stub"));

const { displays, makeWindow, resetElectron } = await import("./test/electron-stub");
const { clampToDisplays, loadWindowState, trackWindowState } = await import("./window-state");

const DEFAULTS = { width: 1240, height: 800 };
const MAIN = { workArea: { x: 0, y: 0, width: 1920, height: 1080 } };
/** A laptop panel: the menu bar takes the top 25px, the dock the bottom 70px. */
const LAPTOP = {
  bounds: { x: 0, y: 0, width: 1440, height: 900 },
  workArea: { x: 0, y: 25, width: 1440, height: 805 },
};

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
/** Replace the connected displays with exactly these. */
const connect = (...connected: (typeof LAPTOP)[]) => {
  displays.length = 0;
  displays.push(...connected);
};

describe("clampToDisplays", () => {
  it("leaves a window that already fits unchanged", () => {
    const bounds = { x: 100, y: 100, width: 800, height: 600 };
    expect(clampToDisplays(bounds, [MAIN])).toEqual(bounds);
  });

  it("rejects a window on a monitor that has been unplugged", () => {
    // The real failure: restored off-screen, invisible, and unrecoverable
    // without editing the file by hand.
    const onSecondScreen = { x: 2600, y: 200, width: 800, height: 600 };
    expect(clampToDisplays(onSecondScreen, [MAIN])).toBeNull();
  });

  it("rejects a window with only a sliver on screen", () => {
    const sliver = { x: 1900, y: 100, width: 800, height: 600 };
    expect(clampToDisplays(sliver, [MAIN])).toBeNull();
  });

  it.each([
    // MIN_VISIBLE (80px) is the boundary: exactly that much still counts as
    // the window being on this display, one pixel less does not — on either
    // axis. A window that counts is pulled fully into the work area.
    [
      "200px left at the right edge",
      { x: 1720, y: 100, width: 800, height: 600 },
      { x: 1120, y: 100, width: 800, height: 600 },
    ],
    [
      "exactly 80px left at the right edge",
      { x: 1840, y: 100, width: 800, height: 600 },
      { x: 1120, y: 100, width: 800, height: 600 },
    ],
    [
      "exactly 80px left above the bottom edge",
      { x: 100, y: 1000, width: 800, height: 600 },
      { x: 100, y: 480, width: 800, height: 600 },
    ],
    [
      "80px left below the top edge",
      { x: 100, y: -520, width: 800, height: 600 },
      { x: 100, y: 0, width: 800, height: 600 },
    ],
    ["a 60px sliver above the bottom edge", { x: 100, y: 1020, width: 800, height: 600 }, null],
    ["hanging off the top with 50px visible", { x: 100, y: -550, width: 800, height: 600 }, null],
  ])("applies the 80px rule to a window with %s", (_case, bounds, want) => {
    expect(clampToDisplays(bounds, [MAIN])).toEqual(want);
  });

  it("shrinks a window saved on a large monitor to the laptop's work area", () => {
    const fromBigMonitor = { x: 0, y: 0, width: 2560, height: 1400 };
    expect(clampToDisplays(fromBigMonitor, [LAPTOP])).toEqual(LAPTOP.workArea);
  });

  it("moves a title bar out from under the menu bar", () => {
    const underMenuBar = { x: 100, y: 0, width: 900, height: 700 };
    expect(clampToDisplays(underMenuBar, [LAPTOP])).toEqual({ ...underMenuBar, y: 25 });
  });

  it("measures overlap against the work area, not the whole display", () => {
    // 90px of the window is on the panel, but only 65px of that is below the
    // menu bar: not enough to count.
    const mostlyAbove = { x: 100, y: -610, width: 900, height: 700 };
    expect(clampToDisplays(mostlyAbove, [LAPTOP])).toBeNull();
  });

  it("checks every display, not just the first", () => {
    const second = { workArea: { x: 1920, y: 0, width: 1920, height: 1080 } };
    const onSecond = { x: 2200, y: 100, width: 800, height: 600 };
    expect(clampToDisplays(onSecond, [MAIN, second])).toEqual(onSecond);
  });

  it.each([
    ["the first", { x: 1300, y: 100, width: 800, height: 600 }, 1120],
    ["the second", { x: 1700, y: 100, width: 800, height: 600 }, 1920],
  ])("puts a window that straddles two displays on %s when it shows more", (_c, bounds, x) => {
    const second = { workArea: { x: 1920, y: 0, width: 1920, height: 1080 } };
    expect(clampToDisplays(bounds, [MAIN, second])).toEqual({ ...bounds, x });
    // The order the displays are listed in must not decide it.
    expect(clampToDisplays(bounds, [second, MAIN])).toEqual({ ...bounds, x });
  });

  it("never shrinks below the minimum window size", () => {
    // The layout breaks below 360x600, and the top-left corner is what has to
    // stay reachable when the window cannot fit.
    const tiny = { workArea: { x: 10, y: 30, width: 320, height: 500 } };
    expect(clampToDisplays({ x: 50, y: 60, width: 900, height: 700 }, [tiny])).toEqual({
      x: 10,
      y: 30,
      width: 360,
      height: 600,
    });
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
    // The state lives in userData as text — not next to the binary, not binary.
    expect(fs.readFileSync).toHaveBeenCalledWith("/tmp/borg-userdata/window-state.json", "utf8");
  });

  it.each([
    // These values go straight into the BrowserWindow constructor; a string
    // `x` from a hand-edited file must be dropped, not forwarded.
    // A broken position drops as a pair, and a broken size takes the position
    // with it — clamping needs the full rectangle, so a partial one must not
    // smuggle unclamped coordinates past clampToDisplays.
    ["x", { x: "40", y: 60, width: 900, height: 700 }, { width: 900, height: 700 }],
    ["y", { x: 40, y: null, width: 900, height: 700 }, { width: 900, height: 700 }],
    ["width", { x: 40, y: 60, width: "900", height: 700 }, { width: 1240, height: 700 }],
    ["height", { x: 40, y: 60, width: 900, height: "tall" }, { width: 900, height: 800 }],
    ["maximized", { width: 900, height: 700, maximized: "yes" }, { width: 900, height: 700 }],
  ])("drops a %s of the wrong type instead of handing it to the window", (_field, saved, want) => {
    stored(saved);
    expect(loadWindowState()).toEqual(want);
  });

  it("drops a non-finite size before it reaches the window", () => {
    // JSON has no Infinity literal, but `1e999` parses to one; a hand-edited
    // file must not hand the constructor an unbounded width.
    fs.readFileSync.mockReturnValue('{"x":40,"y":60,"width":1e999,"height":700}');
    expect(loadWindowState()).toEqual({ width: 1240, height: 700 });
  });

  it("never forwards a position that clampToDisplays has not approved", () => {
    // Regression: a wrong-typed width once skipped the clamp entirely and let
    // a far-off-screen x through, reopening the window out of reach.
    stored({ x: 40000, y: 100, width: "900", height: 700 });
    expect(loadWindowState()).toEqual({ width: 1240, height: 700 });
  });

  it("keeps the size but drops the position when the monitor is gone", () => {
    // Letting the OS place it is the recoverable outcome.
    stored({ x: 3000, y: 200, width: 900, height: 700, maximized: true });
    expect(loadWindowState()).toEqual({ width: 900, height: 700, maximized: true });
  });

  it("caps the size to the primary display when the monitor is gone", () => {
    // Saved on a large external monitor, reopened on the laptop alone.
    connect(LAPTOP);
    stored({ x: 3000, y: 200, width: 2400, height: 1300, maximized: false });
    expect(loadWindowState()).toEqual({ width: 1440, height: 805, maximized: false });
  });

  it("fits an oversized window onto the display it is still on", () => {
    connect(LAPTOP);
    stored({ x: 0, y: 0, width: 2560, height: 1400 });
    expect(loadWindowState()).toEqual(LAPTOP.workArea);
  });

  it("caps the default size on a display smaller than the default", () => {
    connect({
      bounds: { x: 0, y: 0, width: 1024, height: 768 },
      workArea: { x: 0, y: 25, width: 1024, height: 743 },
    });
    fs.readFileSync.mockImplementation(() => {
      throw new Error("ENOENT");
    });
    expect(loadWindowState()).toEqual({ width: 1024, height: 743 });
    stored({ width: "wide", maximized: true });
    expect(loadWindowState()).toEqual({ width: 1024, height: 743, maximized: true });
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

  it("persists the restored bounds after an unmaximize", () => {
    // Leaving maximized is the last chance to learn the normal bounds before
    // a quit; without this listener the un-maximized position is lost.
    const win = makeWindow({ bounds: { x: 12, y: 34, width: 780, height: 560 } });
    trackWindowState(win as never);
    win.emit("unmaximize");
    vi.advanceTimersByTime(500);
    expect(written()).toEqual({ x: 12, y: 34, width: 780, height: 560, maximized: false });
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
