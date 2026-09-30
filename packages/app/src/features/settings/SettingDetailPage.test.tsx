import { describe, expect, it } from "vitest";
import { act, fireEvent, screen, waitFor } from "@testing-library/react";
import { Route, Routes } from "react-router-dom";
import { STORAGE_KEYS, ThemeProvider } from "@borg/ui";
import { BorgError, GrpcStatus } from "@borg/client";
import { INVALID_ARGUMENT, fakeDaemon, renderApp } from "../../test/harness";
import { setupStatus } from "../../test/setupStatus";
import { SettingsProvider } from "../../lib/settings";
import { SettingDetailPage } from "./SettingDetailPage";

const entry = (key: string, value: string, category = "default") => ({
  key,
  value,
  category,
  label: "",
  kind: "",
  group: "",
});

const REGISTRY = [
  entry("llm.thinking", "medium"),
  entry("provider", "anthropic"),
  entry("model", "claude-opus-5"),
  entry("agents.max_depth", "3", "db"),
  entry("gateway.port", "7842"),
];

const DAEMON = { GetSettings: { entries: REGISTRY }, GetSetupStatus: setupStatus() };

function mount(route: string, daemon = fakeDaemon(DAEMON)) {
  return renderApp(
    <Routes>
      <Route path="/settings/:id" element={<SettingDetailPage />} />
      <Route path="/settings/key/:key" element={<SettingDetailPage />} />
    </Routes>,
    {
      daemon,
      route,
      wrapper: (children) => (
        <ThemeProvider>
          <SettingsProvider>{children}</SettingsProvider>
        </ThemeProvider>
      ),
    },
  );
}

const click = (element: HTMLElement) => act(() => element.click());

describe("SettingDetailPage for a curated choice", () => {
  it("marks the option that is currently in force", async () => {
    mount("/settings/thinking");
    await waitFor(() => expect(screen.getByText("Thinking level")).toBeTruthy());
    const selected = screen.getByRole("img", { name: "Selected" });
    const row = selected.closest("button");
    expect(row?.textContent).toContain("Medium");
    expect(row?.getAttribute("aria-pressed")).toBe("true");
    expect(screen.getByRole("button", { name: /^High/ }).getAttribute("aria-pressed")).toBe(
      "false",
    );
  });

  it("writes the picked option straight through", async () => {
    const daemon = fakeDaemon({ GetSettings: { entries: REGISTRY }, SetSetting: {} });
    mount("/settings/thinking", daemon);
    await waitFor(() => expect(screen.getByText("High")).toBeTruthy());

    click(screen.getByText("High"));
    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "SetSetting")?.request).toEqual({
        key: "llm.thinking",
        value: "high",
      }),
    );
  });

  it("does not write when the option picked is the one already set", async () => {
    const daemon = fakeDaemon({ GetSettings: { entries: REGISTRY }, SetSetting: {} });
    mount("/settings/thinking", daemon);
    await waitFor(() => expect(screen.getByText("Medium")).toBeTruthy());

    click(screen.getByText("Medium"));
    expect(daemon.calls.some((c) => c.method === "SetSetting")).toBe(false);
  });

  it("shows why the daemon rejected a choice", async () => {
    const daemon = fakeDaemon({
      GetSettings: { entries: REGISTRY },
      SetSetting: new BorgError(INVALID_ARGUMENT, "unknown thinking level"),
    });
    mount("/settings/thinking", daemon);
    await waitFor(() => expect(screen.getByText("High")).toBeTruthy());

    click(screen.getByText("High"));
    await waitFor(() => expect(screen.getByText("unknown thinking level")).toBeTruthy());
  });
});

