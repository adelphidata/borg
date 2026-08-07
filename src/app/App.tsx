import { useState } from "react";
import { HashRouter, Navigate, Route, Routes } from "react-router-dom";
import { Sidebar } from "../components/Sidebar";
import { TabBar } from "../components/TabBar";
import { OnboardingPage } from "../features/onboarding/OnboardingPage";
import { SettingsProvider } from "../lib/settings";
import { ThemeProvider } from "../lib/theme";
import { useIsMobile } from "../lib/useIsMobile";
import { detailRoutes, routes } from "./routes";

const ONBOARDED_KEY = "borg.onboarded";

function Shell() {
  const isMobile = useIsMobile();

  return (
    <div className={`app${isMobile ? " mobile" : ""}`}>
      {!isMobile && <div className="drag-strip" />}
      <div className="app-body">
        {!isMobile && <Sidebar />}
        <Routes>
          {routes.map((r) => (
            <Route key={r.id} path={r.path} element={<r.page />} />
          ))}
          {detailRoutes.map((r) => (
            <Route key={r.id} path={r.path} element={<r.page />} />
          ))}
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
