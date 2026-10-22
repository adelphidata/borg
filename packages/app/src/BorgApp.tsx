import { useEffect, useRef, useState, useSyncExternalStore } from "react";
import { HashRouter, Navigate, Route, Routes, useLocation, useNavigate } from "react-router-dom";
import {
  ArchiveIcon,
  Menu,
  Sidebar,
  TabBar,
  ThemeProvider,
  Toast,
  useIsMobile,
  usePauseAnimationsWhenHidden,
  useTheme,
} from "@borg/ui";
import { dispatchKeybind, installKeybinds, isMacPlatform, useKeybind } from "./lib/keybinds";
import { QUICK_ENTRY_HASH } from "./lib/chords";
import { readSidebarCollapsed, writeSidebarCollapsed } from "./lib/sidebarCollapsed";
import { clearNotice, offerRestore, showNotice, takeNotice, useNotice } from "./lib/notice";
import { ConversationPage } from "./features/conversation/ConversationPage";
import {
  draftsVersion,
  isBlankSession,
  rowTitle,
  subscribeDrafts,
} from "./features/conversation/draftStore";
import { ConnectionDetailPage } from "./features/connections/ConnectionDetailPage";
import { CommandPalette } from "./features/palette/CommandPalette";
import { BootGate } from "./features/boot/BootGate";
import { OnboardingPage } from "./features/onboarding/OnboardingPage";
import { QuickEntryPage } from "./features/quick/QuickEntryPage";
import {
  activityVersion,
  sessionActivity,
  subscribeActivity,
} from "./features/sessions/activityStore";
import {
  markAllSessionsRead,
  observeSessions,
  subscribeUnread,
  unreadVersion,
} from "./features/sessions/unreadStore";
import { useMenuEscape } from "./features/sessions/SessionRowMenu";
import {
  sessionsChanged,
  sessionsVersion,
  subscribeSessions,
} from "./features/sessions/sessionsVersion";
import { useSessionActivityFeed } from "./features/sessions/useSessionActivity";
import { SettingDetailPage } from "./features/settings/SettingDetailPage";
import { VoiceSettingsPage } from "./features/settings/VoiceSettingsPage";
import { PageErrorBoundary } from "./components/PageErrorBoundary";
import { UpdatePill } from "./components/UpdatePill";
import { useAgentName } from "./lib/agentName";
import { SettingsProvider, useSettings } from "./lib/settings";
import { PlatformProvider, useNative, type Platform } from "./lib/platform";
import { useBorg, useMutation, useQuery } from "./lib/useDaemon";
import type { admin } from "@borg/client";
import { routes } from "./routes";

// TODO(daemon): no RPC carries an operator identity yet; the footer stays static.
const user = { name: "Operator", initials: "OP" };

/**
 * Installs the keyboard layer and lets the native shell drive it.
 *
 * Menu items and global hotkeys arrive as action ids and go through the same
 * registry as typed chords, so ⌘N from the File menu and ⌘N from the keyboard
 * run one implementation.
 */
function KeyboardLayer() {
  const navigate = useNavigate();
  const native = useNative();

  useEffect(() => installKeybinds(window, isMacPlatform()), []);
  useEffect(() => native.onMenuCommand((action) => void dispatchKeybind(action)), [native]);
  // A clicked notification, or a turn started from the quick-entry panel.
  useEffect(
    () => native.onActivateSession((sessionId) => navigate(`/c/${sessionId}`)),
    [native, navigate],
  );

  useKeybind("new-conversation", () => navigate("/c/new"));
  useKeybind("open-settings", () => navigate("/settings"));
  // A page with its own search box rebinds this; this is the fallback that
  // takes the user somewhere they can actually search.
  useKeybind("search-sessions", () => navigate("/sessions"));

  return null;
}

