import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { dispatchKeybind, resetKeybinds } from "../../lib/keybinds";
import type { Command } from "../palette/score";
import { Composer } from "./Composer";
import { collectAttachments } from "./attachments";
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
 * The microphone talks to the daemon too. Its stand-in hands the composer
 * words or an error on click, which is all the composer sees of it.
 */
const spoken = vi.hoisted(() => ({
  text: "hello there",
  error: undefined as string | undefined,
  send: false,
  mounts: 0,
}));
vi.mock("./VoiceInputButton", async () => {
  const { useEffect } = await import("react");
  return {
    VoiceInputButton: ({
      onText,
      onError,
    }: {
      onText: (t: string, options: { send: boolean }) => void;
      onError: (m: string | undefined) => void;
    }) => {
      useEffect(() => {
        spoken.mounts += 1;
      }, []);
      return (
        <button
          type="button"
          aria-label="Speak instead of typing"
          onClick={() =>
            spoken.error ? onError(spoken.error) : onText(spoken.text, { send: spoken.send })
          }
        />
      );
    },
  };
});

/**
 * Path completions come from the daemon, scoped and filtered there. This file
 * is about what the composer does with the rows it is handed.
 */
const workspace = vi.hoisted(() => ({ rows: [] as { path: string; isDir: boolean }[] }));
vi.mock("./usePathCompletions", () => ({
  usePathCompletions: (sessionId: string | undefined, prefix: string | null) =>
    sessionId === undefined || prefix === null ? [] : workspace.rows,
}));

