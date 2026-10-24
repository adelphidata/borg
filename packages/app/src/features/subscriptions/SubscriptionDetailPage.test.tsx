import { afterEach, describe, expect, it } from "vitest";
import { act, fireEvent, screen, waitFor, within } from "@testing-library/react";
import { Route, Routes, useLocation } from "react-router-dom";
import { BorgError } from "@borg/client";
import type { admin } from "@borg/client";
import { INVALID_ARGUMENT, fakeDaemon, renderApp } from "../../test/harness";
import { PRESETS, PUBLIC_URL, emitActivity, event, subscription } from "../../test/subscriptions";
import { clearNotice, pendingNotice } from "../../lib/notice";
import { SubscriptionDetailPage, runsOf } from "./SubscriptionDetailPage";

function Where() {
  return <span data-testid="path">{useLocation().pathname}</span>;
}

function detail(
  over: Parameters<typeof subscription>[0] = {},
  extra: Partial<admin.SubscriptionDetail> = {},
) {
  return {
    subscription: subscription(over),
    events: [],
    senderSteps: [`Set URL to ${PUBLIC_URL}.`, "Add the header Authorization: Bearer YOUR_KEY."],
    runsToday: 4,
    ...extra,
  };
}

function daemonWith(d: unknown, more: Record<string, unknown> = {}) {
  return fakeDaemon({
    GetSubscription: d,
    ListSubscriptionPresets: {
      presets: PRESETS,
      deliveryChannels: ["slack", "telegram", "discord"],
      keyPlaceholder: "YOUR_KEY",
    },
    ...more,
  });
}

function mount(daemon: ReturnType<typeof fakeDaemon>) {
  return renderApp(
    <>
      <Routes>
        <Route path="/connections/subscriptions/:id" element={<SubscriptionDetailPage />} />
        <Route path="*" element={null} />
      </Routes>
      <Routes>
        <Route path="*" element={<Where />} />
      </Routes>
    </>,
    { daemon, route: "/connections/subscriptions/sub_1" },
  );
}

const click = (el: HTMLElement) => act(() => el.click());
const type = (el: HTMLElement, value: string) => fireEvent.change(el, { target: { value } });
const requestOf = (daemon: ReturnType<typeof fakeDaemon>, method: string) =>
  daemon.calls.filter((c) => c.method === method).at(-1)?.request as Record<string, unknown>;
const reads = (daemon: ReturnType<typeof fakeDaemon>) =>
  daemon.calls.filter((c) => c.method === "GetSubscription").length;

async function ready() {
  await waitFor(() => expect(screen.getByText("Recent events")).toBeTruthy());
}

afterEach(() => clearNotice());

describe("runsOf", () => {
  it("groups events by the run that handled them, keeping waiting events apart", () => {
    const runs = runsOf([
      event({ id: "a", runId: "r2" }),
      event({ id: "b", runId: "r1" }),
      event({ id: "c", runId: "r2" }),
      event({ id: "d", runId: "" }),
      event({ id: "e", runId: "" }),
    ]);
    expect(runs.map((r) => r.map((e) => e.id))).toEqual([["a", "c"], ["b"], ["d"], ["e"]]);
  });
});

