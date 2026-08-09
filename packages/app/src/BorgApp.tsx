import { useState } from "react";
import { HashRouter, Navigate, Route, Routes, useLocation, useNavigate } from "react-router-dom";
import { Sidebar, TabBar, ThemeProvider, useIsMobile } from "@borg/ui";
import { ConversationPage } from "./features/conversation/ConversationPage";
import { ConnectionDetailPage } from "./features/connections/ConnectionDetailPage";
import { OnboardingPage } from "./features/onboarding/OnboardingPage";
import { SettingDetailPage } from "./features/settings/SettingDetailPage";
import { SettingsProvider } from "./lib/settings";
import { PlatformProvider, type Platform } from "./lib/platform";
import { useQuery } from "./lib/useDaemon";
import { routes } from "./routes";

const ONBOARDED_KEY = "borg.onboarded";

// TODO(daemon): no RPC carries an operator identity yet; the footer stays static.
const user = { name: "Operator", initials: "OP" };

function Shell({ platform }: { platform: Platform }) {
  const isMobile = useIsMobile();
  const navigate = useNavigate();
  const location = useLocation();
  // Re-listed on every navigation so fresh conversations appear immediately.
  const sessions = useQuery((borg) => borg.admin.listSessions({}), [location.key]);
  const recents = (sessions.state === "ready" ? sessions.data.sessions : [])
    .slice(0, 8)
    .map((s) => ({ id: s.id, title: s.title || "Untitled" }));

  return (
    <div className="flex h-full flex-col bg-surface transition-[background] duration-[250ms]">
      {/* macOS drag strip over the hidden title bar — desktop only; a browser has its own chrome. */}
      {platform.titleBarDrag && !isMobile && (
        <div className="flex h-[38px] flex-none items-center gap-2 border-b border-line2 bg-surface2 px-[15px] [-webkit-app-region:drag]" />
      )}
      <div className="flex min-h-0 flex-1">
        {!isMobile && (
          <Sidebar
            routes={routes}
            recents={recents}
            onOpenRecent={(id) => navigate(`/c/${id}`)}
            onNewConversation={() => navigate("/c/new")}
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
      {isMobile && <TabBar routes={routes} />}
    </div>
  );
}

/**
 * The whole Borg product, shared verbatim by `apps/desktop` and `apps/web`.
 *
 * Surfaces differ only in the `platform` they pass: the transport used to reach
 * borgd, and whether the window needs a custom title-bar drag strip.
 */
export function BorgApp({ platform }: { platform: Platform }) {
  const [onboarded, setOnboarded] = useState(() => localStorage.getItem(ONBOARDED_KEY) === "1");

  return (
    <PlatformProvider platform={platform}>
      <ThemeProvider>
        <SettingsProvider>
          {onboarded ? (
            <HashRouter>
              <Shell platform={platform} />
            </HashRouter>
          ) : (
            <OnboardingPage
              onDone={() => {
                localStorage.setItem(ONBOARDED_KEY, "1");
                setOnboarded(true);
              }}
            />
          )}
        </SettingsProvider>
      </ThemeProvider>
    </PlatformProvider>
  );
}
