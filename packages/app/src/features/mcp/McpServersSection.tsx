import { useState } from "react";
import { useLocation, useNavigate } from "react-router-dom";
import { Pill, PlusIcon, SECTION_LABEL, StatusDot, TrashIcon } from "@borg/ui";
import type { admin } from "@borg/client";
import { DaemonState } from "../../components/DaemonState";
import { Switch } from "../../components/Switch";
import { McpServerForm, draftOf, specOf, type McpDraft } from "./McpServerForm";
import { stateTone, stateWords, useMcpServers, type McpServers } from "./useMcpServers";

/** What each template is for, so the chip says more than its id. */
const PRESET_BLURB: Record<string, string> = {
  time: "Clocks and time zones",
  filesystem: "Read and write a folder",
  github: "Issues, pull requests, repos",
  notion: "Notion pages and databases",
};

const ERROR_LINE = "text-[11.5px] leading-[1.4] text-alert";

function NewServer({
  presets,
  taken,
  servers,
  onClose,
}: {
  presets: string[];
  taken: Set<string>;
  servers: McpServers;
  onClose: () => void;
}) {
  const [draft, setDraft] = useState<McpDraft>(() => draftOf("", undefined));
  const [localError, setLocalError] = useState<string>();
  const [saving, setSaving] = useState(false);
  const [presetName, setPresetName] = useState<string>();

  const close = () => {
    onClose();
    setDraft(draftOf("", undefined));
    setLocalError(undefined);
    // A refusal belongs to the attempt that was just given up on.
    servers.resetError();
  };

  const submit = () => {
    const built = specOf(draft, true);
    if (built.error !== undefined) {
      setLocalError(built.error);
      return;
    }
    setLocalError(undefined);
    setSaving(true);
    void servers
      .save(draft.name.trim(), built.spec, false)
      .then((stored) => {
        // A refused save keeps everything typed so it can be fixed in place.
        if (stored !== undefined) close();
      })
      .finally(() => setSaving(false));
  };

  const available = presets.filter((p) => !taken.has(p));

  return (
    <div className="mb-4 flex flex-col gap-4 rounded-[10px] border border-line bg-surface2 p-4">
      {available.length > 0 && (
        <div className="flex flex-wrap items-center gap-2">
          <span className="text-[12px] leading-none text-ink3">Quick add</span>
          {available.map((p) => (
            <Pill
              key={p}
              disabled={presetName !== undefined}
              title={PRESET_BLURB[p]}
              onClick={() => {
                setPresetName(p);
                // The template's id is its name; a second copy needs a new
                // name, and that is an edit on the detail page.
                void servers
                  .addPreset(p, p)
                  .then((stored) => stored !== undefined && close())
                  .finally(() => setPresetName(undefined));
              }}
            >
              {presetName === p ? "Adding…" : p}
            </Pill>
          ))}
        </div>
      )}
      <McpServerForm draft={draft} onChange={setDraft} />
      {localError && (
        <div role="alert" className={ERROR_LINE}>
          {localError}
        </div>
      )}
      <div className="flex justify-end gap-2">
        <Pill square onClick={close}>
          Cancel
        </Pill>
        <Pill
          square
          variant="primary"
          disabled={saving || draft.name.trim() === ""}
          onClick={submit}
        >
          {saving ? "Adding…" : "Add server"}
        </Pill>
      </div>
    </div>
  );
}

