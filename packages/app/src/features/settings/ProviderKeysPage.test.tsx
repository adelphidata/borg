import { describe, expect, it } from "vitest";
import { act, fireEvent, screen, waitFor } from "@testing-library/react";
import { Route, Routes } from "react-router-dom";
import { BorgError, GrpcStatus } from "@borg/client";
import { INVALID_ARGUMENT, fakeDaemon, renderApp } from "../../test/harness";
import { setupStatus } from "../../test/setupStatus";
import { ProviderKeysPage, describeKey } from "./ProviderKeysPage";

const credential = (over: Record<string, unknown> = {}) => ({
  id: "c1",
  label: "primary",
  provider: "openrouter",
  priority: 0,
  sourceKind: "manual",
  sourceValue: "",
  fingerprint: "sk-…ab12",
  useCount: 2,
  status: "ok",
  ...over,
});

const EMPTY = {
  ListCredentials: { providers: [] },
  ListApiKeys: { entries: [] },
  GetSetupStatus: setupStatus(),
};

const withPool = (entries: unknown[], strategy = "fill_first") => ({
  ...EMPTY,
  ListCredentials: {
    providers: [{ provider: "openrouter", strategy, cooldownSecs: 60, entries }],
  },
});

function mount(route: string, daemon: ReturnType<typeof fakeDaemon>) {
  return renderApp(
    <Routes>
      <Route path="/settings/provider/:name" element={<ProviderKeysPage />} />
    </Routes>,
    { daemon, route },
  );
}

const click = (el: HTMLElement) => act(() => el.click());

