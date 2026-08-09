import { useEffect, useRef, useState } from "react";
import { highlight } from "../lib/highlight";
import { CheckIcon, CopyIcon } from "./icons";

/**
 * A fenced code block: highlighted, horizontally scrollable, copyable.
 *
 * Highlighting is async (grammars load on demand) and streams re-run it as the
 * fence grows, so the raw text renders immediately and the coloured markup
 * replaces it when ready. That keeps a streaming reply readable instead of
 * blank.
 */
export function CodeBlock({ code, lang }: { code: string; lang?: string }) {
  const [html, setHtml] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  const timer = useRef<ReturnType<typeof setTimeout>>(undefined);

  useEffect(() => {
    let live = true;
    highlight(code, lang)
      .then((result) => {
        if (live) setHtml(result);
      })
      .catch(() => {
        // An unloadable grammar is not worth surfacing — plain text still reads.
        if (live) setHtml(null);
      });
    return () => {
      live = false;
    };
  }, [code, lang]);

  useEffect(() => () => clearTimeout(timer.current), []);

  const copy = () => {
    void navigator.clipboard.writeText(code).then(() => {
      setCopied(true);
      clearTimeout(timer.current);
      timer.current = setTimeout(() => setCopied(false), 1400);
    });
  };

  return (
    <div className="group relative my-3">
      <button
        type="button"
        aria-label={copied ? "Copied" : "Copy code"}
        onClick={copy}
        className="absolute top-2 right-2 z-10 flex h-7 w-7 cursor-pointer items-center justify-center rounded-md border border-line2 bg-surface2 text-ink3 opacity-0 transition-opacity group-hover:opacity-100 focus-visible:opacity-100 hover:text-ink"
      >
        {copied ? <CheckIcon size={13} /> : <CopyIcon size={13} />}
      </button>
      {html ? (
        <div
          className="borg-code overflow-x-auto rounded-[10px] border border-line2"
          // Shiki output, built from `code` on this machine — no remote HTML.
          dangerouslySetInnerHTML={{ __html: html }}
        />
      ) : (
        <pre className="borg-code m-0 overflow-x-auto rounded-[10px] border border-line2 bg-surface3 p-3 text-[12px] leading-[1.55] text-ink2">
          <code>{code}</code>
        </pre>
      )}
    </div>
  );
}
