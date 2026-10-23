import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { useNavigate, useParams } from "react-router-dom";
import { AgentsIcon, CONVERSATION_COL, FindBar, PageChrome, Pill, useIsMobile } from "@borg/ui";
import { AgentPanel } from "../agents/AgentPanel";
import { DaemonState } from "../../components/DaemonState";
import { formatElapsed } from "../../lib/format";
import { useKeybind } from "../../lib/keybinds";
import { usePlatform } from "../../lib/platform";
import { useAutoSpeak, useSpeechSource } from "../../lib/speech";
import { useVoicePrefs } from "../../lib/voice";
import { replyId } from "./cells";
import { useTick } from "../../lib/useTick";
import { markSessionSeen } from "../sessions/unreadStore";
import { Composer } from "./Composer";
import { ContextBar } from "./ContextBar";
import { draftKeyFor } from "./draftStore";
import { useFindInTranscript } from "./useFindInTranscript";
import { useFollowBottom, wheelReachesScroller } from "./followBottom";
import { ChangedFilesCard } from "./cells/ChangedFilesCard";
import { PlanPanel } from "./cells/PlanPanel";
import { ToolRunCell } from "./cells/ToolRunCell";
import { TranscriptCell } from "./cells";
import { groupCells } from "./cells/toolRuns";
import { placeTurnUsage } from "./cells/turnUsage";
import {
  INITIAL_BUDGET,
  budgetCutIndex,
  cutHidesEverything,
  pageKeyDirection,
  revealEarlier,
  shouldRevealOnScroll,
  shouldRevealOnWheel,
} from "./renderBudget";
import { FailureCard } from "./FailureCard";
import {
  busyVerb,
  createActivityHold,
  turnActivity,
  visibleLabel,
  type ActivityHold,
} from "./turnActivity";
import { collectEdits, truncatedPatchCount, turnEditCells } from "./turnEdits";
import { useConversation } from "./useConversation";
import { displayTitle } from "./userTurn";
import { useTurnNotifications } from "./useTurnNotifications";

