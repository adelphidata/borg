import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { dispatchKeybind, resetKeybinds } from "../../lib/keybinds";
import type { Command } from "../palette/score";
import { Composer } from "./Composer";
import { draftKeyFor, moveDraft, readDraft } from "./draftStore";

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

/**
 * Reading a real file needs blob URLs and an ArrayBuffer; stub the step. What
 * comes back is keyed off the file's own type, so a test can attach either
 * kind and the composer sees what it would really be handed.
 */
vi.mock("./attachments", () => ({
  collectAttachments: vi.fn((files: File[]) =>
    Promise.resolve({
      attachments: files.map((f) => ({
        kind: f.type.startsWith("image/") ? "image" : "document",
        mimeType: f.type,
        data: new Uint8Array([1]),
        filename: f.name,
        previewUrl: f.type.startsWith("image/") ? `blob:${f.name}` : undefined,
      })),
      errors: [],
    }),
  ),
}));

/** The pill talks to the daemon; this file is about the composer. */
vi.mock("./ModelPill", () => ({ ModelPill: () => null }));

/**
 * Path completions come from the daemon, scoped and filtered there. This file
 * is about what the composer does with the rows it is handed.
 */
const workspace = vi.hoisted(() => ({ rows: [] as { path: string; isDir: boolean }[] }));
vi.mock("./usePathCompletions", () => ({
  usePathCompletions: (sessionId: string | undefined, prefix: string | null) =>
    sessionId === undefined || prefix === null ? [] : workspace.rows,
}));

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

describe("path references", () => {
  function type(value: string) {
    fireEvent.change(screen.getByRole("textbox"), { target: { value } });
  }

  function withWorkspace() {
    return render(
      <Composer
        busy={false}
        draftKey={draftKeyFor("s")}
        conversationKey="s"
        sessionId="live-1"
        onSend={noop}
        onSteer={noop}
        onCancel={noop}
      />,
    );
  }

  beforeEach(() => {
    workspace.rows = [
      { path: "src/", isDir: true },
      { path: "safety.md", isDir: false },
    ];
  });

  it("offers workspace files once an @ is typed", () => {
    withWorkspace();
    type("look at @s");
    expect(screen.getByRole("listbox")).toBeTruthy();
    expect(screen.getByText("src/")).toBeTruthy();
  });

  it("stays out of an email address", () => {
    withWorkspace();
    // No whitespace before the @, so the menu never opens and the rows the
    // daemon would have returned are never asked for.
    type("mail bob@ex");
    expect(screen.queryByRole("listbox")).toBeNull();
  });

  it("does not open a menu on a draft restored from another conversation", () => {
    sessionStorage.setItem(draftKeyFor("other"), "look at @s");
    const { rerender } = withWorkspace();
    // A caret left over from typing here would sit at an offset into text the
    // user has not looked at, and pop a menu on arrival.
    type("look at @s");
    expect(screen.getByRole("listbox")).toBeTruthy();

    rerender(
      <Composer
        busy={false}
        draftKey={draftKeyFor("other")}
        conversationKey="other"
        sessionId="live-1"
        onSend={noop}
        onSteer={noop}
        onCancel={noop}
      />,
    );
    expect(screen.getByRole("textbox")).toHaveProperty("value", "look at @s");
    expect(screen.queryByRole("listbox")).toBeNull();
  });

  it("offers nothing before a session exists to scope the workspace", () => {
    setup({ draftKey: draftKeyFor("s"), conversationKey: "s" });
    type("look at @s");
    expect(screen.queryByRole("listbox")).toBeNull();
  });

  it("writes the chosen path into the sentence, keeping what follows", () => {
    withWorkspace();
    type("look at @s");
    // Rows answer to pointerdown, so choosing one never blurs the box first.
    fireEvent.pointerDown(screen.getByText("safety.md"));
    expect(screen.getByRole("textbox")).toHaveProperty("value", "look at @safety.md ");
  });

  it("takes a written path away whole, not one directory at a time", () => {
    withWorkspace();
    const box = screen.getByRole("textbox") as HTMLTextAreaElement;
    type("read @src/main.rs");
    box.setSelectionRange(box.value.length, box.value.length);

    fireEvent.keyDown(box, { key: "Backspace" });
    expect(box).toHaveProperty("value", "read ");
  });

  it("leaves Backspace alone in ordinary prose", () => {
    withWorkspace();
    const box = screen.getByRole("textbox") as HTMLTextAreaElement;
    type("hello world");
    box.setSelectionRange(box.value.length, box.value.length);

    fireEvent.keyDown(box, { key: "Backspace" });
    // Not prevented, so the browser deletes the one character it always would.
    expect(box).toHaveProperty("value", "hello world");
  });

  it("keeps Escape on the path menu rather than cancelling the turn", () => {
    const onCancel = vi.fn();
    render(
      <Composer
        busy
        draftKey={draftKeyFor("s")}
        conversationKey="s"
        sessionId="live-1"
        onSend={noop}
        onSteer={noop}
        onCancel={onCancel}
      />,
    );
    type("look at @s");
    act(() => dispatchKeybind("cancel-turn"));

    expect(onCancel).not.toHaveBeenCalled();
    expect(screen.queryByRole("listbox")).toBeNull();
  });
});

