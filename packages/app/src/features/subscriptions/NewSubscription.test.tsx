import { afterEach, describe, expect, it, vi } from "vitest";
import { act, fireEvent, screen, waitFor, within } from "@testing-library/react";
import { Route, Routes, useLocation } from "react-router-dom";
import { BorgError, GrpcStatus } from "@borg/client";
import { INVALID_ARGUMENT, fakeDaemon, renderApp } from "../../test/harness";
import { PUBLIC_URL, emitActivity, listing, subscription } from "../../test/subscriptions";
import { clearNotice, pendingNotice } from "../../lib/notice";
import { SubscriptionsSection } from "./SubscriptionsSection";
import { TEST_POLL_MS, TUNNEL_POLL_MS, useSubscriptions } from "./useSubscriptions";

function Section() {
  return <SubscriptionsSection subs={useSubscriptions()} />;
}

function Where() {
  return <span data-testid="path">{useLocation().pathname}</span>;
}

function mount(daemon: ReturnType<typeof fakeDaemon>) {
  return renderApp(
    <>
      <Section />
      <Routes>
        <Route path="*" element={<Where />} />
      </Routes>
    </>,
    { daemon, route: "/connections" },
  );
}

const click = (el: HTMLElement) => act(() => el.click());
const type = (el: HTMLElement, value: string) => fireEvent.change(el, { target: { value } });
const requestOf = (daemon: ReturnType<typeof fakeDaemon>, method: string) =>
  daemon.calls.filter((c) => c.method === method).at(-1)?.request as Record<string, unknown>;

const KEY = "bsk_live_abc";

/** What CreateSubscription answers for a row. */
function created(
  over: Parameters<typeof subscription>[0] = {},
  extra: Record<string, unknown> = {},
) {
  const sub = subscription({ status: "paused", ...over });
  return {
    subscription: sub,
    key: sub.endpoint?.url ? KEY : "",
    senderSteps: sub.endpoint?.url
      ? [`Set URL to ${sub.endpoint.url}.`, `Add the header Authorization: Bearer ${KEY}.`]
      : [],
    waitingForSecret: false,
    ...extra,
  };
}

/** Opens the add card and picks a source tile. */
async function pick(label: string) {
  await waitFor(() => expect(screen.getByText("Add subscription")).toBeTruthy());
  click(screen.getByText("Add subscription"));
  click(screen.getByRole("button", { name: new RegExp(`^${label}`) }));
}

afterEach(() => {
  clearNotice();
  vi.useRealTimers();
});

