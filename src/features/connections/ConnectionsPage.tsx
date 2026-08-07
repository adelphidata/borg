import { useNavigate } from "react-router-dom";
import { MobileTitle } from "../../components/MobileTitle";
import { PageChrome } from "../../components/PageChrome";
import { Pill } from "../../components/Pill";
import { available, connected } from "../../data/mock";
import { useIsMobile } from "../../lib/useIsMobile";

function AlertBanner({ stacked, onFix }: { stacked: boolean; onFix: () => void }) {
  if (stacked) {
    return (
      <div className="alert-banner stacked">
        <div className="head">
          <span className="dot" />
          <div className="title">Gmail needs you</div>
        </div>
        <div className="sub">The sign-in expired yesterday. Three messages are queued.</div>
        <Pill variant="danger" style={{ alignSelf: "flex-start" }} onClick={onFix}>
          Sign in again
        </Pill>
      </div>
    );
  }
  return (
    <div className="alert-banner">
      <span className="dot" />
      <div style={{ flex: 1, minWidth: 0 }}>
        <div className="title">Gmail stopped working yesterday at 4:12pm</div>
        <div className="sub">
          The sign-in expired. Nothing was lost — Borg queued three messages.
        </div>
      </div>
      <Pill variant="danger" onClick={onFix}>
        Sign in again
      </Pill>
    </div>
  );
}

function StatusDot({ needsAttention }: { needsAttention?: boolean }) {
  return needsAttention ? <span className="alert-dot" /> : <span className="neon-dot" />;
}

/** Desktop: card grid of connected services + "Add" grid. */
function DesktopConnections({ open }: { open: (id: string) => void }) {
  return (
    <>
      <section>
        <div className="section-label">Connected</div>
        <div className="conn-grid">
          {connected.map((c) => (
            <div key={c.id} className="conn-card" onClick={() => open(c.id)}>
              <div className="head">
                <span className="badge">{c.badge}</span>
                <StatusDot needsAttention={c.needsAttention} />
              </div>
              <div className="name">{c.name}</div>
              <div className="sub">{c.sub}</div>
            </div>
          ))}
        </div>
      </section>

      <section>
        <div className="section-label">Add</div>
        <div className="conn-grid add">
          {available.map((c) => (
            <div key={c.id} className="add-card" onClick={() => open(c.id)}>
              <span className="badge off">{c.badge}</span>
              <span className="name">{c.name}</span>
            </div>
          ))}
        </div>
      </section>
    </>
  );
}

/** Mobile: connected services as list rows + an "Add" grid of the rest. */
function MobileConnections({ open }: { open: (id: string) => void }) {
  return (
    <>
      <div className="conn-list">
        {connected.map((c) => (
          <div key={c.id} className="conn-row" onClick={() => open(c.id)}>
            <span className="badge">{c.badge}</span>
            <div style={{ flex: 1, minWidth: 0 }}>
              <div className="name">{c.name}</div>
              <div className="sub">{c.sub}</div>
            </div>
            <StatusDot needsAttention={c.needsAttention} />
          </div>
        ))}
      </div>
      <section>
        <div className="section-label">Add</div>
        <div className="conn-grid add">
          {available.map((c) => (
            <div key={c.id} className="add-card" onClick={() => open(c.id)}>
              <span className="badge off">{c.badge}</span>
              <span className="name">{c.name}</span>
            </div>
          ))}
        </div>
      </section>
    </>
  );
}

export function ConnectionsPage() {
  const isMobile = useIsMobile();
  const navigate = useNavigate();
  const open = (id: string) => navigate(`/connections/${id}`);

  return (
    <PageChrome title="Connections" meta="5 connected · 1 needs attention">
      <div className="conn-scroll">
        <div className="conn-col">
          <MobileTitle>Connections</MobileTitle>
          <AlertBanner stacked={isMobile} onFix={() => open("gmail")} />
          {isMobile ? <MobileConnections open={open} /> : <DesktopConnections open={open} />}
        </div>
      </div>
    </PageChrome>
  );
}
