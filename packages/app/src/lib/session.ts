import { STORAGE_KEYS } from "@borg/ui";

/**
 * Forgets the browser's client token and restarts the app, which drops it back
 * at the sign-in screen.
 *
 * Only the web surface holds a token — desktop is authenticated by the unix
 * socket's file permissions — so this is a no-op worth guarding on
 * `platform.kind === "web"` at the call site.
 */
export function signOut(): void {
  localStorage.removeItem(STORAGE_KEYS.token);
  location.reload();
}
