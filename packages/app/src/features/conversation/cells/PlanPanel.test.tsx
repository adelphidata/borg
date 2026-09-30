import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { PlanPanel } from "./PlanPanel";
import type { PlanStep } from "../useConversation";

// jsdom has no layout. Each step is laid out as a 20px row, and the list shows
// two of them at a time; scroll positions are remembered per element.
const ROW_PX = 20;
const LIST_PX = 40;
const scrollTops = new WeakMap<Element, number>();
const patched = ["offsetTop", "offsetHeight", "clientHeight", "scrollTop"] as const;
const original = new Map<string, PropertyDescriptor | undefined>();
const scrollIntoView = vi.fn();

function rowIndex(el: Element): number {
  return Array.prototype.indexOf.call(el.parentElement?.children ?? [], el);
}

beforeEach(() => {
  for (const name of patched) {
    original.set(name, Object.getOwnPropertyDescriptor(HTMLElement.prototype, name));
  }
  Object.defineProperties(HTMLElement.prototype, {
    offsetTop: {
      configurable: true,
      get(this: HTMLElement) {
        return this.tagName === "LI" ? rowIndex(this) * ROW_PX : 0;
      },
    },
    offsetHeight: {
      configurable: true,
      get(this: HTMLElement) {
        return this.tagName === "LI" ? ROW_PX : 0;
      },
    },
    clientHeight: {
      configurable: true,
      get(this: HTMLElement) {
        return this.tagName === "UL" ? LIST_PX : 0;
      },
    },
    scrollTop: {
      configurable: true,
      get(this: HTMLElement) {
        return scrollTops.get(this) ?? 0;
      },
      set(this: HTMLElement, value: number) {
        scrollTops.set(this, value);
      },
    },
  });
  scrollIntoView.mockClear();
  Element.prototype.scrollIntoView = scrollIntoView;
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  for (const name of patched) {
    const descriptor = original.get(name);
    if (descriptor) Object.defineProperty(HTMLElement.prototype, name, descriptor);
    else delete (HTMLElement.prototype as unknown as Record<string, unknown>)[name];
  }
  delete (Element.prototype as { scrollIntoView?: unknown }).scrollIntoView;
});

/** Ten steps with the one at `active` running. */
function plan(active: number | undefined): PlanStep[] {
  return Array.from({ length: 10 }, (_, i) => ({
    id: String(i),
    text: `step ${i}`,
    status: i === active ? "in_progress" : i < (active ?? 0) ? "done" : "pending",
  }));
}

const STEPS: PlanStep[] = [
  { id: "1", text: "read the config", status: "done" },
  { id: "2", text: "rewrite the parser", status: "in_progress" },
  { id: "3", text: "run the tests", status: "pending" },
];

/** The panel opens expanded, so collapsing it is what the header tests need. */
function collapse() {
  fireEvent.click(screen.getByRole("button"));
}

describe("PlanPanel", () => {
  it("names the running step once collapsed", () => {
    render(<PlanPanel steps={STEPS} />);
    collapse();

    expect(screen.getByRole("button")).toHaveProperty("ariaExpanded", "false");
    // The step the agent is on, without paying an expand for it.
    expect(screen.getByText("rewrite the parser")).toBeTruthy();
    expect(screen.getByText("1/3")).toBeTruthy();
    // Still collapsed: the steps either side stay out of the DOM.
    expect(screen.queryByText("run the tests")).toBeNull();
  });

  it("shows only the count when no step is running", () => {
    const settled = STEPS.map((s) => (s.status === "in_progress" ? { ...s, status: "done" } : s));
    render(<PlanPanel steps={settled} />);
    collapse();

    expect(screen.getByText("2/3")).toBeTruthy();
    expect(screen.queryByText("rewrite the parser")).toBeNull();
  });

  it("keeps the running step out of the header while expanded", () => {
    render(<PlanPanel steps={STEPS} />);

    // Expanded, the list already names it — a header copy would say it twice.
    expect(screen.getAllByText("rewrite the parser")).toHaveLength(1);
    expect(screen.getByText("run the tests")).toBeTruthy();
  });

  it("caps a long plan and scrolls inside the cap", () => {
    const long = Array.from({ length: 40 }, (_, i) => ({
      id: String(i),
      text: `step ${i}`,
      status: "pending",
    }));
    render(<PlanPanel steps={long} />);

    const list = screen.getByRole("list");
    expect(list.className).toContain("max-h-[30vh]");
    expect(list.className).toContain("overflow-y-auto");
    // Capped, not cut: every step is still there to scroll to.
    expect(screen.getAllByRole("listitem")).toHaveLength(40);
  });

  it("scrolls the list just far enough to show the running step below the fold", () => {
    render(<PlanPanel steps={plan(5)} />);
    // Row 5 spans 100-120; a 40px window ending at 120 starts at 80.
    expect(screen.getByRole("list").scrollTop).toBe(80);
  });

  it("follows the running step as the plan moves on, up as well as down", () => {
    const { rerender } = render(<PlanPanel steps={plan(5)} />);
    rerender(<PlanPanel steps={plan(6)} />);
    expect(screen.getByRole("list").scrollTop).toBe(100);

    // A plan that is rewritten can put the step in hand back above the view.
    rerender(<PlanPanel steps={plan(1)} />);
    expect(screen.getByRole("list").scrollTop).toBe(20);
  });

  it("leaves the list alone when the running step is already in view", () => {
    render(<PlanPanel steps={plan(1)} />);
    expect(screen.getByRole("list").scrollTop).toBe(0);
  });

  it("does not move the list while the same step is still running", () => {
    // The plan re-publishes on every update; the reader may be looking ahead.
    const { rerender } = render(<PlanPanel steps={plan(5)} />);
    const list = screen.getByRole("list");
    list.scrollTop = 0;
    rerender(<PlanPanel steps={plan(5).map((s) => ({ ...s }))} />);
    expect(list.scrollTop).toBe(0);
  });

  it("moves nothing when no step is running", () => {
    render(<PlanPanel steps={plan(undefined)} />);
    expect(screen.getByRole("list").scrollTop).toBe(0);
  });

  it("never scrolls the page the panel sits in", () => {
    // `scrollIntoView` would move every scrollable ancestor, so a reader
    // scrolled up in the transcript would be hauled somewhere else.
    const page = document.createElement("div");
    document.body.appendChild(page);
    page.scrollTop = 300;
    render(<PlanPanel steps={plan(8)} />, { container: page });

    expect(screen.getByRole("list").scrollTop).toBe(140);
    expect(page.scrollTop).toBe(300);
    expect(scrollIntoView).not.toHaveBeenCalled();
    page.remove();
  });

  it("renders nothing without steps", () => {
    const { container } = render(<PlanPanel steps={[]} />);

    expect(container.firstChild).toBeNull();
  });
});
