/**
 * The curated settings shown above the Advanced fold, mapped to their daemon
 * keys. Everything else in the registry renders from the live
 * `Admin.GetSettings` snapshot instead of a hand-written list.
 */

import { THEME_PREFERENCES, type ThemePreference } from "@borg/ui";
import { capitalize } from "../../lib/format";

export interface SettingOption {
  /** What gets stored, and what `onPick` hands back. */
  value: string;
  /** Shown instead of `value` when the stored form is not presentable. */
  label?: string;
  desc?: string;
}

/** Display label per theme preference, shared by the settings list and the editor. */
export const APPEARANCE_LABELS: Record<ThemePreference, string> = {
  light: "Light",
  dark: "Dark",
  system: "System",
};

const APPEARANCE_DESC: Record<ThemePreference, string> = {
  light: "Paper and ink.",
  dark: "Neon on black. The classic.",
  system: "Follow this machine.",
};

export interface BasicSetting {
  /** Route segment under `/settings/`. */
  id: string;
  /** Daemon settings key; null for the local-only theme setting. */
  key: string | null;
  name: string;
  desc: string;
  /** Enumerable choices. Absent = free-text value. */
  options?: SettingOption[];
  /**
   * Choices come from the daemon's catalog (`Admin.GetSetupStatus`) rather
   * than a list here: the providers, or the models of the current provider.
   * Free text stays reachable behind "Something else…". `name` is the
   * agent's name: free text with a reroll from the daemon's name pool.
   */
  catalog?: "provider" | "model" | "name";
}

/**
 * How hard Borg thinks, as offered to the user.
 *
 * A curated subset of the levels the daemon accepts — the composer's picker and
 * the settings page read this same list, so the two surfaces can never drift
 * into offering different answers to the same question.
 */
export const THINKING_OPTIONS: SettingOption[] = [
  { value: "off", desc: "Answer straight away." },
  { value: "low", desc: "A moment's thought." },
  { value: "medium", desc: "Thinks it through." },
  { value: "high", desc: "Slow and deep. For the hard stuff." },
  { value: "adaptive", desc: "Borg decides per question." },
];

export const basicSettings: BasicSetting[] = [
  {
    id: "name",
    key: "user.agent_name",
    name: "Name",
    desc: "What Borg answers to",
    catalog: "name",
  },
  {
    id: "provider",
    key: "provider",
    name: "Provider",
    desc: "Which service Borg's brain runs on",
    catalog: "provider",
  },
  {
    id: "model",
    key: "model",
    name: "Model",
    desc: "Which brain Borg thinks with",
    catalog: "model",
  },
  {
    id: "thinking",
    key: "llm.thinking",
    name: "Thinking level",
    desc: "How long Borg reasons before it answers",
    options: THINKING_OPTIONS,
  },
  {
    id: "mode",
    key: "conversation.collaboration_mode",
    name: "Mode",
    desc: "How much Borg is allowed to do on its own",
    options: [
      { value: "execute", desc: "Borg acts. The default." },
      { value: "plan", desc: "Read-only. Borg proposes, you approve." },
    ],
  },
  {
    id: "appearance",
    key: null,
    name: "Appearance",
    desc: "",
    options: THEME_PREFERENCES.map((p) => ({
      value: p,
      label: APPEARANCE_LABELS[p],
      desc: APPEARANCE_DESC[p],
    })),
  },
];

/** How an option reads on screen: its label, else its stored value capitalized. */
export function optionLabel(option: SettingOption): string {
  return option.label ?? capitalize(option.value);
}

/**
 * A curated setting's stored value as the user should read it. A value the
 * options do not list (set from the CLI, say) is shown as stored.
 */
export function displayValue(setting: BasicSetting, raw: string): string {
  const option = setting.options?.find((o) => o.value === raw);
  return option ? optionLabel(option) : raw;
}

export function findBasicSetting(id: string): BasicSetting | undefined {
  return basicSettings.find((s) => s.id === id);
}

/** One fold under Advanced: a plain-words bucket over several key prefixes. */
export interface AdvancedCategory {
  id: string;
  name: string;
  desc: string;
  /** First key segments that land here (`voice` for `voice.enabled`). */
  prefixes: readonly string[];
}

/**
 * Advanced, sorted by what the settings are for rather than how the config
 * file nests them. Order is the order on screen: the knobs people reach for
 * first sit at the top. A prefix nobody listed falls into "Other", so a new
 * daemon section shows up without a change here.
 */
export const ADVANCED_CATEGORIES: readonly AdvancedCategory[] = [
  {
    id: "model",
    name: "Model",
    desc: "Providers, fallbacks, caching and spend",
    prefixes: [
      "llm",
      "temperature",
      "max_tokens",
      "compaction",
      "credentials",
      "providers",
      "budget",
    ],
  },
  {
    id: "conversation",
    name: "Conversation",
    desc: "Context, history and how each turn runs",
    prefixes: ["conversation", "loop_guard", "ui"],
  },
  {
    id: "memory",
    name: "Memory",
    desc: "What Borg remembers and how it recalls it",
    prefixes: ["memory", "consolidation", "data"],
  },
  {
    id: "voice",
    name: "Voice & Media",
    desc: "Speech, transcription, images and video",
    prefixes: ["voice", "tts", "audio", "media", "image_gen"],
  },
  {
    id: "tools",
    name: "Tools & Skills",
    desc: "Skills, scripts, MCP servers and plugins",
    prefixes: ["tools", "skills", "scripts", "mcp", "plugins", "hooks", "computer_use"],
  },
  {
    id: "browser",
    name: "Browser & Web",
    desc: "Browsing, search and page fetching",
    prefixes: ["browser", "web"],
  },
  {
    id: "agents",
    name: "Agents & Goals",
    desc: "Sub-agents, long-running goals and background tasks",
    prefixes: ["agents", "goals", "tasks", "evolution"],
  },
  {
    id: "heartbeat",
    name: "Heartbeat",
    desc: "Proactive check-ins and quiet hours",
    prefixes: ["heartbeat"],
  },
  {
    id: "channels",
    name: "Channels & Network",
    desc: "Messaging gateways, webhooks, tunnels and discovery",
    prefixes: ["gateway", "webhooks", "apns", "tunnel", "discovery"],
  },
  {
    id: "security",
    name: "Security",
    desc: "Sandboxing, secrets and what Borg may touch",
    prefixes: ["security", "sandbox"],
  },
  {
    id: "system",
    name: "System",
    desc: "Maintenance, processes, telemetry and updates",
    prefixes: [
      "maintenance",
      "process_registry",
      "telemetry",
      "debug",
      "update",
      "user",
      "onboarded",
    ],
  },
];

/** Where a key no category claims ends up. */
export const OTHER_CATEGORY: AdvancedCategory = {
  id: "other",
  name: "Other",
  desc: "Everything else",
  prefixes: [],
};

const CATEGORY_BY_PREFIX = new Map(
  ADVANCED_CATEGORIES.flatMap((c) => c.prefixes.map((p) => [p, c] as const)),
);

/** A key's first segment: `voice` for `voice.enabled`, the key itself when undotted. */
export function keyPrefix(key: string): string {
  const dot = key.indexOf(".");
  return dot === -1 ? key : key.slice(0, dot);
}

/** The Advanced fold a registry key belongs in. */
export function advancedCategoryOf(key: string): AdvancedCategory {
  return CATEGORY_BY_PREFIX.get(keyPrefix(key)) ?? OTHER_CATEGORY;
}
