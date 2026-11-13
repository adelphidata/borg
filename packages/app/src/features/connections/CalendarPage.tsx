import { useEffect, useState } from "react";
import type { meetings } from "@borg/client";
import {
  BackLink,
  Badge,
  DETAIL_COL,
  PageChrome,
  Pill,
  SECTION_LABEL,
  StatusDot,
  TEXT_FIELD,
  isSubmitEnter,
  scrollCol,
  useIsMobile,
} from "@borg/ui";
import { DaemonState } from "../../components/DaemonState";
import { openExternal } from "../../lib/openExternal";
import { useMutation, useRetryingServerStream } from "../../lib/useDaemon";
import { meetingsChanged } from "../meetings/meetingsStore";
import { useMeetingsStatus } from "../meetings/useMeetings";
import { calendarCard } from "./CalendarSection";

const FIELD = `w-full ${TEXT_FIELD}`;

/**
 * A labelled field, hidden until "Show" when it holds a secret. Kept in this
 * page only; the daemon stores it.
 */
function SecretField({
  label,
  value,
  placeholder,
  secret = true,
  onChange,
  onEnter,
}: {
  label: string;
  value: string;
  placeholder?: string;
  /** False for a value that is safe to see, such as an OAuth client ID. */
  secret?: boolean;
  onChange: (v: string) => void;
  onEnter?: () => void;
}) {
  const [shown, setShown] = useState(!secret);
  return (
    <label className="flex flex-col gap-1.5">
      <span className="text-[12.5px] leading-none font-medium text-ink2">{label}</span>
      <span className="flex items-center gap-2">
        <input
          type={shown ? "text" : "password"}
          autoComplete="off"
          spellCheck={false}
          className={FIELD}
          value={value}
          placeholder={placeholder}
          onChange={(e) => onChange(e.target.value)}
          onKeyDown={(e) => onEnter && isSubmitEnter(e) && onEnter()}
        />
        {secret && (
          <Pill square type="button" onClick={() => setShown((v) => !v)}>
            {shown ? "Hide" : "Show"}
          </Pill>
        )}
      </span>
    </label>
  );
}

const idle = () => () => {};

function Connected() {
  const disconnect = useMutation((borg) => borg.meetings.disconnectCalendar({}), {
    onSuccess: () => meetingsChanged(),
  });
  return (
    <>
      <p className="m-0 text-[13.5px] leading-[1.55] text-ink2">
        Borg reads this calendar to show what's coming up and to name the meetings it takes notes
        in.
      </p>
      {disconnect.error && (
        <div className="text-[12.5px] text-alert">{disconnect.error.message}</div>
      )}
      <div>
        <Pill
          variant="danger"
          disabled={disconnect.pending}
          onClick={() => void disconnect.run(undefined)}
        >
          Disconnect
        </Pill>
      </div>
    </>
  );
}

