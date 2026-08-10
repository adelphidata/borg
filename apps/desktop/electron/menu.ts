import { Menu, app, shell, type MenuItemConstructorOptions } from "electron";
import { acceleratorFor } from "@borg/app/chords";
import { sendMenuCommand } from "./native";

/**
 * The application menu.
 *
 * Menu items carry no behaviour of their own: each one sends a keybind action
 * id to the renderer, which runs the same handler a typed chord would. That is
 * the whole point — ⌘N from this menu and ⌘N from the keyboard must never be
 * two implementations that drift apart.
 *
 * Accelerators are derived from the same `CHORDS` table the renderer binds, so
 * rebinding a chord moves the menu with it. That matters more here than it
 * looks: when a menu item owns an accelerator, Electron consumes the chord
 * before the web contents ever sees it, so the renderer's listener never fires
 * for these. A hand-typed accelerator that drifted from the table would leave
 * the old chord working and the new one dead.
 */

/** A menu item that dispatches a keybind action, under that action's chord. */
function action(label: string, id: string): MenuItemConstructorOptions {
  return { label, accelerator: acceleratorFor(id), click: () => sendMenuCommand(id) };
}

export function installApplicationMenu(): void {
  const isMac = process.platform === "darwin";

  const appMenu: MenuItemConstructorOptions[] = isMac
    ? [
        {
          label: app.name,
          submenu: [
            { role: "about" },
            { type: "separator" },
            action("Settings…", "open-settings"),
            { type: "separator" },
            { role: "services" },
            { type: "separator" },
            { role: "hide" },
            { role: "hideOthers" },
            { role: "unhide" },
            { type: "separator" },
            { role: "quit" },
          ],
        },
      ]
    : [];

  const template: MenuItemConstructorOptions[] = [
    ...appMenu,
    {
      label: "File",
      submenu: [
        action("New Conversation", "new-conversation"),
        { type: "separator" },
        ...(isMac ? [] : [action("Settings…", "open-settings") as MenuItemConstructorOptions]),
        isMac ? { role: "close" as const } : { role: "quit" as const },
      ],
    },
    {
      label: "Edit",
      submenu: [
        { role: "undo" },
        { role: "redo" },
        { type: "separator" },
        { role: "cut" },
        { role: "copy" },
        { role: "paste" },
        // Pasting model output into the composer should not carry styling.
        ...(isMac ? [{ role: "pasteAndMatchStyle" as const }] : []),
        { role: "selectAll" },
      ],
    },
    {
      label: "View",
      submenu: [
        action("Find Conversation…", "search-sessions"),
        action("Command Palette…", "command-palette"),
        { type: "separator" },
        { role: "reload" },
        { role: "toggleDevTools" },
        { type: "separator" },
        { role: "resetZoom" },
        { role: "zoomIn" },
        { role: "zoomOut" },
        { type: "separator" },
        { role: "togglefullscreen" },
      ],
    },
    {
      label: "Window",
      submenu: isMac
        ? [{ role: "minimize" }, { role: "zoom" }, { type: "separator" }, { role: "front" }]
        : [{ role: "minimize" }, { role: "close" }],
    },
    {
      role: "help",
      submenu: [
        {
          label: "Borg on GitHub",
          click: () => void shell.openExternal("https://github.com/borganization/borg"),
        },
        action("Check for Updates…", "check-for-updates"),
      ],
    },
  ];

  Menu.setApplicationMenu(Menu.buildFromTemplate(template));
}
