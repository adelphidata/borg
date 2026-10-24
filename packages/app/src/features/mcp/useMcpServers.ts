import type { admin } from "@borg/client";
import { useEffect } from "react";
import { useMutation, useQuery, type QueryResult } from "../../lib/useDaemon";

/** The transports a server can speak, in the order the form offers them. */
export const TRANSPORTS = ["stdio", "http", "sse"] as const;
export type Transport = (typeof TRANSPORTS)[number];

/** What the lifecycle label from the daemon means to a person. */
export const STATE_WORDS: Record<string, string> = {
  connected: "Connected",
  starting: "Starting…",
  reconnecting: "Reconnecting…",
  "auth-failed": "Sign-in failed",
  degraded: "Having trouble",
  "not-started": "Not started",
  disabled: "Off",
  "live-disabled": "Off",
};

export function stateWords(state: string): string {
  return STATE_WORDS[state] ?? state;
}

/** Dot tone for a row: healthy, wants attention, or simply not running. */
export function stateTone(
  entry: Pick<admin.McpServerEntry, "state">,
): "ok" | "alert" | "busy" | "idle" {
  switch (entry.state) {
    case "connected":
      return "ok";
    case "starting":
    case "reconnecting":
      return "busy";
    case "auth-failed":
    case "degraded":
      return "alert";
    default:
      return "idle";
  }
}

/**
 * Splits a command line into argv the way a shell would for the common
 * cases: whitespace separates, single or double quotes group, a backslash
 * escapes the next character outside single quotes. Enough for
 * `npx -y "@scope/pkg" --root "/My Files"`; a pipeline is not a command.
 */
export function splitCommandLine(line: string): string[] {
  const out: string[] = [];
  let current = "";
  let quote: '"' | "'" | null = null;
  let has = false;
  for (let i = 0; i < line.length; i++) {
    const ch = line[i]!;
    if (quote) {
      if (ch === quote) {
        quote = null;
      } else if (ch === "\\" && quote === '"' && i + 1 < line.length) {
        current += line[++i];
      } else {
        current += ch;
      }
      continue;
    }
    if (ch === '"' || ch === "'") {
      quote = ch;
      has = true;
    } else if (ch === "\\" && i + 1 < line.length) {
      current += line[++i];
      has = true;
    } else if (/\s/.test(ch)) {
      if (has) out.push(current);
      current = "";
      has = false;
    } else {
      current += ch;
      has = true;
    }
  }
  if (has) out.push(current);
  return out;
}

/** The inverse, for showing a stored argv back as one editable line. */
export function joinCommandLine(parts: string[]): string {
  return parts
    .map((p) => (p === "" || /[\s"'\\]/.test(p) ? `"${p.replace(/(["\\])/g, "\\$1")}"` : p))
    .join(" ");
}

/**
 * `KEY=value` lines to a map. Blank lines and `#` comments are skipped; a line
 * without `=` is returned as the error so the form can point at it.
 */
export function parseKeyValues(text: string): { map: Record<string, string>; bad?: string } {
  const map: Record<string, string> = {};
  for (const raw of text.split("\n")) {
    const line = raw.trim();
    if (line === "" || line.startsWith("#")) continue;
    const eq = line.indexOf("=");
    if (eq <= 0) return { map, bad: line };
    map[line.slice(0, eq).trim()] = line.slice(eq + 1).trim();
  }
  return { map };
}

export function formatKeyValues(map: Record<string, string>): string {
  return Object.entries(map)
    .map(([k, v]) => `${k}=${v}`)
    .join("\n");
}

/** A blank spec, with zero for every knob so the daemon applies its defaults. */
export function emptySpec(transport: Transport = "stdio"): admin.McpServerSpec {
  return {
    enabled: true,
    transport,
    command: "",
    args: [],
    env: {},
    url: "",
    headers: {},
    bearerToken: "",
    clientCert: "",
    clientKey: "",
    caCert: "",
    connectTimeoutMs: 0,
    callTimeoutMs: 0,
    rateLimitPerMinute: 0,
    rateLimitBurst: 0,
    maxTools: 0,
  };
}

/** How often the list refetches while a server is still coming up. */
export const SETTLE_POLL_MS = 1500;

/**
 * True while an enabled server has not reached a resting state yet: the
 * daemon starts servers in the background after a write, so the label would
 * otherwise sit on "Not started" until the page is reloaded.
 */
export function settling(list: admin.McpServerList | undefined): boolean {
  if (!list?.mcpEnabled) return false;
  return list.servers.some(
    (s) =>
      (s.spec?.enabled ?? true) &&
      (s.state === "not-started" || s.state === "starting" || s.state === "reconnecting"),
  );
}

export interface McpServers {
  query: QueryResult<admin.McpServerList>;
  /** Create (or with `overwrite`, replace) a server from a spec. */
  save: (
    name: string,
    spec: admin.McpServerSpec,
    overwrite: boolean,
  ) => Promise<admin.McpServerEntry | undefined>;
  /** Create a server from one of the daemon's templates. */
  addPreset: (name: string, preset: string, fsRoot?: string) => Promise<unknown>;
  remove: (name: string) => Promise<unknown>;
  setEnabled: (entry: admin.McpServerEntry, enabled: boolean) => Promise<unknown>;
  /** Turn the global MCP switch on or off. */
  setMcpEnabled: (enabled: boolean) => Promise<unknown>;
  error: string | undefined;
  resetError: () => void;
}

/**
 * MCP servers Borg connects to.
 *
 * Reads join the stored rows with the live registry; every write refetches,
 * and the state label catches up as the daemon's background reload lands.
 */
export function useMcpServers(deps: unknown[] = []): McpServers {
  const query = useQuery((borg) => borg.admin.listMcpServers({}), deps);
  const refresh = { onSuccess: () => query.refetch() };

  const { refetch } = query;
  const busy = settling(query.data);
  useEffect(() => {
    if (!busy) return;
    const id = setInterval(refetch, SETTLE_POLL_MS);
    return () => clearInterval(id);
  }, [busy, refetch]);

  const put = useMutation(
    (
      borg,
      vars: {
        name: string;
        spec?: admin.McpServerSpec;
        preset?: string;
        fsRoot?: string;
        overwrite: boolean;
      },
    ) =>
      borg.admin.putMcpServer({
        name: vars.name,
        spec: vars.spec,
        preset: vars.preset ?? "",
        fsRoot: vars.fsRoot ?? "",
        overwrite: vars.overwrite,
      }),
    refresh,
  );
  const removeMutation = useMutation(
    (borg, name: string) => borg.admin.removeMcpServer({ name }),
    refresh,
  );
  const globalSwitch = useMutation(
    (borg, enabled: boolean) =>
      borg.admin.setSetting({ key: "mcp.enabled", value: enabled ? "true" : "false" }),
    refresh,
  );

  return {
    query,
    save: (name, spec, overwrite) => put.run({ name, spec, overwrite }),
    addPreset: (name, preset, fsRoot) => put.run({ name, preset, fsRoot, overwrite: false }),
    remove: (name) => removeMutation.run(name),
    setEnabled: (entry, enabled) =>
      put.run({
        name: entry.name,
        spec: { ...(entry.spec ?? emptySpec()), enabled },
        overwrite: true,
      }),
    setMcpEnabled: (enabled) => globalSwitch.run(enabled),
    error: (put.error ?? removeMutation.error ?? globalSwitch.error)?.message,
    resetError: () => {
      put.reset();
      removeMutation.reset();
      globalSwitch.reset();
    },
  };
}
