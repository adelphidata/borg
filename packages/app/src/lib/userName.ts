import type { NavUser } from "@borg/ui";
import { useSettings } from "./settings";

/** The daemon setting that holds what the user is called. */
export const USER_NAME_KEY = "user.name";

/** Longest name the daemon accepts, for the user and the agent alike. */
export const MAX_NAME_CHARS = 40;

/**
 * Up to two letters for the avatar: the first and last words' first letters,
 * so "Ada Lovelace" is "AL" and "Ada" is "A". Code points, not UTF-16 units,
 * so a name that starts outside the BMP keeps its first character whole.
 */
export function initialsOf(name: string): string {
  const words = name.trim().split(/\s+/).filter(Boolean);
  const first = (word: string | undefined) => (word ? Array.from(word)[0]! : "");
  const ends = words.length > 1 ? [words[0], words[words.length - 1]] : [words[0]];
  return ends.map(first).join("").toLocaleUpperCase();
}

/**
 * True for a tool call that saves the user's name: the agent passes
 * `user_name` to `write_memory` when the user says what to call them. Blank
 * counts as absent, as it does in the daemon.
 */
export function savesUserName(tool: string, argsJson: string): boolean {
  if (tool !== "write_memory") return false;
  try {
    const name = (JSON.parse(argsJson) as { user_name?: unknown } | null)?.user_name;
    return typeof name === "string" && name.trim() !== "";
  } catch {
    return false;
  }
}

/**
 * The user as the sidebar shows them, from the settings snapshot. Absent
 * while loading or until someone, the user or the agent, saves a name.
 */
export function useNavUser(): NavUser | undefined {
  const { settings } = useSettings();
  const name = settings.state === "ready" ? settings.data.get(USER_NAME_KEY)?.value.trim() : "";
  return name ? { name, initials: initialsOf(name) } : undefined;
}
