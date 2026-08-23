import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import type { ComponentProps } from "react";
import { Pill } from "./Pill";

/** The class tokens a Pill rendered with `props` ends up with. */
function tokensOf(props: Omit<ComponentProps<typeof Pill>, "children"> = {}): Set<string> {
  const { container, unmount } = render(<Pill {...props}>x</Pill>);
  const button = container.querySelector("button");
  if (!button) throw new Error("Pill rendered no button");
  const tokens = new Set(Array.from(button.classList));
  unmount();
  return tokens;
}

describe("Pill", () => {
  it("never submits a surrounding form by accident", () => {
    // A bare <button> inside a form defaults to type=submit; the pills sit in
    // the onboarding form, where that would send a half-filled step.
    render(<Pill>Continue</Pill>);
    expect(screen.getByRole("button", { name: "Continue" })).toHaveProperty("type", "button");
  });

  it("calls the caller's handler", () => {
    const onClick = vi.fn();
    render(<Pill onClick={onClick}>Start</Pill>);
    fireEvent.click(screen.getByRole("button", { name: "Start" }));
    expect(onClick).toHaveBeenCalledTimes(1);
  });

  it("passes button attributes through to the element", () => {
    const onClick = vi.fn();
    render(
      <Pill disabled onClick={onClick} aria-label="Retry send">
        Retry
      </Pill>,
    );
    const button = screen.getByRole("button", { name: "Retry send" });
    expect(button).toHaveProperty("disabled", true);
    fireEvent.click(button);
    expect(onClick).not.toHaveBeenCalled();
  });

  it("renders each variant with its own distinct, non-empty look", () => {
    const variants = ["default", "primary", "danger"] as const;
    const byVariant = new Map(variants.map((v) => [v, tokensOf({ variant: v })]));

    // What every variant shares is the base; a variant must add to it.
    const common = [...byVariant.values()].reduce(
      (acc, tokens) => new Set([...acc].filter((t) => tokens.has(t))),
    );
    for (const [variant, tokens] of byVariant) {
      expect(tokens.has("undefined"), `${variant} leaked an undefined class`).toBe(false);
      expect(tokens.size, `${variant} adds nothing beyond the shared base`).toBeGreaterThan(
        common.size,
      );
      for (const [other, otherTokens] of byVariant) {
        if (other === variant) continue;
        expect([...tokens].sort(), `${variant} looks the same as ${other}`).not.toEqual(
          [...otherTokens].sort(),
        );
      }
    }
  });

  it("defaults to the default variant", () => {
    expect(tokensOf()).toEqual(tokensOf({ variant: "default" }));
  });

  it("is fully round by default and 8px-cornered when square", () => {
    const round = tokensOf();
    expect(round.has("rounded-full")).toBe(true);
    expect(round.has("rounded-lg")).toBe(false);
    const square = tokensOf({ square: true });
    expect(square.has("rounded-lg")).toBe(true);
    expect(square.has("rounded-full")).toBe(false);
  });

  it("applies default sizing that a caller can replace", () => {
    expect(tokensOf().has("px-4")).toBe(true);
    const sized = tokensOf({ sizeClasses: "px-8 py-4" });
    expect(sized.has("px-8")).toBe(true);
    expect(sized.has("px-4")).toBe(false);
  });

  it("appends a caller className, and only that", () => {
    const custom = tokensOf({ className: "custom-marker" });
    expect(custom.has("custom-marker")).toBe(true);
    // Its own classes survive the append.
    expect(custom.has("rounded-full")).toBe(true);

    const plain = tokensOf();
    expect(plain.has("undefined")).toBe(false);
    expect([...custom].filter((t) => !plain.has(t))).toEqual(["custom-marker"]);
    expect([...plain].filter((t) => !custom.has(t))).toEqual([]);
  });
});
