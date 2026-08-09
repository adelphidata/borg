import { useParams } from "react-router-dom";
import {
  BackLink,
  CheckIcon,
  DETAIL_COL,
  PageChrome,
  SETTINGS_SCROLL_DESKTOP_PADDING,
  scrollCol,
  useIsMobile,
} from "@borg/ui";
import { findSetting } from "../../data/mock";
import { useSettingControl } from "../../lib/settings";

/** One setting, expanded into a picker. Selections persist locally until borgd lands. */
export function SettingDetailPage() {
  const { id = "" } = useParams();
  const isMobile = useIsMobile();
  const setting = findSetting(id);
  const { current, select } = useSettingControl();
  const scroll = scrollCol(isMobile, SETTINGS_SCROLL_DESKTOP_PADDING);

  if (!setting) {
    return (
      <PageChrome title="Settings">
        <div className={scroll}>
          <div className={DETAIL_COL}>
            <BackLink to="/settings" label="Settings" />
            <div className="text-[14.5px] leading-[1.62] text-pretty text-ink2">
              That setting doesn’t exist.
            </div>
          </div>
        </div>
      </PageChrome>
    );
  }

  const options = setting.options ?? [];
  const value = current(setting.id);

  return (
    <PageChrome title={setting.name}>
      <div className={scroll}>
        <div className={DETAIL_COL}>
          <BackLink to="/settings" label="Settings" />

          <div>
            <div className="text-[24px] leading-[1.2] font-light tracking-[-0.025em] text-ink">
              {setting.name}
            </div>
            {setting.desc && (
              <div className="mt-2 text-[13.5px] leading-[1.55] text-ink2">{setting.desc}</div>
            )}
          </div>

          <div className="overflow-hidden rounded-[10px] border border-line2 bg-surface2">
            {options.map((o) => {
              const on = o.value === value;
              return (
                <div
                  key={o.value}
                  className="flex cursor-pointer items-center gap-4 border-b border-line2 px-4 py-3.5 transition-[background] duration-150 last:border-b-0 hover:bg-surface3"
                  onClick={() => select(setting.id, o.value)}
                >
                  <div className="min-w-0 flex-1">
                    <div
                      className={`text-[13.5px] leading-[1.3] font-medium ${
                        on ? "text-neon-ink" : "text-ink"
                      }`}
                    >
                      {o.value}
                    </div>
                    {o.desc && (
                      <div className="mt-[3px] text-[12px] leading-[1.45] text-ink3">{o.desc}</div>
                    )}
                  </div>
                  {on && <CheckIcon size={15} role="img" aria-label="Selected" />}
                </div>
              );
            })}
          </div>

          <div className="pt-3.5 text-[13px] leading-[1.6] text-pretty text-ink3">
            Changes apply straight away. Nothing to save.
          </div>
        </div>
      </div>
    </PageChrome>
  );
}
