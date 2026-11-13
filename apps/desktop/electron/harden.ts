import { shell, type BrowserWindow } from "electron";
import { pathToFileURL } from "node:url";
import { isRecoveryReload } from "./recovery";

/**
 * Navigation hardening for every window that loads the app bundle.
 *
 * The main window, the quick-entry panel and both meeting panels all load the
 * same renderer, so they share one set of rules: no in-app windows, top-level
 * navigation stays on the bundle, and only http(s) is ever handed to the OS.
 */
export interface HardenOptions {
  isDev: boolean;
  devServerUrl?: string;
  indexHtml: string;
  /**
   * The recovery page's Reload link was followed. The link is never a real
   * destination, so it is stopped either way; only the main window shows the
   * recovery page and passes this.
   */
  onRecoveryReload?: () => void;
}

/** Hands a URL to the system browser, but only an http(s) one — file:, smb: and custom schemes stay blocked. */
export function openExternally(url: string): void {
  try {
    const { protocol } = new URL(url);
    if (protocol === "https:" || protocol === "http:") void shell.openExternal(url);
  } catch {
    // unparseable URL → not something to hand to the OS
  }
}

/**
 * True when `url` is the app itself. Exact origin (dev) or exact file href
 * (packaged) — a prefix check would let localhost:51737 or any local
 * file:// path count as internal.
 */
export function isInternalUrl(url: string, options: HardenOptions): boolean {
  try {
    const target = new URL(url);
    target.search = "";
    target.hash = "";
    return options.isDev
      ? target.origin === new URL(options.devServerUrl!).origin
      : target.href === pathToFileURL(options.indexHtml).href;
  } catch {
    return false;
  }
}

export function hardenWebContents(win: BrowserWindow, options: HardenOptions): void {
  // Never spawn in-app windows — external links go to the system browser.
  win.webContents.setWindowOpenHandler(({ url }) => {
    openExternally(url);
    return { action: "deny" };
  });

  // Keep top-level navigation inside the app; send anything else to the browser.
  win.webContents.on("will-navigate", (event, url) => {
    if (isRecoveryReload(url)) {
      event.preventDefault();
      options.onRecoveryReload?.();
      return;
    }
    if (!isInternalUrl(url, options)) {
      event.preventDefault();
      openExternally(url);
    }
  });
}
