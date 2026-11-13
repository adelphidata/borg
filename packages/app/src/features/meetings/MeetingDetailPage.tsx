import { useState } from "react";
import { useNavigate, useParams } from "react-router-dom";
import {
  BackLink,
  ClockIcon,
  Markdown,
  MeetingsIcon,
  Menu,
  MoreIcon,
  PageChrome,
  PeopleIcon,
  Pill,
  StatusDot,
  TrashIcon,
  scrollCol,
  useIsMobile,
} from "@borg/ui";
import { DaemonState } from "../../components/DaemonState";
import { showNotice } from "../../lib/notice";
import { useNative, usePlatform } from "../../lib/platform";
import { useTick } from "../../lib/useTick";
import { useMenuEscape } from "../sessions/SessionRowMenu";
import { AskBar, Chip, ChipRow, DOC_COL, DocTitle } from "./MeetingDoc";
import { duration, fullNames, segmentClock, shortDay, timeOfDay } from "./meetingsFormat";
import { meetingsChanged } from "./meetingsStore";
import { NotesEditor } from "./NotesEditor";
import { TranscriptList } from "./TranscriptList";
import {
  useDeleteMeeting,
  useMeeting,
  useResumeMeeting,
  useStopMeeting,
  type Meeting,
} from "./useMeetings";
import { useNotesDraft, type NotesDraft } from "./useNotesDraft";

/**
 * "Listening 4:07" while the meeting records. Once it stops, the Summary tab
 * says Borg is writing; saying it here too would be the same news twice.
 */
function LiveChip({ state, startedAt }: { state: string; startedAt: number }) {
  const now = useTick(1000, state === "recording");
  if (state !== "recording") return null;
  return (
    <Chip icon={<StatusDot tone="busy" />}>
      Listening
      <span className="text-ink3 tabular-nums">{segmentClock(now - startedAt * 1000)}</span>
    </Chip>
  );
}

type View = "summary" | "notes" | "transcript";

/** Summary / My notes / Transcript: one document, three ways to read it. */
function ViewSwitch({
  views,
  view,
  onChange,
}: {
  views: { id: View; label: string }[];
  view: View;
  onChange: (view: View) => void;
}) {
  if (views.length < 2) return null;
  return (
    <div role="tablist" className="flex gap-1 border-b border-line2">
      {views.map((v) => (
        <button
          key={v.id}
          type="button"
          role="tab"
          aria-selected={v.id === view}
          onClick={() => onChange(v.id)}
          className={`-mb-px cursor-pointer border-0 border-b-2 bg-transparent px-2.5 pt-1 pb-2.5 text-[12.5px] leading-none ${
            v.id === view
              ? "border-neon font-medium text-ink"
              : "border-transparent text-ink3 hover:text-ink"
          }`}
        >
          {v.label}
        </button>
      ))}
    </div>
  );
}

function SummaryView({ meeting }: { meeting: Meeting }) {
  const s = meeting.summary!;
  if (s.hasSummary && meeting.summaryMarkdown !== "") {
    return (
      <div className="text-[14.5px] leading-[1.7] text-ink select-text">
        <Markdown>{meeting.summaryMarkdown}</Markdown>
      </div>
    );
  }
  if (s.state === "failed") {
    return (
      <div className="rounded-[10px] border border-alert bg-alert-soft px-4 py-3 text-[12.5px] leading-[1.45] text-ink2">
        Borg couldn't write the notes{meeting.error ? `: ${meeting.error}` : "."}
      </div>
    );
  }
  return <div className="text-[13.5px] leading-[1.5] text-ink3">Borg is writing the summary…</div>;
}

/** A summary to show: written, being written, or failed with a reason. */
function hasSummaryView(meeting: Meeting): boolean {
  const s = meeting.summary!;
  return (
    (s.hasSummary && meeting.summaryMarkdown !== "") ||
    s.state === "finalizing" ||
    s.state === "failed"
  );
}

function MeetingBody({ meeting, draft }: { meeting: Meeting; draft: NotesDraft }) {
  const s = meeting.summary!;
  const views: { id: View; label: string }[] = [];
  // The summary leads once there is one, or once Borg is on it.
  if (hasSummaryView(meeting)) views.push({ id: "summary", label: "Summary" });
  views.push({ id: "notes", label: "My notes" });
  if (meeting.segments.length > 0) views.push({ id: "transcript", label: "Transcript" });
  // The tab is chosen once, when the page opens: a recording that stops
  // must not pull the notes box away from someone typing in it.
  const [picked, setPicked] = useState<View>(views[0]!.id);
  const view = views.find((v) => v.id === picked)?.id ?? views[0]!.id;

  return (
    <>
      <ViewSwitch views={views} view={view} onChange={setPicked} />
      {view === "summary" && <SummaryView meeting={meeting} />}
      {view === "notes" && <NotesEditor draft={draft} />}
      {view === "transcript" && <TranscriptList segments={meeting.segments} />}
      <AskBar meeting={{ id: s.id, title: s.title }} />
    </>
  );
}

