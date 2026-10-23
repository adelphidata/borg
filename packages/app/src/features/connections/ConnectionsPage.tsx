import { useNavigate } from "react-router-dom";
import {
  AlertBanner,
  Badge,
  MobileTitle,
  PageChrome,
  SECTION_LABEL,
  StatusDot,
  scrollCol,
  useIsMobile,
} from "@borg/ui";
import { DaemonState } from "../../components/DaemonState";
import { useMutation } from "../../lib/useDaemon";
import { McpServersSection } from "../mcp/McpServersSection";
import { SkillsSection } from "../skills/SkillsSection";
import type { admin } from "@borg/client";
import {
  PAIR_SOURCES,
  badgeOf,
  providerLabel,
  useConnections,
  type ChannelConnection,
  type Connections,
  type ProviderConnection,
} from "./useConnections";

interface Card {
  id: string;
  badge: string;
  name: string;
  sub: string;
  needsAttention: boolean;
}

function cardOf(
  conn: ProviderConnection | ChannelConnection,
  catalog: admin.ProviderChoice[],
): Card {
  if (conn.kind === "provider") {
    const live = conn.entries.filter((e) => e.status !== "removed").length;
    return {
      id: `provider/${conn.name}`,
      badge: badgeOf(conn.name),
      name: providerLabel(catalog, conn.name),
      sub:
        live > 0
          ? `${live} key${live === 1 ? "" : "s"} · ${conn.strategy.replace(/_/g, " ")}`
          : conn.apiKeyBackend
            ? "key saved"
            : conn.foundAt
              ? "key found on this computer"
              : "no working keys",
      needsAttention: live > 0 && conn.entries.every((e) => e.status !== "ok"),
    };
  }
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

/** Desktop: connected services as a card grid. */
function ConnCards({ cards, open }: { cards: Card[]; open: (id: string) => void }) {
  return (
    <section>
      <div className={SECTION_LABEL}>Connected</div>
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

/** Mobile: connected services as list rows. */
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

function AddGrid({ data, open }: { data: Connections; open: (id: string) => void }) {
  const isMobile = useIsMobile();
  const configured = new Set(data.providers.map((p) => p.name));
  // The detail page behind each tile asks for a key. A provider that takes
  // no key (Claude Code, Bedrock) has nothing to paste, so it does not
  // belong here — it is chosen in Settings instead.
  const available = data.catalog.filter((p) => p.needsApiKey && !configured.has(p.id));
  if (available.length === 0) return null;
  return (
    <section>
      <div className={SECTION_LABEL}>Add a model provider</div>
      <div className={`grid gap-3 ${isMobile ? "grid-cols-2" : "grid-cols-4"}`}>
        {available.map((p) => (
          <button
            type="button"
            key={p.id}
            className="flex cursor-pointer items-center gap-3 rounded-[11px] border border-line2 px-4 py-3.5 text-left transition-[border-color] duration-150 hover:border-neon"
            onClick={() => open(`provider/${p.id}`)}
          >
            <Badge on={false}>{badgeOf(p.id)}</Badge>
            <span className="text-[13px] leading-[1.2] font-medium text-ink2">{p.label}</span>
          </button>
        ))}
      </div>
    </section>
  );
}

/** Import OAuth sign-ins from coding tools already on this machine. */
function PairRow({ refetch }: { refetch: () => void }) {
  const pair = useMutation((borg, kind: string) => borg.admin.pairCredential({ kind }), {
    onSuccess: refetch,
  });

  return (
    <section>
      <div className={SECTION_LABEL}>Import a sign-in</div>
      <div className="flex flex-wrap items-center gap-2">
        {PAIR_SOURCES.map((s) => (
          <button
            key={s.kind}
            type="button"
            disabled={pair.pending}
            className="cursor-pointer rounded-full border border-line2 bg-transparent px-4 py-2 text-[12.5px] leading-none text-ink2 transition-[border-color] duration-150 hover:border-neon disabled:opacity-60"
            onClick={() => void pair.run(s.kind)}
          >
            {s.label}
          </button>
        ))}
      </div>
      {pair.error && (
        <div className="mt-3 text-[12.5px] leading-[1.45] text-ink2">{pair.error.message}</div>
      )}
      {pair.data && (
        <div className="mt-3 text-[12.5px] leading-[1.45] text-ink2">
          {pair.data.entries.length === 0
            ? "Nothing found on this machine — sign in with that tool first."
            : `Imported ${pair.data.entries.length} account${pair.data.entries.length === 1 ? "" : "s"}.`}
        </div>
      )}
    </section>
  );
}

export function ConnectionsPage() {
  const isMobile = useIsMobile();
  const navigate = useNavigate();
  const open = (id: string) => navigate(`/connections/${id}`);
  const connections = useConnections();

  return (
    <PageChrome
      title="Connections"
      meta={
        connections.state === "ready"
          ? `${connections.data.providers.length + connections.data.channels.length} connected` +
            (connections.data.pendingCount > 0
              ? ` · ${connections.data.pendingCount} needs attention`
              : "")
          : undefined
      }
    >
      <div className={scrollCol(isMobile, "px-[38px] py-[34px]")}>
        <div className="flex max-w-[860px] flex-col gap-[30px]">
          <MobileTitle>Connections</MobileTitle>
          <DaemonState query={connections} label="connections">
            {(data) => {
              const cards = [...data.providers, ...data.channels].map((c) =>
                cardOf(c, data.catalog),
              );
              const alerted = data.channels.find((c) => c.pending.length > 0);
              return (
                <>
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
                  {cards.length > 0 ? (
                    isMobile ? (
                      <ConnList cards={cards} open={open} />
                    ) : (
                      <ConnCards cards={cards} open={open} />
                    )
                  ) : (
                    <div className="text-[14.5px] leading-[1.62] text-pretty text-ink2">
                      Nothing is connected yet. Add a model provider below, or import a sign-in from
                      a tool you already use.
                    </div>
                  )}
                  <AddGrid data={data} open={open} />
                  <PairRow refetch={connections.refetch} />
                </>
              );
            }}
          </DaemonState>
          {/* Tools and playbooks Borg reaches for sit with the services it
              talks to: one page for everything it is hooked up to. */}
          <McpServersSection />
          <SkillsSection />
        </div>
      </div>
    </PageChrome>
  );
}
