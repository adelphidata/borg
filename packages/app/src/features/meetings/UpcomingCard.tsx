import type { meetings } from "@borg/client";
import { ChevronIcon, SECTION_LABEL_TEXT } from "@borg/ui";
import { dayRows, timeRange, type DayRow } from "./meetingsFormat";

/** How many days one page of "Coming up" covers. */
export const UPCOMING_DAYS = 7;

/** The accent bar beside an event: Borg's green, grey once there is nothing left. */
const BAR = "w-[3px] flex-none rounded-full";

function EventRow({
  event,
  onOpen,
}: {
  event: meetings.CalendarEvent;
  onOpen: (event: meetings.CalendarEvent) => void;
}) {
  const range = event.allDay ? undefined : timeRange(event.startUnix, event.endUnix);
  return (
    <button
      type="button"
      onClick={() => onOpen(event)}
      className="group -mx-2 flex w-[calc(100%+16px)] min-w-0 cursor-pointer items-stretch gap-3 rounded-[8px] border-0 bg-transparent px-2 py-1.5 text-left hover:bg-surface3"
    >
      <span aria-hidden="true" className={`${BAR} bg-neon`} />
      <span className="min-w-0 flex-1">
        <span className="block truncate text-[13.5px] leading-[1.35] text-ink">
          {event.title || "Untitled event"}
        </span>
        <span className="mt-[3px] block text-[11.5px] leading-[1.4] text-ink3 tabular-nums">
          {range ? (
            <>
              {range.text}
              {range.suffix && <span className="ml-1 text-[10px]">{range.suffix}</span>}
            </>
          ) : (
            "All day"
          )}
        </span>
      </span>
      <span
        aria-hidden="true"
        className="flex flex-none items-center text-ink3 opacity-0 group-hover:opacity-100"
      >
        <ChevronIcon size={13} />
      </span>
    </button>
  );
}

function DayRowView({
  row,
  onOpen,
}: {
  row: DayRow;
  onOpen: (event: meetings.CalendarEvent) => void;
}) {
  const month = row.day.toLocaleDateString(undefined, { month: "short" });
  const weekday = row.day.toLocaleDateString(undefined, { weekday: "short" });
  return (
    <div className="flex gap-4 border-t border-line2 px-4 py-3 first:border-t-0">
      <div className="flex w-[78px] flex-none items-start gap-2 pt-1.5">
        <span className="text-[22px] leading-[0.9] font-medium text-ink tabular-nums">
          {row.day.getDate()}
        </span>
        <div className="flex flex-col gap-[3px]">
          <span className="flex items-center gap-1.5 text-[11.5px] leading-none text-ink2">
            {month}
            {row.isToday && (
              <span
                role="img"
                aria-label="Today"
                className="h-1.5 w-1.5 flex-none rounded-full bg-neon"
              />
            )}
          </span>
          <span className="text-[11.5px] leading-none text-ink3">{weekday}</span>
        </div>
      </div>
      <div className="flex min-w-0 flex-1 flex-col gap-0.5">
        {row.noMoreToday ? (
          <div className="flex items-stretch gap-3 py-1.5">
            <span aria-hidden="true" className={`${BAR} bg-bar-track`} />
            <span className="text-[13px] leading-[1.35] text-ink3">No more events today</span>
          </div>
        ) : (
          row.events.map((e) => <EventRow key={e.id} event={e} onOpen={onOpen} />)
        )}
      </div>
    </div>
  );
}

/**
 * "Coming up": a week of the calendar, one row per day that has something,
 * today always included while it is in view. Each event opens its own page.
 */
export function UpcomingCard({
  events,
  windowStart,
  now,
  onEarlier,
  onLater,
  canGoEarlier,
  onOpenEvent,
}: {
  events: meetings.CalendarEvent[] | undefined;
  windowStart: Date;
  now: Date;
  onEarlier: () => void;
  onLater: () => void;
  /** False on the window that holds today: the past is the list below. */
  canGoEarlier: boolean;
  onOpenEvent: (event: meetings.CalendarEvent) => void;
}) {
  const rows = events ? dayRows(events, windowStart, now, UPCOMING_DAYS) : [];
  const arrow =
    "flex h-6 w-6 cursor-pointer items-center justify-center rounded-[6px] border-0 bg-transparent text-[15px] leading-none text-ink3 hover:bg-surface3 hover:text-ink disabled:cursor-default disabled:opacity-30 disabled:hover:bg-transparent";
  return (
    <section>
      <div className="mb-2.5 flex items-center justify-between">
        <h2 className={`m-0 ${SECTION_LABEL_TEXT}`}>Coming up</h2>
        <div className="flex items-center gap-0.5">
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
      <div className="rounded-[10px] border border-line2">
        {events === undefined ? (
          <div className="px-4 py-3 text-[13px] text-ink3">Loading your calendar…</div>
        ) : rows.length === 0 ? (
          <div className="px-4 py-3 text-[13px] text-ink3">Nothing on the calendar this week</div>
        ) : (
          rows.map((row) => <DayRowView key={row.day.getTime()} row={row} onOpen={onOpenEvent} />)
        )}
      </div>
    </section>
  );
}
