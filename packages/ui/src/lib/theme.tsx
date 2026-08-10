import { STORAGE_KEYS } from "./storageKeys";
import { createContext, useCallback, useContext, useEffect, useState, type ReactNode } from "react";

/** What the user chose. "system" follows the OS. */
export type ThemePreference = "light" | "dark" | "system";
/** What is actually painted. `system` always resolves to one of these. */
export type Theme = "light" | "dark";

export const THEME_PREFERENCES: ThemePreference[] = ["light", "dark", "system"];

interface ThemeContextValue {
  /** The resolved theme currently on screen. */
  theme: Theme;
  /** What the user picked, which may be "system". */
  preference: ThemePreference;
  setPreference: (preference: ThemePreference) => void;
  /** Flips between light and dark, leaving "system" behind. */
  toggleTheme: () => void;
}

const ThemeContext = createContext<ThemeContextValue | null>(null);
const STORAGE_KEY = STORAGE_KEYS.theme;

const MEDIA = "(prefers-color-scheme: dark)";

/** Reads the stored choice, rejecting anything that is not a real preference. */
export function readPreference(stored: string | null): ThemePreference {
  return stored === "light" || stored === "dark" || stored === "system" ? stored : "dark";
}

/** Turns a preference into the theme to paint. */
export function resolveTheme(preference: ThemePreference, systemPrefersDark: boolean): Theme {
  if (preference === "system") return systemPrefersDark ? "dark" : "light";
  return preference;
}

export function ThemeProvider({ children }: { children: ReactNode }) {
  const [preference, setPreferenceState] = useState<ThemePreference>(() =>
    // Validate — a stale/garbage stored value would otherwise become dataset.theme.
    readPreference(localStorage.getItem(STORAGE_KEY)),
  );
  const [systemDark, setSystemDark] = useState(() => window.matchMedia(MEDIA).matches);

  // Following the OS means following it as it changes, not only at startup.
  useEffect(() => {
    const query = window.matchMedia(MEDIA);
    const onChange = (e: MediaQueryListEvent) => setSystemDark(e.matches);
    query.addEventListener("change", onChange);
    return () => query.removeEventListener("change", onChange);
  }, []);

  const theme = resolveTheme(preference, systemDark);

  useEffect(() => {
    document.documentElement.dataset.theme = theme;
    localStorage.setItem(STORAGE_KEY, preference);
  }, [theme, preference]);

  const toggleTheme = useCallback(() => {
    // From "system", flip to the opposite of what is showing — anything else
    // would look like the toggle did nothing.
    setPreferenceState((current) =>
      current === "light" ? "dark" : current === "dark" ? "light" : systemDark ? "light" : "dark",
    );
  }, [systemDark]);

  return (
    <ThemeContext.Provider
      value={{ theme, preference, setPreference: setPreferenceState, toggleTheme }}
    >
      {children}
    </ThemeContext.Provider>
  );
}

export function useTheme(): ThemeContextValue {
  const ctx = useContext(ThemeContext);
  if (!ctx) throw new Error("useTheme must be used within ThemeProvider");
  return ctx;
}
