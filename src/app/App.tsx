import { useState } from "react";
import { HashRouter, Navigate, Route, Routes } from "react-router-dom";
import { Sidebar } from "../components/Sidebar";
import { TabBar } from "../components/TabBar";
import { ConnectionDetailPage } from "../features/connections/ConnectionDetailPage";
import { OnboardingPage } from "../features/onboarding/OnboardingPage";
import { SettingDetailPage } from "../features/settings/SettingDetailPage";
import { SettingsProvider } from "../lib/settings";
import { ThemeProvider } from "../lib/theme";
import { useIsMobile } from "../lib/useIsMobile";
import { routes } from "./routes";

const ONBOARDED_KEY = "borg.onboarded";

function Shell() {
  const isMobile = useIsMobile();

  return (
    <div className="flex h-full flex-col bg-surface transition-[background] duration-[250ms]">
      {/* macOS drag strip over the hidden title bar */}
      {!isMobile && (
        <div className="flex h-[38px] flex-none items-center gap-2 border-b border-line2 bg-surface2 px-[15px] [-webkit-app-region:drag]" />
      )}
      <div className="flex min-h-0 flex-1">
        {!isMobile && <Sidebar />}
        <Routes>
          {routes.map((r) => (
            <Route key={r.id} path={r.path} element={<r.page />} />
          ))}
          <Route path="/connections/:id" element={<ConnectionDetailPage />} />
          <Route path="/settings/:id" element={<SettingDetailPage />} />
          <Route path="*" element={<Navigate to="/" replace />} />
        </Routes>
      </div>
      {isMobile && <TabBar />}
    </div>
  );
}

export function App() {
  const [onboarded, setOnboarded] = useState(() => localStorage.getItem(ONBOARDED_KEY) === "1");

  return (
    <ThemeProvider>
      <SettingsProvider>
        {onboarded ? (
          <HashRouter>
            <Shell />
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
  );
}
