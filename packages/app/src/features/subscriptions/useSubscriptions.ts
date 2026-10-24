import type { BannerAlert } from "@borg/ui";
import type { admin } from "@borg/client";
import { useCallback, useEffect, useRef, useState } from "react";
import { plural } from "../../lib/format";
import {
  useBorg,
  useMutation,
  useQuery,
  useRetryingServerStream,
  type Mutation,
  type QueryResult,
} from "../../lib/useDaemon";

/**
 * Subscriptions: a source, a filter, an instruction and where to report. An
 * event that matches wakes Borg in the subscription's own conversation.
 *
 * The daemon owns every rule; this module turns its rows into words and keeps
 * the screens current from the live activity stream.
 */

export type SubscriptionTone = "ok" | "busy" | "paused" | "alert" | "idle";

/** The name a delivery channel id shows as: `slack` → `Slack`. */
export function channelLabel(id: string): string {
  return id.charAt(0).toUpperCase() + id.slice(1);
}

/** Reply on the channel the message came from. Channel sources only. */
export const ORIGIN_DELIVERY = "origin";

/**
 * The steps a later read hands back, with a key the screen already holds put
 * back in where the daemon wrote `placeholder` (it names the text it uses).
 */
export function stepsWithKey(steps: readonly string[], key: string, placeholder: string): string[] {
  return key === "" || placeholder === ""
    ? [...steps]
    : steps.map((s) => s.split(placeholder).join(key));
}

/** True for a source that has an address a service posts to. */
export function hasEndpoint(sub: Pick<admin.Subscription, "endpoint">): boolean {
  return (sub.endpoint?.url ?? "") !== "";
}

/** Paused until the user pastes the secret the provider shows after setup. */
export function waitingForSecret(
  sub: Pick<admin.Subscription, "status" | "hasSigningSecret">,
  preset: Pick<admin.SubscriptionPreset, "takesSigningSecret"> | undefined,
): boolean {
  return sub.status === "paused" && !!preset?.takesSigningSecret && !sub.hasSigningSecret;
}

/** Storm, or a listening subscription whose last run failed: something the user should see. */
export function needsAttention(sub: Pick<admin.Subscription, "status" | "lastRunStatus">): boolean {
  return sub.status === "storm" || (sub.status === "active" && sub.lastRunStatus === "failed");
}

/** Dot tone for a row. */
export function toneOf(sub: admin.Subscription): SubscriptionTone {
  if (sub.status === "storm") return "alert";
  if (sub.status !== "active") return "paused";
  if (sub.lastRunStatus === "failed") return "alert";
  if (sub.lastRunStatus === "running" || sub.lastRunStatus === "pending") return "busy";
  // A test run is not an event from the source: until one arrives, the
  // subscription has never been proven to hear anything.
  if (sub.lastEventAtUnix === 0) return "idle";
  return "ok";
}

/** "just now", "3 min ago", "2 hours ago", "4 days ago". */
export function ago(unixSecs: number, nowMs: number = Date.now()): string {
  const secs = Math.max(0, nowMs / 1000 - unixSecs);
  if (secs < 45) return "just now";
  if (secs < 3_600) return `${Math.max(1, Math.round(secs / 60))} min ago`;
  if (secs < 86_400) return `${plural(Math.floor(secs / 3_600), "hour")} ago`;
  return `${plural(Math.floor(secs / 86_400), "day")} ago`;
}

/** One line that says whether it works, for the row and the detail header. */
export function wordsOf(
  sub: admin.Subscription,
  preset: Pick<admin.SubscriptionPreset, "takesSigningSecret"> | undefined,
  nowMs: number = Date.now(),
): string {
  if (sub.status === "storm") {
    return sub.lastRunStatus === "failed"
      ? "Paused itself — its runs kept failing"
      : "Paused itself — too many events";
  }
  if (sub.status !== "active") {
    return waitingForSecret(sub, preset) ? "Waiting for the signing secret" : "Paused";
  }
  if (sub.lastRunStatus === "failed") {
    return sub.lastError ? `Last run failed: ${sub.lastError}` : "Last run failed";
  }
  if (sub.lastRunStatus === "running") return "Working on an event now";
  if (sub.lastEventAtUnix === 0) return "Listening for the first event";
  return `Last event ${ago(sub.lastEventAtUnix, nowMs)}`;
}

