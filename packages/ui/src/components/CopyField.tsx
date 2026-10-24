import { useEffect, useRef, useState, type ReactNode } from "react";
import { CopyIcon } from "./icons";
import { SECTION_LABEL } from "./pageClasses";

/** How long "Copied" stays on the button before it reads "Copy" again. */
export const COPIED_MS = 1400;

/** What the copy button says: ready, done, or the clipboard refused. */
type CopyState = "idle" | "copied" | "failed";

/**
 * A value someone has to paste somewhere else — a URL, a key — with its own
 * copy button.
 *
 * The value is in monospace and selectable, so copying by hand still works
 * where `navigator.clipboard` does not exist (an insecure origin, which is the
 * gateway-served page on a LAN address). There the button says it could not
 * copy instead of claiming it did.
 *
 * `secret` hides the value behind dots until Show is pressed. Copy still copies
 * the real value: hiding is about the screen, not the clipboard.
 */
export function CopyField({
  label,
  value,
  secret = false,
  actions,
}: {
  /** Shown above the value, and names the buttons: "Copy url", "Show key". */
  label: string;
  value: string;
  secret?: boolean;
  /** Extra controls at the end of the row, e.g. "Replace key". */
  actions?: ReactNode;
}) {
  const [shown, setShown] = useState(false);
  // Keyed by the value it was copied from: a field that is handed a new value
  // (a replaced key) must not still read "Copied" for one nobody copied.
  const [copy, setCopy] = useState<{ of: string; state: CopyState }>();
  const timer = useRef<ReturnType<typeof setTimeout>>(undefined);
  useEffect(() => () => clearTimeout(timer.current), []);

  const state: CopyState = copy?.of === value ? copy.state : "idle";
  const settle = (next: CopyState) => {
    setCopy({ of: value, state: next });
    clearTimeout(timer.current);
    timer.current = setTimeout(() => setCopy(undefined), COPIED_MS);
  };
  const onCopy = () => {
    const clipboard = navigator.clipboard;
    if (!clipboard) {
      settle("failed");
      return;
    }
    clipboard.writeText(value).then(
      () => settle("copied"),
      () => settle("failed"),
    );
  };

  const hidden = secret && !shown;
  const name = label.toLowerCase();
  const button =
    "flex-none cursor-pointer rounded-lg border border-line bg-transparent px-3 py-2 text-[12px] leading-none font-medium text-ink2 hover:border-neon hover:text-ink";

  return (
    <div>
      <div className={SECTION_LABEL}>{label}</div>
      <div className="flex flex-wrap items-center gap-2">
        <code
          aria-label={label}
          className="min-w-0 flex-1 rounded-[10px] border border-line2 bg-surface2 px-3 py-2.5 font-mono text-[12px] leading-[1.45] [overflow-wrap:anywhere] text-ink select-all"
        >
          {hidden ? "•".repeat(Math.min(Math.max(value.length, 8), 24)) : value}
        </code>
        {secret && (
          <button
            type="button"
            className={button}
            aria-label={`${shown ? "Hide" : "Show"} ${name}`}
            onClick={() => setShown((s) => !s)}
          >
            {shown ? "Hide" : "Show"}
          </button>
        )}
        <button
          type="button"
          className={`${button} flex items-center gap-1.5`}
          aria-label={`Copy ${name}`}
          onClick={onCopy}
        >
          <CopyIcon size={12} />
          {state === "copied" ? "Copied" : state === "failed" ? "Could not copy" : "Copy"}
        </button>
        {actions}
      </div>
      <span role="status" className="sr-only">
        {state === "copied"
          ? `${label} copied`
          : state === "failed"
            ? `Could not copy the ${name}. Select it and copy it by hand.`
            : ""}
      </span>
    </div>
  );
}
