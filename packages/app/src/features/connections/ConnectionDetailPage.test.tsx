import { describe, expect, it } from "vitest";
import { act, fireEvent, screen, waitFor } from "@testing-library/react";
import { Route, Routes } from "react-router-dom";
import { BorgError, GrpcStatus } from "@borg/client";
import { INVALID_ARGUMENT, fakeDaemon, renderApp } from "../../test/harness";
import { setupStatus } from "../../test/setupStatus";
import { ConnectionDetailPage } from "./ConnectionDetailPage";

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
  ListApprovedSenders: { senders: [] },
  ListPairingRequests: { requests: [] },
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
      <Route path="/connections/:kind/:name" element={<ConnectionDetailPage />} />
    </Routes>,
    { daemon, route },
  );
}

const click = (el: HTMLElement) => act(() => el.click());

describe("ConnectionDetailPage for a provider", () => {
  it("invites a key for a provider that has none", async () => {
    const daemon = fakeDaemon(EMPTY);
    mount("/connections/provider/anthropic", daemon);
    await waitFor(() => expect(screen.getByText("Claude, straight from the source.")).toBeTruthy());
    expect(screen.getByText("Add a key")).toBeTruthy();
  });

  it("says a key found on this computer is already in use, and where", async () => {
    const daemon = fakeDaemon({
      ...EMPTY,
      GetSetupStatus: setupStatus({
        detected: [{ provider: "anthropic", source: "dotenv", detail: "~/.borg/.env" }],
      }),
    });
    mount("/connections/provider/anthropic", daemon);
    await waitFor(() => expect(screen.getByText("Connected")).toBeTruthy());
    expect(screen.getByText("~/.borg/.env")).toBeTruthy();
    expect(screen.getByText("Replace the key")).toBeTruthy();
    expect(screen.queryByText("Add a key")).toBeNull();
  });

  it("lists each pooled key with where it came from", async () => {
    const daemon = fakeDaemon(
      withPool([credential({ sourceKind: "env", sourceValue: "OPENROUTER_API_KEY" })]),
    );
    mount("/connections/provider/openrouter", daemon);
    await waitFor(() => expect(screen.getByText("primary")).toBeTruthy());
    expect(screen.getByText(/from OPENROUTER_API_KEY/)).toBeTruthy();
    expect(screen.getByText(/used 2 times/)).toBeTruthy();
  });

  it("says why a key is not usable", async () => {
    const daemon = fakeDaemon(withPool([credential({ status: "exhausted" })]));
    mount("/connections/provider/openrouter", daemon);
    await waitFor(() => expect(screen.getByText(/· exhausted/)).toBeTruthy());
  });

  it("hides a removed key rather than listing a credential that is gone", async () => {
    const daemon = fakeDaemon(
      withPool([credential({ status: "removed" }), credential({ id: "c2", label: "spare" })]),
    );
    mount("/connections/provider/openrouter", daemon);
    await waitFor(() => expect(screen.getByText("spare")).toBeTruthy());
    expect(screen.queryByText("primary")).toBeNull();
  });

  it("saves a typed key and clears the box", async () => {
    // The key must not linger in the DOM after it has been handed over.
    const daemon = fakeDaemon({ ...EMPTY, SetApiKey: {} });
    mount("/connections/provider/anthropic", daemon);
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
    mount("/connections/provider/anthropic", daemon);
    await waitFor(() => expect(screen.getByPlaceholderText("API key")).toBeTruthy());

    fireEvent.change(screen.getByPlaceholderText("API key"), { target: { value: "sk-live" } });
    fireEvent.keyDown(screen.getByPlaceholderText("API key"), { key: "Enter" });
    await waitFor(() => expect(daemon.calls.some((c) => c.method === "SetApiKey")).toBe(true));
  });

  it("does not send an empty key", async () => {
    const daemon = fakeDaemon({ ...EMPTY, SetApiKey: {} });
    mount("/connections/provider/anthropic", daemon);
    await waitFor(() => expect(screen.getByText("Save")).toBeTruthy());

    click(screen.getByText("Save"));
    expect(daemon.calls.some((c) => c.method === "SetApiKey")).toBe(false);
  });

  it("keeps the key in the box when the daemon rejects it", async () => {
    const daemon = fakeDaemon({
      ...EMPTY,
      SetApiKey: new BorgError(INVALID_ARGUMENT, "key looks malformed"),
    });
    mount("/connections/provider/anthropic", daemon);
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
    mount("/connections/provider/anthropic", daemon);
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
    mount("/connections/provider/anthropic", daemon);
    await waitFor(() => expect(screen.getByText("Stored with the daemon")).toBeTruthy());
  });

  it("removes one key from the pool", async () => {
    const daemon = fakeDaemon({ ...withPool([credential()]), RemoveCredential: {} });
    mount("/connections/provider/openrouter", daemon);
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
    mount("/connections/provider/openrouter", daemon);
    await waitFor(() => expect(screen.getByText("primary")).toBeTruthy());
    expect(screen.queryByText("Rotation")).toBeNull();
  });

  it("changes the rotation strategy", async () => {
    const daemon = fakeDaemon({
      ...withPool([credential(), credential({ id: "c2", label: "spare" })]),
      SetCredentialStrategy: {},
    });
    mount("/connections/provider/openrouter", daemon);
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
    mount("/connections/provider/openrouter", daemon);
    await waitFor(() => expect(screen.getByText("Rotation")).toBeTruthy());

    click(screen.getByText("fill first"));
    expect(daemon.calls.some((c) => c.method === "SetCredentialStrategy")).toBe(false);
  });
});