/** The banner for the first subscription that needs the user, if any. */
export function attentionAlert(
  subs: readonly admin.Subscription[],
): { sub: admin.Subscription; alert: BannerAlert } | undefined {
  const sub = subs.find(needsAttention);
  if (!sub) return undefined;
  if (sub.status === "storm") {
    return {
      sub,
      alert: {
        title: `${sub.name} paused itself`,
        stackedTitle: `${sub.name} paused itself`,
        sub:
          sub.lastRunStatus === "failed"
            ? "Its runs kept failing, so it stopped to save model usage. Look at the last run, then resume it."
            : "A burst of events arrived, so it stopped to save model usage. Narrow what it listens for, then resume it.",
        action: "Review",
      },
    };
  }
  return {
    sub,
    alert: {
      title: `${sub.name}: the last run failed`,
      stackedTitle: `${sub.name} needs you`,
      sub: sub.lastError || "Open it to see what went wrong.",
      action: "Review",
    },
  };
}

/* ── Filters ────────────────────────────────────────────────────────────── */

/** A filter being edited. List fields typed as text stay a string until saved. */
export type FilterDraft = Record<string, string | string[] | boolean>;

/** Filter keys whose value is a list. */
export const LIST_FIELDS: ReadonlySet<string> = new Set(["kinds", "repos", "labels"]);
/** Filter keys that are on or off. */
export const BOOL_FIELDS: ReadonlySet<string> = new Set(["mention_only", "exclude_drafts"]);

/** A stored `filter_json` as a draft. Anything unreadable starts empty. */
export function filterDraftOf(json: string): FilterDraft {
  let parsed: unknown;
  try {
    parsed = JSON.parse(json || "{}");
  } catch {
    return {};
  }
  if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) return {};
  const draft: FilterDraft = {};
  for (const [key, value] of Object.entries(parsed)) {
    if (typeof value === "string" || typeof value === "boolean") draft[key] = value;
    else if (Array.isArray(value)) draft[key] = value.map(String);
  }
  return draft;
}

function listOf(value: string | string[]): string[] {
  const items = Array.isArray(value) ? value : value.split(",");
  return items.map((s) => s.trim()).filter((s) => s !== "");
}

/**
 * The draft as `filter_json`: only the preset's own keys, and none that are
 * empty, so "no condition" is an absent key rather than a blank one the
 * daemon has to interpret.
 */
export function filterJsonOf(draft: FilterDraft, fields: readonly string[]): string {
  const out: Record<string, string | string[] | boolean> = {};
  for (const field of fields) {
    const value = draft[field];
    if (value === undefined) continue;
    if (BOOL_FIELDS.has(field)) {
      if (value === true) out[field] = true;
    } else if (LIST_FIELDS.has(field)) {
      const items = typeof value === "boolean" ? [] : listOf(value);
      if (items.length > 0) out[field] = items;
    } else if (typeof value === "string" && value.trim() !== "") {
      out[field] = value.trim();
    }
  }
  return JSON.stringify(out);
}

/* ── Requests ───────────────────────────────────────────────────────────── */

/** A change with every field at its "leave it alone" value, then `over`. */
export function changeOf(
  id: string,
  over: Partial<Omit<admin.SubscriptionChange, "id">>,
): admin.SubscriptionChange {
  return {
    id,
    name: "",
    filterJson: "",
    instruction: "",
    setDelivery: false,
    deliveryChannel: "",
    deliveryTarget: "",
    rateLimitPerHour: 0,
    timeoutSecs: 0,
    setSigningSecret: false,
    signingSecret: "",
    ...over,
  };
}

/* ── Live activity ──────────────────────────────────────────────────────── */

