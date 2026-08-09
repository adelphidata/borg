import { useState } from "react";
import { Brackets, PageChrome, Pill, SendIcon, useIsMobile } from "@borg/ui";

const KICKER =
  "text-[10px] leading-none font-medium tracking-[0.16em] uppercase text-ink3 mb-[13px]";

/** Assistant reply in the bracketed-card style. */
function ReplyCard({ kicker, children }: { kicker: string; children: React.ReactNode }) {
  return (
    <div className="relative rounded-xl border border-line bg-surface2 p-5 pb-[18px] shadow-(--plateGlow)">
      <Brackets card />
      <div className={KICKER}>{kicker}</div>
      <div className="text-[14.5px] leading-[1.6] text-pretty text-ink [&_em]:font-medium [&_em]:text-neon-ink [&_em]:not-italic">
        {children}
      </div>
    </div>
  );
}

function Composer() {
  const isMobile = useIsMobile();
  const [draft, setDraft] = useState("");
  return (
    <div className={`flex-none ${isMobile ? "px-[18px] pt-3.5 pb-3" : "px-[30px] pt-4 pb-[26px]"}`}>
      <div className={`mx-auto max-w-[660px] ${isMobile ? "px-[22px]" : ""}`}>
        <div
          className={`flex items-center gap-3 border border-line bg-surface2 py-2 pr-2 pl-[19px] ${
            isMobile ? "rounded-full" : "rounded-[13px]"
          }`}
        >
          <input
            className="min-w-0 flex-1 border-0 bg-transparent py-1.5 text-[14.5px] leading-[1.3] text-ink outline-none placeholder:text-ink3"
            placeholder="Ask Borg anything"
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
          />
          {/* No daemon yet — "send" just clears the draft until borgd is wired in. */}
          <button
            type="button"
            aria-label="Send"
            className="flex h-[29px] w-[29px] flex-none cursor-pointer items-center justify-center rounded-[9px] border-0 bg-neon shadow-(--glow)"
            onClick={() => setDraft("")}
          >
            <SendIcon size={14} />
          </button>
        </div>
      </div>
    </div>
  );
}

export function ConversationPage() {
  const isMobile = useIsMobile();
  return (
    <PageChrome title="Release check" meta="started 9:14 this morning" mobileBrandHeader>
      <div className={`flex-1 overflow-y-auto pb-2 ${isMobile ? "pt-[22px]" : "pt-[38px]"}`}>
        <div
          className={`mx-auto flex max-w-[660px] flex-col gap-6 ${
            isMobile ? "px-[22px]" : "px-[30px]"
          }`}
        >
          <div className="flex items-center gap-3">
            <span className="h-px flex-1 bg-line2" />
            <span className="text-[9.5px] leading-none font-medium tracking-[0.16em] uppercase text-ink3">
              This morning
            </span>
            <span className="h-px flex-1 bg-line2" />
          </div>

          <div className="text-[14.5px] leading-[1.62] text-pretty text-ink2">
            Nothing needed you overnight. Two schedules ran clean and the nightly sweep reclaimed
            240&nbsp;MB.
          </div>

          <div className="flex justify-end">
            <div className="max-w-[78%] rounded-[13px] rounded-br-[4px] border border-line2 bg-surface3 px-4 py-[11px] text-[13.5px] leading-[1.45] text-ink">
              Anything blocking the release?
            </div>
          </div>

          <ReplyCard kicker="One blocker">
            The daemon certificate still only names <em>localhost</em>, so remote pairing fails. Rob
            owns it and has it down for Thursday. Everything else in the 0.4 milestone is merged and
            green, so the ship date holds if that lands.
          </ReplyCard>

          <div className="rounded-xl border border-line2 px-[18px] py-4">
            <div className={KICKER}>Draft for the team</div>
            <div className="text-[13.5px] leading-[1.6] text-pretty text-ink2">
              0.4 is one issue away. Everything else is merged and green.
            </div>
            <div className="mt-3.5 flex flex-wrap gap-2">
              <Pill variant="primary" square>
                Send to #release
              </Pill>
              <Pill square>Edit</Pill>
            </div>
          </div>
        </div>
      </div>
      <Composer />
    </PageChrome>
  );
}
