import {
  BrowserWindow,
  app,
  desktopCapturer,
  ipcMain,
  screen,
  session,
  systemPreferences,
  type Session,
} from "electron";
import { pathToFileURL } from "node:url";
import { MEETING_DETECTED_HASH, MEETING_PANEL_HASH } from "@borg/app/chords";
import { retryDelayMs } from "@borg/app/backoff";
import type { BorgClient, Unsubscribe, meetings as pb } from "@borg/client";
import { CHANNELS, type WireMeetingDetection } from "./channels";
import { hardenWebContents } from "./harden";

/**
 * The desktop half of Meetings.
 *
 * borgd notices the call and owns the meeting; the shell owns the two small
 * windows around it. One `SubscribeMeetingEvents` stream drives both:
 *
 * - `Detected` shows the "Meeting detected" panel at the top-right of the
 *   screen the user is on, without taking focus from the call.
 * - `MeetingStarted` opens the meeting's notes panel, which is what captures
 *   the audio. That covers both a "Take notes" click and the daemon starting
 *   a calendar meeting on its own, so the shell never starts a meeting itself
 *   and never reads a `meetings.*` setting — the daemon only sends these while
 *   the feature is on.
 *
 * No notification when the summary is written: the green dot on the sidebar's
 * Meetings row is the signal.
 */

export const DETECTED_WIDTH = 320;
export const DETECTED_HEIGHT = 128;
/** Gap between the detected panel and the corner of the work area. */
export const DETECTED_MARGIN = 16;

export const PANEL_WIDTH = 420;
export const PANEL_HEIGHT = 520;
export const PANEL_MIN_WIDTH = 360;
/** Where macOS draws the panel's window buttons: inside its header row. */
export const PANEL_TRAFFIC_LIGHTS = { x: 14, y: 17 };

/**
 * How soon after an audio-only answer a second display-media request counts
 * as the renderer retrying because that answer was refused.
 */
export const DISPLAY_MEDIA_RETRY_MS = 5_000;

/** The detection as the renderer's `MeetingDetection` has it. */
export function flattenDetection(detected: pb.Detected): WireMeetingDetection {
  const event = detected.calendarEvent;
  return event
    ? { sourceApp: detected.sourceApp, calendarEventId: event.id, calendarTitle: event.title }
    : { sourceApp: detected.sourceApp };
}

/**
 * Answers the renderer's `getDisplayMedia` with the system audio.
 *
 * The first answer is audio-only. With no video source there is no
 * `desktopCapturer` call, which is what raises the Screen Recording prompt;
 * on macOS 14.2+ Chromium taps the audio through Core Audio instead and asks
 * only for "System Audio Recording". Where that shape is refused the renderer
 * asks once more, and that request gets a screen as the video source (the
 * renderer drops the video track at once). Once the full shape was needed it
 * is used from then on. No screen at all answers `{}`, so `getDisplayMedia`
 * rejects and the renderer carries on with the microphone alone.
 *
 * Returns the uninstall.
 */
export function installDisplayMediaHandler(
  target: Session,
  now: () => number = Date.now,
): () => void {
  let needsVideo = false;
  let audioOnlyAt = Number.NEGATIVE_INFINITY;
  target.setDisplayMediaRequestHandler(
    (_request, callback) => {
      if (!needsVideo && now() - audioOnlyAt > DISPLAY_MEDIA_RETRY_MS) {
        audioOnlyAt = now();
        callback({ audio: "loopback" });
        return;
      }
      needsVideo = true;
      desktopCapturer
        .getSources({ types: ["screen"], thumbnailSize: { width: 0, height: 0 } })
        .then(
          ([first]) => callback(first ? { video: first, audio: "loopback" } : {}),
          (error: unknown) => {
            console.warn("[borg] no screen to capture system audio through", error);
            callback({});
          },
        );
    },
    { useSystemPicker: false },
  );
  return () => target.setDisplayMediaRequestHandler(null);
}

