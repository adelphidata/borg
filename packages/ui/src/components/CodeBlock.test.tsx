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

  it("drops stale markup when a later pass fails", async () => {
    // The fallback branch must actively reset to plain text: keeping the old
    // markup would show highlighted code that no longer matches the block.
    highlight
      .mockResolvedValueOnce('<pre class="shiki">old</pre>')
      .mockRejectedValueOnce(new Error("grammar load failed"));
    const { container, rerender } = render(<CodeBlock code="old" lang="rust" />);
    await settle();
    expect(container.querySelector(".shiki")).not.toBeNull();

    rerender(<CodeBlock code="new" lang="brainfuck" />);
    await settle();
    expect(container.querySelector(".shiki")).toBeNull();
    expect(container.querySelector("pre")?.textContent).toBe("new");
  });

  it("ignores a failure that lands after the code changed again", async () => {
    // The stale-guard has to hold on the rejection path too, or a slow failing
    // pass blanks the markup the newer pass already painted.
    let rejectFirst: (err: Error) => void = () => {};
    highlight
      .mockImplementationOnce(() => new Promise<string>((_, r) => (rejectFirst = r)))
      .mockResolvedValueOnce('<pre class="second">second</pre>');

    const { container, rerender } = render(<CodeBlock code="first" lang="rust" />);
    act(() => {
      vi.advanceTimersByTime(200);
    });
    rerender(<CodeBlock code="second" lang="rust" />);
    await settle();
    expect(container.querySelector(".second")).not.toBeNull();

    await act(async () => {
      rejectFirst(new Error("late failure"));
    });
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

  it("restarts the Copied window on a second copy", async () => {
    // Without clearing the first timer, a re-copy flips back to "Copy code"
    // on the first timer's schedule, mid-way through the second window.
    highlight.mockResolvedValue(null);
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, "clipboard", { configurable: true, value: { writeText } });

    render(<CodeBlock code="x" />);
    await act(async () => {
      screen.getByRole("button", { name: "Copy code" }).click();
    });
    await act(async () => {
      vi.advanceTimersByTime(1000);
    });
    await act(async () => {
      screen.getByRole("button", { name: "Copied" }).click();
    });
    await act(async () => {
      vi.advanceTimersByTime(900); // 1900ms after the first copy, 900 after the second
    });
    expect(screen.getByRole("button", { name: "Copied" })).toBeTruthy();
    await act(async () => {
      vi.advanceTimersByTime(600); // second window (1400ms) elapsed
    });
    expect(screen.getByRole("button", { name: "Copy code" })).toBeTruthy();
  });

  it("cancels the pending label restore when it unmounts", async () => {
    // The unmount cleanup is the only thing that stops the restore timer
    // firing into a component that no longer exists.
    highlight.mockResolvedValue(null);
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, "clipboard", { configurable: true, value: { writeText } });

    const { unmount } = render(<CodeBlock code="x" />);
    await settle();
    await act(async () => {
      screen.getByRole("button", { name: "Copy code" }).click();
    });
    expect(vi.getTimerCount()).toBe(1);
    unmount();
    expect(vi.getTimerCount()).toBe(0);
  });
});
