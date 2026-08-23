import { describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import { ContextBar } from "./ContextBar";

function renderBar(over: Partial<React.ComponentProps<typeof ContextBar>> = {}) {
  const props = {
    context: { used: 60_000, window: 100_000 },
    busy: false,
    onCompact: vi.fn(),
    onFork: vi.fn(),
    compacting: false,
    ...over,
  };
  const view = render(<ContextBar {...props} />);
  return { ...view, props };
}

describe("ContextBar", () => {
  it("stays hidden while the window is not filling up", () => {
    // A meter that is always on screen is chrome, not information.
    const { container } = renderBar({ context: { used: 10_000, window: 100_000 } });
    expect(container.firstChild).toBeNull();
  });

  it("appears once the number starts to matter", () => {
    renderBar({ context: { used: 50_000, window: 100_000 } });
    expect(screen.getByText("50% full")).toBeTruthy();
  });

  it("renders nothing without a context to report", () => {
    const { container } = renderBar({ context: undefined });
    expect(container.firstChild).toBeNull();
  });

  it("renders nothing rather than dividing by a zero window", () => {
    const { container } = renderBar({ context: { used: 10, window: 0 } });
    expect(container.firstChild).toBeNull();
  });

  it("caps the reported figure at a full window", () => {
    renderBar({ context: { used: 150_000, window: 100_000 } });
    expect(screen.getByText("100% full")).toBeTruthy();
  });

  it("offers both ways out of a full window", () => {
    const { props } = renderBar();
    screen.getByText("Fork").click();
    screen.getByText("Compact").click();
    expect(props.onFork).toHaveBeenCalledTimes(1);
    expect(props.onCompact).toHaveBeenCalledTimes(1);
  });

  it("will not compact during a turn", () => {
    // Compacting mid-turn would rewrite the history the turn is reading.
    const { props } = renderBar({ busy: true });
    const compact = screen.getByText("Compact");
    expect(compact).toHaveProperty("disabled", true);
    compact.click();
    expect(props.onCompact).not.toHaveBeenCalled();
  });

  it("will not start a second compaction while one is running", () => {
    const { props } = renderBar({ compacting: true });
    const compact = screen.getByText("Compacting…");
    expect(compact).toHaveProperty("disabled", true);
    compact.click();
    expect(props.onCompact).not.toHaveBeenCalled();
  });

  it("keeps Fork available during a turn", () => {
    // Forking copies; it does not touch the running conversation.
    const { props } = renderBar({ busy: true });
    screen.getByText("Fork").click();
    expect(props.onFork).toHaveBeenCalledTimes(1);
  });
});
