import { describe, expect, it, vi } from "vitest";
import { act, render } from "@testing-library/react";
import { codeText, Markdown } from "./Markdown";

// Highlighting is async and irrelevant here; a never-settling promise keeps
// every block on its plain-text fallback while still recording the call.
const highlight = vi.hoisted(() => vi.fn(() => new Promise<string>(() => {})));
vi.mock("../lib/highlight", () => ({ highlight, peekHighlight: () => undefined }));

describe("Markdown", () => {
  it("drops raw HTML from model output", () => {
    // Model output is untrusted. A rendered <img onerror> or <iframe> in the
    // transcript is an injection surface, so the raw tag must never become a
    // node — it may only survive as text.
    const { container } = render(
      <Markdown>{`<img src=x onerror="alert(1)"> and <iframe src="evil"></iframe>`}</Markdown>,
    );
    expect(container.querySelector("img")).toBeNull();
    expect(container.querySelector("iframe")).toBeNull();
  });

  it("opens links in a new tab without leaking the referrer", () => {
    const { container } = render(<Markdown>{`[docs](https://example.com)`}</Markdown>);
    const link = container.querySelector("a");
    expect(link?.getAttribute("href")).toBe("https://example.com");
    expect(link?.getAttribute("target")).toBe("_blank");
    // Both tokens matter: noopener stops window.opener control, noreferrer
    // stops the transcript URL reaching the destination.
    expect(link?.getAttribute("rel")).toContain("noopener");
    expect(link?.getAttribute("rel")).toContain("noreferrer");
  });

  it("renders single-line inline code inline, not as a block", () => {
    const { container } = render(<Markdown>{"use \`borg send\` for that"}</Markdown>);
    expect(container.querySelector("code")?.textContent).toBe("borg send");
    expect(container.querySelector("pre")).toBeNull();
  });

  it("renders a fenced block as a block even when it is one line", () => {
    const { container } = render(<Markdown>{"```bash\nls -la\n```"}</Markdown>);
    // Before the async highlight lands, CodeBlock renders the raw text in a pre.
    expect(container.querySelector("pre")?.textContent).toBe("ls -la");
  });

  it("renders multi-line inline-styled code as a block", () => {
    // A newline is the tell: react-markdown gives an indented block no language
    // class, and inlining it would collapse the lines.
    const { container } = render(<Markdown>{"    one\n    two"}</Markdown>);
    expect(container.querySelector("pre")?.textContent).toBe("one\ntwo");
  });

  it("hands the highlighter the exact fence language and its text", () => {
    // The fence class is `language-bash`; the extraction must yield the whole
    // tag, not a prefix of it, and the trailing fence newline must be gone.
    vi.useFakeTimers();
    try {
      render(<Markdown>{"```bash\nls -la\n```"}</Markdown>);
      act(() => {
        vi.advanceTimersByTime(200);
      });
      expect(highlight).toHaveBeenCalledWith("ls -la", "bash");
    } finally {
      vi.useRealTimers();
    }
  });

  it("routes an empty fence to the block path, not inline code", () => {
    // No text and no newline, but the language class alone marks it a block.
    const { container } = render(<Markdown>{"```bash\n```"}</Markdown>);
    const pre = container.querySelector("pre");
    expect(pre).not.toBeNull();
    expect(pre?.textContent).toBe("");
  });

  it("does not double-wrap a fence in nested pre elements", () => {
    const { container } = render(<Markdown>{"```js\nconst a = 1;\n```"}</Markdown>);
    expect(container.querySelectorAll("pre")).toHaveLength(1);
  });

  it("renders a GFM table, which plain markdown would leave as text", () => {
    const { container } = render(<Markdown>{"| a | b |\n| - | - |\n| 1 | 2 |"}</Markdown>);
    const table = container.querySelector("table");
    expect(table).not.toBeNull();
    expect(container.querySelectorAll("th")).toHaveLength(2);
    expect(container.querySelectorAll("td")).toHaveLength(2);
  });

  it("renders GFM strikethrough, which plain markdown would not", () => {
    const { container } = render(<Markdown>{"~~gone~~"}</Markdown>);
    expect(container.querySelector("del")?.textContent).toBe("gone");
  });

  // A single tilde is how people write "about" and a range. Two of them in
  // one paragraph must not strike out the text between them.
  it.each([
    ["two ranges", "Pages 1~10,11~20 are done"],
    ["a range and an estimate", "Takes 3~5 days and costs ~¥0.089 per call"],
  ])("leaves single tildes as text: %s", (_name, source) => {
    const { container } = render(<Markdown>{source}</Markdown>);
    expect(container.querySelector("del")).toBeNull();
    expect(container.textContent).toBe(source);
  });

  it("keeps a single line break the reply was written with", () => {
    // An address arrives one part per line; folded into one line it reads as
    // a sentence.
    const { container } = render(<Markdown>{"Name\nStreet\nCity"}</Markdown>);
    const paragraph = container.querySelector("p");
    expect(paragraph?.querySelectorAll("br")).toHaveLength(2);
    expect(paragraph?.innerHTML).toMatch(/^Name<br>\s*Street<br>\s*City$/);
  });

  it.each([
    ["Windows", "a\r\nb"],
    ["old Mac", "a\rb"],
  ])("keeps a %s line break with no stray carriage return", (_name, source) => {
    const { container } = render(<Markdown>{source}</Markdown>);
    const paragraph = container.querySelector("p");
    expect(paragraph?.querySelectorAll("br")).toHaveLength(1);
    expect(paragraph?.innerHTML).toMatch(/^a<br>\s*b$/);
    expect(paragraph?.innerHTML).not.toContain("\r");
  });

  it("keeps line breaks inside list items and emphasis", () => {
    const { container } = render(<Markdown>{"- one\n  two\n\n*a\nb*"}</Markdown>);
    expect(container.querySelector("li br")).not.toBeNull();
    expect(container.querySelector("em br")).not.toBeNull();
  });

  it("still separates paragraphs on a blank line, with no break added", () => {
    const { container } = render(<Markdown>{"first\n\nsecond"}</Markdown>);
    expect(container.querySelectorAll("p")).toHaveLength(2);
    expect(container.querySelector("br")).toBeNull();
  });

  it("leaves the newlines in a fenced block as they are", () => {
    const { container } = render(<Markdown>{"```\none\ntwo\n```"}</Markdown>);
    expect(container.querySelector("pre")?.textContent).toBe("one\ntwo");
    expect(container.querySelector("br")).toBeNull();
  });

  it("leaves inline code alone beside a kept line break", () => {
    // Markdown itself folds a newline inside a code span into a space; the
    // span must come out whole, with no break put inside it.
    const { container } = render(<Markdown>{"run `a\nb` now\nthen stop"}</Markdown>);
    const code = container.querySelector("code");
    expect(code?.textContent).toBe("a b");
    expect(code?.querySelector("br")).toBeNull();
    expect(container.querySelectorAll("p > br")).toHaveLength(1);
  });

  it("renders a table with no breaks added to it", () => {
    const { container } = render(
      <Markdown>{"| a | b |\n| - | - |\n| 1 | 2 |\n| 3 | 4 |"}</Markdown>,
    );
    expect(container.querySelector("table br")).toBeNull();
    expect([...container.querySelectorAll("tr")].map((r) => r.textContent)).toEqual([
      "ab",
      "12",
      "34",
    ]);
  });

  it("renders headings, lists and blockquotes with the prose styles", () => {
    const { container } = render(
      <Markdown>{"# H1\n## H2\n### H3\n\n- one\n\n1. two\n\n> quoted\n\n---"}</Markdown>,
    );
    expect(container.querySelector("h1")?.textContent).toBe("H1");
    expect(container.querySelector("h2")?.textContent).toBe("H2");
    expect(container.querySelector("h3")?.textContent).toBe("H3");
    expect(container.querySelector("ul li")?.textContent).toBe("one");
    expect(container.querySelector("ol li")?.textContent).toBe("two");
    expect(container.querySelector("blockquote")?.textContent).toContain("quoted");
    expect(container.querySelector("hr")).not.toBeNull();
  });
});

describe("codeText", () => {
  it("flattens nested children into one string, in order", () => {
    expect(codeText(["a", ["b", "c"], "d"])).toBe("abcd");
  });

  it("drops anything that is not text", () => {
    expect(codeText(<em>styled</em>)).toBe("");
  });
});