function ServerRow({
  entry,
  mcpEnabled,
  onOpen,
  onToggle,
  onDelete,
}: {
  entry: admin.McpServerEntry;
  mcpEnabled: boolean;
  onOpen: () => void;
  onToggle: (enabled: boolean) => void;
  onDelete: () => void;
}) {
  const enabled = entry.spec?.enabled ?? true;
  const tools =
    entry.toolCount > 0 ? ` · ${entry.toolCount} tool${entry.toolCount === 1 ? "" : "s"}` : "";
  const status = !mcpEnabled ? "MCP is off" : enabled ? stateWords(entry.state) : "Off";
  return (
    <div className="group flex items-center gap-3 rounded-[10px] border border-line2 px-4 py-3">
      <StatusDot tone={enabled && mcpEnabled ? stateTone(entry) : "idle"} />
      <button
        type="button"
        onClick={onOpen}
        className="block min-w-0 flex-1 cursor-pointer border-0 bg-transparent p-0 text-left"
      >
        <div className="truncate text-[13.5px] leading-[1.35] text-ink">{entry.name}</div>
        <div className="mt-[3px] truncate font-mono text-[11px] leading-[1.4] text-ink3">
          {entry.detail}
        </div>
        <div className="mt-[3px] text-[11.5px] leading-[1.4] text-ink3">
          {status}
          {enabled && mcpEnabled && tools}
        </div>
      </button>
      <Switch on={enabled} onChange={onToggle} label={`Enable ${entry.name}`} />
      <button
        type="button"
        aria-label={`Delete ${entry.name}`}
        onClick={onDelete}
        className="flex h-7 w-7 cursor-pointer items-center justify-center rounded border-0 bg-transparent text-ink3 hover:text-alert"
      >
        <TrashIcon size={14} />
      </button>
    </div>
  );
}

/** The configured MCP servers, each with a switch and its state. One section of Connections. */
export function McpServersSection() {
  const location = useLocation();
  const navigate = useNavigate();
  const servers = useMcpServers([location.key]);
  const [confirm, setConfirm] = useState<string>();
  const [adding, setAdding] = useState(false);

  return (
    <section>
      <div className="mb-3 flex items-center justify-between gap-3">
        <div className={`${SECTION_LABEL} mb-0`}>MCP servers</div>
        {!adding && (
          <Pill square onClick={() => setAdding(true)}>
            <span className="flex items-center gap-1.5">
              <PlusIcon size={13} /> Add server
            </span>
          </Pill>
        )}
      </div>
      <DaemonState query={servers.query} label="your MCP servers">
        {(data) => (
          <>
            {!data.mcpEnabled && data.servers.length > 0 && (
              <div className="mb-3 flex items-center gap-3 text-[12.5px] leading-[1.45] text-ink2">
                <span className="min-w-0 flex-1">MCP is off.</span>
                <Pill square variant="primary" onClick={() => void servers.setMcpEnabled(true)}>
                  Turn on
                </Pill>
              </div>
            )}

            {adding && (
              <NewServer
                presets={data.presets}
                taken={new Set(data.servers.map((s) => s.name))}
                servers={servers}
                onClose={() => setAdding(false)}
              />
            )}

            {servers.error && (
              <div role="alert" className={`mb-3 ${ERROR_LINE}`}>
                {servers.error}
              </div>
            )}

            <div className="flex flex-col gap-2">
              {data.servers.length === 0 && !adding && (
                <div className="text-[13px] leading-[1.6] text-ink3">None yet.</div>
              )}
              {data.servers.map((entry) =>
                confirm === entry.name ? (
                  <div
                    key={entry.name}
                    className="flex flex-wrap items-center gap-3 rounded-[10px] border border-alert bg-alert-soft px-4 py-3"
                  >
                    <div className="min-w-0 flex-1 text-[12.5px] leading-[1.45] text-ink">
                      Remove {entry.name}?
                    </div>
                    <Pill square onClick={() => setConfirm(undefined)}>
                      Keep
                    </Pill>
                    <Pill
                      square
                      variant="danger"
                      onClick={() => {
                        setConfirm(undefined);
                        void servers.remove(entry.name);
                      }}
                    >
                      Remove
                    </Pill>
                  </div>
                ) : (
                  <ServerRow
                    key={entry.name}
                    entry={entry}
                    mcpEnabled={data.mcpEnabled}
                    onOpen={() => navigate(`/connections/mcp/${encodeURIComponent(entry.name)}`)}
                    onToggle={(enabled) => void servers.setEnabled(entry, enabled)}
                    onDelete={() => setConfirm(entry.name)}
                  />
                ),
              )}
            </div>
          </>
        )}
      </DaemonState>
    </section>
  );
}
