/**
 * The page a window shows once its renderer has crashed too often to reload.
 *
 * Without it the window stays blank, which reads as a frozen app with no way
 * out but a quit. The page says what happened in plain words and offers one
 * action.
 *
 * It is a `data:` URL so it ships inside the main process: no file for the
 * build to forget, and nothing in it that the crashing bundle could share.
 * It runs no script. Its one control is a link to `RECOVERY_RELOAD_URL`,
 * which the window's navigation guard catches before it leaves the app: the
 * guard resets the reload budget and loads the app again from the main
 * process. A page-initiated navigation from `data:` to `file:` would be
 * refused by Chromium, so the page cannot load the app itself.
 */

/**
 * The address the Reload link points at. The `.invalid` top-level domain is
 * reserved and never resolves, so even if the guard missed it nothing would
 * load.
 */
export const RECOVERY_RELOAD_URL = "https://reload.borg.invalid/";

const HTML = `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'">
<title>Borg</title>
<style>
  :root { color-scheme: light dark; --bg: #f7f8f6; --ink: #1a1d1a; --ink2: #5b615b; --accent: #1a1d1a; --on-accent: #f7f8f6; }
  @media (prefers-color-scheme: dark) {
    :root { --bg: #0b0e0b; --ink: #e8ebe7; --ink2: #9aa19a; --accent: #e8ebe7; --on-accent: #0b0e0b; }
  }
  html, body { height: 100%; margin: 0; }
  body {
    display: flex; align-items: center; justify-content: center;
    background: var(--bg); color: var(--ink);
    font: 15px/1.6 -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
    -webkit-app-region: drag;
  }
  main { max-width: 360px; padding: 0 24px; text-align: center; }
  h1 { margin: 0 0 8px; font-size: 20px; font-weight: 500; }
  p { margin: 0 0 20px; color: var(--ink2); }
  a {
    display: inline-block; padding: 8px 18px; border-radius: 8px;
    background: var(--accent); color: var(--on-accent); text-decoration: none;
    -webkit-app-region: no-drag;
  }
</style>
</head>
<body>
<main>
  <h1>Borg's window stopped working</h1>
  <p>It failed several times in a row. Your conversations and scheduled tasks are safe.</p>
  <a href="${RECOVERY_RELOAD_URL}">Reload</a>
</main>
</body>
</html>`;

/** The recovery page as a URL a window can load. */
export const RECOVERY_PAGE_URL = `data:text/html;charset=utf-8,${encodeURIComponent(HTML)}`;

/** True when `url` is the recovery page's Reload link. */
export function isRecoveryReload(url: string): boolean {
  try {
    return new URL(url).href === RECOVERY_RELOAD_URL;
  } catch {
    return false;
  }
}
