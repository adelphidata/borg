import { useEffect, useState } from "react";
import { ChevronDownIcon, Menu } from "@borg/ui";
import type { MeetingDetection } from "../../lib/native";
import { useNative } from "../../lib/platform";
import { asBorgError, useBorg } from "../../lib/useDaemon";
import { useMeetingsStatus } from "./useMeetings";

/**
 * The "Meeting detected" pop-up (desktop only, `#/meeting-detected`).
 *
 * Nothing records until the user says so: "Take notes" starts the meeting
 * and opens the notes panel; "Not this meeting" puts the pop-up away. When
 * no transcriber is set up yet the button takes the user to where that is
 * fixed instead of starting a meeting that could not be transcribed.
 */
export function MeetingDetectedPage() {
  const native = useNative();
  const borg = useBorg();
  const status = useMeetingsStatus();
  const [detection, setDetection] = useState<MeetingDetection | null>(null);
  const [menuOpen, setMenuOpen] = useState(false);
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
    <div className="flex h-full items-center gap-3 rounded-[14px] border border-line bg-surface2 px-4 py-3 shadow-(--frameShadow) [-webkit-app-region:drag]">
      <div className="min-w-0 flex-1">
        <div className="text-[13.5px] leading-[1.3] font-medium text-ink">Meeting detected</div>
        <div className="mt-1 truncate text-[12px] leading-[1.3] text-ink3">
          {error ? <span className="text-alert">{error}</span> : sub}
        </div>
      </div>
      <div className="relative flex flex-none items-stretch [-webkit-app-region:no-drag]">
        {ready ? (
          <button
            type="button"
            disabled={starting}
            onClick={() => void takeNotes()}
            className="cursor-pointer rounded-l-full border-0 bg-neon py-2 pr-2.5 pl-3.5 text-[12.5px] leading-none font-medium text-on-neon disabled:opacity-60"
          >
            {starting ? "Starting…" : "Take notes"}
          </button>
        ) : (
          <button
            type="button"
            onClick={setUp}
            className="cursor-pointer rounded-l-full border border-r-0 border-line bg-transparent py-2 pr-2.5 pl-3.5 text-[12.5px] leading-none font-medium text-ink"
          >
            Set up transcription
          </button>
        )}
        <button
          type="button"
          aria-label="More choices"
          aria-expanded={menuOpen}
          onPointerDown={(e) => e.stopPropagation()}
          onClick={() => setMenuOpen((v) => !v)}
          className={`flex cursor-pointer items-center rounded-r-full py-2 pr-2.5 pl-1.5 ${
            ready
              ? "border-0 border-l border-on-neon/15 bg-neon text-on-neon"
              : "border border-line bg-transparent text-ink"
          }`}
        >
          <ChevronDownIcon size={12} />
        </button>
        <Menu
          open={menuOpen}
          onClose={() => setMenuOpen(false)}
          label="Meeting choices"
          actions={[
            {
              id: "dismiss",
              label: "Not this meeting",
              onSelect: () => native.hideWindow(),
            },
          ]}
        />
      </div>
    </div>
  );
}