/** `/meetings/:id`: one meeting as a document — summary, notes, transcript, ask. */
export function MeetingDetailPage() {
  const { id = "" } = useParams();
  // Keyed: the notes box belongs to one meeting and must not carry its text
  // into the next one opened from the same route.
  return <MeetingView key={id} id={id} />;
}

function MeetingView({ id }: { id: string }) {
  const isMobile = useIsMobile();
  const navigate = useNavigate();
  const query = useMeeting(id, { markRead: true });
  const draft = useNotesDraft(id, query.state === "ready" ? query.data.notes : undefined);
  const stop = useStopMeeting();
  const resume = useResumeMeeting();
  const remove = useDeleteMeeting();
  const native = useNative();
  // Only the desktop app can record, so only it offers to record more.
  const canRecord = usePlatform().kind === "desktop";
  const [menuOpen, setMenuOpen] = useState(false);
  useMenuEscape(menuOpen, () => setMenuOpen(false));
  const now = new Date(useTick(60_000, true));

  const summary = query.state === "ready" ? query.data.summary : undefined;
  const title = summary?.title || "Meeting";

  const rename = async (next: string) => {
    const ok = await draft.rename(next);
    if (ok) meetingsChanged();
    else showNotice("Couldn't rename the meeting.");
    return ok;
  };

  const actions =
    summary?.state === "recording" ? (
      <Pill
        variant="danger"
        sizeClasses="px-3 py-1.5 text-[12px]"
        disabled={stop.pending}
        onClick={() => void stop.run(summary.id)}
      >
        Stop
      </Pill>
    ) : summary ? (
      <div className="relative flex items-center gap-2">
        {canRecord && (summary.state === "done" || summary.state === "failed") && (
          <Pill
            sizeClasses="px-3 py-1.5 text-[12px]"
            disabled={resume.pending}
            onClick={() =>
              void resume.run(summary.id).then((r) => {
                if (r !== undefined) native.meetings.openPanel(summary.id);
              })
            }
          >
            {resume.pending ? "Resuming…" : "Resume"}
          </Pill>
        )}
        <button
          type="button"
          aria-label="Meeting actions"
          aria-expanded={menuOpen}
          onPointerDown={(e) => e.stopPropagation()}
          onClick={() => setMenuOpen((v) => !v)}
          className="flex h-7 w-7 cursor-pointer items-center justify-center rounded border-0 bg-transparent text-ink3 hover:text-ink"
        >
          <MoreIcon size={15} />
        </button>
        <Menu
          open={menuOpen}
          onClose={() => setMenuOpen(false)}
          label="Meeting actions"
          actions={[
            {
              id: "delete",
              label: "Delete",
              icon: <TrashIcon size={13} />,
              onSelect: () =>
                void remove.run(summary.id).then((r) => {
                  if (r !== undefined) {
                    showNotice(`Deleted “${title}”`);
                    navigate("/meetings");
                  }
                }),
            },
          ]}
        />
      </div>
    ) : undefined;

  const people = summary ? fullNames(summary.attendees) : [];
  const length = summary ? duration(summary.startedAt, summary.endedAt) : "";

  return (
    <PageChrome title="Meetings" headerActions={actions}>
      <div className={scrollCol(isMobile, "px-[38px] pt-[30px]")}>
        <div className={DOC_COL}>
          <BackLink to="/meetings" label="Meetings" />
          {(stop.error ?? resume.error ?? remove.error) && (
            <div className="rounded-[10px] border border-alert bg-alert-soft px-4 py-3 text-[12.5px] leading-[1.45] text-ink2">
              {(stop.error ?? resume.error ?? remove.error)!.message}
            </div>
          )}
          <DaemonState query={query} label="this meeting">
            {(m) =>
              m.summary ? (
                <>
                  <div className="flex flex-col gap-3">
                    <DocTitle title={title} onRename={rename} />
                    <ChipRow>
                      <Chip icon={<MeetingsIcon size={13} />}>
                        {shortDay(new Date(m.summary.startedAt * 1000), now)} ·{" "}
                        {timeOfDay(m.summary.startedAt)}
                      </Chip>
                      {length !== "" && <Chip icon={<ClockIcon />}>{length}</Chip>}
                      {people.length > 0 && (
                        <Chip icon={<PeopleIcon />} title={people.join(", ")}>
                          {people.length}
                        </Chip>
                      )}
                      <LiveChip state={m.summary.state} startedAt={m.summary.startedAt} />
                    </ChipRow>
                  </div>
                  <MeetingBody meeting={m} draft={draft} />
                </>
              ) : (
                <div className="text-[14px] text-ink2">That meeting doesn't exist.</div>
              )
            }
          </DaemonState>
        </div>
      </div>
    </PageChrome>
  );
}