/** What the stream has said about one subscription since the screen opened. */
export interface Heard {
  /** Events that reached its endpoint (or channel) — the proof it is connected. */
  received: number;
  /** Runs that finished. */
  finished: number;
}

export type Activity = Readonly<Record<string, Heard>>;

/** How long after the last activity frame a screen re-reads. A burst costs one read. */
export const ACTIVITY_REFRESH_MS = 250;

/**
 * Counts the live activity per subscription and calls `refresh` once a burst
 * of frames has passed. A reconnect re-reads too: whatever happened while the
 * stream was down is in the rows, not in the frames.
 */
export function useSubscriptionActivity(
  refresh: () => void,
  matches: (subscriptionId: string) => boolean = () => true,
): Activity {
  const [heard, setHeard] = useState<Activity>({});
  const refreshRef = useRef(refresh);
  refreshRef.current = refresh;
  const matchesRef = useRef(matches);
  matchesRef.current = matches;
  const timer = useRef<ReturnType<typeof setTimeout>>(undefined);
  useEffect(() => () => clearTimeout(timer.current), []);

  const schedule = useCallback(() => {
    clearTimeout(timer.current);
    timer.current = setTimeout(() => refreshRef.current(), ACTIVITY_REFRESH_MS);
  }, []);

  const stream = useRetryingServerStream<admin.SubscriptionActivityEvent>(
    (borg, handlers) => borg.admin.subscribeSubscriptionActivity({}, handlers),
    (event) => {
      if (!matchesRef.current(event.subscriptionId)) return;
      setHeard((prev) => {
        const was = prev[event.subscriptionId] ?? { received: 0, finished: 0 };
        return {
          ...prev,
          [event.subscriptionId]: {
            received: was.received + (event.kind === "received" ? 1 : 0),
            finished: was.finished + (event.kind === "finished" ? 1 : 0),
          },
        };
      });
      schedule();
    },
  );

  // The first open is the mount, which has just read; only a re-open is news.
  const wasOpen = useRef(true);
  useEffect(() => {
    const open = stream.status === "open";
    if (open && !wasOpen.current) schedule();
    wasOpen.current = open;
  }, [stream.status, schedule]);

  return heard;
}

/* ── Writes ─────────────────────────────────────────────────────────────── */

export interface SubscriptionWrites {
  create: Mutation<admin.SubscriptionSpec, admin.SubscriptionCreated>;
  setActive: Mutation<{ id: string; active: boolean }, admin.Subscription>;
  update: Mutation<admin.SubscriptionChange, admin.Subscription>;
  remove: Mutation<string, unknown>;
  test: Mutation<string, admin.SubscriptionEvent>;
  reveal: Mutation<string, admin.SubscriptionKey>;
  rotate: Mutation<string, admin.SubscriptionKey>;
}

/** Every write, each re-reading what it changed. */
export function useSubscriptionWrites(refresh: () => void): SubscriptionWrites {
  const after = { onSuccess: () => refresh() };
  return {
    create: useMutation((borg, spec) => borg.admin.createSubscription(spec), after),
    setActive: useMutation((borg, vars) => borg.admin.setSubscriptionStatus(vars), after),
    update: useMutation((borg, change) => borg.admin.updateSubscription(change), after),
    remove: useMutation((borg, id) => borg.admin.deleteSubscription({ id }), after),
    test: useMutation((borg, id) => borg.admin.testSubscription({ id }), after),
    reveal: useMutation((borg, id) => borg.admin.revealSubscriptionKey({ id })),
    rotate: useMutation((borg, id) => borg.admin.rotateSubscriptionKey({ id }), after),
  };
}

/* ── Reads ──────────────────────────────────────────────────────────────── */

export interface SubscriptionsData {
  subscriptions: admin.Subscription[];
  presets: admin.SubscriptionPreset[];
  /** Channels a report can go to here: supported and set up. */
  deliveryChannels: string[];
  keyPlaceholder: string;
}

export interface Subscriptions extends SubscriptionWrites {
  query: QueryResult<SubscriptionsData>;
  activity: Activity;
}

