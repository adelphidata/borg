import { NavLink } from "react-router-dom";
import { Brackets } from "../components/Brackets";
import { ChevronIcon, LogoIcon } from "../components/icons";
import { StatusDot } from "../components/StatusDot";
import { Truncated } from "../components/Truncated";
import type { AppRoute, NavUser, RecentItem } from "../types";

export interface SidebarProps {
  routes: AppRoute[];
  /** Recent conversations listed under the nav sections. */
  recents: RecentItem[];
  /** Opens one recent conversation. */
  onOpenRecent: (id: string) => void;
  /** Archives one recent conversation, removing it from the list. */
  onArchiveRecent: (id: string) => void;
  /**
   * Right-click on a recent row, with the pointer position. The app renders the
   * menu itself; omit and right-click falls through to the browser's own.
   */
  onRecentContextMenu?: (id: string, at: { x: number; y: number }) => void;
  /**
   * Why the last archive failed, if it did. Rendered under the list — a × that
   * silently does nothing reads as a broken button.
   */
  archiveError?: string;
  /** Starts a fresh conversation. */
  onNewConversation: () => void;
  /** Opens the full, searchable conversation list. */
  onSeeAllConversations: () => void;
  user: NavUser;
  /**
   * Narrows the sidebar to its rail. The nav is chrome, and on a small window
   * it is a third of the screen spent on something the user is not reading.
   */
  collapsed?: boolean;
  /** Toggles `collapsed`. Omit and the sidebar has no collapse control. */
  onToggleCollapsed?: () => void;
  /**
   * How many conversations have replied unseen. Read only while collapsed: the
   * expanded list already marks its own rows, and a second count beside them
   * would be the same news twice.
   */
  unreadCount?: number;
}

function NavItem({ route }: { route: AppRoute }) {
  const Icon = route.icon;
  return (
    // `end` only on the root route so parent items stay lit on detail pages (matches TabBar).
    <NavLink
      to={route.path}
      end={route.path === "/"}
      className={({ isActive }) =>
        `relative flex cursor-pointer items-center gap-[11px] rounded-[7px] px-[11px] py-[9px] text-[13.5px] leading-none font-medium tracking-[-0.01em] transition-[background,color] duration-150 ${
          isActive
            ? "bg-nav-on-bg text-nav-on-ink"
            : "bg-transparent text-ink2 hover:bg-surface3 hover:text-ink"
        }`
      }
    >
      {({ isActive }) => (
        <>
          <Icon size={16} />
          {route.label}
          {route.hasAlert && (
            <>
              <span className="flex-1" />
              <span className="h-1.5 w-1.5 flex-none rounded-full bg-alert" />
            </>
          )}
          {isActive && <Brackets />}
        </>
      )}
    </NavLink>
  );
}

function NavSection({ routes, section }: { routes: AppRoute[]; section: AppRoute["section"] }) {
  return (
    <div className="flex flex-col gap-[3px]">
      {routes
        .filter((r) => r.section === section && r.nav?.sidebar !== false)
        .map((r) => (
          <NavItem key={r.id} route={r} />
        ))}
    </div>
  );
}

/** 1px divider between sidebar sections. */
function Rule({ className = "mb-4" }: { className?: string }) {
  return <div className={`mx-1.5 h-px bg-line2 ${className}`} />;
}

/**
 * What each dot means to a reader who cannot see it. A sidebar row carries the
 * title and nothing else, so unlike the sessions page there is no text here for
 * a label to repeat.
 */
const DOT_LABELS: Record<NonNullable<RecentItem["activity"]>, string> = {
  working: "Working",
  unread: "Unread reply",
  draft: "Unsent draft",
};

/**
 * One recent conversation. The archive control only materializes on hover or
 * keyboard focus: eight rows each carrying a permanent × would read as a list
 * of delete buttons rather than a list of conversations.
 */
function RecentRow({
  item,
  onOpen,
  onArchive,
  onContextMenu,
}: {
  item: RecentItem;
  onOpen: () => void;
  onArchive: () => void;
  onContextMenu?: (at: { x: number; y: number }) => void;
}) {
  return (
    <div
      className="group flex cursor-pointer items-center gap-1 rounded-md px-3 py-[7px] text-[12.5px] leading-[1.35] text-ink3 hover:bg-surface3 hover:text-ink2"
      onClick={onOpen}
      // Right-click reaches the same action the hover × does. The menu itself
      // belongs to the app, which owns both the archive call and the stacked
      // Escape handling this package cannot see.
      onContextMenu={
        onContextMenu &&
        ((e) => {
          e.preventDefault();
          onContextMenu({ x: e.clientX, y: e.clientY });
        })
      }
    >
      {item.activity && <StatusDot tone={item.activity} label={DOT_LABELS[item.activity]} />}
      <Truncated className="min-w-0 flex-1">{item.title}</Truncated>
      <button
        type="button"
        aria-label={`Archive ${item.title}`}
        title="Archive conversation"
        // Without this the row's own onClick would open the conversation the
        // user just asked to close.
        onClick={(e) => {
          e.stopPropagation();
          onArchive();
        }}
        className="flex-none rounded px-1 text-[13px] leading-none text-ink3 hover-reveal hover:text-ink"
      >
        ×
      </button>
    </div>
  );
}

