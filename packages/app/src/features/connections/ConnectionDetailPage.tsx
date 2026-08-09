import { useNavigate, useParams } from "react-router-dom";
import {
  AlertBanner,
  BackLink,
  Badge,
  DETAIL_COL,
  PageChrome,
  Pill,
  SECTION_LABEL,
  StatusDot,
  scrollCol,
  useIsMobile,
} from "@borg/ui";
import { connected, findConnection } from "../../data/mock";

/**
 * One connection, up close. Connected services show status + recent activity;
 * available ones show the pitch and a Connect button (mock until borgd lands).
 */
export function ConnectionDetailPage() {
  const { id = "" } = useParams();
  const navigate = useNavigate();
  const isMobile = useIsMobile();
  const conn = findConnection(id);
  const scroll = scrollCol(isMobile, "px-[38px] py-[34px]");

  if (!conn) {
    return (
      <PageChrome title="Connections">
        <div className={scroll}>
          <div className={DETAIL_COL}>
            <BackLink to="/connections" label="Connections" />
            <div className="text-[14.5px] leading-[1.62] text-pretty text-ink2">
              That connection doesn’t exist.
            </div>
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
      <div className={scroll}>
        <div className={DETAIL_COL}>
          <BackLink to="/connections" label="Connections" />

          <div className="flex items-center gap-4">
            <Badge on={isConnected} sizeClasses="h-10 w-10 rounded-xl text-[12.5px]">
              {conn.badge}
            </Badge>
            <div style={{ flex: 1, minWidth: 0 }}>
              <div className="text-[24px] leading-[1.2] font-light tracking-[-0.025em] text-ink">
                {conn.name}
              </div>
              <div className="mt-1 text-[12.5px] leading-[1.4] text-ink3">
                {isConnected ? conn.sub : (conn.blurb ?? "Not connected yet.")}
              </div>
            </div>
            {isConnected && <StatusDot needsAttention={conn.needsAttention} />}
          </div>

          <AlertBanner alert={conn.alert} />

          {conn.can && (
            <section>
              <div className={SECTION_LABEL}>What Borg can do here</div>
              <div className="flex flex-col">
                {conn.can.map((line) => (
                  <div
                    key={line}
                    className="flex items-center gap-[13px] border-b border-line2 px-0.5 py-3 text-[13.5px] leading-[1.45] text-ink"
                  >
                    <span className="h-[5px] w-[5px] flex-none rounded-full bg-neon" />
                    <span>{line}</span>
                  </div>
                ))}
              </div>
            </section>
          )}

          {isConnected && conn.activity && (
            <section>
              <div className={SECTION_LABEL}>Recently</div>
              <div className="flex flex-col">
                {conn.activity.map((a) => (
                  <div
                    key={a.when + a.what}
                    className="flex items-baseline gap-4 border-b border-line2 px-0.5 py-3"
                  >
                    <span className="w-[140px] flex-none text-[11.5px] leading-[1.4] text-ink3">
                      {a.when}
                    </span>
                    <span className="text-[13.5px] leading-[1.45] text-ink2">{a.what}</span>
                  </div>
                ))}
              </div>
            </section>
          )}

          <div className="flex gap-[9px]">
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
