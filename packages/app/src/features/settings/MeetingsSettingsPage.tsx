import { Link } from "react-router-dom";
import {
  BackLink,
  DETAIL_COL,
  PageChrome,
  SETTINGS_SCROLL_DESKTOP_PADDING,
  scrollCol,
  useIsMobile,
} from "@borg/ui";
import { DaemonState } from "../../components/DaemonState";
import { useSettings } from "../../lib/settings";
import { useVoiceCatalog, type VoiceProvider } from "../../lib/voice";
import { meetingsEnabled } from "../meetings/meetingsEnabled";
import { useMeetingsStatus, type MeetingsStatus } from "../meetings/useMeetings";
import { useTurnOnMeetings } from "../meetings/useTurnOnMeetings";
import { CARD, MoreOptions, Switch, TextSetting, useChain } from "./fields";
import { TranscriberPicker } from "./TranscriberPicker";

/** "Google · name@example.com", "ICS feed", or "Not connected". */
export function calendarLabel(calendar: MeetingsStatus["calendar"]): string {
  if (!calendar?.connected) return "Not connected";
  const kind = calendar.kind === "google" ? "Google" : calendar.kind === "ics" ? "ICS feed" : "";
  return [kind, calendar.accountLabel].filter(Boolean).join(" · ") || "Connected";
}

function MeetingsCard({
  providers,
  ready,
  refetch,
  status,
}: {
  providers: VoiceProvider[];
  ready: boolean;
  refetch: () => void;
  status: MeetingsStatus | undefined;
}) {
  const { settings, save } = useSettings();
  const chain = useChain("audio.models");
  const turnOn = useTurnOnMeetings(chain, providers);
  if (settings.state !== "ready") return null;
  const enabled = meetingsEnabled(settings.data);
  const value = (key: string) => settings.data.get(key)?.value ?? "";

  return (
    <div className={CARD}>
      <Switch label="Take notes in meetings" on={enabled} onChange={(on) => void turnOn(on)} />
      {enabled && (
        <>
          <TranscriberPicker chain={chain} providers={providers} ready={ready} refetch={refetch} />
          <div className="flex items-center gap-3 border-t border-line2 pt-4">
            <div className="min-w-0 flex-1">
              <div className="text-[13.5px] leading-[1.3] font-medium text-ink">Calendar</div>
              <div className="mt-[3px] text-[12px] leading-[1.45] text-ink3">
                {status ? calendarLabel(status.calendar) : "…"}
              </div>
            </div>
            <Link
              to="/connections/calendar"
              className="text-[12.5px] font-medium text-neon-ink hover:underline"
            >
              {status?.calendar?.connected ? "Change" : "Connect"}
            </Link>
          </div>
          {status && !status.detectionSupported && (
            <div className="text-[12px] leading-[1.45] text-ink3">
              Borg notices meetings automatically on a Mac; elsewhere it follows your calendar.
            </div>
          )}
          <MoreOptions>
            <TextSetting
              label="Folder"
              value={value("meetings.dir")}
              placeholder={status?.recordingsDir || "~/.borg/recordings/meetings"}
              onCommit={(v) => save("meetings.dir", v.trim())}
            />
            <Switch
              label="Keep audio"
              on={value("meetings.keep_audio") === "true"}
              onChange={(on) => void save("meetings.keep_audio", String(on))}
            />
            <Switch
              label="Ask before recording"
              warning={
                value("meetings.ask_before_recording") === "false"
                  ? "Meetings on your calendar start recording as soon as they are noticed."
                  : undefined
              }
              on={value("meetings.ask_before_recording") !== "false"}
              onChange={(on) => void save("meetings.ask_before_recording", String(on))}
            />
          </MoreOptions>
        </>
      )}
    </div>
  );
}

/**
 * Settings → Meetings. One switch turns note-taking on; with it on, the
 * transcriber (shared with Voice input, from the daemon's own list) and the
 * calendar. The folder and the recording rules wait under "More options".
 */
export function MeetingsSettingsPage() {
  const isMobile = useIsMobile();
  const { saveError } = useSettings();
  const catalog = useVoiceCatalog();
  const status = useMeetingsStatus();

  return (
    <PageChrome title="Meetings">
      <div className={scrollCol(isMobile, SETTINGS_SCROLL_DESKTOP_PADDING)}>
        <div className={DETAIL_COL}>
          <BackLink to="/settings" label="Settings" />
          <h2 className="m-0 text-[24px] leading-[1.2] font-light tracking-[-0.025em] text-ink">
            Meetings
          </h2>
          {saveError && (
            <div className="rounded-[10px] border border-alert bg-alert-soft px-4 py-3 text-[12.5px] leading-[1.45] text-ink2">
              {saveError.message}
            </div>
          )}
          <DaemonState query={catalog} label="transcription providers">
            {(data) => (
              <MeetingsCard
                providers={data.transcription}
                ready={
                  status.state === "ready"
                    ? status.data.transcriptionReady
                    : data.transcriptionReady
                }
                refetch={() => {
                  catalog.refetch();
                  status.refetch();
                }}
                status={status.state === "ready" ? status.data : undefined}
              />
            )}
          </DaemonState>
          {status.state === "error" && !status.error.isUnavailable && (
            <div className="text-[12.5px] leading-[1.45] text-ink3">
              This version of Borg can't take meeting notes yet.
            </div>
          )}
        </div>
      </div>
    </PageChrome>
  );
}