describe("NewSubscription: picking and creating", () => {
  it("groups the sources the daemon offers, in a fixed order", async () => {
    mount(fakeDaemon(listing([])));
    await waitFor(() => expect(screen.getByText("Add subscription")).toBeTruthy());
    click(screen.getByText("Add subscription"));
    const groups = ["Alerts", "Code", "Messages", "Email"].map((g) => screen.getByText(g));
    // Alerts lead even though the daemon listed GitHub first.
    for (let i = 1; i < groups.length; i++) {
      expect(groups[i - 1]!.compareDocumentPosition(groups[i]!)).toBe(
        Node.DOCUMENT_POSITION_FOLLOWING,
      );
    }
  });

  it("sends the spec built from what was picked, then shows how to connect", async () => {
    const daemon = fakeDaemon({ ...listing([]), CreateSubscription: created() });
    mount(daemon);
    await pick("Datadog");

    // Only Datadog's own conditions: kinds and severity, nothing for repos.
    expect(screen.queryByLabelText("Repositories")).toBeNull();
    expect(
      screen.getByText("Keep it narrow. Every match wakes Borg and uses model usage."),
    ).toBeTruthy();
    const kinds = screen.getByRole("group", { name: "Kinds of event" });
    const triggered = within(kinds).getByRole("button", { name: "alert triggered" });
    click(triggered);
    expect(triggered.getAttribute("aria-pressed")).toBe("true");
    type(screen.getByLabelText("How serious"), "error");
    type(screen.getByLabelText("Instruction"), "Page me if prod is down.");
    click(screen.getByRole("button", { name: "Create" }));

    await waitFor(() => expect(requestOf(daemon, "CreateSubscription")).toBeTruthy());
    expect(requestOf(daemon, "CreateSubscription")).toEqual({
      provider: "datadog",
      name: "",
      instruction: "Page me if prod is down.",
      filterJson: JSON.stringify({ kinds: ["alert.triggered"], severity_min: "error" }),
      deliveryChannel: "",
      deliveryTarget: "",
      signingSecret: "",
      rateLimitPerHour: 0,
      timeoutSecs: 0,
      active: false,
    });

    // The same card now says what to paste where.
    await waitFor(() =>
      expect((screen.getByLabelText("URL") as HTMLInputElement).value).toBe(PUBLIC_URL),
    );
    // Hidden until Show, in the field and in the steps alike.
    const key = screen.getByLabelText("Key") as HTMLInputElement;
    expect(key.type).toBe("password");
    const stepTexts = () =>
      within(screen.getByRole("list", { name: "Set up Datadog" }))
        .getAllByRole("listitem")
        .map((li) => li.textContent ?? "");
    expect(stepTexts()).toHaveLength(2);
    expect(stepTexts().join(" ")).not.toContain(KEY);
    expect(stepTexts()[1]).toContain("Bearer YOUR_KEY");
    click(screen.getByRole("button", { name: "Show key" }));
    expect(key.type).toBe("text");
    expect(key.value).toBe(KEY);
    expect(stepTexts()[1]).toContain(`Bearer ${KEY}`);
    expect(pendingNotice()?.message).toBe("Created “Datadog alerts”");
  });

  it("flips from listening to received when this subscription's first event lands, then enables it", async () => {
    const daemon = fakeDaemon({
      ...listing([]),
      CreateSubscription: created(),
      SetSubscriptionStatus: subscription(),
    });
    mount(daemon);
    await pick("Datadog");
    click(screen.getByRole("button", { name: "Create" }));
    await waitFor(() => expect(screen.getByText("Listening for the first event…")).toBeTruthy());

    // Somebody else's event is not proof this one is connected.
    emitActivity(daemon, { subscriptionId: "sub_other", kind: "received" });
    expect(screen.getByText("Listening for the first event…")).toBeTruthy();
    emitActivity(daemon, { subscriptionId: "sub_1", kind: "received" });
    expect(screen.getByText("Got it. First event received just now.")).toBeTruthy();

    click(screen.getByRole("button", { name: "Enable" }));
    await waitFor(() =>
      expect(requestOf(daemon, "SetSubscriptionStatus")).toEqual({ id: "sub_1", active: true }),
    );
    await waitFor(() => expect(screen.queryByText(/First event received/)).toBeNull());
    expect(pendingNotice()?.message).toBe("“Datadog alerts” is on");
  });

  it("keeps the card and shows the daemon's refusal word for word", async () => {
    const refusal = "Say who on telegram should get the reply.";
    const daemon = fakeDaemon({
      ...listing([]),
      CreateSubscription: new BorgError(INVALID_ARGUMENT as never, refusal),
    });
    mount(daemon);
    await pick("Datadog");
    click(screen.getByRole("button", { name: "Create" }));
    await waitFor(() => expect(screen.getByRole("alert").textContent).toBe(refusal));
    expect(screen.getByLabelText("Instruction")).toBeTruthy();
  });

  it("shows only GitHub's conditions, folding the rarer ones", async () => {
    const daemon = fakeDaemon({
      ...listing([]),
      CreateSubscription: created({ provider: "github" }),
    });
    mount(daemon);
    await pick("GitHub");
    expect(screen.queryByLabelText("How serious")).toBeNull();
    type(screen.getByLabelText("Repositories"), "acme/web, acme/api");
    expect(screen.queryByLabelText("Branch")).toBeNull();
    click(screen.getByRole("button", { name: "More conditions" }));
    type(screen.getByLabelText("Branch"), "main");
    click(screen.getByRole("switch", { name: "Skip draft pull requests" }));
    click(screen.getByRole("button", { name: "Create" }));
    await waitFor(() => expect(requestOf(daemon, "CreateSubscription")).toBeTruthy());
    expect(JSON.parse(requestOf(daemon, "CreateSubscription").filterJson as string)).toEqual({
      repos: ["acme/web", "acme/api"],
      branch: "main",
      exclude_drafts: true,
    });
  });

  it("offers a reply where it came from only for channel sources, and sends a named channel with its recipient", async () => {
    const daemon = fakeDaemon({ ...listing([]), CreateSubscription: created() });
    mount(daemon);
    await pick("Datadog");
    const where = screen.getByRole("radiogroup", { name: "Where it reports" });
    expect(within(where).queryByRole("radio", { name: "Reply where it came from" })).toBeNull();
    expect(
      within(where).getByRole("radio", { name: "Only here" }).getAttribute("aria-checked"),
    ).toBe("true");
    click(within(where).getByRole("radio", { name: "Telegram" }));
    type(screen.getByLabelText("Who gets it on Telegram"), " 12345 ");
    click(screen.getByRole("button", { name: "Create" }));
    await waitFor(() => expect(requestOf(daemon, "CreateSubscription")).toBeTruthy());
    expect(requestOf(daemon, "CreateSubscription")).toMatchObject({
      deliveryChannel: "telegram",
      deliveryTarget: "12345",
    });

    click(screen.getByRole("button", { name: "Keep it paused" }));
    click(screen.getByText("Add subscription"));
    click(screen.getByRole("button", { name: /^Slack/ }));
    expect(screen.getByRole("radio", { name: "Reply where it came from" })).toBeTruthy();
  });

  it("offers only the channels that are set up on this machine", async () => {
    const base = listing([]);
    const daemon = fakeDaemon({
      ...base,
      ListSubscriptionPresets: { ...base.ListSubscriptionPresets, deliveryChannels: ["slack"] },
    });
    mount(daemon);
    await pick("Datadog");
    const where = screen.getByRole("radiogroup", { name: "Where it reports" });
    expect(within(where).getByRole("radio", { name: "Slack" })).toBeTruthy();
    expect(within(where).queryByRole("radio", { name: "Telegram" })).toBeNull();
    expect(within(where).queryByRole("radio", { name: "Discord" })).toBeNull();
  });

  it("sends a changed name, and the preset's own name as empty", async () => {
    const daemon = fakeDaemon({ ...listing([]), CreateSubscription: created() });
    mount(daemon);
    await pick("Datadog");
    click(screen.getByRole("button", { name: "Advanced" }));
    expect((screen.getByLabelText("Name") as HTMLInputElement).value).toBe("Datadog alerts");
    type(screen.getByLabelText("Name"), "Prod alerts");
    click(screen.getByRole("button", { name: "Create" }));
    await waitFor(() => expect(requestOf(daemon, "CreateSubscription")?.name).toBe("Prod alerts"));
  });

  it("goes back to the source grid, and closes on Cancel", async () => {
    mount(fakeDaemon(listing([])));
    await pick("Datadog");
    click(screen.getByRole("button", { name: "Change" }));
    expect(
      screen.getByText("What should wake Borg? Pick where the events come from."),
    ).toBeTruthy();
    click(screen.getByRole("button", { name: "Cancel" }));
    expect(
      screen.queryByText("What should wake Borg? Pick where the events come from."),
    ).toBeNull();
    expect(screen.getByText("Add subscription")).toBeTruthy();
  });
});

