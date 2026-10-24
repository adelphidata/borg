import { describe, expect, it } from "vitest";
import { DATADOG, SENTRY, subscription } from "../../test/subscriptions";
import {
  ago,
  attentionAlert,
  changeOf,
  filterDraftOf,
  filterJsonOf,
  needsAttention,
  stepsWithKey,
  toneOf,
  wordsOf,
} from "./useSubscriptions";

const NOW = 1_800_000_000_000;
const nowSecs = NOW / 1000;

describe("toneOf and wordsOf", () => {
  // One row per state a person has to tell apart at a glance.
  const cases: [string, Parameters<typeof subscription>[0], string, string][] = [
    ["never heard anything", {}, "idle", "Listening for the first event"],
    [
      "heard something a while ago",
      { lastEventAtUnix: nowSecs - 180, lastRunStatus: "succeeded" },
      "ok",
      "Last event 3 min ago",
    ],
    [
      "working now",
      { lastEventAtUnix: nowSecs, lastRunStatus: "running" },
      "busy",
      "Working on an event now",
    ],
    [
      "last run failed",
      { lastEventAtUnix: nowSecs, lastRunStatus: "failed", lastError: "model timed out" },
      "alert",
      "Last run failed: model timed out",
    ],
    ["last run failed, no reason", { lastRunStatus: "failed" }, "alert", "Last run failed"],
    ["paused by the user", { status: "paused" }, "paused", "Paused"],
    ["paused itself in a burst", { status: "storm" }, "alert", "Paused itself — too many events"],
    [
      "paused itself after failures",
      { status: "storm", lastRunStatus: "failed" },
      "alert",
      "Paused itself — its runs kept failing",
    ],
  ];
  it.each(cases)("%s", (_name, over, tone, words) => {
    const sub = subscription(over);
    expect(toneOf(sub)).toBe(tone);
    expect(wordsOf(sub, DATADOG, NOW)).toBe(words);
  });

  it("says a provider that signs is waiting for its secret, until one is stored", () => {
    const waiting = subscription({ provider: "sentry", status: "paused" });
    expect(wordsOf(waiting, SENTRY, NOW)).toBe("Waiting for the signing secret");
    expect(wordsOf({ ...waiting, hasSigningSecret: true }, SENTRY, NOW)).toBe("Paused");
    // A provider that signs with Borg's own key never waits.
    expect(wordsOf(waiting, DATADOG, NOW)).toBe("Paused");
  });
});

describe("ago", () => {
  it.each([
    [10, "just now"],
    [90, "2 min ago"],
    [3 * 3_600 + 5, "3 hours ago"],
    [86_400 * 2, "2 days ago"],
  ])("%i seconds back reads %s", (secs, words) => {
    expect(ago(nowSecs - secs, NOW)).toBe(words);
  });

  it("does not report the future as negative time", () => {
    expect(ago(nowSecs + 30, NOW)).toBe("just now");
  });
});

describe("needsAttention and attentionAlert", () => {
  it("lights for a storm and for a listening subscription whose last run failed", () => {
    expect(needsAttention(subscription({ status: "storm" }))).toBe(true);
    expect(needsAttention(subscription({ lastRunStatus: "failed" }))).toBe(true);
    // Paused by hand: the failure is old news the user already acted on.
    expect(needsAttention(subscription({ status: "paused", lastRunStatus: "failed" }))).toBe(false);
    expect(needsAttention(subscription())).toBe(false);
  });

  it("names the first one that needs the user and says what happened", () => {
    const ok = subscription({ id: "a", name: "Quiet" });
    const storm = subscription({ id: "b", name: "Sentry errors", status: "storm" });
    const found = attentionAlert([ok, storm]);
    expect(found?.sub.id).toBe("b");
    expect(found?.alert.title).toBe("Sentry errors paused itself");
    expect(found?.alert.sub).toContain("burst");

    const failed = attentionAlert([
      subscription({ name: "Inbox", lastRunStatus: "failed", lastError: "no credits left" }),
    ]);
    expect(failed?.alert.title).toBe("Inbox: the last run failed");
    expect(failed?.alert.sub).toBe("no credits left");
    expect(attentionAlert([ok])).toBeUndefined();
  });
});

describe("filters", () => {
  it("writes only the preset's keys, and none that are empty", () => {
    const json = filterJsonOf(
      {
        kinds: ["alert.triggered"],
        severity_min: "",
        phrase: "  deploy  ",
        repos: "acme/web, , acme/api",
        mention_only: false,
        exclude_drafts: true,
        sender: "not-a-field-here",
      },
      ["kinds", "severity_min", "phrase", "repos", "mention_only", "exclude_drafts"],
    );
    expect(JSON.parse(json)).toEqual({
      kinds: ["alert.triggered"],
      phrase: "deploy",
      repos: ["acme/web", "acme/api"],
      exclude_drafts: true,
    });
  });

  it("writes no conditions as an empty object", () => {
    expect(filterJsonOf({ kinds: [] }, ["kinds"])).toBe("{}");
  });

  it("keeps the stored keys the form does not show when it saves an edit", () => {
    const stored = JSON.stringify({
      kinds: ["push"],
      phrase: "deploy",
      field_equals: [{ path: "repository.private", value: true }],
    });
    const json = filterJsonOf({ kinds: ["pull_request"] }, ["kinds", "repos"], stored);
    expect(JSON.parse(json)).toEqual({
      kinds: ["pull_request"],
      phrase: "deploy",
      field_equals: [{ path: "repository.private", value: true }],
    });
    // A form field cleared in the draft is cleared, not kept from the stored one.
    expect(JSON.parse(filterJsonOf({ kinds: [] }, ["kinds"], stored))).not.toHaveProperty("kinds");
    expect(filterJsonOf({}, ["kinds"], "not json")).toBe("{}");
  });

  it("reads a stored filter back, and starts empty from anything unreadable", () => {
    expect(filterDraftOf('{"kinds":["push"],"mention_only":true,"phrase":"hi","n":3}')).toEqual({
      kinds: ["push"],
      mention_only: true,
      phrase: "hi",
    });
    for (const bad of ["", "not json", "[1]", "null"]) expect(filterDraftOf(bad)).toEqual({});
  });
});

describe("requests", () => {
  it("leaves every field it is not told about unchanged", () => {
    expect(changeOf("sub_1", { name: "New" })).toEqual({
      id: "sub_1",
      name: "New",
      filterJson: "",
      instruction: "",
      setDelivery: false,
      deliveryChannel: "",
      deliveryTarget: "",
      rateLimitPerHour: 0,
      timeoutSecs: 0,
      setSigningSecret: false,
      signingSecret: "",
    });
  });

  it("puts the key in hand back into masked steps", () => {
    expect(
      stepsWithKey(
        ["Header: Bearer YOUR_KEY", "URL https://borg:YOUR_KEY@h/x"],
        "bsk_9",
        "YOUR_KEY",
      ),
    ).toEqual(["Header: Bearer bsk_9", "URL https://borg:bsk_9@h/x"]);
    expect(stepsWithKey(["Bearer YOUR_KEY"], "", "YOUR_KEY")).toEqual(["Bearer YOUR_KEY"]);
    // An older daemon names no placeholder: the steps stay as they came.
    expect(stepsWithKey(["Bearer YOUR_KEY"], "bsk_9", "")).toEqual(["Bearer YOUR_KEY"]);
  });
});
