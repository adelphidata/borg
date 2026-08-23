import { describe, expect, it } from "vitest";
import { render } from "@testing-library/react";
import { Markdown } from "./Markdown";

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
