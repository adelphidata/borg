import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { dispatchKeybind, resetKeybinds } from "../../lib/keybinds";
import { Composer } from "./Composer";
import { draftKeyFor } from "./draftStore";
import { ModelPill } from "./ModelPill";

/** The composer's menus talk to the daemon; here it is only the box to return to. */
vi.mock("../palette/useCommands", () => ({
  useCommands: () => ({ commands: [], saveError: undefined, clearSaveError: () => {} }),
}));
vi.mock("./usePathCompletions", () => ({ usePathCompletions: () => [] }));

const save = vi.fn(() => Promise.resolve(true));

/** The settings the daemon currently reports, rewritten per test. */
let stored: Record<string, string> = {};
/** False stands in for a snapshot that has not arrived yet. */
let ready = true;
/** A rejected write, as the daemon reports one. */
let saveError: { message: string } | undefined;
const resetSaveError = vi.fn();

/** The daemon's provider catalog, as `Admin.GetSetupStatus` reports it. */
const CATALOG = [
  {
    id: "anthropic",
    label: "Anthropic",
    blurb: "",
    keyUrl: "",
    needsApiKey: true,
    recommended: false,
    featured: true,
    defaultModel: "claude-opus-5",
    models: [
      { id: "claude-opus-5", label: "Opus 5", note: "" },
      { id: "claude-sonnet-5", label: "Sonnet 5", note: "" },
    ],
  },
];

vi.mock("../onboarding/useSetupStatus", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../onboarding/useSetupStatus")>()),
  useSetupStatus: () => ({
    state: "ready",
    data: { configured: true, provider: "anthropic", model: "", providers: CATALOG, detected: [] },
    refetch: () => {},
    refreshing: false,
  }),
}));

vi.mock("../../lib/settings", () => ({
  useSettings: () => ({
    settings: ready
      ? {
          state: "ready",
          data: new Map(
            Object.entries(stored).map(([key, value]) => [key, { key, value, overridden: true }]),
          ),
          refetch: () => {},
          refreshing: false,
        }
      : { state: "loading", refetch: () => {}, refreshing: false },
    save,
    clear: vi.fn(),
    saving: false,
    saveError,
    resetSaveError,
  }),
}));

beforeEach(() => {
  save.mockClear();
  resetSaveError.mockClear();
  saveError = undefined;
  ready = true;
  stored = { provider: "anthropic", model: "claude-opus-5", "llm.thinking": "medium" };
});

afterEach(() => {
  cleanup();
  resetKeybinds();
});

/** The pill itself, found by the one label that never changes. */
function pill() {
  return screen.queryByRole("button", { name: "Model and thinking level" });
}

/** Opens the menu the way a user does. */
function openMenu() {
  fireEvent.click(pill()!);
}

