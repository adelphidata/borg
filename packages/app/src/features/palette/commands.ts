/**
 * The command list, in one place.
 *
 * ⌘K renders it, and so does the composer's `/` menu. Building it here rather
 * than inside either surface is what stops the two offering different answers
 * to the same question — a palette that can toggle something the slash menu
 * cannot is two products.
 */

import { THEME_PREFERENCES, type ThemePreference } from "@borg/ui";
import type { DaemonSetting } from "../../lib/settings";
import { APPEARANCE_LABELS } from "../settings/catalog";
import type { Command } from "./score";

/** The subset of a session listing a command row needs. */
export interface CommandSession {
  id: string;
  title: string;
}

/** The shape `routes.tsx` supplies; kept local so this module stays testable. */
export interface RouteRow {
  id: string;
  path: string;
  label: string;
}

export interface CommandContext {
  navigate: (path: string) => void;
  /** Runs a registered keybind action, so a row can never drift from its chord. */
  dispatch: (action: string) => void;
  /** Renders a chord hint for an action, or undefined when it has none. */
  hintFor: (action: string) => string | undefined;
  settings: Map<string, DaemonSetting> | undefined;
  saveSetting: (key: string, value: string) => void;
  sessions: CommandSession[];
  themePreference: ThemePreference;
  setThemePreference: (preference: ThemePreference) => void;
  /**
   * True once the user has typed. Uncurated settings keys are reachable only
   * then: an empty palette is a menu, and a menu of 500 dotted keys teaches
   * nothing.
   */
  searching: boolean;
}

/** Uncurated registry keys sort below every row that has a real name. */
const ADVANCED_TIER = 1;

/** A daemon setting the curated table named, and that reads as a toggle. */
function isToggle(setting: DaemonSetting): boolean {
  return setting.kind === "bool" && (setting.value === "true" || setting.value === "false");
}

/**
 * One row that flips a boolean in place.
 *
 * The label keeps the setting's own outcome-facing name and the state trails
 * in muted text, so the row says both what it is and which way it will go. The
 * palette stays open — you are watching the state change, and closing would
 * hide the only feedback there is.
 */
function toggleCommand(setting: DaemonSetting, ctx: CommandContext): Command {
  const on = setting.value === "true";
  return {
    id: `setting:${setting.key}`,
    label: setting.label,
    hint: setting.group,
    detail: on ? "on" : "off",
    keywords: `${setting.key.replace(/[._]/g, " ")} on off enable disable toggle`,
    keepOpen: true,
    run: () => ctx.saveSetting(setting.key, on ? "false" : "true"),
  };
}

/** A curated non-boolean setting: still a navigation, but under a real name. */
function settingCommand(setting: DaemonSetting, ctx: CommandContext): Command {
  return {
    id: `setting:${setting.key}`,
    label: setting.label,
    hint: setting.group,
    keywords: setting.key.replace(/[._]/g, " "),
    run: () => ctx.navigate(`/settings/key/${encodeURIComponent(setting.key)}`),
  };
}

export function buildCommands(ctx: CommandContext, routes: RouteRow[]): Command[] {
  const actions: Command[] = [
    {
      id: "action:new",
      label: "New conversation",
      // Both the hint and the behaviour come from the keybind layer, so a row
      // can never advertise or perform a stale version of either.
      hint: ctx.hintFor("new-conversation"),
      keywords: "chat start fresh",
      run: () => ctx.dispatch("new-conversation"),
    },
  ];

  const go: Command[] = routes.map((route) => ({
    id: `route:${route.id}`,
    label: `Go to ${route.label}`,
    hint: "Navigate",
    run: () => ctx.navigate(route.path),
  }));

  // Appearance is the one setting the daemon does not own — it has to apply
  // before first paint, so it lives in localStorage. It still belongs here.
  const appearance: Command[] = THEME_PREFERENCES.map((preference) => ({
    id: `theme:${preference}`,
    label: APPEARANCE_LABELS[preference],
    hint: "Appearance",
    active: ctx.themePreference === preference,
    keywords: "theme appearance colour color dark light",
    keepOpen: true,
    run: () => ctx.setThemePreference(preference),
  }));

  const recents: Command[] = ctx.sessions.map((session) => ({
    id: `session:${session.id}`,
    label: session.title || "Untitled",
    hint: "Conversation",
    run: () => ctx.navigate(`/c/${session.id}`),
  }));

  const all = [...(ctx.settings?.values() ?? [])];
  const curated = all.filter((setting) => setting.label !== "");
  const settingRows: Command[] = curated.map((setting) =>
    isToggle(setting) ? toggleCommand(setting, ctx) : settingCommand(setting, ctx),
  );

  // Everything the curated table does not name. Dotted keys are the subsystem
  // spelling, not a name, so they stay out of the resting list and appear only
  // once a query narrows them down — and even then below every named row,
  // because a raw key matches a query letter-for-letter in a way a real name
  // cannot and would otherwise win every search for its own subsystem.
  const raw: Command[] = ctx.searching
    ? all
        .filter((setting) => setting.label === "")
        .map((setting) => ({
          id: `setting:${setting.key}`,
          label: setting.key,
          hint: "Advanced",
          keywords: setting.key.replace(/[._]/g, " "),
          tier: ADVANCED_TIER,
          run: () => ctx.navigate(`/settings/key/${encodeURIComponent(setting.key)}`),
        }))
    : [];

  return [...actions, ...go, ...appearance, ...settingRows, ...recents, ...raw];
}