function Shell({ platform }: { platform: Platform }) {
  const isMobile = useIsMobile();
  usePauseAnimationsWhenHidden();
  const { theme, toggleTheme } = useTheme();
  const agentName = useAgentName();
  const navigate = useNavigate();
  const location = useLocation();
  // Re-fetched on every navigation so fresh conversations and cleared
  // warnings appear without a reload.
  // A write on the conversations page must renew this list too — the two
  // queries are independent, so only a shared counter connects them.
  const listVersion = useSyncExternalStore(subscribeSessions, sessionsVersion, sessionsVersion);
  // The last listing that arrived. A refresh that fails — one dropped call
  // during a daemon restart — must not empty the sidebar and zero the unread
  // badge; the rows it had are still the best answer until a later read
  // replaces them. Only a first load has nothing to fall back on.
  //
  // Only the same daemon's rows, though. A new client is a different daemon,
  // or the same one reached another way, and its conversations are not the
  // old one's: a failed first read there must show nothing rather than rows
  // that belong somewhere else.
  const client = useBorg();
  const lastListed = useRef<admin.SessionSummary[]>([]);
  const listedFrom = useRef(client);
  if (listedFrom.current !== client) {
    listedFrom.current = client;
    lastListed.current = [];
  }
  const sessions = useQuery(
    async (borg) => {
      const list = await borg.admin.listSessions({});
      // Read marks are written in the daemon's units, so every listing tells the
      // store what those units currently are.
      observeSessions(
        list.sessions.map((s) => ({
          id: s.id,
          updatedAt: Number(s.updatedUnixTs),
          messageCount: Number(s.messageCount),
        })),
      );
      // Tagged with the client this render holds: `useQuery` keeps the last
      // answer on screen across a client change, and that answer must not
      // refill the rows. The closure's client, not `borg`: each `useBorg()`
      // builds its own object, so the two never compare equal.
      return { list, from: client };
    },
    [location.key, listVersion],
  );
  // Same naming rule as the full list: an unsent session is called after what
  // has been typed into it. The version counter re-renders as that changes.
  useSyncExternalStore(subscribeDrafts, draftsVersion, draftsVersion);
  if (sessions.state === "ready" && sessions.data.from === client) {
    lastListed.current = sessions.data.list.sessions;
  }
  // One subscription for the whole app; every list reads the store it fills.
  // It also renews this list when a turn ends or a conversation it has never
  // listed starts one, so a reply that lands in the background gets its dot
  // — and a conversation a schedule opened gets its row — without a click.
  useSessionActivityFeed((id) => lastListed.current.some((s) => s.id === id));
  useSyncExternalStore(subscribeActivity, activityVersion, activityVersion);
  // And the read marks, so opening a conversation drops its dot here at once
  // rather than on the next refetch. Lighting a dot still waits for one: the
  // mark is compared against the listing's own figures.
  useSyncExternalStore(subscribeUnread, unreadVersion, unreadVersion);
  const listed = lastListed.current
    .filter((s) => s.pinned || !isBlankSession(s.title, Number(s.messageCount), s.id))
    .map((s) => ({
      id: s.id,
      title: rowTitle(s.title, Number(s.messageCount), s.id),
      activity: sessionActivity(s.id, Number(s.messageCount), Number(s.updatedUnixTs)),
    }));
  const recents = listed.slice(0, 8);

  // Counted over the whole listing rather than the eight rows above: the badge
  // exists for the collapsed rail, where no row is on screen at all, and a
  // ninth conversation replying is exactly the case nothing else would show.
  //
  // A session mid-turn is not counted. `sessionActivity` calls that "working",
  // and a reply that has not finished arriving is not yet something the user
  // has missed — it becomes unread when it lands.
  const unreadCount = listed.filter((r) => r.activity === "unread").length;

  // Archiving is a soft hide, not a delete — `borg sessions unarchive <id>`
  // brings a conversation back. The daemon also closes the session if it is
  // live, so the refetch is enough to drop the row.
  const restoreSession = useMutation(
    (borg, id: string) => borg.admin.setSessionArchived({ id, archived: false }),
    { onSuccess: () => sessionsChanged() },
  );
  const archiveSession = useMutation(
    (borg, id: string) => borg.admin.setSessionArchived({ id, archived: true }),
    {
      onSuccess: (_data, id) => {
        sessionsChanged();
        // Closing the conversation on screen would otherwise leave the user
        // staring at a session that is no longer in the list.
        if (location.pathname === `/c/${id}`) navigate("/");
        // A row vanishing is the only sign the archive worked, and it is also
        // exactly what an accidental click looks like. The toast says which
        // conversation went, and gives it back without a dialog having stood
        // in the way of the deliberate case.
        offerRestore(id, listed.find((r) => r.id === id)?.title);
      },
    },
  );

  const notice = useNotice();
  // A restore that failed must not pass for one that worked: the row stays
  // gone, so saying nothing would leave the user believing the conversation
  // came back. `useMutation` parks the reason here rather than throwing.
  const restoreFailure = restoreSession.error?.message;
  useEffect(() => {
    if (restoreFailure) showNotice(`Could not bring the conversation back: ${restoreFailure}`);
  }, [restoreFailure]);

  // Right-click on a sidebar row. The menu lives here rather than in the
  // sidebar because it needs the archive call and the stacked Escape registry,
  // neither of which the design-system package can see.
  const [recentMenu, setRecentMenu] = useState<{ id: string; at: { x: number; y: number } }>();
  const [sidebarCollapsed, setSidebarCollapsed] = useState(() =>
    readSidebarCollapsed(globalThis.localStorage),
  );
  // Persist in the handler, not in the updater: React may run an updater
  // speculatively and throw the render away, and a discarded toggle must not
  // leave a collapsed sidebar written down.
  const toggleSidebar = () => {
    const next = !sidebarCollapsed;
    writeSidebarCollapsed(globalThis.localStorage, next);
    setSidebarCollapsed(next);
  };
  useKeybind("toggle-sidebar", toggleSidebar);
  useMenuEscape(recentMenu !== undefined, () => setRecentMenu(undefined));

  // Attention dots: doctor warnings light Health, pending pairings light
  // Connections. Unreachable daemon lights nothing — the pages say it better.
  const alerts = useQuery(
    async (borg) => {
      const [warnings, pairings] = await Promise.all([
        borg.admin.getPersistentWarnings({}),
        borg.admin.listPairingRequests({}),
      ]);
      return {
        health: warnings.warnings.length > 0,
        connections: pairings.requests.some((r) => r.status === "pending"),
      };
    },
    [location.key],
  );
  const navRoutes = routes.map((r) => ({
    ...r,
    hasAlert:
      alerts.state === "ready" &&
      ((r.id === "health" && alerts.data.health) ||
        (r.id === "connections" && alerts.data.connections)),
  }));

  return (
    <div className="flex h-full flex-col bg-surface transition-[background] duration-[250ms]">
      <KeyboardLayer />
      {/* macOS drag strip over the hidden title bar — desktop only; a browser has its own chrome. */}
      {platform.titleBarDrag && !isMobile && (
        <div className="flex h-[38px] flex-none items-center gap-2 border-b border-line2 bg-surface2 px-[15px] [-webkit-app-region:drag]" />
      )}
      <div className="flex min-h-0 flex-1">
        {!isMobile && (
          <Sidebar
            routes={navRoutes}
            recents={recents}
            onOpenRecent={(id) => navigate(`/c/${id}`)}
            onArchiveRecent={(id) => void archiveSession.run(id)}
            onRecentContextMenu={(id, at) => setRecentMenu({ id, at })}
            archiveError={archiveSession.error?.message}
            onNewConversation={() => navigate("/c/new")}
            onSeeAllConversations={() => navigate("/sessions")}
            user={user}
            agentName={agentName}
            collapsed={sidebarCollapsed}
            onToggleCollapsed={toggleSidebar}
            unreadCount={unreadCount}
            theme={theme}
            onToggleTheme={toggleTheme}
          />
        )}
        <Menu
          open={recentMenu !== undefined}
          onClose={() => setRecentMenu(undefined)}
          label="Conversation actions"
          at={recentMenu?.at}
          // The sidebar row offers archiving and nothing else, so the menu
          // offers exactly that. Right-click should reach what the row can do,
          // not advertise capability it does not have here.
          actions={[
            {
              id: "archive",
              label: "Archive",
              icon: <ArchiveIcon size={13} />,
              onSelect: () => {
                if (recentMenu) void archiveSession.run(recentMenu.id);
              },
            },
          ]}
        />
        <PageErrorBoundary resetKey={location.pathname}>
          <Routes>
            {routes.map((r) => (
              <Route key={r.id} path={r.path} element={<r.page />} />
            ))}
            {/* Keyed on the navigation, not the path: "+ New" from a
                conversation that itself began at `/c/new` lands on the same
                route, and without a remount the page keeps the session it
                already has. */}
            <Route path="/c/:sessionId" element={<ConversationPage key={location.key} />} />
            <Route path="/connections/:kind/:name" element={<ConnectionDetailPage />} />
            <Route path="/settings/voice" element={<VoiceSettingsPage />} />
            <Route path="/settings/key/:key" element={<SettingDetailPage />} />
            <Route path="/settings/:id" element={<SettingDetailPage />} />
            <Route path="*" element={<Navigate to="/" replace />} />
          </Routes>
        </PageErrorBoundary>
      </div>
      {isMobile && <TabBar routes={navRoutes} />}
      <CommandPalette />
      <UpdatePill />
      {/* One slot for the whole shell: every reversible action anywhere in the
          product offers its way back here. */}
      <Toast
        id={notice?.id}
        message={notice?.message}
        // The shell runs the restore rather than a closure from the page that
        // archived: that page is often unmounted by now.
        actionLabel={notice?.sessionId ? "Undo" : undefined}
        onAction={() => {
          const offer = takeNotice();
          if (offer?.sessionId) void restoreSession.run(offer.sessionId);
        }}
        onDismiss={clearNotice}
        // Clear of the mobile tab bar, which is 72px plus the home indicator.
        bottomInset={isMobile ? "calc(88px + env(safe-area-inset-bottom))" : undefined}
      />
    </div>
  );
}

