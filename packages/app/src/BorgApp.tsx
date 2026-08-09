import { useEffect, useState } from "react";
import { HashRouter, Navigate, Route, Routes, useLocation, useNavigate } from "react-router-dom";
import { Sidebar, TabBar, ThemeProvider, useIsMobile } from "@borg/ui";
import { dispatchKeybind, installKeybinds, useKeybind } from "./lib/keybinds";
import { ConversationPage } from "./features/conversation/ConversationPage";
import { ConnectionDetailPage } from "./features/connections/ConnectionDetailPage";
import { OnboardingPage } from "./features/onboarding/OnboardingPage";
import { QuickEntryPage } from "./features/quick/QuickEntryPage";
import { SettingDetailPage } from "./features/settings/SettingDetailPage";
import { UpdatePill } from "./components/UpdatePill";
import { SettingsProvider } from "./lib/settings";
import { PlatformProvider, useNative, type Platform } from "./lib/platform";
import { useMutation, useQuery } from "./lib/useDaemon";
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

  useEffect(() => installKeybinds(window, /macintosh|mac os x/i.test(navigator.userAgent)), []);
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
  const navigate = useNavigate();
  const location = useLocation();
  // Re-fetched on every navigation so fresh conversations and cleared
  // warnings appear without a reload.
  const sessions = useQuery((borg) => borg.admin.listSessions({}), [location.key]);
  const recents = (sessions.state === "ready" ? sessions.data.sessions : [])
    .slice(0, 8)
    .map((s) => ({ id: s.id, title: s.title || "Untitled" }));

  // Archiving is a soft hide, not a delete — `borg sessions unarchive <id>`
  // brings a conversation back. The daemon also closes the session if it is
  // live, so the refetch is enough to drop the row.
  const archiveSession = useMutation(
    (borg, id: string) => borg.admin.setSessionArchived({ id, archived: true }),
    {
      onSuccess: (_data, id) => {
        sessions.refetch();
        // Closing the conversation on screen would otherwise leave the user
        // staring at a session that is no longer in the list.
        if (location.pathname === `/c/${id}`) navigate("/");
      },
    },
  );

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
            archiveError={archiveSession.error?.message}
            onNewConversation={() => navigate("/c/new")}
            onSeeAllConversations={() => navigate("/sessions")}
            user={user}
          />
        )}
        <Routes>
          {routes.map((r) => (
            <Route key={r.id} path={r.path} element={<r.page />} />
          ))}
          <Route path="/c/:sessionId" element={<ConversationPage />} />
          <Route path="/connections/:kind/:name" element={<ConnectionDetailPage />} />
          <Route path="/settings/key/:key" element={<SettingDetailPage />} />
          <Route path="/settings/:id" element={<SettingDetailPage />} />
          <Route path="*" element={<Navigate to="/" replace />} />
        </Routes>
      </div>
      {isMobile && <TabBar routes={navRoutes} />}
      <UpdatePill />
    </div>
  );
}

/**
 * The whole Borg product, shared verbatim by `apps/desktop` and `apps/web`.
 *
 * Surfaces differ only in the `platform` they pass: the transport used to reach
 * borgd, and whether the window needs a custom title-bar drag strip.
 */
/**
 * Gates the shell behind the daemon's onboarding flag. Only a positive
 * "not onboarded yet" answer shows the wizard — when the daemon is down the
 * shell renders and its pages explain the outage properly.
 */
function OnboardingGate({ platform }: { platform: Platform }) {
  const onboarding = useQuery((borg) => borg.admin.getOnboardingState({}));
  const [justFinished, setJustFinished] = useState(false);

  if (onboarding.state === "loading") return null;
  if (onboarding.state === "ready" && !onboarding.data.onboarded && !justFinished) {
    return <OnboardingPage onDone={() => setJustFinished(true)} />;
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
  return window.location.hash.startsWith("#/quick");
}

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
        <SettingsProvider>
          <OnboardingGate platform={platform} />
        </SettingsProvider>
      </ThemeProvider>
    </PlatformProvider>
  );
}
