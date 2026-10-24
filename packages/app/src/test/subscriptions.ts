import { act } from "@testing-library/react";
import type { admin } from "@borg/client";
import type { FakeDaemon } from "./harness";

/**
 * Subscription rows and presets shaped like the daemon's, for the
 * subscription screens' tests. Field sets mirror `cortex::subscriptions::presets`.
 */

export function preset(over: Partial<admin.SubscriptionPreset> = {}): admin.SubscriptionPreset {
  return {
    id: "custom",
    label: "Custom webhook",
    blurb: "Anything that can POST JSON",
    group: "Custom",
    source: "webhook",
    authScheme: "bearer",
    takesSigningSecret: false,
    kinds: [],
    filterFields: ["kinds", "severity_min"],
    defaultName: "Custom events",
    defaultInstruction: "Look at the event and decide whether it needs attention.",
    samplePayload: "{}",
    ...over,
  };
}

export const SENTRY = preset({
  id: "sentry",
  label: "Sentry",
  blurb: "New issues and alert rules",
  group: "Alerts",
  authScheme: "sentry",
  takesSigningSecret: true,
  kinds: ["issue.created", "issue.resolved"],
  defaultName: "Sentry errors",
  defaultInstruction: "Open the Sentry issue and post a one-paragraph diagnosis.",
});

export const DATADOG = preset({
  id: "datadog",
  label: "Datadog",
  blurb: "Monitor alerts and recoveries",
  group: "Alerts",
  kinds: ["alert.triggered", "alert.recovered"],
  defaultName: "Datadog alerts",
  defaultInstruction: "Look at the alert and say whether it needs a human now.",
});

export const GITHUB = preset({
  id: "github",
  label: "GitHub",
  blurb: "Pull requests, issues, pushes, CI runs",
  group: "Code",
  authScheme: "github",
  kinds: ["pull_request", "issues", "push"],
  filterFields: ["kinds", "repos", "branch", "labels", "author", "exclude_drafts"],
  defaultName: "GitHub activity",
  defaultInstruction: "Review the pull request.",
});

export const SLACK = preset({
  id: "slack",
  label: "Slack",
  blurb: "Messages in Slack channels",
  group: "Messages",
  source: "channel",
  authScheme: "none",
  kinds: ["message"],
  filterFields: ["channel_id", "phrase", "mention_only", "reaction", "sender"],
  defaultName: "Slack messages",
  defaultInstruction: "Answer the message if you can.",
});

export const POSTMARK = preset({
  id: "postmark",
  label: "Postmark",
  blurb: "Email sent to an inbound address",
  group: "Email",
  source: "email",
  kinds: ["email.received"],
  filterFields: ["phrase"],
  defaultName: "Email to Borg",
  defaultInstruction: "Read the email.",
});

export const PRESETS = [GITHUB, SENTRY, SLACK, DATADOG, POSTMARK];

export const PUBLIC_URL = "https://borg.example.com/v1/events/sub_1";

export function subscription(over: Partial<admin.Subscription> = {}): admin.Subscription {
  return {
    id: "sub_1",
    name: "Datadog alerts",
    source: "webhook",
    provider: "datadog",
    status: "active",
    filterJson: "{}",
    instruction: "Look at the alert and say whether it needs a human now.",
    deliveryChannel: "",
    deliveryTarget: "",
    rateLimitPerHour: 60,
    timeoutSecs: 300,
    droppedCount: 0,
    createdAtUnix: 1_700_000_000,
    updatedAtUnix: 1_700_000_000,
    lastEventAtUnix: 0,
    lastRunStatus: "",
    lastError: "",
    hasSigningSecret: false,
    sessionKey: "subscription:sub_1",
    endpoint: { url: PUBLIC_URL, public: true, hint: "" },
    ...over,
  };
}

export function event(over: Partial<admin.SubscriptionEvent> = {}): admin.SubscriptionEvent {
  return {
    id: "evt_1",
    subscriptionId: "sub_1",
    kind: "alert.triggered",
    summary: "CPU above 90% on web-3",
    status: "succeeded",
    runId: "run_1",
    receivedAtUnix: Math.floor(Date.now() / 1000) - 120,
    startedAtUnix: 0,
    finishedAtUnix: 0,
    durationMs: 4_200,
    result: "",
    error: "",
    payload: '{"title":"CPU above 90%"}',
    payloadTruncated: false,
    runSize: 0,
    ...over,
  };
}

/** The listing pair every screen with the list reads. */
export function listing(subs: admin.Subscription[], presets = PRESETS) {
  return {
    ListSubscriptions: { subscriptions: subs },
    ListSubscriptionPresets: {
      presets,
      deliveryChannels: ["slack", "telegram", "discord"],
      keyPlaceholder: "YOUR_KEY",
    },
  };
}

/** Pushes one live activity frame down the newest activity stream. */
export function emitActivity(
  daemon: FakeDaemon,
  frame: Partial<admin.SubscriptionActivityEvent> & { subscriptionId: string; kind: string },
): void {
  const stream = daemon.streams.filter((s) => s.method === "SubscribeSubscriptionActivity").at(-1);
  if (!stream) throw new Error("no activity stream is open");
  act(() =>
    stream.handlers.onMessage({
      runId: "",
      status: "",
      summary: "",
      eventCount: 1,
      emittedAtUnix: Math.floor(Date.now() / 1000),
      ...frame,
    } as never),
  );
}
