/**
 * Which skills this person actually reaches for.
 *
 * A skill set grows without limit — built-ins plus everything under
 * `~/.borg/skills/` — and an alphabetical wall of them teaches nothing, which
 * is the same failure the raw settings registry had. Recency is what keeps the
 * default list short: the handful you use sit at the top, the rest are still
 * one query away.
 *
 * Deliberately local, not a daemon setting. It is a UI ordering hint, and a
 * knob nobody should ever see.
 */

import { STORAGE_KEYS } from "@borg/ui";

/** Past this the tail is noise, and a stale name would outrank a fresh one. */
const MAX_REMEMBERED = 12;

/** Skill names, most recently used first. */
export function readSkillRecency(store: Storage): string[] {
  try {
    const raw = store.getItem(STORAGE_KEYS.skillRecency);
    if (!raw) return [];
    const parsed: unknown = JSON.parse(raw);
    // Anything could be under this key — another tab, an older build, a person
    // with devtools open. A bad value must not take the menu down with it.
    if (!Array.isArray(parsed)) return [];
    return parsed.filter((n): n is string => typeof n === "string").slice(0, MAX_REMEMBERED);
  } catch {
    return [];
  }
}

/** Records `name` as the most recent use, returning the new order. */
export function noteSkillUse(store: Storage, name: string): string[] {
  const next = [name, ...readSkillRecency(store).filter((n) => n !== name)].slice(
    0,
    MAX_REMEMBERED,
  );
  try {
    store.setItem(STORAGE_KEYS.skillRecency, JSON.stringify(next));
  } catch {
    // A full or blocked store costs an ordering hint, nothing more. The menu
    // still works; it just forgets.
  }
  return next;
}

/**
 * Orders `names` by last use, keeping the given order for everything unused.
 *
 * The base order is already stable (the daemon sorts by name), so two skills
 * neither of which has been used never swap places between renders.
 */
export function byRecency<T>(rows: T[], nameOf: (row: T) => string, recent: string[]): T[] {
  const rank = new Map(recent.map((name, i) => [name, i]));
  return [...rows].sort(
    (a, b) => (rank.get(nameOf(a)) ?? recent.length) - (rank.get(nameOf(b)) ?? recent.length),
  );
}
