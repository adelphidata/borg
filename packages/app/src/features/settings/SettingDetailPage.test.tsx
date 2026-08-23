import { describe, expect, it } from "vitest";
import { act, fireEvent, screen, waitFor } from "@testing-library/react";
import { Route, Routes } from "react-router-dom";
import { STORAGE_KEYS, ThemeProvider } from "@borg/ui";
import { BorgError, GrpcStatus } from "@borg/client";
import { INVALID_ARGUMENT, fakeDaemon, renderApp } from "../../test/harness";
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
  entry("model", "claude-opus-5"),
  entry("agents.max_depth", "3", "db"),
  entry("gateway.port", "7842"),
];

function mount(route: string, daemon = fakeDaemon({ GetSettings: { entries: REGISTRY } })) {
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
    expect(selected.closest("div")?.textContent).toContain("medium");
  });

  it("writes the picked option straight through", async () => {
    const daemon = fakeDaemon({ GetSettings: { entries: REGISTRY }, SetSetting: {} });
    mount("/settings/thinking", daemon);
    await waitFor(() => expect(screen.getByText("high")).toBeTruthy());

    click(screen.getByText("high"));
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
    await waitFor(() => expect(screen.getByText("medium")).toBeTruthy());

    click(screen.getByText("medium"));
    expect(daemon.calls.some((c) => c.method === "SetSetting")).toBe(false);
  });

  it("shows why the daemon rejected a choice", async () => {
    const daemon = fakeDaemon({
      GetSettings: { entries: REGISTRY },
      SetSetting: new BorgError(INVALID_ARGUMENT, "unknown thinking level"),
    });
    mount("/settings/thinking", daemon);
    await waitFor(() => expect(screen.getByText("high")).toBeTruthy());

    click(screen.getByText("high"));
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
