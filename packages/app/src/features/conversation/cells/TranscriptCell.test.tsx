import { describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { TranscriptCell } from "./index";
import { registerComposer } from "../composerFocus";
import type { Cell } from "../useConversation";
import { formatClock } from "../../../lib/format";

function renderCell(cell: Cell, over: { ordinal?: number; editable?: boolean } = {}) {
  const respond = vi.fn();
  const onEdit = vi.fn();
  const view = render(
    // FailureCard navigates to Settings for a credential failure, so the tree
    // needs a router.
    <MemoryRouter>
      <TranscriptCell
        cell={cell}
        respond={respond}
        ordinal={over.ordinal ?? -1}
        editable={over.editable ?? true}
        onEdit={onEdit}
      />
    </MemoryRouter>,
  );
  return { ...view, respond, onEdit };
}

describe("TranscriptCell — user turns", () => {
  it("shows the message the user sent", () => {
    renderCell({ kind: "user", text: "ship it" });
    expect(screen.getByText("ship it")).toBeTruthy();
  });

  it("counts images and names documents, none of whose bytes it still holds", () => {
    renderCell({
      kind: "user",
      text: "look at this",
      attachments: [
        { kind: "image", filename: "IMG_1.png" },
        { kind: "document", filename: "notes.md" },
        { kind: "image", filename: "IMG_2.png" },
      ],
    });
    expect(screen.getByText("2 images attached")).toBeTruthy();
    expect(screen.getByText("notes.md")).toBeTruthy();
    expect(screen.queryByText(/IMG_/)).toBeNull();
  });

  it("calls a long paste what the user did, not what the file was named", () => {
    renderCell({
      kind: "user",
      text: "",
      attachments: [
        { kind: "document", filename: "pasted-text.txt" },
        { kind: "document", filename: "pasted-text-2.txt" },
      ],
    });
    expect(screen.getAllByText("Pasted text")).toHaveLength(2);
  });

  it("singular for one image", () => {
    renderCell({ kind: "user", text: "", attachments: [{ kind: "image", filename: "a.png" }] });
    expect(screen.getByText("1 image attached")).toBeTruthy();
  });

  it("renders no empty text block for a turn that was only its attachments", () => {
    const { container } = renderCell({
      kind: "user",
      text: "",
      attachments: [{ kind: "document", filename: "notes.md" }],
    });
    const bubble = container.querySelector(".whitespace-pre-wrap")!;
    // Only the attachment line, so the bubble has no blank first line.
    expect(bubble.children).toHaveLength(1);
    expect(bubble.firstElementChild!.textContent).toBe("notes.md");
  });

  it("ignores an input method's Enter in the edit box, which would rewind mid-word", () => {
    const { onEdit } = renderCell({ kind: "user", text: "ship it" }, { ordinal: 0 });
    fireEvent.click(screen.getByRole("button", { name: "Edit and resend" }));
    const box = screen.getByDisplayValue("ship it");
    fireEvent.keyDown(box, { key: "Enter", isComposing: true });
    fireEvent.keyDown(box, { key: "Enter", keyCode: 229 });
    expect(onEdit).not.toHaveBeenCalled();
    expect(screen.getByDisplayValue("ship it")).toBeTruthy();
  });

  it("opens the edit box at the height of the text, not of its line breaks", () => {
    Object.defineProperty(HTMLTextAreaElement.prototype, "scrollHeight", {
      configurable: true,
      get: () => 96,
    });
    try {
      renderCell({ kind: "user", text: "one long paragraph".repeat(20) }, { ordinal: 0 });
      fireEvent.click(screen.getByRole("button", { name: "Edit and resend" }));
      const box = screen.getByRole("textbox") as HTMLTextAreaElement;
      expect(box.style.height).toBe("96px");
    } finally {
      delete (HTMLTextAreaElement.prototype as { scrollHeight?: unknown }).scrollHeight;
    }
  });

  it("rewrites a turn and re-runs from there", () => {
    const { onEdit } = renderCell({ kind: "user", text: "ship it" }, { ordinal: 3 });
    fireEvent.click(screen.getByRole("button", { name: "Edit and resend" }));

    const box = screen.getByDisplayValue("ship it");
    fireEvent.change(box, { target: { value: "  ship it properly  " } });
    fireEvent.click(screen.getByText("Resend"));
    // The daemon rewinds by ordinal among user cells.
    expect(onEdit).toHaveBeenCalledWith(3, "ship it properly");
  });

  it("says up front that editing discards what came after", () => {
    // The daemon cannot splice a turn into the middle of a history.
    renderCell({ kind: "user", text: "ship it" }, { ordinal: 0 });
    fireEvent.click(screen.getByRole("button", { name: "Edit and resend" }));
    expect(screen.getByText("Replies after this are discarded")).toBeTruthy();
  });

  it("resends on Enter and abandons on Escape", () => {
    const { onEdit } = renderCell({ kind: "user", text: "ship it" }, { ordinal: 0 });
    fireEvent.click(screen.getByRole("button", { name: "Edit and resend" }));
    fireEvent.keyDown(screen.getByDisplayValue("ship it"), { key: "Escape" });
    expect(screen.queryByDisplayValue("ship it")).toBeNull();
    expect(onEdit).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole("button", { name: "Edit and resend" }));
    fireEvent.keyDown(screen.getByDisplayValue("ship it"), { key: "Enter" });
    expect(onEdit).toHaveBeenCalledWith(0, "ship it");
  });

  it("refuses to resend an empty message", () => {
    const { onEdit } = renderCell({ kind: "user", text: "ship it" }, { ordinal: 0 });
    fireEvent.click(screen.getByRole("button", { name: "Edit and resend" }));
    fireEvent.change(screen.getByDisplayValue("ship it"), { target: { value: "   " } });
    fireEvent.click(screen.getByText("Resend"));
    expect(onEdit).not.toHaveBeenCalled();
  });

  it("cancels an edit without sending", () => {
    const { onEdit } = renderCell({ kind: "user", text: "ship it" }, { ordinal: 0 });
    fireEvent.click(screen.getByRole("button", { name: "Edit and resend" }));
    fireEvent.click(screen.getByText("Cancel"));
    expect(onEdit).not.toHaveBeenCalled();
    expect(screen.getByText("ship it")).toBeTruthy();
  });

  it("offers no edit mid-turn, where a rewrite would race the running turn", () => {
    renderCell({ kind: "user", text: "ship it" }, { ordinal: 0, editable: false });
    expect(screen.queryByRole("button", { name: "Edit and resend" })).toBeNull();
  });
});

