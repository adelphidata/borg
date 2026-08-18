import { afterEach, describe, expect, it } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { ThinkingCell } from "./ThinkingCell";
import type { Cell } from "../useConversation";

afterEach(cleanup);

function thinking(over: Partial<Cell & { kind: "thinking" }> = {}) {
  return { kind: "thinking", text: "weighing the options", streaming: true, ...over } as Cell & {
    kind: "thinking";
  };
}

describe("ThinkingCell", () => {
  it("keeps a streaming block closed and its text out of the DOM", () => {
    render(<ThinkingCell cell={thinking()} />);

    expect(screen.getByRole("button")).toHaveProperty("ariaExpanded", "false");
    expect(screen.getByText("Thinking…")).toBeTruthy();
    expect(screen.queryByText("weighing the options")).toBeNull();
  });

  it("stays closed once sealed, showing only the elapsed stamp", () => {
    render(<ThinkingCell cell={thinking({ streaming: false, seconds: 3 })} />);

    expect(screen.getByText("Thought for 3s")).toBeTruthy();
    expect(screen.queryByText("weighing the options")).toBeNull();
  });

  it("reveals the reasoning on click", () => {
    render(<ThinkingCell cell={thinking({ streaming: false, seconds: 3 })} />);

    fireEvent.click(screen.getByRole("button"));

    expect(screen.getByRole("button")).toHaveProperty("ariaExpanded", "true");
    expect(screen.getByText("weighing the options")).toBeTruthy();
  });

  it("does not open itself when more reasoning streams in", () => {
    const { rerender } = render(<ThinkingCell cell={thinking()} />);

    rerender(<ThinkingCell cell={thinking({ text: "weighing the options, and more" })} />);

    expect(screen.getByRole("button")).toHaveProperty("ariaExpanded", "false");
  });

  it("leaves a block the user opened open when it seals", () => {
    const { rerender } = render(<ThinkingCell cell={thinking()} />);
    fireEvent.click(screen.getByRole("button"));

    rerender(<ThinkingCell cell={thinking({ streaming: false, seconds: 2 })} />);

    expect(screen.getByRole("button")).toHaveProperty("ariaExpanded", "true");
    expect(screen.getByText("weighing the options")).toBeTruthy();
  });
});
