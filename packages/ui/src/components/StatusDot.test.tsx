import { describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
import type { ComponentProps } from "react";
import { StatusDot, type DotTone } from "./StatusDot";

const ALL_TONES: readonly DotTone[] = [
  "ok",
  "alert",
  "busy",
  "working",
  "unread",
  "draft",
  "paused",
  "idle",
];

/** The class tokens a dot rendered with `props` ends up with. */
function tokensOf(props: ComponentProps<typeof StatusDot> = {}): Set<string> {
  const { container, unmount } = render(<StatusDot {...props} />);
  const dot = container.firstElementChild;
  if (!dot) throw new Error("StatusDot rendered nothing");
  const tokens = new Set(Array.from(dot.classList));
  unmount();
  return tokens;
}

describe("StatusDot", () => {
  it("stays out of the accessibility tree when it has no label", () => {
    // The dot beside a row that already says "Running" would otherwise be
    // announced twice.
    const { container } = render(<StatusDot tone="ok" />);
    const dot = container.firstElementChild;
    expect(dot?.getAttribute("role")).toBeNull();
    expect(dot?.getAttribute("aria-label")).toBeNull();
  });

  it("announces itself as an image when it carries meaning of its own", () => {
    render(<StatusDot tone="alert" label="Needs attention" />);
    expect(screen.getByRole("img", { name: "Needs attention" })).toBeTruthy();
  });

  it("gives every tone a mark of its own", () => {
    const byTone = new Map(ALL_TONES.map((tone) => [tone, tokensOf({ tone })]));

    // The base the tones share; a tone must add to it or the vocabulary is gone.
    const common = [...byTone.values()].reduce(
      (acc, tokens) => new Set([...acc].filter((t) => tokens.has(t))),
    );
    for (const [tone, tokens] of byTone) {
      expect(tokens.has("undefined"), `${tone} leaked an undefined class`).toBe(false);
      expect(tokens.size, `${tone} adds nothing beyond the shared base`).toBeGreaterThan(
        common.size,
      );
      for (const [other, otherTokens] of byTone) {
        if (other === tone) continue;
        expect([...tokens].sort(), `${tone} looks the same as ${other}`).not.toEqual(
          [...otherTokens].sort(),
        );
      }
    }
  });

  it("pulses only for busy — working carries the same news at rest", () => {
    const pulses = (tokens: Set<string>) => [...tokens].some((t) => t.startsWith("animate-"));
    expect(pulses(tokensOf({ tone: "busy" }))).toBe(true);
    expect(pulses(tokensOf({ tone: "working" }))).toBe(false);
  });

  it("defaults to the ok tone", () => {
    expect(tokensOf()).toEqual(tokensOf({ tone: "ok" }));
  });

  it("appends a caller className, and only that", () => {
    const custom = tokensOf({ tone: "idle", className: "custom-marker" });
    expect(custom.has("custom-marker")).toBe(true);
    // The shape and tone survive the append.
    expect(custom.has("rounded-full")).toBe(true);

    const plain = tokensOf({ tone: "idle" });
    expect(plain.has("undefined")).toBe(false);
    expect([...custom].filter((t) => !plain.has(t))).toEqual(["custom-marker"]);
    expect([...plain].filter((t) => !custom.has(t))).toEqual([]);
  });
});
