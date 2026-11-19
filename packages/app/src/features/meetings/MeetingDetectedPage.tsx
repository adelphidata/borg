import { useEffect, useState } from "react";
import { CloseIcon } from "@borg/ui";
import type { MeetingDetection } from "../../lib/native";
import { useNative } from "../../lib/platform";
import { asBorgError, useBorg } from "../../lib/useDaemon";
import { useMeetingsStatus } from "./useMeetings";

/**
 * The "Meeting detected" pop-up (desktop only, `#/meeting-detected`).
 *
 * Nothing records until the user says so: "Take notes" starts the meeting
 * and opens the notes panel; the close button (or Escape) puts it away. When
 * no transcriber is set up yet the button takes the user to where that is
 * fixed instead of starting a meeting that could not be transcribed.
 */
export function MeetingDetectedPage() {
  const native = useNative();
  const borg = useBorg();
  const status = useMeetingsStatus();
  const [detection, setDetection] = useState<MeetingDetection | null>(null);
  const [starting, setStarting] = useState(false);
  const [error, setError] = useState<string>();

  // The pop-up is built once and reused, so whether transcription is ready
  // is asked again for each call rather than remembered from the first.
  const { refetch } = status;
  useEffect(
    () =>
      native.meetings.onDetection((d) => {
        setDetection(d);
        setError(undefined);
        if (d) refetch();
      }),
    [native, refetch],
  );

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") native.hideWindow();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [native]);

  if (!detection) return null;
  const ready = status.state !== "ready" || status.data.transcriptionReady;

  const takeNotes = async () => {
    if (starting) return;
    setStarting(true);
    setError(undefined);
    try {
      const started = await borg.meetings.startMeeting({
        title: "",
        sourceApp: detection.sourceApp,
        calendarEventId: detection.calendarEventId ?? "",
      });
      if (started.summary) native.meetings.openPanel(started.summary.id);
    } catch (e) {
      setError(asBorgError(e).message);
    } finally {
      setStarting(false);
    }
  };

  const setUp = () => {
    native.revealPath("/settings/meetings");
    native.hideWindow();
  };

  const sub = [detection.calendarTitle, detection.sourceApp].filter(Boolean).join(" · ");

  return (
    <div className="relative flex h-full items-center gap-3 rounded-[14px] border border-line bg-surface2 px-4 py-3 shadow-(--frameShadow) [-webkit-app-region:drag]">
      <button
        type="button"
        aria-label="Close"
        title="Close"
        onClick={() => native.hideWindow()}
        className="absolute top-1.5 left-1.5 flex h-5 w-5 cursor-pointer items-center justify-center rounded-full border border-line bg-surface text-ink3 hover:text-ink [-webkit-app-region:no-drag]"
      >
        <CloseIcon size={10} />
      </button>
      <div className="min-w-0 flex-1 pl-2">
        <div className="text-[13.5px] leading-[1.3] font-medium text-ink">Meeting detected</div>
        <div className="mt-1 truncate text-[12px] leading-[1.3] text-ink3">
          {error ? <span className="text-alert">{error}</span> : sub}
        </div>
      </div>
      <div className="flex flex-none [-webkit-app-region:no-drag]">
        {ready ? (
          <button
            type="button"
            disabled={starting}
            onClick={() => void takeNotes()}
            className="cursor-pointer rounded-full border-0 bg-neon px-3.5 py-2 text-[12.5px] leading-none font-medium text-on-neon disabled:opacity-60"
          >
            {starting ? "Starting…" : "Take notes"}
          </button>
        ) : (
          <button
            type="button"
            onClick={setUp}
            className="cursor-pointer rounded-full border border-line bg-transparent px-3.5 py-2 text-[12.5px] leading-none font-medium text-ink"
          >
            Set up transcription
          </button>
        )}
      </div>
    </div>
  );
}
