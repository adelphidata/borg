import { useSettings } from "./settings";

/** The daemon setting that names the agent. */
export const AGENT_NAME_KEY = "user.agent_name";

/** What the agent is called until someone names it. */
export const DEFAULT_AGENT_NAME = "Borg";

/**
 * A name from the daemon's pool (`SetupStatus.agentNames`) other than
 * `current`, so a reroll always visibly changes something. `current` comes
 * back when the pool has nothing else to offer.
 */
export function rerollName(
  pool: readonly string[],
  current: string,
  random: () => number = Math.random,
): string {
  const others = pool.filter((n) => n !== current);
  if (others.length === 0) return current;
  return others[Math.floor(random() * others.length)] ?? current;
}

/** The agent's name from the settings snapshot; "Borg" while loading or unset. */
export function useAgentName(): string {
  const { settings } = useSettings();
  const value = settings.state === "ready" ? settings.data.get(AGENT_NAME_KEY)?.value.trim() : "";
  return value || DEFAULT_AGENT_NAME;
}
