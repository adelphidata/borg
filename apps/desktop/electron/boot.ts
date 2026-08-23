import { BrowserWindow, ipcMain } from "electron";
import { CHANNELS, type WireBootStatus } from "./channels";
import {
  BOOT_STEPS,
  DID_NOT_START,
  bootFailure,
  ensureDaemonRunning,
  type BootDeps,
} from "./daemon";

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
    // A throw anywhere in the attempt must still end in a status the renderer
    // can act on: an unreported failure leaves the boot screen spinning on a
    // step that will never finish, with no retry and no way past it.
    inFlight ??= ensureDaemonRunning(deps, publish)
      .catch((error: unknown) => {
        console.error("[borg] boot attempt failed:", error);
        publish(bootFailure(DID_NOT_START));
        return false;
      })
      .finally(() => {
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
