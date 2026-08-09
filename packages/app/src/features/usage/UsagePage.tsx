import { useLocation } from "react-router-dom";
import {
  PageChrome,
  SECTION_LABEL,
  SETTINGS_SCROLL_DESKTOP_PADDING,
  StatusDot,
  scrollCol,
  useIsMobile,
} from "@borg/ui";
import { DaemonState } from "../../components/DaemonState";
import { useUsage, type ModelSpend } from "./useUsage";

const TOKENS = new Intl.NumberFormat(undefined, { notation: "compact", maximumFractionDigits: 1 });
const MONEY = new Intl.NumberFormat(undefined, {
  style: "currency",
  currency: "USD",
  maximumFractionDigits: 2,
});
/** Sub-cent spend rounds to $0.00, which reads as "nothing happened". */
const SMALL_MONEY = new Intl.NumberFormat(undefined, {
  style: "currency",
  currency: "USD",
  maximumFractionDigits: 4,
});

export function formatCost(costUsd: number): string {
  if (costUsd === 0) return MONEY.format(0);
  return costUsd < 0.01 ? SMALL_MONEY.format(costUsd) : MONEY.format(costUsd);
}

function Stat({ label, value, sub }: { label: string; value: string; sub?: string }) {
  return (
    <div className="flex-1 rounded-[10px] border border-line2 px-4 py-3.5">
      <div className="text-[9.5px] leading-none font-medium tracking-[0.16em] text-ink3 uppercase">
        {label}
      </div>
      <div className="mt-2 text-[22px] leading-none text-ink">{value}</div>
      {sub && <div className="mt-1.5 text-[11.5px] leading-none text-ink3">{sub}</div>}
    </div>
  );
}

/** A model's share of live spend, with a bar for proportion. */
function ModelRow({ spend, of }: { spend: ModelSpend; of: number }) {
  const share = of > 0 ? Math.round((spend.costUsd / of) * 100) : 0;
  return (
    <div className="py-2">
      <div className="flex items-baseline gap-2">
        <span className="min-w-0 flex-1 truncate text-[13px] leading-[1.35] text-ink">
          {spend.model || "unknown"}
        </span>
        <span className="flex-none text-[11.5px] leading-none text-ink3">
          {TOKENS.format(spend.tokens)} tok
        </span>
        <span className="w-[70px] flex-none text-right text-[12.5px] leading-none text-ink2">
          {formatCost(spend.costUsd)}
        </span>
      </div>
      <div className="mt-1.5 h-1 overflow-hidden rounded-full bg-bar-track">
        <div className="h-full rounded-full bg-neon" style={{ width: `${share}%` }} />
      </div>
    </div>
  );
}

/**
 * What Borg costs.
 *
 * Two horizons on one page: the daemon's month-to-date totals, which are true
 * the moment the page opens, and a live per-model breakdown fed by the cost
 * stream. The live half only covers what has happened since this page was
 * opened, and says so — quietly implying it was the month's full breakdown
 * would be a lie the numbers cannot back up.
 */
export function UsagePage() {
  const isMobile = useIsMobile();
  const location = useLocation();
  const usage = useUsage([location.key]);

  return (
    <PageChrome title="Usage" mobileBrandHeader>
      <div className={scrollCol(isMobile, SETTINGS_SCROLL_DESKTOP_PADDING)}>
        <DaemonState query={usage.month} label="your usage">
          {(month) => (
            <>
              <div className={SECTION_LABEL}>This month</div>
              <div className={`mb-7 flex gap-3 ${isMobile ? "flex-col" : ""}`}>
                <Stat
                  label="Spend"
                  value={month.costUsd === undefined ? "—" : formatCost(month.costUsd)}
                  sub={month.costUsd === undefined ? "No priced calls yet" : undefined}
                />
                <Stat label="Tokens" value={TOKENS.format(month.tokens)} />
              </div>
            </>
          )}
        </DaemonState>

        <div className="mb-3.5 flex items-center gap-2">
          <span className={`${SECTION_LABEL} mb-0`}>Live, by model</span>
          <StatusDot needsAttention={!usage.streaming} />
          <span className="flex-1" />
          {usage.live.events > 0 && (
            <button
              type="button"
              onClick={usage.reset}
              className="cursor-pointer border-0 bg-transparent text-[11.5px] leading-none text-ink3 hover:text-ink2"
            >
              Clear
            </button>
          )}
        </div>

        {usage.live.events === 0 ? (
          <div className="rounded-[10px] border border-line2 px-4 py-6 text-center text-[12.5px] leading-[1.5] text-ink3">
            {usage.streaming
              ? "Waiting for the next model call."
              : "The cost stream is not connected."}
          </div>
        ) : (
          <div className="rounded-[10px] border border-line2 px-4 py-2">
            {usage.live.byModel.map((spend) => (
              <ModelRow key={spend.key} spend={spend} of={usage.live.costUsd} />
            ))}
            <div className="mt-1 border-t border-line2 pt-2.5 pb-1 text-[11.5px] leading-none text-ink3">
              {usage.live.events} call{usage.live.events === 1 ? "" : "s"} since this page opened ·{" "}
              {formatCost(usage.live.costUsd)}
            </div>
          </div>
        )}
      </div>
    </PageChrome>
  );
}
