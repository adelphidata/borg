import { useEffect, useRef, useState } from "react";
import type { meetings } from "@borg/client";
import { Markdown, Meter, Pill, StatusDot } from "@borg/ui";
import { MEETING_PANEL_HASH } from "../../lib/chords";
import { isMacPlatform } from "../../lib/keybinds";
import { useNative } from "../../lib/platform";
import { asBorgError, useRetryingServerStream } from "../../lib/useDaemon";
import { useMeetingCapture } from "./capture/useMeetingCapture";
import { meetingsChanged } from "./meetingsStore";
import { NotesEditor } from "./NotesEditor";
import { useMeeting } from "./useMeetings";
import { useNotesDraft } from "./useNotesDraft";

/** How long after the call seems to end before recording stops by itself. */
export const AUTO_STOP_MS = 30_000;

/** The meeting id this panel window was opened for, from its hash. */
export function panelMeetingId(hash: string = window.location.hash): string | undefined {
  if (!hash.startsWith(MEETING_PANEL_HASH)) return undefined;
  try {
    return decodeURIComponent(hash.slice(MEETING_PANEL_HASH.length));
  } catch {
    return undefined;
  }
}

const DRAG = "[-webkit-app-region:drag]";
const NO_DRAG = "[-webkit-app-region:no-drag]";
/** On macOS the window buttons sit at the header's left edge; leave them room. */
const HEADER_PAD = isMacPlatform() ? "pr-4 pl-[76px]" : "px-4";

/**
 * The floating notes panel (desktop only, `#/meeting-panel/<id>`).
 *
 * It hosts the recording: while the meeting is "recording" this window
 * captures the microphone and the system's audio and streams them to the
 * daemon, and the user types notes beside it. The main process keeps the
 * window alive (hidden) if it is closed mid-meeting, so capture carries on.
 * When the meeting ends it shows "Writing notes…", then the summary.
 */
export function MeetingPanelPage({ meetingId }: { meetingId: string }) {
  const native = useNative();
  const query = useMeeting(meetingId);
  const summary = query.state === "ready" ? query.data.summary : undefined;
  const state = summary?.state ?? "";
  const capture = useMeetingCapture(meetingId, {
    active: state === "recording",
    startedAtMs: (summary?.startedAt ?? 0) * 1000,
  });
  const draft = useNotesDraft(meetingId, query.state === "ready" ? query.data.notes : undefined);
  const [failure, setFailure] = useState<string>();
  const [stopping, setStopping] = useState(false);
  const [stopError, setStopError] = useState<string>();

  const stopRef = useRef(capture.stop);
  stopRef.current = capture.stop;
  const stop = () => {
    if (stopping) return;
    setStopping(true);
    setStopError(undefined);
    draft.flush();
    stopRef
      .current()
      .catch((e: unknown) => setStopError(`Couldn't stop: ${asBorgError(e).message}`))
      .finally(() => setStopping(false));
  };
  const stopFn = useRef(stop);
  stopFn.current = stop;

  // The call seemed to end: stop on our own after a grace period, unless it
  // comes back (a dropped connection, a switched device).
  const autoStop = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  useEffect(() => () => clearTimeout(autoStop.current), []);
  const recording = state === "recording";
  const recordingRef = useRef(recording);
  recordingRef.current = recording;

  useRetryingServerStream<meetings.MeetingEvent>(
    (borg, handlers) => borg.meetings.subscribeMeetingEvents({}, handlers),
    (event) => {
      if (event.detectionEnded && recordingRef.current) {
        clearTimeout(autoStop.current);
        autoStop.current = setTimeout(() => stopFn.current(), AUTO_STOP_MS);
        return;
      }
      if (event.detected) {
        clearTimeout(autoStop.current);
        return;
      }
      const about =
        event.stopped?.id ??
        event.summaryReady?.id ??
        event.summaryFailed?.id ??
        event.started?.id ??
        event.transcriptAppended?.meetingId;
      if (about !== meetingId) return;
      if (event.summaryFailed) setFailure(event.summaryFailed.message);
      if (event.stopped) clearTimeout(autoStop.current);
      meetingsChanged();
    },
    [meetingId],
  );

  const openInBorg = () => {
    native.revealPath(`/meetings/${encodeURIComponent(meetingId)}`);
    native.hideWindow();
  };

  const title = summary?.title || "Meeting";
  const failed = state === "failed" || failure !== undefined;

  return (
    <div className="flex h-full flex-col overflow-hidden bg-surface">
      <header
        className={`flex items-center gap-2 border-b border-line2 py-3 ${HEADER_PAD} ${DRAG}`}
      >
        <div className="min-w-0 flex-1">
          <div className="truncate text-[14px] leading-[1.3] font-medium text-ink">{title}</div>
          {recording && (
            <div className="mt-1 flex items-center gap-1.5 text-[11.5px] leading-none text-ink2">
              <StatusDot tone="busy" />
              Listening
            </div>
          )}
        </div>
        {recording && (
          <Pill
            variant="danger"
            sizeClasses="px-3 py-1.5 text-[12px]"
            className={NO_DRAG}
            disabled={stopping}
            onClick={stop}
          >
            {stopping ? "Stopping…" : "Stop"}
          </Pill>
        )}
      </header>
      <div className="flex min-h-0 flex-1 flex-col gap-3 overflow-y-auto px-4 py-3">
        {query.state === "error" && (
          <div className="text-[12.5px] text-alert">{query.error.message}</div>
        )}
        {recording && (
          <>
            <Meter percent={capture.level * 100} />
            {capture.error && (
              <div className="text-[12px] leading-[1.45] text-alert">{capture.error}</div>
            )}
            {capture.systemAudio === "unavailable" && !capture.error && (
              <div className="text-[12px] leading-[1.45] text-ink3">
                Using the microphone only — system audio isn't available here.
              </div>
            )}
            {capture.warning && (
              <div className="text-[12px] leading-[1.45] text-warn">{capture.warning}</div>
            )}
            {stopError && <div className="text-[12px] leading-[1.45] text-alert">{stopError}</div>}
          </>
        )}
        {(recording || state === "finalizing") && <NotesEditor draft={draft} maxPx={360} />}
        {state === "finalizing" && !failed && (
          <div className="text-[12.5px] leading-none text-ink3">Writing notes…</div>
        )}
        {state === "done" && query.state === "ready" && (
          <div className="text-[13px] leading-[1.55] text-ink">
            <Markdown>{query.data.summaryMarkdown}</Markdown>
          </div>
        )}
        {failed && (
          <div className="flex flex-col gap-1 text-[12.5px] leading-[1.45]">
            <span className="font-medium text-ink">Borg couldn't write the notes</span>
            <span className="text-ink3">
              {failure ?? (query.state === "ready" ? query.data.error : "")}
            </span>
          </div>
        )}
      </div>
      {(state === "done" || failed) && (
        <footer className="border-t border-line2 px-4 py-3">
          <Pill variant="primary" onClick={openInBorg}>
            Open in Borg
          </Pill>
        </footer>
      )}
    </div>
  );
}
