/**
 * Extractors: the page state every property and action generator reads.
 *
 * Each `extract` callback runs inside the browser on every captured state and
 * must return JSON. Everything below is derived from the DOM hooks the shared
 * app actually renders (roles, aria-labels, the `#/` hash router), so a
 * selector here is a contract with `packages/app` and `packages/ui`.
 */
import { extract } from "@antithesishq/bombadil/browser";

/** The viewport width at and below which the app swaps the sidebar for the tab bar. */
export const MOBILE_MAX_WIDTH = 700;

function visible(el: Element): boolean {
  const rect = el.getBoundingClientRect();
  return rect.width > 0 && rect.height > 0;
}

function enabled(el: Element): boolean {
  return !(el as HTMLButtonElement).disabled && el.getAttribute("aria-disabled") !== "true";
}

/**
 * Drops lone surrogates. Extracted values travel as JSON over the debugger
 * protocol, and a string cut in the middle of an emoji is not valid there.
 */
function clean(text: string): string {
  return text.replace(
    /[\ud800-\udbff](?![\udc00-\udfff])|(?<![\ud800-\udbff])[\udc00-\udfff]/g,
    "",
  );
}

/** Text that only ever reaches the screen when a value escaped unformatted. */
const RAW_VALUE = /\[object Object\]|\bNaN\b|\bundefined\b/;

export const page = extract((state) => {
  const d = state.document;
  const w = state.window;
  const text = d.body?.innerText ?? "";
  const actionable = Array.from(
    d.querySelectorAll(
      "button, a[href], input, textarea, select, [role=button], [role=switch], [role=menuitem], [role=option]",
    ),
  ).filter((el) => visible(el) && enabled(el)).length;

  const dialogs = Array.from(d.querySelectorAll("[role=dialog]")).filter(visible);
  const menus = Array.from(d.querySelectorAll("[role=menu]")).filter(visible);
  const active = d.activeElement;

  let storedTheme: string | null = null;
  try {
    storedTheme = w.localStorage.getItem("borg.theme");
  } catch {
    // storage blocked: the app falls back to dark, and so does the property
  }

  // The chat box only; other pages have textareas of their own.
  const composer = d.querySelector<HTMLTextAreaElement>(
    'main textarea[placeholder^="Ask Borg"], main textarea[placeholder^="Steer Borg"], main textarea[placeholder^="Send after this"]',
  );

  return {
    hash: w.location.hash,
    width: w.innerWidth,
    height: w.innerHeight,
    scrollWidth: d.documentElement.scrollWidth,
    actionable,
    crashed: text.includes("This page ran into a problem."),
    loading: /\bLoading [^\n]*…/.test(text) || text.includes("Starting Borg…"),
    lostConnection:
      text.includes("Lost the connection to Borg") || text.includes("Still cannot reach Borg"),
    rawValueOnScreen: RAW_VALUE.test(text),
    tokenGate: d.querySelector('input[aria-label="Client token"]') !== null,
    onboarding: d.querySelector('input[aria-label="Agent name"]') !== null,
    dialogs: dialogs.length,
    menus: menus.length,
    toasts: Array.from(d.querySelectorAll("[role=status][aria-live=polite]")).filter(visible)
      .length,
    h1s: Array.from(d.querySelectorAll("h1")).filter(visible).length,
    sidebar:
      d.querySelector(
        'aside button[aria-label^="Collapse sidebar"], aside button[aria-label^="Expand sidebar"]',
      ) !== null,
    tabs: d.querySelectorAll("nav a[href]").length,
    activeSidebarLinks: Array.from(d.querySelectorAll('aside a[aria-current="page"]')).map(
      (a) => a.getAttribute("href") ?? "",
    ),
    activeTabLinks: Array.from(d.querySelectorAll('nav a[aria-current="page"]')).map(
      (a) => a.getAttribute("href") ?? "",
    ),
    theme: d.documentElement.dataset.theme ?? null,
    storedTheme,
    prefersDark: w.matchMedia("(prefers-color-scheme: dark)").matches,
    // Focus belongs inside an open dialog; `null` when there is none.
    focusInDialog:
      dialogs.length === 0
        ? null
        : dialogs.some((dialog) => active !== null && dialog.contains(active)),
    dialogLabels: dialogs.map((dialog) => clean(dialog.getAttribute("aria-label") ?? "")),
    activeElement: active
      ? clean(
          `${active.tagName.toLowerCase()}[${
            active.getAttribute("aria-label") ??
            active.getAttribute("placeholder") ??
            Array.from(active.textContent?.trim() ?? "")
              .slice(0, 40)
              .join("")
          }]`,
        )
      : null,
    composer: composer ? clean(composer.value) : null,
  };
});

/** The whole app shell (sidebar or tab bar) is on screen, as opposed to a gate. */
export function shellRendered(): boolean {
  const p = page.current;
  return p.sidebar || p.tabs > 0;
}

export function overlays(): number {
  const p = page.current;
  return p.dialogs + p.menus;
}

/** The theme the app should be painting, by the same rules as `packages/ui`. */
export function expectedTheme(): "light" | "dark" {
  const p = page.current;
  if (p.storedTheme === "light" || p.storedTheme === "dark") return p.storedTheme;
  if (p.storedTheme === "system") return p.prefersDark ? "dark" : "light";
  return "dark";
}
