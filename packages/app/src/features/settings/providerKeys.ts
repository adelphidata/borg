import type { BorgClient, admin } from "@borg/client";
import { useQuery, type QueryResult } from "../../lib/useDaemon";
import { KEY_SOURCES } from "../onboarding/useSetupStatus";

/**
 * The keys behind each model provider: the daemon's credential pool, the
 * keys Settings saved, and keys the daemon found on this machine. Read by
 * the Provider page and by each provider's key page under it.
 */
export interface ProviderKeys {
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
   * `.env` path, never the secret. It already works, so asking for a key
   * here would ask for something Borg is already using.
   */
  foundAt?: string;
}

export interface ProviderKeyData {
  /** Every provider with at least one key, by name. */
  providers: ProviderKeys[];
  /** The daemon's catalog, for display names; empty when it could not answer. */
  catalog: admin.ProviderChoice[];
  /** What the daemon found on this machine, keys and local runtimes alike. */
  detected: admin.DetectedCredential[];
}

/**
 * Sign-ins the daemon can import from tools already on this machine, and
 * the provider each one feeds. Qwen CLI lands in the custom endpoint's pool:
 * its endpoint is read from the file it imports.
 */
export const SIGN_INS: { kind: string; label: string; provider: string }[] = [
  { kind: "claude_code", label: "Claude Code", provider: "claude-code-oauth" },
  { kind: "codex", label: "Codex", provider: "codex-oauth" },
  { kind: "qwen_cli", label: "Qwen CLI", provider: "custom" },
];

export async function loadProviderKeys(borg: BorgClient): Promise<ProviderKeyData> {
  const [credentials, apiKeys, setup] = await Promise.all([
    borg.admin.listCredentials({}),
    borg.admin.listApiKeys({}),
    // The catalog only supplies display names. It is the heaviest of the
    // three calls, so a slow probe must not take the page down; a key found
    // on this computer then reads as missing, so say why.
    borg.admin.getSetupStatus({}).catch((err: unknown) => {
      console.warn("provider keys: setup status unavailable", err);
      return { providers: [], detected: [] };
    }),
  ]);

  const providers = new Map<string, ProviderKeys>();
  const providerOf = (name: string): ProviderKeys => {
    const existing = providers.get(name);
    if (existing) return existing;
    const fresh: ProviderKeys = { name, strategy: "fill_first", cooldownSecs: 0, entries: [] };
    providers.set(name, fresh);
    return fresh;
  };
  for (const p of credentials.providers) {
    const keys = providerOf(p.provider);
    keys.strategy = p.strategy;
    keys.cooldownSecs = p.cooldownSecs;
    keys.entries = p.entries;
  }
  // Persisted keys (SetApiKey) count before the pool sees them.
  for (const k of apiKeys.entries) {
    if (k.backend !== "unset") providerOf(k.provider).apiKeyBackend = k.backend;
  }
  const detected = setup.detected ?? [];
  for (const d of detected) {
    if (!KEY_SOURCES.has(d.source)) continue;
    providerOf(d.provider.toLowerCase()).foundAt ??= d.detail || "this computer";
  }

  return {
    providers: [...providers.values()].sort((a, b) => a.name.localeCompare(b.name)),
    catalog: setup.providers,
    detected,
  };
}

export function useProviderKeys(deps: unknown[] = []): QueryResult<ProviderKeyData> {
  return useQuery(loadProviderKeys, deps);
}

/** Pool entries that still count; a removed one is gone. */
export function liveEntries(keys: ProviderKeys | undefined): admin.CredentialEntry[] {
  return (keys?.entries ?? []).filter((e) => e.status !== "removed");
}

/** The key situation of one provider in a few words, or undefined when it has none. */
export function keySummary(keys: ProviderKeys | undefined): string | undefined {
  const live = liveEntries(keys).length;
  if (live > 0) return `${live} key${live === 1 ? "" : "s"}`;
  if (keys?.apiKeyBackend) return "Saved";
  if (keys?.foundAt) return "Found on this computer";
  return undefined;
}

/** True when every pooled key has stopped working: the state that silently stops Borg. */
export function keysFailing(keys: ProviderKeys | undefined): boolean {
  const live = liveEntries(keys);
  return live.length > 0 && live.every((e) => e.status !== "ok");
}
