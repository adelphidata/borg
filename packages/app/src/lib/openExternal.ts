/**
 * Opens a web page outside the app: a new browser tab on the web, the system
 * browser on desktop (the shell's window-open handler routes it there).
 *
 * Only `http:` and `https:`. A URL that came from the daemon — a Google sign-in
 * page, say — is still not trusted to name a `file:` path or a script.
 * Returns whether anything was opened.
 */
export function openExternal(url: string): boolean {
  let protocol: string;
  try {
    protocol = new URL(url).protocol;
  } catch {
    return false;
  }
  if (protocol !== "http:" && protocol !== "https:") return false;
  window.open(url, "_blank", "noopener,noreferrer");
  return true;
}
