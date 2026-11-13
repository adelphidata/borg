import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { MEETING_DETECTED_HASH, MEETING_PANEL_HASH } from "@borg/app/chords";
import type { meetings as pb, StreamHandlers } from "@borg/client";
import { CHANNELS } from "./channels";

vi.mock("electron", () => import("./test/electron-stub"));
// A predictable backoff: the nth consecutive failure waits n+1 seconds.
vi.mock("@borg/app/backoff", () => ({
  retryDelayMs: (failures: number) => (failures + 1) * 1_000,
}));

const stub = await import("./test/electron-stub");
const {
  constructedOptions,
  constructedWindows,
  cursor,
  displays,
  emitApp,
  handlers,
  invoke,
  mediaState,
  resetElectron,
} = stub;
const { DISPLAY_MEDIA_RETRY_MS, flattenDetection, installDisplayMediaHandler, registerMeetings } =
  await import("./meetings");

const INDEX_HTML = "/Apps/Borg.app/Contents/Resources/app/dist/index.html";
const BUNDLE = `file://${INDEX_HTML}`;

/** A daemon whose meetings feed the test drives by hand. */
function fakeClient() {
  const streams: { handlers: StreamHandlers<pb.MeetingEvent>; cancel: ReturnType<typeof vi.fn> }[] =
    [];
  const meetings = {
    subscribeMeetingEvents: vi.fn((_request: unknown, h: StreamHandlers<pb.MeetingEvent>) => {
      const cancel = vi.fn();
      streams.push({ handlers: h, cancel });
      return cancel;
    }),
    // The shell must never start a meeting itself; this is here to prove it.
    startMeeting: vi.fn(),
  };
  return {
    client: { meetings } as never,
    meetings,
    streams,
    /** The newest stream, as the daemon sees it. */
    emit: (event: Partial<pb.MeetingEvent>) => streams.at(-1)!.handlers.onMessage(event as never),
  };
}

let feed: ReturnType<typeof fakeClient>;
let registered: { dispose(): void } | undefined;
const realPlatform = process.platform;
const setPlatform = (value: NodeJS.Platform) =>
  Object.defineProperty(process, "platform", { configurable: true, value });

function register() {
  feed = fakeClient();
  registered = registerMeetings({
    client: feed.client,
    isDev: false,
    indexHtml: INDEX_HTML,
    preload: "/preload.js",
  });
  return registered;
}

/** Lets the microphone prompt (an awaited promise) settle. */
const settle = async () => {
  for (let i = 0; i < 5; i++) await Promise.resolve();
};

const loadedUrl = (win: (typeof constructedWindows)[number]) => win.loadURL.mock.calls[0]?.[0];
const detectedWindow = () =>
  constructedWindows.find((w) => loadedUrl(w) === `${BUNDLE}${MEETING_DETECTED_HASH}`);
const panelFor = (id: string) =>
  constructedWindows.find(
    (w) => loadedUrl(w) === `${BUNDLE}${MEETING_PANEL_HASH}${encodeURIComponent(id)}`,
  );
const optionsOf = (win: (typeof constructedWindows)[number]) =>
  constructedOptions[constructedWindows.indexOf(win)];
const detections = (win: (typeof constructedWindows)[number]) =>
  win.sent.filter((s) => s.channel === CHANNELS.meetingDetection).map((s) => s.args[0]);

const DETECTED: Partial<pb.MeetingEvent> = {
  detected: {
    sourceApp: "Zoom",
    calendarEvent: {
      id: "ev1",
      title: "Weekly sync",
      startUnix: 0,
      endUnix: 0,
      attendees: [],
      joinUrl: "",
      provider: "zoom",
      allDay: false,
    },
  },
};

beforeEach(() => {
  resetElectron();
  setPlatform("linux");
  vi.spyOn(console, "warn").mockImplementation(() => {});
});

afterEach(() => {
  registered?.dispose();
  registered = undefined;
  setPlatform(realPlatform);
  vi.useRealTimers();
  vi.restoreAllMocks();
});

describe("flattenDetection", () => {
  it("keeps the app and names the calendar event when there is one", () => {
    expect(flattenDetection(DETECTED.detected!)).toEqual({
      sourceApp: "Zoom",
      calendarEventId: "ev1",
      calendarTitle: "Weekly sync",
    });
    expect(flattenDetection({ sourceApp: "Chrome" })).toEqual({ sourceApp: "Chrome" });
  });
});

