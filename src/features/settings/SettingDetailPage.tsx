import { useParams } from "react-router-dom";
import { BackLink } from "../../components/BackLink";
import { PageChrome } from "../../components/PageChrome";
import { CheckIcon } from "../../components/icons";
import { findSetting } from "../../data/mock";
import { useSettingControl } from "../../lib/settings";

/** One setting, expanded into a picker. Selections persist locally until borgd lands. */
export function SettingDetailPage() {
  const { id = "" } = useParams();
  const setting = findSetting(id);
  const { current, select } = useSettingControl();

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

  const options = setting.options ?? [];
  const value = current(setting.id);

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
              const on = o.value === value;
              return (
                <div
                  key={o.value}
                  className={`option-row${on ? " on" : ""}`}
                  onClick={() => select(setting.id, o.value)}
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
