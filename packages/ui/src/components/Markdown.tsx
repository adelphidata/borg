import { memo, type ReactNode } from "react";
import ReactMarkdown, { type Components } from "react-markdown";
import remarkGfm from "remark-gfm";
import { CodeBlock } from "./CodeBlock";

/**
 * Assistant prose.
 *
 * Deliberately narrow: GFM (tables, strikethrough, task lists) and nothing
 * else. No raw HTML — `react-markdown` drops it by default and we keep that,
 * because model output is untrusted text and a rendered `<img onerror>` or
 * `<iframe>` in the transcript is a real injection surface.
 *
 * Memoised on the source string: a streaming reply re-renders on every delta,
 * and reparsing unchanged prose for each of them is the expensive part.
 */

function codeText(children: ReactNode): string {
  if (typeof children === "string") return children;
  if (Array.isArray(children)) return children.map(codeText).join("");
  return "";
}

const components: Components = {
  code({ className, children, ...props }) {
    // react-markdown gives inline code no language class and no newline.
    const fence = /language-(\S+)/.exec(className ?? "")?.[1];
    const text = codeText(children);
    if (fence === undefined && !text.includes("\n")) {
      return (
        <code
          className="rounded-[5px] border border-line2 bg-surface3 px-[5px] py-[1.5px] text-[0.88em] text-ink"
          {...props}
        >
          {children}
        </code>
      );
    }
    return <CodeBlock code={text.replace(/\n$/, "")} lang={fence} />;
  },
  // The fence is rendered by `code` above; `pre` would double-wrap it.
  pre({ children }) {
    return <>{children}</>;
  },
  p({ children }) {
    return <p className="my-2 first:mt-0 last:mb-0">{children}</p>;
  },
  ul({ children }) {
    return <ul className="my-2 list-disc pl-[22px] first:mt-0 last:mb-0">{children}</ul>;
  },
  ol({ children }) {
    return <ol className="my-2 list-decimal pl-[22px] first:mt-0 last:mb-0">{children}</ol>;
  },
  li({ children }) {
    return <li className="my-[3px]">{children}</li>;
  },
  h1({ children }) {
    return <h1 className="mt-4 mb-2 text-[17px] font-medium first:mt-0">{children}</h1>;
  },
  h2({ children }) {
    return <h2 className="mt-4 mb-2 text-[15.5px] font-medium first:mt-0">{children}</h2>;
  },
  h3({ children }) {
    return <h3 className="mt-3.5 mb-1.5 text-[14.5px] font-medium first:mt-0">{children}</h3>;
  },
  blockquote({ children }) {
    return (
      <blockquote className="my-2 border-l-2 border-line pl-3.5 text-ink2 italic">
        {children}
      </blockquote>
    );
  },
  hr() {
    return <hr className="my-4 border-0 border-t border-line2" />;
  },
  a({ children, href }) {
    return (
      // Model-authored links leave the app; never let one navigate the window
      // itself, and never leak the referrer.
      <a href={href} target="_blank" rel="noreferrer noopener" className="underline">
        {children}
      </a>
    );
  },
  table({ children }) {
    return (
      <div className="my-3 overflow-x-auto rounded-[10px] border border-line2">
        <table className="w-full border-collapse text-[13px]">{children}</table>
      </div>
    );
  },
  th({ children }) {
    return (
      <th className="border-b border-line2 px-3 py-2 text-left font-medium text-ink2">
        {children}
      </th>
    );
  },
  td({ children }) {
    return <td className="border-b border-line2 px-3 py-2 align-top last:border-0">{children}</td>;
  },
};

export const Markdown = memo(function Markdown({ children }: { children: string }) {
  return (
    // `select-text` overrides the app-wide `user-select: none`; a transcript
    // you cannot copy out of is not a transcript.
    <div className="borg-prose text-[14.5px] leading-[1.6] text-pretty text-ink select-text">
      <ReactMarkdown remarkPlugins={[remarkGfm]} components={components}>
        {children}
      </ReactMarkdown>
    </div>
  );
});
