import { act, renderHook, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { DaemonSetting } from "../../lib/settings";
import { useCommands } from "./useCommands";

/**
 * The daemon seam, stubbed at the two hooks `useCommands` reaches for. The
 * point of these tests is what the hook does *between* issuing a write and the
 * refreshed snapshot arriving — a window the real provider hides behind a
 * round-trip.
 */
const daemon = vi.hoisted(() => ({
  settings: new Map<string, DaemonSetting>(),
  /** Settles the in-flight `save`, so a test can hold the round-trip open. */
  resolveSave: undefined as ((ok: boolean) => void) | undefined,
  saveError: undefined as { message: string } | undefined,
  sessions: [] as { id: string; title: string; messageCount: bigint; pinned: boolean }[],
}));

vi.mock("react-router-dom", () => ({ useNavigate: () => vi.fn() }));

// The real module pulls in every page component; only the row shape matters.
vi.mock("../../routes", () => ({
  routes: [{ id: "settings", path: "/settings", label: "Settings" }],
}));

vi.mock("@borg/ui", () => ({
  useTheme: () => ({ preference: "system", setPreference: vi.fn() }),
  THEME_PREFERENCES: ["system", "light", "dark"],
}));

vi.mock("../../lib/useDaemon", () => ({
  // Told apart by what they ask for: the conversation listing, or the skills.
  useQuery: (fn: () => unknown) => ({
    state: "ready",
    data: String(fn).includes("listSessions") ? daemon.sessions : [],
  }),
}));

vi.mock("../../lib/settings", () => ({
  useSettings: () => ({
    settings: { state: "ready", data: daemon.settings },
    save: () => new Promise<boolean>((resolve) => (daemon.resolveSave = resolve)),
    saveError: daemon.saveError,
  }),
}));

function bool(key: string, value: string): DaemonSetting {
  return { key, value, overridden: false, label: "Evolution", kind: "bool", group: "Personality" };
}

function toggle(commands: { id: string; detail?: string; run: () => void }[]) {
  const row = commands.find((c) => c.id === "setting:evolution.enabled");
  if (!row) throw new Error("expected the toggle row to exist");
  return row;
}

beforeEach(() => {
  daemon.settings = new Map([["evolution.enabled", bool("evolution.enabled", "true")]]);
  daemon.resolveSave = undefined;
  daemon.saveError = undefined;
  daemon.sessions = [];
});

afterEach(() => vi.clearAllMocks());

describe("useCommands", () => {
  it("names conversations as the sidebar does and leaves out empty ones", () => {
    daemon.sessions = [
      { id: "a", title: "Ship the fix (live)", messageCount: 2n, pinned: false },
      { id: "b", title: "New conversation", messageCount: 0n, pinned: false },
    ];
    const { result } = renderHook(() => useCommands({ active: true, searching: false }));
    const rows = result.current.commands.filter((c) => c.id.startsWith("session:"));
    expect(rows.map((r) => r.label)).toEqual(["Ship the fix"]);
  });

  it("keeps the new value on the row until the snapshot confirms it", async () => {
    const { result, rerender } = renderHook(() => useCommands({ active: true, searching: false }));
    expect(toggle(result.current.commands).detail).toBe("on");

    act(() => toggle(result.current.commands).run());
    expect(toggle(result.current.commands).detail).toBe("off");

    // The RPC returns before the refetch it triggers lands. Dropping the
    // overlay here would flash "on" back and let a second Enter rewrite the
    // same value instead of toggling it.
    await act(async () => daemon.resolveSave?.(true));
    expect(toggle(result.current.commands).detail).toBe("off");

    daemon.settings = new Map([["evolution.enabled", bool("evolution.enabled", "false")]]);
    rerender();
    await waitFor(() => expect(toggle(result.current.commands).detail).toBe("off"));
  });

  it("puts the row back and names the reason when the write is refused", async () => {
    const { result } = renderHook(() => useCommands({ active: true, searching: false }));

    act(() => toggle(result.current.commands).run());
    daemon.saveError = { message: "daemon unreachable." };
    await act(async () => daemon.resolveSave?.(false));

    // A row that leaves its surface open is picked by watching the state
    // change, so a refusal has to both revert the row and say why.
    expect(toggle(result.current.commands).detail).toBe("on");
    expect(result.current.saveError).toContain("daemon unreachable.");
  });

  it("clears a reported failure on request", async () => {
    const { result } = renderHook(() => useCommands({ active: true, searching: false }));
    act(() => toggle(result.current.commands).run());
    daemon.saveError = { message: "daemon unreachable." };
    await act(async () => daemon.resolveSave?.(false));
    expect(result.current.saveError).toBeDefined();

    act(() => result.current.clearSaveError());
    expect(result.current.saveError).toBeUndefined();
  });
});