export function Sidebar({
  routes,
  recents,
  onOpenRecent,
  onArchiveRecent,
  onRecentContextMenu,
  archiveError,
  onNewConversation,
  onSeeAllConversations,
  user,
  collapsed = false,
  onToggleCollapsed,
  unreadCount = 0,
}: SidebarProps) {
  // A collapsed rail hides the rows that carry the unread dots, so a reply that
  // lands while it is narrow arrives in silence. The count rides the one control
  // still on screen — the way back — so opening the sidebar is both the answer
  // to "what did I miss" and the thing the badge is pointing at.
  const badge = collapsed && unreadCount > 0;
  // The way back has to live on the rail itself. A collapsed sidebar whose only
  // reopen is a shortcut is a sidebar the user has lost.
  const toggle = onToggleCollapsed && (
    <button
      type="button"
      aria-label={
        collapsed
          ? badge
            ? `Expand sidebar, ${unreadCount} unread`
            : "Expand sidebar"
          : "Collapse sidebar"
      }
      aria-expanded={!collapsed}
      onClick={onToggleCollapsed}
      className="relative flex-none cursor-pointer rounded border-0 bg-transparent p-1 text-ink3 hover:text-ink"
    >
      <ChevronIcon size={14} className={collapsed ? "" : "rotate-180"} />
      {badge && (
        // A number, not a dot: "there are replies" is what a dot says, and the
        // rail already has no way to tell the user how many are waiting.
        <span
          aria-hidden
          className="absolute -top-1 -right-1 flex h-[15px] min-w-[15px] items-center justify-center rounded-full bg-alert px-1 text-[9.5px] leading-none font-semibold text-surface"
        >
          {unreadCount > 9 ? "9+" : unreadCount}
        </span>
      )}
    </button>
  );

  if (collapsed) {
    return (
      <aside className="flex w-[52px] flex-none flex-col items-center border-r border-line2 bg-surface2 px-2 pt-[18px] pb-4">
        <LogoIcon size={20} />
        <div className="pt-4">{toggle}</div>
      </aside>
    );
  }

  return (
    <aside className="flex w-[236px] flex-none flex-col border-r border-line2 bg-surface2 px-[14px] pt-[18px] pb-4">
      <div className="flex items-center gap-[11px] px-2 pt-0.5 pb-5">
        <LogoIcon size={20} />
        <span className="flex-1 text-[13px] leading-none font-semibold tracking-[0.2em] text-ink">
          BORG
        </span>
        {toggle}
      </div>

      <Rule />
      <NavSection routes={routes} section="primary" />
      <Rule className="my-4" />
      <NavSection routes={routes} section="secondary" />
      <Rule className="mt-4 mb-3.5" />

      <div className="flex items-center px-2 pb-2">
        <span className="flex-1 text-[9.5px] leading-none font-medium tracking-[0.16em] uppercase text-ink3">
          Recent
        </span>
        <span
          className="cursor-pointer rounded px-1.5 py-0.5 text-[11px] leading-none text-ink3 hover:bg-surface3 hover:text-ink"
          onClick={onNewConversation}
        >
          + New
        </span>
      </div>
      <div className="flex flex-col gap-px">
        {recents.map((r) => (
          <RecentRow
            key={r.id}
            item={r}
            onOpen={() => onOpenRecent(r.id)}
            onArchive={() => onArchiveRecent(r.id)}
            onContextMenu={onRecentContextMenu && ((at) => onRecentContextMenu(r.id, at))}
          />
        ))}
      </div>
      {archiveError && (
        <div role="alert" className="px-3 pt-1.5 text-[11px] leading-[1.35] text-alert">
          Could not archive: {archiveError}
        </div>
      )}
      {/* The list is capped; without this the older conversations are unreachable. */}
      <button
        type="button"
        onClick={onSeeAllConversations}
        className="mt-1 cursor-pointer rounded-md border-0 bg-transparent px-3 py-[7px] text-left text-[12px] leading-none text-ink3 hover:bg-surface3 hover:text-ink2"
      >
        Show all…
      </button>

      <div className="flex-1" />

      <div className="flex items-center gap-[11px] border-t border-line2 px-2 pt-3">
        <span className="flex h-[26px] w-[26px] items-center justify-center rounded-full border border-line bg-surface3 text-[10.5px] leading-none font-semibold text-ink2">
          {user.initials}
        </span>
        <div className="min-w-0 flex-1">
          <div className="text-[12.5px] leading-[1.2] font-medium text-ink">{user.name}</div>
        </div>
      </div>
    </aside>
  );
}
