import { useNavigate } from "react-router-dom";
import {
  ChevronIcon,
  MobileTitle,
  PageChrome,
  SECTION_LABEL,
  SETTINGS_SCROLL_DESKTOP_PADDING,
  scrollCol,
  useIsMobile,
  useTheme,
} from "@borg/ui";
import { DaemonState } from "../../components/DaemonState";
import { useSettings } from "../../lib/settings";
import { routes } from "../../routes";
import {
  effectiveModel,
  modelLabel,
  providerLabel,
  useSetupStatus,
} from "../onboarding/useSetupStatus";
import { DEFAULT_AGENT_NAME } from "../../lib/agentName";
import { AdvancedSettings, SETTING_NAME, SETTING_ROW } from "./AdvancedSettings";
import { APPEARANCE_LABELS, basicSettings, displayValue } from "./catalog";

/**
 * Registry keys Advanced leaves out: the curated list above the fold already
 * shows them, or the daemon writes them as its own state. The heartbeat
 * stamps `last_fired_at` on every fire to share one throttle across
 * scheduled, poked and session-start runs; a hand edit only breaks that.
 */
const SKIPPED_KEYS = new Set([
  ...basicSettings.flatMap((s) => (s.key ? [s.key] : [])),
  "heartbeat.last_fired_at",
]);

export function SettingsPage() {
  const isMobile = useIsMobile();
  const navigate = useNavigate();
  const { settings } = useSettings();
  const { preference } = useTheme();
  // Provider and model read as their display names; the raw id is the
  // honest fallback while the catalog is still on its way.
  const setup = useSetupStatus();
  const catalog = setup.state === "ready" ? setup.data.providers : [];

  return (
    <PageChrome title="Settings">
      <div className={scrollCol(isMobile, SETTINGS_SCROLL_DESKTOP_PADDING)}>
        <div className="flex max-w-[620px] flex-col">
          <MobileTitle className="mb-1.5">Settings</MobileTitle>

          {/* The tab bar tops out at five, so the pages it cannot hold are
              reached from here. On desktop the sidebar already lists them. */}
          {isMobile && (
            <div className="mb-6">
              {routes
                .filter((r) => r.nav?.tabBar === false)
                .map((route) => (
                  <button
                    key={route.id}
                    type="button"
                    onClick={() => navigate(route.path)}
                    className="flex w-full cursor-pointer items-center gap-3 border-0 border-b border-line2 bg-transparent px-1 py-3.5 text-left last:border-0"
                  >
                    <route.icon size={16} />
                    <span className="flex-1 text-[13.5px] leading-none text-ink">
                      {route.label}
                    </span>
                    <ChevronIcon />
                  </button>
                ))}
            </div>
          )}

          <DaemonState query={settings} label="settings">
            {(map) => (
              <>
                {basicSettings.map((s) => {
                  const raw = s.key === null ? undefined : map.get(s.key)?.value;
                  const value =
                    s.key === null
                      ? APPEARANCE_LABELS[preference]
                      : raw === undefined
                        ? "—"
                        : s.catalog === "name"
                          ? raw.trim() || DEFAULT_AGENT_NAME
                          : s.catalog === "provider"
                            ? providerLabel(catalog, raw)
                            : s.catalog === "model"
                              ? modelLabel(
                                  catalog,
                                  map.get("provider")?.value ?? "",
                                  effectiveModel(catalog, map.get("provider")?.value ?? "", raw),
                                )
                              : displayValue(s, raw);
                  return (
                    <button
                      type="button"
                      key={s.id}
                      className={SETTING_ROW}
                      onClick={() => navigate(`/settings/${s.id}`)}
                    >
                      <span className={SETTING_NAME}>{s.name}</span>
                      <span className="max-w-[220px] flex-none truncate text-[13px] leading-none text-ink2">
                        {value}
                      </span>
                      <ChevronIcon size={13} className="flex-none text-ink3" />
                    </button>
                  );
                })}

                <AdvancedSettings settings={map} skip={SKIPPED_KEYS} />
              </>
            )}
          </DaemonState>
        </div>
      </div>
    </PageChrome>
  );
}
