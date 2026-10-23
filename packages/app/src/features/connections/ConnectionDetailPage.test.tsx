import { describe, expect, it } from "vitest";
import { act, fireEvent, screen, waitFor } from "@testing-library/react";
import { Route, Routes, useLocation } from "react-router-dom";
import { BorgError, GrpcStatus } from "@borg/client";
import { INVALID_ARGUMENT, fakeDaemon, renderApp } from "../../test/harness";
import { ConnectionDetailPage } from "./ConnectionDetailPage";

const EMPTY = {
  ListApprovedSenders: { senders: [] },
  ListPairingRequests: { requests: [] },
};

function Where() {
  return <span data-testid="path">{useLocation().pathname}</span>;
}

function mount(route: string, daemon: ReturnType<typeof fakeDaemon>) {
  return renderApp(
    <Routes>
      <Route path="/connections/:kind/:name" element={<ConnectionDetailPage />} />
      <Route path="/settings/provider/:name" element={<Where />} />
    </Routes>,
    { daemon, route },
  );
}

const click = (el: HTMLElement) => act(() => el.click());

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
describe("ConnectionDetailPage and an old provider link", () => {
  it("sends a provider's page to its new home under Settings", async () => {
    const daemon = fakeDaemon(EMPTY);
    mount("/connections/provider/anthropic", daemon);
    await waitFor(() =>
      expect(screen.getByTestId("path").textContent).toBe("/settings/provider/anthropic"),
    );
    // Nothing about channels is loaded for a page that is not a channel.
    expect(daemon.calls).toEqual([]);
  });
});
