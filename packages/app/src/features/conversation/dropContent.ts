/** The slice of a `DataTransfer` a drag handler reads. */
export interface DragPayload {
  types: readonly string[];
  files: ArrayLike<File>;
  getData: (format: string) => string;
}

/**
 * Whether the drag carries files.
 *
 * Read from `types` because that is all a `dragover` is allowed to see: the
 * file list itself stays empty until the drop.
 */
export function carriesFiles(data: Pick<DragPayload, "types">): boolean {
  return Array.from(data.types).includes("Files");
}

/** Whether the drag carries a link or a run of text. */
export function carriesText(data: Pick<DragPayload, "types">): boolean {
  const types = Array.from(data.types);
  return types.includes("text/uri-list") || types.includes("text/plain");
}

/**
 * The text a drop is carrying: its links when it has any, else its plain text.
 *
 * A `text/uri-list` may hold several entries and `#` comment lines; only the
 * addresses are kept, one per line.
 */
export function droppedText(data: Pick<DragPayload, "getData">): string {
  const links = data
    .getData("text/uri-list")
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter((line) => line !== "" && !line.startsWith("#"));
  if (links.length > 0) return links.join("\n");
  return data.getData("text/plain");
}

/**
 * A shortcut file the browser adds beside a dragged link.
 *
 * It is a pointer to the address, not something the user chose to attach, and
 * the address itself is already in the drop.
 */
export function isLinkShortcut(file: { name: string }): boolean {
  return /\.(url|webloc)$/i.test(file.name);
}

/**
 * What a drop asks the composer to do: attach `files`, or write `text`.
 *
 * Files win when both are present — an image dragged off a page carries its
 * address too, and the picture is what was dragged. Shortcut files are dropped
 * only when an address came with them; alone, they are reported like any other
 * file Borg cannot read.
 */
export function readDrop(data: DragPayload): { files: File[]; text: string } {
  const text = droppedText(data);
  const all = Array.from(data.files);
  const files = text === "" ? all : all.filter((f) => !isLinkShortcut(f));
  return files.length > 0 ? { files, text: "" } : { files: [], text };
}

/** The draft with `text` written over the selection, and where the caret lands. */
export function insertAt(
  draft: string,
  text: string,
  start: number,
  end: number,
): { text: string; caret: number } {
  const from = Math.max(0, Math.min(start, draft.length));
  const to = Math.max(from, Math.min(end, draft.length));
  return { text: draft.slice(0, from) + text + draft.slice(to), caret: from + text.length };
}
