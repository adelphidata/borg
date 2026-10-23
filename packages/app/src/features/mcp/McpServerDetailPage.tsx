import { useEffect, useRef, useState } from "react";
import { useNavigate, useParams } from "react-router-dom";
import {
  BackLink,
  DETAIL_COL,
  PageChrome,
  Pill,
  SECTION_LABEL,
  StatusDot,
  scrollCol,
  useIsMobile,
} from "@borg/ui";
import type { admin } from "@borg/client";
import { DaemonState } from "../../components/DaemonState";
import { Switch } from "../../components/Switch";
import { useMutation } from "../../lib/useDaemon";
import { McpServerForm, draftOf, specOf, type McpDraft } from "./McpServerForm";
import { stateTone, stateWords, useMcpServers } from "./useMcpServers";

const ERROR_BOX =
  "rounded-[10px] border border-alert bg-alert-soft px-4 py-3 text-[12.5px] leading-[1.45] text-ink2";

/** One-shot connection check, with the tools it found. */
function TestPanel({ name }: { name: string }) {
  const test = useMutation((borg) => borg.admin.testMcpServer({ name }));
  const result = test.data;
  return (
    <section>
      <div className={SECTION_LABEL}>Check the connection</div>
      <div className="flex flex-col gap-3">
        <div className="flex items-center gap-3">
          <Pill square disabled={test.pending} onClick={() => void test.run(undefined)}>
            {test.pending ? "Connecting…" : "Test connection"}
          </Pill>
        </div>
        {test.error && <div className={ERROR_BOX}>{test.error.message}</div>}
        {result && !result.ok && <div className={ERROR_BOX}>{result.error}</div>}
        {result?.ok && (
          <div className="rounded-[10px] border border-line2 bg-surface2 px-4 py-3">
            <div className="text-[12.5px] leading-[1.4] text-ink">
              Connected{result.serverInfo ? ` to ${result.serverInfo}` : ""} · {result.tools.length}{" "}
              tool{result.tools.length === 1 ? "" : "s"}
            </div>
            {result.tools.length > 0 && (
              <div className="mt-2 flex flex-wrap gap-1.5">
                {result.tools.map((t) => (
                  <span
                    key={t}
                    className="rounded-md border border-line2 px-2 py-1 font-mono text-[11px] leading-none text-ink2"
                  >
                    {t}
                  </span>
                ))}
              </div>
            )}
          </div>
        )}
      </div>
    </section>
  );
}

