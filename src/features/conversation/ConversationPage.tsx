import { useState } from "react";
import { Brackets } from "../../components/Brackets";
import { PageChrome } from "../../components/PageChrome";
import { Pill } from "../../components/Pill";
import { SendIcon } from "../../components/icons";

/** Assistant reply in the bracketed-card style. */
function ReplyCard({ kicker, children }: { kicker: string; children: React.ReactNode }) {
  return (
    <div className="reply-card">
      <Brackets card />
      <div className="card-kicker">{kicker}</div>
      <div className="card-body">{children}</div>
    </div>
  );
}

function Composer() {
  const [draft, setDraft] = useState("");
  return (
    <div className="composer-wrap">
      <div className="composer-col">
        <div className="composer">
          <input
            placeholder="Ask Borg anything"
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
          />
          {/* No daemon yet — "send" just clears the draft until borgd is wired in. */}
          <button type="button" className="send" onClick={() => setDraft("")}>
            <SendIcon size={14} />
          </button>
        </div>
      </div>
    </div>
  );
}

export function ConversationPage() {
  return (
    <PageChrome title="Release check" meta="started 9:14 this morning" mobileBrandHeader>
      <div className="chat-scroll">
        <div className="chat-col">
          <div className="day-divider">
            <span className="rule" />
            <span className="label">This morning</span>
            <span className="rule" />
          </div>

          <div className="ambient">
            Nothing needed you overnight. Two schedules ran clean and the nightly sweep reclaimed
            240&nbsp;MB.
          </div>

          <div className="user-bubble-row">
            <div className="user-bubble">Anything blocking the release?</div>
          </div>

          <ReplyCard kicker="One blocker">
            The daemon certificate still only names <em>localhost</em>, so remote pairing fails. Rob
            owns it and has it down for Thursday. Everything else in the 0.4 milestone is merged and
            green, so the ship date holds if that lands.
          </ReplyCard>

          <div className="quiet-card">
            <div className="card-kicker">Draft for the team</div>
            <div className="card-body">
              0.4 is one issue away. Everything else is merged and green.
            </div>
            <div className="card-actions">
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
