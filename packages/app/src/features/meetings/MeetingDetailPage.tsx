import { useState } from "react";
import { useNavigate, useParams } from "react-router-dom";
import {
  BackLink,
  DETAIL_COL,
  Markdown,
  Menu,
  MoreIcon,
  PageChrome,
  Pill,
  SECTION_LABEL,
  SendIcon,
  StatusDot,
  TrashIcon,
  isSubmitEnter,
  scrollCol,
  useIsMobile,
} from "@borg/ui";
import { DaemonState } from "../../components/DaemonState";
import { showNotice } from "../../lib/notice";
import { asBorgError, useBorg } from "../../lib/useDaemon";
import { useTick } from "../../lib/useTick";
import { useMenuEscape } from "../sessions/SessionRowMenu";
import { askAboutMeeting } from "./askAboutMeeting";
import { meetingMeta, segmentClock } from "./meetingsFormat";
import { meetingsChanged } from "./meetingsStore";
import { NotesEditor } from "./NotesEditor";
import { TranscriptFold } from "./TranscriptFold";
import { useDeleteMeeting, useMeeting, useStopMeeting, type Meeting } from "./useMeetings";
import { useNotesDraft, type NotesDraft } from "./useNotesDraft";

/** The line under the title while the meeting is still live or being written up. */
function RecordingBanner({ state, startedAt }: { state: string; startedAt: number }) {
  const now = useTick(1000, state === "recording");
  if (state === "recording") {
    return (
      <div className="flex items-center gap-2 text-[12.5px] leading-none text-ink2">
        <StatusDot tone="busy" />
        Listening
        <span className="text-ink3 tabular-nums">{segmentClock(now - startedAt * 1000)}</span>
      </div>
    );
  }
  if (state === "finalizing") {
    return <div className="text-[12.5px] leading-none text-ink3">Writing notes…</div>;
  }
  return null;
}

/** "Ask anything about this meeting": starts a chat that knows which meeting. */
function AskBar({ meeting }: { meeting: { id: string; title: string } }) {
  const borg = useBorg();
  const navigate = useNavigate();
  const [question, setQuestion] = useState("");
  const [asking, setAsking] = useState(false);
  const [error, setError] = useState<string>();
  const submit = async () => {
    if (question.trim() === "" || asking) return;
    setAsking(true);
    setError(undefined);
    try {
      const sessionId = await askAboutMeeting(borg, question, meeting);
      navigate(`/c/${sessionId}`);
    } catch (e) {
      setError(`Couldn't ask Borg: ${asBorgError(e).message}`);
      setAsking(false);
    }
  };
  return (
    <div className="sticky bottom-0 bg-surface pt-3 pb-1">
      <div className="flex items-center gap-2 rounded-[14px] border border-line bg-surface2 px-4 py-2.5 shadow-(--frameShadow)">
        <input
          aria-label="Ask about this meeting"
          className="min-w-0 flex-1 border-0 bg-transparent text-[14px] leading-[1.4] text-ink outline-none placeholder:text-ink3"
          placeholder="Ask anything"
          value={question}
          onChange={(e) => setQuestion(e.target.value)}
          onKeyDown={(e) => {
            if (isSubmitEnter(e)) {
              e.preventDefault();
              void submit();
            }
          }}
        />
        <button
          type="button"
          aria-label="Ask"
          disabled={asking}
          onClick={() => void submit()}
          className="flex h-[27px] w-[27px] flex-none cursor-pointer items-center justify-center rounded-[8px] border-0 bg-neon disabled:opacity-50"
        >
          <SendIcon size={13} />
        </button>
      </div>
      {error && <div className="mt-2 text-[12px] leading-[1.45] text-alert">{error}</div>}
    </div>
  );
}

function MeetingBody({ meeting, draft }: { meeting: Meeting; draft: NotesDraft }) {
  const s = meeting.summary!;
  return (
    <>
      <RecordingBanner state={s.state} startedAt={s.startedAt} />
      <NotesEditor draft={draft} />
      {s.hasSummary && meeting.summaryMarkdown !== "" ? (
        <section>
          <div className={SECTION_LABEL}>Summary</div>
          <div className="text-[14px] leading-[1.6] text-ink">
            <Markdown>{meeting.summaryMarkdown}</Markdown>
          </div>
        </section>
      ) : s.state === "finalizing" ? (
        <div className="text-[13px] leading-[1.5] text-ink3">Borg is writing the summary…</div>
      ) : s.state === "failed" ? (
        <div className="rounded-[10px] border border-alert bg-alert-soft px-4 py-3 text-[12.5px] leading-[1.45] text-ink2">
          Borg couldn't write the notes{meeting.error ? `: ${meeting.error}` : "."}
        </div>
      ) : null}
      <TranscriptFold segments={meeting.segments} />
      <AskBar meeting={{ id: s.id, title: s.title }} />
    </>
  );
}

/** `/meetings/:id`: one meeting, Granola-style — notes, summary, transcript, ask. */
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
  const remove = useDeleteMeeting();
  const [menuOpen, setMenuOpen] = useState(false);
  useMenuEscape(menuOpen, () => setMenuOpen(false));

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
      <div className="relative">
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

  return (
    <PageChrome
      title={title}
      meta={summary ? meetingMeta(summary) : undefined}
      onRename={summary ? rename : undefined}
      renameLabel="Meeting name"
      headerActions={actions}
    >
      <div className={scrollCol(isMobile, "px-[38px] pt-[34px]")}>
        <div className={DETAIL_COL}>
          <BackLink to="/meetings" label="Meetings" />
          {(stop.error ?? remove.error) && (
            <div className="rounded-[10px] border border-alert bg-alert-soft px-4 py-3 text-[12.5px] leading-[1.45] text-ink2">
              {(stop.error ?? remove.error)!.message}
            </div>
          )}
          <DaemonState query={query} label="this meeting">
            {(m) =>
              m.summary ? (
                <MeetingBody meeting={m} draft={draft} />
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
