import { useState } from "react";
import { useNavigate } from "react-router-dom";
import { MobileTitle } from "../../components/MobileTitle";
import { PageChrome } from "../../components/PageChrome";
import { ChevronIcon } from "../../components/icons";
import { advancedGroups, basicSettings } from "../../data/mock";
import { useSettings } from "../../lib/settings";
import { useTheme } from "../../lib/theme";

export function SettingsPage() {
  const navigate = useNavigate();
  const { theme } = useTheme();
  const { values } = useSettings();
  const [advOpen, setAdvOpen] = useState(false);

  return (
    <PageChrome title="Settings">
      <div className="settings-scroll">
        <div className="settings-col">
          <MobileTitle style={{ marginBottom: 6 }}>Settings</MobileTitle>

          {basicSettings.map((s) => {
            // Appearance reflects the real theme; everything else reads the local store.
            const value =
              s.id === "appearance" ? (theme === "light" ? "Light" : "Dark") : values[s.id];
            return (
              <div key={s.id} className="setting-row" onClick={() => navigate(`/settings/${s.id}`)}>
                <div style={{ flex: 1, minWidth: 0 }}>
                  <div className="name">{s.name}</div>
                  {s.desc && <div className="desc">{s.desc}</div>}
                </div>
                <div className="value">{value}</div>
                <ChevronIcon size={13} style={{ color: "var(--ink3)" }} />
              </div>
            );
          })}

          <div className="adv-toggle" onClick={() => setAdvOpen((v) => !v)}>
            <span className={`chev${advOpen ? " open" : ""}`}>
              <ChevronIcon size={13} style={{ color: "var(--neon)" }} />
            </span>
            <span className="label">Advanced</span>
            <span className="rule" />
          </div>

          {advOpen && (
            <div className="adv-body">
              <div className="adv-note">
                Borg works without changing any of this. Everything below is on a sensible default.
              </div>
              {advancedGroups.map((g) => (
                <div key={g.name}>
                  <div className="section-label" style={{ marginBottom: 11 }}>
                    {g.name}
                  </div>
                  <div className="adv-group-box">
                    {g.rows.map((r) => (
                      <div key={r.key} className="adv-row">
                        <div style={{ minWidth: 0 }}>
                          <div className="key">{r.key}</div>
                          {r.desc && <div className="desc">{r.desc}</div>}
                        </div>
                        <span className="value">{r.value}</span>
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
