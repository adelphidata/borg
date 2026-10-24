import { afterEach, describe, expect, it } from "vitest";
import { act, screen, waitFor } from "@testing-library/react";
import { Route, Routes, useLocation } from "react-router-dom";
import { BorgError, GrpcStatus } from "@borg/client";
import { INVALID_ARGUMENT, fakeDaemon, renderApp } from "../../test/harness";
import { emitActivity, listing, subscription } from "../../test/subscriptions";
import { SettingsProvider } from "../../lib/settings";
import { clearNotice, pendingNotice } from "../../lib/notice";
import { SubscriptionsSection } from "./SubscriptionsSection";
import { ACTIVITY_REFRESH_MS, useSubscriptions } from "./useSubscriptions";

function Section() {
  return <SubscriptionsSection subs={useSubscriptions()} />;
}

function Where() {
  return <span data-testid="path">{useLocation().pathname}</span>;
}

function mount(daemon: ReturnType<typeof fakeDaemon>, withSettings = false) {
  return renderApp(
    <>
      <Section />
      <Routes>
        <Route path="*" element={<Where />} />
      </Routes>
    </>,
    {
      daemon,
      route: "/connections",
      wrapper: withSettings ? (c) => <SettingsProvider>{c}</SettingsProvider> : undefined,
    },
  );
}

const click = (el: HTMLElement) => act(() => el.click());
const listCalls = (daemon: ReturnType<typeof fakeDaemon>) =>
  daemon.calls.filter((c) => c.method === "ListSubscriptions").length;

afterEach(() => clearNotice());

describe("SubscriptionsSection listing", () => {
  it("says for each subscription whether it works", async () => {
    const daemon = fakeDaemon(
      listing([
        subscription({ id: "a", name: "Datadog alerts" }),
        subscription({ id: "b", name: "Paused one", status: "paused" }),
        subscription({ id: "c", name: "Stormy", status: "storm" }),
        subscription({ id: "d", name: "Broken", lastRunStatus: "failed", lastError: "no credits" }),
      ]),
    );
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Datadog alerts")).toBeTruthy());
    expect(screen.getByText("Listening for the first event")).toBeTruthy();
    expect(screen.getByText("Paused")).toBeTruthy();
    expect(screen.getByText("Paused itself — too many events").className).toContain("text-alert");
    expect(screen.getByText("Last run failed: no credits").className).toContain("text-alert");
    // The switch is on only while it listens; a storm reads off until resumed.
    expect(screen.getByRole("switch", { name: "Enable Datadog alerts" }).ariaChecked).toBe("true");
    expect(screen.getByRole("switch", { name: "Enable Stormy" }).ariaChecked).toBe("false");
  });

  it("opens the detail page from a row", async () => {
    mount(fakeDaemon(listing([subscription()])));
    await waitFor(() => expect(screen.getByText("Datadog alerts")).toBeTruthy());
    click(screen.getByText("Datadog alerts"));
    expect(screen.getByTestId("path").textContent).toBe("/connections/subscriptions/sub_1");
  });

  it("offers three starting points when there are none, each opening the add flow", async () => {
    mount(fakeDaemon(listing([])));
    await waitFor(() => expect(screen.getByText(/Let an outside event wake Borg/)).toBeTruthy());
    for (const chip of ["Sentry errors", "Slack messages", "Email to Borg"]) {
      expect(screen.getByRole("button", { name: chip })).toBeTruthy();
    }
    click(screen.getByRole("button", { name: "Slack messages" }));
    // Straight to Slack's own conditions: no source grid to pick from again.
    expect(screen.getByText("Messages in Slack channels")).toBeTruthy();
    expect(screen.getByLabelText("Channel")).toBeTruthy();
    expect(
      screen.queryByText("What should wake Borg? Pick where the events come from."),
    ).toBeNull();
  });

  it("re-reads the list when the live stream says something happened", async () => {
    const daemon = fakeDaemon(listing([subscription()]));
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Datadog alerts")).toBeTruthy());
    const before = listCalls(daemon);
    daemon.answer("ListSubscriptions", {
      subscriptions: [subscription({ lastRunStatus: "failed", lastError: "timed out" })],
    });
    emitActivity(daemon, { subscriptionId: "sub_1", kind: "finished", status: "failed" });
    await waitFor(() => expect(screen.getByText("Last run failed: timed out")).toBeTruthy(), {
      timeout: ACTIVITY_REFRESH_MS * 8,
    });
    expect(listCalls(daemon)).toBe(before + 1);
  });

  it("reports a failed listing", async () => {
    mount(
      fakeDaemon({
        ListSubscriptions: new BorgError(GrpcStatus.Unavailable, "down"),
        ListSubscriptionPresets: { presets: [] },
      }),
    );
    await waitFor(() =>
      expect(screen.getByText("Could not load your subscriptions.")).toBeTruthy(),
    );
  });
});