describe("attaching a document", () => {
  const MIXED = [
    new File(["# hi"], "notes.md", { type: "text/markdown" }),
    new File([new Uint8Array([1])], "shot.png", { type: "image/png" }),
  ];

  function pick(container: HTMLElement, files: File[]) {
    const input = container.querySelector('input[type="file"]')!;
    fireEvent.change(input, { target: { files } });
  }

  it("shows a text file by name, since there is nothing to look at", async () => {
    const { container } = setup({ draftKey: draftKeyFor("s"), conversationKey: "s" });
    pick(container, [MIXED[0]!]);

    await waitFor(() => expect(screen.getByText("notes.md")).toBeTruthy());
    // A thumbnail would be an empty box: no image was attached.
    expect(screen.queryByAltText("notes.md")).toBeNull();
  });

  // Which wire field each one lands in is `splitAttachments`' claim, tested
  // against the real function in attachments.test.ts. This is only that the
  // composer carries both kinds through to the send handler and drops neither.
  it("hands a document and an image together to the send handler", async () => {
    const onSend = vi.fn();
    const { container } = render(
      <Composer
        busy={false}
        draftKey={draftKeyFor("s")}
        conversationKey="s"
        onSend={onSend}
        onSteer={noop}
        onCancel={noop}
      />,
    );
    pick(container, MIXED);
    await waitFor(() => expect(screen.getByText("notes.md")).toBeTruthy());

    fireEvent.change(screen.getByRole("textbox"), { target: { value: "look at these" } });
    fireEvent.keyDown(screen.getByRole("textbox"), { key: "Enter" });

    const [text, attached] = onSend.mock.calls[0]!;
    expect(text).toBe("look at these");
    expect(attached.map((a: { filename: string }) => a.filename)).toEqual(["notes.md", "shot.png"]);
  });

  it("drops a document chip without touching the image beside it", async () => {
    const { container } = setup({ draftKey: draftKeyFor("s"), conversationKey: "s" });
    pick(container, MIXED);
    await waitFor(() => expect(screen.getByText("notes.md")).toBeTruthy());

    fireEvent.click(screen.getByLabelText("Remove notes.md"));

    expect(screen.queryByText("notes.md")).toBeNull();
    expect(screen.getByAltText("shot.png")).toBeTruthy();
  });
});

describe("typing while Borg waits on a prompt", () => {
  /** The composer as the page renders it while an Allow/Deny sits above. */
  function awaiting() {
    const props = {
      busy: true,
      awaitingInput: true,
      draftKey: draftKeyFor("sess-1"),
      conversationKey: "sess-1",
      onSend: vi.fn<(text: string, attachments: unknown[]) => void>(),
      onSteer: vi.fn<(text: string) => void>(),
      onCancel: vi.fn<() => void>(),
    };
    return { props, ...render(<Composer {...props} />) };
  }

  /** Types `text` and presses Enter. */
  function submit(text: string) {
    const box = screen.getByRole("textbox");
    fireEvent.change(box, { target: { value: text } });
    fireEvent.keyDown(box, { key: "Enter" });
  }

  it("holds the text instead of steering it into a channel nobody drains", () => {
    const { props } = awaiting();

    submit("also check the logs");

    expect(props.onSteer).not.toHaveBeenCalled();
    expect(props.onSend).not.toHaveBeenCalled();
    expect(screen.getByRole("textbox")).toHaveProperty("value", "");
    expect(screen.getByText(/Sends next/)).toBeTruthy();
  });

  it("sends the held text once the turn ends", () => {
    const { props, rerender } = awaiting();
    submit("also check the logs");

    rerender(<Composer {...props} busy={false} awaitingInput={false} />);

    expect(props.onSend).toHaveBeenCalledTimes(1);
    expect(props.onSend.mock.calls[0][0]).toBe("also check the logs");
    expect(screen.queryByText(/Sends next/)).toBeNull();
  });

  it("merges a second submission into the one entry", () => {
    const { props, rerender } = awaiting();
    submit("also check the logs");
    submit("and the config");

    rerender(<Composer {...props} busy={false} awaitingInput={false} />);

    expect(props.onSend).toHaveBeenCalledTimes(1);
    expect(props.onSend.mock.calls[0][0]).toBe("also check the logs\nand the config");
  });

  it("gives the text back rather than binning it", () => {
    const { props, rerender } = awaiting();
    submit("also check the logs");

    fireEvent.click(screen.getByLabelText("Return to composer"));

    expect(screen.getByRole("textbox")).toHaveProperty("value", "also check the logs");
    rerender(<Composer {...props} busy={false} awaitingInput={false} />);
    expect(props.onSend).not.toHaveBeenCalled();
  });

  it("returns the held text when the turn is stopped, and never sends it", () => {
    const { props, rerender } = awaiting();
    submit("also check the logs");

    fireEvent.click(screen.getByText("Stop"));

    expect(props.onCancel).toHaveBeenCalledTimes(1);
    expect(screen.getByRole("textbox")).toHaveProperty("value", "also check the logs");
    rerender(<Composer {...props} busy={false} awaitingInput={false} />);
    expect(props.onSend).not.toHaveBeenCalled();
  });

  it("returns the held text when Escape cancels the turn", () => {
    const { props } = awaiting();
    submit("also check the logs");

    act(() => dispatchKeybind("cancel-turn"));

    expect(props.onCancel).toHaveBeenCalledTimes(1);
    expect(screen.getByRole("textbox")).toHaveProperty("value", "also check the logs");
  });

  it("keeps the held text recoverable as this conversation's draft", () => {
    awaiting();

    submit("also check the logs");

    expect(readDraft(draftKeyFor("sess-1"))).toBe("also check the logs");
  });

  it("names what Enter will do before it is pressed", () => {
    awaiting();

    expect(screen.getByRole("textbox")).toHaveProperty("placeholder", "Send after this…");
  });
});
