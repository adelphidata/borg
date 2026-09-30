/** The slice of a keyboard event the composition check reads. */
export interface ComposableKeyEvent {
  key?: unknown;
  keyCode?: number;
  isComposing?: boolean;
  nativeEvent?: { isComposing?: boolean; keyCode?: number };
}

/**
 * True while an input method (Japanese, Chinese, Korean…) owns the keystroke.
 *
 * Some IMEs confirm a candidate with an Enter whose `isComposing` has already
 * gone false; that keydown still carries the legacy `keyCode` 229.
 */
export function isImeKey(e: ComposableKeyEvent): boolean {
  const native = e.nativeEvent ?? e;
  return native.isComposing === true || (native.keyCode ?? e.keyCode) === 229;
}

/** True for an Enter the user meant as "submit", never one that confirms an IME candidate. */
export function isSubmitEnter(e: ComposableKeyEvent): boolean {
  return e.key === "Enter" && !isImeKey(e);
}
