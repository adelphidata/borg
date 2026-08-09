import { useEffect, useRef } from "react";
import { useParams } from "react-router-dom";
import { PageChrome, useIsMobile } from "@borg/ui";
import { DaemonState } from "../../components/DaemonState";
import { Composer } from "./Composer";
import { PlanPanel } from "./cells/PlanPanel";
import { TranscriptCell } from "./cells";
import { useConversation } from "./useConversation";

export function ConversationPage() {
  const { sessionId: target } = useParams();
  const isMobile = useIsMobile();
  const { opened, cells, plan, busy, sendError, streamError, send, steer, cancel, respond } =
    useConversation(target);
  const bottomRef = useRef<HTMLDivElement>(null);

  // Follow the newest cell, like any chat surface.
  useEffect(() => {
    bottomRef.current?.scrollIntoView({ block: "end" });
  }, [cells.length, busy]);

  const title = opened.state === "ready" && opened.data.title ? opened.data.title : "Conversation";
  const meta = opened.state === "ready" && opened.data.model ? opened.data.model : undefined;

  return (
    <PageChrome title={title} meta={meta} mobileBrandHeader>
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
                {cells.map((cell, i) => (
                  <TranscriptCell key={i} cell={cell} respond={respond} />
                ))}
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
        <PlanPanel steps={plan} />
      </div>

      <Composer
        busy={busy}
        draftKey={`borg:draft:${target ?? "latest"}`}
        onSend={send}
        onSteer={steer}
        onCancel={cancel}
      />
    </PageChrome>
  );
}
