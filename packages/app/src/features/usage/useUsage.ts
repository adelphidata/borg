import { useCallback, useEffect, useMemo, useState } from "react";
import type { telemetry as telemetryPb } from "@borg/client";
import { useQuery, useRetryingServerStream, type QueryResult } from "../../lib/useDaemon";

/** One model's share of the spend. */
export interface ModelSpend {
  key: string;
  provider: string;
  model: string;
  costUsd: number;
  tokens: number;
  calls: number;
}

export interface LiveSpend {
  events: number;
  costUsd: number;
  byModel: ModelSpend[];
}

/**
 * Folds cost events into a per-model total.
 *
 * `cost_usd` is a string, and `cost_usd_present` distinguishes "this provider
 * reported nothing" from "this call really was free" — a provider that never
 * reports must not drag the total toward zero, so its calls count tokens and
 * contribute no cost.
 */
export function accumulate(events: telemetryPb.CostEvent[]): LiveSpend {
  const byKey = new Map<string, ModelSpend>();
  let costUsd = 0;

  for (const event of events) {
    const key = `${event.provider}/${event.model}`;
    const parsed = event.costUsdPresent ? Number.parseFloat(event.costUsd) : Number.NaN;
    const cost = Number.isFinite(parsed) ? parsed : 0;
    costUsd += cost;

    const existing = byKey.get(key);
    const tokens = event.promptTokens + event.completionTokens;
    if (existing) {
      existing.costUsd += cost;
      existing.tokens += tokens;
      existing.calls += 1;
    } else {
      byKey.set(key, {
        key,
        provider: event.provider,
        model: event.model,
        costUsd: cost,
        tokens,
        calls: 1,
      });
    }
  }

  return {
    events: events.length,
    costUsd,
    // Most expensive first; ties fall back to volume so the list is stable.
    byModel: [...byKey.values()].sort((a, b) => b.costUsd - a.costUsd || b.tokens - a.tokens),
  };
}

export interface Usage {
  month: QueryResult<{ tokens: number; costUsd: number | undefined }>;
  live: LiveSpend;
  /** True once the cost stream is attached. */
  streaming: boolean;
  reset: () => void;
}

/** Keeps memory bounded on a session left open for days. */
const MAX_EVENTS = 2000;

/**
 * Spend, monthly and live.
 *
 * `Telemetry.StreamCostEvents` is the only RPC of a service that has been
 * exposed and never consumed by either client — this is what it is for. The
 * monthly figures come from the daemon's own totals so the page is meaningful
 * the moment it opens, before a single live event has arrived.
 */
export function useUsage(deps: unknown[] = []): Usage {
  const [events, setEvents] = useState<telemetryPb.CostEvent[]>([]);

  const month = useQuery(async (borg) => {
    const [tokens, cost] = await Promise.all([
      borg.admin.getMonthlyTokenTotal({}),
      borg.admin.getMonthlyTotalCost({}),
    ]);
    return {
      tokens: Number(tokens.total),
      // `present` separates "no rows yet" from a genuine $0.00.
      costUsd: cost.present ? cost.cost : undefined,
    };
  }, deps);

  // Retrying: the stream outlives a daemon restart only by being redialled.
  const stream = useRetryingServerStream<telemetryPb.CostEvent>(
    (borg, handlers) => borg.telemetry.streamCostEvents({}, handlers),
    (event) => setEvents((prev) => [...prev, event].slice(-MAX_EVENTS)),
    [],
  );

  const live = useMemo(() => accumulate(events), [events]);
  const reset = useCallback(() => setEvents([]), []);

  // A stream that never opens should not leave the page claiming it is live.
  useEffect(() => {
    if (stream.status === "error") setEvents([]);
  }, [stream.status]);

  return { month, live, streaming: stream.status === "open", reset };
}
