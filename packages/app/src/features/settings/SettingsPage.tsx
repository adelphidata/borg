import { useState } from "react";
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
import { useSettings, type DaemonSetting } from "../../lib/settings";
import { routes } from "../../routes";
import { APPEARANCE_LABELS, advancedGroupOf, basicSettings } from "./catalog";

/** Registry keys already surfaced by the curated list above the fold. */
const CURATED_KEYS = new Set(basicSettings.flatMap((s) => (s.key ? [s.key] : [])));

function groupAdvanced(settings: Map<string, DaemonSetting>) {
  const groups = new Map<string, DaemonSetting[]>();
  for (const entry of settings.values()) {
    if (CURATED_KEYS.has(entry.key)) continue;
    const group = advancedGroupOf(entry.key);
    const rows = groups.get(group) ?? [];
    rows.push(entry);
    groups.set(group, rows);
  }
  return [...groups.entries()]
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([name, rows]) => ({
      name,
      rows: rows.sort((a, b) => a.key.localeCompare(b.key)),
    }));
}

export function SettingsPage() {
  const isMobile = useIsMobile();
  const navigate = useNavigate();
  const { settings } = useSettings();
  const { preference } = useTheme();
  const [advOpen, setAdvOpen] = useState(false);

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
                  const value =
                    s.key === null ? APPEARANCE_LABELS[preference] : (map.get(s.key)?.value ?? "—");
                  return (
                    <div
                      key={s.id}
                      className="flex cursor-pointer items-center gap-[18px] border-b border-line2 px-0.5 py-[18px]"
                      onClick={() => navigate(`/settings/${s.id}`)}
                    >
                      <div className="min-w-0 flex-1">
                        <div className="text-[14px] leading-[1.3] font-medium text-ink">
                          {s.name}
                        </div>
                        {s.desc && (
                          <div className="mt-1 text-[12.5px] leading-[1.45] text-ink3">
                            {s.desc}
                          </div>
                        )}
                      </div>
                      <div className="max-w-[220px] flex-none truncate text-[13px] leading-none text-ink2">
                        {value}
                      </div>
                      <ChevronIcon size={13} className="text-ink3" />
                    </div>
                  );
                })}

                <div
                  className="flex cursor-pointer items-center gap-[11px] px-0.5 pt-[22px]"
                  onClick={() => setAdvOpen((v) => !v)}
                >
                  <span
                    className={`flex transition-transform duration-[180ms] ${advOpen ? "[transform:rotate(90deg)]" : ""}`}
                  >
                    <ChevronIcon size={13} className="text-neon" />
                  </span>
                  <span className="text-[10px] leading-none font-medium tracking-[0.16em] uppercase text-ink2">
                    Advanced
                  </span>
                  <span className="h-px flex-1 bg-line2" />
                </div>

                {advOpen && (
                  <div className="flex flex-col gap-6 pt-2">
                    <div className="pt-3.5 text-[13px] leading-[1.6] text-pretty text-ink3">
                      Borg works without changing any of this. Everything below is on a sensible
                      default; anything shown in green has been changed.
                    </div>
                    {groupAdvanced(map).map((g) => (
                      <div key={g.name}>
                        <div className={SECTION_LABEL} style={{ marginBottom: 11 }}>
                          {g.name}
                        </div>
                        <div className="overflow-hidden rounded-[10px] border border-line2 bg-surface2">
                          {g.rows.map((r) => (
                            <div
                              key={r.key}
                              className="flex cursor-pointer items-center justify-between gap-[18px] border-b border-line2 px-4 py-[13px] transition-[background] duration-150 last:border-b-0 hover:bg-surface3"
                              onClick={() => navigate(`/settings/key/${encodeURIComponent(r.key)}`)}
                            >
                              <div className="min-w-0 text-[13px] leading-[1.3] font-medium text-ink">
                                {r.key}
                              </div>
                              <span
                                className={`max-w-[200px] flex-none truncate text-[12.5px] leading-none font-medium ${
                                  r.overridden ? "text-neon-ink" : "text-ink3"
                                }`}
                              >
                                {r.value === "" ? "—" : r.value}
                              </span>
                            </div>
                          ))}
                        </div>
                      </div>
                    ))}
                  </div>
                )}
              </>
            )}
          </DaemonState>
        </div>
      </div>
    </PageChrome>
  );
}
