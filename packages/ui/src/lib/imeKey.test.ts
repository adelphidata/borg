import { describe, expect, it } from "vitest";

import { isImeKey, isSubmitEnter } from "./imeKey";

describe("isSubmitEnter", () => {
  it.each([
    ["a plain Enter", { key: "Enter" }, true],
    ["a React event with an idle native event", { key: "Enter", nativeEvent: {} }, true],
    ["Enter during composition", { key: "Enter", isComposing: true }, false],
    [
      "a React Enter during composition",
      { key: "Enter", nativeEvent: { isComposing: true } },
      false,
    ],
    ["the 229 Enter an IME sends after composition ends", { key: "Enter", keyCode: 229 }, false],
    [
      "a React 229 Enter",
      { key: "Enter", keyCode: 229, nativeEvent: { isComposing: false, keyCode: 229 } },
      false,
    ],
    ["another key", { key: "a" }, false],
    ["a synthetic keydown with no key", {}, false],
  ])("%s", (_name, event, expected) => {
    expect(isSubmitEnter(event)).toBe(expected);
  });
});

describe("isImeKey", () => {
  it("flags a composing Escape so it cannot cancel a turn", () => {
    expect(isImeKey({ key: "Escape", isComposing: true })).toBe(true);
  });

  it("passes an ordinary Escape", () => {
    expect(isImeKey({ key: "Escape", keyCode: 27 })).toBe(false);
  });
});
