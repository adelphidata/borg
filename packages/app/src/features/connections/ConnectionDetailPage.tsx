import { useState } from "react";
import { useParams } from "react-router-dom";
import {
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
import type { BorgError } from "@borg/client";
import { DaemonState } from "../../components/DaemonState";
import { useMutation } from "../../lib/useDaemon";
import type { admin } from "@borg/client";
import {
  badgeOf,
  providerLabel,
  useConnections,
  type ChannelConnection,
  type ProviderConnection,
} from "./useConnections";

const STRATEGIES = ["fill_first", "round_robin", "least_used", "random"];

function ErrorLine({ error }: { error: BorgError | undefined }) {
  if (!error) return null;
  return (
    <div className="rounded-[10px] border border-alert bg-alert-soft px-4 py-3 text-[12.5px] leading-[1.45] text-ink2">
      {error.message}
    </div>
  );
}

function ProviderDetail({
  conn,
  configured,
  catalog,
  refetch,
}: {
  conn: ProviderConnection | undefined;
  /** Provider name from the route; used when no credentials exist yet. */
  configured: string;
  catalog: admin.ProviderChoice[];
  refetch: () => void;
}) {
  const name = conn?.name ?? configured;
  const [draftKey, setDraftKey] = useState("");
  // SetApiKey persists; AddCredential entries are runtime-only by design.
  const saveKey = useMutation(
    (borg, key: string) => borg.admin.setApiKey({ provider: name, literal: key }),
    { onSuccess: refetch },
  );
  const forgetKey = useMutation((borg) => borg.admin.unsetApiKey({ provider: name }), {
    onSuccess: refetch,
  });
  const remove = useMutation(
    (borg, target: string) => borg.admin.removeCredential({ provider: name, target }),
    { onSuccess: refetch },
  );
  const setStrategy = useMutation(
    (borg, strategy: string) => borg.admin.setCredentialStrategy({ provider: name, strategy }),
    { onSuccess: refetch },
  );

  const entries = (conn?.entries ?? []).filter((e) => e.status !== "removed");
  const saved = conn?.apiKeyBackend;
  const isConnected = entries.length > 0 || saved !== undefined;
  const blurb = catalog.find((p) => p.id === name)?.blurb;

  const submitKey = () => {
    const key = draftKey.trim();
    if (!key || saveKey.pending) return;
    void saveKey.run(key).then((r) => r !== undefined && setDraftKey(""));
  };

  return (
    <>
      <div className="flex items-center gap-4">
        <Badge on={isConnected} sizeClasses="h-10 w-10 rounded-xl text-[12.5px]">
          {badgeOf(name)}
        </Badge>
        <div className="min-w-0 flex-1">
          <div className="text-[24px] leading-[1.2] font-light tracking-[-0.025em] text-ink">
            {providerLabel(catalog, name)}
          </div>
          <div className="mt-1 text-[12.5px] leading-[1.4] text-ink3">
            {isConnected ? "Connected" : (blurb ?? "Not connected yet.")}
          </div>
        </div>
        {isConnected && (
          <StatusDot
            tone={entries.length > 0 && entries.every((e) => e.status !== "ok") ? "alert" : "ok"}
          />
        )}
      </div>

      <ErrorLine error={saveKey.error ?? forgetKey.error ?? remove.error ?? setStrategy.error} />

      {saved && (
        <section>
          <div className={SECTION_LABEL}>Saved key</div>
          <div className="flex items-center gap-[13px] border-b border-line2 px-0.5 py-3">
            <span className="h-[7px] w-[7px] flex-none rounded-full bg-neon" />
            <div className="min-w-0 flex-1">
              <div className="text-[13.5px] leading-[1.3] font-medium text-ink">
                {saved === "keychain" ? "Stored in the keychain" : "Stored with the daemon"}
              </div>
            </div>
            <Pill
              variant="danger"
              onClick={() => !forgetKey.pending && void forgetKey.run(undefined)}
            >
              Forget
            </Pill>
          </div>
        </section>
      )}

      {entries.length > 0 && (
        <section>
          <div className={SECTION_LABEL}>Active key pool</div>
          <div className="flex flex-col">
            {entries.map((e) => (
              <div
                key={e.id}
                className="flex items-center gap-[13px] border-b border-line2 px-0.5 py-3"
              >
                <span
                  className={`h-[7px] w-[7px] flex-none rounded-full ${
                    e.status === "ok" ? "bg-neon" : "bg-alert"
                  }`}
                />
                <div className="min-w-0 flex-1">
                  <div className="text-[13.5px] leading-[1.3] font-medium text-ink">{e.label}</div>
                  <div className="mt-[3px] text-[11.5px] leading-[1.4] text-ink3">
                    {e.fingerprint} · used {e.useCount} time{e.useCount === 1 ? "" : "s"}
                    {e.status !== "ok" && ` · ${e.status}`}
                    {e.sourceKind !== "manual" && ` · from ${e.sourceValue || e.sourceKind}`}
                  </div>
                </div>
                <Pill variant="danger" onClick={() => !remove.pending && void remove.run(e.id)}>
                  Remove
                </Pill>
              </div>
            ))}
          </div>
        </section>
      )}

      {entries.length > 1 && conn && (
        <section>
          <div className={SECTION_LABEL}>Rotation</div>
          <div className="flex flex-wrap gap-2">
            {STRATEGIES.map((s) => (
              <Pill
                key={s}
                variant={conn.strategy === s ? "primary" : undefined}
                onClick={() =>
                  conn.strategy !== s && !setStrategy.pending && void setStrategy.run(s)
                }
              >
                {s.replace(/_/g, " ")}
              </Pill>
            ))}
          </div>
        </section>
      )}

      <section>
        <div className={SECTION_LABEL}>{saved ? "Replace the key" : "Add a key"}</div>
        <div className="flex items-center gap-2.5">
          <input
            type="password"
            className="min-w-0 flex-1 rounded-[10px] border border-line2 bg-surface2 px-4 py-3 text-[13.5px] leading-none text-ink outline-none placeholder:text-ink3 focus:border-neon"
            placeholder="API key"
            value={draftKey}
            onChange={(e) => setDraftKey(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && submitKey()}
          />
          <Pill variant="primary" onClick={submitKey}>
            {saveKey.pending ? "Saving…" : "Save"}
          </Pill>
        </div>
        <div className="mt-3 text-[12px] leading-[1.5] text-ink3">
          The key goes straight to the daemon and is never shown again.
        </div>
      </section>
    </>
  );
}

function ChannelDetail({ conn, refetch }: { conn: ChannelConnection; refetch: () => void }) {
  const approve = useMutation(
    (borg, code: string) => borg.admin.approvePairing({ channelName: conn.name, code }),
    { onSuccess: refetch },
  );
  const revoke = useMutation(
    (borg, senderId: string) =>
      borg.admin.revokeApprovedSender({ channelName: conn.name, senderId }),
    { onSuccess: refetch },
  );

  return (
    <>
      <div className="flex items-center gap-4">
        <Badge on sizeClasses="h-10 w-10 rounded-xl text-[12.5px]">
          {badgeOf(conn.name)}
        </Badge>
        <div className="min-w-0 flex-1">
          <div className="text-[24px] leading-[1.2] font-light tracking-[-0.025em] text-ink">
            {conn.name}
          </div>
          <div className="mt-1 text-[12.5px] leading-[1.4] text-ink3">
            {conn.senders.length} approved sender{conn.senders.length === 1 ? "" : "s"}
          </div>
        </div>
        <StatusDot tone={conn.pending.length > 0 ? "alert" : "ok"} />
      </div>

      <ErrorLine error={approve.error ?? revoke.error} />

      {conn.pending.length > 0 && (
        <section>
          <div className={SECTION_LABEL}>Waiting for approval</div>
          <div className="flex flex-col">
            {conn.pending.map((r) => (
              <div
                key={r.id}
                className="flex items-center gap-[13px] border-b border-line2 px-0.5 py-3"
              >
                <div className="min-w-0 flex-1">
                  <div className="text-[13.5px] leading-[1.3] font-medium text-ink">
                    {r.displayName || r.senderId}
                  </div>
                  <div className="mt-[3px] text-[11.5px] leading-[1.4] text-ink3">
                    code {r.code}
                  </div>
                </div>
                <Pill
                  variant="primary"
                  onClick={() => !approve.pending && void approve.run(r.code)}
                >
                  Approve
                </Pill>
              </div>
            ))}
          </div>
        </section>
      )}

      <section>
        <div className={SECTION_LABEL}>Approved senders</div>
        {conn.senders.length === 0 ? (
          <div className="text-[13px] leading-[1.6] text-ink3">No one is approved yet.</div>
        ) : (
          <div className="flex flex-col">
            {conn.senders.map((s) => (
              <div
                key={s.senderId}
                className="flex items-center gap-[13px] border-b border-line2 px-0.5 py-3"
              >
                <div className="min-w-0 flex-1">
                  <div className="text-[13.5px] leading-[1.3] font-medium text-ink">
                    {s.displayName || s.senderId}
                  </div>
                  <div className="mt-[3px] text-[11.5px] leading-[1.4] text-ink3">{s.senderId}</div>
                </div>
                <Pill
                  variant="danger"
                  onClick={() => !revoke.pending && void revoke.run(s.senderId)}
                >
                  Revoke
                </Pill>
              </div>
            ))}
          </div>
        )}
      </section>
    </>
  );
}

/** One connection, up close: a provider's key pool or a channel's senders. */
export function ConnectionDetailPage() {
  const { kind = "", name = "" } = useParams();
  const isMobile = useIsMobile();
  const connections = useConnections();
  const scroll = scrollCol(isMobile, "px-[38px] py-[34px]");

  return (
    <PageChrome
      title={
        kind === "provider" && connections.state === "ready"
          ? providerLabel(connections.data.catalog, name)
          : name
      }
    >
      <div className={scroll}>
        <div className={DETAIL_COL}>
          <BackLink to="/connections" label="Connections" />
          <DaemonState query={connections} label="this connection">
            {(data) => {
              if (kind === "provider") {
                const conn = data.providers.find((p) => p.name === name);
                return (
                  <ProviderDetail
                    conn={conn}
                    configured={name}
                    catalog={data.catalog}
                    refetch={connections.refetch}
                  />
                );
              }
              const channel = data.channels.find((c) => c.name === name);
              if (!channel) {
                return (
                  <div className="text-[14.5px] leading-[1.62] text-pretty text-ink2">
                    That connection doesn’t exist.
                  </div>
                );
              }
              return <ChannelDetail conn={channel} refetch={connections.refetch} />;
            }}
          </DaemonState>
        </div>
      </div>
    </PageChrome>
  );
}
