import { useParams } from "react-router-dom";
import { BackLink } from "../../components/BackLink";
import { PageChrome } from "../../components/PageChrome";
import { CheckIcon } from "../../components/icons";
import { findSetting, type SettingOption } from "../../data/mock";
import { useSettings } from "../../lib/settings";
import { useTheme } from "../../lib/theme";

const APPEARANCE_OPTIONS: SettingOption[] = [
  { value: "Light", desc: "Paper and ink." },
  { value: "Dark", desc: "Neon on black. The classic." },
];

/** One setting, expanded into a picker. Selections persist locally until borgd lands. */
export function SettingDetailPage() {
  const { id = "" } = useParams();
  const setting = findSetting(id);
  const { values, setValue } = useSettings();
  const { theme, toggleTheme } = useTheme();

  if (!setting) {
    return (
      <PageChrome title="Settings">
        <div className="settings-scroll">
          <div className="detail-col">
            <BackLink to="/settings" label="Settings" />
            <div className="ambient">That setting doesn’t exist.</div>
          </div>
        </div>
      </PageChrome>
    );
  }

  // Appearance is backed by the real theme, everything else by the settings store.
  const isAppearance = setting.id === "appearance";
  const options = isAppearance ? APPEARANCE_OPTIONS : (setting.options ?? []);
  const current = isAppearance ? (theme === "light" ? "Light" : "Dark") : values[setting.id];
  const select = (value: string) => {
    if (isAppearance) {
      if (value !== current) toggleTheme();
    } else {
      setValue(setting.id, value);
    }
  };

  return (
    <PageChrome title={setting.name}>
      <div className="settings-scroll">
        <div className="detail-col">
          <BackLink to="/settings" label="Settings" />

          <div className="detail-title-block">
            <div className="title">{setting.name}</div>
            {setting.desc && <div className="desc">{setting.desc}</div>}
          </div>

          <div className="option-list">
            {options.map((o) => {
              const on = o.value === current;
              return (
                <div
                  key={o.value}
                  className={`option-row${on ? " on" : ""}`}
                  onClick={() => select(o.value)}
                >
                  <div style={{ flex: 1, minWidth: 0 }}>
                    <div className="value">{o.value}</div>
                    {o.desc && <div className="desc">{o.desc}</div>}
                  </div>
                  {on && <CheckIcon size={15} />}
                </div>
              );
            })}
          </div>

          <div className="adv-note">Changes apply straight away. Nothing to save.</div>
        </div>
      </div>
    </PageChrome>
  );
}
