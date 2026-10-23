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

function withPlatform(platform: NodeJS.Platform, run: () => void): void {
  const real = process.platform;
  Object.defineProperty(process, "platform", { configurable: true, value: platform });
  try {
    run();
  } finally {
    Object.defineProperty(process, "platform", { configurable: true, value: real });
  }
}

/** The declarative face of a menu item: everything except click handlers. */
type Shape = {
  label?: string;
  role?: string;
  type?: string;
  accelerator?: string;
  submenu?: Shape[];
};

function shapeOf(items: Item[]): Shape[] {
  return items.map((item) => {
    const s: Shape = {};
    if (item.label !== undefined) s.label = item.label;
    if (item.role !== undefined) s.role = item.role;
    if (item.type !== undefined) s.type = item.type;
    if (item.accelerator !== undefined) s.accelerator = item.accelerator as string;
    if (Array.isArray(item.submenu)) s.submenu = shapeOf(item.submenu as Item[]);
    return s;
  });
}

const sep: Shape = { type: "separator" };

/** An action item's shape: its label under the chord table's accelerator. */
function act(label: string, id: string): Shape {
  const accelerator = acceleratorFor(id);
  return accelerator === undefined ? { label } : { label, accelerator };
}

const editMenu = (isMac: boolean): Shape => ({
  label: "Edit",
  submenu: [
    { role: "undo" },
    { role: "redo" },
    sep,
    { role: "cut" },
    { role: "copy" },
    { role: "paste" },
    // Pasting model output into the composer should not carry styling.
    ...(isMac ? [{ role: "pasteAndMatchStyle" }] : []),
    { role: "selectAll" },
    sep,
    // Its key is a setting the renderer binds; the menu row carries none.
    { label: "Start Dictation" },
  ],
});

const viewMenu: Shape = {
  label: "View",
  submenu: [
    act("Find in Conversation…", "find-in-conversation"),
    act("Find Conversation…", "search-sessions"),
    act("Command Palette…", "command-palette"),
    sep,
    { role: "reload" },
    { role: "toggleDevTools" },
    sep,
    { role: "resetZoom" },
    { role: "zoomIn" },
    { role: "zoomOut" },
    sep,
    { role: "togglefullscreen" },
  ],
};

const helpMenu: Shape = {
  role: "help",
  submenu: [{ label: "Borg on GitHub" }, act("Check for Updates…", "check-for-updates")],
};

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

  it("builds the exact macOS menu — the copy is the spec", () => {
    withPlatform("darwin", () => {
      installApplicationMenu();
      expect(shapeOf(template())).toEqual([
        {
          label: "Borg",
          submenu: [
            { role: "about" },
            sep,
            // Settings lives in the application menu, where Mac users look.
            act("Settings…", "open-settings"),
            sep,
            { role: "services" },
            sep,
            { role: "hide" },
            { role: "hideOthers" },
            { role: "unhide" },
            sep,
            { role: "quit" },
          ],
        },
        {
          label: "File",
          submenu: [act("New Conversation", "new-conversation"), sep, { role: "close" }],
        },
        editMenu(true),
        viewMenu,
        {
          label: "Window",
          submenu: [{ role: "minimize" }, { role: "zoom" }, sep, { role: "front" }],
        },
        helpMenu,
      ]);
    });
  });

  it("builds the exact Windows/Linux menu — the copy is the spec", () => {
    withPlatform("win32", () => {
      installApplicationMenu();
      expect(shapeOf(template())).toEqual([
        {
          label: "File",
          submenu: [
            act("New Conversation", "new-conversation"),
            sep,
            // No application menu here, so Settings would be unreachable.
            act("Settings…", "open-settings"),
            // Closing the last window should quit, not leave a stray process.
            { role: "quit" },
          ],
        },
        editMenu(false),
        viewMenu,
        { label: "Window", submenu: [{ role: "minimize" }, { role: "close" }] },
        helpMenu,
      ]);
    });
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

  it("opens the project link in the browser, not in the app window", () => {
    installApplicationMenu();
    (findItem("Borg on GitHub").click as () => void)();
    expect(shell.openExternal).toHaveBeenCalledWith("https://github.com/adelphidata/borg");
  });
});
