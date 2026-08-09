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
  // Mirrors `resolveTheme` in src/lib/theme.tsx: an explicit choice wins,
  // "system" asks the OS, and anything else (including no value at all) is
  // dark. Resolving "system" here too is what keeps a system-light user from
  // seeing a dark frame before the provider mounts.
  const script = [
    "try {",
    `  var p = localStorage.getItem(${JSON.stringify(STORAGE_KEYS.theme)});`,
    `  var dark = p === "system"`,
    `    ? window.matchMedia("(prefers-color-scheme: dark)").matches`,
    `    : p !== "light";`,
    `  document.documentElement.dataset.theme = dark ? "dark" : "light";`,
    "} catch {}",
  ].join("\n");

  return {
    name: "borg:theme-preload",
    transformIndexHtml() {
      return [{ tag: "script", children: script, injectTo: "head" }];
    },
  };
}
