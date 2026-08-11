import { useCallback, useMemo, useState } from "react";
import { useNavigate } from "react-router-dom";
import { useTheme } from "@borg/ui";
import { dispatchKeybind, hintFor } from "../../lib/keybinds";
import { useQuery } from "../../lib/useDaemon";
import { useSettings } from "../../lib/settings";
import { routes } from "../../routes";
import { buildCommands } from "./commands";
import type { Command } from "./score";

interface UseCommandsOptions {
  /**
   * Whether the surface asking is on screen. Gates the conversation listing, so
   * a closed palette and a composer with no `/` in it cost no daemon call.
   */
  active: boolean;
  /** Whether the user has typed a query — uncurated keys appear only then. */
  searching: boolean;
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
export function useCommands({ active, searching }: UseCommandsOptions): CommandsResult {
  const navigate = useNavigate();
  const settings = useSettings();
  const { preference, setPreference } = useTheme();

  const sessions = useQuery(
    async (borg) => (active ? (await borg.admin.listSessions({})).sessions : []),
    [active],
  );

  // Values written but not yet confirmed by a fresh snapshot. Without this a
  // toggle reads its old value until the write round-trips, so pressing Enter
  // twice writes the same value twice and the second press silently does
  // nothing. Cleared per key when the snapshot catches up, or on failure.
  const [pending, setPending] = useState<Map<string, string>>(new Map());
  const [saveError, setSaveError] = useState<string>();

  const clearSaveError = useCallback(() => setSaveError(undefined), []);

  const saveSetting = useCallback(
    (key: string, value: string) => {
      setSaveError(undefined);
      setPending((prev) => new Map(prev).set(key, value));
      void settings.save(key, value).then((ok) => {
        // Reporting success we did not have would leave the row showing a
        // state the daemon never took.
        if (!ok)
          setSaveError(`Could not change that setting. ${settings.saveError?.message ?? ""}`);
        setPending((prev) => {
          const next = new Map(prev);
          next.delete(key);
          return next;
        });
      });
    },
    [settings.save, settings.saveError],
  );

  const snapshot = settings.settings.state === "ready" ? settings.settings.data : undefined;
  const effective = useMemo(() => {
    if (!snapshot || pending.size === 0) return snapshot;
    const merged = new Map(snapshot);
    for (const [key, value] of pending) {
      const current = merged.get(key);
      if (current) merged.set(key, { ...current, value });
    }
    return merged;
  }, [snapshot, pending]);

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
              sessions: sessions.state === "ready" ? sessions.data : [],
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
      sessions.state,
      sessions.data,
      effective,
      saveSetting,
      preference,
      setPreference,
      searching,
    ],
  );

  return { commands, saveError, clearSaveError };
}
