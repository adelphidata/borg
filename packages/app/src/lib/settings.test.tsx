import { describe, expect, it } from "vitest";
import { act, screen, waitFor } from "@testing-library/react";
import { BorgError, GrpcStatus } from "@borg/client";
import { INVALID_ARGUMENT, fakeDaemon, renderApp } from "../test/harness";
import { SettingsProvider, useSettings } from "./settings";

const ENTRIES = [
  {
    key: "heartbeat.enabled",
    value: "true",
    category: "db",
    label: "Let Borg act on its own",
    kind: "bool",
    group: "Autonomy",
  },
  {
    key: "llm.model",
    value: "claude-opus-5",
    category: "default",
    label: "",
    kind: "",
    group: "",
  },
];

function Probe() {
  const { settings, save, clear, saving, saveError, resetSaveError } = useSettings();
  return (
    <div>
      <span data-testid="state">{settings.state}</span>
      <span data-testid="heartbeat">
        {settings.data?.get("heartbeat.enabled")?.value ?? "none"}
      </span>
      <span data-testid="overridden">
        {String(settings.data?.get("heartbeat.enabled")?.overridden)}
      </span>
      <span data-testid="internal">{String(settings.data?.get("llm.model")?.overridden)}</span>
      <span data-testid="saving">{String(saving)}</span>
      <span data-testid="error">{saveError?.message ?? ""}</span>
      <button onClick={() => void save("heartbeat.enabled", "false")}>save</button>
      <button onClick={() => void clear("heartbeat.enabled")}>clear</button>
      <button onClick={resetSaveError}>reset</button>
    </div>
  );
}

const press = async (name: string) => {
  await act(async () => {
    screen.getByText(name).click();
  });
};

function mount(daemon = fakeDaemon({ GetSettings: { entries: ENTRIES } })) {
  const view = renderApp(<Probe />, {
    daemon,
    wrapper: (children) => <SettingsProvider>{children}</SettingsProvider>,
  });
  return view;
}

describe("SettingsProvider", () => {
  it("exposes the daemon's snapshot keyed by setting key", async () => {
    mount();
    await waitFor(() => expect(screen.getByTestId("state").textContent).toBe("ready"));
    expect(screen.getByTestId("heartbeat").textContent).toBe("true");
  });

  it("marks a DB override apart from a compiled default", async () => {
    // The distinction drives the "modified" mark and the reset control; getting
    // it backwards would offer to reset a value that was never changed.
    mount();
    await waitFor(() => expect(screen.getByTestId("state").textContent).toBe("ready"));
    expect(screen.getByTestId("overridden").textContent).toBe("true");
    expect(screen.getByTestId("internal").textContent).toBe("false");
  });

  it("writes a value and refreshes the snapshot from the daemon", async () => {
    // The refresh is the point: the daemon may normalise what it stored.
    const daemon = fakeDaemon({ GetSettings: { entries: ENTRIES }, SetSetting: {} });
    mount(daemon);
    await waitFor(() => expect(screen.getByTestId("state").textContent).toBe("ready"));

    daemon.answer("GetSettings", {
      entries: [{ ...ENTRIES[0], value: "false" }, ENTRIES[1]],
    });
    await press("save");
    await waitFor(() => expect(screen.getByTestId("heartbeat").textContent).toBe("false"));
    expect(daemon.calls.filter((c) => c.method === "SetSetting")).toEqual([
      { method: "SetSetting", request: { key: "heartbeat.enabled", value: "false" } },
    ]);
  });

  it("clears a value through Unset rather than writing an empty string", async () => {
    // Writing "" would store an override that shadows the compiled default.
    const daemon = fakeDaemon({ GetSettings: { entries: ENTRIES }, UnsetSetting: {} });
    mount(daemon);
    await waitFor(() => expect(screen.getByTestId("state").textContent).toBe("ready"));

    await press("clear");
    expect(daemon.calls.filter((c) => c.method === "UnsetSetting")).toEqual([
      { method: "UnsetSetting", request: { key: "heartbeat.enabled" } },
    ]);
    expect(daemon.calls.some((c) => c.method === "SetSetting")).toBe(false);
  });

  it("surfaces a rejected write instead of pretending it saved", async () => {
    const daemon = fakeDaemon({
      GetSettings: { entries: ENTRIES },
      SetSetting: new BorgError(INVALID_ARGUMENT, "value out of range"),
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByTestId("state").textContent).toBe("ready"));

    await press("save");
    expect(screen.getByTestId("error").textContent).toBe("value out of range");
    expect(screen.getByTestId("saving").textContent).toBe("false");
  });

  it("clears the error once the user edits again", async () => {
    const daemon = fakeDaemon({
      GetSettings: { entries: ENTRIES },
      SetSetting: new BorgError(INVALID_ARGUMENT, "nope"),
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByTestId("state").textContent).toBe("ready"));
    await press("save");
    expect(screen.getByTestId("error").textContent).toBe("nope");

    await press("reset");
    expect(screen.getByTestId("error").textContent).toBe("");
  });

  it("reports a failed snapshot rather than showing an empty settings list", async () => {
    const daemon = fakeDaemon({
      GetSettings: new BorgError(GrpcStatus.Unavailable, "daemon down"),
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByTestId("state").textContent).toBe("error"));
  });

  it("refuses to run outside the provider", () => {
    expect(() => renderApp(<Probe />)).toThrow(/SettingsProvider/);
  });
});
