import { useQuery, type QueryResult } from "../../lib/useDaemon";

/** Month-to-date spend, from the daemon's own totals. */
export type MonthUsage = QueryResult<{ tokens: number; costUsd: number | undefined }>;

/**
 * What Borg has spent this month.
 *
 * Read from the daemon's totals rather than summed in the client, so the page
 * is true the moment it opens and agrees with the TUI and the budget check.
 */
export function useUsage(deps: unknown[] = []): MonthUsage {
  return useQuery(async (borg) => {
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
}
