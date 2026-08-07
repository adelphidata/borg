import { useNavigate } from "react-router-dom";
import { AlertBanner } from "../../components/AlertBanner";
import { MobileTitle } from "../../components/MobileTitle";
import { PageChrome } from "../../components/PageChrome";
import { StatusDot } from "../../components/StatusDot";
import { available, connected } from "../../data/mock";
import { useIsMobile } from "../../lib/useIsMobile";

/** Desktop: connected services as a card grid. */
function ConnCards({ open }: { open: (id: string) => void }) {
  return (
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
  );
}

/** Mobile: connected services as list rows. */
function ConnList({ open }: { open: (id: string) => void }) {
  return (
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
  );
}

function AddGrid({ open }: { open: (id: string) => void }) {
  return (
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
  );
}

export function ConnectionsPage() {
  const isMobile = useIsMobile();
  const navigate = useNavigate();
  const open = (id: string) => navigate(`/connections/${id}`);

  const attention = connected.filter((c) => c.needsAttention);
  const alerted = attention[0];
  const meta = `${connected.length} connected${
    attention.length > 0 ? ` · ${attention.length} needs attention` : ""
  }`;

  return (
    <PageChrome title="Connections" meta={meta}>
      <div className="conn-scroll">
        <div className="conn-col">
          <MobileTitle>Connections</MobileTitle>
          {alerted && (
            <AlertBanner conn={alerted} stacked={isMobile} onAction={() => open(alerted.id)} />
          )}
          {isMobile ? <ConnList open={open} /> : <ConnCards open={open} />}
          <AddGrid open={open} />
        </div>
      </div>
    </PageChrome>
  );
}
