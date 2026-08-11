import { createContext, useCallback, useContext, useMemo, type ReactNode } from "react";
import type { BorgError } from "@borg/client";
import { useMutation, useQuery, type QueryResult } from "./useDaemon";

/** One daemon setting: its effective value and whether it differs from the default. */
export interface DaemonSetting {
  key: string;
  value: string;
  /** True when the value is a DB override rather than the compiled default. */
  overridden: boolean;
  /**
   * Curated display metadata from the daemon's settings table. Empty for a key
   * that table does not name — the great majority — which is what marks a
   * setting as internal rather than something to put in front of someone.
   */
  label: string;
  /** `bool` | `float` | `uint` | `select` | `text` | `secret`, or empty. */
  kind: string;
  group: string;
}

interface SettingsContextValue {
  /** Full registry snapshot from `Admin.GetSettings`, keyed by setting key. */
  settings: QueryResult<Map<string, DaemonSetting>>;
  /** Writes one key via `Admin.SetSetting`, then refreshes the snapshot. */
  save: (key: string, value: string) => Promise<boolean>;
  /** Clears one key back to its default via `Admin.UnsetSetting`, then refreshes. */
  clear: (key: string) => Promise<boolean>;
  saving: boolean;
  saveError: BorgError | undefined;
  resetSaveError: () => void;
}

const SettingsContext = createContext<SettingsContextValue | null>(null);

/**
 * The daemon is the source of truth for every setting except the theme, which
 * stays in localStorage because it must apply before first paint.
 */
export function SettingsProvider({ children }: { children: ReactNode }) {
  const settings = useQuery(async (borg) => {
    const snapshot = await borg.admin.getSettings({});
    return new Map(
      snapshot.entries.map((e) => [
        e.key,
        {
          key: e.key,
          value: e.value,
          overridden: e.category === "db",
          label: e.label,
          kind: e.kind,
          group: e.group,
        },
      ]),
    );
  });

  const write = useMutation(
    (borg, vars: { key: string; value: string | null }) =>
      vars.value === null
        ? borg.admin.unsetSetting({ key: vars.key })
        : borg.admin.setSetting({ key: vars.key, value: vars.value }),
    { onSuccess: () => settings.refetch() },
  );

  const save = useCallback(
    async (key: string, value: string) => (await write.run({ key, value })) !== undefined,
    [write.run],
  );
  const clear = useCallback(
    async (key: string) => (await write.run({ key, value: null })) !== undefined,
    [write.run],
  );

  const value = useMemo(
    () => ({
      settings,
      save,
      clear,
      saving: write.pending,
      saveError: write.error,
      resetSaveError: write.reset,
    }),
    [settings, save, clear, write.pending, write.error, write.reset],
  );

  return <SettingsContext.Provider value={value}>{children}</SettingsContext.Provider>;
}

export function useSettings(): SettingsContextValue {
  const ctx = useContext(SettingsContext);
  if (!ctx) throw new Error("useSettings must be used within SettingsProvider");
  return ctx;
}