describe("SettingDetailPage for a free-text value", () => {
  it("opens with the effective value and saves an edit", async () => {
    const daemon = fakeDaemon({ GetSettings: { entries: REGISTRY }, SetSetting: {} });
    mount("/settings/key/gateway.port", daemon);
    await waitFor(() => expect(screen.getByDisplayValue("7842")).toBeTruthy());

    fireEvent.change(screen.getByDisplayValue("7842"), { target: { value: "9000" } });
    click(screen.getByText("Save"));
    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "SetSetting")?.request).toEqual({
        key: "gateway.port",
        value: "9000",
      }),
    );
  });

  it("saves on Enter, the way a single-field form should", async () => {
    const daemon = fakeDaemon({ GetSettings: { entries: REGISTRY }, SetSetting: {} });
    mount("/settings/key/gateway.port", daemon);
    await waitFor(() => expect(screen.getByDisplayValue("7842")).toBeTruthy());

    fireEvent.keyDown(screen.getByDisplayValue("7842"), { key: "Enter" });
    await waitFor(() => expect(daemon.calls.some((c) => c.method === "SetSetting")).toBe(true));
  });

  it("follows the daemon's normalised value after a save", async () => {
    // The daemon may store something other than what was typed.
    const daemon = fakeDaemon({ GetSettings: { entries: REGISTRY }, SetSetting: {} });
    mount("/settings/key/gateway.port", daemon);
    await waitFor(() => expect(screen.getByDisplayValue("7842")).toBeTruthy());

    daemon.answer("GetSettings", {
      entries: [...REGISTRY.slice(0, 3), entry("gateway.port", "9001")],
    });
    fireEvent.change(screen.getByDisplayValue("7842"), { target: { value: " 9001 " } });
    click(screen.getByText("Save"));
    await waitFor(() => expect(screen.getByDisplayValue("9001")).toBeTruthy());
  });

  it("offers a reset only for a key that was actually overridden", async () => {
    // Resetting a compiled default would be a control that does nothing.
    mount("/settings/key/gateway.port");
    await waitFor(() => expect(screen.getByDisplayValue("7842")).toBeTruthy());
    expect(screen.queryByText("Reset to default")).toBeNull();
  });

  it("clears an overridden key through Unset", async () => {
    const daemon = fakeDaemon({ GetSettings: { entries: REGISTRY }, UnsetSetting: {} });
    mount("/settings/key/agents.max_depth", daemon);
    await waitFor(() => expect(screen.getByText("Reset to default")).toBeTruthy());

    click(screen.getByText("Reset to default"));
    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "UnsetSetting")?.request).toEqual({
        key: "agents.max_depth",
      }),
    );
  });

  it("decodes a key that had to be escaped into the URL", async () => {
    mount(`/settings/key/${encodeURIComponent("agents.max_depth")}`);
    await waitFor(() => expect(screen.getByDisplayValue("3")).toBeTruthy());
  });
});

describe("SettingDetailPage edge cases", () => {
  it("says so for a route segment that names no setting", () => {
    mount("/settings/nonsense");
    expect(screen.getByText("That setting doesn’t exist.")).toBeTruthy();
  });

  it("says so for a key the registry does not hold", async () => {
    mount("/settings/key/not.a.real.key");
    await waitFor(() => expect(screen.getByText("That setting doesn’t exist.")).toBeTruthy());
  });

  it("reports a failed registry load rather than an empty editor", async () => {
    const daemon = fakeDaemon({
      GetSettings: new BorgError(GrpcStatus.Unavailable, "daemon down"),
    });
    mount("/settings/key/gateway.port", daemon);
    await waitFor(() => expect(screen.getByText("Could not load settings.")).toBeTruthy());
  });
});

