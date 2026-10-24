import { useLocation, useNavigate } from "react-router-dom";
import {
  AlertBanner,
  Badge,
  MobileTitle,
  PageChrome,
  StatusDot,
  scrollCol,
  useIsMobile,
} from "@borg/ui";
import { DaemonState } from "../../components/DaemonState";
import { PageGroup } from "../../components/PageGroup";
import { McpServersSection } from "../mcp/McpServersSection";
import { SkillsSection } from "../skills/SkillsSection";
import { SubscriptionsSection, subscriptionPath } from "../subscriptions/SubscriptionsSection";
import { attentionAlert, useSubscriptions } from "../subscriptions/useSubscriptions";
import { badgeOf, useConnections, type ChannelConnection } from "./useConnections";

interface Card {
  id: string;
  badge: string;
  name: string;
  sub: string;
  needsAttention: boolean;
}

function cardOf(conn: ChannelConnection): Card {
  return {
    id: `channel/${conn.name}`,
    badge: badgeOf(conn.name),
    name: conn.name,
    sub:
      conn.pending.length > 0
        ? `${conn.pending.length} waiting for approval`
        : `${conn.senders.length} approved sender${conn.senders.length === 1 ? "" : "s"}`,
    needsAttention: conn.pending.length > 0,
  };
}

/** Desktop: channels as a card grid. */
function ConnCards({ cards, open }: { cards: Card[]; open: (id: string) => void }) {
  return (
    <section>
      <div className="grid grid-cols-4 gap-3">
        {cards.map((c) => (
          <button
            type="button"
            key={c.id}
            className="cursor-pointer rounded-[11px] border border-line bg-surface2 p-4 text-left"
            onClick={() => open(c.id)}
          >
            <div className="mb-[11px] flex items-center justify-between">
              <Badge>{c.badge}</Badge>
              <StatusDot tone={c.needsAttention ? "alert" : "ok"} />
            </div>
            <div className="text-[13.5px] leading-[1.2] font-medium text-ink">{c.name}</div>
            <div className="mt-[5px] text-[11.5px] leading-[1.4] text-ink3">{c.sub}</div>
          </button>
        ))}
      </div>
    </section>
  );
}

/** Mobile: channels as list rows. */
function ConnList({ cards, open }: { cards: Card[]; open: (id: string) => void }) {
  return (
    <div className="flex flex-col">
      {cards.map((c) => (
        <button
          type="button"
          key={c.id}
          className="flex w-full cursor-pointer items-center gap-[13px] border-b border-line2 py-3.5 text-left"
          onClick={() => open(c.id)}
        >
          <Badge sizeClasses="h-[30px] w-[30px] rounded-[9px] text-[10.5px]">{c.badge}</Badge>
          <div className="min-w-0 flex-1">
            <div className="text-[13.5px] leading-[1.25] font-medium text-ink">{c.name}</div>
            <div className="mt-[3px] text-[11.5px] leading-[1.3] text-ink3">{c.sub}</div>
          </div>
          <StatusDot tone={c.needsAttention ? "alert" : "ok"} />
        </button>
      ))}
    </div>
  );
}

/**
 * What Borg is hooked up to beyond its model: channels, the subscriptions
 * that wake it, MCP servers and skills.
 */
export function ConnectionsPage() {
  const isMobile = useIsMobile();
  const navigate = useNavigate();
  const location = useLocation();
  const open = (id: string) => navigate(`/connections/${id}`);
  const connections = useConnections();
  const subs = useSubscriptions([location.key]);
  const subAlert =
    subs.query.state === "ready" ? attentionAlert(subs.query.data.subscriptions) : undefined;
  const meta = (() => {
    if (connections.state !== "ready") return undefined;
    const { channels, pendingCount } = connections.data;
    const parts = [
      channels.length > 0 && `${channels.length} channel${channels.length === 1 ? "" : "s"}`,
      pendingCount > 0 && `${pendingCount} needs attention`,
    ].filter(Boolean);
    return parts.length > 0 ? parts.join(" · ") : undefined;
  })();

  return (
    <PageChrome title="Connections" meta={meta}>
      <div className={scrollCol(isMobile, "px-[38px] py-[34px]")}>
        <div className="flex max-w-[860px] flex-col gap-8">
          <MobileTitle>Connections</MobileTitle>
          <DaemonState query={connections} label="connections">
            {(data) => {
              const cards = data.channels.map(cardOf);
              const alerted = data.channels.find((c) => c.pending.length > 0);
              return (
                <>
                  {/* One banner at a time: a person waiting outranks a
                      subscription, whose row stays red either way. */}
                  {!alerted && subAlert && (
                    <AlertBanner
                      alert={subAlert.alert}
                      stacked={isMobile}
                      onAction={() => navigate(subscriptionPath(subAlert.sub.id))}
                    />
                  )}
                  {alerted && (
                    <AlertBanner
                      alert={{
                        title: `${alerted.name} has ${alerted.pending.length} pairing request${alerted.pending.length === 1 ? "" : "s"} waiting`,
                        stackedTitle: `${alerted.name} needs you`,
                        sub: "Someone asked to talk to Borg on this channel. Approve or ignore them.",
                        action: "Review",
                      }}
                      stacked={isMobile}
                      onAction={() => open(`channel/${alerted.name}`)}
                    />
                  )}
                  {cards.length > 0 && (
                    <PageGroup title="Channels">
                      {isMobile ? (
                        <ConnList cards={cards} open={open} />
                      ) : (
                        <ConnCards cards={cards} open={open} />
                      )}
                    </PageGroup>
                  )}
                </>
              );
            }}
          </DaemonState>
          <SubscriptionsSection subs={subs} />
          {/* Tools and playbooks Borg reaches for sit with the channels it
              answers on: one page for everything it is hooked up to. */}
          <McpServersSection />
          <SkillsSection />
        </div>
      </div>
    </PageChrome>
  );
}
