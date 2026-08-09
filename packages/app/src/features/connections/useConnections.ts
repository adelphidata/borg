import type { BorgClient, admin } from "@borg/client";
import { useQuery, type QueryResult } from "../../lib/useDaemon";

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
}

/** Providers worth offering before any key exists. Display metadata only. */
export const PROVIDER_CATALOG: { name: string; label: string; blurb: string }[] = [
  { name: "openrouter", label: "OpenRouter", blurb: "One key, every model behind it." },
  { name: "anthropic", label: "Anthropic", blurb: "Claude, straight from the source." },
  { name: "openai", label: "OpenAI", blurb: "GPT models with your own key." },
  { name: "gemini", label: "Gemini", blurb: "Google's models with an AI Studio key." },
  { name: "deepseek", label: "DeepSeek", blurb: "Fast, inexpensive reasoning models." },
  { name: "groq", label: "Groq", blurb: "Open models at very high speed." },
  { name: "xai", label: "xAI", blurb: "Grok models with your own key." },
  { name: "ollama", label: "Ollama", blurb: "A model running on this machine. Nothing leaves." },
];

/** OAuth sign-ins the daemon can import from tools already on this machine. */
export const PAIR_SOURCES: { kind: string; label: string }[] = [
  { kind: "claude_code", label: "Claude Code" },
  { kind: "codex", label: "Codex" },
  { kind: "copilot", label: "Copilot" },
  { kind: "qwen_cli", label: "Qwen CLI" },
];

export function providerLabel(name: string): string {
  return PROVIDER_CATALOG.find((p) => p.name === name)?.label ?? name;
}

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
  const [credentials, apiKeys, senders, pairings] = await Promise.all([
    borg.admin.listCredentials({}),
    borg.admin.listApiKeys({}),
    borg.admin.listApprovedSenders({}),
    borg.admin.listPairingRequests({}),
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
  };
}

export function useConnections(): QueryResult<Connections> {
  return useQuery(loadConnections);
}