describe("SettingDetailPage for provider and model", () => {
  it("offers the daemon's providers by name and writes the id", async () => {
    const daemon = fakeDaemon({ ...DAEMON, SetSetting: {} });
    mount("/settings/provider", daemon);
    await waitFor(() => expect(screen.getByText("OpenRouter")).toBeTruthy());
    const selected = screen.getByRole("img", { name: "Selected" });
    expect(selected.closest("button")?.textContent).toContain("Anthropic");

    click(screen.getByText("OpenRouter"));
    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "SetSetting")?.request).toEqual({
        key: "provider",
        value: "openrouter",
      }),
    );
  });

  it("moves to the new provider's default model when it does not list the current one", async () => {
    // claude-opus-5 is an Anthropic id; OpenRouter would be sent a model it
    // does not carry under that name.
    const daemon = fakeDaemon({ ...DAEMON, SetSetting: {} });
    mount("/settings/provider", daemon);
    await waitFor(() => expect(screen.getByText("OpenRouter")).toBeTruthy());
    click(screen.getByText("OpenRouter"));
    await waitFor(() =>
      expect(daemon.calls.filter((c) => c.method === "SetSetting").map((c) => c.request)).toEqual([
        { key: "provider", value: "openrouter" },
        { key: "model", value: "deepseek/deepseek-v4-flash" },
      ]),
    );
  });

  it("leaves the model alone when the provider write fails", async () => {
    const daemon = fakeDaemon({
      ...DAEMON,
      SetSetting: new BorgError(GrpcStatus.Internal, "disk full"),
    });
    mount("/settings/provider", daemon);
    await waitFor(() => expect(screen.getByText("OpenRouter")).toBeTruthy());
    click(screen.getByText("OpenRouter"));
    await waitFor(() => expect(screen.getByText(/disk full/)).toBeTruthy());
    expect(daemon.calls.filter((c) => c.method === "SetSetting").map((c) => c.request)).toEqual([
      { key: "provider", value: "openrouter" },
    ]);
  });

  it("lists only the models of the provider in force", async () => {
    mount("/settings/model");
    await waitFor(() => expect(screen.getByText("Opus 5")).toBeTruthy());
    expect(screen.getByText("Sonnet 5")).toBeTruthy();
    expect(screen.queryByText("DeepSeek V4 Flash")).toBeNull();
  });

  it("keeps free text one step away, for an id the catalog does not carry", async () => {
    const daemon = fakeDaemon({ ...DAEMON, SetSetting: {} });
    mount("/settings/model", daemon);
    await waitFor(() => expect(screen.getByText("Something else…")).toBeTruthy());
    expect(screen.queryByDisplayValue("claude-opus-5")).toBeNull();

    click(screen.getByText("Something else…"));
    fireEvent.change(screen.getByDisplayValue("claude-opus-5"), {
      target: { value: "claude-opus-6-preview" },
    });
    click(screen.getByText("Save"));
    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "SetSetting")?.request).toEqual({
        key: "model",
        value: "claude-opus-6-preview",
      }),
    );
  });

  it("opens straight into text for a value the catalog does not list", async () => {
    const daemon = fakeDaemon({
      ...DAEMON,
      GetSettings: { entries: [entry("provider", "anthropic"), entry("model", "my-finetune")] },
    });
    mount("/settings/model", daemon);
    // The text editor can appear before the catalog lands (no options yet
    // means text); the way back exists only once the catalog has answered.
    await waitFor(() => expect(screen.getByText("Back to the list")).toBeTruthy());
    expect(screen.getByDisplayValue("my-finetune")).toBeTruthy();
    click(screen.getByText("Back to the list"));
    expect(screen.getByText("Opus 5")).toBeTruthy();
  });

  it("falls back to text when the catalog cannot answer", async () => {
    const daemon = fakeDaemon({
      GetSettings: { entries: REGISTRY },
      GetSetupStatus: new BorgError(GrpcStatus.Unavailable, "daemon down"),
    });
    mount("/settings/provider", daemon);
    await waitFor(() => expect(screen.getByDisplayValue("anthropic")).toBeTruthy());
    expect(screen.queryByText("Back to the list")).toBeNull();
  });

  it("says when the chosen provider has no key, and where to add one", async () => {
    mount("/settings/provider");
    await waitFor(() => expect(screen.getByText(/No key saved for Anthropic yet/)).toBeTruthy());
    expect((screen.getByText("Add one") as HTMLAnchorElement).getAttribute("href")).toBe(
      "/connections/provider/anthropic",
    );
  });

  it("stays quiet about keys once the daemon has one for that provider", async () => {
    const daemon = fakeDaemon({
      ...DAEMON,
      GetSetupStatus: setupStatus({
        detected: [{ provider: "anthropic", source: "stored", detail: "" }],
      }),
    });
    mount("/settings/provider", daemon);
    await waitFor(() => expect(screen.getByText("OpenRouter")).toBeTruthy());
    expect(screen.queryByText(/No key saved/)).toBeNull();
  });
});

describe("appearance, which never reaches the daemon", () => {
  it("applies a theme change immediately and stores it locally", async () => {
    const daemon = fakeDaemon({ GetSettings: { entries: REGISTRY } });
    mount("/settings/appearance", daemon);
    expect(screen.getByText("Changes apply straight away. Nothing to save.")).toBeTruthy();

    click(screen.getByText("Light"));
    await waitFor(() => expect(localStorage.getItem(STORAGE_KEYS.theme)).toBe("light"));
    // The theme must not become a daemon setting: it has to apply before paint.
    expect(daemon.calls.some((c) => c.method === "SetSetting")).toBe(false);
  });

  it("ignores a pick of the theme already in force", async () => {
    mount("/settings/appearance");
    click(screen.getByText("Light"));
    await waitFor(() => expect(localStorage.getItem(STORAGE_KEYS.theme)).toBe("light"));
    click(screen.getByText("Light"));
    expect(localStorage.getItem(STORAGE_KEYS.theme)).toBe("light");
  });
});

/** The two shapes an input method's Enter takes; neither one means "submit". */
const IME_ENTERS: [string, { key: string; isComposing?: boolean; keyCode?: number }][] = [
  ["while a word is still being composed", { key: "Enter", isComposing: true }],
  ["that confirms a candidate after composition ended", { key: "Enter", keyCode: 229 }],
];

describe("SettingDetailPage and an input method", () => {
  it.each(IME_ENTERS)("does not save on an Enter %s", async (_name, init) => {
    const daemon = fakeDaemon({ GetSettings: { entries: REGISTRY }, SetSetting: {} });
    mount("/settings/key/gateway.port", daemon);
    await waitFor(() => expect(screen.getByDisplayValue("7842")).toBeTruthy());

    fireEvent.keyDown(screen.getByDisplayValue("7842"), init);
    await act(async () => {
      await new Promise((resolve) => setTimeout(resolve, 20));
    });

    expect(daemon.calls.some((c) => c.method === "SetSetting")).toBe(false);
  });
});
