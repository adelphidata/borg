import { describe, expect, it } from "vitest";
import { cleanup, render, waitFor } from "@testing-library/react";
import { renderToString } from "react-dom/server";
import { CodeBlock } from "./CodeBlock";

// The real highlighter, not a stand-in: what is under test is that the
// component and the cache agree on a key.
describe("CodeBlock on a second mount", () => {
  it("paints highlighted markup on its first render", async () => {
    // Reopening a conversation remounts every fence in it. Each one used to
    // show plain text and recolour a beat later.
    const first = render(<CodeBlock code="let x = 1;" lang="rs" />);
    await waitFor(() => expect(first.container.querySelector(".shiki")).not.toBeNull(), {
      timeout: 20000,
    });
    cleanup();

    // A server render runs no effects, so this is the first render alone.
    const html = renderToString(<CodeBlock code="let x = 1;" lang="rs" />);
    expect(html).toContain("shiki");
    expect(html).toContain("--shiki-light");
  }, 30000);

  it("starts from plain text for code it has not seen", () => {
    const html = renderToString(<CodeBlock code="let never_seen = 2;" lang="rs" />);
    expect(html).not.toContain("shiki");
    expect(html).toContain("let never_seen = 2;");
  });
});
