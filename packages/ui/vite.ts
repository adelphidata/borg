import type { Plugin } from "vite";
import STORAGE_KEYS from "./storage-keys.json" with { type: "json" };

/**
 * Injects the pre-paint theme script into every surface's `index.html`.
 *
 * The theme must be on `<html>` before the first paint, or the window flashes
 * light before `ThemeProvider` mounts. Generating it from `STORAGE_KEYS` means
 * the inline script and the provider can never disagree on the key.
 */
export function borgThemePreload(): Plugin {
  const script = [
    "try {",
    `  document.documentElement.dataset.theme =`,
    `    localStorage.getItem(${JSON.stringify(STORAGE_KEYS.theme)}) === "light" ? "light" : "dark";`,
    "} catch {}",
  ].join("\n");

  return {
    name: "borg:theme-preload",
    transformIndexHtml() {
      return [{ tag: "script", children: script, injectTo: "head" }];
    },
  };
}
