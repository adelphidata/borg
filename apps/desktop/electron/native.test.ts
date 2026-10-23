import { beforeEach, describe, expect, it, vi } from "vitest";
import { CHANNELS } from "./channels";

vi.mock("electron", () => import("./test/electron-stub"));

const {
  app,
  appState,
  emitApp,
  handlers,
  invoke,
  makeWindow,
  notifications,
  notificationState,
  resetElectron,
  setFocused,
  shortcutState,
  windows,
} = await import("./test/electron-stub");
const { activateSession, raise, registerNativeIpc, sendMenuCommand } = await import("./native");

beforeEach(() => {
  resetElectron();
});

const senderOf = (win: ReturnType<typeof makeWindow>) => win.webContents;

describe("sendMenuCommand", () => {
  it("sends to the focused window", () => {
    const background = makeWindow();
    const front = makeWindow();
    windows.push(background, front);
    setFocused(front);

    sendMenuCommand("new-conversation");
    expect(front.sent).toEqual([{ channel: CHANNELS.menuCommand, args: ["new-conversation"] }]);
    expect(background.sent).toEqual([]);
  });

  it("falls back to the first window when nothing has focus", () => {
    // The menu is reachable on macOS with every window in the background.
    const only = makeWindow();
    windows.push(only);
    setFocused(null);

    sendMenuCommand("open-settings");
    expect(only.sent.at(-1)?.args).toEqual(["open-settings"]);
  });

  it("does nothing when there is no window at all", () => {
    expect(() => sendMenuCommand("open-settings")).not.toThrow();
  });

  it("does not send to a destroyed window", () => {
    const win = makeWindow();
    windows.push(win);
    setFocused(win);
    win.destroy();

    sendMenuCommand("open-settings");
    expect(win.sent).toEqual([]);
  });
});

describe("raise", () => {
  it("brings a minimised window all the way back", () => {
    const win = makeWindow({ minimized: true, visible: false });
    raise(win as never);
    expect(win.minimized).toBe(false);
    expect(win.visible).toBe(true);
  });

  it("leaves a destroyed window entirely alone", () => {
    const win = makeWindow({ visible: false, minimized: true });
    win.destroy();
    expect(() => raise(win as never)).not.toThrow();
    expect(win.visible).toBe(false);
    expect(win.restore).not.toHaveBeenCalled();
    expect(win.focus).not.toHaveBeenCalled();
  });

  it("focuses without restoring a window that is not minimized", () => {
    // Restoring a normal window is a visible flicker on some platforms.
    const win = makeWindow();
    raise(win as never);
    expect(win.restore).not.toHaveBeenCalled();
    expect(win.focus).toHaveBeenCalled();
  });
});

describe("activateSession", () => {
  it("raises the window and names the session to open", () => {
    const win = makeWindow({ minimized: true, visible: false });
    activateSession(win as never, "sess-9");
    expect(win.visible).toBe(true);
    expect(win.sent).toEqual([{ channel: CHANNELS.activateSession, args: ["sess-9"] }]);
  });

  it("says nothing to a destroyed window", () => {
    const win = makeWindow();
    win.destroy();
    activateSession(win as never, "sess-9");
    expect(win.sent).toEqual([]);
  });
});