/**
 * Gates the shell behind the daemon's onboarding flag. Only a positive
 * "not onboarded yet" answer shows the wizard — when the daemon is down the
 * shell renders and its pages explain the outage properly.
 */
function OnboardingGate({ platform }: { platform: Platform }) {
  const onboarding = useQuery((borg) => borg.admin.getOnboardingState({}));
  const { settings } = useSettings();
  const [justFinished, setJustFinished] = useState(false);

  if (onboarding.state === "loading") return null;
  if (onboarding.state === "ready" && !onboarding.data.onboarded && !justFinished) {
    // The snapshot predates the first run's writes; the sidebar needs the name.
    return (
      <OnboardingPage
        onDone={() => {
          settings.refetch();
          setJustFinished(true);
        }}
      />
    );
  }
  return (
    <HashRouter>
      <Shell platform={platform} />
    </HashRouter>
  );
}

/**
 * True when this renderer is the quick-entry panel.
 *
 * Read from the hash once rather than routed: the panel is its own
 * `BrowserWindow` loaded at `#/quick` and never navigates, and keeping it
 * outside the router means it skips the onboarding gate and the settings
 * fetch — neither of which a one-line capture box has any use for.
 */
function isQuickEntry(): boolean {
  return window.location.hash.startsWith(QUICK_ENTRY_HASH);
}

/**
 * The whole Borg product, shared verbatim by `apps/desktop` and `apps/web`.
 *
 * Surfaces differ only in the `platform` they pass: the transport used to reach
 * borgd, and whether the window needs a custom title-bar drag strip.
 */
export function BorgApp({ platform }: { platform: Platform }) {
  if (isQuickEntry()) {
    return (
      <PlatformProvider platform={platform}>
        <ThemeProvider>
          <QuickEntryPage />
        </ThemeProvider>
      </PlatformProvider>
    );
  }

  return (
    <PlatformProvider platform={platform}>
      <ThemeProvider>
        <BootGate>
          <SettingsProvider>
            <OnboardingGate platform={platform} />
          </SettingsProvider>
        </BootGate>
      </ThemeProvider>
    </PlatformProvider>
  );
}
