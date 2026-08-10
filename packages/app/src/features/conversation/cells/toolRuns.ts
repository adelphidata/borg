import type { Cell } from "../useConversation";
import { collectEdits } from "../turnEdits";
import { parseArgs, summarize } from "./ToolCell";
import { capitalize } from "../../../lib/format";

/** A tool cell, narrowed once so the grouping code stops re-proving it. */
export type ToolCellData = Cell & { kind: "tool" };

/**
 * What the transcript actually renders: either one cell as it always was, or a
 * run of tool calls standing in for several.
 */
export type RenderItem =
  | { type: "cell"; cell: Cell; index: number }
  | { type: "run"; key: string; tools: ToolCellData[]; live: boolean; index: number };

/** How a tool call reads in a summary line. */
type Category = "edit" | "explore" | "lookup" | "command" | "other";

const CATEGORIES: Record<string, Category> = {
  apply_patch: "edit",
  apply_skill_patch: "edit",
  read_file: "explore",
  list_dir: "explore",
  // Not files: a web search and a memory lookup have no path to count, and
  // calling three searches "3 files" describes work that never happened.
  list: "lookup",
  memory_search: "lookup",
  read_memory: "lookup",
  search_tools: "lookup",
  web_search: "lookup",
  web_fetch: "lookup",
  run_shell: "command",
};

function categoryOf(name: string): Category {
  return CATEGORIES[name] ?? "other";
}

/** The last path segment, which is what identifies a file to a reader. */
function basename(value: string): string {
  const cleaned = value.replace(/\/+$/, "");
  return cleaned.slice(cleaned.lastIndexOf("/") + 1) || cleaned;
}

/** The one thing a call was pointed at, for the singular phrasings below. */
function subjectOf(tool: ToolCellData): string | null {
  return summarize(parseArgs(tool.args));
}

const PHRASES: Record<Category, { verb: string; gerund: string; noun: string; nouns: string }> = {
  edit: { verb: "Edited", gerund: "Editing", noun: "file", nouns: "files" },
  explore: { verb: "read", gerund: "reading", noun: "file", nouns: "files" },
  lookup: { verb: "looked up", gerund: "looking up", noun: "thing", nouns: "things" },
  command: { verb: "ran", gerund: "running", noun: "command", nouns: "commands" },
  other: { verb: "used", gerund: "using", noun: "tool", nouns: "tools" },
};

const ORDER: Category[] = ["edit", "explore", "lookup", "command", "other"];

/**
 * One sentence for a run of tool calls.
 *
 * Names the thing when a category holds exactly one — "Edited toolRuns.ts"
 * tells you more than "edited 1 file" — and counts otherwise. A live run reads
 * in the present tense, because it is still happening.
 */
export function runSummary(tools: ToolCellData[], live: boolean): string {
  const buckets = new Map<Category, ToolCellData[]>();
  for (const tool of tools) {
    const category = categoryOf(tool.name);
    const bucket = buckets.get(category);
    if (bucket) bucket.push(tool);
    else buckets.set(category, [tool]);
  }

  const parts: string[] = [];
  for (const category of ORDER) {
    const bucket = buckets.get(category);
    if (!bucket) continue;
    const phrase = PHRASES[category];
    const verb = live ? phrase.gerund : phrase.verb;

    // Distinct subjects, so re-reading one file twice is not two files. A call
    // that names nothing counts as its own subject rather than merging with
    // every other anonymous call.
    //
    // A patch names no file in its arguments — `apply_patch` takes the patch
    // text and nothing else — so the files it touched can only be read back out
    // of the diff the daemon applied. Counting the calls instead would report
    // three edits to one file as "3 files".
    const subjects =
      category === "edit"
        ? collectEdits(bucket).map((e) => e.path)
        : bucket.map((t) => subjectOf(t));
    const distinct = new Set(
      category === "edit" ? subjects : subjects.map((s, i) => s ?? `#${bucket[i]!.id}`),
    );

    // A patch whose output was truncated or which failed outright yields no
    // readable path. Name the activity without a number rather than print a
    // count of zero next to calls that plainly did something.
    if (category === "edit" && distinct.size === 0) {
      parts.push(`${verb} ${phrase.nouns}`);
      continue;
    }

    // One subject is worth naming: "read toolRuns.ts" says more than
    // "read 1 file", and the count would be the less useful half.
    if (distinct.size === 1 && subjects[0] !== null) {
      // A command is quoted whole; a path is worth only its last segment.
      const named = category === "command" ? subjects[0]!.split("\n")[0]! : basename(subjects[0]!);
      parts.push(`${verb} ${named}`);
      continue;
    }
    parts.push(`${verb} ${distinct.size} ${distinct.size === 1 ? phrase.noun : phrase.nouns}`);
  }

  if (parts.length === 0) return live ? "Working" : "Did nothing";
  const sentence = parts.join(", ");
  return capitalize(sentence);
}

/** The shortest run worth collapsing. One row is already one line. */
const MIN_RUN = 2;

/**
 * Folds consecutive settled tool calls into runs.
 *
 * A turn that touches thirty files should not cost thirty rows of transcript to
 * say so. Anything that still wants attention refuses to collapse — a call
 * still running, and one that wrote to stderr — and because those are not
 * collapsible they also end the run they sit in, so they keep their place in
 * the order and stay readable on their own.
 *
 * Runs are keyed by their first call's id, never by position: the live
 * transcript and the one rehydrated from history agree on which calls belong
 * together but not on where they sit in the array, and an index key makes the
 * whole run remount the moment a turn settles.
 */
export function groupCells(cells: Cell[], busy: boolean): RenderItem[] {
  const collapsible = (cell: Cell): cell is ToolCellData =>
    cell.kind === "tool" && cell.done && !cell.stderr;

  const items: RenderItem[] = [];
  for (let i = 0; i < cells.length;) {
    if (!collapsible(cells[i]!)) {
      items.push({ type: "cell", cell: cells[i]!, index: i });
      i++;
      continue;
    }
    // Where the run starts, so a caller that trims the transcript can cut at
    // item boundaries in the same coordinate space the cells use.
    const start = i;
    const run: ToolCellData[] = [];
    while (i < cells.length && collapsible(cells[i]!)) {
      run.push(cells[i] as ToolCellData);
      i++;
    }
    if (run.length < MIN_RUN) {
      items.push({ type: "cell", cell: run[0]!, index: i - 1 });
      continue;
    }
    items.push({ type: "run", key: `run:${run[0]!.id}`, tools: run, live: false, index: start });
  }

  // Only the trailing run of a turn still in flight is live; an earlier one has
  // already been overtaken by whatever came after it.
  if (busy) {
    const last = items[items.length - 1];
    if (last?.type === "run") last.live = true;
  }
  return items;
}