export interface MeetingsOptions {
  client: Pick<BorgClient, "meetings">;
  isDev: boolean;
  devServerUrl?: string;
  indexHtml: string;
  preload: string;
}

export function registerMeetings(options: MeetingsOptions): { dispose(): void } {
  const urlFor = (hash: string) =>
    options.isDev
      ? `${options.devServerUrl}${hash}`
      : `${pathToFileURL(options.indexHtml).href}${hash}`;
  const webPreferences = {
    preload: options.preload,
    // Same renderer sandbox invariant as the main window.
    contextIsolation: true,
    nodeIntegration: false,
  };

  let disposed = false;
  let quitting = false;
  /** The detection the detected panel is showing, replayed to a fresh page. */
  let current: WireMeetingDetection | null = null;
  /** The meeting the daemon says is recording; its panel hides rather than closes. */
  let recordingId: string | undefined;
  let detectedWin: BrowserWindow | null = null;
  const panels = new Map<string, BrowserWindow>();
  /** Panels waiting on the microphone prompt, so two triggers build one window. */
  const opening = new Set<string>();

  const live = (win: BrowserWindow | null | undefined): win is BrowserWindow =>
    !!win && !win.isDestroyed();

  const buildDetected = () => {
    const created = new BrowserWindow({
      width: DETECTED_WIDTH,
      height: DETECTED_HEIGHT,
      show: false,
      frame: false,
      resizable: false,
      minimizable: false,
      maximizable: false,
      fullscreenable: false,
      skipTaskbar: true,
      alwaysOnTop: true,
      transparent: true,
      backgroundColor: "#00000000",
      webPreferences,
    });
    // The call is often full screen; the panel has to show over it.
    created.setVisibleOnAllWorkspaces(true, { visibleOnFullScreen: true });
    hardenWebContents(created, options);
    created.on("closed", () => {
      if (detectedWin === created) detectedWin = null;
    });
    void created.loadURL(urlFor(MEETING_DETECTED_HASH));
    return created;
  };

  const showDetected = (detection: WireMeetingDetection) => {
    current = detection;
    if (!live(detectedWin)) detectedWin = buildDetected();
    // The screen the user is looking at, not the primary one.
    const area = screen.getDisplayNearestPoint(screen.getCursorScreenPoint()).workArea;
    detectedWin.setBounds({
      x: Math.round(area.x + area.width - DETECTED_WIDTH - DETECTED_MARGIN),
      y: Math.round(area.y + DETECTED_MARGIN),
      width: DETECTED_WIDTH,
      height: DETECTED_HEIGHT,
    });
    // Never steal focus from the call itself.
    detectedWin.showInactive();
    detectedWin.webContents.send(CHANNELS.meetingDetection, detection);
  };

  const hideDetected = () => {
    current = null;
    if (!live(detectedWin)) return;
    detectedWin.hide();
    detectedWin.webContents.send(CHANNELS.meetingDetection, null);
  };

  const showPanel = async (id: string, focus: boolean) => {
    const open = panels.get(id);
    if (live(open)) {
      if (focus) open.show();
      return;
    }
    if (opening.has(id)) return;
    opening.add(id);
    try {
      // Asked before the window exists, so the prompt is about the panel the
      // user is about to see rather than racing its first `getUserMedia`.
      if (process.platform === "darwin") {
        try {
          if (!(await systemPreferences.askForMediaAccess("microphone"))) {
            console.warn("[borg] microphone access refused; meeting notes will lack your side");
          }
        } catch (error) {
          console.warn("[borg] could not ask for microphone access", error);
        }
      }
      if (disposed) return;
      const win = new BrowserWindow({
        width: PANEL_WIDTH,
        height: PANEL_HEIGHT,
        minWidth: PANEL_MIN_WIDTH,
        show: false,
        resizable: true,
        alwaysOnTop: true,
        // A window people can close the usual way: on macOS the traffic
        // lights sit over the panel's own header (only close does anything
        // in a panel), elsewhere the system frame carries the close button.
        ...(process.platform === "darwin"
          ? {
              titleBarStyle: "hidden" as const,
              trafficLightPosition: PANEL_TRAFFIC_LIGHTS,
              minimizable: false,
              maximizable: false,
              fullscreenable: false,
            }
          : {}),
        webPreferences: {
          ...webPreferences,
          // The panel hosts the capture; a hidden or covered window must not
          // have its timers and audio processing slowed down.
          backgroundThrottling: false,
        },
      });
      hardenWebContents(win, options);
      // Closing the panel mid-meeting hides it: the capture lives in it.
      win.on("close", (event: Electron.Event) => {
        if (quitting || recordingId !== id) return;
        event.preventDefault();
        win.hide();
      });
      win.on("closed", () => {
        if (panels.get(id) === win) panels.delete(id);
      });
      panels.set(id, win);
      void win.loadURL(urlFor(`${MEETING_PANEL_HASH}${encodeURIComponent(id)}`));
      if (focus) win.show();
      else win.showInactive();
    } finally {
      opening.delete(id);
    }
  };

  const handle = (event: pb.MeetingEvent) => {
    if (event.detected) showDetected(flattenDetection(event.detected));
    else if (event.detectionEnded) hideDetected();
    else if (event.started) {
      recordingId = event.started.id;
      hideDetected();
      // A click in the detected panel has usually opened this panel already;
      // a daemon auto-start has not, and this is where it opens.
      if (!panels.has(event.started.id)) void showPanel(event.started.id, false);
    } else if (event.stopped) {
      if (recordingId === event.stopped.id) recordingId = undefined;
    }
  };

  // The feed. It redials on its own with full-jitter backoff; a clean end is
  // an outage too, since the stream is unbounded. Only a message proves the
  // connection works, so only a message resets the backoff.
  let cancel: Unsubscribe | undefined;
  let redial: ReturnType<typeof setTimeout> | undefined;
  let failures = 0;
  let generation = 0;
  const watch = () => {
    const mine = ++generation;
    const outage = (error?: Error) => {
      if (mine !== generation || disposed) return;
      generation += 1;
      cancel = undefined;
      if (failures === 0) console.warn("[borg] meetings feed dropped", error?.message ?? "");
      redial = setTimeout(watch, retryDelayMs(failures));
      failures += 1;
    };
    cancel = options.client.meetings.subscribeMeetingEvents(
      {},
      {
        onMessage: (event) => {
          if (mine !== generation) return;
          failures = 0;
          handle(event);
        },
        onError: outage,
        onComplete: () => outage(),
      },
    );
  };

  const onBeforeQuit = () => {
    quitting = true;
  };
  app.on("before-quit", onBeforeQuit);

  ipcMain.handle(CHANNELS.meetingDetectionRead, () => current);
  ipcMain.handle(CHANNELS.meetingOpenPanel, (_event, meetingId: unknown) => {
    if (typeof meetingId !== "string" || meetingId === "") return;
    hideDetected();
    return showPanel(meetingId, true);
  });
  const uninstallDisplayMedia = installDisplayMediaHandler(session.defaultSession);
  watch();

  return {
    dispose() {
      disposed = true;
      generation += 1;
      clearTimeout(redial);
      cancel?.();
      cancel = undefined;
      app.removeListener("before-quit", onBeforeQuit);
      ipcMain.removeHandler(CHANNELS.meetingDetectionRead);
      ipcMain.removeHandler(CHANNELS.meetingOpenPanel);
      uninstallDisplayMedia();
      if (live(detectedWin)) detectedWin.destroy();
      detectedWin = null;
      for (const win of panels.values()) if (live(win)) win.destroy();
      panels.clear();
    },
  };
}
