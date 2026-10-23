import { useState } from "react";
import { useParams } from "react-router-dom";
import {
  BackLink,
  Badge,
  DETAIL_COL,
  PageChrome,
  Pill,
  SECTION_LABEL,
  SETTINGS_SCROLL_DESKTOP_PADDING,
  StatusDot,
  TEXT_FIELD,
  isSubmitEnter,
  scrollCol,
  useIsMobile,
} from "@borg/ui";
import type { BorgError } from "@borg/client";
import type { admin } from "@borg/client";
import { DaemonState } from "../../components/DaemonState";
import { useMutation } from "../../lib/useDaemon";
import { badgeOf } from "../connections/useConnections";
import { providerLabel } from "../onboarding/useSetupStatus";
import { keysFailing, liveEntries, useProviderKeys, type ProviderKeys } from "./providerKeys";

const STRATEGIES = ["fill_first", "round_robin", "least_used", "random"];

/**
 * A pool entry in words. A key from config is named by its slot
 * (`llm.api_keys[0]`) — a setting path, not a name — so it reads "Key 1".
 * Where it came from is worth saying only when that is something the user
 * set up themselves, like an environment variable.
 */
export function describeKey(
  e: Pick<admin.CredentialEntry, "label" | "sourceKind" | "sourceValue">,
): {
  name: string;
  from: string | undefined;
} {
  const slot = /^llm\.api_keys\[(\d+)\]$/.exec(e.label);
  const name = slot ? `Key ${Number(slot[1]) + 1}` : e.label;
  if (e.sourceKind === "manual" || e.sourceKind === "config") return { name, from: undefined };
  return { name, from: e.sourceValue || e.sourceKind };
}

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
  conn: ProviderKeys | undefined;
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

  const entries = liveEntries(conn);
  const saved = conn?.apiKeyBackend;
  const foundAt = conn?.foundAt;
  const isConnected = entries.length > 0 || saved !== undefined || foundAt !== undefined;

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
          <h2 className="m-0 text-[24px] leading-[1.2] font-light tracking-[-0.025em] text-ink">
            {providerLabel(catalog, name)}
          </h2>
          <div className="mt-1 text-[12.5px] leading-[1.4] text-ink3">
            {isConnected ? "Connected" : "Not connected yet."}
          </div>
        </div>
        {isConnected && <StatusDot tone={keysFailing(conn) ? "alert" : "ok"} />}
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

      {foundAt && !saved && entries.length === 0 && (
        <section>
          <div className={SECTION_LABEL}>Key in use</div>
          <div className="text-[13px] leading-[1.55] text-pretty text-ink2">
            Borg is using the key it found in <span className="break-all text-ink">{foundAt}</span>.
          </div>
        </section>
      )}

      {entries.length > 0 && (
        <section>
          <div className={SECTION_LABEL}>Active key pool</div>
          <div className="flex flex-col">
            {entries.map((e) => {
              const key = describeKey(e);
              return (
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
                    <div className="text-[13.5px] leading-[1.3] font-medium text-ink">
                      {key.name}
                    </div>
                    <div className="mt-[3px] text-[11.5px] leading-[1.4] text-ink3">
                      {e.fingerprint} · used {e.useCount} time{e.useCount === 1 ? "" : "s"}
                      {e.status !== "ok" && ` · ${e.status}`}
                      {key.from && ` · from ${key.from}`}
                    </div>
                  </div>
                  <Pill variant="danger" onClick={() => !remove.pending && void remove.run(e.id)}>
                    Remove
                  </Pill>
                </div>
              );
            })}
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
        <div className={SECTION_LABEL}>{saved || foundAt ? "Replace the key" : "Add a key"}</div>
        <div className="flex items-center gap-2.5">
          <input
            type="password"
            className={`min-w-0 flex-1 ${TEXT_FIELD}`}
            placeholder="API key"
            aria-label="API key"
            value={draftKey}
            onChange={(e) => setDraftKey(e.target.value)}
            onKeyDown={(e) => isSubmitEnter(e) && submitKey()}
          />
          <Pill variant="primary" onClick={submitKey}>
            {saveKey.pending ? "Saving…" : "Save"}
          </Pill>
        </div>
      </section>
    </>
  );
}

/** One provider's keys, up close: saved, found, pooled, rotated. Under Settings → Provider. */
export function ProviderKeysPage() {
  const { name = "" } = useParams();
  const isMobile = useIsMobile();
  const keys = useProviderKeys();

  return (
    <PageChrome title={keys.state === "ready" ? providerLabel(keys.data.catalog, name) : name}>
      <div className={scrollCol(isMobile, SETTINGS_SCROLL_DESKTOP_PADDING)}>
        <div className={DETAIL_COL}>
          <BackLink to="/settings/provider" label="Provider" />
          <DaemonState query={keys} label="this provider's keys">
            {(data) => (
              <ProviderDetail
                conn={data.providers.find((p) => p.name === name)}
                configured={name}
                catalog={data.catalog}
                refetch={keys.refetch}
              />
            )}
          </DaemonState>
        </div>
      </div>
    </PageChrome>
  );
}