function Connect() {
  const [clientId, setClientId] = useState("");
  const [clientSecret, setClientSecret] = useState("");
  const [icsUrl, setIcsUrl] = useState("");
  const [waiting, setWaiting] = useState(false);
  const [message, setMessage] = useState<string>();
  const connect = useMutation(
    (
      borg,
      req: { kind: string; icsUrl?: string; googleClientId?: string; googleClientSecret?: string },
    ) =>
      borg.meetings.connectCalendar({
        kind: req.kind,
        icsUrl: req.icsUrl ?? "",
        googleClientId: req.googleClientId ?? "",
        googleClientSecret: req.googleClientSecret ?? "",
      }),
  );

  // The sign-in finishes in the browser; coming back to this window is when
  // the person looks for the result, so ask again then.
  useEffect(() => {
    if (!waiting) return;
    const onFocus = () => meetingsChanged();
    window.addEventListener("focus", onFocus);
    return () => window.removeEventListener("focus", onFocus);
  }, [waiting]);

  // A sign-in that fails after the browser step (a mistyped secret, a
  // timeout, a refusal) arrives as CalendarChanged with a reason. Without
  // this the page would wait forever.
  useRetryingServerStream<meetings.MeetingEvent>(
    waiting ? (borg, handlers) => borg.meetings.subscribeMeetingEvents({}, handlers) : idle,
    (event) => {
      const error = event.calendarChanged?.error;
      if (error) {
        setWaiting(false);
        setMessage(error);
      }
    },
    [waiting],
  );

  const google = async () => {
    if (clientId.trim() === "" || clientSecret.trim() === "") {
      setMessage("Paste both the client ID and the client secret.");
      return;
    }
    setMessage(undefined);
    const res = await connect.run({
      kind: "google",
      googleClientId: clientId.trim(),
      googleClientSecret: clientSecret.trim(),
    });
    if (!res) return;
    if (res.authUrl && openExternal(res.authUrl)) {
      setWaiting(true);
      setMessage(undefined);
    } else {
      setMessage(res.statusMessage || "Google didn't give Borg a sign-in page to open.");
    }
  };

  const ics = async () => {
    if (icsUrl.trim() === "") {
      setMessage("Paste the calendar's secret address first.");
      return;
    }
    setMessage(undefined);
    const res = await connect.run({ kind: "ics", icsUrl: icsUrl.trim() });
    if (!res) return;
    meetingsChanged();
    if (res.statusMessage) setMessage(res.statusMessage);
  };

  return (
    <>
      {(message || connect.error) && (
        <div
          role="alert"
          className="rounded-[10px] border border-alert bg-alert-soft px-4 py-3 text-[12.5px] leading-[1.45] text-ink2"
        >
          {message ?? connect.error?.message}
        </div>
      )}
      <section className="flex flex-col gap-4">
        <div className={SECTION_LABEL}>Google Calendar</div>
        {waiting ? (
          <div className="flex items-center gap-3 text-[13.5px] leading-[1.55] text-ink2">
            <span className="flex-1">Finish signing in in your browser…</span>
            <Pill type="button" onClick={() => setWaiting(false)}>
              Cancel
            </Pill>
          </div>
        ) : (
          <>
            <SecretField label="Client ID" value={clientId} secret={false} onChange={setClientId} />
            <SecretField
              label="Client secret"
              value={clientSecret}
              onChange={setClientSecret}
              onEnter={() => void google()}
            />
            <div className="text-[11.5px] leading-[1.45] text-ink3">
              From a Desktop OAuth client in your own Google Cloud project.
            </div>
            <div>
              <Pill variant="primary" disabled={connect.pending} onClick={() => void google()}>
                Connect Google Calendar
              </Pill>
            </div>
          </>
        )}
      </section>
      <section className="flex flex-col gap-4 border-t border-line2 pt-6">
        <div className={SECTION_LABEL}>Calendar feed (ICS)</div>
        <SecretField
          label="Feed URL"
          value={icsUrl}
          placeholder="https://…/basic.ics"
          onChange={setIcsUrl}
          onEnter={() => void ics()}
        />
        <div>
          <Pill disabled={connect.pending} onClick={() => void ics()}>
            Use this feed
          </Pill>
        </div>
      </section>
    </>
  );
}

/** `/connections/calendar`: connect Google Calendar or an ICS feed, or disconnect. */
export function CalendarPage() {
  const isMobile = useIsMobile();
  const status = useMeetingsStatus();
  return (
    <PageChrome title="Calendar">
      <div className={scrollCol(isMobile, "px-[38px] py-[34px]")}>
        <div className={DETAIL_COL}>
          <BackLink to="/connections" label="Connections" />
          <DaemonState query={status} label="the calendar">
            {(s) => {
              const card = calendarCard(s.calendar);
              return (
                <>
                  <div className="flex items-center gap-4">
                    <Badge on sizeClasses="h-10 w-10 rounded-xl text-[12.5px]">
                      {card.badge}
                    </Badge>
                    <div className="min-w-0 flex-1">
                      <h2 className="m-0 text-[24px] leading-[1.2] font-light tracking-[-0.025em] text-ink">
                        {card.name}
                      </h2>
                      <div className="mt-1 text-[12.5px] leading-[1.4] text-ink3">{card.sub}</div>
                    </div>
                    <StatusDot tone={card.tone} />
                  </div>
                  {s.calendar?.connected ? <Connected /> : <Connect />}
                </>
              );
            }}
          </DaemonState>
        </div>
      </div>
    </PageChrome>
  );
}
