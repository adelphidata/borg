import { useEffect, useMemo, useRef, useState } from "react";
import { useNavigate, useParams } from "react-router-dom";
import { AgentsIcon, PageChrome, useIsMobile } from "@borg/ui";
import { AgentPanel } from "../agents/AgentPanel";
import { DaemonState } from "../../components/DaemonState";
import { useKeybind } from "../../lib/keybinds";
import { Composer } from "./Composer";
import { ContextBar } from "./ContextBar";
import { draftKeyFor } from "./draftStore";
import { ChangedFilesCard } from "./cells/ChangedFilesCard";
import { PlanPanel } from "./cells/PlanPanel";
import { ToolRunCell } from "./cells/ToolRunCell";
import { TranscriptCell } from "./cells";
import { groupCells } from "./cells/toolRuns";
import { collectEdits, truncatedPatchCount, turnEditCells } from "./turnEdits";
import { useConversation } from "./useConversation";
import { useTurnNotifications } from "./useTurnNotifications";

export function ConversationPage() {
  const { sessionId: target } = useParams();
  const isMobile = useIsMobile();
  const navigate = useNavigate();
  const [compacting, setCompacting] = useState(false);
  const [activityOpen, setActivityOpen] = useState(false);
  const {
    opened,
    cells,
    plan,
    busy,
    sendError,
    streamError,
    send,
    steer,
    cancel,
    respond,
    editAndResend,
    compact,
    fork,
    context,
  } = useConversation(target);
  const bottomRef = useRef<HTMLDivElement>(null);

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

  useTurnNotifications(cells, busy, target);
  useKeybind("toggle-activity", () => setActivityOpen((v) => !v));

  // Follow the newest cell, like any chat surface.
  useEffect(() => {
    bottomRef.current?.scrollIntoView({ block: "end" });
  }, [cells.length, busy]);

  const title = opened.state === "ready" && opened.data.title ? opened.data.title : "Conversation";
  const meta = opened.state === "ready" && opened.data.model ? opened.data.model : undefined;

  // Session lists look for a draft under the agent session id, but a
  // conversation opened at `/c/new` has no id until the daemon answers, so the
  // composer types under a key named for the route until then. `useConversation`
  // carries the text across as the id lands; this only follows it.
  const agentSessionId = opened.state === "ready" ? opened.data.agentSessionId : "";
  const draftKey = agentSessionId ? draftKeyFor(agentSessionId) : draftKeyFor(target ?? "latest");

  return (
    <PageChrome
      title={title}
      meta={meta}
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
          // Empty resolves to the daemon's active session, which is this one.
          <AgentPanel sessionId="" onClose={() => setActivityOpen(false)} />
        ) : undefined
      }
    >
      <div className={`flex-1 overflow-y-auto pb-2 ${isMobile ? "pt-[22px]" : "pt-[38px]"}`}>
        <div
          className={`mx-auto flex max-w-[660px] flex-col gap-6 ${
            isMobile ? "px-[22px]" : "px-[30px]"
          }`}
        >
          <DaemonState query={opened} label="the conversation">
            {() => (
              <>
                {cells.length === 0 && (
                  <div className="pt-10 text-center text-[14.5px] leading-[1.62] text-pretty text-ink3">
                    Nothing here yet. Ask Borg anything.
                  </div>
                )}
                {items.map((item) =>
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
                    />
                  ),
                )}
                {/* Only once the turn settles: mid-turn the tool rows narrate,
                    and a card rebuilt on every patch would thrash. */}
                {!busy && (edits.length > 0 || uncounted > 0) && (
                  <ChangedFilesCard edits={edits} uncounted={uncounted} />
                )}
                {busy && (
                  <div className="px-1 text-[12px] leading-none text-ink3">Borg is thinking…</div>
                )}
                {sendError && (
                  <div className="rounded-xl border border-alert bg-alert-soft px-4 py-3 text-[12.5px] leading-[1.5] text-ink2">
                    {sendError.message}
                  </div>
                )}
                <div ref={bottomRef} />
              </>
            )}
          </DaemonState>
        </div>
      </div>

      <div
        className={`mx-auto w-full max-w-[660px] ${isMobile ? "px-[40px]" : "px-[30px]"} empty:hidden`}
      >
        {/* A silent stall used to look identical to an idle agent. */}
        {streamError && (
          <div className="mb-2 rounded-[10px] border border-alert bg-alert-soft px-3.5 py-2 text-[11.5px] leading-[1.5] text-ink2">
            Lost the connection to Borg. Reconnecting…
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

      <Composer
        busy={busy}
        draftKey={draftKey}
        conversationKey={target ?? "latest"}
        onSend={send}
        onSteer={steer}
        onCancel={cancel}
      />
    </PageChrome>
  );
}
