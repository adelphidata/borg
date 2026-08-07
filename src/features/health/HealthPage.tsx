import { useState } from "react";
import { MobileTitle } from "../../components/MobileTitle";
import { PageChrome } from "../../components/PageChrome";
import { Pill } from "../../components/Pill";
import { CheckIcon } from "../../components/icons";
import { checks } from "../../data/mock";

export function HealthPage() {
  const [openId, setOpenId] = useState<string | null>(null);
  // No daemon yet — "Check again" just refreshes the mock timestamp.
  const [lastChecked, setLastChecked] = useState("a few minutes ago");
  const failing = checks.filter((c) => !c.ok);

  return (
    <PageChrome title="Health" meta={`last checked ${lastChecked}`}>
      <div className="health-scroll">
        <div className="health-col">
          <MobileTitle>Health</MobileTitle>

          <div className="health-hero">
            <span className="ring">
              <CheckIcon size={22} />
            </span>
            <div style={{ flex: 1, paddingTop: 2 }}>
              <h1>Borg is healthy.</h1>
              <p>
                {failing.length > 0 && <>One thing needs you: the {failing[0].name}. </>}
                Everything else checked out {lastChecked}.
              </p>
            </div>
          </div>

          <div>
            {checks.map((c) => {
              const open = openId === c.id;
              return (
                <div key={c.id}>
                  <div
                    className="check-row"
                    onClick={() => setOpenId((id) => (id === c.id ? null : c.id))}
                  >
                    <span
                      className="dot"
                      style={{ background: c.ok ? "var(--neon)" : "var(--alert)" }}
                    />
                    <span className="name">{c.name}</span>
                    <span className="value">{c.value}</span>
                  </div>
                  {open && (
                    <div className="check-detail">
                      <p>{c.detail}</p>
                      {c.action && <Pill variant="danger">{c.action}</Pill>}
                    </div>
                  )}
                </div>
              );
            })}
          </div>

          <div className="health-actions">
            <Pill variant="primary" onClick={() => setLastChecked("just now")}>
              Check again
            </Pill>
            <Pill>Tidy up now</Pill>
          </div>
        </div>
      </div>
    </PageChrome>
  );
}
