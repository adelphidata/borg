import { useState } from "react";
import { Pill, TEXT_FIELD } from "@borg/ui";
import type { admin } from "@borg/client";
import {
  TRANSPORTS,
  formatKeyValues,
  joinCommandLine,
  parseKeyValues,
  splitCommandLine,
  type Transport,
} from "./useMcpServers";

const TRANSPORT_LABEL: Record<Transport, string> = {
  stdio: "Command",
  http: "URL",
  sse: "URL (SSE)",
};

const FIELD = `${TEXT_FIELD} w-full select-text`;
const AREA =
  "w-full resize-y rounded-[10px] border border-field bg-surface2 px-4 py-3 font-mono text-[12.5px] leading-[1.5] text-ink outline-none select-text placeholder:text-ink3 focus:border-focus";
const LABEL = "mb-1.5 block text-[11.5px] leading-none text-ink3";
const HINT = "mt-1.5 px-1 text-[11px] leading-[1.4] text-ink3";

/** What the form edits — strings throughout, converted on submit. */
export interface McpDraft {
  name: string;
  transport: Transport;
  commandLine: string;
  env: string;
  url: string;
  bearerToken: string;
  headers: string;
  clientCert: string;
  clientKey: string;
  caCert: string;
  connectTimeoutMs: string;
  callTimeoutMs: string;
  rateLimitPerMinute: string;
  rateLimitBurst: string;
  maxTools: string;
}

export function draftOf(name: string, spec: admin.McpServerSpec | undefined): McpDraft {
  const s = spec;
  const knob = (v: number | undefined) => (v && v > 0 ? String(v) : "");
  return {
    name,
    transport: (TRANSPORTS as readonly string[]).includes(s?.transport ?? "")
      ? (s!.transport as Transport)
      : "stdio",
    commandLine: s ? joinCommandLine([s.command, ...s.args].filter((p) => p !== "")) : "",
    env: formatKeyValues(s?.env ?? {}),
    url: s?.url ?? "",
    bearerToken: s?.bearerToken ?? "",
    headers: formatKeyValues(s?.headers ?? {}),
    clientCert: s?.clientCert ?? "",
    clientKey: s?.clientKey ?? "",
    caCert: s?.caCert ?? "",
    connectTimeoutMs: knob(s?.connectTimeoutMs),
    callTimeoutMs: knob(s?.callTimeoutMs),
    rateLimitPerMinute: knob(s?.rateLimitPerMinute),
    rateLimitBurst: knob(s?.rateLimitBurst),
    maxTools: knob(s?.maxTools),
  };
}

/**
 * Turns the draft into the wire spec, or names what is wrong with it. Only
 * the checks that need no daemon live here: the daemon's own validation is
 * still the final word and its message is shown the same way.
 */
export function specOf(
  draft: McpDraft,
  enabled: boolean,
): { spec: admin.McpServerSpec; error?: undefined } | { spec?: undefined; error: string } {
  const argv = splitCommandLine(draft.commandLine);
  if (draft.transport === "stdio" && argv.length === 0) {
    return { error: "Enter the command that starts the server." };
  }
  if (draft.transport !== "stdio" && !/^https?:\/\//.test(draft.url.trim())) {
    return { error: "The URL must start with http:// or https://." };
  }
  const env = parseKeyValues(draft.env);
  if (env.bad !== undefined)
    return { error: `Each environment line is KEY=value; "${env.bad}" is not.` };
  const headers = parseKeyValues(draft.headers);
  if (headers.bad !== undefined) {
    return { error: `Each header line is Name=value; "${headers.bad}" is not.` };
  }
  const num = (v: string, what: string): number | string => {
    if (v.trim() === "") return 0;
    const n = Number(v);
    return Number.isInteger(n) && n > 0 ? n : `${what} must be a whole number above zero.`;
  };
  const knobs = {
    connectTimeoutMs: num(draft.connectTimeoutMs, "The connect timeout"),
    callTimeoutMs: num(draft.callTimeoutMs, "The call timeout"),
    rateLimitPerMinute: num(draft.rateLimitPerMinute, "Calls per minute"),
    rateLimitBurst: num(draft.rateLimitBurst, "The burst limit"),
    maxTools: num(draft.maxTools, "The tool cap"),
  };
  for (const v of Object.values(knobs)) if (typeof v === "string") return { error: v };
  const [command = "", ...args] = argv;
  return {
    spec: {
      enabled,
      transport: draft.transport,
      command: draft.transport === "stdio" ? command : "",
      args: draft.transport === "stdio" ? args : [],
      env: draft.transport === "stdio" ? env.map : {},
      url: draft.transport === "stdio" ? "" : draft.url.trim(),
      headers: draft.transport === "stdio" ? {} : headers.map,
      bearerToken: draft.transport === "stdio" ? "" : draft.bearerToken.trim(),
      clientCert: draft.clientCert.trim(),
      clientKey: draft.clientKey.trim(),
      caCert: draft.caCert.trim(),
      ...(knobs as Record<keyof typeof knobs, number>),
    },
  };
}

/**
 * The server form, shared by "new" and "edit".
 *
 * Name, how to reach it, and the one secret most servers need. Everything
 * else — headers, certificates, timeouts, rate limits — sits under Advanced,
 * because a server is usually one command or one URL.
 */
