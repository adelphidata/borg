import { NavLink } from "react-router-dom";
import { Brackets } from "../components/Brackets";
import { LogoIcon } from "../components/icons";
import { StatusDot } from "../components/StatusDot";
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
   * Why the last archive failed, if it did. Rendered under the list — a × that
   * silently does nothing reads as a broken button.
   */
  archiveError?: string;
  /** Starts a fresh conversation. */
  onNewConversation: () => void;
  /** Opens the full, searchable conversation list. */
  onSeeAllConversations: () => void;
  user: NavUser;
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
 * One recent conversation. The archive control only materializes on hover or
 * keyboard focus: eight rows each carrying a permanent × would read as a list
 * of delete buttons rather than a list of conversations.
 */
function RecentRow({
  item,
  onOpen,
  onArchive,
}: {
  item: RecentItem;
  onOpen: () => void;
  onArchive: () => void;
}) {
  return (
    <div
      className="group flex cursor-pointer items-center gap-1 rounded-md px-3 py-[7px] text-[12.5px] leading-[1.35] text-ink3 hover:bg-surface3 hover:text-ink2"
      onClick={onOpen}
    >
      {item.activity && (
        <StatusDot
          tone={item.activity}
          label={item.activity === "working" ? "Working" : "Unsent draft"}
        />
      )}
      <span className="min-w-0 flex-1 overflow-hidden text-ellipsis whitespace-nowrap">
        {item.title}
      </span>
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
        className="flex-none rounded px-1 text-[13px] leading-none text-ink3 opacity-0 transition-opacity duration-150 group-hover:opacity-100 hover:text-ink focus-visible:opacity-100"
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
  archiveError,
  onNewConversation,
  onSeeAllConversations,
  user,
}: SidebarProps) {
  return (
    <aside className="flex w-[236px] flex-none flex-col border-r border-line2 bg-surface2 px-[14px] pt-[18px] pb-4">
      <div className="flex items-center gap-[11px] px-2 pt-0.5 pb-5">
        <LogoIcon size={20} />
        <span className="text-[13px] leading-none font-semibold tracking-[0.2em] text-ink">
          BORG
        </span>
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