describe("registerNativeIpc", () => {
  it("shows a notification with the title and body it was given", () => {
    const win = makeWindow();
    windows.push(win);
    registerNativeIpc(() => win as never);

    invoke(CHANNELS.notify, { sender: senderOf(win) }, { title: "Done", body: "Turn finished" });
    expect(notifications).toHaveLength(1);
    expect(notifications[0].options).toEqual({ title: "Done", body: "Turn finished" });
    expect(notifications[0].shown).toBe(true);
  });

  it("does nothing where the OS has no notifications", () => {
    notificationState.supported = false;
    const win = makeWindow();
    windows.push(win);
    registerNativeIpc(() => win as never);

    invoke(CHANNELS.notify, { sender: senderOf(win) }, { title: "Done", body: "" });
    expect(notifications).toHaveLength(0);
  });

  it("replaces a keyed notification instead of stacking a second one", () => {
    // Two "turn finished" toasts for the same conversation is a pile-up, not
    // twice the news.
    const win = makeWindow();
    windows.push(win);
    registerNativeIpc(() => win as never);

    invoke(CHANNELS.notify, { sender: senderOf(win) }, { title: "One", body: "", key: "s1" });
    invoke(CHANNELS.notify, { sender: senderOf(win) }, { title: "Two", body: "", key: "s1" });
    expect(notifications[0].closed).toBe(true);
    expect(notifications[1].closed).toBe(false);
  });

  it("keeps unkeyed notifications independent of each other", () => {
    const win = makeWindow();
    windows.push(win);
    registerNativeIpc(() => win as never);

    invoke(CHANNELS.notify, { sender: senderOf(win) }, { title: "One", body: "" });
    invoke(CHANNELS.notify, { sender: senderOf(win) }, { title: "Two", body: "" });
    expect(notifications.map((n) => n.closed)).toEqual([false, false]);
  });

  it("opens the conversation a clicked notification is about", () => {
    const win = makeWindow({ minimized: true });
    windows.push(win);
    registerNativeIpc(() => win as never);

    invoke(
      CHANNELS.notify,
      { sender: senderOf(win) },
      { title: "Reply", body: "", sessionId: "sess-3" },
    );
    notifications[0].emit("click");
    expect(win.sent.at(-1)).toEqual({ channel: CHANNELS.activateSession, args: ["sess-3"] });
    expect(win.minimized).toBe(false);
  });

  it("just raises the window for a notification about nothing in particular", () => {
    const win = makeWindow({ minimized: true });
    windows.push(win);
    registerNativeIpc(() => win as never);

    invoke(CHANNELS.notify, { sender: senderOf(win) }, { title: "Hi", body: "" });
    notifications[0].emit("click");
    expect(win.minimized).toBe(false);
    expect(win.sent).toEqual([]);
  });

  it("ignores a click after the window that sent it is gone", () => {
    const win = makeWindow();
    windows.push(win);
    registerNativeIpc(() => win as never);

    invoke(
      CHANNELS.notify,
      { sender: senderOf(win) },
      { title: "Reply", body: "", sessionId: "sess-3" },
    );
    windows.length = 0; // the window closed before the user clicked the toast
    expect(() => notifications[0].emit("click")).not.toThrow();
  });

  it("does not track an unkeyed notification, so dispose leaves it alone", () => {
    // Only keyed notifications are replaceable; tracking unkeyed ones would
    // make dispose retract toasts that should outlive the app.
    const win = makeWindow();
    windows.push(win);
    const native = registerNativeIpc(() => win as never);

    invoke(CHANNELS.notify, { sender: senderOf(win) }, { title: "One", body: "" });
    native.dispose();
    expect(notifications[0].closed).toBe(false);
  });

  it("keeps the replacement live when the replaced toast's close arrives late", () => {
    const win = makeWindow();
    windows.push(win);
    const native = registerNativeIpc(() => win as never);

    invoke(CHANNELS.notify, { sender: senderOf(win) }, { title: "One", body: "", key: "s1" });
    invoke(CHANNELS.notify, { sender: senderOf(win) }, { title: "Two", body: "", key: "s1" });
    notifications[0].emit("close"); // the OS reports the replaced toast late
    native.dispose();
    // The replacement was still tracked, so dispose closes it.
    expect(notifications[1].closed).toBe(true);
  });

  it("forgets a notification once it closes, so a later key reuse is clean", () => {
    const win = makeWindow();
    windows.push(win);
    const native = registerNativeIpc(() => win as never);

    invoke(CHANNELS.notify, { sender: senderOf(win) }, { title: "One", body: "", key: "s1" });
    notifications[0].emit("close");
    native.dispose();
    // Dispose closes what is still live; the closed one must not be closed again.
    expect(notifications[0].closed).toBe(false);
  });

  it("reveals a session in the main window, not in the panel that asked", () => {
    // Quick entry starts a turn and gets out of the way.
    const main = makeWindow();
    const panel = makeWindow();
    windows.push(main, panel);
    registerNativeIpc(() => main as never);

    invoke(CHANNELS.revealSession, { sender: senderOf(panel) }, "sess-7");
    expect(main.sent.at(-1)?.args).toEqual(["sess-7"]);
    expect(panel.sent).toEqual([]);
  });

  it("ignores a reveal when there is no main window", () => {
    registerNativeIpc(() => null);
    expect(() => invoke(CHANNELS.revealSession, {}, "sess-7")).not.toThrow();
  });

  it("hides the window that asked to be hidden", () => {
    const main = makeWindow();
    const panel = makeWindow();
    windows.push(main, panel);
    registerNativeIpc(() => main as never);

    invoke(CHANNELS.hideWindow, { sender: senderOf(panel) });
    expect(panel.visible).toBe(false);
    expect(main.visible).toBe(true);
  });

  it("ignores a hide request from a window that no longer exists", () => {
    registerNativeIpc(() => null);
    expect(() => invoke(CHANNELS.hideWindow, { sender: {} })).not.toThrow();
  });

  it("skips the badge where the platform offers none", () => {
    // Some Linux desktops have no badge; Electron then has no setBadgeCount.
    const win = makeWindow();
    windows.push(win);
    registerNativeIpc(() => win as never);

    const real = app.setBadgeCount;
    (app as { setBadgeCount: unknown }).setBadgeCount = undefined;
    try {
      expect(() => invoke(CHANNELS.setBadge, {}, 3)).not.toThrow();
    } finally {
      app.setBadgeCount = real;
    }
  });

  it("sets the dock badge, and never to a negative count", () => {
    const win = makeWindow();
    windows.push(win);
    registerNativeIpc(() => win as never);

    invoke(CHANNELS.setBadge, {}, 4);
    expect(appState.badge).toBe(4);
    invoke(CHANNELS.setBadge, {}, -2);
    expect(appState.badge).toBe(0);
  });

  it("closes live notifications and removes its handlers on dispose", () => {
    const win = makeWindow();
    windows.push(win);
    const native = registerNativeIpc(() => win as never);
    invoke(CHANNELS.notify, { sender: senderOf(win) }, { title: "One", body: "", key: "s1" });

    native.dispose();
    expect(notifications[0].closed).toBe(true);
    for (const channel of [
      CHANNELS.notify,
      CHANNELS.revealSession,
      CHANNELS.hideWindow,
      CHANNELS.setBadge,
      CHANNELS.dictationHotkey,
    ]) {
      expect(handlers.has(channel)).toBe(false);
    }
  });

  describe("the dictation hotkey", () => {
    it("registers the accelerator, and pressing it raises the window and dispatches dictate", () => {
      const win = makeWindow();
      windows.push(win);
      win.minimized = true;
      registerNativeIpc(() => win as never);

      invoke(CHANNELS.dictationHotkey, {}, "CmdOrCtrl+Shift+Space");
      shortcutState.registered.get("CmdOrCtrl+Shift+Space")?.();
      expect(win.minimized).toBe(false);
      expect(win.sent.at(-1)).toEqual({ channel: CHANNELS.menuCommand, args: ["dictate"] });
    });

    it("swaps a changed key and clears on empty, so only the current setting is live", () => {
      const win = makeWindow();
      windows.push(win);
      registerNativeIpc(() => win as never);

      invoke(CHANNELS.dictationHotkey, {}, "Alt+D");
      invoke(CHANNELS.dictationHotkey, {}, "Alt+V");
      expect([...shortcutState.registered.keys()]).toEqual(["Alt+V"]);
      invoke(CHANNELS.dictationHotkey, {}, "");
      expect(shortcutState.registered.size).toBe(0);
    });

    it("ignores a repeat of the same key rather than re-registering it", () => {
      const win = makeWindow();
      windows.push(win);
      registerNativeIpc(() => win as never);
      invoke(CHANNELS.dictationHotkey, {}, "Alt+D");
      invoke(CHANNELS.dictationHotkey, {}, "Alt+D");
      expect(shortcutState.registered.size).toBe(1);
    });

    it("does not keep a key another app already owns, and says so", () => {
      const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
      shortcutState.available = false;
      const win = makeWindow();
      windows.push(win);
      const native = registerNativeIpc(() => win as never);
      invoke(CHANNELS.dictationHotkey, {}, "Alt+D");
      expect(warn).toHaveBeenCalledWith(expect.stringContaining("Alt+D"));
      // Nothing was held, so dispose has nothing to release.
      native.dispose();
      expect(shortcutState.registered.size).toBe(0);
      warn.mockRestore();
    });

    it("lets the key go while the main window has focus, so the page sees the press", () => {
      const win = makeWindow();
      windows.push(win);
      registerNativeIpc(() => win as never);
      invoke(CHANNELS.dictationHotkey, {}, "Alt+D");
      expect(shortcutState.registered.has("Alt+D")).toBe(true);

      emitApp("browser-window-focus", {}, win);
      expect(shortcutState.registered.size).toBe(0);
      emitApp("browser-window-blur", {}, win);
      expect(shortcutState.registered.has("Alt+D")).toBe(true);
    });

    it("keeps the key while another window, such as quick entry, has focus", () => {
      const win = makeWindow();
      const panel = makeWindow();
      windows.push(win, panel);
      registerNativeIpc(() => win as never);
      invoke(CHANNELS.dictationHotkey, {}, "Alt+D");
      emitApp("browser-window-focus", {}, panel);
      expect(shortcutState.registered.has("Alt+D")).toBe(true);
    });

    it("set while the main window has focus, waits for focus to leave", () => {
      const win = makeWindow();
      windows.push(win);
      setFocused(win);
      registerNativeIpc(() => win as never);
      invoke(CHANNELS.dictationHotkey, {}, "Alt+D");
      expect(shortcutState.registered.size).toBe(0);
      setFocused(null);
      emitApp("browser-window-blur", {}, win);
      expect(shortcutState.registered.has("Alt+D")).toBe(true);
    });

    it("cleared while focused stays cleared when focus leaves, and dispose stops listening", () => {
      const win = makeWindow();
      windows.push(win);
      const native = registerNativeIpc(() => win as never);
      invoke(CHANNELS.dictationHotkey, {}, "Alt+D");
      emitApp("browser-window-focus", {}, win);
      invoke(CHANNELS.dictationHotkey, {}, "");
      emitApp("browser-window-blur", {}, win);
      expect(shortcutState.registered.size).toBe(0);

      invoke(CHANNELS.dictationHotkey, {}, "Alt+V");
      native.dispose();
      emitApp("browser-window-blur", {}, win);
      expect(shortcutState.registered.size).toBe(0);
    });

    it("warns once about a taken key, not on every blur", () => {
      const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
      shortcutState.available = false;
      const win = makeWindow();
      windows.push(win);
      registerNativeIpc(() => win as never);
      invoke(CHANNELS.dictationHotkey, {}, "Alt+D");
      emitApp("browser-window-focus", {}, win);
      emitApp("browser-window-blur", {}, win);
      expect(warn).toHaveBeenCalledTimes(1);
      warn.mockRestore();
    });

    it("releases the key on dispose and when the window is gone does nothing on press", () => {
      const win = makeWindow();
      windows.push(win);
      const native = registerNativeIpc(() => win as never);
      invoke(CHANNELS.dictationHotkey, {}, "Alt+D");
      win.destroy();
      expect(() => shortcutState.registered.get("Alt+D")?.()).not.toThrow();
      expect(win.sent).toEqual([]);
      native.dispose();
      expect(shortcutState.registered.size).toBe(0);
    });
  });

  it("clears its ledger on dispose, so a fresh registration starts clean", () => {
    // Without the clear, a re-registered bridge would find the old disposed
    // toast under the same key and close it a second time.
    const win = makeWindow();
    windows.push(win);
    const first = registerNativeIpc(() => win as never);
    invoke(CHANNELS.notify, { sender: senderOf(win) }, { title: "One", body: "", key: "s1" });
    first.dispose();

    const second = registerNativeIpc(() => win as never);
    invoke(CHANNELS.notify, { sender: senderOf(win) }, { title: "Two", body: "", key: "s1" });
    expect(notifications[0].closeCount).toBe(1);
    second.dispose();
  });
});
