import { describe, expect, it } from "vitest";
import { classifyFailure, namedSetting, nextLimit, retryHint } from "./sendFailure";

describe("classifyFailure — the layer", () => {
  it.each([
    // Real provider and OS wording, not paraphrases: the classifier only ever
    // sees the string some other layer chose.
    ["SQLITE_FULL: database or disk is full", "disk"],
    ["Error: ENOSPC: no space left on device, write", "disk"],
    ["write failed: No space left on device", "disk"],
    ["EROFS: read-only file system, open '/home/u/.borg/borg.db'", "permissions"],
    ["attempt to write a readonly database", "permissions"],
    // A sandbox or tool denial is the machine refusing, not a provider.
    ["tool failed: /usr/bin/foo: Permission denied", "permissions"],
    ["429 Too Many Requests", "rate-limit"],
    ["Rate limit reached for gpt-4o. Please check your plan and billing details.", "rate-limit"],
    ["Error 402: payment required", "billing"],
    ["insufficient_quota: you exceeded your current quota", "billing"],
    ["status 401: invalid api key", "auth"],
    ["403 Forbidden", "auth"],
    ["model_not_found: gpt-9 does not exist", "model"],
    ["transport error: connection reset by peer", "connection"],
    ["deadline exceeded", "connection"],
    ["internal: assertion failed at frame 12", "unknown"],
  ] as const)("files %j under the %s layer", (raw, layer) => {
    expect(classifyFailure({ message: raw })!.layer).toBe(layer);
  });

  it("reads a rate limit as a rate limit even when its body mentions billing", () => {
    // The regression that made this a regex: a provider's ordinary 429 body
    // says "check your plan and billing details". Read as billing, the single
    // most common transient failure would be branded unpayable and its retry
    // withheld — the one case where trying again is exactly right.
    const failure = classifyFailure({
      message: "429 Too Many Requests: retry after 20s (see billing dashboard)",
    })!;
    expect(failure.layer).toBe("rate-limit");
    expect(failure.retryable).toBe(true);
  });

  it("still reads a real payment refusal as billing", () => {
    const failure = classifyFailure({
      message: "402 payment required: api key has insufficient credit",
    })!;
    expect(failure.layer).toBe("billing");
  });

  it.each([
    // Every one of these was misfiled while status codes matched as bare
    // substrings: an id or a token count is not an HTTP status.
    "session 402a9f does not exist",
    "context length exceeded: 4029 tokens > limit",
    "request req_4031f2 failed: internal server error",
  ])("does not read a number inside an id as a status code (%j)", (raw) => {
    expect(classifyFailure({ message: raw })!.layer).toBe("unknown");
  });

  it.each([
    // The transcript shows tool and database failures through this same card,
    // so "does not exist" cannot be allowed to mean "your model is gone".
    "libsql error: table sessions does not exist",
    "patch failed: file src/main.rs does not exist",
  ])("does not blame the model for something else that is missing (%j)", (raw) => {
    expect(classifyFailure({ message: raw })!.layer).toBe("unknown");
  });

  it.each([
    // The transcript routes tool and sub-agent failures through this same card.
    // "Check the network" is confidently wrong for every one of these.
    "run_shell: command timed out after 120s",
    "sub-agent stalled: no progress for 450s, timed out",
    "Timeout waiting for the sandbox to start",
  ])("does not call a local timeout a network problem (%j)", (raw) => {
    expect(classifyFailure({ message: raw })!.layer).toBe("unknown");
  });

  it("still reads a network timeout as a network timeout", () => {
    expect(classifyFailure({ message: "request timed out after 30s" })!.layer).toBe("connection");
  });

  it("recognises a missing model through its dotted name", () => {
    // `gemini-2.5-pro` is the ordinary shape of the thing in the middle.
    expect(
      classifyFailure({ message: "Model gemini-2.5-pro not found for API version v1beta" })!.layer,
    ).toBe("model");
  });

  it("matches however the layer that noticed happened to capitalise it", () => {
    expect(classifyFailure({ message: "Disk Full" })!.layer).toBe(
      classifyFailure({ message: "disk full" })!.layer,
    );
  });

  it("classifies nothing when the turn succeeded", () => {
    expect(classifyFailure(undefined)).toBeUndefined();
  });
});

