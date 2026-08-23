import { beforeEach, describe, expect, it, vi } from "vitest";
import { CHANNELS } from "./channels";

vi.mock("electron", () => import("./test/electron-stub"));

const {
  appState,
  handlers,
  invoke,
  makeWindow,
  notifications,
  notificationState,
  resetElectron,
  setFocused,
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

  it("leaves a destroyed window alone", () => {
    const win = makeWindow();
    win.destroy();
    expect(() => raise(win as never)).not.toThrow();
    expect(win.visible).toBe(true);
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
    ]) {
      expect(handlers.has(channel)).toBe(false);
    }
  });
});