function Detail({
  entry,
  mcpEnabled,
  servers,
}: {
  entry: admin.McpServerEntry;
  mcpEnabled: boolean;
  servers: ReturnType<typeof useMcpServers>;
}) {
  const navigate = useNavigate();
  const [draft, setDraft] = useState<McpDraft>(() => draftOf(entry.name, entry.spec));
  const [dirty, setDirty] = useState(false);
  const [localError, setLocalError] = useState<string>();
  const [saving, setSaving] = useState(false);
  const [confirm, setConfirm] = useState(false);
  const enabled = entry.spec?.enabled ?? true;

  // A refetch that lands while nothing is being edited brings the stored
  // row back into the form; typed changes are never thrown away. Keyed on
  // the entry alone: clearing `dirty` after a save must not pull the row
  // that was just replaced back into the form while the refetch is in flight.
  const dirtyRef = useRef(false);
  const markDirty = (next: boolean) => {
    dirtyRef.current = next;
    setDirty(next);
  };
  useEffect(() => {
    if (!dirtyRef.current) setDraft(draftOf(entry.name, entry.spec));
  }, [entry]);

  const save = () => {
    const built = specOf(draft, enabled);
    if (built.error !== undefined) {
      setLocalError(built.error);
      return;
    }
    setLocalError(undefined);
    setSaving(true);
    void servers
      .save(entry.name, built.spec, true)
      .then((stored) => {
        if (stored === undefined) return;
        // The daemon's answer is the row as stored; show that until the
        // list catches up.
        setDraft(draftOf(entry.name, stored.spec));
        markDirty(false);
      })
      .finally(() => setSaving(false));
  };

  return (
    <>
      <div className="flex items-center gap-4">
        <div className="min-w-0 flex-1">
          <h2 className="m-0 truncate text-[24px] leading-[1.2] font-light tracking-[-0.025em] text-ink">
            {entry.name}
          </h2>
          <div className="mt-1 flex items-center gap-2 text-[12.5px] leading-[1.4] text-ink3">
            <StatusDot tone={enabled && mcpEnabled ? stateTone(entry) : "idle"} />
            {!mcpEnabled ? "MCP is turned off" : enabled ? stateWords(entry.state) : "Off"}
            {enabled && entry.toolCount > 0 && (
              <>
                {" "}
                · {entry.toolCount} tool{entry.toolCount === 1 ? "" : "s"}
              </>
            )}
          </div>
        </div>
        <Switch
          on={enabled}
          onChange={(next) => void servers.setEnabled(entry, next)}
          label={`Enable ${entry.name}`}
        />
      </div>

      <section>
        <div className={SECTION_LABEL}>Connection</div>
        <McpServerForm
          draft={draft}
          nameLocked
          onChange={(next) => {
            setDraft(next);
            markDirty(true);
          }}
        />
        {(localError || servers.error) && (
          <div role="alert" className={`mt-3 ${ERROR_BOX}`}>
            {localError ?? servers.error}
          </div>
        )}
        <div className="mt-4 flex items-center gap-2">
          <Pill square variant="primary" disabled={!dirty || saving} onClick={save}>
            {saving ? "Saving…" : "Save changes"}
          </Pill>
          {dirty && (
            <Pill
              square
              onClick={() => {
                setDraft(draftOf(entry.name, entry.spec));
                markDirty(false);
                setLocalError(undefined);
                servers.resetError();
              }}
            >
              Discard
            </Pill>
          )}
        </div>
      </section>

      <TestPanel name={entry.name} />

      <section>
        <div className={SECTION_LABEL}>Remove</div>
        {!confirm ? (
          <Pill square variant="danger" onClick={() => setConfirm(true)}>
            Remove this server
          </Pill>
        ) : (
          <div className="flex flex-wrap items-center gap-3 rounded-[10px] border border-alert bg-alert-soft px-4 py-3">
            <div className="min-w-0 flex-1 text-[12.5px] leading-[1.45] text-ink">
              Remove {entry.name}? Its tools disappear from every conversation.
            </div>
            <Pill square onClick={() => setConfirm(false)}>
              Keep
            </Pill>
            <Pill
              square
              variant="danger"
              onClick={() =>
                void servers
                  .remove(entry.name)
                  .then((r) => r !== undefined && navigate("/connections"))
              }
            >
              Remove
            </Pill>
          </div>
        )}
      </section>
    </>
  );
}

/** Edit one MCP server: its connection, a live check, and removal. */
export function McpServerDetailPage() {
  const isMobile = useIsMobile();
  const { name = "" } = useParams();
  const servers = useMcpServers([name]);

  return (
    <PageChrome title="Connections">
      <div className={scrollCol(isMobile, "px-[38px] py-[34px]")}>
        <div className={DETAIL_COL}>
          <BackLink to="/connections" label="Connections" />
          <DaemonState query={servers.query} label="this server">
            {(data) => {
              const entry = data.servers.find((s) => s.name === name);
              if (!entry) {
                return (
                  <div className="text-[13.5px] leading-[1.6] text-ink3">
                    There is no server called {name}.
                  </div>
                );
              }
              return <Detail entry={entry} mcpEnabled={data.mcpEnabled} servers={servers} />;
            }}
          </DaemonState>
        </div>
      </div>
    </PageChrome>
  );
}
