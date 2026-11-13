import type { meetings } from "@borg/client";
import { dayRows, timeRange, type DayRow } from "./meetingsFormat";

/** How many days one page of "Coming up" covers. */
export const UPCOMING_DAYS = 7;

function EventRow({ event }: { event: meetings.CalendarEvent }) {
  const range = event.allDay ? undefined : timeRange(event.startUnix, event.endUnix);
  return (
    <div className="flex min-w-0 items-stretch gap-3">
      <span aria-hidden="true" className="w-[3px] flex-none rounded bg-meeting-bar" />
      <div className="min-w-0 py-0.5">
        <div className="truncate text-[14.5px] leading-[1.3] font-medium text-ink">
          {event.title || "Untitled event"}
        </div>
        <div className="mt-1 text-[12.5px] leading-none text-ink3 tabular-nums">
          {range ? (
            <>
              {range.text}
              {range.suffix && (
                <span className="ml-1 text-[10.5px] tracking-[0.04em]">{range.suffix}</span>
              )}
            </>
          ) : (
            "All day"
          )}
        </div>
      </div>
    </div>
  );
}

function DayRowView({ row }: { row: DayRow }) {
  const month = row.day.toLocaleDateString(undefined, { month: "short" });
  const weekday = row.day.toLocaleDateString(undefined, { weekday: "short" });
  return (
    <div className="flex gap-5 border-t border-dashed border-line px-5 py-4 first:border-t-0">
      <div className="flex w-[92px] flex-none items-start gap-2.5">
        <span className="text-[34px] leading-[0.9] font-light text-ink tabular-nums">
          {row.day.getDate()}
        </span>
        <div className="flex flex-col gap-1 pt-0.5">
          <span className="flex items-center gap-1.5 text-[12.5px] leading-none font-medium text-ink">
            {month}
            {row.isToday && (
              <span
                role="img"
                aria-label="Today"
                className="h-1.5 w-1.5 flex-none rounded-full bg-today"
              />
            )}
          </span>
          <span className="text-[12px] leading-none text-ink3">{weekday}</span>
        </div>
      </div>
      <div className="flex min-w-0 flex-1 flex-col gap-3">
        {row.noMoreToday ? (
          <div className="flex items-stretch gap-3">
            <span aria-hidden="true" className="w-[3px] flex-none rounded bg-bar-track" />
            <span className="py-1 text-[13.5px] leading-[1.3] text-ink3">No more events today</span>
          </div>
        ) : (
          row.events.map((e) => <EventRow key={e.id} event={e} />)
        )}
      </div>
    </div>
  );
}

/**
 * "Coming up": a week of the calendar, one row per day that has something,
 * today always included while it is in view.
 */
export function UpcomingCard({
  events,
  windowStart,
  now,
  onEarlier,
  onLater,
  canGoEarlier,
}: {
  events: meetings.CalendarEvent[] | undefined;
  windowStart: Date;
  now: Date;
  onEarlier: () => void;
  onLater: () => void;
  /** False on the window that holds today: the past is the list below. */
  canGoEarlier: boolean;
}) {
  const rows = events ? dayRows(events, windowStart, now, UPCOMING_DAYS) : [];
  const arrow =
    "flex h-7 w-7 cursor-pointer items-center justify-center rounded-full border-0 bg-transparent text-[18px] leading-none text-ink2 hover:bg-surface3 hover:text-ink disabled:cursor-default disabled:opacity-30";
  return (
    <section>
      <div className="mb-3 flex items-center justify-between">
        <h2 className="m-0 font-serif text-[26px] leading-[1.2] font-normal tracking-[-0.01em] text-ink">
          Coming up
        </h2>
        <div className="flex items-center gap-1">
          <button
            type="button"
            aria-label="Earlier"
            className={arrow}
            disabled={!canGoEarlier}
            onClick={onEarlier}
          >
            ‹
          </button>
          <button type="button" aria-label="Later" className={arrow} onClick={onLater}>
            ›
          </button>
        </div>
      </div>
      <div className="overflow-hidden rounded-[14px] border border-line2 bg-surface">
        {events === undefined ? (
          <div className="px-5 py-4 text-[13px] text-ink3">Loading your calendar…</div>
        ) : rows.length === 0 ? (
          <div className="px-5 py-4 text-[13.5px] text-ink3">Nothing on the calendar this week</div>
        ) : (
          rows.map((row) => <DayRowView key={row.day.getTime()} row={row} />)
        )}
      </div>
    </section>
  );
}
