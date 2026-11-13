import { useState } from "react";
import { Link, useNavigate } from "react-router-dom";
import {
  MobileTitle,
  PageChrome,
  Pill,
  SETTINGS_SCROLL_DESKTOP_PADDING,
  scrollCol,
  useIsMobile,
} from "@borg/ui";
import { DaemonState } from "../../components/DaemonState";
import { useSettings } from "../../lib/settings";
import { useTick } from "../../lib/useTick";
import { useVoiceCatalog } from "../../lib/voice";
import { useChain } from "../settings/fields";
import { meetingsEnabled } from "./meetingsEnabled";
import { eventPath } from "./MeetingEventPage";
import { addDays, startOfDay } from "./meetingsFormat";
import { PastMeetings } from "./PastMeetings";
import { UPCOMING_DAYS, UpcomingCard } from "./UpcomingCard";
import { useMeetingList, useMeetingsStatus, useUpcomingEvents } from "./useMeetings";
import { useTurnOnMeetings } from "./useTurnOnMeetings";

/** Shown in place of the page while the feature is off. */
function OffCard() {
  const catalog = useVoiceCatalog();
  const chain = useChain("audio.models");
  const turnOn = useTurnOnMeetings(
    chain,
    catalog.state === "ready" ? catalog.data.transcription : undefined,
  );
  const [busy, setBusy] = useState(false);
  return (
    <div className="flex flex-col items-start gap-3 rounded-[10px] border border-line2 px-4 py-3.5">
      <div className="text-[13.5px] leading-[1.4] font-medium text-ink">
        Borg can take notes in your calls
      </div>
      <div className="text-[13px] leading-[1.5] text-ink2">
        Nothing joins the meeting. Borg listens on this computer, writes the transcript, and sums it
        up when the call ends.
      </div>
      <div className="flex items-center gap-2">
        <Pill
          variant="primary"
          // Wait for the transcriber list, so "on" can pick one in the same press.
          disabled={busy || catalog.state === "loading"}
          onClick={() => {
            setBusy(true);
            void turnOn(true).finally(() => setBusy(false));
          }}
        >
          Turn on
        </Pill>
        <Link to="/settings/meetings" className="text-[12.5px] text-ink3 hover:text-ink">
          Settings
        </Link>
      </div>
    </div>
  );
}

function Upcoming({ now }: { now: Date }) {
  const [page, setPage] = useState(0);
  const windowStart = addDays(startOfDay(now), page * UPCOMING_DAYS);
  const from = Math.floor(windowStart.getTime() / 1000);
  const to = Math.floor(addDays(windowStart, UPCOMING_DAYS).getTime() / 1000);
  const events = useUpcomingEvents(from, to);
  const navigate = useNavigate();
  return (
    <>
      <UpcomingCard
        events={events.state === "ready" ? events.data : undefined}
        windowStart={windowStart}
        now={now}
        canGoEarlier={page > 0}
        onEarlier={() => setPage((p) => Math.max(0, p - 1))}
        onLater={() => setPage((p) => p + 1)}
        onOpenEvent={(e) => navigate(eventPath(e.id))}
      />
      {events.state === "error" && (
        <div className="-mt-5 text-[12.5px] text-alert">
          Couldn't read your calendar: {events.error.message}
        </div>
      )}
    </>
  );
}

/**
 * Meetings: what is coming up on the calendar, and the meetings Borg has
 * taken notes in. Modelled on Granola's home view.
 */
export function MeetingsPage() {
  const isMobile = useIsMobile();
  const { settings } = useSettings();
  const enabled = meetingsEnabled(settings.state === "ready" ? settings.data : undefined);
  const status = useMeetingsStatus();
  const list = useMeetingList();
  const tick = useTick(60_000, true);
  const now = new Date(tick);

  return (
    <PageChrome title="Meetings">
      <div className={scrollCol(isMobile, SETTINGS_SCROLL_DESKTOP_PADDING)}>
        <div className="flex max-w-[720px] flex-col gap-8">
          <MobileTitle>Meetings</MobileTitle>
          {settings.state === "ready" && !enabled && <OffCard />}
          <DaemonState query={status} label="meetings">
            {(s) =>
              s.calendar?.connected ? (
                <Upcoming now={now} />
              ) : !enabled ? null : (
                <Link
                  to="/connections/calendar"
                  className="text-[13.5px] leading-[1.5] text-ink2 hover:text-ink"
                >
                  Connect a calendar to see what's coming up ›
                </Link>
              )
            }
          </DaemonState>
          {list.query.state === "ready" && (
            <PastMeetings
              list={list.meetings}
              now={now}
              hasMore={list.hasMore}
              loadingMore={list.loadingMore}
              onLoadMore={list.loadMore}
            />
          )}
        </div>
      </div>
    </PageChrome>
  );
}
