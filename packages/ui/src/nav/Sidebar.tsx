import { NavLink } from "react-router-dom";
import { Brackets } from "../components/Brackets";
import { ChevronIcon, LogoIcon, MoonIcon, SunIcon, UpdateIcon } from "../components/icons";
import { StatusDot } from "../components/StatusDot";
import { Truncated } from "../components/Truncated";
import type { AppRoute, NavUser, RecentItem, SidebarUpdate } from "../types";

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
  /** What the agent is called, shown beside the logo. Defaults to "Borg". */
  agentName?: string;
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
  /** The theme on screen; with `onToggleTheme`, shows a switch beside the user. */
  theme?: "light" | "dark";
  /** Flips between light and dark. Omit and the footer has no theme switch. */
  onToggleTheme?: () => void;
  /**
   * A newer build on its way in. Rendered at the foot of the sidebar, on the
   * rail too: an update that only shows while the nav is wide is an update
   * the user never hears about.
   */
  update?: SidebarUpdate;
  /** Restarts into the downloaded build. Called only once `update` is ready. */
  onInstallUpdate?: () => void;
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

/** What the update control says, in words, for the label and the tooltip. */
function describeUpdate(update: SidebarUpdate): { title: string; detail: string } {
  const build = update.version ? `Borg ${update.version}` : "a new version of Borg";
  if (update.state === "ready") {
    return { title: "Restart to update", detail: `${build} is ready` };
  }
  const percent = update.percent === undefined ? "" : ` ${Math.round(update.percent)}%`;
  return { title: "Update available", detail: `Downloading ${build}…${percent}` };
}

/**
 * The one thing an update is allowed to do to the user.
 *
 * No modal, no forced restart, no progress bar competing with the transcript.
 * The download happens quietly, and this sits in the corner of the chrome
 * until it is convenient. While the build is still downloading it is a status,
 * not a control; once it is on disk it becomes the restart.
 */
function UpdateItem({
  update,
  collapsed,
  onInstall,
}: {
  update: SidebarUpdate;
  collapsed: boolean;
  onInstall?: () => void;
}) {
  const { title, detail } = describeUpdate(update);
  const label = `${title}: ${detail}`;
  const ready = update.state === "ready";

  if (collapsed) {
    const rail =
      "relative flex h-[30px] w-[30px] flex-none items-center justify-center rounded-md border";
    return ready ? (
      <button
        type="button"
        aria-label={label}
        title={label}
        onClick={onInstall}
        className={`${rail} cursor-pointer border-neon bg-transparent text-neon-ink shadow-(--plateGlow) hover:bg-nav-on-bg`}
      >
        <UpdateIcon size={16} />
      </button>
    ) : (
      <div
        role="status"
        aria-label={label}
        title={label}
        className={`${rail} border-line text-ink3 motion-safe:animate-pulse`}
      >
        <UpdateIcon size={16} />
      </div>
    );
  }

  const row =
    "flex w-full items-center gap-[10px] rounded-[8px] border px-[10px] py-[8px] text-left";
  const text = (
    <div className="min-w-0 flex-1">
      <div className="truncate text-[12.5px] leading-[1.2] font-medium">{title}</div>
      <div className="truncate pt-[3px] text-[11px] leading-[1.2] text-ink3">{detail}</div>
    </div>
  );
  return ready ? (
    <button
      type="button"
      aria-label={label}
      onClick={onInstall}
      className={`${row} cursor-pointer border-neon bg-transparent text-neon-ink shadow-(--plateGlow) transition-[background] duration-150 hover:bg-nav-on-bg`}
    >
      <UpdateIcon size={16} className="flex-none" />
      {text}
    </button>
  ) : (
    <div role="status" aria-label={label} className={`${row} border-line text-ink2`}>
      <UpdateIcon size={16} className="flex-none text-ink3 motion-safe:animate-pulse" />
      {text}
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
      className="group flex items-center gap-1 rounded-md pr-3 text-[12.5px] leading-[1.35] text-ink3 hover:bg-surface3 hover:text-ink2"
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
      {/* The row's own control, and a sibling of the archive × rather than its
          parent: a button cannot hold another button, and Tab has to reach
          each of them on its own. */}
      <button
        type="button"
        onClick={onOpen}
        className="flex min-w-0 flex-1 cursor-pointer items-center gap-1 border-0 bg-transparent py-[7px] pr-0 pl-3 text-left text-[length:inherit] leading-[inherit] text-inherit [font-family:inherit]"
      >
        {item.activity && <StatusDot tone={item.activity} label={DOT_LABELS[item.activity]} />}
        <Truncated className="min-w-0 flex-1">{item.title}</Truncated>
      </button>
      <button
        type="button"
        aria-label={`Archive ${item.title}`}
        title="Archive conversation"
        onClick={onArchive}
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
  agentName = "Borg",
  collapsed = false,
  onToggleCollapsed,
  unreadCount = 0,
  theme,
  onToggleTheme,
  update,
  onInstallUpdate,
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
        <div className="flex-1" />
        {update && <UpdateItem update={update} collapsed onInstall={onInstallUpdate} />}
      </aside>
    );
  }

  return (
    <aside className="flex w-[236px] flex-none flex-col border-r border-line2 bg-surface2 px-[14px] pt-[18px] pb-4">
      <div className="flex items-center gap-[11px] px-2 pt-0.5 pb-5">
        <LogoIcon size={20} />
        <span
          title={agentName}
          className="min-w-0 flex-1 truncate text-[13.5px] leading-[1.2] font-semibold tracking-[-0.01em] text-ink"
        >
          {agentName}
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
        <button
          type="button"
          onClick={onNewConversation}
          className="cursor-pointer rounded border-0 bg-transparent px-1.5 py-0.5 text-[11px] leading-none text-ink3 hover:bg-surface3 hover:text-ink"
        >
          + New
        </button>
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

      {update && (
        <div className="pb-3">
          <UpdateItem update={update} collapsed={false} onInstall={onInstallUpdate} />
        </div>
      )}

      <div className="flex items-center gap-[11px] border-t border-line2 px-2 pt-3">
        <span className="flex h-[26px] w-[26px] items-center justify-center rounded-full border border-line bg-surface3 text-[10.5px] leading-none font-semibold text-ink2">
          {user.initials}
        </span>
        <div className="min-w-0 flex-1">
          <div className="text-[12.5px] leading-[1.2] font-medium text-ink">{user.name}</div>
        </div>
        {onToggleTheme && theme && (
          // The icon shows where a click goes, and the label says it in words.
          <button
            type="button"
            aria-label={theme === "dark" ? "Switch to light mode" : "Switch to dark mode"}
            onClick={onToggleTheme}
            className="flex h-[26px] w-[26px] flex-none cursor-pointer items-center justify-center rounded-md border-0 bg-transparent text-ink3 hover:bg-surface3 hover:text-ink"
          >
            {theme === "dark" ? <SunIcon /> : <MoonIcon />}
          </button>
        )}
      </div>
    </aside>
  );
}
