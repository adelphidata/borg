import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { dispatchKeybind, resetKeybinds } from "../../lib/keybinds";
import { Composer } from "./Composer";
import { draftKeyFor } from "./draftStore";
import { ModelPill } from "./ModelPill";

/** The composer's menus talk to the daemon; here it is only the box to return to. */
vi.mock("../palette/useCommands", () => ({
  useCommands: () => ({ commands: [], saveError: undefined, clearSaveError: () => {} }),
}));
vi.mock("./usePathCompletions", () => ({ usePathCompletions: () => [] }));
vi.mock("./VoiceInputButton", () => ({ VoiceInputButton: () => null }));

/** The width the page is rendered at; a phone has less room and a keyboard to raise. */
const device = vi.hoisted(() => ({ mobile: false }));
vi.mock("@borg/ui", async (importOriginal) => ({
  ...(await importOriginal<typeof import("@borg/ui")>()),
  useIsMobile: () => device.mobile,
}));

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
      { id: "claude-sonnet-5", label: "Sonnet 5", note: "balanced" },
      { id: "claude-haiku-4-5", label: "Haiku 4.5", note: "fast, cheap" },
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
  device.mobile = false;
  stored = { provider: "anthropic", model: "claude-opus-5", "llm.thinking": "medium" };
});

afterEach(() => {
  cleanup();
  resetKeybinds();
});

/** The pill itself, found by the one label that never changes. */
function pill() {
  return screen.queryByRole("button", { name: "Model, thinking level and mode" });
}

/** Opens the menu the way a user does. */
function openMenu() {
  fireEvent.click(pill()!);
}