describe("the meetings feed", () => {
  it("subscribes as soon as it is registered", () => {
    register();
    expect(feed.meetings.subscribeMeetingEvents).toHaveBeenCalledTimes(1);
    expect(feed.meetings.subscribeMeetingEvents).toHaveBeenCalledWith({}, expect.anything());
  });

  it("redials after a drop with a growing delay, and a message resets it", () => {
    vi.useFakeTimers();
    register();
    feed.streams[0].handlers.onError!(new Error("unavailable"));
    vi.advanceTimersByTime(999);
    expect(feed.streams).toHaveLength(1);
    vi.advanceTimersByTime(1);
    expect(feed.streams).toHaveLength(2);

    // Still failing: the second wait is longer.
    feed.streams[1].handlers.onError!(new Error("unavailable"));
    vi.advanceTimersByTime(1_999);
    expect(feed.streams).toHaveLength(2);
    vi.advanceTimersByTime(1);
    expect(feed.streams).toHaveLength(3);

    // A message proves the connection; the next drop starts from the bottom.
    feed.emit({ detectionEnded: {} });
    feed.streams[2].handlers.onError!(new Error("unavailable"));
    vi.advanceTimersByTime(1_000);
    expect(feed.streams).toHaveLength(4);
  });

  it("treats a clean end as an outage, since the feed never ends on purpose", () => {
    vi.useFakeTimers();
    register();
    feed.streams[0].handlers.onComplete!();
    vi.advanceTimersByTime(1_000);
    expect(feed.streams).toHaveLength(2);
  });

  it("ignores a stream that already failed once it has been replaced", () => {
    vi.useFakeTimers();
    register();
    const first = feed.streams[0].handlers;
    first.onError!(new Error("unavailable"));
    first.onComplete!(); // a second end from the same dead stream
    vi.advanceTimersByTime(1_000);
    expect(feed.streams).toHaveLength(2);
    first.onMessage(DETECTED as never); // and a late message from it
    expect(constructedWindows).toHaveLength(0);
  });

  it("stops redialling once disposed", () => {
    vi.useFakeTimers();
    register();
    feed.streams[0].handlers.onError!(new Error("unavailable"));
    registered!.dispose();
    registered = undefined;
    vi.advanceTimersByTime(60_000);
    expect(feed.streams).toHaveLength(1);
  });
});

describe("the detected panel", () => {
  it("shows at the top-right of the screen the pointer is on, without taking focus", () => {
    displays.length = 0;
    displays.push(
      { bounds: { x: 0, y: 0, width: 1440, height: 900 } },
      { bounds: { x: 1440, y: 0, width: 2560, height: 1440 } },
    );
    cursor.point = { x: 2000, y: 500 };
    register();
    feed.emit(DETECTED);

    const win = detectedWindow()!;
    expect(win).toBeDefined();
    expect(optionsOf(win)).toMatchObject({
      width: 320,
      height: 128,
      frame: false,
      alwaysOnTop: true,
      transparent: true,
      skipTaskbar: true,
      webPreferences: { contextIsolation: true, nodeIntegration: false, preload: "/preload.js" },
    });
    expect(win.bounds).toEqual({ x: 1440 + 2560 - 320 - 16, y: 16, width: 320, height: 128 });
    expect(win.showInactive).toHaveBeenCalledTimes(1);
    expect(win.focus).not.toHaveBeenCalled();
    expect(win.setVisibleOnAllWorkspaces).toHaveBeenCalledWith(true, { visibleOnFullScreen: true });
    expect(detections(win)).toEqual([
      { sourceApp: "Zoom", calendarEventId: "ev1", calendarTitle: "Weekly sync" },
    ]);
  });

  it("is built once and reused for the next call", () => {
    register();
    feed.emit(DETECTED);
    feed.emit({ detectionEnded: {} });
    feed.emit({ detected: { sourceApp: "Teams" } });
    expect(constructedWindows).toHaveLength(1);
    expect(detections(detectedWindow()!).at(-1)).toEqual({ sourceApp: "Teams" });
  });

  it("is built again after it was closed", () => {
    register();
    feed.emit(DETECTED);
    detectedWindow()!.close();
    feed.emit(DETECTED);
    expect(constructedWindows).toHaveLength(2);
  });

  it("hides when the call ends, and tells its page", () => {
    register();
    feed.emit(DETECTED);
    feed.emit({ detectionEnded: {} });
    const win = detectedWindow()!;
    expect(win.visible).toBe(false);
    expect(detections(win).at(-1)).toBeNull();
  });

  it("replays the current detection to a page that asks, and nothing once it ended", () => {
    register();
    expect(invoke(CHANNELS.meetingDetectionRead, {})).toBeNull();
    feed.emit(DETECTED);
    expect(invoke(CHANNELS.meetingDetectionRead, {})).toEqual({
      sourceApp: "Zoom",
      calendarEventId: "ev1",
      calendarTitle: "Weekly sync",
    });
    feed.emit({ detectionEnded: {} });
    expect(invoke(CHANNELS.meetingDetectionRead, {})).toBeNull();
  });

  it("is hardened like every other window", () => {
    register();
    feed.emit(DETECTED);
    expect(detectedWindow()!.webContents.windowOpenHandler!({ url: "https://x.test" })).toEqual({
      action: "deny",
    });
  });
});

