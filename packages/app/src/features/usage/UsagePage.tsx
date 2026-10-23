import { useLocation } from "react-router-dom";
import {
  PageChrome,
  SECTION_LABEL,
  SECTION_LABEL_TEXT,
  SETTINGS_SCROLL_DESKTOP_PADDING,
  scrollCol,
  useIsMobile,
} from "@borg/ui";
import { DaemonState } from "../../components/DaemonState";
import { formatCost, formatTokens } from "../../lib/format";
import { useUsage } from "./useUsage";

function Stat({ label, value, sub }: { label: string; value: string; sub?: string }) {
  return (
    <div className="flex-1 rounded-[10px] border border-line2 px-4 py-3.5">
      <div className={SECTION_LABEL_TEXT}>{label}</div>
      <div className="mt-2 text-[22px] leading-none text-ink">{value}</div>
      {sub && <div className="mt-1.5 text-[11.5px] leading-none text-ink3">{sub}</div>}
    </div>
  );
}

/** What Borg costs: the daemon's month-to-date totals. */
export function UsagePage() {
  const isMobile = useIsMobile();
  const location = useLocation();
  const month = useUsage([location.key]);

  return (
    <PageChrome title="Usage" mobileBrandHeader>
      <div className={scrollCol(isMobile, SETTINGS_SCROLL_DESKTOP_PADDING)}>
        <DaemonState query={month} label="your usage">
          {(totals) => (
            <>
              <div className={SECTION_LABEL}>This month</div>
              <div className={`flex gap-3 ${isMobile ? "flex-col" : ""}`}>
                <Stat
                  label="Spend"
                  value={totals.costUsd === undefined ? "—" : formatCost(totals.costUsd)}
                  sub={totals.costUsd === undefined ? "No priced calls yet" : undefined}
                />
                <Stat label="Tokens" value={formatTokens(totals.tokens)} />
              </div>
            </>
          )}
        </DaemonState>
      </div>
    </PageChrome>
  );
}