describe("NewSubscription: the Connect card", () => {
  it("says a channel source is ready, with no address or key to copy", async () => {
    const daemon = fakeDaemon({
      ...listing([]),
      CreateSubscription: created({
        name: "Slack messages",
        source: "channel",
        provider: "slack",
        endpoint: { url: "", public: false, hint: "" },
      }),
    });
    mount(daemon);
    await pick("Slack");
    click(screen.getByRole("button", { name: "Create" }));
    await waitFor(() =>
      expect(screen.getByText("Ready. Enable when you want Borg to start reacting.")).toBeTruthy(),
    );
    expect(screen.queryByLabelText("URL")).toBeNull();
    expect(screen.queryByText("Listening for the first event…")).toBeNull();
    // No reachability warning either: there is no address to reach.
    expect(screen.queryByRole("button", { name: "Turn on" })).toBeNull();
  });

  it("warns about an address only this computer can reach, and turns on the tunnel", async () => {
    const local = "http://localhost:7860/v1/events/sub_1";
    const hint = "Only this computer can reach this address. Turn on a tunnel.";
    const publicSub = subscription({ status: "paused" });
    const daemon = fakeDaemon({
      ...listing([]),
      CreateSubscription: created({ endpoint: { url: local, public: false, hint } }),
      TunnelStart: {
        provider: "cloudflare",
        running: true,
        publicUrl: "https://t.example",
        error: "",
      },
      GetSubscription: {
        subscription: publicSub,
        events: [],
        senderSteps: [
          `Set URL to ${PUBLIC_URL}.`,
          "Add the header Authorization: Bearer YOUR_KEY.",
        ],
        runsToday: 0,
      },
    });
    mount(daemon);
    await pick("Datadog");
    click(screen.getByRole("button", { name: "Create" }));
    await waitFor(() => expect(screen.getByText(hint)).toBeTruthy());
    expect((screen.getByLabelText("URL") as HTMLInputElement).value).toBe(local);

    click(screen.getByRole("button", { name: "Turn on" }));
    await waitFor(() =>
      expect((screen.getByLabelText("URL") as HTMLInputElement).value).toBe(PUBLIC_URL),
    );
    expect(daemon.calls.some((c) => c.method === "TunnelStart")).toBe(true);
    expect(screen.queryByText(hint)).toBeNull();
    // The re-read masks the key; the one this card holds goes back in on Show.
    const second = () =>
      within(screen.getByRole("list", { name: "Set up Datadog" })).getAllByRole("listitem")[1]!
        .textContent;
    expect(second()).toContain("Bearer YOUR_KEY");
    click(screen.getByRole("button", { name: "Show key" }));
    expect(second()).toContain(`Bearer ${KEY}`);
  });

  it("waits for a tunnel that is still coming up", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const local = "http://localhost:7860/v1/events/sub_1";
    const status = vi
      .fn()
      .mockReturnValueOnce({ provider: "cloudflare", running: true, publicUrl: "", error: "" })
      .mockReturnValue({
        provider: "cloudflare",
        running: true,
        publicUrl: "https://t",
        error: "",
      });
    const daemon = fakeDaemon({
      ...listing([]),
      CreateSubscription: created({ endpoint: { url: local, public: false, hint: "local only" } }),
      TunnelStart: { provider: "cloudflare", running: true, publicUrl: "", error: "" },
      TunnelStatus: status,
      GetSubscription: { subscription: subscription(), events: [], senderSteps: [], runsToday: 0 },
    });
    mount(daemon);
    await pick("Datadog");
    click(screen.getByRole("button", { name: "Create" }));
    await waitFor(() => expect(screen.getByText("local only")).toBeTruthy());
    click(screen.getByRole("button", { name: "Turn on" }));
    expect(screen.getByRole("button", { name: "Turning on…" })).toBeTruthy();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(TUNNEL_POLL_MS * 2);
    });
    await waitFor(() =>
      expect((screen.getByLabelText("URL") as HTMLInputElement).value).toBe(PUBLIC_URL),
    );
    expect(status).toHaveBeenCalledTimes(2);
  });

  it("says what to do when no tunnel is chosen, or when it fails", async () => {
    const local = "http://localhost:7860/v1/events/sub_1";
    const localSub = subscription({ endpoint: { url: local, public: false, hint: "local only" } });
    const daemon = fakeDaemon({
      ...listing([]),
      CreateSubscription: created({ endpoint: localSub.endpoint }),
      TunnelStart: { provider: "none", running: false, publicUrl: "", error: "" },
      GetSubscription: { subscription: localSub, events: [], senderSteps: [], runsToday: 0 },
    });
    mount(daemon);
    await pick("Datadog");
    click(screen.getByRole("button", { name: "Create" }));
    await waitFor(() => expect(screen.getByText("local only")).toBeTruthy());
    click(screen.getByRole("button", { name: "Turn on" }));
    await waitFor(() => expect(screen.getByText(/Borg has no tunnel chosen yet/)).toBeTruthy());

    daemon.answer("TunnelStart", {
      provider: "ngrok",
      running: false,
      publicUrl: "",
      error: "ngrok needs an auth token: set tunnel.ngrok_token.",
    });
    click(screen.getByRole("button", { name: "Turn on" }));
    await waitFor(() =>
      expect(screen.getByRole("alert").textContent).toBe(
        "ngrok needs an auth token: set tunnel.ngrok_token.",
      ),
    );
    expect(screen.queryByText(/Borg has no tunnel chosen yet/)).toBeNull();

    daemon.answer("TunnelStart", { provider: "none", running: false, publicUrl: "", error: "" });
    click(screen.getByRole("button", { name: "Turn on" }));
    await waitFor(() => expect(screen.getByText(/Borg has no tunnel chosen yet/)).toBeTruthy());
    click(screen.getByRole("button", { name: "Choose a tunnel" }));
    expect(screen.getByTestId("path").textContent).toBe("/settings/key/tunnel.provider");
  });

  it("asks for the signing secret only for a provider that issues one, and holds Enable until it is saved", async () => {
    const daemon = fakeDaemon({
      ...listing([]),
      CreateSubscription: created(
        { name: "Sentry errors", provider: "sentry" },
        { waitingForSecret: true },
      ),
      UpdateSubscription: subscription({ provider: "sentry", hasSigningSecret: true }),
    });
    mount(daemon);
    await pick("Sentry");
    click(screen.getByRole("button", { name: "Create" }));
    await waitFor(() => expect(screen.getByLabelText("Signing secret")).toBeTruthy());
    const enable = screen.getByRole("button", { name: "Enable" }) as HTMLButtonElement;
    expect(enable.disabled).toBe(true);
    expect(screen.getByText("Enable unlocks once the signing secret is saved.")).toBeTruthy();

    type(screen.getByLabelText("Signing secret"), "  whsec_123 ");
    click(screen.getByRole("button", { name: "Save secret" }));
    await waitFor(() =>
      expect(requestOf(daemon, "UpdateSubscription")).toEqual({
        id: "sub_1",
        name: "",
        filterJson: "",
        instruction: "",
        setDelivery: false,
        deliveryChannel: "",
        deliveryTarget: "",
        rateLimitPerHour: 0,
        timeoutSecs: 0,
        setSigningSecret: true,
        signingSecret: "whsec_123",
      }),
    );
    await waitFor(() => expect(enable.disabled).toBe(false));
    expect((screen.getByLabelText("Signing secret") as HTMLInputElement).value).toBe("");
  });

  it("shows no signing secret field for a provider that signs with Borg's key", async () => {
    const daemon = fakeDaemon({ ...listing([]), CreateSubscription: created() });
    mount(daemon);
    await pick("Datadog");
    click(screen.getByRole("button", { name: "Create" }));
    await waitFor(() => expect(screen.getByLabelText("URL")).toBeTruthy());
    expect(screen.queryByLabelText("Signing secret")).toBeNull();
  });

  it("runs a test event and says when it finished", async () => {
    const daemon = fakeDaemon({
      ...listing([]),
      CreateSubscription: created(),
      TestSubscription: { id: "evt_t", kind: "test", status: "pending" },
    });
    mount(daemon);
    await pick("Datadog");
    click(screen.getByRole("button", { name: "Create" }));
    await waitFor(() => expect(screen.getByLabelText("URL")).toBeTruthy());
    click(screen.getByRole("button", { name: "Send a test event" }));
    await waitFor(() => expect(screen.getByText("Test sent. Borg is working on it…")).toBeTruthy());
    expect(requestOf(daemon, "TestSubscription")).toEqual({ id: "sub_1" });
    expect(screen.getByRole("button", { name: "Running…" })).toBeTruthy();
    // A test run is not the sender's first event.
    expect(screen.getByText("Listening for the first event…")).toBeTruthy();

    emitActivity(daemon, { subscriptionId: "sub_1", kind: "finished", status: "succeeded" });
    expect(screen.getByText(/Test finished/)).toBeTruthy();
    click(screen.getByRole("button", { name: "See the run" }));
    expect(screen.getByTestId("path").textContent).toBe("/connections/subscriptions/sub_1");
  });

  it("shows a refused enable word for word and keeps the card", async () => {
    const refusal =
      'Subscriptions are turned off. Turn on "Let subscriptions wake Borg" in Settings first.';
    const daemon = fakeDaemon({
      ...listing([]),
      CreateSubscription: created(),
      SetSubscriptionStatus: new BorgError(INVALID_ARGUMENT as never, refusal),
    });
    mount(daemon);
    await pick("Datadog");
    click(screen.getByRole("button", { name: "Create" }));
    await waitFor(() => expect(screen.getByLabelText("URL")).toBeTruthy());
    click(screen.getByRole("button", { name: "Enable" }));
    await waitFor(() => expect(screen.getByRole("alert").textContent).toBe(refusal));
    expect(screen.getByLabelText("URL")).toBeTruthy();
  });
});

