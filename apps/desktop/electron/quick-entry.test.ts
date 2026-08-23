import { beforeEach, describe, expect, it, vi } from "vitest";
import { QUICK_ENTRY_HASH } from "@borg/app/chords";

vi.mock("electron", () => import("./test/electron-stub"));

const stub = await import("./test/electron-stub");
const { appState, cursor, displays, resetElectron, shortcutState } = stub;
const { registerQuickEntry } = await import("./quick-entry");

const OPTIONS = {
  accelerator: "CommandOrControl+Shift+Space",
  isDev: false,
  indexHtml: "/Apps/Borg.app/Contents/Resources/app/dist/index.html",
  preload: "/Apps/Borg.app/Contents/Resources/app/dist-electron/preload.js",
};

beforeEach(() => {
  resetElectron();
  vi.spyOn(console, "warn").mockImplementation(() => {});
});

const press = () => shortcutState.registered.get(OPTIONS.accelerator)?.();

describe("registerQuickEntry", () => {
  it("registers the hotkey it was given, with nothing to warn about", () => {
    const panel = registerQuickEntry(OPTIONS);
    expect(shortcutState.registered.has(OPTIONS.accelerator)).toBe(true);
    expect(console.warn).not.toHaveBeenCalled();
    panel.dispose();
  });

  it("reports a hotkey another app already owns instead of crashing", () => {
    shortcutState.available = false;
    expect(() => registerQuickEntry(OPTIONS).dispose()).not.toThrow();
    // The warning names the contested accelerator, or the report is useless.
    expect(console.warn).toHaveBeenCalledWith(expect.stringContaining(OPTIONS.accelerator));
  });

  it("releases the hotkey when the app quits", () => {
    const panel = registerQuickEntry(OPTIONS);
    for (const listener of appState.listeners.get("will-quit") ?? []) listener();
    expect(shortcutState.registered.has(OPTIONS.accelerator)).toBe(false);
    panel.dispose();
  });

  it("builds the panel once and reuses it, so the hotkey never stutters", () => {
    const panel = registerQuickEntry(OPTIONS);
    press();
    press();
    press();
    expect(stub.constructedWindows).toHaveLength(1);
    panel.dispose();
  });

  it("toggles: a second press hides the panel it just showed", () => {
    const panel = registerQuickEntry(OPTIONS);
    press();
    expect(stub.constructedWindows[0].visible).toBe(true);
    press();
    expect(stub.constructedWindows[0].visible).toBe(false);
    panel.dispose();
  });

  it("opens on the display the pointer is on, not the primary one", () => {
    displays.push({ bounds: { x: 1920, y: 0, width: 1280, height: 1024 } });
    cursor.point = { x: 2400, y: 300 };
    const panel = registerQuickEntry(OPTIONS);
    press();
    // The stub reports the display nearest the cursor; the panel must be
    // centred horizontally within that display's work area.
    const bounds = stub.constructedWindows[0].bounds;
    const area = displays[1].bounds;
    expect(bounds.x).toBe(Math.round(area.x + (area.width - bounds.width) / 2));
    panel.dispose();
  });

  it("places the panel a third of the way down the pointer's display", () => {
    displays.length = 0;
    displays.push({ bounds: { x: 100, y: 200, width: 1620, height: 1000 } });
    cursor.point = { x: 500, y: 500 };
    const panel = registerQuickEntry(OPTIONS);
    press();
    // x: 100 + (1620 − 620) / 2 = 600; y: 200 + 1000 × 0.28 = 480.
    expect(stub.constructedWindows[0].bounds).toEqual({ x: 600, y: 480, width: 620, height: 148 });
    panel.dispose();
  });

  it("builds a frameless spotlight-style panel, not another app window", () => {
    // Each flag here is user-visible: a frame, a taskbar entry, or a resize
    // handle would make the capture panel read as a second app window.
    const panel = registerQuickEntry(OPTIONS);
    press();
    expect(stub.constructedOptions[0]).toMatchObject({
      width: 620,
      height: 148,
      show: false,
      frame: false,
      resizable: false,
      minimizable: false,
      maximizable: false,
      skipTaskbar: true,
      alwaysOnTop: true,
      transparent: true,
      backgroundColor: "#00000000",
    });
    panel.dispose();
  });

  it("focuses the panel it shows, so the next keystroke lands in it", () => {
    const panel = registerQuickEntry(OPTIONS);
    press();
    expect(stub.constructedWindows[0].focus).toHaveBeenCalled();
    panel.dispose();
  });

  it("keeps the renderer sandboxed exactly as the main window is", () => {
    const panel = registerQuickEntry(OPTIONS);
    press();
    expect(stub.constructedOptions[0].webPreferences).toMatchObject({
      preload: OPTIONS.preload,
      contextIsolation: true,
      nodeIntegration: false,
    });
    panel.dispose();
  });

  it("loads the packaged bundle at the quick-entry route", () => {
    const panel = registerQuickEntry(OPTIONS);
    press();
    const url = stub.constructedWindows[0].loadURL.mock.calls[0][0] as string;
    expect(url.startsWith("file://")).toBe(true);
    expect(url.endsWith(QUICK_ENTRY_HASH)).toBe(true);
    panel.dispose();
  });

  it("loads the dev server at the same route in development", () => {
    const panel = registerQuickEntry({
      ...OPTIONS,
      isDev: true,
      devServerUrl: "http://localhost:5173",
    });
    press();
    expect(stub.constructedWindows[0].loadURL.mock.calls[0][0]).toBe(
      `http://localhost:5173${QUICK_ENTRY_HASH}`,
    );
    panel.dispose();
  });

  it("dismisses itself when it loses focus, the way Spotlight does", () => {
    const panel = registerQuickEntry(OPTIONS);
    press();
    stub.constructedWindows[0].emit("blur");
    expect(stub.constructedWindows[0].visible).toBe(false);
    panel.dispose();
  });

  it("stays visible over full-screen apps", () => {
    const panel = registerQuickEntry(OPTIONS);
    press();
    expect(stub.constructedWindows[0].setVisibleOnAllWorkspaces).toHaveBeenCalledWith(true, {
      visibleOnFullScreen: true,
    });
    panel.dispose();
  });

  it("gives up the hotkey and the window on dispose", () => {
    const panel = registerQuickEntry(OPTIONS);
    press();
    const window = stub.constructedWindows[0];
    panel.dispose();
    expect(shortcutState.registered.has(OPTIONS.accelerator)).toBe(false);
    expect(window.destroyed).toBe(true);
  });
});