export function ConversationPage() {
  const { sessionId: target } = useParams();
  const isMobile = useIsMobile();
  const navigate = useNavigate();
  const { kind } = usePlatform();
  const [compacting, setCompacting] = useState(false);
  const [activityOpen, setActivityOpen] = useState(false);
  const {
    opened,
    ready,
    sessionId: liveSessionId,
    cells,
    plan,
    busy,
    awaitingInput,
    turnStartedAt,
    sendError,
    streamError,
    streamOutageSustained,
    send,
    runCommand,
    retryLast,
    steer,
    cancel,
    respond,
    rename,
    editAndResend,
    compact,
    fork,
    context,
  } = useConversation(target);

  // A live count of the wait, ticking only while there is a wait to count.
  const now = useTick(1_000, busy && turnStartedAt !== undefined);

  // What the turn is waiting on, damped so a run of quick tool calls does not
  // flash a guess between every pair. The hold is a ref because it is a running
  // account of what has already been shown, not state the render derives.
  // Built once, not on every render — `useRef(createActivityHold())` would
  // allocate a fresh hold per render and throw all but the first away.
  const holdRef = useRef<ActivityHold>(undefined);
  holdRef.current ??= createActivityHold();
  const hold = holdRef as { current: ActivityHold };
  useEffect(() => {
    if (!busy) hold.current.reset();
  }, [busy]);
  const activity = busy ? hold.current.settle(turnActivity(cells), now) : undefined;
  const shownActivity = activity && visibleLabel(activity, busyVerb(turnStartedAt));

  // The daemon rewinds by user-message ordinal, so each user cell needs its
  // position among user cells rather than its index in the list.
  const ordinals = useMemo(() => {
    let nth = -1;
    return cells.map((cell) => (cell.kind === "user" ? ++nth : -1));
  }, [cells]);

  // What this turn changed on disk, read off the patch calls it already made.
  // Only once the turn settles — that is the only time the card renders, and
  // parsing every diff again on each streaming delta would cost the whole
  // transcript's frame rate to build something nobody is looking at.
  const edits = useMemo(() => (busy ? [] : collectEdits(turnEditCells(cells))), [cells, busy]);
  const uncounted = useMemo(
    () => (busy ? 0 : truncatedPatchCount(turnEditCells(cells))),
    [cells, busy],
  );
  // Settled tool calls fold into runs so a tool-heavy turn stays readable.
  const items = useMemo(() => groupCells(cells, busy), [cells, busy]);
  const usage = useMemo(() => placeTurnUsage(cells), [cells]);

  // How much of a long history is mounted. Held as a cell index rather than
  // recomputed per render: the budget is spent once, when the conversation
  // opens, so a streaming reply cannot push history the user is reading back
  // over the line and hide it mid-sentence.
  const [cutIndex, setCutIndex] = useState(0);
  const budgeted = useRef(false);
  useEffect(() => {
    budgeted.current = false;
    setCutIndex(0);
  }, [target]);
  useEffect(() => {
    if (budgeted.current || items.length === 0) return;
    budgeted.current = true;
    setCutIndex(budgetCutIndex(items, INITIAL_BUDGET));
  }, [items]);
  // Editing a message rewinds the transcript, so the cut can end up past
  // everything left. Showing all of it is the safe answer — the history is
  // short again.
  useEffect(() => {
    if (cutHidesEverything(items, cutIndex)) setCutIndex(0);
  }, [items, cutIndex]);

  const visible = useMemo(
    () => (cutIndex > 0 ? items.filter((item) => item.index >= cutIndex) : items),
    [items, cutIndex],
  );
  const hiddenCount = items.length - visible.length;

  // Revealing older history mounts cells ABOVE the reading position, which
  // would otherwise shove the page down by however much they turned out to be.
  // Distance from the bottom is the one measure that survives that.
  const scrollRef = useRef<HTMLDivElement>(null);
  const contentRef = useRef<HTMLDivElement>(null);
  const fromBottom = useRef<number | undefined>(undefined);
  const showEarlier = () => {
    const el = scrollRef.current;
    fromBottom.current = el ? el.scrollHeight - el.scrollTop : undefined;
    setCutIndex((current) => revealEarlier(items, current));
  };
  useLayoutEffect(() => {
    const el = scrollRef.current;
    if (!el || fromBottom.current === undefined) return;
    el.scrollTop = el.scrollHeight - fromBottom.current;
    fromBottom.current = undefined;
  }, [cutIndex]);

  // Scrolling up into the top edge reveals the next page by itself; the
  // button stays as the visible promise that there is more, and for anyone
  // who would rather click. Through a ref so the listener is registered once
  // rather than on every change to the transcript.
  const showEarlierRef = useRef(showEarlier);
  showEarlierRef.current = showEarlier;
  const hasHidden = cutIndex > 0;
  useEffect(() => {
    const el = scrollRef.current;
    if (!el || !hasHidden) return;
    let previousTop = el.scrollTop;
    const onScroll = () => {
      const top = el.scrollTop;
      // One reveal at a time: a pending one has yet to re-anchor the
      // scroller, and firing again would measure a page that is mid-move.
      if (fromBottom.current === undefined && shouldRevealOnScroll(previousTop, top)) {
        showEarlierRef.current();
      }
      previousTop = top;
    };
    // A view already at the top cannot move, so the wheel that asks for more
    // sends no scroll event; the wheel itself is the only sign of it. A wheel
    // turned inside a tool's output box scrolls that box, not the history.
    const onWheel = (event: WheelEvent) => {
      if (
        shouldRevealOnWheel(event.deltaY, el.scrollTop, fromBottom.current !== undefined) &&
        wheelReachesScroller(event, el)
      ) {
        showEarlierRef.current();
      }
    };
    el.addEventListener("scroll", onScroll, { passive: true });
    el.addEventListener("wheel", onWheel, { passive: true });
    return () => {
      el.removeEventListener("scroll", onScroll);
      el.removeEventListener("wheel", onWheel);
    };
  }, [hasHidden]);

  // Page keys reach the transcript from the composer, where focus normally
  // sits. Moving `scrollTop` fires the scroll the follow hook listens to, so
  // paging down onto the newest message resumes following by itself.
  const composerRef = useRef<HTMLDivElement>(null);
  const hasHiddenRef = useRef(hasHidden);
  hasHiddenRef.current = hasHidden;
  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      const el = scrollRef.current;
      const direction = pageKeyDirection(event, document.activeElement, composerRef.current, el);
      if (!el || direction === undefined) return;
      event.preventDefault();
      if (direction < 0 && el.scrollTop === 0) {
        if (hasHiddenRef.current && fromBottom.current === undefined) showEarlierRef.current();
        return;
      }
      el.scrollTop += direction * el.clientHeight;
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, []);

  useTurnNotifications(
    cells,
    busy,
    target,
    opened.state === "ready" ? opened.data.title : undefined,
  );
  useKeybind("toggle-activity", () => setActivityOpen((v) => !v));

  // Spoken replies: the speaker learns where sound comes from here, once for
  // every read-aloud button in the transcript; auto-speak reads the reply
  // that just finished.
  useSpeechSource();
  const voice = useVoicePrefs();
  const lastReply = useMemo(() => {
    for (let i = cells.length - 1; i >= 0; i -= 1) {
      const cell = cells[i]!;
      if (cell.kind === "user") return undefined;
      if (cell.kind === "assistant") return { id: replyId(cell), text: cell.text };
    }
    return undefined;
  }, [cells]);
  useAutoSpeak(voice.autoSpeak, busy, lastReply);

  // Search over the mounted transcript. The revision is what tells it to
  // re-derive: every streamed token replaces text nodes its ranges point into.
  // A turn settling counts too — token appends mutate the last cell in place,
  // so the counts alone never move while a reply streams.
  const find = useFindInTranscript(contentRef, `${visible.length}:${cells.length}:${busy}`);
  useKeybind("find-in-conversation", find.show, { inInputs: true });
  // Escape belongs to the find bar while it is open — registered after the
  // composer's own binding, and the keyboard layer runs the frontmost one.
  useKeybind("cancel-turn", find.close, { enabled: find.open, inInputs: true });

  // Follow the newest content, but only while the user is actually down there.
  const { pinned, missed, jumpToLatest } = useFollowBottom(scrollRef, contentRef, target);

  // Ways out of a conversation that failed to open. Borg being down or the
  // token being refused already get their own remedy from `DaemonState`, and
  // a new conversation would fail the same way — only retrying is left, and
  // only where `DaemonState` offers nothing to press (Borg down in a browser).
  // A new conversation is also no way out of one that is already new.
  const failure = opened.state === "error" ? opened.error : undefined;
  const daemonDown = failure?.isUnavailable === true || failure?.isUnauthenticated === true;
  const offerRetry = !daemonDown || (kind === "web" && failure?.isUnavailable === true);
  const offerNew = !daemonDown && target !== "new";
  const failureActions =
    offerRetry || offerNew ? (
      <>
        {offerNew && (
          <Pill variant="primary" onClick={() => navigate("/c/new")}>
            Start a new conversation
          </Pill>
        )}
        {offerRetry && (
          <Pill variant={offerNew ? "default" : "primary"} onClick={opened.refetch}>
            Try again
          </Pill>
        )}
      </>
    ) : undefined;

  const title =
    opened.state === "ready" && opened.data.title
      ? displayTitle(opened.data.title)
      : "Conversation";

  // Session lists look for a draft under the agent session id, but a
  // conversation opened at `/c/new` has no id until the daemon answers, so the
  // composer types under a key named for the route until then. `useConversation`
  // carries the text across as the id lands; this only follows it.
  const agentSessionId = opened.state === "ready" ? opened.data.agentSessionId : "";
  const draftKey = agentSessionId ? draftKeyFor(agentSessionId) : draftKeyFor(target ?? "latest");

  // Having this conversation on screen is what reading it means, so the mark
  // is renewed as the transcript grows and once more when the turn settles —
  // watching an answer arrive must not leave the row asking to be read.
  useEffect(() => {
    markSessionSeen(agentSessionId);
  }, [agentSessionId, cells.length, busy]);

  return (
    <PageChrome
      title={title}
      onRename={opened.state === "ready" ? rename : undefined}
      mobileBrandHeader
      headerActions={
        <button
          type="button"
          aria-label="Toggle activity panel"
          aria-pressed={activityOpen}
          onClick={() => setActivityOpen((v) => !v)}
          className={`flex h-7 w-7 cursor-pointer items-center justify-center rounded-md border-0 bg-transparent ${
            activityOpen ? "text-neon-ink" : "text-ink3 hover:text-ink"
          }`}
        >
          <AgentsIcon size={15} />
        </button>
      }
      aside={
        activityOpen ? (
          // This conversation's own id: an empty one makes the daemon pick its
          // most recent session, which a check-in or another window can own.
          <AgentPanel
            sessionId={opened.state === "ready" ? opened.data.agentSessionId : ""}
            onClose={() => setActivityOpen(false)}
          />
        ) : undefined
      }
    >
      {/* `relative` so the find bar can float over the transcript: inside the
          scroller it would scroll away with the text it is searching. */}
      <div className="relative flex min-h-0 flex-1 flex-col">
        {/* Always mounted, so a screen reader hears the work start and stop:
            a live region that appears with its text is often not announced
            at all. The visible lines below repeat it, so they are hidden from
            the reader; the counter ticking every second would be noise. */}
        <div className="sr-only">
          <div role="status" aria-label="Borg activity">
            {activity?.label ?? ""}
          </div>
          <div role="status" aria-label="Connection">
            {streamError
              ? streamOutageSustained
                ? "Still cannot reach Borg. Check that the Borg app is running, then try again."
                : "Lost the connection to Borg. Reconnecting…"
              : ""}
          </div>
        </div>
        {find.open && (
          <div data-find-skip>
            <FindBar
              query={find.query}
              onQueryChange={find.setQuery}
              count={find.count}
              position={find.position}
              onNext={find.next}
              onPrevious={find.previous}
              onClose={find.close}
              focusRequest={find.focusRequest}
              // Only when history is genuinely out of reach. Revealing it is one
              // deliberate step, not the default — that budget is why a long
              // conversation opens at all.
              onSearchAll={hiddenCount > 0 ? () => setCutIndex(0) : undefined}
            />
          </div>
        )}
        {/* Focusable so a keyboard can scroll it when nothing inside takes
            focus (a transcript of plain text); the page keys above already
            treat the transcript itself as a place they may page from. */}
        <div
          ref={scrollRef}
          tabIndex={0}
          role="region"
          aria-label="Transcript"
          className={`flex-1 overflow-y-auto pb-2 focus-visible:outline-offset-[-2px] ${isMobile ? "pt-[22px]" : "pt-[38px]"}`}
        >
          <div
            ref={contentRef}
            className={`mx-auto flex ${CONVERSATION_COL} flex-col gap-6 ${
              isMobile ? "px-[22px]" : "px-[40px]"
            }`}
          >
            <DaemonState query={opened} label="the conversation" actions={failureActions}>
              {() => (
                <>
                  {cells.length === 0 && (
                    <div className="pt-10 text-center text-[14.5px] leading-[1.62] text-pretty text-ink3">
                      Nothing here yet. Ask Borg anything.
                    </div>
                  )}
                  {hiddenCount > 0 && (
                    <button
                      type="button"
                      onClick={showEarlier}
                      className="mx-auto cursor-pointer rounded-full border border-line bg-surface2 px-3.5 py-1.5 text-[11.5px] leading-none text-ink3 hover:text-ink"
                    >
                      Show earlier messages
                    </button>
                  )}
                  {visible.map((item) =>
                    item.type === "run" ? (
                      <ToolRunCell key={item.key} tools={item.tools} live={item.live} />
                    ) : (
                      <TranscriptCell
                        key={item.index}
                        cell={item.cell}
                        respond={respond}
                        ordinal={ordinals[item.index]!}
                        editable={item.cell.kind === "user" && !busy}
                        onEdit={editAndResend}
                        usage={usage.get(item.index)}
                      />
                    ),
                  )}
                  {/* Only once the turn settles: mid-turn the tool rows narrate,
                      and a card rebuilt on every patch would thrash. */}
                  {!busy && (edits.length > 0 || uncounted > 0) && (
                    <ChangedFilesCard edits={edits} uncounted={uncounted} />
                  )}
                  {shownActivity !== undefined && (
                    <div className="px-1 text-[12px] leading-none text-ink3" aria-hidden="true">
                      {shownActivity}
                      {turnStartedAt !== undefined && (
                        <span className="text-ink3"> {formatElapsed(now - turnStartedAt)}</span>
                      )}
                    </div>
                  )}
                  {/* The live slot, so this card speaks: it reports what just
                      happened, unlike the settled failures further up. */}
                  <FailureCard error={sendError} onRetry={busy ? undefined : retryLast} live />
                </>
              )}
            </DaemonState>
          </div>
        </div>
      </div>

      <div className={`mx-auto w-full ${CONVERSATION_COL} px-[40px] empty:hidden`}>
        {/* Only once something has actually arrived unseen. Scrolling up on a
            settled transcript is reading, not falling behind, and does not
            need a way back — unless Borg is waiting on one, which is the one
            thing worth interrupting reading for. */}
        {!pinned && (missed || awaitingInput) && (
          <div className="mb-2 flex justify-center">
            <button
              type="button"
              onClick={jumpToLatest}
              className="cursor-pointer rounded-full border border-line bg-surface2 px-3.5 py-1.5 text-[11.5px] leading-none text-ink2 hover:text-ink"
            >
              {awaitingInput ? "Borg has a question" : "Jump to latest"}
            </button>
          </div>
        )}
        {/* A silent stall used to look identical to an idle agent. */}
        {streamError && (
          <div
            className="mb-2 rounded-[10px] border border-alert bg-alert-soft px-3.5 py-2 text-[11.5px] leading-[1.5] text-ink2"
            aria-hidden="true"
          >
            {/* After a while "reconnecting" stops being news and starts
                hiding the fact that nothing is coming back. */}
            {streamOutageSustained
              ? "Still cannot reach Borg. Check that the Borg app is running, then try again."
              : "Lost the connection to Borg. Reconnecting…"}
          </div>
        )}
        <ContextBar
          context={context}
          busy={busy}
          compacting={compacting}
          onCompact={() => {
            setCompacting(true);
            void compact().finally(() => setCompacting(false));
          }}
          onFork={() => {
            void fork().then((id) => {
              if (id) navigate(`/c/${id}`);
            });
          }}
        />
        <PlanPanel steps={plan} />
      </div>

      {/* `contents` keeps the wrapper out of layout; it only marks where the
          composer's keys come from. */}
      <div ref={composerRef} className="contents">
        <Composer
          busy={busy}
          ready={ready}
          awaitingInput={awaitingInput}
          draftKey={draftKey}
          conversationKey={target ?? "latest"}
          sessionId={liveSessionId}
          onSend={send}
          onSteer={steer}
          onCancel={cancel}
          onCommand={runCommand}
        />
      </div>
    </PageChrome>
  );
}