describe("NewSubscription: staying put through trouble", () => {
  it("keeps the Connect card and its one-time key when a re-read fails", async () => {
    const daemon = fakeDaemon({ ...listing([]), CreateSubscription: created() });
    mount(daemon);
    await pick("Datadog");
    click(screen.getByRole("button", { name: "Create" }));
    await waitFor(() => expect(screen.getByLabelText("Key")).toBeTruthy());

    // borgd restarts; the next re-read fails.
    daemon.answer("ListSubscriptions", new BorgError(GrpcStatus.Unavailable, "down"));
    emitActivity(daemon, { subscriptionId: "sub_other", kind: "finished" });
    await waitFor(() =>
      expect(screen.getByText("Could not load your subscriptions.")).toBeTruthy(),
    );

    // The card, and the key the daemon will not show in clear again, are still there.
    expect((screen.getByLabelText("Key") as HTMLInputElement).value).toBe(KEY);
    expect((screen.getByLabelText("URL") as HTMLInputElement).value).toBe(PUBLIC_URL);
    expect(screen.getByText("Listening for the first event…")).toBeTruthy();
  });

  it("does not re-read on a reconnect attempt, only on a frame", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const daemon = fakeDaemon(listing([]));
    mount(daemon);
    await waitFor(() => expect(screen.getByText("Add subscription")).toBeTruthy());
    const reads = () => daemon.calls.filter((c) => c.method === "ListSubscriptions").length;
    const before = reads();

    const first = daemon.streams.filter((s) => s.method === "SubscribeSubscriptionActivity");
    act(() => first.at(-1)!.handlers.onError?.(new BorgError(GrpcStatus.Unavailable, "down")));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(31_000);
    });
    // It dialled again, and asking the daemon for the list was not part of that.
    expect(
      daemon.streams.filter((s) => s.method === "SubscribeSubscriptionActivity").length,
    ).toBeGreaterThan(first.length);
    expect(reads()).toBe(before);

    emitActivity(daemon, { subscriptionId: "sub_1", kind: "received" });
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1_000);
    });
    expect(reads()).toBe(before + 1);
  });

  it("ends Running… from a re-read when the stream missed the finished frame", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    const daemon = fakeDaemon({
      ...listing([]),
      CreateSubscription: created(),
      TestSubscription: { id: "evt_t", kind: "test", status: "pending" },
      ListSubscriptionEvents: { events: [{ id: "evt_t", kind: "test", status: "pending" }] },
    });
    mount(daemon);
    await pick("Datadog");
    click(screen.getByRole("button", { name: "Create" }));
    await waitFor(() => expect(screen.getByLabelText("URL")).toBeTruthy());
    click(screen.getByRole("button", { name: "Send a test event" }));
    await waitFor(() => expect(screen.getByRole("button", { name: "Running…" })).toBeTruthy());

    // Still running at the first look; done at the next. No frame ever arrives.
    await act(async () => {
      await vi.advanceTimersByTimeAsync(TEST_POLL_MS);
    });
    expect(screen.getByRole("button", { name: "Running…" })).toBeTruthy();
    daemon.answer("ListSubscriptionEvents", {
      events: [{ id: "evt_t", kind: "test", status: "succeeded" }],
    });
    await act(async () => {
      await vi.advanceTimersByTimeAsync(TEST_POLL_MS);
    });
    await waitFor(() => expect(screen.getByText(/Test finished/)).toBeTruthy());
    expect(requestOf(daemon, "ListSubscriptionEvents")).toEqual({ id: "sub_1", limit: 20 });
  });

  it("says so when the address cannot be re-read after the tunnel starts", async () => {
    const daemon = fakeDaemon({
      ...listing([]),
      CreateSubscription: created({
        endpoint: { url: "http://localhost:7860/v1/events/sub_1", public: false, hint: "local" },
      }),
      TunnelStart: { provider: "cloudflare", running: true, publicUrl: "https://t", error: "" },
      GetSubscription: new BorgError(GrpcStatus.Unavailable, "Borg is restarting"),
    });
    mount(daemon);
    await pick("Datadog");
    click(screen.getByRole("button", { name: "Create" }));
    await waitFor(() => expect(screen.getByText("local")).toBeTruthy());
    click(screen.getByRole("button", { name: "Turn on" }));
    await waitFor(() =>
      expect(screen.getByRole("alert").textContent).toBe(
        "Could not read the new address: Borg is restarting",
      ),
    );
  });
});

