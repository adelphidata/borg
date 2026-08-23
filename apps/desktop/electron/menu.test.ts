import { beforeEach, describe, expect, it, vi } from "vitest";
import type { MenuItemConstructorOptions } from "electron";
import { acceleratorFor } from "@borg/app/chords";
import { CHANNELS } from "./channels";

vi.mock("electron", () => import("./test/electron-stub"));

const { makeWindow, menuState, resetElectron, setFocused, shell, windows } =
  await import("./test/electron-stub");
const { installApplicationMenu } = await import("./menu");

beforeEach(() => {
  resetElectron();
});

type Item = MenuItemConstructorOptions;

const template = () => menuState.template as Item[];

function findItem(label: string): Item {
  const walk = (items: Item[]): Item | undefined => {
    for (const item of items) {
      if (item.label === label) return item;
      const submenu = item.submenu;
      if (Array.isArray(submenu)) {
        const found = walk(submenu as Item[]);
        if (found) return found;
      }
    }
    return undefined;
  };
  const found = walk(template());
  if (!found) throw new Error(`no menu item labelled ${label}`);
  return found;
}

describe("installApplicationMenu", () => {
  it("installs a menu", () => {
    installApplicationMenu();
    expect(menuState.applied).not.toBeNull();
  });

  it("gives every top-level menu the app expects", () => {
    installApplicationMenu();
    const labels = template().map((item) => item.label ?? item.role);
    expect(labels).toContain("File");
    expect(labels).toContain("Edit");
    expect(labels).toContain("View");
    expect(labels).toContain("Window");
    expect(labels).toContain("help");
  });

  it("dispatches a keybind action rather than acting itself", () => {
    // ⌘N from the menu and ⌘N from the keyboard must be one implementation.
    installApplicationMenu();
    const win = makeWindow();
    windows.push(win);
    setFocused(win);

    (findItem("New Conversation").click as () => void)();
    expect(win.sent.at(-1)).toEqual({
      channel: CHANNELS.menuCommand,
      args: ["new-conversation"],
    });
  });

  it("takes its accelerators from the same chord table the renderer binds", () => {
    // A menu item's accelerator is consumed before the web contents sees the
    // chord, so a hand-typed one that drifted would leave the new chord dead.
    installApplicationMenu();
    for (const [label, action] of [
      ["New Conversation", "new-conversation"],
      ["Find in Conversation…", "find-in-conversation"],
      ["Find Conversation…", "search-sessions"],
      ["Command Palette…", "command-palette"],
      ["Check for Updates…", "check-for-updates"],
    ] as const) {
      expect(findItem(label).accelerator).toBe(acceleratorFor(action));
    }
  });

  it("reaches every renderer action the menu advertises", () => {
    installApplicationMenu();
    const win = makeWindow();
    windows.push(win);
    setFocused(win);

    for (const [label, action] of [
      ["Find in Conversation…", "find-in-conversation"],
      ["Command Palette…", "command-palette"],
      ["Settings…", "open-settings"],
      ["Check for Updates…", "check-for-updates"],
    ] as const) {
      (findItem(label).click as () => void)();
      expect(win.sent.at(-1)?.args).toEqual([action]);
    }
  });

  it("puts Settings in the File menu where there is no application menu", () => {
    // Windows and Linux have no app menu, so Settings would be unreachable.
    const real = process.platform;
    Object.defineProperty(process, "platform", { configurable: true, value: "win32" });
    try {
      installApplicationMenu();
      const file = template().find((item) => item.label === "File");
      const labels = (file?.submenu as Item[]).map((i) => i.label ?? i.role);
      expect(labels).toContain("Settings…");
      // Closing the last window on Windows should quit, not leave a dock icon.
      expect(labels).toContain("quit");
      expect(template()[0].label).toBe("File");
    } finally {
      Object.defineProperty(process, "platform", { configurable: true, value: real });
    }
  });

  it("keeps Settings in the application menu on macOS", () => {
    const real = process.platform;
    Object.defineProperty(process, "platform", { configurable: true, value: "darwin" });
    try {
      installApplicationMenu();
      const file = template().find((item) => item.label === "File");
      const labels = (file?.submenu as Item[]).map((i) => i.label ?? i.role);
      expect(labels).not.toContain("Settings…");
      expect(labels).toContain("close");
      expect(template()[0].label).toBe("Borg");
    } finally {
      Object.defineProperty(process, "platform", { configurable: true, value: real });
    }
  });

  it("opens the project link in the browser, not in the app window", () => {
    installApplicationMenu();
    (findItem("Borg on GitHub").click as () => void)();
    expect(shell.openExternal).toHaveBeenCalledWith("https://github.com/adelphidata/borg");
  });
});
