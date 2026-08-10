/**
 * Unsent drafts, shared between the composer that writes them and the session
 * lists that name rows after them.
 *
 * The composer already persists each draft to `sessionStorage`, but storage
 * events do not fire in the tab that wrote them, so a list rendered beside the
 * composer would never learn a key was typed. This module keeps the same values
 * in memory and publishes a version number the lists can subscribe to;
 * `sessionStorage` stays the durable copy that survives a reload.
 */

const PREFIX = "borg:draft:";

const drafts = new Map<string, string>();
const listeners = new Set<() => void>();
let version = 0;

/** The storage key a session's draft lives under. */
export function draftKeyFor(session: string): string {
  return `${PREFIX}${session}`;
}

/**
 * The title the daemon will give a conversation started with this text.
 *
 * Mirrors `Session::update_from_history` in `crates/cortex/src/session.rs`
 * exactly — first paragraph, 60 code points, ellipsis when it was longer — so
 * the name a row shows before sending is the name it keeps afterwards. A
 * word-boundary truncation would read better and rename the row on send, which
 * is the one outcome worth avoiding. The text is trimmed first because that is
 * what the composer submits.
 */
export function draftTitle(text: string): string {
  const trimmed = text.trim();
  if (trimmed === "") return "";
  const firstPara = trimmed.split("\n\n")[0] ?? trimmed;
  // Spread rather than slice: `chars()` on the daemon side counts code points,
  // and a UTF-16 slice would cut an emoji in half at the boundary.
  const chars = [...firstPara];
  return chars.length > 60 ? `${chars.slice(0, 60).join("")}...` : firstPara;
}

function notify() {
  version += 1;
  for (const listener of listeners) listener();
}

/** Records the current text for a draft key. No-ops when nothing changed. */
export function publishDraft(key: string, text: string): void {
  if ((drafts.get(key) ?? "") === text) return;
  if (text === "") drafts.delete(key);
  else drafts.set(key, text);
  notify();
}

/** The draft for a key: the live value, else whatever survived a reload. */
export function readDraft(key: string): string {
  const live = drafts.get(key);
  if (live !== undefined) return live;
  try {
    return sessionStorage.getItem(key) ?? "";
  } catch {
    // Private-mode browsers can throw on storage access. A missing draft only
    // costs the row its provisional name.
    return "";
  }
}

/**
 * Re-keys a draft typed before the session had an id.
 *
 * A conversation opened at `/c/new` stores its draft under a placeholder key;
 * once the daemon names the session, the draft has to follow or the composer
 * would blank on the very next render.
 */
export function moveDraft(fromKey: string, toKey: string): void {
  if (fromKey === toKey) return;
  const text = readDraft(fromKey);
  if (text === "") return;
  try {
    sessionStorage.setItem(toKey, text);
    sessionStorage.removeItem(fromKey);
  } catch {
    // Memory alone still carries the draft for this session.
  }
  drafts.delete(fromKey);
  drafts.set(toKey, text);
  notify();
}

/** Subscribes to every draft change. Returns the unsubscribe. */
export function subscribeDrafts(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

/** Snapshot for `useSyncExternalStore`: bumped whenever any draft changes. */
export function draftsVersion(): number {
  return version;
}

/**
 * What to call a session row.
 *
 * The daemon calls a fresh session "New conversation" and the list falls back
 * to "Untitled"; both are placeholders standing in for a name the user has
 * effectively already written. Only a session with no messages can be renamed
 * this way — a titled conversation in use must never be relabelled by a draft.
 */
export function rowTitle(title: string, messageCount: number, sessionId: string): string {
  if (title !== "" && title !== "New conversation") return title;
  if (messageCount === 0) {
    const derived = draftTitle(readDraft(draftKeyFor(sessionId)));
    if (derived !== "") return derived;
  }
  return title || "Untitled";
}
