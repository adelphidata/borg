import type { admin } from "@borg/client";
import { useQuery, type QueryResult } from "../../lib/useDaemon";

/**
 * The daemon's provider catalog, current choice, and what it has already
 * found on this machine — `Admin.GetSetupStatus`, one round trip.
 *
 * Every surface that names a provider or model reads this: the first run,
 * the settings pickers, the composer's model pill, the connections pages.
 * The app carries no list of its own, so a catalog change in the daemon
 * lands everywhere at once and two screens can never disagree.
 */
export type SetupStatus = admin.SetupStatus;
export type ProviderChoice = admin.ProviderChoice;
export type ModelChoice = admin.ModelChoice;
export type DetectedCredential = admin.DetectedCredential;

/** Detection sources that are an API key the daemon can already use. */
export const KEY_SOURCES: ReadonlySet<string> = new Set([
  "env",
  "dotenv",
  "stored",
  "keychain",
  "pool",
]);

export function useSetupStatus(): QueryResult<SetupStatus> {
  return useQuery((borg) => borg.admin.getSetupStatus({}));
}

export function providerOf(providers: ProviderChoice[], id: string): ProviderChoice | undefined {
  return providers.find((p) => p.id.toLowerCase() === id.toLowerCase());
}

/** Display name for a provider id; the id itself when the catalog has no row. */
export function providerLabel(providers: ProviderChoice[], id: string): string {
  return providerOf(providers, id)?.label ?? id;
}

export function modelsOf(providers: ProviderChoice[], provider: string): ModelChoice[] {
  return providerOf(providers, provider)?.models ?? [];
}

/** Short display name for a model id; the id itself when it is not curated. */
export function modelLabel(providers: ProviderChoice[], provider: string, model: string): string {
  return modelsOf(providers, provider).find((m) => m.id === model)?.label ?? model;
}

/**
 * Which detection to lead with. A credential Borg already holds beats one
 * found in the environment, which beats a local runtime; within a tier the
 * daemon's order stands. The configured provider's own detection wins
 * outright, so a deliberate choice is never second-guessed.
 */
const SOURCE_RANK = ["pool", "stored", "keychain", "env", "dotenv", "claude_cli", "ollama"];

export function leadDetection(status: SetupStatus): DetectedCredential | undefined {
  const mine = status.detected.find(
    (d) => status.provider !== "" && d.provider.toLowerCase() === status.provider.toLowerCase(),
  );
  if (mine) return mine;
  // A source this build does not know about sorts last, never first.
  const rank = (source: string) => {
    const at = SOURCE_RANK.indexOf(source);
    return at === -1 ? SOURCE_RANK.length : at;
  };
  return [...status.detected].sort((a, b) => rank(a.source) - rank(b.source))[0];
}

/** One plain sentence saying what was found, with nothing secret in it. */
export function detectionNote(providers: ProviderChoice[], d: DetectedCredential): string {
  const label = providerLabel(providers, d.provider);
  switch (d.source) {
    case "claude_cli":
      return "Claude Code is signed in on this machine.";
    case "ollama":
      return "Ollama is running on this machine.";
    case "env":
      return `Found your ${label} key in ${d.detail}.`;
    case "dotenv":
      return `Found your ${label} key in ${d.detail}.`;
    default:
      return `Your ${label} key is already saved.`;
  }
}