export function McpServerForm({
  draft,
  onChange,
  nameLocked = false,
}: {
  draft: McpDraft;
  onChange: (next: McpDraft) => void;
  /** Editing an existing row: the name is the key and cannot change. */
  nameLocked?: boolean;
}) {
  const [advanced, setAdvanced] = useState(false);
  const set = <K extends keyof McpDraft>(key: K, value: McpDraft[K]) =>
    onChange({ ...draft, [key]: value });
  const remote = draft.transport !== "stdio";

  return (
    <div className="flex flex-col gap-4">
      <div>
        <label className={LABEL} htmlFor="mcp-name">
          Name
        </label>
        <input
          id="mcp-name"
          className={FIELD}
          placeholder="github"
          value={draft.name}
          disabled={nameLocked}
          autoFocus={!nameLocked}
          onChange={(e) => set("name", e.target.value)}
        />
      </div>

      <div className="flex flex-wrap gap-2" role="group" aria-label="How Borg reaches it">
        {TRANSPORTS.map((t) => (
          <Pill
            key={t}
            square
            variant={t === draft.transport ? "primary" : undefined}
            aria-pressed={t === draft.transport}
            onClick={() => set("transport", t)}
          >
            {TRANSPORT_LABEL[t]}
          </Pill>
        ))}
      </div>

      {!remote && (
        <div>
          <label className={LABEL} htmlFor="mcp-command">
            Command
          </label>
          <input
            id="mcp-command"
            className={`${FIELD} font-mono text-[12.5px]`}
            placeholder="npx -y @modelcontextprotocol/server-github"
            value={draft.commandLine}
            onChange={(e) => set("commandLine", e.target.value)}
          />
        </div>
      )}

      {remote && (
        <>
          <div>
            <label className={LABEL} htmlFor="mcp-url">
              URL
            </label>
            <input
              id="mcp-url"
              className={`${FIELD} font-mono text-[12.5px]`}
              placeholder="https://mcp.example.com/mcp"
              value={draft.url}
              onChange={(e) => set("url", e.target.value)}
            />
          </div>
          <div>
            <label className={LABEL} htmlFor="mcp-token">
              Bearer token
            </label>
            <input
              id="mcp-token"
              type="password"
              autoComplete="off"
              className={FIELD}
              placeholder="Optional"
              value={draft.bearerToken}
              onChange={(e) => set("bearerToken", e.target.value)}
            />
            <div className={HINT}>
              Sent as Authorization: Bearer. Write ${"{"}SECRET:name{"}"} to read a saved
              credential.
            </div>
          </div>
        </>
      )}

      <button
        type="button"
        aria-expanded={advanced}
        onClick={() => setAdvanced((v) => !v)}
        className="self-start cursor-pointer border-0 bg-transparent p-0 text-[11.5px] leading-none text-ink3 hover:text-ink"
      >
        {advanced ? "Hide advanced" : "Advanced…"}
      </button>

      {advanced && (
        <div className="flex flex-col gap-4 rounded-[10px] border border-line2 p-4">
          {!remote && (
            <div>
              <label className={LABEL} htmlFor="mcp-env">
                Environment
              </label>
              <textarea
                id="mcp-env"
                rows={3}
                className={AREA}
                placeholder={"GITHUB_PERSONAL_ACCESS_TOKEN=${SECRET:github_pat}"}
                value={draft.env}
                onChange={(e) => set("env", e.target.value)}
              />
              <div className={HINT}>One KEY=value per line, passed to the command.</div>
            </div>
          )}
          {remote && (
            <>
              <div>
                <label className={LABEL} htmlFor="mcp-headers">
                  Headers
                </label>
                <textarea
                  id="mcp-headers"
                  rows={3}
                  className={AREA}
                  placeholder="X-Api-Key=${SECRET:api_key}"
                  value={draft.headers}
                  onChange={(e) => set("headers", e.target.value)}
                />
                <div className={HINT}>One Name=value per line, sent on every request.</div>
              </div>
              <div className="grid grid-cols-1 gap-3 sm:grid-cols-3">
                <div>
                  <label className={LABEL} htmlFor="mcp-client-cert">
                    Client certificate
                  </label>
                  <input
                    id="mcp-client-cert"
                    className={FIELD}
                    placeholder="~/certs/client.pem"
                    value={draft.clientCert}
                    onChange={(e) => set("clientCert", e.target.value)}
                  />
                </div>
                <div>
                  <label className={LABEL} htmlFor="mcp-client-key">
                    Client key
                  </label>
                  <input
                    id="mcp-client-key"
                    className={FIELD}
                    placeholder="~/certs/client-key.pem"
                    value={draft.clientKey}
                    onChange={(e) => set("clientKey", e.target.value)}
                  />
                </div>
                <div>
                  <label className={LABEL} htmlFor="mcp-ca-cert">
                    Extra trusted CA
                  </label>
                  <input
                    id="mcp-ca-cert"
                    className={FIELD}
                    placeholder="~/certs/corp-ca.pem"
                    value={draft.caCert}
                    onChange={(e) => set("caCert", e.target.value)}
                  />
                </div>
              </div>
            </>
          )}
          <div className="grid grid-cols-2 gap-3 sm:grid-cols-5">
            {(
              [
                ["connectTimeoutMs", "Connect timeout (ms)", "60000"],
                ["callTimeoutMs", "Call timeout (ms)", "120000"],
                ["rateLimitPerMinute", "Calls per minute", "60"],
                ["rateLimitBurst", "Burst", "10"],
                ["maxTools", "Tool cap", "32"],
              ] as const
            ).map(([key, label, placeholder]) => (
              // Bottom-aligned, so a label that wraps keeps its field in line.
              <div key={key} className="flex flex-col justify-end">
                <label className={LABEL} htmlFor={`mcp-${key}`}>
                  {label}
                </label>
                <input
                  id={`mcp-${key}`}
                  inputMode="numeric"
                  className={FIELD}
                  placeholder={placeholder}
                  value={draft[key]}
                  onChange={(e) => set(key, e.target.value)}
                />
              </div>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}