describe("SubscriptionsSection writes", () => {
  it("resumes from the switch, re-reads, and says so", async () => {
    const daemon = fakeDaemon({
      ...listing([subscription({ status: "paused" })]),
      SetSubscriptionStatus: subscription(),
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Paused")).toBeTruthy());
    const before = listCalls(daemon);
    click(screen.getByRole("switch", { name: "Enable Datadog alerts" }));
    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "SetSubscriptionStatus")?.request).toEqual({
        id: "sub_1",
        active: true,
      }),
    );
    await waitFor(() => expect(listCalls(daemon)).toBe(before + 1));
    expect(pendingNotice()?.message).toBe("“Datadog alerts” is on");
  });

  it("shows the daemon's refusal word for word", async () => {
    const refusal = "Paste the signing secret from Sentry first, then enable it.";
    const daemon = fakeDaemon({
      ...listing([subscription({ status: "paused" })]),
      SetSubscriptionStatus: new BorgError(INVALID_ARGUMENT as never, refusal),
    });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Paused")).toBeTruthy());
    click(screen.getByRole("switch", { name: "Enable Datadog alerts" }));
    await waitFor(() => expect(screen.getByRole("alert").textContent).toBe(refusal));
    expect(pendingNotice()).toBeUndefined();
  });

  it("removes a subscription only after the inline confirm", async () => {
    const daemon = fakeDaemon({ ...listing([subscription()]), DeleteSubscription: {} });
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Datadog alerts")).toBeTruthy());

    click(screen.getByRole("button", { name: "Delete Datadog alerts" }));
    expect(screen.getByText(/Its address stops working at once/)).toBeTruthy();
    click(screen.getByRole("button", { name: "Keep" }));
    expect(daemon.calls.some((c) => c.method === "DeleteSubscription")).toBe(false);
    expect(screen.getByText("Datadog alerts")).toBeTruthy();

    click(screen.getByRole("button", { name: "Delete Datadog alerts" }));
    click(screen.getByRole("button", { name: "Remove" }));
    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "DeleteSubscription")?.request).toEqual({
        id: "sub_1",
      }),
    );
    await waitFor(() => expect(pendingNotice()?.message).toBe("Removed “Datadog alerts”"));
  });

  it("says a channel subscription has no address to break", async () => {
    mount(
      fakeDaemon(
        listing([
          subscription({
            name: "Slack messages",
            source: "channel",
            provider: "slack",
            endpoint: { url: "", public: false, hint: "" },
          }),
        ]),
      ),
    );
    await waitFor(() => expect(screen.getByText("Slack messages")).toBeTruthy());
    click(screen.getByRole("button", { name: "Delete Slack messages" }));
    expect(screen.getByText(/Borg stops reacting to these events/)).toBeTruthy();
  });

  it("explains when subscriptions are switched off and turns them on", async () => {
    const daemon = fakeDaemon({
      ...listing([subscription()]),
      GetSettings: {
        entries: [
          {
            key: "subscriptions.enabled",
            value: "false",
            category: "db",
            label: "Let subscriptions wake Borg",
            kind: "bool",
            group: "Subscriptions",
          },
        ],
      },
      SetSetting: {},
    });
    mount(daemon, true);
    await waitFor(() =>
      expect(screen.getByText("Subscriptions are off, so no event wakes Borg.")).toBeTruthy(),
    );
    click(screen.getByRole("button", { name: "Turn on" }));
    await waitFor(() =>
      expect(daemon.calls.find((c) => c.method === "SetSetting")?.request).toEqual({
        key: "subscriptions.enabled",
        value: "true",
      }),
    );
  });
});