describe("ProviderKeysPage", () => {
  it("invites a key for a provider that has none", async () => {
    const daemon = fakeDaemon(EMPTY);
    mount("/settings/provider/anthropic", daemon);
    await waitFor(() => expect(screen.getByText("Not connected yet.")).toBeTruthy());
    expect(screen.getByText("Add a key")).toBeTruthy();
  });

  it("says a key found on this computer is already in use, and where", async () => {
    const daemon = fakeDaemon({
      ...EMPTY,
      GetSetupStatus: setupStatus({
        detected: [{ provider: "anthropic", source: "dotenv", detail: "~/.borg/.env" }],
      }),
    });
    mount("/settings/provider/anthropic", daemon);
    await waitFor(() => expect(screen.getByText("Connected")).toBeTruthy());
    expect(screen.getByText("~/.borg/.env")).toBeTruthy();
    expect(screen.getByText("Replace the key")).toBeTruthy();
    expect(screen.queryByText("Add a key")).toBeNull();
  });

  it("lists each pooled key with where it came from", async () => {
    const daemon = fakeDaemon(
      withPool([credential({ sourceKind: "env", sourceValue: "OPENROUTER_API_KEY" })]),
    );
    mount("/settings/provider/openrouter", daemon);
    await waitFor(() => expect(screen.getByText("primary")).toBeTruthy());
    expect(screen.getByText(/from OPENROUTER_API_KEY/)).toBeTruthy();
    expect(screen.getByText(/used 2 times/)).toBeTruthy();
  });

  it("names a config slot as a key, not as a setting path", async () => {
    const daemon = fakeDaemon(
      withPool([
        credential({
          label: "llm.api_keys[0]",
          sourceKind: "config",
          sourceValue: "llm.api_keys[0]",
        }),
      ]),
    );
    mount("/settings/provider/openrouter", daemon);
    await waitFor(() => expect(screen.getByText("Key 1")).toBeTruthy());
    expect(screen.queryByText(/llm\.api_keys/)).toBeNull();
  });

  it("says why a key is not usable", async () => {
    const daemon = fakeDaemon(withPool([credential({ status: "exhausted" })]));
    mount("/settings/provider/openrouter", daemon);
    await waitFor(() => expect(screen.getByText(/· exhausted/)).toBeTruthy());
  });

  it("hides a removed key rather than listing a credential that is gone", async () => {
    const daemon = fakeDaemon(
      withPool([credential({ status: "removed" }), credential({ id: "c2", label: "spare" })]),
    );
    mount("/settings/provider/openrouter", daemon);
    await waitFor(() => expect(screen.getByText("spare")).toBeTruthy());
    expect(screen.queryByText("primary")).toBeNull();
  });

  it("saves a typed key and clears the box", async () => {
    // The key must not linger in the DOM after it has been handed over.
    const daemon = fakeDaemon({ ...EMPTY, SetApiKey: {} });
    mount("/settings/provider/anthropic", daemon);
    await waitFor(() => expect(screen.getByPlaceholderText("API key")).toBeTruthy());

    fireEvent.change(screen.getByPlaceholderText("API key"), { target: { value: "  sk-live  " } });
    click(screen.getByText("Save"));
    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "SetApiKey")?.request).toEqual({
        provider: "anthropic",
        literal: "sk-live",
      }),
    );
    await waitFor(() => expect(screen.getByPlaceholderText("API key")).toHaveProperty("value", ""));
  });

  it("saves on Enter", async () => {
    const daemon = fakeDaemon({ ...EMPTY, SetApiKey: {} });
    mount("/settings/provider/anthropic", daemon);
    await waitFor(() => expect(screen.getByPlaceholderText("API key")).toBeTruthy());

    fireEvent.change(screen.getByPlaceholderText("API key"), { target: { value: "sk-live" } });
    fireEvent.keyDown(screen.getByPlaceholderText("API key"), { key: "Enter" });
    await waitFor(() => expect(daemon.calls.some((c) => c.method === "SetApiKey")).toBe(true));
  });

  it("does not send an empty key", async () => {
    const daemon = fakeDaemon({ ...EMPTY, SetApiKey: {} });
    mount("/settings/provider/anthropic", daemon);
    await waitFor(() => expect(screen.getByText("Save")).toBeTruthy());

    click(screen.getByText("Save"));
    expect(daemon.calls.some((c) => c.method === "SetApiKey")).toBe(false);
  });

  it("keeps the key in the box when the daemon rejects it", async () => {
    const daemon = fakeDaemon({
      ...EMPTY,
      SetApiKey: new BorgError(INVALID_ARGUMENT, "key looks malformed"),
    });
    mount("/settings/provider/anthropic", daemon);
    await waitFor(() => expect(screen.getByPlaceholderText("API key")).toBeTruthy());

    fireEvent.change(screen.getByPlaceholderText("API key"), { target: { value: "nope" } });
    click(screen.getByText("Save"));
    await waitFor(() => expect(screen.getByText("key looks malformed")).toBeTruthy());
    expect(screen.getByPlaceholderText("API key")).toHaveProperty("value", "nope");
  });

  it("says where a saved key lives, and can forget it", async () => {
    const daemon = fakeDaemon({
      ...EMPTY,
      ListApiKeys: { entries: [{ provider: "anthropic", backend: "keychain" }] },
      UnsetApiKey: {},
    });
    mount("/settings/provider/anthropic", daemon);
    await waitFor(() => expect(screen.getByText("Stored in the keychain")).toBeTruthy());
    // With a key saved, the field replaces rather than adds.
    expect(screen.getByText("Replace the key")).toBeTruthy();

    click(screen.getByText("Forget"));
    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "UnsetApiKey")?.request).toEqual({
        provider: "anthropic",
      }),
    );
  });

  it("names the daemon's own store when the key is not in a keychain", async () => {
    const daemon = fakeDaemon({
      ...EMPTY,
      ListApiKeys: { entries: [{ provider: "anthropic", backend: "literal" }] },
    });
    mount("/settings/provider/anthropic", daemon);
    await waitFor(() => expect(screen.getByText("Stored with the daemon")).toBeTruthy());
  });

  it("removes one key from the pool", async () => {
    const daemon = fakeDaemon({ ...withPool([credential()]), RemoveCredential: {} });
    mount("/settings/provider/openrouter", daemon);
    await waitFor(() => expect(screen.getByText("Remove")).toBeTruthy());

    click(screen.getByText("Remove"));
    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "RemoveCredential")?.request).toEqual({
        provider: "openrouter",
        target: "c1",
      }),
    );
  });

  it("offers rotation only where there is more than one key to rotate", async () => {
    const daemon = fakeDaemon(withPool([credential()]));
    mount("/settings/provider/openrouter", daemon);
    await waitFor(() => expect(screen.getByText("primary")).toBeTruthy());
    expect(screen.queryByText("Rotation")).toBeNull();
  });

  it("changes the rotation strategy", async () => {
    const daemon = fakeDaemon({
      ...withPool([credential(), credential({ id: "c2", label: "spare" })]),
      SetCredentialStrategy: {},
    });
    mount("/settings/provider/openrouter", daemon);
    await waitFor(() => expect(screen.getByText("Rotation")).toBeTruthy());

    click(screen.getByText("round robin"));
    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "SetCredentialStrategy")?.request).toEqual({
        provider: "openrouter",
        strategy: "round_robin",
      }),
    );
  });

  it("does not rewrite the strategy already in force", async () => {
    const daemon = fakeDaemon({
      ...withPool([credential(), credential({ id: "c2" })]),
      SetCredentialStrategy: {},
    });
    mount("/settings/provider/openrouter", daemon);
    await waitFor(() => expect(screen.getByText("Rotation")).toBeTruthy());

    click(screen.getByText("fill first"));
    expect(daemon.calls.some((c) => c.method === "SetCredentialStrategy")).toBe(false);
  });
});

const IME_ENTERS: [string, { key: string; isComposing?: boolean; keyCode?: number }][] = [
  ["while a word is still being composed", { key: "Enter", isComposing: true }],
  ["that confirms a candidate after composition ended", { key: "Enter", keyCode: 229 }],
];

describe("ProviderKeysPage and an input method", () => {
  it.each(IME_ENTERS)("does not save the key on an Enter %s", async (_name, init) => {
    const daemon = fakeDaemon({ ...EMPTY, SetApiKey: {} });
    mount("/settings/provider/anthropic", daemon);
    await waitFor(() => expect(screen.getByPlaceholderText("API key")).toBeTruthy());

    fireEvent.change(screen.getByPlaceholderText("API key"), { target: { value: "sk-live" } });
    fireEvent.keyDown(screen.getByPlaceholderText("API key"), init);
    await act(async () => {
      await new Promise((resolve) => setTimeout(resolve, 20));
    });

    expect(daemon.calls.some((c) => c.method === "SetApiKey")).toBe(false);
    expect(screen.getByPlaceholderText("API key")).toHaveProperty("value", "sk-live");
  });
});