/** The width the page is rendered at; a phone must not be handed the keyboard. */
const device = vi.hoisted(() => ({ mobile: false }));
vi.mock("@borg/ui", async (importOriginal) => ({
  ...(await importOriginal<typeof import("@borg/ui")>()),
  useIsMobile: () => device.mobile,
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
  device.mobile = false;
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

  it("hands a command with words to Borg, even mid-turn, instead of steering it", () => {
    const onSteer = vi.fn();
    const onSend = vi.fn();
    const onCommand = vi.fn((text: string) => text.startsWith("/goal "));
    const props = {
      draftKey: draftKeyFor("s"),
      conversationKey: "s",
      onSend,
      onSteer,
      onCancel: noop,
      onCommand,
    };
    render(<Composer busy {...props} />);

    type("/goal Ship the notes");
    enter();
    expect(onCommand).toHaveBeenCalledWith("/goal Ship the notes");
    expect(onSteer).not.toHaveBeenCalled();
    expect(screen.getByRole("textbox")).toHaveProperty("value", "");

    // Anything the command hook declines is still ordinary text.
    type("keep going");
    enter();
    expect(onSteer).toHaveBeenCalledWith("keep going");
  });

  it("runs a bare /goal rather than the menu row it happens to match", () => {
    const toggle = vi.fn();
    registry.commands = [command("setting:goals.enabled", "Enable /goal", toggle)];
    const onCommand = vi.fn((text: string) => text === "/goal");
    render(
      <Composer
        busy={false}
        draftKey={draftKeyFor("s")}
        conversationKey="s"
        onSend={noop}
        onSteer={noop}
        onCancel={noop}
        onCommand={onCommand}
      />,
    );

    type("/goal");
    expect(screen.getByRole("listbox")).toBeTruthy();
    enter();
    expect(onCommand).toHaveBeenCalledWith("/goal");
    expect(toggle).not.toHaveBeenCalled();
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

    fireEvent.click(screen.getByRole("button", { name: "Stop" }));

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

describe("Composer pasted text", () => {
  const clipboard = (text: string) => ({
    clipboardData: { files: [], getData: () => text, types: ["text/plain"] },
  });
  const long = "trace line\n".repeat(400);

  it("attaches a long paste as a file and keeps the box clear for the question", async () => {
    setup({ draftKey: draftKeyFor("sess-1"), conversationKey: "sess-1" });
    fireEvent.paste(screen.getByRole("textbox"), clipboard(long));

    await waitFor(() => expect(screen.getByText("pasted-text.txt")).toBeTruthy());
    expect(screen.getByRole("textbox")).toHaveProperty("value", "");
    expect(vi.mocked(collectAttachments).mock.calls.at(-1)?.[0][0]?.name).toBe("pasted-text.txt");
  });

  it("leaves a short paste in the browser's hands, at the caret", () => {
    setup({ draftKey: draftKeyFor("sess-1"), conversationKey: "sess-1" });
    const before = vi.mocked(collectAttachments).mock.calls.length;
    const event = fireEvent.paste(screen.getByRole("textbox"), clipboard("just a line"));

    // Not cancelled: the textarea inserts it natively.
    expect(event).toBe(true);
    expect(vi.mocked(collectAttachments).mock.calls.length).toBe(before);
  });

  it("numbers a second long paste rather than showing two identical chips", async () => {
    setup({ draftKey: draftKeyFor("sess-1"), conversationKey: "sess-1" });
    fireEvent.paste(document.body, clipboard(long));
    await waitFor(() => expect(screen.getByText("pasted-text.txt")).toBeTruthy());
    fireEvent.paste(document.body, clipboard(long));

    await waitFor(() => expect(screen.getByText("pasted-text-2.txt")).toBeTruthy());
  });

  it("falls back to the box when the paste cannot become a file", async () => {
    vi.mocked(collectAttachments).mockResolvedValueOnce({
      attachments: [],
      errors: ["pasted-text.txt is too long to attach to one message"],
    });
    setup({ draftKey: draftKeyFor("sess-1"), conversationKey: "sess-1" });
    fireEvent.paste(document.body, clipboard(long));

    // Cluttering the box is a nuisance; losing the paste would not be.
    await waitFor(() => expect(screen.getByRole("textbox")).toHaveProperty("value", long));
    expect(screen.queryByText(/too long to attach/)).toBeNull();
  });

  it("does not drop a failed paste into the conversation the user moved to", async () => {
    let refuse: (value: { attachments: never[]; errors: string[] }) => void = () => {};
    vi.mocked(collectAttachments).mockImplementationOnce(
      () => new Promise((resolve) => (refuse = resolve)),
    );
    const { rerender } = setup({ draftKey: draftKeyFor("sess-1"), conversationKey: "sess-1" });
    fireEvent.paste(document.body, clipboard(long));

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
    await act(async () => refuse({ attachments: [], errors: ["pasted-text.txt is too long"] }));

    expect(screen.getByRole("textbox")).toHaveProperty("value", "");
    expect(readDraft(draftKeyFor("sess-2"))).toBe("");
  });
});

describe("sending before the conversation is open", () => {
  function opening() {
    const props = {
      busy: false,
      ready: false,
      draftKey: draftKeyFor("new"),
      conversationKey: "new",
      onSend: vi.fn<(text: string, attachments: { filename: string }[]) => void>(),
      onSteer: vi.fn<(text: string) => void>(),
      onCancel: noop,
    };
    return { props, ...render(<Composer {...props} />) };
  }

  it("holds the message and its attachment, then sends both once", async () => {
    const { props, container, rerender } = opening();
    await attach(container);
    const box = screen.getByRole("textbox");
    fireEvent.change(box, { target: { value: "first question" } });
    fireEvent.keyDown(box, { key: "Enter" });

    expect(props.onSend).not.toHaveBeenCalled();
    expect(props.onSteer).not.toHaveBeenCalled();
    expect(screen.getByText(/Sends next · first question/)).toBeTruthy();

    // The daemon issues the id: the draft is re-keyed and the page is ready.
    moveDraft(draftKeyFor("new"), draftKeyFor("sess-1"));
    rerender(<Composer {...props} ready draftKey={draftKeyFor("sess-1")} />);
    rerender(<Composer {...props} ready draftKey={draftKeyFor("sess-1")} />);

    expect(props.onSend).toHaveBeenCalledTimes(1);
    const [text, attached] = props.onSend.mock.calls[0]!;
    expect(text).toBe("first question");
    expect(attached.map((a) => a.filename)).toEqual(["shot.png"]);
    expect(screen.queryByText(/Sends next/)).toBeNull();
    // Sent, so it must not also come back as a draft to send again.
    expect(screen.getByRole("textbox")).toHaveProperty("value", "");
  });

  it("keeps holding while a turn is still running", () => {
    const { props, rerender } = opening();
    const box = screen.getByRole("textbox");
    fireEvent.change(box, { target: { value: "first question" } });
    fireEvent.keyDown(box, { key: "Enter" });

    rerender(<Composer {...props} busy ready />);
    expect(props.onSend).not.toHaveBeenCalled();

    rerender(<Composer {...props} busy={false} ready />);
    expect(props.onSend).toHaveBeenCalledTimes(1);
  });
});

describe("steering a running turn", () => {
  function steering(onSteer: (text: string) => Promise<boolean> | boolean | void) {
    const props = {
      busy: true,
      draftKey: draftKeyFor("sess-1"),
      conversationKey: "sess-1",
      onSend: noop,
      onSteer: vi.fn(onSteer),
      onCancel: noop,
    };
    return { props, ...render(<Composer {...props} />) };
  }

  async function submit(text: string) {
    const box = screen.getByRole("textbox");
    fireEvent.change(box, { target: { value: text } });
    fireEvent.keyDown(box, { key: "Enter" });
    expect(box).toHaveProperty("value", "");
    // Lets the answer arrive.
    await act(async () => {});
  }

  it.each([
    { name: "a promise of false", answer: () => Promise.resolve(false) },
    { name: "a plain false", answer: () => false },
    { name: "a call that failed", answer: () => Promise.reject(new Error("offline")) },
  ])("gives the text back on $name", async ({ answer }) => {
    const { props } = steering(answer);
    await submit("use the staging db");

    expect(props.onSteer).toHaveBeenCalledWith("use the staging db");
    expect(screen.getByRole("textbox")).toHaveProperty("value", "use the staging db");
  });

  it.each([
    { name: "true", answer: () => Promise.resolve(true) },
    { name: "nothing", answer: () => undefined },
  ])("leaves the box empty when the turn answers $name", async ({ answer }) => {
    const { props } = steering(answer);
    await submit("use the staging db");

    expect(props.onSteer).toHaveBeenCalledTimes(1);
    expect(screen.getByRole("textbox")).toHaveProperty("value", "");
  });

  it("reports a steer call that failed instead of swallowing it", async () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    const failure = new Error("offline");
    steering(() => Promise.reject(failure));
    await submit("use the staging db");

    expect(warn).toHaveBeenCalledWith(expect.any(String), failure);
    expect(screen.getByRole("textbox")).toHaveProperty("value", "use the staging db");
    warn.mockRestore();
  });

  it("adds the text after what was typed in the meantime", async () => {
    let refuse: (taken: boolean) => void = () => {};
    steering(() => new Promise<boolean>((resolve) => (refuse = resolve)));
    const box = screen.getByRole("textbox");
    fireEvent.change(box, { target: { value: "use the staging db" } });
    fireEvent.keyDown(box, { key: "Enter" });
    fireEvent.change(box, { target: { value: "and then" } });

    await act(async () => refuse(false));

    expect(box).toHaveProperty("value", "and then use the staging db");
  });

  it("returns the text to its own conversation when the user has moved on", async () => {
    let refuse: (taken: boolean) => void = () => {};
    const { props, rerender } = steering(
      () => new Promise<boolean>((resolve) => (refuse = resolve)),
    );
    const box = screen.getByRole("textbox");
    fireEvent.change(box, { target: { value: "use the staging db" } });
    fireEvent.keyDown(box, { key: "Enter" });

    rerender(<Composer {...props} draftKey={draftKeyFor("sess-2")} conversationKey="sess-2" />);
    await act(async () => refuse(false));

    expect(screen.getByRole("textbox")).toHaveProperty("value", "");
    expect(sessionStorage.getItem(draftKeyFor("sess-1"))).toBe("use the staging db");
    expect(readDraft(draftKeyFor("sess-1"))).toBe("use the staging db");
  });
});

describe("dropping onto the composer", () => {
  const image = new File([new Uint8Array([1])], "shot.png", { type: "image/png" });
  const shortcut = new File(["[InternetShortcut]"], "Example.url", { type: "" });

  function transfer(data: Record<string, string>, files: File[] = []) {
    return {
      dataTransfer: {
        types: [...Object.keys(data), ...(files.length > 0 ? ["Files"] : [])],
        files,
        getData: (format: string) => data[format] ?? "",
      },
    };
  }

  function mount() {
    setup({ draftKey: draftKeyFor("s"), conversationKey: "s" });
    const box = screen.getByRole("textbox") as HTMLTextAreaElement;
    return { box, frame: box.parentElement! };
  }

  beforeEach(() => void vi.mocked(collectAttachments).mockClear());

  it("attaches a dropped image", async () => {
    const { frame } = mount();
    const delivered = fireEvent.drop(frame, transfer({}, [image]));

    // Cancelled, or the browser would navigate to the file.
    expect(delivered).toBe(false);
    await waitFor(() => expect(screen.getByAltText("shot.png")).toBeTruthy());
  });

  it("shows the drop target for files only", () => {
    const { frame } = mount();

    fireEvent.dragOver(frame, transfer({ "text/plain": "some words" }));
    expect(frame.className.split(" ")).not.toContain("border-focus");

    expect(fireEvent.dragOver(frame, transfer({}, [image]))).toBe(false);
    expect(frame.className.split(" ")).toContain("border-focus");
  });

  it("leaves text dragged onto the box for the browser to place", () => {
    const { box } = mount();
    const data = transfer({ "text/plain": "some words" });

    expect(fireEvent.dragOver(box, data)).toBe(true);
    expect(fireEvent.drop(box, data)).toBe(true);
    expect(collectAttachments).not.toHaveBeenCalled();
  });

  it("accepts text dragged onto the frame around the box", () => {
    const { frame } = mount();
    // Without this the browser never delivers the drop at all.
    expect(fireEvent.dragOver(frame, transfer({ "text/plain": "some words" }))).toBe(false);
  });

  it("writes a link dropped on the frame at the caret", () => {
    const { box, frame } = mount();
    fireEvent.change(box, { target: { value: "read  please" } });
    box.setSelectionRange(5, 5);

    const delivered = fireEvent.drop(
      frame,
      transfer({ "text/uri-list": "https://example.com/doc", "text/plain": "Example" }),
    );

    expect(delivered).toBe(false);
    expect(box).toHaveProperty("value", "read https://example.com/doc please");
  });

  it("ignores the shortcut file that comes with a dragged link", () => {
    const { box, frame } = mount();
    fireEvent.dragOver(frame, transfer({ "text/uri-list": "https://example.com/" }, [shortcut]));
    fireEvent.drop(box, transfer({ "text/uri-list": "https://example.com/" }, [shortcut]));

    expect(box).toHaveProperty("value", "https://example.com/");
    expect(collectAttachments).not.toHaveBeenCalled();
    expect(frame.className.split(" ")).not.toContain("border-focus");
  });

  it("still says why a shortcut file dropped alone cannot be attached", async () => {
    vi.mocked(collectAttachments).mockResolvedValueOnce({
      attachments: [],
      errors: ["Example.url is not an image or a text file."],
    });
    const { frame } = mount();
    fireEvent.drop(frame, transfer({}, [shortcut]));

    await waitFor(() => expect(screen.getByText(/Example.url is not an image/)).toBeTruthy());
  });
});

describe("where the cursor is when a conversation opens", () => {
  it("is in the composer", () => {
    setup({ draftKey: draftKeyFor("s"), conversationKey: "s" });
    expect(document.activeElement).toBe(screen.getByRole("textbox"));
  });

  it("returns to the composer when another conversation is opened", () => {
    const { rerender } = setup({ draftKey: draftKeyFor("s"), conversationKey: "s" });
    const box = screen.getByRole("textbox");
    box.blur();

    rerender(
      <Composer
        busy={false}
        draftKey={draftKeyFor("t")}
        conversationKey="t"
        onSend={noop}
        onSteer={noop}
        onCancel={noop}
      />,
    );
    expect(document.activeElement).toBe(box);
  });

  it("stays out of the box on a phone, so no keyboard covers the page", () => {
    device.mobile = true;
    setup({ draftKey: draftKeyFor("s"), conversationKey: "s" });
    expect(document.activeElement).not.toBe(screen.getByRole("textbox"));
  });

  it("does not take the cursor from a field the user is typing in", () => {
    const search = document.createElement("input");
    document.body.appendChild(search);
    search.focus();

    setup({ draftKey: draftKeyFor("s"), conversationKey: "s" });

    expect(document.activeElement).toBe(search);
    search.remove();
  });
});

describe("Enter while an input method is composing", () => {
  function typed(onSend: () => void) {
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
    const box = screen.getByRole("textbox");
    fireEvent.change(box, { target: { value: "こんにちは" } });
    return box;
  }

  it.each([
    { name: "confirms a candidate flagged as composing", init: { isComposing: true } },
    { name: "confirms a candidate that only carries keyCode 229", init: { keyCode: 229 } },
    { name: "breaks the line with Shift", init: { shiftKey: true } },
  ])("$name, and does not send", ({ init }) => {
    const onSend = vi.fn();
    const box = typed(onSend);

    const delivered = fireEvent.keyDown(box, { key: "Enter", ...init });

    // Not cancelled either: the keystroke is the browser's to finish.
    expect(delivered).toBe(true);
    expect(onSend).not.toHaveBeenCalled();
    expect(box).toHaveProperty("value", "こんにちは");
  });

  it("sends on a plain Enter", () => {
    const onSend = vi.fn();
    const box = typed(onSend);

    fireEvent.keyDown(box, { key: "Enter", keyCode: 13 });
    expect(onSend).toHaveBeenCalledWith("こんにちは", []);
  });
});

describe("Composer voice input", () => {
  beforeEach(() => {
    spoken.text = "hello there";
    spoken.error = undefined;
    spoken.send = false;
    spoken.mounts = 0;
  });

  it("sends the spoken words at once when Settings says to, through the same path as Enter", () => {
    spoken.send = true;
    const onSend = vi.fn();
    render(
      <Composer
        busy={false}
        onSend={onSend}
        onSteer={noop}
        onCancel={noop}
        draftKey="v0"
        conversationKey="v0"
      />,
    );
    fireEvent.click(screen.getByLabelText("Speak instead of typing"));
    expect(onSend).toHaveBeenCalledWith("hello there", []);
    expect((screen.getByRole("textbox") as HTMLTextAreaElement).value).toBe("");
  });

  it("keeps the same microphone when a new conversation is issued its id", () => {
    const props = { busy: false, onSend: noop, onSteer: noop, onCancel: noop };
    const { rerender } = render(<Composer {...props} draftKey="new" conversationKey="c1" />);
    rerender(<Composer {...props} draftKey="session-123" conversationKey="c1" />);
    expect(spoken.mounts).toBe(1);
    rerender(<Composer {...props} draftKey="session-456" conversationKey="c2" />);
    expect(spoken.mounts).toBe(2);
  });

  it("puts spoken words into an empty box without sending them", () => {
    const onSend = vi.fn();
    render(
      <Composer
        busy={false}
        onSend={onSend}
        onSteer={noop}
        onCancel={noop}
        draftKey="v1"
        conversationKey="v1"
      />,
    );
    fireEvent.click(screen.getByLabelText("Speak instead of typing"));
    expect((screen.getByRole("textbox") as HTMLTextAreaElement).value).toBe("hello there");
    expect(onSend).not.toHaveBeenCalled();
  });

  it("appends spoken words after what was already typed, one space apart", () => {
    setup({ draftKey: "v2", conversationKey: "v2" });
    const box = screen.getByRole("textbox") as HTMLTextAreaElement;
    fireEvent.change(box, { target: { value: "Remind me  " } });
    fireEvent.click(screen.getByLabelText("Speak instead of typing"));
    expect(box.value).toBe("Remind me hello there");
  });

  it("shows the microphone's failure under the box", () => {
    spoken.error = "Didn't catch that. Try again a bit closer.";
    setup({ draftKey: "v3", conversationKey: "v3" });
    fireEvent.click(screen.getByLabelText("Speak instead of typing"));
    expect(screen.getByText("Didn't catch that. Try again a bit closer.")).toBeTruthy();
  });
});
