import type { Cell } from "./useConversation";

/** One file a turn changed, with the diff that changed it. */
export interface FileEdit {
  path: string;
  added: number;
  removed: number;
  /** The unified diff for this file, concatenated across repeat edits. */
  diff: string;
}

/** The tools that write files. Everything else leaves the tree alone. */
export const EDIT_TOOLS = new Set(["apply_patch", "apply_skill_patch"]);

/**
 * True when a patch call actually wrote something.
 *
 * The handler reports failure as an ordinary `Ok("Error applying …")` result
 * rather than an error, so `stderr` alone says nothing — the daemon's success
 * banner is the only reliable signal. Matched on the first line so a diff body
 * that happens to quote the phrase cannot pass for one.
 */
function applied(output: string): boolean {
  const firstLine = output.trimStart().split("\n", 1)[0] ?? "";
  return /applied successfully\.$/.test(firstLine.trim());
}

/**
 * The daemon's middle-ellipsis marker (`util::truncate`).
 *
 * Tool output reaches this surface already truncated at
 * `conversation.tool_output_max_tokens`, and the success banner sits in the
 * surviving head — so a large patch looks like a clean success with a hole
 * punched through its diff. Counting that hole would attribute the tail's lines
 * to the wrong file and drop any file whose header fell inside it, then print
 * the total as fact. A number we cannot stand behind is worse than no number.
 */
const TRUNCATED = /…\[head\+tail of .*tokens omitted\]…/;

/** How many of a turn's patches came back too long to count. */
export function truncatedPatchCount(tools: (Cell & { kind: "tool" })[]): number {
  return tools.filter((t) => t.done && applied(t.output) && TRUNCATED.test(t.output)).length;
}

/**
 * The tool cells belonging to the newest turn.
 *
 * A turn is everything after the last user message. That makes the card
 * describe only the working tree as it stands: the next message pushes a new
 * user cell and the previous turn's card retires itself, rather than leaving a
 * trail of cards describing a tree that has since moved on. A steer lands as a
 * user cell mid-turn, so a steered turn is scoped to its post-steer segment.
 */
export function turnEditCells(cells: Cell[]): (Cell & { kind: "tool" })[] {
  let start = 0;
  for (let i = cells.length - 1; i >= 0; i--) {
    if (cells[i]!.kind === "user") {
      start = i + 1;
      break;
    }
  }
  return cells
    .slice(start)
    .filter((c): c is Cell & { kind: "tool" } => c.kind === "tool" && EDIT_TOOLS.has(c.name));
}

/** Strips the `a/` or `b/` prefix a unified-diff header carries. */
function stripPrefix(path: string): string {
  return path.replace(/^[ab]\//, "");
}

/**
 * Splits one patch result's diff into per-file sections with +/- counts.
 *
 * Header detection needs all three lines of the header, because diff content
 * can forge the first two: a removed `-- x` beside an added `++ y` renders as
 * `--- x` / `+++ y` at column zero, identical to a header. Only a real header
 * is followed by a hunk marker, and a real diff always has one — the daemon
 * emits nothing at all for a no-op. A delete names `/dev/null` as its
 * destination, so the source side supplies the path.
 */
function parseDiff(diff: string): FileEdit[] {
  const lines = diff.split("\n");
  const edits: { edit: FileEdit; lines: string[] }[] = [];
  // Diffs run to thousands of lines; `+=` on a string would copy the whole
  // accumulated body once per line.
  let current: { edit: FileEdit; lines: string[] } | undefined;

  for (let i = 0; i < lines.length; i++) {
    const line = lines[i]!;
    const next = lines[i + 1];
    if (line.startsWith("--- ") && next?.startsWith("+++ ") && lines[i + 2]?.startsWith("@@")) {
      const from = line.slice(4).trim();
      const to = next.slice(4).trim();
      const path = to === "/dev/null" ? stripPrefix(from) : stripPrefix(to);
      current = { edit: { path, added: 0, removed: 0, diff: "" }, lines: [line, next] };
      edits.push(current);
      i++;
      continue;
    }
    if (!current) continue;
    current.lines.push(line);
    if (line.startsWith("+")) current.edit.added++;
    else if (line.startsWith("-")) current.edit.removed++;
  }
  return edits.map(({ edit, lines: body }) => ({ ...edit, diff: body.join("\n") }));
}

/**
 * What a turn changed on disk, one row per file.
 *
 * Read entirely off the tool cells already in the transcript — the daemon
 * prints the unified diff it applied, so the counts cost no extra call and
 * cannot disagree with what actually landed. Files whose diff came back empty
 * are left out: an unchanged file is not a change, and a row of zeroes reads
 * like a bug.
 */
export function collectEdits(tools: (Cell & { kind: "tool" })[]): FileEdit[] {
  const byPath = new Map<string, FileEdit>();

  for (const tool of tools) {
    if (!tool.done || !applied(tool.output)) continue;
    // Counted from a diff with a hole in it, the totals would be wrong without
    // looking wrong. The card says so instead; see `truncatedPatchCount`.
    if (TRUNCATED.test(tool.output)) continue;
    for (const edit of parseDiff(tool.output)) {
      const existing = byPath.get(edit.path);
      // A turn that edits one file twice reports one row with the totals.
      if (existing) {
        existing.added += edit.added;
        existing.removed += edit.removed;
        existing.diff += `\n\n${edit.diff}`;
      } else {
        byPath.set(edit.path, edit);
      }
    }
  }
  return [...byPath.values()];
}