describe("ModelPill", () => {
  it("shows the model's short name, not its id", () => {
    render(<ModelPill />);
    expect(pill()!.textContent).toContain("Opus 5");
    expect(pill()!.textContent).not.toContain("claude-opus-5");
  });

  it("falls back to the raw id for a model the shortlist does not know", () => {
    stored.model = "some-provider/experimental-v3";
    render(<ModelPill />);
    expect(pill()!.textContent).toContain("some-provider/experimental-v3");
  });

  it("names the thinking level beside the model, unless it is off", () => {
    render(<ModelPill />);
    expect(pill()!.textContent).toContain("medium");
    cleanup();

    // "off" is the absence of a claim, not a badge worth carrying.
    stored["llm.thinking"] = "off";
    render(<ModelPill />);
    expect(pill()!.textContent).not.toContain("off");
  });

  it("stays hidden until a model is configured", () => {
    stored = {};
    render(<ModelPill />);
    expect(pill()).toBeNull();
  });

  it("names the provider default the daemon runs when no model is set", () => {
    // An empty `model` makes the daemon adopt the catalog default, so a fresh
    // install still has a model to show and to switch away from.
    stored.model = "";
    render(<ModelPill />);
    expect(pill()!.textContent).toContain("Opus 5");
    openMenu();
    const current = screen.getByRole("button", { name: /Opus 5/ });
    expect(current.querySelector("svg")).not.toBeNull();
  });

  it("stays hidden while the settings snapshot is still loading", () => {
    ready = false;
    render(<ModelPill />);
    expect(pill()).toBeNull();
  });

  it("writes the model through the same mutation Settings uses", () => {
    render(<ModelPill />);
    openMenu();
    fireEvent.click(screen.getByRole("button", { name: /Sonnet 5/ }));
    expect(save).toHaveBeenCalledWith("model", "claude-sonnet-5");
  });

  it("writes the thinking level under the daemon's own key", () => {
    render(<ModelPill />);
    openMenu();
    fireEvent.click(screen.getByRole("button", { name: /high/ }));
    expect(save).toHaveBeenCalledWith("llm.thinking", "high");
  });

  it("closes the menu once a choice is made", () => {
    render(<ModelPill />);
    openMenu();
    fireEvent.click(screen.getByRole("button", { name: /Sonnet 5/ }));
    expect(screen.queryByRole("button", { name: /Sonnet 5/ })).toBeNull();
  });

  it("offers thinking levels even for a provider with no model shortlist", () => {
    stored.provider = "openrouter";
    render(<ModelPill />);
    openMenu();
    // No opinion on this provider's models, so that section is simply absent —
    // rather than an empty list or a lecture about where to go instead.
    expect(screen.queryByText("Model")).toBeNull();
    expect(screen.getByText("Thinking")).toBeTruthy();
  });

  it("closes on an outside click without choosing anything", () => {
    render(<ModelPill />);
    openMenu();
    fireEvent.pointerDown(document.body);
    expect(screen.queryByRole("button", { name: /Sonnet 5/ })).toBeNull();
    expect(save).not.toHaveBeenCalled();
  });

  it("says so when the daemon refuses the change", () => {
    // The menu closes on click, so without this the pill would keep naming the
    // old model with nothing anywhere to say the switch did not take.
    saveError = { message: "connection refused" };
    render(<ModelPill />);
    expect(screen.getByRole("alert").textContent).toContain("Could not change the model");
  });

  it("clears a stale failure when the menu is reopened", () => {
    saveError = { message: "connection refused" };
    render(<ModelPill />);
    openMenu();
    expect(resetSaveError).toHaveBeenCalled();
  });
});

describe("ModelPill beside the composer", () => {
  /** A half-written message with the caret in the middle of it, menu open. */
  function midSentence() {
    // jsdom has no matchMedia, and the composer asks it for the page width.
    window.matchMedia = vi.fn(() => ({
      matches: false,
      addEventListener: () => {},
      removeEventListener: () => {},
    })) as unknown as typeof window.matchMedia;
    render(
      <Composer
        busy={false}
        draftKey={draftKeyFor("s")}
        conversationKey="s"
        onSend={() => {}}
        onSteer={() => {}}
        onCancel={() => {}}
      />,
    );
    const box = screen.getByRole("textbox") as HTMLTextAreaElement;
    fireEvent.change(box, { target: { value: "compare these two" } });
    box.setSelectionRange(8, 13);
    // A click moves focus to the pill, which is where it used to stay.
    pill()!.focus();
    openMenu();
    return box;
  }

  it.each([
    {
      name: "a model is picked",
      close: () => void fireEvent.click(screen.getByRole("button", { name: /Sonnet 5/ })),
    },
    { name: "Escape closes the menu", close: () => act(() => void dispatchKeybind("cancel-turn")) },
  ])("hands the cursor back where it was once $name", ({ close }) => {
    const box = midSentence();
    expect(document.activeElement).not.toBe(box);

    close();

    expect(screen.queryByRole("button", { name: /Sonnet 5/ })).toBeNull();
    expect(document.activeElement).toBe(box);
    expect([box.selectionStart, box.selectionEnd]).toEqual([8, 13]);
    expect(box.value).toBe("compare these two");
  });
});
