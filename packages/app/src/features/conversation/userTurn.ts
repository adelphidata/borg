/**
 * Reads a user-role turn the way a person should see it.
 *
 * Not every user turn was typed by the user. The heartbeat and the session
 * greeting are written by Borg into the user's role, opened by a marker like
 * `*heartbeat tick*` and followed by tagged blocks such as
 * `<proactive_nudges>…</proactive_nudges>` that brief the model. Shown raw, that is
 * a wall of angle brackets and asterisks in a bubble that claims the user said
 * it. Split apart, it can be labelled for what it is, with each block laid out
 * as the list it already is.
 */

/** A tagged block Borg attached for the model. */
export interface AttachedBlock {
  tag: string;
  /** `proactive_nudges` → "Proactive nudges". */
  label: string;
  /** The block's text, as Markdown: the briefs are already written as lists. */
  body: string;
  /** Top-level `- ` entries, the count a collapsed section shows. */
  items: number;
}

export interface UserTurn {
  /** Set when Borg wrote this turn, naming what kind it is. */
  system?: string;
  /** What remains to show as the message itself, markers and blocks removed. */
  text: string;
  blocks: AttachedBlock[];
}

/** Markers that open a turn Borg wrote, with the name a person sees for each. */
const SYSTEM_MARKERS: readonly (readonly [string, string])[] = [
  ["*heartbeat tick*", "Heartbeat check-in"],
  ["*session start greeting*", "Session start"],
];

/**
 * A snake_case tag on its own line, its body, and the matching close on its
 * own line. Anchored to whole lines so an inline `<b>` someone typed, or a tag
 * quoted mid-sentence, stays text. The open tag may carry attributes: the
 * heartbeat's pre-run output arrives as
 * `<preprocessed_context source="pre_run" exit_code="0">`, ahead of the marker.
 */
const BLOCK = /^<([a-z][a-z0-9_]*)(?:[ \t][^>\n]*)?>[ \t]*\n([\s\S]*?)\n?<\/\1>[ \t]*$/gm;

function labelOf(tag: string): string {
  const words = tag.replaceAll("_", " ");
  return words.charAt(0).toUpperCase() + words.slice(1);
}

/**
 * Whether a block in a turn folds away. In a turn Borg wrote, every block is a
 * brief. In one the user typed, only Borg's own multi-word snake_case briefs
 * are (`preprocessed_context`): a `<template>` or `<div>` they pasted is part
 * of what they said, and hiding it behind a fold would lose their message.
 */
function folds(tag: string, system: boolean): boolean {
  return system || tag.includes("_");
}

/** Splits a user turn into its message, its kind, and its attached blocks. */
export function parseUserTurn(raw: string): UserTurn {
  // Briefs can lead the marker (the heartbeat's pre-run output does), so the
  // marker is looked for with every block set aside.
  const marker = SYSTEM_MARKERS.find(([m]) => raw.replace(BLOCK, "").trim().startsWith(m));
  const blocks: AttachedBlock[] = [];
  const rest = raw
    .replace(BLOCK, (match: string, tag: string, body: string) => {
      if (!folds(tag, marker !== undefined)) return match;
      blocks.push({
        tag,
        label: labelOf(tag),
        body: body.trim(),
        items: body.match(/^- /gm)?.length ?? 0,
      });
      return "";
    })
    .trim();
  if (!marker) return { text: rest, blocks };
  return { system: marker[1], text: rest.slice(marker[0].length).trim(), blocks };
}

/**
 * A session title that is only a marker, renamed for people. The daemon names
 * a session after its first message, which for a heartbeat session is
 * `*heartbeat tick*`.
 */
export function displayTitle(title: string): string {
  const trimmed = title.trim();
  return SYSTEM_MARKERS.find(([m]) => trimmed === m)?.[1] ?? title;
}