/** Every subscription and the sources a new one can pick from, kept live. */
export function useSubscriptions(deps: unknown[] = []): Subscriptions {
  const query = useQuery(async (borg): Promise<SubscriptionsData> => {
    const [list, presets] = await Promise.all([
      borg.admin.listSubscriptions({}),
      borg.admin.listSubscriptionPresets({}),
    ]);
    return {
      subscriptions: list.subscriptions,
      presets: presets.presets,
      deliveryChannels: presets.deliveryChannels ?? [],
      keyPlaceholder: presets.keyPlaceholder ?? "",
    };
  }, deps);
  const activity = useSubscriptionActivity(query.refetch);
  return { query, activity, ...useSubscriptionWrites(query.refetch) };
}

export interface SubscriptionDetailData {
  detail: admin.SubscriptionDetail;
  sub: admin.Subscription | undefined;
  preset: admin.SubscriptionPreset | undefined;
  deliveryChannels: string[];
}

export interface SubscriptionView extends SubscriptionWrites {
  query: QueryResult<SubscriptionDetailData>;
  activity: Heard;
}

/** One subscription with its recent events, re-read whenever it does something. */
export function useSubscription(id: string): SubscriptionView {
  const query = useQuery(
    async (borg): Promise<SubscriptionDetailData> => {
      const [detail, presets] = await Promise.all([
        borg.admin.getSubscription({ id }),
        borg.admin.listSubscriptionPresets({}),
      ]);
      const sub = detail.subscription;
      return {
        detail,
        sub,
        preset: presets.presets.find((p) => p.id === sub?.provider),
        deliveryChannels: presets.deliveryChannels ?? [],
      };
    },
    [id],
  );
  const activity = useSubscriptionActivity(query.refetch, (sid) => sid === id);
  return {
    query,
    activity: activity[id] ?? { received: 0, finished: 0 },
    ...useSubscriptionWrites(query.refetch),
  };
}

/* ── Tunnel ─────────────────────────────────────────────────────────────── */

/** How often a started tunnel is asked for its public address. */
export const TUNNEL_POLL_MS = 1500;
/** Asks before giving up, about thirty seconds. */
export const TUNNEL_POLL_LIMIT = 20;

export interface Tunnel {
  turnOn: () => Promise<void>;
  /** From the press until the address is public (or the wait gives up). */
  pending: boolean;
  /** Why it did not come up, in words the user can act on. */
  error: string | undefined;
  /** No tunnel is chosen in settings, so there is nothing to start. */
  unconfigured: boolean;
}

/**
 * Starts the tunnel so a service on the internet can reach Borg, waits for it
 * to report its public address, then calls `onReady` so the screen re-reads
 * the endpoint.
 */
export function useTunnel(onReady: () => void): Tunnel {
  const borg = useBorg();
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string>();
  const [unconfigured, setUnconfigured] = useState(false);
  const live = useRef(true);
  useEffect(() => {
    live.current = true;
    return () => {
      live.current = false;
    };
  }, []);

  const turnOn = useCallback(async () => {
    setPending(true);
    setError(undefined);
    setUnconfigured(false);
    try {
      let reply = await borg.admin.tunnelStart({});
      for (let i = 0; i < TUNNEL_POLL_LIMIT && live.current; i++) {
        if (reply.error || reply.publicUrl || !reply.running) break;
        await new Promise((resolve) => setTimeout(resolve, TUNNEL_POLL_MS));
        reply = await borg.admin.tunnelStatus({});
      }
      if (!live.current) return;
      if (reply.provider === "none" || reply.provider === "") {
        setUnconfigured(true);
      } else if (reply.error) {
        setError(reply.error);
      } else if (!reply.publicUrl) {
        setError("The tunnel did not report a public address yet. Try again in a moment.");
      }
      onReady();
    } catch (e) {
      if (live.current) setError(e instanceof Error ? e.message : String(e));
    } finally {
      if (live.current) setPending(false);
    }
  }, [borg, onReady]);

  return { turnOn, pending, error, unconfigured };
}