describe("the notes panel", () => {
  it("opens for the meeting the detected panel started, and hides the detected panel", async () => {
    register();
    feed.emit(DETECTED);
    await invoke(CHANNELS.meetingOpenPanel, {}, "m 1");

    expect(detectedWindow()!.visible).toBe(false);
    const panel = panelFor("m 1")!;
    expect(panel).toBeDefined();
    expect(optionsOf(panel)).toMatchObject({
      width: 420,
      height: 520,
      minWidth: 360,
      alwaysOnTop: true,
      resizable: true,
      frame: false,
      webPreferences: {
        preload: "/preload.js",
        contextIsolation: true,
        nodeIntegration: false,
        // It hosts the capture; a covered window must keep full speed.
        backgroundThrottling: false,
      },
    });
    expect(panel.visible).toBe(true);
    expect(panel.webContents.windowOpenHandler!({ url: "https://x.test" })).toEqual({
      action: "deny",
    });
  });

  it("ignores a request with no meeting id", async () => {
    register();
    await invoke(CHANNELS.meetingOpenPanel, {}, "");
    await invoke(CHANNELS.meetingOpenPanel, {}, 42);
    expect(constructedWindows).toHaveLength(0);
  });

  it("opens on its own for a meeting the daemon started, without a detected panel or a StartMeeting", async () => {
    register();
    feed.emit({ started: { id: "m1", autoStarted: true } });
    await settle();
    expect(panelFor("m1")).toBeDefined();
    expect(detectedWindow()).toBeUndefined();
    // Shown without focus: the user is in the call, not in Borg.
    expect(panelFor("m1")!.showInactive).toHaveBeenCalled();
    expect(feed.meetings.startMeeting).not.toHaveBeenCalled();
  });

  it("builds one panel when the click and the daemon's started event both arrive", async () => {
    setPlatform("darwin");
    register();
    // Both land while the microphone prompt is still up.
    const opening = invoke(CHANNELS.meetingOpenPanel, {}, "m1");
    feed.emit({ started: { id: "m1", autoStarted: false } });
    await opening;
    await settle();
    expect(constructedWindows).toHaveLength(1);
  });

  it("does nothing for a started event whose panel is already open", async () => {
    register();
    await invoke(CHANNELS.meetingOpenPanel, {}, "m1");
    const panel = panelFor("m1")!;
    panel.hide();
    feed.emit({ started: { id: "m1", autoStarted: false } });
    await settle();
    expect(constructedWindows).toHaveLength(1);
    expect(panel.visible).toBe(false);
  });

  it("raises an open panel asked for again", async () => {
    register();
    await invoke(CHANNELS.meetingOpenPanel, {}, "m1");
    panelFor("m1")!.hide();
    await invoke(CHANNELS.meetingOpenPanel, {}, "m1");
    expect(constructedWindows).toHaveLength(1);
    expect(panelFor("m1")!.visible).toBe(true);
  });

  it("asks for the microphone first on macOS, and only there", async () => {
    setPlatform("darwin");
    register();
    await invoke(CHANNELS.meetingOpenPanel, {}, "m1");
    expect(mediaState.mediaAccessAsks).toEqual(["microphone"]);

    registered!.dispose();
    resetElectron();
    setPlatform("win32");
    register();
    await invoke(CHANNELS.meetingOpenPanel, {}, "m2");
    expect(mediaState.mediaAccessAsks).toEqual([]);
    expect(panelFor("m2")).toBeDefined();
  });

  it("still opens when the microphone is refused or the prompt fails", async () => {
    // Capture then runs with the system audio alone; the panel says so.
    setPlatform("darwin");
    mediaState.mediaAccess = false;
    register();
    await invoke(CHANNELS.meetingOpenPanel, {}, "m1");
    expect(panelFor("m1")).toBeDefined();

    mediaState.mediaAccess = new Error("TCC unavailable");
    await invoke(CHANNELS.meetingOpenPanel, {}, "m2");
    expect(panelFor("m2")).toBeDefined();
  });

  it("builds nothing when disposed while the microphone prompt was up", async () => {
    setPlatform("darwin");
    register();
    const opening = invoke(CHANNELS.meetingOpenPanel, {}, "m1");
    registered!.dispose();
    registered = undefined;
    await opening;
    expect(constructedWindows).toHaveLength(0);
  });

  it("hides rather than closes while its meeting records, and closes once it stopped", async () => {
    register();
    feed.emit({ started: { id: "m1", autoStarted: false } });
    await settle();
    const panel = panelFor("m1")!;
    panel.show();

    panel.close();
    expect(panel.isDestroyed()).toBe(false);
    expect(panel.visible).toBe(false);

    feed.emit({ stopped: { id: "m1" } });
    panel.close();
    expect(panel.isDestroyed()).toBe(true);
  });

  it("ignores a stop for another meeting", async () => {
    register();
    feed.emit({ started: { id: "m1", autoStarted: false } });
    await settle();
    feed.emit({ stopped: { id: "m0" } });
    panelFor("m1")!.close();
    expect(panelFor("m1")!.isDestroyed()).toBe(false);
  });

  it("lets a recording panel close when the app quits, so it cannot block the quit", async () => {
    register();
    feed.emit({ started: { id: "m1", autoStarted: false } });
    await settle();
    emitApp("before-quit");
    panelFor("m1")!.close();
    expect(panelFor("m1")!.isDestroyed()).toBe(true);
  });

  it("builds a fresh panel for a meeting whose panel was closed", async () => {
    register();
    await invoke(CHANNELS.meetingOpenPanel, {}, "m1");
    panelFor("m1")!.close();
    await invoke(CHANNELS.meetingOpenPanel, {}, "m1");
    expect(constructedWindows).toHaveLength(2);
  });

  it("raises no notification when the summary is written", () => {
    // The sidebar's green dot is the signal; the user declined a toast.
    register();
    feed.emit({ summaryReady: { id: "m1" } });
    expect(stub.notifications).toHaveLength(0);
    expect(constructedWindows).toHaveLength(0);
  });
});

