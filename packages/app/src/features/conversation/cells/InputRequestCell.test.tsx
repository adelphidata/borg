import { describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { InputRequestCell } from "./InputRequestCell";
import { registerComposer } from "../composerFocus";
import type { Cell } from "../useConversation";

const question = (over: Record<string, unknown> = {}) => ({
  id: "q1",
  header: "Database",
  question: "Which database?",
  multiSelect: false,
  options: [
    { label: "sqlite", description: "" },
    { label: "postgres", description: "" },
  ],
  ...over,
});

function renderCell(over: Record<string, unknown> = {}) {
  const respond = vi.fn();
  const cell = {
    id: "c1",
    kind: "input-request",
    promptId: "p1",
    answered: false,
    questions: [question()],
    ...over,
  } as Cell & { kind: "input-request" };
  render(<InputRequestCell cell={cell} respond={respond} />);
  return { respond };
}

const answered = (respond: ReturnType<typeof vi.fn>) =>
  JSON.parse(respond.mock.calls.at(-1)?.[1] as string);

describe("InputRequestCell", () => {
  it("sends the chosen option under its question id", () => {
    const { respond } = renderCell();
    fireEvent.click(screen.getByText("postgres"));
    fireEvent.click(screen.getByText("Send answer"));

    expect(respond).toHaveBeenCalledWith("p1", expect.any(String));
    expect(answered(respond)).toEqual({ q1: { answers: ["postgres"] } });
  });

  it("replaces the previous choice for a single-select question", () => {
    const { respond } = renderCell();
    fireEvent.click(screen.getByText("sqlite"));
    fireEvent.click(screen.getByText("postgres"));
    fireEvent.click(screen.getByText("Send answer"));

    expect(answered(respond)).toEqual({ q1: { answers: ["postgres"] } });
  });

  it("accumulates choices for a multi-select question", () => {
    const { respond } = renderCell({ questions: [question({ multiSelect: true })] });
    fireEvent.click(screen.getByText("sqlite"));
    fireEvent.click(screen.getByText("postgres"));
    fireEvent.click(screen.getByText("Send answer"));

    expect(answered(respond)).toEqual({ q1: { answers: ["sqlite", "postgres"] } });
  });

  it("lets a multi-select choice be taken back", () => {
    const { respond } = renderCell({ questions: [question({ multiSelect: true })] });
    fireEvent.click(screen.getByText("sqlite"));
    fireEvent.click(screen.getByText("postgres"));
    fireEvent.click(screen.getByText("sqlite"));
    fireEvent.click(screen.getByText("Send answer"));

    expect(answered(respond)).toEqual({ q1: { answers: ["postgres"] } });
  });

  it("always offers a way out of the options the model supplied", () => {
    // The proto says appending this row is the client's job; without it a user
    // faced with three wrong options cannot answer at all.
    const { respond } = renderCell();
    fireEvent.click(screen.getByText("Other"));
    fireEvent.change(screen.getByPlaceholderText("Your answer"), {
      target: { value: "duckdb" },
    });
    fireEvent.click(screen.getByText("Send answer"));

    // The sentinel is a UI affordance; the agent gets the typed text.
    expect(answered(respond)).toEqual({ q1: { answers: ["duckdb"] } });
  });

  it("sends the typed answer on Enter", () => {
    const { respond } = renderCell();
    fireEvent.click(screen.getByText("Other"));
    fireEvent.change(screen.getByPlaceholderText("Your answer"), { target: { value: "duckdb" } });
    fireEvent.keyDown(screen.getByPlaceholderText("Your answer"), { key: "Enter" });

    expect(answered(respond)).toEqual({ q1: { answers: ["duckdb"] } });
  });

  it("sends the chosen options on ⌘↩ or Ctrl+↩, like the composer", () => {
    for (const chord of [{ metaKey: true }, { ctrlKey: true }]) {
      const { respond } = renderCell();
      fireEvent.click(screen.getByText("postgres"));
      fireEvent.keyDown(screen.getByText("postgres"), { key: "Enter", ...chord });

      expect(answered(respond)).toEqual({ q1: { answers: ["postgres"] } });
      cleanup();
    }
  });

  it("does not send on plain Enter from a pill, or the chord once answered", () => {
    const { respond } = renderCell();
    fireEvent.click(screen.getByText("postgres"));
    fireEvent.keyDown(screen.getByText("postgres"), { key: "Enter" });
    expect(respond).not.toHaveBeenCalled();
    cleanup();

    const done = renderCell({ answered: true });
    fireEvent.keyDown(screen.getByText("postgres"), { key: "Enter", metaKey: true });
    expect(done.respond).not.toHaveBeenCalled();
  });

  it("sends the typed answer once when the chord lands in the free-text box", () => {
    const { respond } = renderCell();
    fireEvent.click(screen.getByText("Other"));
    fireEvent.change(screen.getByPlaceholderText("Your answer"), { target: { value: "duckdb" } });
    fireEvent.keyDown(screen.getByPlaceholderText("Your answer"), { key: "Enter", metaKey: true });

    expect(respond).toHaveBeenCalledTimes(1);
    expect(answered(respond)).toEqual({ q1: { answers: ["duckdb"] } });
  });

  it("drops an Other row left empty rather than sending a blank answer", () => {
    const { respond } = renderCell();
    fireEvent.click(screen.getByText("Other"));
    fireEvent.click(screen.getByText("Send answer"));

    expect(answered(respond)).toEqual({ q1: { answers: [] } });
  });

  it("only shows the free-text box for the question that asked for it", () => {
    renderCell({
      questions: [question(), question({ id: "q2", question: "Which cache?" })],
    });
    expect(screen.queryByPlaceholderText("Your answer")).toBeNull();

    fireEvent.click(screen.getAllByText("Other")[0]);
    expect(screen.getAllByPlaceholderText("Your answer")).toHaveLength(1);
  });

  it("answers every question in the batch, including the ones left blank", () => {
    const { respond } = renderCell({
      questions: [question(), question({ id: "q2", question: "Which cache?" })],
    });
    // Both questions offer the same options, so pick the first question's.
    fireEvent.click(screen.getAllByText("sqlite")[0]);
    fireEvent.click(screen.getByText("Send answer"));

    expect(answered(respond)).toEqual({
      q1: { answers: ["sqlite"] },
      q2: { answers: [] },
    });
  });

  it("offers no send control once the question has been answered", () => {
    renderCell({ answered: true });
    expect(screen.queryByText("Send answer")).toBeNull();
  });
});

describe("InputRequestCell — input methods and answered prompts", () => {
  it("does not send on an input method's Enter in the free-text box", () => {
    const { respond } = renderCell();
    fireEvent.click(screen.getByText("Other"));
    const box = screen.getByPlaceholderText("Your answer");
    fireEvent.change(box, { target: { value: "duckdb" } });
    fireEvent.keyDown(box, { key: "Enter", isComposing: true });
    fireEvent.keyDown(box, { key: "Enter", keyCode: 229 });
    expect(respond).not.toHaveBeenCalled();
  });

  it("does not send on an input method's Enter with the chord held", () => {
    const { respond } = renderCell();
    fireEvent.click(screen.getByText("postgres"));
    fireEvent.keyDown(screen.getByText("postgres"), { key: "Enter", metaKey: true, keyCode: 229 });
    expect(respond).not.toHaveBeenCalled();
  });

  it("ignores the pills and Enter once the prompt is answered", () => {
    const respond = vi.fn();
    const cell = {
      kind: "input-request",
      promptId: "p1",
      answered: false,
      questions: [question()],
    } as Cell & { kind: "input-request" };
    const view = render(<InputRequestCell cell={cell} respond={respond} />);
    fireEvent.click(screen.getByText("Other"));
    const box = screen.getByPlaceholderText("Your answer");
    fireEvent.change(box, { target: { value: "duckdb" } });

    view.rerender(<InputRequestCell cell={{ ...cell, answered: true }} respond={respond} />);
    fireEvent.click(screen.getByText("postgres"));
    fireEvent.keyDown(box, { key: "Enter" });
    fireEvent.keyDown(box, { key: "Enter", metaKey: true });
    expect(respond).not.toHaveBeenCalled();
    expect((screen.getByText("postgres") as HTMLButtonElement).disabled).toBe(true);
  });

  it("hands focus back to the composer once the answer is sent", () => {
    const composer = document.createElement("textarea");
    document.body.appendChild(composer);
    const unregister = registerComposer(() => composer);
    try {
      renderCell();
      fireEvent.click(screen.getByText("postgres"));
      fireEvent.click(screen.getByText("Send answer"));
      expect(document.activeElement).toBe(composer);
    } finally {
      unregister();
      composer.remove();
    }
  });
});
