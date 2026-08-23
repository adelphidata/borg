import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, render, screen } from "@testing-library/react";
import { CodeBlock } from "./CodeBlock";

const highlight = vi.hoisted(() => vi.fn());
vi.mock("../lib/highlight", () => ({ highlight }));

beforeEach(() => {
  vi.useFakeTimers();
  highlight.mockReset();
});

afterEach(() => {
  vi.useRealTimers();
});

/** Let the debounce fire and the highlight promise settle. */
async function settle() {
  await act(async () => {
    vi.advanceTimersByTime(200);
  });
}

describe("CodeBlock", () => {
  it("shows the raw code immediately, before any highlighting", () => {
    // A streaming fence must be readable, not blank, while grammars load.
    highlight.mockReturnValue(new Promise(() => {}));
    render(<CodeBlock code="cargo build" lang="bash" />);
    expect(screen.getByText("cargo build")).toBeTruthy();
  });

  it("replaces the raw text with the highlighted markup when it arrives", async () => {
    highlight.mockResolvedValue('<pre class="shiki"><code>cargo</code></pre>');
    const { container } = render(<CodeBlock code="cargo" lang="bash" />);
    await settle();
    expect(container.querySelector(".shiki")).not.toBeNull();
  });

  it("keeps plain text when the language is unsupported", async () => {
    highlight.mockResolvedValue(null);
    const { container } = render(<CodeBlock code="???" />);
    await settle();
    expect(container.querySelector("pre")?.textContent).toBe("???");
  });

  it("keeps plain text when highlighting fails", async () => {
    // An unloadable grammar is not worth surfacing; the code still reads.
    highlight.mockRejectedValue(new Error("grammar load failed"));
    const { container } = render(<CodeBlock code="x = 1" lang="python" />);
    await settle();
    expect(container.querySelector("pre")?.textContent).toBe("x = 1");
  });

  it("tokenises once for a fence that streams in over many deltas", async () => {
    // Tokenising is O(block), not O(delta): without the debounce, a fence
    // arriving over hundreds of updates re-tokenises from the top each time.
    highlight.mockResolvedValue("<pre>done</pre>");
    const { rerender } = render(<CodeBlock code="a" lang="rust" />);
    for (const code of ["ab", "abc", "abcd"]) {
      rerender(<CodeBlock code={code} lang="rust" />);
      act(() => {
        vi.advanceTimersByTime(20);
      });
    }
    await settle();
    expect(highlight).toHaveBeenCalledTimes(1);
    expect(highlight).toHaveBeenLastCalledWith("abcd", "rust");
  });

  it("ignores a highlight that lands after the code changed again", async () => {
    // Otherwise a slow first pass paints over the newer block.
    let resolveFirst: (html: string) => void = () => {};
    highlight
      .mockImplementationOnce(() => new Promise<string>((r) => (resolveFirst = r)))
      .mockResolvedValueOnce('<pre class="second">second</pre>');

    const { container, rerender } = render(<CodeBlock code="first" lang="rust" />);
    act(() => {
      vi.advanceTimersByTime(200);
    });
    rerender(<CodeBlock code="second" lang="rust" />);
    await settle();
    await act(async () => {
      resolveFirst('<pre class="first">first</pre>');
    });

    expect(container.querySelector(".first")).toBeNull();
    expect(container.querySelector(".second")).not.toBeNull();
  });

  it("copies the code and says so, then goes back to offering the copy", async () => {
    highlight.mockResolvedValue(null);
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, "clipboard", { configurable: true, value: { writeText } });

    render(<CodeBlock code="cargo test" lang="bash" />);
    const button = screen.getByRole("button", { name: "Copy code" });
    await act(async () => {
      button.click();
    });
    expect(writeText).toHaveBeenCalledWith("cargo test");
    expect(screen.getByRole("button", { name: "Copied" })).toBeTruthy();

    await act(async () => {
      vi.advanceTimersByTime(1500);
    });
    expect(screen.getByRole("button", { name: "Copy code" })).toBeTruthy();
  });
});
