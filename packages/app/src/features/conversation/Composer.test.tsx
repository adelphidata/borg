import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { dispatchKeybind, resetKeybinds } from "../../lib/keybinds";
import type { Command } from "../palette/score";
import { Composer } from "./Composer";
import { draftKeyFor, moveDraft } from "./draftStore";

/**
 * The command list reaches the composer through the daemon-backed hook the
 * palette also uses; this file is about what the composer does with it.
 */
const registry = vi.hoisted(() => ({ commands: [] as Command[] }));
vi.mock("../palette/useCommands", () => ({
  useCommands: () => ({
    commands: registry.commands,
    saveError: undefined,
    clearSaveError: () => {},
  }),
}));

/** Attaching a real image needs blob URLs and a FileReader; stub the step. */
vi.mock("./attachments", () => ({
  collectImages: vi.fn(() =>
    Promise.resolve({
      attachments: [
        {
          mimeType: "image/png",
          data: new Uint8Array([1]),
          filename: "shot.png",
          previewUrl: "blob:shot",
        },
      ],
      errors: [],
    }),
  ),
}));

/** The pill talks to the daemon; this file is about the composer. */
vi.mock("./ModelPill", () => ({ ModelPill: () => null }));

const noop = () => {};

function setup(props: { draftKey: string; conversationKey: string }) {
  return render(<Composer busy={false} onSend={noop} onSteer={noop} onCancel={noop} {...props} />);
}

/** Attaches an image through the file picker, and waits for its chip. */
async function attach(container: HTMLElement) {
  const file = new File([new Uint8Array([1])], "shot.png", { type: "image/png" });
  const input = container.querySelector('input[type="file"]')!;
  fireEvent.change(input, { target: { files: [file] } });
  await waitFor(() => expect(screen.getByAltText("shot.png")).toBeTruthy());
}

beforeEach(() => {
  sessionStorage.clear();
  registry.commands = [];
  globalThis.URL.revokeObjectURL = vi.fn();
  // jsdom implements neither of these, and the composer reaches for both.
  window.matchMedia = vi.fn(() => ({
    matches: false,
    addEventListener: noop,
    removeEventListener: noop,
  })) as unknown as typeof window.matchMedia;
});

afterEach(() => {
  cleanup();
  resetKeybinds();
});

describe("Composer", () => {
  it("keeps an attachment when a new conversation is issued its id", async () => {
    // `/c/new` types under a provisional key until the daemon answers. An image
    // attached in that window used to vanish silently when the key changed.
    const { container, rerender } = setup({
      draftKey: draftKeyFor("new"),
      conversationKey: "new",
    });
    await attach(container);

    rerender(
      <Composer
        busy={false}
        draftKey={draftKeyFor("sess-1")}
        conversationKey="new"
        onSend={noop}
        onSteer={noop}
        onCancel={noop}
      />,
    );
    expect(screen.getByAltText("shot.png")).toBeTruthy();
  });

  it("drops the attachment when the user moves to another conversation", async () => {
    const { container, rerender } = setup({
      draftKey: draftKeyFor("sess-1"),
      conversationKey: "sess-1",
    });
    await attach(container);

    rerender(
      <Composer
        busy={false}
        draftKey={draftKeyFor("sess-2")}
        conversationKey="sess-2"
        onSend={noop}
        onSteer={noop}
        onCancel={noop}
      />,
    );
    expect(screen.queryByAltText("shot.png")).toBeNull();
  });

  it("still shows the typed text after the draft is re-keyed under the real id", () => {
    const { rerender } = setup({ draftKey: draftKeyFor("new"), conversationKey: "new" });
    const box = screen.getByRole("textbox");
    fireEvent.change(box, { target: { value: "half a thought" } });

    // What `useConversation` does once the daemon issues the id.
    moveDraft(draftKeyFor("new"), draftKeyFor("sess-1"));
    rerender(
      <Composer
        busy={false}
        draftKey={draftKeyFor("sess-1")}
        conversationKey="new"
        onSend={noop}
        onSteer={noop}
        onCancel={noop}
      />,
    );
    expect(screen.getByRole("textbox")).toHaveProperty("value", "half a thought");
  });
});