describe("TranscriptCell — other kinds", () => {
  it("renders an assistant reply as prose", () => {
    renderCell({ kind: "assistant", text: "**done**" });
    expect(screen.getByText("done").tagName).toBe("STRONG");
  });

  it("asks before running a command, and answers either way", () => {
    const { respond } = renderCell({
      kind: "shell-confirm",
      promptId: "p1",
      command: "rm -rf build",
      answered: false,
    });
    expect(screen.getByText("rm -rf build")).toBeTruthy();

    fireEvent.click(screen.getByText("Allow"));
    expect(respond).toHaveBeenCalledWith("p1", "true");

    fireEvent.click(screen.getByText("Deny"));
    expect(respond).toHaveBeenCalledWith("p1", "false");
  });

  it("hands focus back to the composer once a command is allowed or denied", () => {
    for (const choice of ["Allow", "Deny"]) {
      const composer = document.createElement("textarea");
      document.body.appendChild(composer);
      const unregister = registerComposer(() => composer);
      try {
        renderCell({ kind: "shell-confirm", promptId: "p1", command: "ls", answered: false });
        fireEvent.click(screen.getByText(choice));
        expect(document.activeElement).toBe(composer);
      } finally {
        unregister();
        composer.remove();
        cleanup();
      }
    }
  });

  it("drops the answer controls once the prompt is answered", () => {
    renderCell({
      kind: "shell-confirm",
      promptId: "p1",
      command: "ls",
      answered: true,
    });
    expect(screen.queryByText("Allow")).toBeNull();
  });

  it("leads a turn footer with the time it took", () => {
    // "How long did that take" is the question people ask unprompted.
    const { container } = renderCell({
      kind: "usage",
      input: 1200,
      output: 340,
      cacheRead: 0,
      cacheWrite: 0,
      elapsedMs: 12_400,
    });
    expect(container.textContent).toContain("12.4s");
    expect(container.textContent).toContain("1.2K in");
    expect(container.textContent).toContain("340 out");
  });

  it("reports cached tokens only when there were any", () => {
    const { container } = renderCell({
      kind: "usage",
      input: 10,
      output: 5,
      cacheRead: 900,
      cacheWrite: 100,
      elapsedMs: 1000,
    });
    expect(container.textContent).toContain("1K cached");
  });

  it("omits token counts for a turn that reported none", () => {
    // Otherwise the footer claims the turn cost zero.
    const { container } = renderCell({
      kind: "usage",
      input: 0,
      output: 0,
      cacheRead: 0,
      cacheWrite: 0,
      elapsedMs: 800,
    });
    expect(container.textContent).toBe("0.8s");
  });

  it("renders nothing at all when a footer has nothing to say", () => {
    const { container } = renderCell({
      kind: "usage",
      input: 0,
      output: 0,
      cacheRead: 0,
      cacheWrite: 0,
    });
    expect(container.firstChild).toBeNull();
  });

  it("reports a turn error as a card, with no retry of its own", () => {
    // The daemon has already written the failed turn into history.
    renderCell({ kind: "error", text: "provider refused the request" });
    expect(screen.getByText(/provider refused the request/)).toBeTruthy();
    expect(screen.queryByText("Retry")).toBeNull();
  });

  it("renders a system notice", () => {
    renderCell({ kind: "notice", text: "Older messages were compacted to save space." });
    expect(screen.getByText("Older messages were compacted to save space.")).toBeTruthy();
  });

  it("shows a sub-agent's nickname and status", () => {
    renderCell({ kind: "sub-agent", agentId: "a1", nickname: "scout", status: "running" });
    expect(screen.getByText("scout")).toBeTruthy();
    expect(screen.getByText("running")).toBeTruthy();
  });

  it("falls back to the agent id when it has no nickname", () => {
    renderCell({ kind: "sub-agent", agentId: "a1", nickname: "", status: "done" });
    expect(screen.getByText("a1")).toBeTruthy();
  });
});

