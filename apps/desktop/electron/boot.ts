import { BrowserWindow, ipcMain } from "electron";
import { CHANNELS, type WireBootStatus } from "./channels";
import { BOOT_STEPS, ensureDaemonRunning, type BootDeps } from "./daemon";

/**
 * The boot conversation between the shell and the window.
 *
 * A renderer can attach at any point — it may be created after the daemon is
 * already up, or reloaded halfway through a wait — so the last status is kept
 * and replayed on request instead of only being pushed. Without that, a window
 * that missed the broadcast would sit on an empty overlay forever.
 */
export function registerBoot(deps: BootDeps): {
  /** Runs a boot attempt now. Resolves when it has settled. */
  start(): Promise<boolean>;
  dispose(): void;
} {
  let status: WireBootStatus = BOOT_STEPS.probing;
  let inFlight: Promise<boolean> | null = null;

  const publish = (next: WireBootStatus) => {
    status = next;
    for (const win of BrowserWindow.getAllWindows()) {
      if (!win.isDestroyed() && !win.webContents.isDestroyed()) {
        win.webContents.send(CHANNELS.bootStatus, next);
      }
    }
  };

  const start = (): Promise<boolean> => {
    // A retry arriving mid-attempt joins the attempt already running rather
    // than racing a second daemon into the same socket.
    inFlight ??= ensureDaemonRunning(deps, publish).finally(() => {
      inFlight = null;
    });
    return inFlight;
  };

  ipcMain.handle(CHANNELS.bootRead, () => status);
  ipcMain.handle(CHANNELS.bootRetry, () => start());

  return {
    start,
    dispose() {
      ipcMain.removeHandler(CHANNELS.bootRead);
      ipcMain.removeHandler(CHANNELS.bootRetry);
    },
  };
}
