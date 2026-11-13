import { useState } from "react";
import { Link, useNavigate, useParams } from "react-router-dom";
import {
  BackLink,
  MeetingsIcon,
  PageChrome,
  PeopleIcon,
  Pill,
  SECTION_LABEL,
  VideoIcon,
  scrollCol,
  useIsMobile,
} from "@borg/ui";
import { DaemonState } from "../../components/DaemonState";
import { openExternal } from "../../lib/openExternal";
import { useNative, usePlatform } from "../../lib/platform";
import { asBorgError, useBorg } from "../../lib/useDaemon";
import { Chip, ChipButton, ChipRow, DOC_COL, DocTitle } from "./MeetingDoc";
import { addDays, eventWhen, fullNames, startOfDay } from "./meetingsFormat";
import { meetingsChanged } from "./meetingsStore";
import {
  useMeetingList,
  useMeetingsStatus,
  useUpcomingEvents,
  type CalendarEvent,
} from "./useMeetings";

/** Where a calendar event's page lives. */
export function eventPath(eventId: string): string {
  return `/meetings/event/${encodeURIComponent(eventId)}`;
}

/** "Join Google Meet", "Join Zoom", "Join Teams", else "Join call". */
function joinLabel(provider: string): string {
  if (provider === "meet") return "Join Google Meet";
  if (provider === "zoom") return "Join Zoom";
  if (provider === "teams") return "Join Teams";
  return "Join call";
}

/**
 * Taking notes in this event: on the desktop app it starts the meeting and
 * opens the notes panel; elsewhere nothing can record, so it says where to go.
 */
function TakeNotes({ event }: { event: CalendarEvent }) {
  const borg = useBorg();
  const native = useNative();
  const navigate = useNavigate();
  const platform = usePlatform();
  const status = useMeetingsStatus();
  const [starting, setStarting] = useState(false);
  const [error, setError] = useState<string>();

  // A browser tab cannot record, and with Meetings off the daemon would
  // refuse; either way there is nothing to offer. Nor before the status is in.
  if (platform.kind !== "desktop" || status.state !== "ready" || !status.data.enabled) return null;
  if (!status.data.transcriptionReady) {
    return (
      <div className="text-[13px] leading-[1.5] text-ink3">
        Borg needs a transcriber before it can take notes.{" "}
        <Link to="/settings/meetings" className="text-ink2 hover:text-ink">
          Set one up ›
        </Link>
      </div>
    );
  }

  const start = async () => {
    if (starting) return;
    setStarting(true);
    setError(undefined);
    try {
      const started = await borg.meetings.startMeeting({
        title: "",
        sourceApp: "",
        calendarEventId: event.id,
      });
      meetingsChanged();
      if (started.summary) {
        native.meetings.openPanel(started.summary.id);
        navigate(`/meetings/${encodeURIComponent(started.summary.id)}`);
      }
    } catch (e) {
      setError(asBorgError(e).message);
    } finally {
      setStarting(false);
    }
  };
  return (
    <div className="flex flex-col items-start gap-2">
      <Pill variant="primary" disabled={starting} onClick={() => void start()}>
        {starting ? "Starting…" : "Take notes"}
      </Pill>
      {error && <div className="text-[12px] leading-[1.45] text-alert">{error}</div>}
    </div>
  );
}

function EventBody({ event, now }: { event: CalendarEvent; now: Date }) {
  const navigate = useNavigate();
  const list = useMeetingList();
  // Until the list has loaded, "no notes yet" is unknown: offering Take notes
  // then could start a second meeting for an event that already has one.
  const listed = list.query.state === "ready";
  const taken = list.meetings.find((m) => m.calendarEventId === event.id);
  const people = fullNames(event.attendees);
  const attendees = event.attendees.filter((a) => a.name.trim() !== "" || a.email.trim() !== "");
  return (
    <>
      <div className="flex flex-col gap-3">
        <DocTitle title={event.title || "Untitled event"} />
        <ChipRow>
          <Chip icon={<MeetingsIcon size={13} />}>{eventWhen(event, now)}</Chip>
          {people.length > 0 && (
            <Chip icon={<PeopleIcon />} title={people.join(", ")}>
              {people.length}
            </Chip>
          )}
          {event.joinUrl !== "" && (
            <ChipButton icon={<VideoIcon size={13} />} onClick={() => openExternal(event.joinUrl)}>
              {joinLabel(event.provider)}
            </ChipButton>
          )}
        </ChipRow>
      </div>
      {taken ? (
        <div>
          <Pill onClick={() => navigate(`/meetings/${encodeURIComponent(taken.id)}`)}>
            Open notes
          </Pill>
        </div>
      ) : listed ? (
        <TakeNotes event={event} />
      ) : null}
      {people.length > 0 && (
        <section>
          <div className={SECTION_LABEL}>People</div>
          <ul className="m-0 flex list-none flex-col gap-2 p-0">
            {attendees.map((a, i) => (
              <li key={`${a.email}:${i}`} className="flex items-baseline gap-2 text-[13.5px]">
                <span className="text-ink">{a.name.trim() || a.email}</span>
                {a.name.trim() !== "" && a.email !== "" && (
                  <span className="truncate text-[12px] text-ink3">{a.email}</span>
                )}
              </li>
            ))}
          </ul>
        </section>
      )}
    </>
  );
}

/**
 * `/meetings/event/:eventId`: a calendar event before (or without) notes —
 * when it is, who is coming, the call link, and "Take notes".
 */
export function MeetingEventPage() {
  const { eventId = "" } = useParams();
  const isMobile = useIsMobile();
  // The calendar cache holds a day back and two weeks ahead
  // (`SYNC_AHEAD_SECS`); look across all of it, so any event the card can
  // page to opens here.
  const [now] = useState(() => new Date());
  const from = Math.floor(addDays(startOfDay(now), -1).getTime() / 1000);
  const to = Math.floor(addDays(startOfDay(now), 16).getTime() / 1000);
  const events = useUpcomingEvents(from, to);
  return (
    <PageChrome title="Meetings">
      <div className={scrollCol(isMobile, "px-[38px] pt-[30px] pb-10")}>
        <div className={DOC_COL}>
          <BackLink to="/meetings" label="Meetings" />
          <DaemonState query={events} label="your calendar">
            {(list) => {
              const event = list.find((e) => e.id === eventId);
              return event ? (
                <EventBody event={event} now={now} />
              ) : (
                <div className="text-[14px] text-ink2">
                  That event isn't on your calendar any more.
                </div>
              );
            }}
          </DaemonState>
        </div>
      </div>
    </PageChrome>
  );
}
