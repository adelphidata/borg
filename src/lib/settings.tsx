import { createContext, useCallback, useContext, useState, type ReactNode } from "react";
import { basicSettings } from "../data/mock";

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
    return { ...defaults, ...JSON.parse(localStorage.getItem(STORAGE_KEY) ?? "{}") };
  } catch {
    return { ...defaults };
  }
}

export function SettingsProvider({ children }: { children: ReactNode }) {
  const [values, setValues] = useState<Record<string, string>>(load);

  const setValue = useCallback((id: string, value: string) => {
    setValues((v) => {
      const next = { ...v, [id]: value };
      localStorage.setItem(STORAGE_KEY, JSON.stringify(next));
      return next;
    });
  }, []);

  return (
    <SettingsContext.Provider value={{ values, setValue }}>{children}</SettingsContext.Provider>
  );
}

export function useSettings(): SettingsContextValue {
  const ctx = useContext(SettingsContext);
  if (!ctx) throw new Error("useSettings must be used within SettingsProvider");
  return ctx;
}
