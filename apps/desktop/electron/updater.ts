import { BrowserWindow, ipcMain } from "electron";
import { autoUpdater } from "electron-updater";
import { CHANNELS, type WireUpdateStatus } from "./channels";

/**
 * Background updates.
 *
 * Checks once shortly after launch and then daily. Nothing is ever forced: the
 * download happens quietly, the renderer shows an unobtrusive pill when a build
 * is ready, and the restart only happens when the user asks for it.
 *
 * Disabled in development, where there is no published feed to check against
 * and `checkForUpdates` would only produce noise in the log.
 */

const DAY_MS = 24 * 60 * 60 * 1000;

/**
 * Where the builds live. Mirrors the `publish` block in electron-builder.yml,
 * which is what the packaged app reads by default; this copy exists only so a
 * token can switch the same feed to its authenticated form.
 */
const FEED = { provider: "github", owner: "adelphidata", repo: "borg" } as const;

/**
 * A GitHub token from the environment, if the user set one.
 *
 * The anonymous feed is a 404 while the repo is private. With a token the
 * updater reads the release through the API instead, which works on a private
 * repo and on a public one. Same two names `borg update` reads.
 */
export function githubToken(env: NodeJS.ProcessEnv = process.env): string | undefined {
  for (const name of ["GH_TOKEN", "GITHUB_TOKEN"]) {
    const value = env[name]?.trim();
    if (value) return value;
  }
  return undefined;
}

function broadcast(status: WireUpdateStatus): void {
  for (const win of BrowserWindow.getAllWindows()) {
    if (!win.isDestroyed()) win.webContents.send(CHANNELS.updateStatus, status);
  }
}

export function registerUpdater(options: { enabled: boolean }): { dispose(): void } {
  let timer: ReturnType<typeof setInterval> | undefined;
  let firstCheck: ReturnType<typeof setTimeout> | undefined;

  // The renderer drives the restart, so never quit out from under a live turn.
  autoUpdater.autoDownload = true;
  autoUpdater.autoInstallOnAppQuit = true;

  const token = githubToken();
  if (token) autoUpdater.setFeedURL({ ...FEED, private: true, token });

  autoUpdater.on("checking-for-update", () => broadcast({ state: "checking" }));
  autoUpdater.on("update-available", (info) =>
    broadcast({ state: "available", version: info.version }),
  );
  autoUpdater.on("update-not-available", () => broadcast({ state: "idle" }));
  autoUpdater.on("download-progress", (progress) =>
    broadcast({ state: "downloading", percent: Math.round(progress.percent) }),
  );
  autoUpdater.on("update-downloaded", (info) =>
    broadcast({ state: "ready", version: info.version }),
  );
  autoUpdater.on("error", (error) =>
    // An unreachable feed is normal (offline, or a private repo); report it to
    // the renderer, which shows nothing unless the user asked for a check.
    broadcast({ state: "error", message: error.message }),
  );

  const check = () => {
    if (!options.enabled) {
      broadcast({ state: "idle" });
      return;
    }
    void autoUpdater.checkForUpdates().catch((error: unknown) => {
      broadcast({
        state: "error",
        message: error instanceof Error ? error.message : String(error),
      });
    });
  };

  ipcMain.handle(CHANNELS.updateCheck, check);
  ipcMain.handle(CHANNELS.updateInstall, () => {
    if (options.enabled) autoUpdater.quitAndInstall();
  });

  if (options.enabled) {
    // Late enough that it never competes with the first paint.
    firstCheck = setTimeout(check, 8_000);
    timer = setInterval(check, DAY_MS);
  }

  return {
    dispose() {
      // The startup check is a timeout, not the interval — leaving it armed
      // lets a torn-down updater fire a check into removed handlers.
      clearTimeout(firstCheck);
      clearInterval(timer);
      autoUpdater.removeAllListeners();
      ipcMain.removeHandler(CHANNELS.updateCheck);
      ipcMain.removeHandler(CHANNELS.updateInstall);
    },
  };
}
