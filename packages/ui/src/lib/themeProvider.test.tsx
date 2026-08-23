import { beforeEach, describe, expect, it } from "vitest";
import { act, render, screen } from "@testing-library/react";
import { installMatchMedia } from "../test/matchMedia";
import { STORAGE_KEYS } from "./storageKeys";
import { ThemeProvider, useTheme } from "./theme";

const MEDIA = "(prefers-color-scheme: dark)";

// One install per file: ThemeProvider asks for the same query on every mount.
const media = installMatchMedia(() => false);
const system = media.controller(MEDIA);

function Probe() {
  const { theme, preference, setPreference, toggleTheme } = useTheme();
  return (
    <div>
      <span data-testid="theme">{theme}</span>
      <span data-testid="preference">{preference}</span>
      <button type="button" onClick={toggleTheme}>
        toggle
      </button>
      <button type="button" onClick={() => setPreference("system")}>
        follow system
      </button>
    </div>
  );
}

const theme = () => screen.getByTestId("theme").textContent;
const preference = () => screen.getByTestId("preference").textContent;
const click = (name: string) => act(() => screen.getByRole("button", { name }).click());

beforeEach(() => {
  localStorage.clear();
  system.set(false);
  delete document.documentElement.dataset.theme;
});

describe("ThemeProvider", () => {
  it("restores the stored preference", () => {
    localStorage.setItem(STORAGE_KEYS.theme, "light");
    render(
      <ThemeProvider>
        <Probe />
      </ThemeProvider>,
    );
    expect(preference()).toBe("light");
    expect(theme()).toBe("light");
  });

  it("ignores a corrupt stored value rather than painting it", () => {
    // The value lands in dataset.theme, so garbage would reach the stylesheet.
    localStorage.setItem(STORAGE_KEYS.theme, "neon");
    render(
      <ThemeProvider>
        <Probe />
      </ThemeProvider>,
    );
    expect(preference()).toBe("dark");
  });

  it("paints the resolved theme onto the document and persists the choice", () => {
    localStorage.setItem(STORAGE_KEYS.theme, "system");
    system.set(true);
    render(
      <ThemeProvider>
        <Probe />
      </ThemeProvider>,
    );
    expect(theme()).toBe("dark");
    expect(document.documentElement.dataset.theme).toBe("dark");
    // The preference is stored, not the resolved theme: "system" must stay
    // "system" across a restart, or it would freeze at whatever it resolved to.
    expect(localStorage.getItem(STORAGE_KEYS.theme)).toBe("system");
  });

  it("keeps following the OS after it changes", () => {
    localStorage.setItem(STORAGE_KEYS.theme, "system");
    render(
      <ThemeProvider>
        <Probe />
      </ThemeProvider>,
    );
    expect(theme()).toBe("light");

    act(() => system.set(true));
    expect(theme()).toBe("dark");
    expect(preference()).toBe("system");
  });

  it("toggles between the two explicit themes", () => {
    localStorage.setItem(STORAGE_KEYS.theme, "dark");
    render(
      <ThemeProvider>
        <Probe />
      </ThemeProvider>,
    );
    click("toggle");
    expect(preference()).toBe("light");
    click("toggle");
    expect(preference()).toBe("dark");
  });

  it("toggling out of system flips away from what is on screen", () => {
    // Anything else would look like the toggle did nothing.
    localStorage.setItem(STORAGE_KEYS.theme, "system");
    system.set(true);
    render(
      <ThemeProvider>
        <Probe />
      </ThemeProvider>,
    );
    expect(theme()).toBe("dark");
    click("toggle");
    expect(preference()).toBe("light");
    expect(theme()).toBe("light");
  });

  it("stops following the OS once the user picks a side", () => {
    localStorage.setItem(STORAGE_KEYS.theme, "light");
    render(
      <ThemeProvider>
        <Probe />
      </ThemeProvider>,
    );
    act(() => system.set(true));
    expect(theme()).toBe("light");
  });

  it("can be put back on system, and follows it again", () => {
    localStorage.setItem(STORAGE_KEYS.theme, "light");
    render(
      <ThemeProvider>
        <Probe />
      </ThemeProvider>,
    );
    click("follow system");
    expect(preference()).toBe("system");
    act(() => system.set(true));
    expect(theme()).toBe("dark");
  });

  it("refuses to run outside a provider instead of silently painting nothing", () => {
    expect(() => render(<Probe />)).toThrow(/ThemeProvider/);
  });
});
