import { describe, expect, it, vi } from "vitest";
import { act, screen, waitFor } from "@testing-library/react";
import { Route, Routes, useLocation } from "react-router-dom";
import { BorgError, GrpcStatus } from "@borg/client";
import { installMatchMedia } from "@borg/ui/test/matchMedia";
import { fakeDaemon, renderApp } from "../../test/harness";
import { SETUP_CATALOG, setupStatus } from "../../test/setupStatus";
import { ConnectionsPage } from "./ConnectionsPage";
import { badgeOf, providerLabel } from "./useConnections";

const media = installMatchMedia(() => false);
const mobile = media.controller("(max-width: 700px)");

const credential = (over: Record<string, unknown> = {}) => ({
  id: "c1",
  label: "primary",
  provider: "openrouter",
  priority: 0,
  sourceKind: "env",
  sourceValue: "OPENROUTER_API_KEY",
  fingerprint: "ab12",
  useCount: 3,
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

function Where() {
  return <span data-testid="path">{useLocation().pathname}</span>;
}

function mount(daemon: ReturnType<typeof fakeDaemon>) {
  return renderApp(
    <>
      <ConnectionsPage />
      <Routes>
        <Route path="*" element={<Where />} />
      </Routes>
    </>,
    { daemon },
  );
}

describe("badgeOf", () => {
  it("takes the first two letters of a name", () => {
    expect(badgeOf("openrouter")).toBe("OP");
  });

  it("skips punctuation rather than badging it", () => {
    expect(badgeOf("claude-code")).toBe("CL");
    expect(badgeOf("_x")).toBe("X");
  });

  it("falls back rather than rendering an empty tile", () => {
    expect(badgeOf("---")).toBe("??");
    expect(badgeOf("")).toBe("??");
  });
});

describe("providerLabel", () => {
  it("uses the daemon catalog's display name", () => {
    expect(providerLabel(SETUP_CATALOG, "openrouter")).toBe("OpenRouter");
  });

  it("passes an unknown provider through rather than dropping it", () => {
    expect(providerLabel(SETUP_CATALOG, "mystery")).toBe("mystery");
  });
});

describe("ConnectionsPage", () => {
  it("lists a configured provider with its key count and strategy", async () => {
    const daemon = fakeDaemon({
      ...EMPTY,
      ListCredentials: {
        providers: [
          {
            provider: "openrouter",
            strategy: "fill_first",
            cooldownSecs: 60,
            entries: [credential(), credential({ id: "c2" })],
          },
        ],
      },
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("OpenRouter")).toBeTruthy());
    expect(screen.getByText("2 keys · fill first")).toBeTruthy();
    // The tile opens its detail page, so the keyboard has to reach it.
    expect(screen.getByRole("button", { name: /OpenRouter/ })).toBeTruthy();
  });

  it("counts a persisted key as connected before the pool has seen it", async () => {
    // SetApiKey writes to the DB; the runtime pool only picks it up on restart.
    const daemon = fakeDaemon({
      ...EMPTY,
      ListApiKeys: { entries: [{ provider: "anthropic", backend: "keychain" }] },
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Anthropic")).toBeTruthy());
    expect(screen.getByText("key saved")).toBeTruthy();
  });

  it("ignores an unset key rather than showing a provider that has none", async () => {
    const daemon = fakeDaemon({
      ...EMPTY,
      ListApiKeys: { entries: [{ provider: "anthropic", backend: "unset" }] },
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Add a model provider")).toBeTruthy());
    expect(screen.queryByText("key saved")).toBeNull();
  });

  it("flags a provider whose every key is exhausted", async () => {
    // Keys present but none usable is the state that silently stops Borg.
    const daemon = fakeDaemon({
      ...EMPTY,
      ListCredentials: {
        providers: [
          {
            provider: "openrouter",
            strategy: "fill_first",
            cooldownSecs: 60,
            entries: [credential({ status: "exhausted" })],
          },
        ],
      },
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("OpenRouter")).toBeTruthy());
    expect(screen.getAllByText("1 key · fill first")).toHaveLength(1);
  });

  it("offers only the providers that are not configured yet", async () => {
    const daemon = fakeDaemon({
      ...EMPTY,
      ListCredentials: {
        providers: [
          { provider: "openrouter", strategy: "fill_first", cooldownSecs: 0, entries: [] },
        ],
      },
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Add a model provider")).toBeTruthy());
    expect(screen.getByText("Anthropic")).toBeTruthy();
    // OpenRouter appears once, as a connected card, not again as an offer.
    expect(screen.getAllByText("OpenRouter")).toHaveLength(1);
  });

  it("counts a key found on this computer as connected, not as one to add", async () => {
    // The daemon was already answering with this key; offering "Add
    // Anthropic" asked the user to paste something Borg was using.
    const daemon = fakeDaemon({
      ...EMPTY,
      GetSetupStatus: setupStatus({
        detected: [
          { provider: "anthropic", source: "dotenv", detail: "~/.borg/.env" },
          // A CLI sign-in is not a key; it stays an offer.
          { provider: "groq", source: "claude_cli", detail: "/usr/local/bin/claude" },
        ],
      }),
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("key found on this computer")).toBeTruthy());
    expect(screen.getAllByText("Anthropic")).toHaveLength(1);
    expect(screen.getByText("Groq")).toBeTruthy();
    expect(screen.getAllByText("key found on this computer")).toHaveLength(1);
  });

  it("does not offer a provider that has no key to paste", async () => {
    // The detail page behind a tile is a key form; Ollama needs no key.
    const daemon = fakeDaemon(EMPTY);
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Add a model provider")).toBeTruthy());
    expect(screen.queryByText("Ollama")).toBeNull();
  });

  it("still lists the connections when the catalog cannot be fetched", async () => {
    // The catalog is display metadata; a slow probe must not blank the page.
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    const daemon = fakeDaemon({
      ...EMPTY,
      GetSetupStatus: new BorgError(GrpcStatus.Unavailable, "probe timed out"),
      ListApiKeys: { entries: [{ provider: "anthropic", backend: "literal" }] },
    });
    mount(daemon);
    // No catalog means no display name either: the id stands in for it.
    await waitFor(() => expect(screen.getByText("anthropic")).toBeTruthy());
    expect(screen.getByText("key saved")).toBeTruthy();
    expect(screen.queryByText("Add a model provider")).toBeNull();
    // Keys found on this computer drop out with the catalog; the reason must
    // not be silent.
    expect(warn).toHaveBeenCalledWith(
      "connections: setup status unavailable",
      expect.objectContaining({ message: "probe timed out" }),
    );
    warn.mockRestore();
  });

  it("opens a provider's detail page", async () => {
    const daemon = fakeDaemon(EMPTY);
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Anthropic")).toBeTruthy());

    act(() => screen.getByText("Anthropic").click());
    expect(screen.getByTestId("path").textContent).toBe("/connections/provider/anthropic");
  });

  it("raises a pairing request that is waiting on the user", async () => {
    const daemon = fakeDaemon({
      ...EMPTY,
      ListPairingRequests: {
        requests: [
          { id: "p1", channelName: "telegram", senderId: "42", status: "pending", code: "" },
        ],
      },
    });
    mount(daemon);
    await waitFor(() =>
      expect(screen.getByText("telegram has 1 pairing request waiting")).toBeTruthy(),
    );

    act(() => screen.getByRole("button", { name: "Review" }).click());
    expect(screen.getByTestId("path").textContent).toBe("/connections/channel/telegram");
  });

  it("ignores a pairing request that is no longer pending", async () => {
    const daemon = fakeDaemon({
      ...EMPTY,
      ListApprovedSenders: {
        senders: [{ id: 1, channelName: "telegram", senderId: "42", displayName: "Ada" }],
      },
      ListPairingRequests: {
        requests: [
          { id: "p1", channelName: "telegram", senderId: "42", status: "approved", code: "" },
        ],
      },
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("1 approved sender")).toBeTruthy());
    expect(screen.queryByRole("button", { name: "Review" })).toBeNull();
  });

  it("counts what is connected, and what needs attention", async () => {
    const daemon = fakeDaemon({
      ...EMPTY,
      ListCredentials: {
        providers: [
          {
            provider: "openrouter",
            strategy: "fill_first",
            cooldownSecs: 0,
            entries: [credential()],
          },
        ],
      },
      ListPairingRequests: {
        requests: [
          { id: "p1", channelName: "telegram", senderId: "42", status: "pending", code: "" },
        ],
      },
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("2 connected · 1 needs attention")).toBeTruthy());
  });

  it("imports a sign-in from a tool already on this machine", async () => {
    const daemon = fakeDaemon({ ...EMPTY, PairCredential: { entries: [{ id: "oauth-1" }] } });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Claude Code")).toBeTruthy());

    act(() => screen.getByText("Claude Code").click());
    await waitFor(() => expect(screen.getByText("Imported 1 account.")).toBeTruthy());
    expect(daemon.calls.find((c) => c.method === "PairCredential")?.request).toEqual({
      kind: "claude_code",
    });
  });

  it("says what to do when there is nothing to import", async () => {
    const daemon = fakeDaemon({ ...EMPTY, PairCredential: { entries: [] } });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Codex")).toBeTruthy());

    act(() => screen.getByText("Codex").click());
    await waitFor(() =>
      expect(
        screen.getByText("Nothing found on this machine — sign in with that tool first."),
      ).toBeTruthy(),
    );
  });

  it("reports a failed import rather than looking like it did nothing", async () => {
    const daemon = fakeDaemon({
      ...EMPTY,
      PairCredential: new BorgError(GrpcStatus.NotFound, "no credentials file"),
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Copilot")).toBeTruthy());

    act(() => screen.getByText("Copilot").click());
    await waitFor(() => expect(screen.getByText("no credentials file")).toBeTruthy());
  });

  it("reports a failed load", async () => {
    const daemon = fakeDaemon({
      ...EMPTY,
      ListCredentials: new BorgError(GrpcStatus.Unavailable, "daemon down"),
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Could not load connections.")).toBeTruthy());
  });

  it("switches from cards to rows at phone width", async () => {
    mobile.set(true);
    try {
      const daemon = fakeDaemon({
        ...EMPTY,
        ListCredentials: {
          providers: [
            {
              provider: "openrouter",
              strategy: "fill_first",
              cooldownSecs: 0,
              entries: [credential()],
            },
          ],
        },
      });
      mount(daemon);
      // The connected heading is desktop-only; the rows carry the same names.
      await waitFor(() => expect(screen.getByText("OpenRouter")).toBeTruthy());
      expect(screen.queryByText("Connected")).toBeNull();
    } finally {
      mobile.set(false);
    }
  });
});
