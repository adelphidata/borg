/**
 * Whether a paste that landed somewhere on the page belongs to the composer.
 *
 * ⌘V is muscle memory, and on a conversation page there is only one thing a
 * paste could sensibly mean. Copy a screenshot, hit paste, and the image should
 * attach — without first hunting for the box and clicking it.
 *
 * The one rule is not to steal a paste someone aimed at another box: the search
 * field, a rename input, or an editable transcript turn all handle their own.
 * Selects are included because a paste inside one is a type-ahead, not text.
 */
export function composerOwnsPaste(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return true;
  // `closest` rather than the element's own flag: a paste inside a rich-text
  // editor is delivered to whichever node holds the caret, which is usually a
  // child of the editable element rather than the editable element itself.
  if (target.closest("[contenteditable]:not([contenteditable='false'])")) return false;
  const tag = target.tagName;
  return tag !== "INPUT" && tag !== "TEXTAREA" && tag !== "SELECT";
}

/**
 * The draft after pasted text is appended, plus a space when the existing draft
 * needs one.
 *
 * A paste onto the end of a word would otherwise weld the two together. A
 * draft that already ends in whitespace is left exactly as typed — someone
 * mid-newline meant that newline.
 */
export function appendPastedText(draft: string, pasted: string): string {
  if (draft === "") return pasted;
  return /\s$/.test(draft) ? draft + pasted : `${draft} ${pasted}`;
}
