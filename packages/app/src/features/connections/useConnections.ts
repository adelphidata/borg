import type { BorgClient, admin } from "@borg/client";
import { useQuery, type QueryResult } from "../../lib/useDaemon";
import { KEY_SOURCES } from "../onboarding/useSetupStatus";

/**
 * What the connections surface is really made of: LLM provider credentials
 * from the daemon's credential pool, and messaging channels with approved
 * senders / pending pairing requests. The old connected/available split is
 * derived from this, not invented.
 */

export interface ProviderConnection {
  kind: "provider";
  /** Provider token as the daemon knows it, e.g. "openrouter". */
  name: string;
  strategy: string;
  cooldownSecs: number;
  /** Runtime credential pool (env/config-seeded, with live status). */
  entries: admin.CredentialEntry[];
  /** Persisted per-provider key backend: "literal" | "keychain" | undefined. */
  apiKeyBackend?: string;
  /**
   * Where the daemon found a key on this machine — a variable name or a
   * `.env` path, never the secret. It already works, so the provider is
   * connected; offering to "add" it asked for a key Borg was already using.
   */
  foundAt?: string;
}

export interface ChannelConnection {
  kind: "channel";
  name: string;
  senders: admin.ApprovedSender[];
  pending: admin.PairingRequest[];
}

export interface Connections {
  providers: ProviderConnection[];
  channels: ChannelConnection[];
  /** Total pending pairing requests across all channels. */
  pendingCount: number;
  /**
   * Providers worth offering before any key exists, from the daemon's
   * catalog (`Admin.GetSetupStatus`). Display metadata only; the same rows
   * the first run and the settings pickers show.
   */
  catalog: admin.ProviderChoice[];
}

/** OAuth sign-ins the daemon can import from tools already on this machine. */
export const PAIR_SOURCES: { kind: string; label: string }[] = [
  { kind: "claude_code", label: "Claude Code" },
  { kind: "codex", label: "Codex" },
  { kind: "copilot", label: "Copilot" },
  { kind: "qwen_cli", label: "Qwen CLI" },
];

export { providerLabel } from "../onboarding/useSetupStatus";

/** Two-letter tile badge for any connection name. */
export function badgeOf(name: string): string {
  return (
    name
      .replace(/[^a-zA-Z0-9]/g, "")
      .slice(0, 2)
      .toUpperCase() || "??"
  );
}

export async function loadConnections(borg: BorgClient): Promise<Connections> {
  const [credentials, apiKeys, senders, pairings, setup] = await Promise.all([
    borg.admin.listCredentials({}),
    borg.admin.listApiKeys({}),
    borg.admin.listApprovedSenders({}),
    borg.admin.listPairingRequests({}),
    // The catalog only supplies display names and the "Add" grid. It is the
    // heaviest of the five calls, so a slow probe must not take the page
    // down; `providerLabel` passes an unknown id straight through.
    borg.admin.getSetupStatus({}).catch(() => ({ providers: [], detected: [] })),
  ]);

  const providers = new Map<string, ProviderConnection>();
  const providerOf = (name: string): ProviderConnection => {
    const existing = providers.get(name);
    if (existing) return existing;
    const fresh: ProviderConnection = {
      kind: "provider",
      name,
      strategy: "fill_first",
      cooldownSecs: 0,
      entries: [],
    };
    providers.set(name, fresh);
    return fresh;
  };
  for (const p of credentials.providers) {
    const conn = providerOf(p.provider);
    conn.strategy = p.strategy;
    conn.cooldownSecs = p.cooldownSecs;
    conn.entries = p.entries;
  }
  // Persisted keys (SetApiKey) count as connected even before the pool sees them.
  for (const k of apiKeys.entries) {
    if (k.backend !== "unset") providerOf(k.provider).apiKeyBackend = k.backend;
  }

  for (const d of setup.detected ?? []) {
    if (!KEY_SOURCES.has(d.source)) continue;
    const conn = providerOf(d.provider.toLowerCase());
    conn.foundAt ??= d.detail || "this computer";
  }

  const channels = new Map<string, ChannelConnection>();
  const channelOf = (name: string): ChannelConnection => {
    const existing = channels.get(name);
    if (existing) return existing;
    const fresh: ChannelConnection = { kind: "channel", name, senders: [], pending: [] };
    channels.set(name, fresh);
    return fresh;
  };
  for (const s of senders.senders) channelOf(s.channelName).senders.push(s);
  for (const r of pairings.requests) {
    if (r.status === "pending") channelOf(r.channelName).pending.push(r);
  }

  const channelList = [...channels.values()].sort((a, b) => a.name.localeCompare(b.name));
  const providerList = [...providers.values()].sort((a, b) => a.name.localeCompare(b.name));
  return {
    providers: providerList,
    channels: channelList,
    pendingCount: channelList.reduce((n, c) => n + c.pending.length, 0),
    catalog: setup.providers,
  };
}

export function useConnections(): QueryResult<Connections> {
  return useQuery(loadConnections);
}
