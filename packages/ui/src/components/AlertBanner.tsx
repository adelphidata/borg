import { Pill } from "./Pill";
import type { BannerAlert } from "../types";

/** Red banner for something that needs the user. Copy comes from the data layer. */
export function AlertBanner({
  alert,
  stacked = false,
  onAction,
}: {
  alert: BannerAlert | undefined;
  /** Vertical layout for narrow columns; horizontal otherwise. */
  stacked?: boolean;
  onAction?: () => void;
}) {
  if (!alert) return null;

  const dot = "h-[7px] w-[7px] flex-none rounded-full bg-alert";
  const sub = "mt-1 text-[12.5px] leading-[1.4] text-ink2";

  if (stacked) {
    return (
      <div className="flex flex-col items-stretch gap-3 rounded-xl border border-alert bg-alert-soft px-4 py-[15px]">
        <div className="flex items-center gap-2.5">
          <span className={dot} />
          <div className="text-[13px] leading-[1.3] font-medium text-ink">{alert.stackedTitle}</div>
        </div>
        <div className={sub}>{alert.sub}</div>
        <Pill variant="danger" className="self-start" onClick={onAction}>
          {alert.action}
        </Pill>
      </div>
    );
  }

  return (
    <div className="flex items-center gap-3.5 rounded-[11px] border border-alert bg-alert-soft px-[18px] py-[15px]">
      <span className={dot} />
      <div className="min-w-0 flex-1">
        <div className="text-[13.5px] leading-[1.3] font-medium text-ink">{alert.title}</div>
        <div className={sub}>{alert.sub}</div>
      </div>
      <Pill variant="danger" onClick={onAction}>
        {alert.action}
      </Pill>
    </div>
  );
}