describe("SubscriptionDetailPage", () => {
  it("shows the status line, and the runs with their events grouped", async () => {
    const digest = Array.from({ length: 37 }, (_, i) =>
      event({ id: `burst_${i}`, runId: "run_burst", summary: "Disk full on db-1" }),
    );
    const daemon = daemonWith(
      detail(
        { lastEventAtUnix: Math.floor(Date.now() / 1000) - 120, lastRunStatus: "succeeded" },
        {
          events: [
            event({
              id: "t1",
              runId: "run_t",
              kind: "test",
              summary: "Test: CPU above 90%",
              status: "failed",
              error: "the model refused the request",
            }),
            ...digest,
          ],
        },
      ),
    );
    mount(daemon);
    await ready();
    expect(screen.getByText("Last event 2 min ago")).toBeTruthy();
    expect(screen.getByText("Test")).toBeTruthy();
    expect(screen.getByText("the model refused the request")).toBeTruthy();
    expect(screen.getByText(/37 events → 1 run · Done/)).toBeTruthy();

    // Opening a run shows what came in, readable.
    const row = screen.getByRole("button", { name: /Disk full on db-1/ });
    expect(row.getAttribute("aria-expanded")).toBe("false");
    click(row);
    expect(row.getAttribute("aria-expanded")).toBe("true");
    expect(screen.getAllByText(/"title": "CPU above 90%"/)).toHaveLength(3);
    expect(screen.getByText("And 34 more in this run.")).toBeTruthy();
  });

  it("links to the conversation its runs land in, once there is one", async () => {
    mount(daemonWith(detail({}, { events: [event()] })));
    await ready();
    click(screen.getByRole("button", { name: "Open conversation" }));
    expect(screen.getByTestId("path").textContent).toBe("/c/subscription%3Asub_1");
  });

  it("has no conversation to open before anything ran", async () => {
    mount(daemonWith(detail()));
    await ready();
    expect(screen.queryByRole("button", { name: "Open conversation" })).toBeNull();
    expect(screen.getByText(/No events yet/)).toBeTruthy();
  });

  it("explains a storm and clears it with Resume", async () => {
    const daemon = daemonWith(detail({ status: "storm" }), {
      SetSubscriptionStatus: subscription(),
    });
    mount(daemon);
    await ready();
    expect(screen.getByText("Paused itself: too many events at once")).toBeTruthy();
    daemon.answer("GetSubscription", detail());
    const banner = screen
      .getByText("Paused itself: too many events at once")
      .closest<HTMLElement>(".bg-alert-soft")!;
    click(within(banner).getByRole("button", { name: "Resume" }));
    await waitFor(() =>
      expect(requestOf(daemon, "SetSubscriptionStatus")).toEqual({ id: "sub_1", active: true }),
    );
    await waitFor(() => expect(screen.queryByText(/Paused itself/)).toBeNull());
    expect(screen.getByRole("button", { name: "Pause" })).toBeTruthy();
    expect(pendingNotice()?.message).toBe("“Datadog alerts” is on");
  });

  it("pauses a listening subscription", async () => {
    const daemon = daemonWith(detail(), {
      SetSubscriptionStatus: subscription({ status: "paused" }),
    });
    mount(daemon);
    await ready();
    click(screen.getByRole("button", { name: "Pause" }));
    await waitFor(() =>
      expect(requestOf(daemon, "SetSubscriptionStatus")).toEqual({ id: "sub_1", active: false }),
    );
    await waitFor(() => expect(pendingNotice()?.message).toBe("Paused “Datadog alerts”"));
  });

  it("saves a new instruction and nothing else", async () => {
    const daemon = daemonWith(detail(), { UpdateSubscription: subscription() });
    mount(daemon);
    await ready();
    // Nothing typed, nothing to save.
    expect(screen.queryByRole("button", { name: "Save instruction" })).toBeNull();
    type(screen.getByLabelText("Instruction"), "Only page for prod.");
    click(screen.getByRole("button", { name: "Save instruction" }));
    await waitFor(() =>
      expect(requestOf(daemon, "UpdateSubscription")).toMatchObject({
        id: "sub_1",
        instruction: "Only page for prod.",
        name: "",
        filterJson: "",
        setDelivery: false,
        setSigningSecret: false,
      }),
    );
    await waitFor(() =>
      expect(screen.queryByRole("button", { name: "Save instruction" })).toBeNull(),
    );
  });

  it("saves the conditions, and shows a refusal word for word in that section", async () => {
    const refusal = 'Unknown filter key "color". Datadog filters on: kinds, severity_min.';
    const daemon = daemonWith(detail({ filterJson: '{"kinds":["alert.triggered"]}' }), {
      UpdateSubscription: new BorgError(INVALID_ARGUMENT as never, refusal),
    });
    mount(daemon);
    await ready();
    // The stored filter is what the editor starts from.
    expect(
      screen.getByRole("button", { name: "alert triggered" }).getAttribute("aria-pressed"),
    ).toBe("true");
    type(screen.getByLabelText("How serious"), "critical");
    click(screen.getByRole("button", { name: "Save when" }));
    await waitFor(() => expect(screen.getByRole("alert").textContent).toBe(refusal));
    expect(JSON.parse(requestOf(daemon, "UpdateSubscription").filterJson as string)).toEqual({
      kinds: ["alert.triggered"],
      severity_min: "critical",
    });
    // What was typed stays, so it can be fixed.
    expect((screen.getByLabelText("How serious") as HTMLSelectElement).value).toBe("critical");
    click(screen.getByRole("button", { name: "Discard" }));
    expect((screen.getByLabelText("How serious") as HTMLSelectElement).value).toBe("");
  });

  it("never hides a stored condition behind More conditions", async () => {
    mount(
      daemonWith(
        detail({
          provider: "github",
          filterJson: '{"labels":["bug","p1"],"exclude_drafts":true}',
        }),
      ),
    );
    await ready();
    // Folded by default on a new GitHub subscription, but these hold values.
    expect((screen.getByLabelText("Labels") as HTMLInputElement).value).toBe("bug, p1");
    expect(
      screen.getByRole("switch", { name: "Skip draft pull requests" }).getAttribute("aria-checked"),
    ).toBe("true");
    expect(screen.queryByLabelText("Branch")).toBeNull();
    expect(screen.getByRole("button", { name: "More conditions" })).toBeTruthy();
  });

  it("changes where it reports, with the delivery flag set", async () => {
    const daemon = daemonWith(detail(), { UpdateSubscription: subscription() });
    mount(daemon);
    await ready();
    click(screen.getByRole("button", { name: "Slack" }));
    type(screen.getByLabelText("Who gets it on Slack"), "C0123");
    click(screen.getByRole("button", { name: "Save where it reports" }));
    await waitFor(() =>
      expect(requestOf(daemon, "UpdateSubscription")).toMatchObject({
        setDelivery: true,
        deliveryChannel: "slack",
        deliveryTarget: "C0123",
      }),
    );
  });

  it("reveals the key on request, and replaces it only after a confirm", async () => {
    const daemon = daemonWith(detail(), {
      RevealSubscriptionKey: { key: "bsk_old", senderSteps: ["Bearer bsk_old"] },
      RotateSubscriptionKey: { key: "bsk_new", senderSteps: ["Bearer bsk_new"] },
    });
    mount(daemon);
    await ready();
    expect(screen.getByLabelText("URL").textContent).toBe(PUBLIC_URL);
    expect(screen.getByText("Hidden. Reveal it to copy it again.")).toBeTruthy();
    click(screen.getByRole("button", { name: "Reveal key" }));
    await waitFor(() => expect(screen.getByLabelText("Key")).toBeTruthy());
    expect(requestOf(daemon, "RevealSubscriptionKey")).toEqual({ id: "sub_1" });
    click(screen.getByRole("button", { name: "Show key" }));
    expect(screen.getByLabelText("Key").textContent).toBe("bsk_old");

    click(screen.getByRole("button", { name: "Replace key" }));
    expect(screen.getByText(/The old one stops working at once/)).toBeTruthy();
    click(screen.getByRole("button", { name: "Keep" }));
    expect(daemon.calls.some((c) => c.method === "RotateSubscriptionKey")).toBe(false);

    click(screen.getByRole("button", { name: "Replace key" }));
    click(screen.getByRole("button", { name: "Replace" }));
    await waitFor(() => expect(screen.getByLabelText("Key").textContent).toBe("bsk_new"));
    // The steps open with the new key in them, since the sender needs it now.
    expect(screen.getByRole("list", { name: "Set up Datadog" }).textContent).toContain("bsk_new");
    expect(pendingNotice()?.message).toBe("Key replaced. Paste the new one into Datadog.");
  });

  it("folds the setup steps until asked", async () => {
    mount(daemonWith(detail()));
    await ready();
    expect(screen.queryByRole("list", { name: "Set up Datadog" })).toBeNull();
    click(screen.getByRole("button", { name: "How to set it up in Datadog" }));
    expect(screen.getByRole("list", { name: "Set up Datadog" }).textContent).toContain(PUBLIC_URL);
  });

  it("asks for a signing secret only where the provider issues one", async () => {
    const daemon = daemonWith(
      detail({ provider: "sentry", name: "Sentry errors", status: "paused" }),
      { UpdateSubscription: subscription({ provider: "sentry", hasSigningSecret: true }) },
    );
    mount(daemon);
    await ready();
    expect(screen.getByText("Waiting for the signing secret")).toBeTruthy();
    type(screen.getByLabelText("Signing secret"), "whsec_9");
    click(screen.getByRole("button", { name: "Save secret" }));
    await waitFor(() =>
      expect(requestOf(daemon, "UpdateSubscription")).toMatchObject({
        setSigningSecret: true,
        signingSecret: "whsec_9",
      }),
    );
  });

  it("has no signing secret field for a provider that signs with Borg's key", async () => {
    mount(daemonWith(detail()));
    await ready();
    expect(screen.queryByLabelText("Signing secret")).toBeNull();
  });

  it("has no endpoint section for a channel source", async () => {
    mount(
      daemonWith(
        detail({
          provider: "slack",
          source: "channel",
          endpoint: { url: "", public: false, hint: "" },
        }),
      ),
    );
    await ready();
    expect(screen.queryByText("Endpoint")).toBeNull();
    expect(screen.getByRole("button", { name: "Reply where it came from" })).toBeTruthy();
  });

  it("turns on the tunnel for an address only this computer can reach, then re-reads", async () => {
    const hint = "Only this computer can reach this address.";
    const daemon = daemonWith(
      detail({ endpoint: { url: "http://localhost:7860/v1/events/sub_1", public: false, hint } }),
      { TunnelStart: { provider: "cloudflare", running: true, publicUrl: "https://t", error: "" } },
    );
    mount(daemon);
    await ready();
    const before = reads(daemon);
    daemon.answer("GetSubscription", detail());
    click(screen.getByRole("button", { name: "Turn on" }));
    await waitFor(() => expect(screen.queryByText(hint)).toBeNull());
    expect(reads(daemon)).toBe(before + 1);
    expect(screen.getByLabelText("URL").textContent).toBe(PUBLIC_URL);
  });

  it("shows Running… for a test until the stream says it finished", async () => {
    const daemon = daemonWith(detail(), {
      TestSubscription: event({ id: "evt_t", kind: "test", status: "pending", runId: "" }),
    });
    mount(daemon);
    await ready();
    daemon.answer(
      "GetSubscription",
      detail({}, { events: [event({ id: "evt_t", kind: "test", status: "pending", runId: "" })] }),
    );
    click(screen.getByRole("button", { name: "Send a test event" }));
    await waitFor(() => expect(screen.getByRole("button", { name: "Running…" })).toBeTruthy());
    expect(requestOf(daemon, "TestSubscription")).toEqual({ id: "sub_1" });
    // Another subscription finishing says nothing about this test.
    emitActivity(daemon, { subscriptionId: "sub_2", kind: "finished" });
    expect(screen.getByRole("button", { name: "Running…" })).toBeTruthy();
    emitActivity(daemon, { subscriptionId: "sub_1", kind: "finished", status: "succeeded" });
    expect(screen.getByRole("button", { name: "Send a test event" })).toBeTruthy();
  });

  it("stops saying Running… once the re-read shows the test settled, even if the stream missed it", async () => {
    const daemon = daemonWith(detail(), {
      TestSubscription: event({ id: "evt_t", kind: "test", status: "pending", runId: "" }),
    });
    mount(daemon);
    await ready();
    daemon.answer(
      "GetSubscription",
      detail({}, { events: [event({ id: "evt_t", kind: "test", status: "succeeded" })] }),
    );
    click(screen.getByRole("button", { name: "Send a test event" }));
    await waitFor(() => expect(requestOf(daemon, "TestSubscription")).toBeTruthy());
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "Send a test event" })).toBeTruthy(),
    );
  });

  it("deletes after the inline confirm and goes back to Connections", async () => {
    const daemon = daemonWith(detail(), { DeleteSubscription: {} });
    mount(daemon);
    await ready();
    click(screen.getByRole("button", { name: "Delete" }));
    expect(
      screen.getByText(/Delete Datadog alerts\? Its address stops working at once/),
    ).toBeTruthy();
    click(screen.getByRole("button", { name: "Keep" }));
    expect(daemon.calls.some((c) => c.method === "DeleteSubscription")).toBe(false);
    click(screen.getByRole("button", { name: "Delete" }));
    click(screen.getByRole("button", { name: "Delete" }));
    await waitFor(() => expect(screen.getByTestId("path").textContent).toBe("/connections"));
    expect(requestOf(daemon, "DeleteSubscription")).toEqual({ id: "sub_1" });
  });

  it("renames from the page title", async () => {
    const daemon = daemonWith(detail(), {
      UpdateSubscription: subscription({ name: "Prod alerts" }),
    });
    mount(daemon);
    await ready();
    fireEvent.doubleClick(screen.getByRole("heading", { name: "Datadog alerts" }));
    const box = screen.getByLabelText("Subscription name");
    type(box, "Prod alerts");
    fireEvent.keyDown(box, { key: "Enter" });
    await waitFor(() =>
      expect(requestOf(daemon, "UpdateSubscription")).toMatchObject({
        id: "sub_1",
        name: "Prod alerts",
        instruction: "",
      }),
    );
  });

  it("keeps the rename box open and says why when the name is refused", async () => {
    const refusal = "A name can be at most 80 characters.";
    const daemon = daemonWith(detail(), {
      UpdateSubscription: new BorgError(INVALID_ARGUMENT as never, refusal),
    });
    mount(daemon);
    await ready();
    fireEvent.doubleClick(screen.getByRole("heading", { name: "Datadog alerts" }));
    const box = screen.getByLabelText("Subscription name");
    type(box, "x".repeat(90));
    fireEvent.keyDown(box, { key: "Enter" });
    await waitFor(() => expect(screen.getByRole("alert").textContent).toBe(refusal));
    expect(screen.getByLabelText("Subscription name")).toBeTruthy();
  });

  it("keeps the limits and counts behind Advanced", async () => {
    const daemon = daemonWith(detail({ droppedCount: 12 }), {
      UpdateSubscription: subscription({ rateLimitPerHour: 10 }),
    });
    mount(daemon);
    await ready();
    expect(screen.queryByText("Runs today")).toBeNull();
    click(screen.getByRole("button", { name: "Advanced" }));
    expect(screen.getByText("Runs today").nextElementSibling?.textContent).toBe("4");
    expect(
      screen.getByText("Events dropped in bursts or while paused").nextElementSibling?.textContent,
    ).toBe("12");
    type(screen.getByLabelText("Runs per hour, at most"), "10");
    type(screen.getByLabelText("Stop a run after (seconds)"), "nonsense");
    click(screen.getByRole("button", { name: "Save limits" }));
    await waitFor(() =>
      expect(requestOf(daemon, "UpdateSubscription")).toMatchObject({
        rateLimitPerHour: 10,
        // Not a number: left alone rather than sent as a zero-second timeout.
        timeoutSecs: 0,
      }),
    );
  });

  it("says so when the subscription is gone", async () => {
    mount(daemonWith({ events: [], senderSteps: [], runsToday: 0 }));
    await waitFor(() =>
      expect(screen.getByText("This subscription no longer exists.")).toBeTruthy(),
    );
  });
});
