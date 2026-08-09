import { useRef, useState } from "react";
import { useLocation, useNavigate } from "react-router-dom";
import {
  PageChrome,
  PlusIcon,
  SearchIcon,
  SETTINGS_SCROLL_DESKTOP_PADDING,
  scrollCol,
  useIsMobile,
} from "@borg/ui";
import { DaemonState } from "../../components/DaemonState";
import { useKeybind } from "../../lib/keybinds";
import { SessionRowMenu } from "./SessionRowMenu";
import { useSessions, type SessionRow } from "./useSessions";

function Row({
  session,
  onOpen,
  onRename,
  onArchive,
  onExport,
}: {
  session: SessionRow;
  onOpen: () => void;
  onRename: (title: string) => void;
  onArchive: () => void;
  onExport: (format: string) => void;
}) {
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(session.title);

  const commit = () => {
    const title = draft.trim();
    setEditing(false);
    if (title !== "" && title !== session.title) onRename(title);
    else setDraft(session.title);
  };

  if (editing) {
    return (
      <div className="flex items-center gap-2 rounded-[9px] px-3 py-2.5">
        <input
          autoFocus
          className="min-w-0 flex-1 rounded-[7px] border border-line bg-surface2 px-2.5 py-1.5 text-[13px] text-ink outline-none select-text"
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          onBlur={commit}
          onKeyDown={(e) => {
            if (e.key === "Enter") commit();
            if (e.key === "Escape") {
              setDraft(session.title);
              setEditing(false);
            }
          }}
        />
      </div>
    );
  }

  return (
    <div
      role="button"
      tabIndex={0}
      onClick={onOpen}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          onOpen();
        }
      }}
      className="group flex cursor-pointer items-center gap-2 rounded-[9px] px-3 py-2.5 hover:bg-surface3"
    >
      <div className="min-w-0 flex-1">
        <div className="truncate text-[13.5px] leading-[1.35] text-ink">{session.title}</div>
        <div className="mt-[3px] text-[11px] leading-none text-ink3">
          {session.messageCount} message{session.messageCount === 1 ? "" : "s"}
        </div>
      </div>
      <SessionRowMenu
        onRename={() => {
          setDraft(session.title);
          setEditing(true);
        }}
        onArchive={onArchive}
        onExport={onExport}
      />
    </div>
  );
}

/**
 * Every conversation, searchable.
 *
 * The sidebar shows a handful of recents; this is where the rest live, and on
 * mobile it is the only way to reach them at all — there is no sidebar under
 * 700px.
 */
export function SessionsPage() {
  const navigate = useNavigate();
  const location = useLocation();
  const sessions = useSessions([location.key]);
  const searchBox = useRef<HTMLInputElement>(null);
  const isMobile = useIsMobile();

  useKeybind("search-sessions", () => searchBox.current?.focus());

  return (
    <PageChrome title="Conversations" mobileBrandHeader>
      <div className={scrollCol(isMobile, SETTINGS_SCROLL_DESKTOP_PADDING)}>
        <div className="mb-4 flex items-center gap-2">
          <div className="flex min-w-0 flex-1 items-center gap-2 rounded-[10px] border border-line bg-surface2 px-3 py-2">
            <SearchIcon size={14} />
            <input
              ref={searchBox}
              className="min-w-0 flex-1 border-0 bg-transparent text-[13px] text-ink outline-none select-text placeholder:text-ink3"
              placeholder="Search conversations"
              value={sessions.search}
              onChange={(e) => sessions.setSearch(e.target.value)}
            />
          </div>
          <button
            type="button"
            aria-label="New conversation"
            onClick={() => navigate("/c/new")}
            className="flex h-[38px] w-[38px] flex-none cursor-pointer items-center justify-center rounded-[10px] border-0 bg-neon shadow-(--glow)"
          >
            <PlusIcon size={16} stroke="#0C100D" strokeWidth={2.2} />
          </button>
        </div>

        {sessions.error && (
          <div role="alert" className="mb-3 text-[11.5px] leading-[1.4] text-alert">
            {sessions.error}
          </div>
        )}

        <DaemonState query={sessions.query} label="your conversations">
          {() => (
            <>
              {sessions.groups.length === 0 && (
                <div className="pt-10 text-center text-[13.5px] leading-[1.6] text-ink3">
                  {sessions.search.trim() === ""
                    ? "No conversations yet."
                    : `Nothing matches “${sessions.search.trim()}”.`}
                </div>
              )}
              {sessions.groups.map((group) => (
                <div key={group.label} className="mb-5">
                  <div className="mb-1 px-3 text-[9.5px] leading-none font-medium tracking-[0.16em] text-ink3 uppercase">
                    {group.label}
                  </div>
                  {group.sessions.map((session) => (
                    <Row
                      key={session.id}
                      session={session}
                      onOpen={() => navigate(`/c/${session.id}`)}
                      onRename={(title) => void sessions.rename(session.id, title)}
                      onArchive={() => void sessions.archive(session.id)}
                      onExport={(format) => void sessions.exportSession(session.id, format)}
                    />
                  ))}
                </div>
              ))}
            </>
          )}
        </DaemonState>
      </div>
    </PageChrome>
  );
}
