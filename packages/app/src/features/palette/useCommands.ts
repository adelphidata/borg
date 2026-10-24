import { useCallback, useEffect, useMemo, useState } from "react";
import { useNavigate } from "react-router-dom";
import { useTheme } from "@borg/ui";
import { dispatchKeybind, hintFor } from "../../lib/keybinds";
import { useQuery } from "../../lib/useDaemon";
import { useSettings } from "../../lib/settings";
import { isBlankSession, rowTitle } from "../conversation/draftStore";
import { isSessionUnread, markAllSessionsRead } from "../sessions/unreadStore";
import { routes } from "../../routes";
import { buildCommands } from "./commands";
import type { Command } from "./score";
import { noteSkillUse, readSkillRecency } from "./skillRecency";

interface UseCommandsOptions {
  /**
   * Whether the surface asking is on screen. Gates the conversation listing, so
   * a closed palette and a composer with no `/` in it cost no daemon call.
   */
  active: boolean;
  /** Whether the user has typed a query — uncurated keys appear only then. */
  searching: boolean;
  /**
   * Whether this surface can accept the prompt text a skill row writes. Only
   * the composer can, so only the composer pays for the listing.
   */
  withSkills?: boolean;
  /** Compacts the conversation this surface belongs to, when it can. */
  compact?: () => void;
}

export interface CommandsResult {
  commands: Command[];
  /**
   * Set when a setting write failed. A row that leaves its surface open is
   * picked by watching the state change, so a failure has to say so — otherwise
   * it is indistinguishable from a slow write.
   */
  saveError: string | undefined;
  clearSaveError: () => void;
}

/**
 * Builds the command list against live daemon state.
 *
 * Both ⌘K and the composer's `/` menu call this, which is what stops the two
 * from offering different answers to the same question.
 */
export function useCommands({
  active,
  searching,
  withSkills = false,
  compact,
}: UseCommandsOptions): CommandsResult {
  const navigate = useNavigate();
  const settings = useSettings();
  const { preference, setPreference } = useTheme();

  const sessions = useQuery(
    async (borg) => (active ? (await borg.admin.listSessions({})).sessions : []),
    [active],
  );

  // Latched rather than tracking `active`: the daemon answers `ListSkills` by
  // walking the skills directory and probing `$PATH` for every declared binary,
  // and the composer's menu opens and closes on every `/` typed and deleted.
  // Once is enough — the set does not change while the surface is mounted.
  const [askedSkills, setAskedSkills] = useState(false);
  const wantSkills = askedSkills || (active && withSkills);
  useEffect(() => {
    if (active && withSkills) setAskedSkills(true);
  }, [active, withSkills]);

  const skills = useQuery(
    async (borg) => (wantSkills ? (await borg.admin.listSkills({})).skills : []),
    [wantSkills],
  );

  const [skillRecency, setSkillRecency] = useState<string[]>(() => readSkillRecency(localStorage));
  const noteSkill = useCallback((name: string) => {
    setSkillRecency(noteSkillUse(localStorage, name));
  }, []);

  // Values written but not yet confirmed by a fresh snapshot. Without this a
  // toggle reads its old value until the write round-trips, so pressing Enter
  // twice writes the same value twice and the second press silently does
  // nothing. Cleared per key when the snapshot catches up, or on failure.
  const [pending, setPending] = useState<Map<string, string>>(new Map());
  const [failed, setFailed] = useState(false);

  const clearSaveError = useCallback(() => setFailed(false), []);

  const saveSetting = useCallback(
    (key: string, value: string) => {
      setFailed(false);
      setPending((prev) => new Map(prev).set(key, value));
      void settings.save(key, value).then((ok) => {
        // Reporting success we did not have would leave the row showing a
        // state the daemon never took. The overlay for a *successful* write
        // stays until the snapshot confirms it: `save` resolves as soon as the
        // RPC returns, while the refetch it triggers is still in flight, so
        // dropping it here would flash the old state back and let a second
        // Enter rewrite the same value.
        if (ok) return;
        setFailed(true);
        setPending((prev) => {
          const next = new Map(prev);
          next.delete(key);
          return next;
        });
      });
    },
    [settings.save],
  );

  const snapshot = settings.settings.state === "ready" ? settings.settings.data : undefined;

  // Retire an overlay once the snapshot agrees with it — or once its key has
  // left the registry, which would otherwise pin the overlay forever.
  useEffect(() => {
    if (!snapshot || pending.size === 0) return;
    const next = new Map(pending);
    for (const [key, value] of pending) {
      const current = snapshot.get(key);
      if (!current || current.value === value) next.delete(key);
    }
    if (next.size !== pending.size) setPending(next);
  }, [snapshot, pending]);

  // Composed at render rather than inside the `then` above: the provider sets
  // its own error state as the write rejects, so a message built in that
  // callback would carry the *previous* failure's text (usually none at all).
  const saveError = failed
    ? `Could not change that setting. ${settings.saveError?.message ?? ""}`.trim()
    : undefined;

  const effective = useMemo(() => {
    if (!snapshot || pending.size === 0) return snapshot;
    const merged = new Map(snapshot);
    for (const [key, value] of pending) {
      const current = merged.get(key);
      if (current) merged.set(key, { ...current, value });
    }
    return merged;
  }, [snapshot, pending]);

  // Read rows for the sessions the palette already fetched, in the daemon's own
  // units — the same figures the dots are derived from.
  const readRows = useMemo(
    () =>
      (sessions.state === "ready" ? sessions.data : []).map((s) => ({
        id: s.id,
        updatedAt: Number(s.updatedUnixTs),
        messageCount: Number(s.messageCount),
      })),
    [sessions.state, sessions.data],
  );
  // Named and filtered as the sidebar does, so a row never shows the daemon's
  // " (live)" marker or an empty session the lists already hide.
  const commandSessions = useMemo(
    () =>
      (sessions.state === "ready" ? sessions.data : [])
        .filter((s) => s.pinned || !isBlankSession(s.title, Number(s.messageCount), s.id))
        .map((s) => ({ id: s.id, title: rowTitle(s.title, Number(s.messageCount), s.id) })),
    [sessions.state, sessions.data],
  );
  const anyUnread = readRows.some((r) => isSessionUnread(r.id, r.updatedAt, r.messageCount));

  const commands = useMemo<Command[]>(
    () =>
      !active
        ? []
        : buildCommands(
            {
              navigate,
              dispatch: dispatchKeybind,
              hintFor,
              settings: effective,
              saveSetting,
              sessions: commandSessions,
              markAllRead: anyUnread ? () => markAllSessionsRead(readRows) : undefined,
              compact,
              skills: skills.state === "ready" ? skills.data : [],
              skillRecency,
              noteSkillUse: noteSkill,
              themePreference: preference,
              setThemePreference: setPreference,
              searching,
            },
            routes,
          ),
    // `active` gates the build itself: a closed palette and a composer with no
    // `/` in it render no rows, and building ~500 of them on every settings
    // snapshot for a surface nobody is looking at is pure cost.
    [
      active,
      navigate,
      commandSessions,
      readRows,
      anyUnread,
      compact,
      skills.state,
      skills.data,
      skillRecency,
      noteSkill,
      effective,
      saveSetting,
      preference,
      setPreference,
      searching,
    ],
  );

  return { commands, saveError, clearSaveError };
}