describe("ConnectionDetailPage for a channel", () => {
  const channel = (over: Record<string, unknown> = {}) => ({
    ...EMPTY,
    ListApprovedSenders: {
      senders: [{ id: 1, channelName: "telegram", senderId: "42", displayName: "Ada" }],
    },
    ...over,
  });

  it("lists approved senders by name, with the id underneath", async () => {
    mount("/connections/channel/telegram", fakeDaemon(channel()));
    await waitFor(() => expect(screen.getByText("Ada")).toBeTruthy());
    expect(screen.getByText("42")).toBeTruthy();
    expect(screen.getByText("1 approved sender")).toBeTruthy();
  });

  it("falls back to the sender id when there is no name", async () => {
    const daemon = fakeDaemon(
      channel({
        ListApprovedSenders: {
          senders: [{ id: 1, channelName: "telegram", senderId: "42", displayName: "" }],
        },
      }),
    );
    mount("/connections/channel/telegram", daemon);
    await waitFor(() => expect(screen.getAllByText("42").length).toBeGreaterThan(0));
  });

  it("approves a pairing request by its code", async () => {
    const daemon = fakeDaemon({
      ...channel({
        ListPairingRequests: {
          requests: [
            {
              id: "p1",
              channelName: "telegram",
              senderId: "77",
              status: "pending",
              code: "hash-of-code",
              displayName: "Bob",
            },
          ],
        },
      }),
      ApprovePairing: {},
    });
    mount("/connections/channel/telegram", daemon);
    await waitFor(() => expect(screen.getByText("Waiting for approval")).toBeTruthy());
    expect(screen.getByText("Bob")).toBeTruthy();

    click(screen.getByText("Approve"));
    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "ApprovePairing")?.request).toEqual({
        channelName: "telegram",
        code: "hash-of-code",
      }),
    );
  });

  it("revokes an approved sender", async () => {
    const daemon = fakeDaemon({ ...channel(), RevokeApprovedSender: {} });
    mount("/connections/channel/telegram", daemon);
    await waitFor(() => expect(screen.getByText("Revoke")).toBeTruthy());

    click(screen.getByText("Revoke"));
    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "RevokeApprovedSender")?.request).toEqual({
        channelName: "telegram",
        senderId: "42",
      }),
    );
  });

  it("surfaces a failed approval", async () => {
    const daemon = fakeDaemon({
      ...channel({
        ListPairingRequests: {
          requests: [
            { id: "p1", channelName: "telegram", senderId: "77", status: "pending", code: "x" },
          ],
        },
      }),
      ApprovePairing: new BorgError(GrpcStatus.NotFound, "code expired"),
    });
    mount("/connections/channel/telegram", daemon);
    await waitFor(() => expect(screen.getByText("Approve")).toBeTruthy());

    click(screen.getByText("Approve"));
    await waitFor(() => expect(screen.getByText("code expired")).toBeTruthy());
  });

  it("says so for a channel that is not connected", async () => {
    mount("/connections/channel/nowhere", fakeDaemon(EMPTY));
    await waitFor(() => expect(screen.getByText("That connection doesn’t exist.")).toBeTruthy());
  });

  it("says nobody is approved yet rather than showing an empty list", async () => {
    const daemon = fakeDaemon({
      ...EMPTY,
      ListPairingRequests: {
        requests: [
          { id: "p1", channelName: "telegram", senderId: "77", status: "pending", code: "x" },
        ],
      },
    });
    mount("/connections/channel/telegram", daemon);
    await waitFor(() => expect(screen.getByText("No one is approved yet.")).toBeTruthy());
  });
});

/** The two shapes an input method's Enter takes; neither one means "submit". */
const IME_ENTERS: [string, { key: string; isComposing?: boolean; keyCode?: number }][] = [
  ["while a word is still being composed", { key: "Enter", isComposing: true }],
  ["that confirms a candidate after composition ended", { key: "Enter", keyCode: 229 }],
];

describe("ConnectionDetailPage and an input method", () => {
  it.each(IME_ENTERS)("does not save the key on an Enter %s", async (_name, init) => {
    const daemon = fakeDaemon({ ...EMPTY, SetApiKey: {} });
    mount("/connections/provider/anthropic", daemon);
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
