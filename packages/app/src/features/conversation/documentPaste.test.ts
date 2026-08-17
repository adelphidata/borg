import { describe, expect, it } from "vitest";
import { appendPastedText, composerOwnsPaste } from "./documentPaste";

/** Builds a detached element, optionally content-editable. */
function el(tag: string, editable = false): HTMLElement {
  const node = document.createElement(tag);
  if (editable) node.setAttribute("contenteditable", "true");
  return node;
}

describe("composerOwnsPaste", () => {
  it("takes a paste aimed at nothing in particular", () => {
    // The whole point: ⌘V after copying a screenshot, without clicking first.
    expect(composerOwnsPaste(el("div"))).toBe(true);
    expect(composerOwnsPaste(null)).toBe(true);
  });

  it("leaves a paste aimed at another text box alone", () => {
    // Pasting into the session search field must search, not attach a file to
    // a conversation the user is not looking at.
    expect(composerOwnsPaste(el("input"))).toBe(false);
    expect(composerOwnsPaste(el("textarea"))).toBe(false);
  });

  it("leaves a select's type-ahead alone", () => {
    expect(composerOwnsPaste(el("select"))).toBe(false);
  });

  it("leaves a rich-text editor alone", () => {
    expect(composerOwnsPaste(el("div", true))).toBe(false);
  });

  it("leaves a paste inside a rich-text editor alone", () => {
    // The caret sits on a child node, so the event target is rarely the
    // editable element itself.
    const editor = el("div", true);
    const inner = el("span");
    editor.append(inner);
    expect(composerOwnsPaste(inner)).toBe(false);
  });

  it("still takes a paste in an explicitly non-editable region", () => {
    const off = el("div");
    off.setAttribute("contenteditable", "false");
    expect(composerOwnsPaste(off)).toBe(true);
  });
});

describe("appendPastedText", () => {
  it("uses the pasted text as the whole draft when nothing is typed", () => {
    expect(appendPastedText("", "pasted")).toBe("pasted");
  });

  it("separates the paste from a word it would otherwise weld to", () => {
    expect(appendPastedText("look at", "src/main.rs")).toBe("look at src/main.rs");
  });

  it("respects whitespace the user typed on purpose", () => {
    expect(appendPastedText("a list:\n", "- one")).toBe("a list:\n- one");
    expect(appendPastedText("look at ", "this")).toBe("look at this");
  });
});
