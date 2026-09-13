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

/**
 * Text longer than this is attached as a file rather than typed into the box.
 *
 * A pasted log, stack trace, or whole source file is content to hand over, not
 * a message to edit: dropped inline it fills the textarea past its height cap,
 * buries the question the user was about to type under it, and makes the
 * Enter key a gamble. A few thousand characters is well past anything typed by
 * hand and short of most things pasted by mistake.
 */
export const PASTE_AS_FILE_CHARS = 3_000;

/**
 * The pasted text as a plain-text file, or null when it is short enough to
 * stay in the box.
 *
 * `ordinal` numbers a second paste so its chip is not a twin of the first.
 * The bytes reach the model as a labelled document, which is why the name
 * says what it is.
 */
export function pastedTextFile(text: string, ordinal = 1): File | null {
  if (text.length < PASTE_AS_FILE_CHARS) return null;
  const name = ordinal > 1 ? `pasted-text-${ordinal}.txt` : "pasted-text.txt";
  return new File([text], name, { type: "text/plain" });
}
