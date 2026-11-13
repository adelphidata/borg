import { useLocation, useNavigate } from "react-router-dom";
import { AlertBanner, MobileTitle, PageChrome, scrollCol, useIsMobile } from "@borg/ui";
import { DaemonState } from "../../components/DaemonState";
import { PageGroup } from "../../components/PageGroup";
import { McpServersSection } from "../mcp/McpServersSection";
import { SkillsSection } from "../skills/SkillsSection";
import { SubscriptionsSection, subscriptionPath } from "../subscriptions/SubscriptionsSection";
import { attentionAlert, useSubscriptions } from "../subscriptions/useSubscriptions";
import { badgeOf, useConnections, type ChannelConnection } from "./useConnections";
import { CalendarSection } from "./CalendarSection";
import { ConnCards, ConnList, type Card } from "./connectionCards";

function cardOf(conn: ChannelConnection): Card {
  return {
    id: `channel/${conn.name}`,
    badge: badgeOf(conn.name),
    name: conn.name,
    sub:
      conn.pending.length > 0
        ? `${conn.pending.length} waiting for approval`
        : `${conn.senders.length} approved sender${conn.senders.length === 1 ? "" : "s"}`,
    tone: conn.pending.length > 0 ? "alert" : "ok",
  };
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
          <CalendarSection />
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
