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
  /** Enumerable choices. Absent = free-text value. */
  options?: SettingOption[];
  /**
   * Choices come from the daemon's catalog (`Admin.GetSetupStatus`) rather
   * than a list here: the providers, or the models of the current provider.
   * Free text stays reachable behind "Something else…". `name` is the
   * agent's name, free text with a reroll from the daemon's name pool, and
   * the user's beside it.
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

/** The setting that holds the collaboration mode. */
export const MODE_KEY = "conversation.collaboration_mode";

/** The collaboration modes, for Settings, the composer and the `/` menu. */
export const MODE_OPTIONS: SettingOption[] = [
  { value: "execute", desc: "Borg acts. The default." },
  { value: "plan", desc: "Read-only. Borg proposes, you approve." },
];

export const basicSettings: BasicSetting[] = [
  {
    id: "name",
    key: "user.agent_name",
    name: "Name",
    catalog: "name",
  },
  {
    id: "provider",
    key: "provider",
    name: "Provider",
    catalog: "provider",
  },
  {
    id: "model",
    key: "model",
    name: "Model",
    catalog: "model",
  },
  {
    id: "thinking",
    key: "llm.thinking",
    name: "Thinking level",
    options: THINKING_OPTIONS,
  },
  {
    id: "mode",
    key: MODE_KEY,
    name: "Mode",
    options: MODE_OPTIONS,
  },
  {
    id: "appearance",
    key: null,
    name: "Appearance",
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

/**
 * Where a registry key's editor lives: its curated page when Settings has one
 * (a picker with plain names), else the raw key's own editor.
 */
export function settingPath(key: string): string {
  const basic = basicSettings.find((s) => s.key === key);
  return basic ? `/settings/${basic.id}` : `/settings/key/${encodeURIComponent(key)}`;
}

/** One fold under Advanced: a plain-words bucket over several key prefixes. */
export interface AdvancedCategory {
  id: string;
  name: string;
  /** First key segments that land here (`voice` for `voice.enabled`). */
  prefixes: readonly string[];
  /** A guided page for this fold's settings, listed above the raw keys. */
  page?: { path: string; name: string };
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
    prefixes: ["conversation", "loop_guard", "ui"],
  },
  {
    id: "memory",
    name: "Memory",
    prefixes: ["memory", "consolidation", "data"],
  },
  {
    id: "voice",
    name: "Voice & Media",
    prefixes: ["voice", "tts", "audio", "telephony", "media", "image_gen"],
    page: {
      path: "/settings/voice",
      name: "Voice & phone",
    },
  },
  {
    id: "tools",
    name: "Tools & Skills",
    prefixes: ["tools", "skills", "scripts", "mcp", "plugins", "hooks", "computer_use"],
  },
  {
    id: "browser",
    name: "Browser & Web",
    prefixes: ["browser", "web"],
  },
  {
    id: "agents",
    name: "Agents & Goals",
    prefixes: ["agents", "goals", "tasks", "evolution"],
  },
  {
    id: "heartbeat",
    name: "Heartbeat",
    prefixes: ["heartbeat"],
  },
  {
    id: "channels",
    name: "Channels & Network",
    prefixes: ["gateway", "webhooks", "apns", "tunnel", "discovery"],
  },
  {
    id: "security",
    name: "Security",
    prefixes: ["security", "sandbox"],
  },
  {
    id: "system",
    name: "System",
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