describe("the composer's slash menu", () => {
  function command(id: string, label: string, run: () => void): Command {
    return { id, label, run };
  }

  function type(value: string) {
    fireEvent.change(screen.getByRole("textbox"), { target: { value } });
  }

  function enter() {
    fireEvent.keyDown(screen.getByRole("textbox"), { key: "Enter" });
  }

  it("writes a skill row's prompt text and leaves the sending to the user", () => {
    // Choosing a skill is the agent's call. The row hands over the sentence it
    // would have taken to say so, and stops.
    const onSend = vi.fn();
    const noted = vi.fn();
    registry.commands = [
      { id: "skill:git", label: "git", insert: "Use the git skill to ", run: noted },
    ];
    render(
      <Composer
        busy={false}
        draftKey={draftKeyFor("s")}
        conversationKey="s"
        onSend={onSend}
        onSteer={noop}
        onCancel={noop}
      />,
    );

    type("/git");
    enter();

    expect(noted).toHaveBeenCalled();
    expect(screen.getByRole("textbox")).toHaveProperty("value", "Use the git skill to ");
    expect(onSend).not.toHaveBeenCalled();
    expect(screen.queryByRole("listbox")).toBeNull();
  });

  it("opens on a leading slash and offers the commands ⌘K offers", () => {
    registry.commands = [command("action:new", "New conversation", noop)];
    setup({ draftKey: draftKeyFor("s"), conversationKey: "s" });
    expect(screen.queryByRole("listbox")).toBeNull();

    type("/");
    expect(screen.getByRole("option", { name: /New conversation/ })).toBeTruthy();
  });

  it("stays shut for a slash inside ordinary prose", () => {
    registry.commands = [command("action:new", "New conversation", noop)];
    setup({ draftKey: draftKeyFor("s"), conversationKey: "s" });

    type("look at src/lib/mod.rs");
    expect(screen.queryByRole("listbox")).toBeNull();
  });

  it("runs the highlighted command on Enter instead of sending the token", () => {
    const run = vi.fn();
    const onSend = vi.fn();
    registry.commands = [command("action:new", "New conversation", run)];
    render(
      <Composer
        busy={false}
        draftKey={draftKeyFor("s")}
        conversationKey="s"
        onSend={onSend}
        onSteer={noop}
        onCancel={noop}
      />,
    );

    type("/new");
    enter();

    expect(run).toHaveBeenCalled();
    // The daemon has no slash handler: forwarding "/new" would silently do
    // nothing while looking exactly like a sent message.
    expect(onSend).not.toHaveBeenCalled();
    expect(screen.getByRole("textbox")).toHaveProperty("value", "");
    expect(screen.queryByRole("listbox")).toBeNull();
  });

  it("never steers a running turn with a slash token either", () => {
    const onSteer = vi.fn();
    registry.commands = [command("action:new", "New conversation", noop)];
    render(
      <Composer
        busy
        draftKey={draftKeyFor("s")}
        conversationKey="s"
        onSend={noop}
        onSteer={onSteer}
        onCancel={noop}
      />,
    );

    type("/new");
    enter();
    expect(onSteer).not.toHaveBeenCalled();
  });

  it("moves the highlight with the arrows rather than the caret", () => {
    const second = vi.fn();
    registry.commands = [
      command("a", "Alpha", noop),
      command("b", "Bravo", second),
      command("c", "Charlie", noop),
    ];
    setup({ draftKey: draftKeyFor("s"), conversationKey: "s" });

    type("/");
    fireEvent.keyDown(screen.getByRole("textbox"), { key: "ArrowDown" });
    expect(screen.getByRole("option", { name: "Bravo" }).getAttribute("aria-selected")).toBe(
      "true",
    );

    enter();
    expect(second).toHaveBeenCalled();
  });

  it("walks the highlight back up, and stops at the first row", () => {
    const first = vi.fn();
    registry.commands = [command("a", "Alpha", first), command("b", "Bravo", noop)];
    setup({ draftKey: draftKeyFor("s"), conversationKey: "s" });

    type("/");
    const box = screen.getByRole("textbox");
    fireEvent.keyDown(box, { key: "ArrowDown" });
    fireEvent.keyDown(box, { key: "ArrowUp" });
    fireEvent.keyDown(box, { key: "ArrowUp" });

    expect(screen.getByRole("option", { name: "Alpha" }).getAttribute("aria-selected")).toBe(
      "true",
    );
    enter();
    expect(first).toHaveBeenCalled();
  });

  it("keeps Escape on the menu even when the turn starts after it opened", () => {
    // `useKeybind` re-registers when `enabled` flips, so a turn beginning while
    // the menu is open used to push the cancel binding on top and take Escape.
    const onCancel = vi.fn();
    registry.commands = [command("a", "Alpha", noop)];
    const props = {
      draftKey: draftKeyFor("s"),
      conversationKey: "s",
      onSend: noop,
      onSteer: noop,
      onCancel,
    };
    const { rerender } = render(<Composer busy={false} {...props} />);

    type("/");
    rerender(<Composer busy {...props} />);

    act(() => void dispatchKeybind("cancel-turn"));
    expect(screen.queryByRole("listbox")).toBeNull();
    expect(onCancel).not.toHaveBeenCalled();
  });

  it("gives Escape to the menu first, and to a running turn only after", () => {
    const onCancel = vi.fn();
    registry.commands = [command("a", "Alpha", noop)];
    render(
      <Composer
        busy
        draftKey={draftKeyFor("s")}
        conversationKey="s"
        onSend={noop}
        onSteer={noop}
        onCancel={onCancel}
      />,
    );

    type("/");
    act(() => void dispatchKeybind("cancel-turn"));
    expect(screen.queryByRole("listbox")).toBeNull();
    expect(onCancel).not.toHaveBeenCalled();

    act(() => void dispatchKeybind("cancel-turn"));
    expect(onCancel).toHaveBeenCalled();
  });

  it("does not reopen on the next keystroke after it was dismissed", () => {
    registry.commands = [command("a", "Alpha", noop)];
    setup({ draftKey: draftKeyFor("s"), conversationKey: "s" });

    type("/");
    act(() => void dispatchKeybind("cancel-turn"));
    type("/al");
    expect(screen.queryByRole("listbox")).toBeNull();
  });

  it("offers itself again once the dismissed token is gone", () => {
    registry.commands = [command("a", "Alpha", noop)];
    setup({ draftKey: draftKeyFor("s"), conversationKey: "s" });

    type("/");
    act(() => void dispatchKeybind("cancel-turn"));
    type("");
    type("/");
    expect(screen.getByRole("listbox")).toBeTruthy();
  });

  it("says so when nothing matches, rather than showing an empty box", () => {
    registry.commands = [command("a", "Alpha", noop)];
    setup({ draftKey: draftKeyFor("s"), conversationKey: "s" });

    type("/zzzz");
    expect(screen.queryAllByRole("option")).toHaveLength(0);
    expect(screen.getByText(/No command matches/)).toBeTruthy();
  });

  it("sends a slash token that matches nothing, rather than swallowing it", () => {
    // `/tmp` and `/etc/hosts` are things people say. With no row on offer the
    // token is ordinary text, and Enter must not be a silent no-op.
    const onSend = vi.fn();
    registry.commands = [command("a", "Alpha", noop)];
    render(
      <Composer
        busy={false}
        draftKey={draftKeyFor("s")}
        conversationKey="s"
        onSend={onSend}
        onSteer={noop}
        onCancel={noop}
      />,
    );

    type("/zzzz");
    enter();
    expect(onSend).toHaveBeenCalledWith("/zzzz", []);
  });

  it("keeps the menu open for a row that is picked by watching it land", () => {
    // Theme and toggle rows carry `keepOpen` in the palette; the same row
    // reached through `/` has to behave the same way.
    const run = vi.fn();
    registry.commands = [{ id: "theme:dark", label: "Dark", run, keepOpen: true }];
    setup({ draftKey: draftKeyFor("s"), conversationKey: "s" });

    type("/dark");
    enter();

    expect(run).toHaveBeenCalled();
    expect(screen.getByRole("textbox")).toHaveProperty("value", "/dark");
    expect(screen.getByRole("listbox")).toBeTruthy();
  });
});
