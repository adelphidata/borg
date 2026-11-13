import { useNavigate } from "react-router-dom";
import { Pill, SECTION_LABEL, StatusDot } from "@borg/ui";
import type { meetings } from "@borg/client";
import {
  AVATAR_TILES,
  attendeeLine,
  avatarTone,
  groupPast,
  initialOf,
  timeOfDay,
} from "./meetingsFormat";

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
  if (list.length === 0) {
    return (
      <div className="text-[13.5px] leading-[1.6] text-ink3">
        Meetings you take notes in show up here.
      </div>
    );
  }
  let index = 0;
  return (
    <div className="flex flex-col gap-7">
      {groupPast(list, now).map((group) => (
        <section key={group.label}>
          <div className={SECTION_LABEL}>{group.label}</div>
          <div className="flex flex-col">
            {group.meetings.map((m) => {
              const tile = AVATAR_TILES[avatarTone(index++)];
              const who = attendeeLine(m.attendees);
              return (
                <button
                  type="button"
                  key={m.id}
                  onClick={() => navigate(`/meetings/${encodeURIComponent(m.id)}`)}
                  className="flex w-full cursor-pointer items-center gap-3.5 rounded-[10px] border-0 bg-transparent px-2 py-2.5 text-left hover:bg-surface3"
                >
                  <span
                    aria-hidden="true"
                    className={`flex h-9 w-9 flex-none items-center justify-center rounded-[9px] text-[14px] font-medium text-ink ${tile}`}
                  >
                    {initialOf(m)}
                  </span>
                  <span className="min-w-0 flex-1">
                    <span className="block truncate text-[14px] leading-[1.3] font-medium text-ink">
                      {m.title || "Untitled meeting"}
                    </span>
                    {who && (
                      <span className="mt-0.5 block truncate text-[12.5px] leading-[1.3] text-ink3">
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