/** One choice in the open menu, a model or a thinking level. */
const item = (name: RegExp | string) => screen.getByRole("menuitemradio", { name });
const queryItem = (name: RegExp | string) => screen.queryByRole("menuitemradio", { name });

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
    expect(pill()!.textContent).toContain("Medium");
    cleanup();

    // "off" is the absence of a claim, not a badge worth carrying.
    stored["llm.thinking"] = "off";
    render(<ModelPill />);
    expect(pill()!.textContent).not.toContain("Off");
  });

  it("names only the model on a phone, and keeps the level in the menu", () => {
    device.mobile = true;
    render(<ModelPill />);
    expect(pill()!.textContent).toContain("Opus 5");
    expect(pill()!.textContent).not.toContain("Medium");

    openMenu();
    expect(item("Medium").getAttribute("aria-checked")).toBe("true");
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
    expect(item(/Opus 5/).getAttribute("aria-checked")).toBe("true");
  });

  it("stays hidden while the settings snapshot is still loading", () => {
    ready = false;
    render(<ModelPill />);
    expect(pill()).toBeNull();
  });

  it("writes the model through the same mutation Settings uses", () => {
    render(<ModelPill />);
    openMenu();
    fireEvent.click(item(/Sonnet 5/));
    expect(save).toHaveBeenCalledWith("model", "claude-sonnet-5");
  });

  it("writes the thinking level under the daemon's own key", () => {
    render(<ModelPill />);
    openMenu();
    fireEvent.click(item("High"));
    expect(save).toHaveBeenCalledWith("llm.thinking", "high");
  });

  it("switches the mode under the daemon's own key", () => {
    stored["conversation.collaboration_mode"] = "execute";
    render(<ModelPill />);
    openMenu();
    fireEvent.click(item("Plan"));
    expect(save).toHaveBeenCalledWith("conversation.collaboration_mode", "plan");
  });

  it("says on the pill when plan mode is on, at every width", () => {
    stored["conversation.collaboration_mode"] = "execute";
    render(<ModelPill />);
    expect(pill()!.textContent).not.toContain("Plan");
    cleanup();

    stored["conversation.collaboration_mode"] = "plan";
    device.mobile = true;
    render(<ModelPill />);
    expect(pill()!.textContent).toContain("Plan");
    expect(pill()!.getAttribute("aria-describedby")).toBeTruthy();
  });

  it("closes the menu once a choice is made", () => {
    render(<ModelPill />);
    openMenu();
    fireEvent.click(item(/Sonnet 5/));
    expect(queryItem(/Sonnet 5/)).toBeNull();
  });

  it("offers only the model in force for a provider with no shortlist", () => {
    stored.provider = "openrouter";
    stored.model = "some-vendor/some-model";
    render(<ModelPill />);
    openMenu();
    // No opinion on this provider's models, and the other provider's list is
    // not this one's to offer.
    const models = within(screen.getByRole("group", { name: "Model" }));
    expect(models.getAllByRole("menuitemradio").map((m) => m.textContent)).toEqual([
      "some-vendor/some-model",
    ]);
    expect(
      within(screen.getByRole("group", { name: "Thinking" })).getAllByRole("menuitemradio"),
    ).toHaveLength(5);
  });

  it("lists the provider's models in the catalog's order, strongest first, with their notes", () => {
    render(<ModelPill />);
    openMenu();
    const models = within(screen.getByRole("group", { name: "Model" }));
    expect(models.getAllByRole("menuitemradio").map((m) => m.textContent)).toEqual([
      "Opus 5",
      "Sonnet 5balanced",
      "Haiku 4.5fast, cheap",
    ]);
  });

  it("heads the list with a model set by hand outside the shortlist, ticked", () => {
    stored.model = "claude-opus-5-preview";
    render(<ModelPill />);
    openMenu();
    const models = within(screen.getByRole("group", { name: "Model" })).getAllByRole(
      "menuitemradio",
    );
    expect(models[0]!.textContent).toBe("claude-opus-5-preview");
    expect(models[0]!.getAttribute("aria-checked")).toBe("true");
    expect(models.filter((m) => m.getAttribute("aria-checked") === "true")).toHaveLength(1);
  });

  it("says what the thinking level in force does", () => {
    render(<ModelPill />);
    openMenu();
    expect(item("Medium").getAttribute("aria-checked")).toBe("true");
    expect(screen.getByText("Thinks it through.")).toBeTruthy();
  });

  it("opens on the model in force and walks every choice with the arrow keys", () => {
    stored.model = "claude-sonnet-5";
    render(<ModelPill />);
    openMenu();
    expect(document.activeElement).toBe(item(/Sonnet 5/));
    const menu = screen.getByRole("menu");
    fireEvent.keyDown(menu, { key: "ArrowDown" });
    expect(document.activeElement).toBe(item(/Haiku/));
    // Past the last model into the thinking levels, on through the modes,
    // then round to the top.
    fireEvent.keyDown(menu, { key: "ArrowDown" });
    expect(document.activeElement).toBe(item("Off"));
    fireEvent.keyDown(menu, { key: "End" });
    expect(document.activeElement).toBe(item("Plan"));
    fireEvent.keyDown(menu, { key: "ArrowUp" });
    expect(document.activeElement).toBe(item("Execute"));
    fireEvent.keyDown(menu, { key: "ArrowUp" });
    expect(document.activeElement).toBe(item("Adaptive"));
    fireEvent.keyDown(menu, { key: "End" });
    fireEvent.keyDown(menu, { key: "ArrowRight" });
    expect(document.activeElement).toBe(item(/Opus 5/));
    fireEvent.keyDown(menu, { key: "Home" });
    expect(document.activeElement).toBe(item(/Opus 5/));
  });

  it("closes without a write when the choice in force is picked again", () => {
    render(<ModelPill />);
    openMenu();
    fireEvent.click(item(/Opus 5/));
    expect(screen.queryByRole("menu")).toBeNull();
    openMenu();
    fireEvent.click(item("Medium"));
    expect(screen.queryByRole("menu")).toBeNull();
    expect(save).not.toHaveBeenCalled();
  });

  it("closes on an outside click without choosing anything", () => {
    render(<ModelPill />);
    openMenu();
    fireEvent.pointerDown(document.body);
    expect(queryItem(/Sonnet 5/)).toBeNull();
    expect(save).not.toHaveBeenCalled();
  });

  it("says so when the daemon refuses the change", () => {
    // The menu closes on click, so without this the pill would keep naming the
    // old model with nothing anywhere to say the switch did not take.
    saveError = { message: "connection refused" };
    render(<ModelPill />);
    expect(screen.getByRole("alert").textContent).toContain("Could not save the change");
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
      close: () => void fireEvent.click(item(/Sonnet 5/)),
    },
    { name: "Escape closes the menu", close: () => act(() => void dispatchKeybind("cancel-turn")) },
  ])("hands the cursor back where it was once $name", ({ close }) => {
    const box = midSentence();
    expect(document.activeElement).not.toBe(box);

    close();

    expect(queryItem(/Sonnet 5/)).toBeNull();
    expect(document.activeElement).toBe(box);
    expect([box.selectionStart, box.selectionEnd]).toEqual([8, 13]);
    expect(box.value).toBe("compare these two");
  });

  it("leaves the keyboard down on a phone once a model is picked", () => {
    device.mobile = true;
    const box = midSentence();

    fireEvent.click(item(/Sonnet 5/));

    expect(queryItem(/Sonnet 5/)).toBeNull();
    expect(document.activeElement).not.toBe(box);
  });
});