describe("installDisplayMediaHandler", () => {
  const SCREEN = { id: "screen:1", name: "Screen 1" };
  let clock = 0;
  const install = () =>
    installDisplayMediaHandler(stub.session.defaultSession as never, () => clock);
  const request = () =>
    new Promise<unknown>((resolve) => mediaState.displayMediaHandler!({}, resolve));

  beforeEach(() => {
    clock = 100_000;
  });

  it("answers with the system audio alone first, so no screen is ever asked for", async () => {
    mediaState.sources = [SCREEN];
    install();
    expect(mediaState.displayMediaOptions).toEqual({ useSystemPicker: false });
    expect(await request()).toEqual({ audio: "loopback" });
    // Listing screens is what raises the Screen Recording prompt.
    expect(stub.desktopCapturer.getSources).not.toHaveBeenCalled();
  });

  it("adds the first screen when the renderer asks again right after, and keeps doing so", async () => {
    mediaState.sources = [SCREEN, { id: "screen:2", name: "Screen 2" }];
    install();
    await request();
    clock += 200; // the renderer's retry after the audio-only shape was refused
    expect(await request()).toEqual({ video: SCREEN, audio: "loopback" });
    expect(stub.desktopCapturer.getSources).toHaveBeenCalledWith({
      types: ["screen"],
      thumbnailSize: { width: 0, height: 0 },
    });
    // The next meeting, much later, goes straight to the shape that worked.
    clock += 3_600_000;
    expect(await request()).toEqual({ video: SCREEN, audio: "loopback" });
  });

  it("answers audio-only again for a request long after one that worked", async () => {
    install();
    await request();
    clock += DISPLAY_MEDIA_RETRY_MS + 1;
    expect(await request()).toEqual({ audio: "loopback" });
  });

  it("answers nothing when there is no screen, so the renderer falls back to the microphone", async () => {
    mediaState.sources = [];
    install();
    await request();
    expect(await request()).toEqual({});
  });

  it("answers nothing when listing screens fails", async () => {
    mediaState.sources = new Error("denied");
    install();
    await request();
    expect(await request()).toEqual({});
  });

  it("removes itself when uninstalled", () => {
    const uninstall = install();
    uninstall();
    expect(mediaState.displayMediaHandler).toBeNull();
  });
});

describe("dispose", () => {
  it("cancels the feed, drops its handlers, uninstalls display media and destroys its windows", async () => {
    register();
    expect(mediaState.displayMediaHandler).not.toBeNull();
    feed.emit(DETECTED);
    await invoke(CHANNELS.meetingOpenPanel, {}, "m1");

    registered!.dispose();
    registered = undefined;
    expect(feed.streams[0].cancel).toHaveBeenCalledTimes(1);
    expect(handlers.has(CHANNELS.meetingDetectionRead)).toBe(false);
    expect(handlers.has(CHANNELS.meetingOpenPanel)).toBe(false);
    expect(mediaState.displayMediaHandler).toBeNull();
    expect(constructedWindows.every((w) => w.isDestroyed())).toBe(true);
    expect(stub.appState.listeners.get("before-quit") ?? []).toHaveLength(0);
  });
});
