import { useNavigate } from "react-router-dom";
import { Pill, SECTION_LABEL, StatusDot } from "@borg/ui";
import type { meetings } from "@borg/client";
import { attendeeLine, groupPast, initialOf, timeOfDay } from "./meetingsFormat";

/** What sits at the right of a row: the time, or what the meeting is doing. */
function RowState({ m }: { m: meetings.MeetingSummary }) {
  if (m.state === "recording") {
    return (
      <span className="flex items-center gap-1.5 rounded-full bg-nav-on-bg px-2.5 py-1 text-[11.5px] leading-none font-medium text-nav-on-ink">
        <StatusDot tone="busy" />
        Recording
      </span>
    );
  }
  if (m.state === "finalizing") {
    return <span className="text-[12.5px] leading-none text-ink3">Writing notes…</span>;
  }
  if (m.state === "failed") {
    return (
      <span className="flex items-center gap-1.5 text-[12.5px] leading-none text-ink3">
        <StatusDot tone="alert" />
        Didn't finish
      </span>
    );
  }
  return (
    <span className="flex items-center gap-2 text-[12.5px] leading-none text-ink3 tabular-nums">
      {m.unread && m.hasSummary && <StatusDot tone="unread" label="New notes" />}
      {timeOfDay(m.startedAt)}
    </span>
  );
}

/** Recorded meetings under "Today", "Yesterday", then their dates. */
export function PastMeetings({
  list,
  now,
  hasMore,
  loadingMore,
  onLoadMore,
}: {
  list: meetings.MeetingSummary[];
  now: Date;
  hasMore: boolean;
  loadingMore: boolean;
  onLoadMore: () => void;
}) {
  const navigate = useNavigate();
  if (list.length === 0) return null;
  return (
    <div className="flex flex-col gap-7">
      {groupPast(list, now).map((group) => (
        <section key={group.label}>
          <div className={`${SECTION_LABEL} px-3`}>{group.label}</div>
          <div className="flex flex-col">
            {group.meetings.map((m) => {
              const who = attendeeLine(m.attendees);
              return (
                <button
                  type="button"
                  key={m.id}
                  onClick={() => navigate(`/meetings/${encodeURIComponent(m.id)}`)}
                  className="flex w-full cursor-pointer items-center gap-3 rounded-[10px] border-0 bg-transparent px-3 py-2 text-left hover:bg-surface3"
                >
                  <span
                    aria-hidden="true"
                    className="flex h-8 w-8 flex-none items-center justify-center rounded-[8px] border border-line2 bg-surface2 text-[13px] font-medium text-ink2"
                  >
                    {initialOf(m)}
                  </span>
                  <span className="min-w-0 flex-1">
                    <span className="block truncate text-[13.5px] leading-[1.35] text-ink">
                      {m.title || "Untitled meeting"}
                    </span>
                    {who && (
                      <span className="mt-[3px] block truncate text-[11.5px] leading-[1.4] text-ink3">
                        {who}
                      </span>
                    )}
                  </span>
                  <RowState m={m} />
                </button>
              );
            })}
          </div>
        </section>
      ))}
      {hasMore && (
        <div>
          <Pill onClick={onLoadMore} disabled={loadingMore}>
            {loadingMore ? "Loading…" : "Show earlier"}
          </Pill>
        </div>
      )}
    </div>
  );
}
