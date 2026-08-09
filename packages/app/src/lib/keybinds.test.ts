import { afterEach, describe, expect, it, vi } from "vitest";
import { renderHook } from "@testing-library/react";
import {
  chordOf,
  dispatchKeybind,
  installKeybinds,
  isEditable,
  resetKeybinds,
  useKeybind,
} from "./keybinds";

/**
 * Binds an action the way a component does, and returns its unbind.
 *
 * The registry is module-private on purpose, so tests go through the public
 * hook rather than reaching inside it.
 */
function bind(action: string, handler: () => void, inInputs = false): () => void {
  const { unmount } = renderHook(() => useKeybind(action, handler, { inInputs }));
  return unmount;
}

afterEach(() => {
  resetKeybinds();
});

function press(init: KeyboardEventInit & { key: string }, target?: Element) {
  const event = new KeyboardEvent("keydown", { bubbles: true, cancelable: true, ...init });
  (target ?? document.body).dispatchEvent(event);
  return event;
}

describe("chordOf", () => {
  const cases: [string, KeyboardEventInit & { key: string }, boolean, string][] = [
    ["plain letter", { key: "k" }, true, "k"],
    ["uppercases fold", { key: "K" }, true, "k"],
    ["cmd on mac is mod", { key: "k", metaKey: true }, true, "mod+k"],
    ["ctrl on windows is mod", { key: "k", ctrlKey: true }, false, "mod+k"],
    ["cmd on windows is the ctrl token", { key: "k", metaKey: true }, false, "ctrl+k"],
    ["ctrl on mac is the ctrl token", { key: "k", ctrlKey: true }, true, "ctrl+k"],
    ["shift orders last", { key: "F", metaKey: true, shiftKey: true }, true, "mod+shift+f"],
    ["named keys keep their name", { key: "Escape" }, true, "escape"],
    [
      "alt sits between ctrl and shift",
      { key: "p", altKey: true, shiftKey: true },
      true,
      "alt+shift+p",
    ],
  ];

  it.each(cases)("%s", (_name, init, isMac, expected) => {
    expect(chordOf(new KeyboardEvent("keydown", init), isMac)).toBe(expected);
  });
});

describe("isEditable", () => {
  it("is true for inputs, textareas, selects and contenteditable", () => {
    for (const tag of ["input", "textarea", "select"]) {
      expect(isEditable(document.createElement(tag))).toBe(true);
    }
    const div = document.createElement("div");
    div.contentEditable = "true";
    // jsdom does not derive isContentEditable from the attribute.
    Object.defineProperty(div, "isContentEditable", { value: true });
    expect(isEditable(div)).toBe(true);
  });

  it("is false for a plain element and for null", () => {
    expect(isEditable(document.createElement("div"))).toBe(false);
    expect(isEditable(null)).toBe(false);
  });
});

describe("installKeybinds", () => {
  it("runs the handler bound to a chord and stops the browser default", () => {
    const uninstall = installKeybinds(window, true);
    const handler = vi.fn();
    bind("command-palette", handler);

    const event = press({ key: "k", metaKey: true });

    expect(handler).toHaveBeenCalledOnce();
    expect(event.defaultPrevented).toBe(true);
    uninstall();
  });

  it("ignores a chord once the listener is removed", () => {
    const uninstall = installKeybinds(window, true);
    const handler = vi.fn();
    bind("command-palette", handler);
    uninstall();

    press({ key: "k", metaKey: true });

    expect(handler).not.toHaveBeenCalled();
  });

  it("lets a modifierless chord type normally inside a text field", () => {
    const uninstall = installKeybinds(window, true);
    const handler = vi.fn();
    bind("cancel-turn", handler);
    const input = document.createElement("input");
    document.body.appendChild(input);

    press({ key: "Escape" }, input);

    expect(handler).not.toHaveBeenCalled();
    input.remove();
    uninstall();
  });

  it("still fires a modifierless chord in a text field when inInputs is set", () => {
    const uninstall = installKeybinds(window, true);
    const handler = vi.fn();
    bind("cancel-turn", handler, true);
    const input = document.createElement("input");
    document.body.appendChild(input);

    press({ key: "Escape" }, input);

    expect(handler).toHaveBeenCalledOnce();
    input.remove();
    uninstall();
  });

  it("fires a modified chord even inside a text field", () => {
    const uninstall = installKeybinds(window, true);
    const handler = vi.fn();
    bind("command-palette", handler);
    const input = document.createElement("input");
    document.body.appendChild(input);

    press({ key: "k", metaKey: true }, input);

    expect(handler).toHaveBeenCalledOnce();
    input.remove();
    uninstall();
  });

  it("leaves an unbound chord alone so the browser keeps it", () => {
    const uninstall = installKeybinds(window, true);
    const event = press({ key: "p", metaKey: true });
    expect(event.defaultPrevented).toBe(false);
    uninstall();
  });
});

describe("dispatchKeybind", () => {
  it("reports false when nothing is bound", () => {
    expect(dispatchKeybind("new-conversation")).toBe(false);
  });

  it("runs the handler and reports true — this is the native menu's path", () => {
    const handler = vi.fn();
    bind("new-conversation", handler);

    expect(dispatchKeybind("new-conversation")).toBe(true);
    expect(handler).toHaveBeenCalledOnce();
  });

  it("gives the action to the most recent binder, then back on unmount", () => {
    const page = vi.fn();
    const overlay = vi.fn();
    bind("cancel-turn", page);
    const closeOverlay = bind("cancel-turn", overlay);

    dispatchKeybind("cancel-turn");
    expect(overlay).toHaveBeenCalledOnce();
    expect(page).not.toHaveBeenCalled();

    closeOverlay();
    dispatchKeybind("cancel-turn");
    expect(page).toHaveBeenCalledOnce();
  });
});