describe("NewSubscription: keyboard and screen readers", () => {
  it("moves the single delivery choice with the arrow keys, one tab stop for the group", async () => {
    mount(fakeDaemon(listing([])));
    await pick("Datadog");
    const where = screen.getByRole("radiogroup", { name: "Where it reports" });
    const radios = within(where).getAllByRole("radio");
    expect(radios.map((r) => r.tabIndex)).toEqual([0, -1, -1, -1]);
    act(() => radios[0]!.focus());
    fireEvent.keyDown(radios[0]!, { key: "ArrowRight" });
    const slack = within(where).getByRole("radio", { name: "Slack" });
    expect(slack.getAttribute("aria-checked")).toBe("true");
    expect(document.activeElement).toBe(slack);
    expect(screen.getByLabelText("Who gets it on Slack")).toBeTruthy();
    // Back past the start wraps to the end.
    fireEvent.keyDown(slack, { key: "ArrowLeft" });
    fireEvent.keyDown(within(where).getByRole("radio", { name: "Only here" }), {
      key: "ArrowUp",
    });
    expect(within(where).getByRole("radio", { name: "Discord" }).getAttribute("aria-checked")).toBe(
      "true",
    );
    // Other keys are left alone.
    fireEvent.keyDown(within(where).getByRole("radio", { name: "Discord" }), { key: "a" });
    expect(within(where).getByRole("radio", { name: "Discord" }).getAttribute("aria-checked")).toBe(
      "true",
    );
  });

  it("puts focus on the first condition More conditions reveals", async () => {
    mount(fakeDaemon(listing([])));
    await pick("GitHub");
    const more = screen.getByRole("button", { name: "More conditions" });
    expect(more.hasAttribute("aria-expanded")).toBe(false);
    act(() => more.focus());
    click(more);
    expect(document.activeElement).toBe(screen.getByLabelText("Branch"));
  });
});
