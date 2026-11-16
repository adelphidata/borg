/**
 * Action generators shared by the web and desktop surfaces.
 *
 * The defaults are kept, minus the clicks that would end the run (external
 * links, sign-out, daemon restarts) or run something on the host (installers,
 * agent turns on a schedule). Everything else is fair game: the daemon behind
 * the test is a scratch one with a dead model endpoint.
 */
import { isAction } from "@antithesishq/bombadil/actions";
import {
  actions,
  registerCustomAction,
  weighted,
  type Fingerprint,
} from "@antithesishq/bombadil/browser";
import {
  back,
  clicks,
  forward,
  inputs,
  reload,
  scroll,
} from "@antithesishq/bombadil/browser/defaults/actions";
import { ARROW_DOWN, ARROW_UP, ENTER, ESCAPE, TAB } from "./keys.ts";
import { page } from "./state.ts";

/**
 * Controls that would leave the app, touch the host, or stop the daemon.
 * Reversible actions (archive, rename, pause) stay in: they are the product.
 */
const BLOCKED_LABELS =
  /^(Delete|Delete .*|Remove|Remove .*|Forget|Revoke|Discard|Install|Run now|Replace|Replace key|Reveal key|Send a test event|Tidy up now|Sign in again|Reload|Start Borg|Restart to update.*|Stop this agent.*|Resend|Allow|Deny|Reset to default|Clear|Export as Markdown|Test connection|Approve)$/;

function labelOf(fingerprint: Fingerprint): string {
  return (fingerprint.accessibleName ?? fingerprint.textContent ?? "").trim();
}

function blocked(fingerprint: Fingerprint): boolean {
  if (fingerprint.href !== null && /^https?:/i.test(fingerprint.href)) return true;
  if (fingerprint.inputType === "file") return true;
  if (BLOCKED_LABELS.test(labelOf(fingerprint))) return true;
  // The raw registry editor can rewrite gateway.* and security.* keys, which
  // would change the daemon the test is talking to mid-run.
  if (page.current.hash.startsWith("#/settings/key/") && /^Sav(e|ing…)$/.test(labelOf(fingerprint)))
    return true;
  return false;
}

/** The default clicks, without the ones that would end or escape the run. */
const safeClicks = clicks.filter(
  (action) => !(isAction(action, "Click") && blocked(action.Click.fingerprint)),
);

const keys = actions(() =>
  [ESCAPE, ENTER, TAB, ARROW_UP, ARROW_DOWN].map((code) => ({ PressKey: { code } })),
);

/** `mod+<key>` chords the app binds: palette, new chat, sidebar, activity, find, settings. */
const chord = registerCustomAction("chord", async (document, window, key: string) => {
  const mac = /Mac|iPhone|iPad/.test(window.navigator.platform);
  const target = document.activeElement ?? document.body;
  target.dispatchEvent(
    new KeyboardEvent("keydown", {
      key,
      metaKey: mac,
      ctrlKey: !mac,
      bubbles: true,
      cancelable: true,
    }),
  );
});

const chords = actions(() => ["k", "n", "b", "j", "f", ","].map((key) => chord(key)));

/** Jumps straight to a route, including ones only reachable through data or a typo. */
const goto = registerCustomAction("goto", async (_document, window, hash: string) => {
  window.location.hash = hash;
});

const ROUTES = [
  "#/",
  "#/c/new",
  "#/sessions",
  "#/connections",
  "#/connections/skills/new",
  "#/schedules",
  "#/usage",
  "#/health",
  "#/settings",
  "#/settings/name",
  "#/settings/provider",
  "#/settings/model",
  "#/settings/thinking",
  "#/settings/mode",
  "#/settings/appearance",
  "#/settings/voice",
  "#/no-such-page",
];

const routes = actions(() => ROUTES.map((hash) => goto(hash)));

/** Phone, the breakpoint on both sides, and two desktop sizes. */
const viewports = actions(() =>
  [
    [390, 844],
    [700, 800],
    [701, 800],
    [1024, 768],
    [1440, 900],
  ].map(([width, height]) => ({ SetViewport: { width, height } })),
);

/** While nothing is actionable or a loader shows, waiting is the only sound move. */
const settle = actions(() =>
  page.current.actionable === 0 || page.current.loading ? ["Wait"] : [],
);

export const borgActions = weighted([
  [100, safeClicks],
  [40, inputs],
  [15, keys],
  [10, chords],
  [10, scroll],
  [10, routes],
  [5, viewports],
  [4, back],
  [2, forward],
  [3, reload],
  [20, settle],
]);
