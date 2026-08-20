/**
 * Whether the user has narrowed the sidebar to its rail.
 *
 * Deliberately local, not a daemon setting. Where the nav sits is a property of
 * the window someone is looking at, not of the agent — the same account on a
 * wide desktop and a narrow laptop wants different answers, and a synced value
 * would give them one.
 *
 * Absent or unreadable means expanded: a first run should show the whole nav,
 * and a corrupt value must never leave someone staring at a rail they did not
 * ask for.
 */

import { STORAGE_KEYS } from "@borg/ui";

export function readSidebarCollapsed(store: Storage): boolean {
  try {
    return store.getItem(STORAGE_KEYS.sidebarCollapsed) === "true";
  } catch {
    return false;
  }
}

export function writeSidebarCollapsed(store: Storage, collapsed: boolean): void {
  try {
    store.setItem(STORAGE_KEYS.sidebarCollapsed, String(collapsed));
  } catch {
    // A full or blocked store costs the preference next reload, nothing more.
    // Refusing to collapse because we could not write it down would be worse.
  }
}
