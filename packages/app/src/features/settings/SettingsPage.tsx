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
} from "@borg/ui";
import { advancedGroups, basicSettings } from "../../data/mock";
import { useSettingControl } from "../../lib/settings";

export function SettingsPage() {
  const isMobile = useIsMobile();
  const navigate = useNavigate();
  const { current } = useSettingControl();
  const [advOpen, setAdvOpen] = useState(false);

  return (
    <PageChrome title="Settings">
      <div className={scrollCol(isMobile, SETTINGS_SCROLL_DESKTOP_PADDING)}>
        <div className="flex max-w-[620px] flex-col">
          <MobileTitle className="mb-1.5">Settings</MobileTitle>

          {basicSettings.map((s) => (
            <div
              key={s.id}
              className="flex cursor-pointer items-center gap-[18px] border-b border-line2 px-0.5 py-[18px]"
              onClick={() => navigate(`/settings/${s.id}`)}
            >
              <div className="min-w-0 flex-1">
                <div className="text-[14px] leading-[1.3] font-medium text-ink">{s.name}</div>
                {s.desc && (
                  <div className="mt-1 text-[12.5px] leading-[1.45] text-ink3">{s.desc}</div>
                )}
              </div>
              <div className="flex-none text-[13px] leading-none text-ink2">{current(s.id)}</div>
              <ChevronIcon size={13} className="text-ink3" />
            </div>
          ))}

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
                Borg works without changing any of this. Everything below is on a sensible default.
              </div>
              {advancedGroups.map((g) => (
                <div key={g.name}>
                  <div className={SECTION_LABEL} style={{ marginBottom: 11 }}>
                    {g.name}
                  </div>
                  <div className="overflow-hidden rounded-[10px] border border-line2 bg-surface2">
                    {g.rows.map((r) => (
                      <div
                        key={r.key}
                        className="flex items-center justify-between gap-[18px] border-b border-line2 px-4 py-[13px] last:border-b-0"
                      >
                        <div className="min-w-0">
                          <div className="text-[13px] leading-[1.3] font-medium text-ink">
                            {r.key}
                          </div>
                          {r.desc && (
                            <div className="mt-[3px] text-[11.5px] leading-[1.4] text-ink3">
                              {r.desc}
                            </div>
                          )}
                        </div>
                        <span className="flex-none text-[12.5px] leading-none font-medium text-neon-ink">
                          {r.value}
                        </span>
                      </div>
                    ))}
                  </div>
                </div>
              ))}
            </div>
          )}
        </div>
      </div>
    </PageChrome>
  );
}
