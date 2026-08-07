import { useNavigate, useParams } from "react-router-dom";
import { AlertBanner } from "../../components/AlertBanner";
import { BackLink } from "../../components/BackLink";
import { PageChrome } from "../../components/PageChrome";
import { Pill } from "../../components/Pill";
import { StatusDot } from "../../components/StatusDot";
import { connected, findConnection } from "../../data/mock";

/**
 * One connection, up close. Connected services show status + recent activity;
 * available ones show the pitch and a Connect button (mock until borgd lands).
 */
export function ConnectionDetailPage() {
  const { id = "" } = useParams();
  const navigate = useNavigate();
  const conn = findConnection(id);

  if (!conn) {
    return (
      <PageChrome title="Connections">
        <div className="conn-scroll">
          <div className="detail-col">
            <BackLink to="/connections" label="Connections" />
            <div className="ambient">That connection doesn’t exist.</div>
          </div>
        </div>
      </PageChrome>
    );
  }

  const isConnected = connected.includes(conn);

  return (
    <PageChrome
      title={conn.name}
      meta={isConnected ? (conn.needsAttention ? "needs attention" : "connected") : "not connected"}
    >
      <div className="conn-scroll">
        <div className="detail-col">
          <BackLink to="/connections" label="Connections" />

          <div className="detail-head">
            <span className={`badge lg${isConnected ? "" : " off"}`}>{conn.badge}</span>
            <div style={{ flex: 1, minWidth: 0 }}>
              <div className="name">{conn.name}</div>
              <div className="sub">
                {isConnected ? conn.sub : (conn.blurb ?? "Not connected yet.")}
              </div>
            </div>
            {isConnected && <StatusDot needsAttention={conn.needsAttention} />}
          </div>

          <AlertBanner conn={conn} />

          {conn.can && (
            <section>
              <div className="section-label">What Borg can do here</div>
              <div className="detail-list">
                {conn.can.map((line) => (
                  <div key={line} className="detail-list-row">
                    <span className="tick" />
                    <span>{line}</span>
                  </div>
                ))}
              </div>
            </section>
          )}

          {isConnected && conn.activity && (
            <section>
              <div className="section-label">Recently</div>
              <div className="detail-list">
                {conn.activity.map((a) => (
                  <div key={a.when + a.what} className="detail-activity-row">
                    <span className="when">{a.when}</span>
                    <span className="what">{a.what}</span>
                  </div>
                ))}
              </div>
            </section>
          )}

          <div className="detail-actions">
            {isConnected ? (
              <>
                <Pill>Pause</Pill>
                <Pill variant="danger">Disconnect</Pill>
              </>
            ) : (
              // No daemon yet — Connect just returns to the list until borgd is wired in.
              <Pill variant="primary" onClick={() => navigate("/connections")}>
                Connect {conn.name}
              </Pill>
            )}
          </div>
        </div>
      </div>
    </PageChrome>
  );
}
