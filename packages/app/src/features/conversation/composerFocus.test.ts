import { afterEach, describe, expect, it } from "vitest";
import { focusComposer, registerComposer } from "./composerFocus";

function mount<K extends keyof HTMLElementTagNameMap>(tag: K): HTMLElementTagNameMap[K] {
  const el = document.createElement(tag);
  document.body.appendChild(el);
  return el;
}

afterEach(() => {
  document.body.replaceChildren();
});

describe("focusComposer", () => {
  it("moves focus from a used control to the registered box", () => {
    const box = mount("textarea");
    const button = mount("button");
    button.focus();
    const unregister = registerComposer(() => box);

    expect(focusComposer()).toBe(true);
    expect(document.activeElement).toBe(box);
    unregister();
  });

  it.each(["input", "textarea"] as const)(
    "leaves the caret alone while the user types in another %s",
    (tag) => {
      const box = mount("textarea");
      const other = mount(tag);
      other.focus();
      const unregister = registerComposer(() => box);

      expect(focusComposer()).toBe(false);
      expect(document.activeElement).toBe(other);
      unregister();
    },
  );

  it("takes focus back from a file input, which holds no caret", () => {
    const box = mount("textarea");
    const file = mount("input");
    file.type = "file";
    file.focus();
    const unregister = registerComposer(() => box);

    expect(focusComposer()).toBe(true);
    expect(document.activeElement).toBe(box);
    unregister();
  });

  it("does nothing once the composer is gone", () => {
    const box = mount("textarea");
    registerComposer(() => box)();

    expect(focusComposer()).toBe(false);
    expect(document.activeElement).not.toBe(box);
  });

  it("does nothing while the box is not on screen yet", () => {
    const unregister = registerComposer(() => null);

    expect(focusComposer()).toBe(false);
    unregister();
  });

  it("keeps the newer composer when an older one unregisters late", () => {
    const older = mount("textarea");
    const newer = mount("textarea");
    const dropOlder = registerComposer(() => older);
    const dropNewer = registerComposer(() => newer);

    dropOlder();

    expect(focusComposer()).toBe(true);
    expect(document.activeElement).toBe(newer);
    dropNewer();
  });
});
