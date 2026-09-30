import { Navigate, useParams } from "react-router-dom";
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
import { showNotice } from "../../lib/notice";
import { useMutation } from "../../lib/useDaemon";
import { badgeOf, useConnections, type ChannelConnection } from "./useConnections";

function ErrorLine({ error }: { error: BorgError | undefined }) {
  if (!error) return null;
  return (
    <div className="rounded-[10px] border border-alert bg-alert-soft px-4 py-3 text-[12.5px] leading-[1.45] text-ink2">
      {error.message}
    </div>
  );
}

function ChannelDetail({ conn, refetch }: { conn: ChannelConnection; refetch: () => void }) {
  const approve = useMutation(
    (borg, code: string) => borg.admin.approvePairing({ channelName: conn.name, code }),
    {
      onSuccess: (_data, code) => {
        refetch();
        const request = conn.pending.find((r) => r.code === code);
        showNotice(
          `Approved ${request ? request.displayName || request.senderId : "the sender"} on ${conn.name}`,
        );
      },
    },
  );
  const revoke = useMutation(
    (borg, senderId: string) =>
      borg.admin.revokeApprovedSender({ channelName: conn.name, senderId }),
    {
      onSuccess: (_data, senderId) => {
        refetch();
        const sender = conn.senders.find((s) => s.senderId === senderId);
        showNotice(`Revoked ${sender?.displayName || senderId} on ${conn.name}`);
      },
    },
  );

  return (
    <>
      <div className="flex items-center gap-4">
        <Badge on sizeClasses="h-10 w-10 rounded-xl text-[12.5px]">
          {badgeOf(conn.name)}
        </Badge>
        <div className="min-w-0 flex-1">
          <h2 className="m-0 text-[24px] leading-[1.2] font-light tracking-[-0.025em] text-ink">
            {conn.name}
          </h2>
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

/** One channel, up close: who may talk to Borg on it, and who is waiting. */
function ChannelPage({ name }: { name: string }) {
  const isMobile = useIsMobile();
  const connections = useConnections();

  return (
    <PageChrome title={name}>
      <div className={scrollCol(isMobile, "px-[38px] py-[34px]")}>
        <div className={DETAIL_COL}>
          <BackLink to="/connections" label="Connections" />
          <DaemonState query={connections} label="this connection">
            {(data) => {
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

/**
 * `/connections/:kind/:name`. Channels are the only kind left: a provider's
 * keys live under Settings → Provider now, and an old link still lands there.
 */
export function ConnectionDetailPage() {
  const { kind = "", name = "" } = useParams();
  if (kind === "provider") {
    return <Navigate to={`/settings/provider/${encodeURIComponent(name)}`} replace />;
  }
  return <ChannelPage name={name} />;
}