describe("classifyFailure — the actions it earns", () => {
  it("offers retry only where repeating the request could work", () => {
    // A Retry that is certain to fail again teaches the user the app is
    // guessing.
    expect(classifyFailure({ message: "429 rate limit" })!.retryable).toBe(true);
    expect(classifyFailure({ message: "connection refused" })!.retryable).toBe(true);
    expect(classifyFailure({ message: "status 401 unauthorized" })!.retryable).toBe(false);
    expect(classifyFailure({ message: "no space left on device" })!.retryable).toBe(false);
  });

  it("points at settings only where a setting is the fix", () => {
    expect(classifyFailure({ message: "invalid api key" })!.settings).toBe(true);
    expect(classifyFailure({ message: "insufficient_quota" })!.settings).toBe(true);
    // Nothing in Settings frees disk space, reconnects a network, or chmods a
    // path.
    expect(classifyFailure({ message: "disk is full" })!.settings).toBe(false);
    expect(classifyFailure({ message: "socket hang up" })!.settings).toBe(false);
    expect(classifyFailure({ message: "Permission denied" })!.settings).toBe(false);
  });

  it("names the next action in every message it writes", () => {
    const raws = [
      "no space left on device",
      "Permission denied",
      "429 rate limit",
      "402 payment required",
      "401 unauthorized",
      "model_not_found",
      "connection refused",
    ];
    for (const raw of raws) {
      // The point of the card is the next action, not the diagnosis.
      expect(classifyFailure({ message: raw })!.message.toLowerCase()).toMatch(
        /try again|free up space|add credit|set a working key|pick another model|check the/,
      );
    }
  });

  it("gives an unexplained failure no title, and keeps its text", () => {
    const failure = classifyFailure({ message: "internal: assertion failed at frame 12" })!;
    expect(failure.title).toBeUndefined();
    expect(failure.message).toBe("internal: assertion failed at frame 12");
    // Unknown means unclassified, not unrecoverable: the user is left the one
    // action that costs nothing to try.
    expect(failure.retryable).toBe(true);
  });
});

describe("classifyFailure — the daemon's own diagnosis", () => {
  it.each([
    // The exact texts `classify_status` writes, which carry the daemon's
    // reading of the response body in brackets after the status.
    [
      "anthropic returned 403 (request blocked by Cloudflare — usually a restricted region or a flagged VPN/proxy; try a different network or region): <html>Attention Required!</html>",
      { layer: "connection", retryable: false, settings: true },
    ],
    [
      "openrouter returned 429 (billing error: quota or spend limit exhausted): insufficient credits",
      { layer: "billing", retryable: false, settings: true },
    ],
    [
      "openrouter returned 402 (billing error): insufficient_quota",
      { layer: "billing", retryable: false, settings: true },
    ],
    [
      "anthropic returned 402 (usage limit): usage limit reached, try again in 5 minutes",
      { layer: "rate-limit", retryable: true, settings: false },
    ],
    [
      "openai returned 429 (rate limited): slow down",
      { layer: "rate-limit", retryable: true, settings: false },
    ],
    [
      "anthropic returned 403 (auth permanent): forbidden",
      { layer: "auth", retryable: false, settings: true },
    ],
  ] as const)("trusts %j over the bare status code", (raw, expected) => {
    expect(classifyFailure({ message: raw })).toMatchObject(expected);
  });

  it("tells the user the network was blocked, and where the switch is", () => {
    const failure = classifyFailure({
      message:
        "anthropic returned 403 (request blocked by Cloudflare — try a different network): x",
    })!;
    expect(failure.message).toBe(
      "Your network or region was blocked by the provider. Try another network, or switch provider in Settings.",
    );
  });
});

describe("classifyFailure — the provider's own wait", () => {
  it.each([
    ["usage limit reached, try again in 5 minutes", "Try again in about 5 min."],
    ["please retry after 45 seconds", "Try again in about 45 seconds."],
    ["429 Too Many Requests: retry after 20s", "Try again in about 20 seconds."],
    ["rate limited; try again in 1 second", "Try again in about 1 second."],
    ["rate limited; try again in 1 min", "Try again in about 1 min."],
    ["rate limited; retry after 300 seconds", "Try again in about 5 min."],
    // Providers name sub-second precision; a wait is only ever rounded up.
    ["Rate limit reached. Please try again in 1.234s.", "Try again in about 2 seconds."],
    ["rate limited; try again in 0.4s", "Try again in about 1 second."],
    ["quota exhausted; retry in 1.5 minutes", "Try again in about 2 min."],
  ])("turns %j into %j", (raw, hint) => {
    expect(retryHint(raw)).toBe(hint);
    const failure = classifyFailure({ message: `429 ${raw}` })!;
    expect(failure.message.endsWith(hint)).toBe(true);
    expect(failure.message).not.toContain("Wait a moment");
  });

  it.each(["429 too many requests", "429 retry after a while", "429 try again in 0 seconds"])(
    "keeps the plain wording when no wait is named (%j)",
    (raw) => {
      expect(retryHint(raw)).toBeUndefined();
      expect(classifyFailure({ message: raw })!.message).toContain("Wait a moment");
    },
  );

  it("adds no wait to a failure that is not a throttle", () => {
    const failure = classifyFailure({ message: "402 payment required, try again in 5 minutes" })!;
    expect(failure.layer).toBe("billing");
    expect(failure.message).not.toContain("Try again in about");
  });
});

