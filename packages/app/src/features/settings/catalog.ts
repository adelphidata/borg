/**
 * The curated settings shown above the Advanced fold, mapped to their daemon
 * keys. Everything else in the registry renders from the live
 * `Admin.GetSettings` snapshot instead of a hand-written list.
 */

export interface SettingOption {
  value: string;
  desc?: string;
}

export interface BasicSetting {
  /** Route segment under `/settings/`. */
  id: string;
  /** Daemon settings key; null for the local-only theme setting. */
  key: string | null;
  name: string;
  desc: string;
  /** Enumerable choices. Absent = free-text value. */
  options?: SettingOption[];
}

export const basicSettings: BasicSetting[] = [
  {
    id: "provider",
    key: "provider",
    name: "Provider",
    desc: "Which service Borg's brain runs on",
  },
  {
    id: "model",
    key: "model",
    name: "Model",
    desc: "Which brain Borg thinks with",
  },
  {
    id: "thinking",
    key: "llm.thinking",
    name: "Thinking level",
    desc: "How long Borg reasons before it answers",
    options: [
      { value: "off", desc: "Answer straight away." },
      { value: "low", desc: "A moment's thought." },
      { value: "medium", desc: "Thinks it through." },
      { value: "high", desc: "Slow and deep. For the hard stuff." },
      { value: "adaptive", desc: "Borg decides per question." },
    ],
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
    options: [
      { value: "Light", desc: "Paper and ink." },
      { value: "Dark", desc: "Neon on black. The classic." },
    ],
  },
];

export function findBasicSetting(id: string): BasicSetting | undefined {
  return basicSettings.find((s) => s.id === id);
}

/** `llm.thinking` → "Llm" bucket; keys with no dot land in "General". */
export function advancedGroupOf(key: string): string {
  const dot = key.indexOf(".");
  if (dot === -1) return "General";
  const head = key.slice(0, dot);
  return head.charAt(0).toUpperCase() + head.slice(1);
}
