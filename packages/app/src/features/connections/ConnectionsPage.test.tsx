import { describe, expect, it } from "vitest";
import { act, screen, waitFor } from "@testing-library/react";
import { Route, Routes, useLocation } from "react-router-dom";
import { BorgError, GrpcStatus } from "@borg/client";
import { installMatchMedia } from "@borg/ui/test/matchMedia";
import { fakeDaemon, renderApp } from "../../test/harness";
import { ConnectionsPage } from "./ConnectionsPage";
import { badgeOf } from "./useConnections";

const media = installMatchMedia(() => false);
const mobile = media.controller("(max-width: 700px)");

const EMPTY = {
  ListApprovedSenders: { senders: [] },
  ListPairingRequests: { requests: [] },
  ListMcpServers: { servers: [], presets: [], mcpEnabled: true },
  ListSkillCatalog: { skills: [] },
};

const TELEGRAM = {
  ListApprovedSenders: {
    senders: [{ id: 1, channelName: "telegram", senderId: "42", displayName: "Ada" }],
  },
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

describe("ConnectionsPage", () => {
  it("leaves model providers to Settings: no keys, no catalog, no provider tiles", async () => {
    // A daemon that would answer every provider question, to prove the page
    // does not ask: provider keys live under Settings → Provider now.
    const daemon = fakeDaemon({
      ...EMPTY,
      ...TELEGRAM,
      ListCredentials: { providers: [{ provider: "openrouter", entries: [] }] },
      ListApiKeys: { entries: [{ provider: "anthropic", backend: "literal" }] },
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("telegram")).toBeTruthy());
    const asked = daemon.calls.map((c) => c.method);
    for (const method of ["ListCredentials", "ListApiKeys", "GetSetupStatus", "PairCredential"]) {
      expect(asked).not.toContain(method);
    }
    for (const gone of ["Model providers", "Add a provider", "Import a sign-in", "Anthropic"]) {
      expect(screen.queryByText(gone)).toBeNull();
    }
    // What the page is for still stands: channels, MCP servers, skills.
    for (const heading of ["Channels", "MCP servers", "Skills"]) {
      expect(screen.getByRole("heading", { name: heading })).toBeTruthy();
    }
  });

  it("shows no Channels group until a channel exists", async () => {
    mount(fakeDaemon(EMPTY));
    await waitFor(() => expect(screen.getByRole("heading", { name: "Skills" })).toBeTruthy());
    expect(screen.queryByRole("heading", { name: "Channels" })).toBeNull();
  });

  it("opens a channel's detail page from its card", async () => {
    mount(fakeDaemon({ ...EMPTY, ...TELEGRAM }));
    await waitFor(() => expect(screen.getByText("1 approved sender")).toBeTruthy());
    act(() => screen.getByRole("button", { name: /telegram/ }).click());
    expect(screen.getByTestId("path").textContent).toBe("/connections/channel/telegram");
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
      ...TELEGRAM,
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

  it("counts the channels, and what needs attention", async () => {
    const daemon = fakeDaemon({
      ...EMPTY,
      ...TELEGRAM,
      ListPairingRequests: {
        requests: [
          { id: "p1", channelName: "discord", senderId: "7", status: "pending", code: "" },
        ],
      },
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("2 channels · 1 needs attention")).toBeTruthy());
  });

  it("says nothing in the header when there is nothing to count", async () => {
    mount(fakeDaemon(EMPTY));
    await waitFor(() => expect(screen.getByRole("heading", { name: "Skills" })).toBeTruthy());
    expect(screen.queryByText(/connected|channels? ·|needs attention/)).toBeNull();
  });

  it("reports a failed load", async () => {
    const daemon = fakeDaemon({
      ...EMPTY,
      ListApprovedSenders: new BorgError(GrpcStatus.Unavailable, "daemon down"),
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Could not load connections.")).toBeTruthy());
  });

  it("switches from cards to rows at phone width", async () => {
    mobile.set(true);
    try {
      mount(fakeDaemon({ ...EMPTY, ...TELEGRAM }));
      await waitFor(() => expect(screen.getByText("telegram")).toBeTruthy());
      // A row is a full-width button with the channel's badge in it.
      const row = screen.getByRole("button", { name: /telegram/ });
      expect(row.className).toMatch(/w-full/);
    } finally {
      mobile.set(false);
    }
  });
});
