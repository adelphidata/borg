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
   * Free text stays reachable behind "Something else…".
   */
  catalog?: "provider" | "model";
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

/** `llm.thinking` → "Llm" bucket; keys with no dot land in "General". */
export function advancedGroupOf(key: string): string {
  const dot = key.indexOf(".");
  if (dot === -1) return "General";
  const head = key.slice(0, dot);
  return capitalize(head);
}
