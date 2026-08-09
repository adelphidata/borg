import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useState,
  type ReactNode,
} from "react";
import { basicSettings } from "../data/mock";
import { useTheme } from "@borg/ui";

interface SettingsContextValue {
  /** Current value per setting id (mock defaults until borgd lands). */
  values: Record<string, string>;
  setValue: (id: string, value: string) => void;
}

const SettingsContext = createContext<SettingsContextValue | null>(null);
const STORAGE_KEY = "borg.settings";

const defaults = Object.fromEntries(basicSettings.map((s) => [s.id, s.value]));

function load(): Record<string, string> {
  try {
    const stored: unknown = JSON.parse(localStorage.getItem(STORAGE_KEY) ?? "{}");
    // Shape-check — a stored scalar/array would otherwise spread garbage keys into the map.
    if (typeof stored !== "object" || stored === null || Array.isArray(stored)) {
      return { ...defaults };
    }
    const valid = Object.entries(stored).filter(
      ([id, v]) => id in defaults && typeof v === "string",
    );
    return { ...defaults, ...Object.fromEntries(valid) };
  } catch {
    return { ...defaults };
  }
}

export function SettingsProvider({ children }: { children: ReactNode }) {
  const [values, setValues] = useState<Record<string, string>>(load);

  // Persist outside the state updater — updaters must stay pure (StrictMode runs them twice).
  useEffect(() => {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(values));
  }, [values]);

  const setValue = useCallback((id: string, value: string) => {
    setValues((v) => ({ ...v, [id]: value }));
  }, []);

  const value = useMemo(() => ({ values, setValue }), [values, setValue]);

  return <SettingsContext.Provider value={value}>{children}</SettingsContext.Provider>;
}

export function useSettings(): SettingsContextValue {
  const ctx = useContext(SettingsContext);
  if (!ctx) throw new Error("useSettings must be used within SettingsProvider");
  return ctx;
}

interface SettingControl {
  /** Current display value for a setting id. */
  current: (id: string) => string;
  /** Apply a picked option for a setting id. */
  select: (id: string, value: string) => void;
}

/**
 * Bridges setting ids to their backing store: "appearance" reads and writes the
 * real theme, everything else the local settings map. Helpers take the id so
 * pages can resolve many settings without calling hooks in a loop.
 */
export function useSettingControl(): SettingControl {
  const { values, setValue } = useSettings();
  const { theme, toggleTheme } = useTheme();

  const current = (id: string) =>
    id === "appearance" ? (theme === "light" ? "Light" : "Dark") : values[id];

  const select = (id: string, value: string) => {
    if (id === "appearance") {
      if (value !== current(id)) toggleTheme();
    } else {
      setValue(id, value);
    }
  };

  return { current, select };
}