const BUDGET =
  "Monthly token budget exceeded (1013994/1000000). Increase budget.monthly_token_limit in /settings to continue.";

describe("classifyFailure — Borg's own monthly limit", () => {
  it("names the limit in plain numbers instead of the setting key", () => {
    const failure = classifyFailure({ message: BUDGET })!;
    expect(failure.layer).toBe("budget");
    expect(failure.message).toBe(
      "Borg has used 1,013,994 of its 1,000,000 tokens this month. Raise the limit to keep going.",
    );
    expect(failure.message).not.toContain("budget.monthly_token_limit");
  });

  it("is not read as a provider rate limit or bill", () => {
    // The text says "budget" and "limit"; neither the provider's 429 nor its
    // billing page has anything to do with it.
    expect(classifyFailure({ message: BUDGET })!.layer).toBe("budget");
  });

  it("offers a raise that clears what is already spent", () => {
    const failure = classifyFailure({ message: BUDGET })!;
    expect(failure.retryable).toBe(false);
    expect(failure.fix).toMatchObject({
      key: "budget.monthly_token_limit",
      suggested: "2000000",
      action: "Raise limit",
    });
    expect(failure.fix!.done!("2000000")).toBe("Limit raised to 2,000,000.");
    expect(failure.fix!.done!("0")).toBe("Limit removed.");
  });

  it("still points at the setting when the numbers are missing", () => {
    const failure = classifyFailure({ message: "Monthly token budget exceeded." })!;
    expect(failure.message).toBe(
      "Borg has used all of its tokens for this month. Raise the limit to keep going.",
    );
    expect(failure.fix).toMatchObject({ key: "budget.monthly_token_limit" });
    expect(failure.fix!.suggested).toBeUndefined();
  });
});

describe("nextLimit", () => {
  it.each([
    [1_013_994, 1_000_000, "2000000"],
    // Usage far past the old limit (the limit was lowered mid-month).
    [5_000_000, 1_000_000, "8000000"],
    // A zero limit has no double; start from one.
    [10, 0, "16"],
  ])("used %d of %d → %s", (used, limit, next) => {
    expect(nextLimit(used, limit)).toBe(next);
  });
});

describe("namedSetting", () => {
  it.each([
    [BUDGET, "budget.monthly_token_limit"],
    [
      "Provider `custom` needs `llm.base_url` pointing at an endpoint. Set it via `/settings` or `borg settings set llm.base_url <url>`.",
      "llm.base_url",
    ],
    ["TTS is not configured. Enable it via: borg settings set tts.enabled true", "tts.enabled"],
    ["Usage: /settings gateway.port 7842", "gateway.port"],
  ])("finds the key in %j", (raw, key) => {
    expect(namedSetting(raw)).toBe(key);
  });

  it.each([
    "could not read ~/.borg/hooks.json",
    "Try switching the model or provider in /settings",
    "connection refused",
  ])("finds nothing in %j", (raw) => {
    expect(namedSetting(raw)).toBeUndefined();
  });

  it("gives an unclassified failure that names a key the way to it", () => {
    const failure = classifyFailure({
      message: "gateway.host is empty. Set it with `borg settings set gateway.host 127.0.0.1`",
    })!;
    expect(failure.layer).toBe("unknown");
    expect(failure.settings).toBe(true);
    expect(failure.fix).toEqual({ key: "gateway.host" });
  });

  it("does not attach a key to a failure no setting can fix", () => {
    const failure = classifyFailure({
      message: "no space left on device while running borg settings set data.dir /x",
    })!;
    expect(failure.layer).toBe("disk");
    expect(failure.fix).toBeUndefined();
  });
});
