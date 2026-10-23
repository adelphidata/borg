import { useRef, useState } from "react";
import { useLocation, useNavigate } from "react-router-dom";
import {
  ChevronDownIcon,
  ChevronIcon,
  Menu,
  PageChrome,
  SECTION_LABEL_TEXT,
  PlusIcon,
  SearchIcon,
  SETTINGS_SCROLL_DESKTOP_PADDING,
  StatusDot,
  Truncated,
  isSubmitEnter,
  scrollCol,
  useIsMobile,
} from "@borg/ui";
import { DaemonState } from "../../components/DaemonState";
import { useKeybind } from "../../lib/keybinds";
import { SessionRowMenu, sessionRowActions, useMenuEscape } from "./SessionRowMenu";
import { rowSubtitle, useSessions, type SessionRow } from "./useSessions";
import { useTick } from "../../lib/useTick";
import type { SessionActivity } from "./activityStore";

/** What each dot means to a reader who cannot see it. */
const DOT_LABELS: Record<SessionActivity, string | undefined> = {
  working: undefined,
  unread: "Unread reply",
  draft: "Unsent draft",
};

function Row({
  session,
  now,
  onOpen,
  onRename,
  onArchive,
  onRestore,
  onExport,
  onPin,
}: {
  session: SessionRow;
  /** The shared clock, so a running row counts up without a timer of its own. */
  now: number;
  onOpen: () => void;
  onRename: (title: string) => void;
  onArchive: () => void;
  onRestore: () => void;
  onExport: (format: string) => void;
  onPin: (pinned: boolean) => void;
}) {
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(session.title);
  /** Where a right-click opened the menu; absent while it is closed. */
  const [menuAt, setMenuAt] = useState<{ x: number; y: number }>();

  const rename = () => {
    setDraft(session.title);
    setEditing(true);
  };
  const actions = sessionRowActions({
    onRename: rename,
    onArchive,
    onRestore,
    onExport,
    archived: session.archived,
    pinned: session.pinned,
    onPin,
  });
  useMenuEscape(menuAt !== undefined, () => setMenuAt(undefined));

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
          className="min-w-0 flex-1 rounded-[7px] border border-field bg-surface2 px-2.5 py-1.5 text-[13px] text-ink outline-none select-text focus:border-focus"
          aria-label="Conversation title"
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          onBlur={commit}
          onKeyDown={(e) => {
            if (isSubmitEnter(e)) commit();
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
      // The gesture most people reach for on a list row. Same actions as the
      // kebab — a second door, not a second set of choices.
      onContextMenu={(e) => {
        e.preventDefault();
        setMenuAt({ x: e.clientX, y: e.clientY });
      }}
      className="group flex items-center rounded-[9px] pr-3 hover:bg-surface3"
    >
      {/* The row's open action is its own button beside the kebab rather than
          the whole row being one: a control inside a control is announced as
          one and leaves the inner one unreachable to assistive tech. */}
      <button
        type="button"
        onClick={onOpen}
        className="flex min-w-0 flex-1 cursor-pointer items-center gap-2 rounded-[9px] border-0 bg-transparent py-2.5 pr-2 pl-3 text-left"
      >
        {/* "Working" is already spelled out in the line below, so labelling the
            dot too would make a screen reader say it twice. The other two are
            not written anywhere, so those keep their labels. */}
        {session.activity && (
          <StatusDot tone={session.activity} label={DOT_LABELS[session.activity]} />
        )}
        <span className="block min-w-0 flex-1">
          {/* Archived rows recede by colour, not opacity: opacity would take
              the grey subtitle below the 4.5:1 text contrast floor. */}
          <Truncated
            className={`block text-[13.5px] leading-[1.35] ${session.archived ? "text-ink2" : "text-ink"}`}
          >
            {session.title}
          </Truncated>
          <Truncated className="mt-[3px] block text-[11px] leading-[1.35] text-ink3">
            {rowSubtitle(session, now)}
          </Truncated>
        </span>
      </button>
      <SessionRowMenu
        onRename={rename}
        onArchive={onArchive}
        onRestore={onRestore}
        onExport={onExport}
        archived={session.archived}
        pinned={session.pinned}
        onPin={onPin}
      />
      <Menu
        open={menuAt !== undefined}
        onClose={() => setMenuAt(undefined)}
        label="Conversation actions"
        actions={actions}
        at={menuAt}
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
  // One clock for the page. It runs only while a row is counting up, so a
  // settled list costs no renders at all.
  const anyWorking = sessions.groups.some((g) => g.sessions.some((s) => s.activity === "working"));
  const now = useTick(1_000, anyWorking);

  useKeybind("search-sessions", () => searchBox.current?.focus());

  const searching = sessions.search.trim() !== "";
  // A search that turned up archived matches opens the section itself — but the
  // header is still a control, and a click on an auto-opened section has to
  // close it. Without the override the disclosure would sit open, un-closable,
  // for as long as the search matched something archived.
  const [collapsed, setCollapsed] = useState(false);
  const showArchived =
    !collapsed && (sessions.showArchived || (searching && sessions.archived.length > 0));
  const toggleArchived = () => {
    setCollapsed(showArchived);
    sessions.setShowArchived(!showArchived);
  };

  return (
    <PageChrome title="Conversations" mobileBrandHeader>
      <div className={scrollCol(isMobile, SETTINGS_SCROLL_DESKTOP_PADDING)}>
        <div className="mb-4 flex items-center gap-2">
          <div className="flex min-w-0 flex-1 items-center gap-2 rounded-[10px] border border-field bg-surface2 px-3 py-2 focus-within:border-focus">
            <SearchIcon size={14} />
            <input
              ref={searchBox}
              className="min-w-0 flex-1 border-0 bg-transparent text-[13px] text-ink outline-none select-text placeholder:text-ink3"
              placeholder="Search conversations"
              aria-label="Search conversations"
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
                  <div className={`${SECTION_LABEL_TEXT} mb-1 px-3`}>{group.label}</div>
                  {group.sessions.map((session) => (
                    <Row
                      key={session.id}
                      session={session}
                      now={now}
                      onOpen={() => navigate(`/c/${session.id}`)}
                      onRename={(title) => void sessions.rename(session.id, title)}
                      onArchive={() => void sessions.archive(session.id, session.title)}
                      onRestore={() => void sessions.restore(session.id)}
                      onExport={(format) => void sessions.exportSession(session.id, format)}
                      onPin={(pinned) => void sessions.pin(session.id, pinned)}
                    />
                  ))}
                </div>
              ))}

              {/* Archiving is a soft hide, so there has to be a way back. One
                  quiet line rather than a nav entry: putting conversations away
                  is the common act, digging them out is not. A search reveals
                  its matches on its own — someone hunting for a conversation
                  they archived types its name, and should not have to know a
                  disclosure exists. */}
              <div className="mb-6">
                <button
                  type="button"
                  aria-expanded={showArchived}
                  onClick={toggleArchived}
                  className={`${SECTION_LABEL_TEXT} mb-1 flex cursor-pointer items-center gap-1.5 border-0 bg-transparent px-3 hover:text-ink2`}
                >
                  {showArchived ? <ChevronDownIcon size={12} /> : <ChevronIcon size={12} />}
                  {searching && sessions.archived.length > 0
                    ? `Archived · matches “${sessions.search.trim()}”`
                    : "Archived"}
                </button>
                {showArchived &&
                  (sessions.archived.length === 0 ? (
                    <div className="px-3 py-2 text-[11.5px] leading-[1.5] text-ink3">
                      {searching ? "No archived conversation matches." : "Nothing archived."}
                    </div>
                  ) : (
                    sessions.archived.map((session) => (
                      <Row
                        key={session.id}
                        session={session}
                        now={now}
                        onOpen={() => {
                          // Resuming a conversation un-archives it daemon-side
                          // regardless, so do it here where the row can be seen
                          // to move rather than letting it happen behind the
                          // user's back on the first message.
                          void sessions.restore(session.id);
                          navigate(`/c/${session.id}`);
                        }}
                        onRename={(title) => void sessions.rename(session.id, title)}
                        onArchive={() => void sessions.archive(session.id, session.title)}
                        onRestore={() => void sessions.restore(session.id)}
                        onExport={(format) => void sessions.exportSession(session.id, format)}
                        onPin={(pinned) => void sessions.pin(session.id, pinned)}
                      />
                    ))
                  ))}
              </div>
            </>
          )}
        </DaemonState>
      </div>
    </PageChrome>
  );
}