describe("TranscriptCell — timestamps", () => {
  it("shows the local send time under every message, the same for both sides", () => {
    const at = Date.UTC(2026, 7, 19, 14, 32, 7);
    const clock = formatClock(at);
    for (const cell of [
      { kind: "user", text: "hello", at },
      { kind: "assistant", text: "hi", at },
    ] satisfies Cell[]) {
      const { container, unmount } = renderCell(cell);
      const time = container.querySelector("time");
      expect(time?.textContent).toBe(clock);
      expect(time?.getAttribute("dateTime")).toBe(new Date(at).toISOString());
      // Below the message, not inside its header: the last thing in the row.
      expect(time?.nextElementSibling).toBeNull();
      unmount();
    }
  });

  it("offers the full wall-clock time of a row that has one, as a tip", () => {
    const at = Date.UTC(2026, 7, 19, 14, 32, 7);
    const { container } = renderCell({ kind: "assistant", text: "hi", at });
    const wrapper = container.firstElementChild as HTMLElement;
    expect(wrapper.title).toBe(
      new Date(at).toLocaleString(undefined, {
        day: "numeric",
        month: "short",
        year: "numeric",
        hour: "2-digit",
        minute: "2-digit",
        second: "2-digit",
      }),
    );
  });

  it("adds no wrapper for a row with no time of its own", () => {
    const { container } = renderCell({
      kind: "usage",
      input: 1,
      output: 1,
      cacheRead: 0,
      cacheWrite: 0,
    });
    expect((container.firstElementChild as HTMLElement).title).toBe("");
    expect(container.querySelector("time")).toBeNull();
  });
});

describe("TranscriptCell — tool rows", () => {
  const tool = (output: string, over: Partial<Cell & { kind: "tool" }> = {}): Cell => ({
    kind: "tool",
    id: "t1",
    name: "run_shell",
    args: '{"command":"make"}',
    output,
    done: true,
    stderr: false,
    ...over,
  });
  const dot = (container: HTMLElement) => container.querySelector("button > span")!;
  // The last <pre> is the output; an opened row shows the arguments above it.
  const output = (container: HTMLElement) => Array.from(container.querySelectorAll("pre")).at(-1)!;

  it("marks a command that exited non-zero as a failure, in red", () => {
    const { container } = renderCell(tool("Exit code: 2\n--- stdout ---\n--- stderr ---\nno rule"));
    fireEvent.click(screen.getByRole("button"));
    expect(dot(container).className).toContain("bg-alert");
    expect(output(container).className).toContain("text-alert");
  });

  it("keeps a clean exit green even when it wrote to stderr", () => {
    // Progress on stderr is what most build tools do; red here teaches the
    // user to ignore red.
    const { container } = renderCell(
      tool("Exit code: 0\n--- stdout ---\n--- stderr ---\nwarning: unused", { stderr: true }),
    );
    fireEvent.click(screen.getByRole("button"));
    expect(dot(container).className).toContain("bg-neon");
    expect(output(container).className).not.toContain("text-alert");
  });

  it("reads the same failure off a reloaded row, which carries no stderr flag", () => {
    const { container } = renderCell(tool("Shell command denied: matches a blocked pattern"));
    expect(dot(container).className).toContain("bg-alert");
  });

  it("shows a cancelled call as Stopped, neither green nor red", () => {
    const { container } = renderCell(tool("[tool call cancelled by user]"));
    fireEvent.click(screen.getByRole("button"));
    expect(screen.getByText("Stopped")).toBeTruthy();
    expect(screen.queryByText("[tool call cancelled by user]")).toBeNull();
    expect(dot(container).className).not.toContain("bg-alert");
    expect(dot(container).className).not.toContain("bg-neon");
  });
});
