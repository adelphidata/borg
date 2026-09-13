import { describe, expect, it } from "vitest";
import { groupCells, type RenderItem } from "./cells/toolRuns";
import type { Cell } from "./useConversation";
import {
  INITIAL_BUDGET,
  MIN_USER_TURNS,
  TOP_EDGE_PX,
  budgetCutIndex,
  cellWeight,
  cutHidesEverything,
  itemWeight,
  revealEarlier,
  shouldRevealOnScroll,
} from "./renderBudget";

const user = (text = "ask"): Cell => ({ kind: "user", text });
const reply = (text = "answer"): Cell => ({ kind: "assistant", text });
const tool = (id: string, output = ""): Cell => ({
  kind: "tool",
  id,
  name: "read_file",
  args: "{}",
  output,
  done: true,
  stderr: false,
});

/** `n` plain exchanges, oldest first. */
function exchanges(n: number): Cell[] {
  return Array.from({ length: n }, (_, i) => [user(`ask ${i}`), reply(`answer ${i}`)]).flat();
}

const group = (cells: Cell[]): RenderItem[] => groupCells(cells, false);

describe("weights", () => {
  it("prices a tool call above a line of prose", () => {
    expect(cellWeight(tool("t1"))).toBeGreaterThan(cellWeight(reply("a short answer")));
  });

  it("prices long text above short text", () => {
    expect(cellWeight(reply("x".repeat(20_000)))).toBeGreaterThan(cellWeight(reply("x")));
  });

  it("prices a collapsed run below the calls it holds", () => {
    const cells = [tool("t1", "x".repeat(9_000)), tool("t2", "x".repeat(9_000))];
    const run = group(cells)[0]!;
    expect(run.type).toBe("run");
    const raw = cells.reduce((sum, c) => sum + cellWeight(c), 0);
    expect(itemWeight(run)).toBeLessThan(raw);
  });

  it("prices a run above a single line, since opening it mounts everything", () => {
    const run = group([tool("t1"), tool("t2"), tool("t3")])[0]!;
    expect(itemWeight(run)).toBeGreaterThan(cellWeight(reply("one line")));
  });
});

describe("budgetCutIndex", () => {
  it("shows a short transcript whole", () => {
    expect(budgetCutIndex(group(exchanges(3)), INITIAL_BUDGET)).toBe(0);
  });

  it("hides the oldest history once the budget is spent", () => {
    const cut = budgetCutIndex(group(exchanges(400)), INITIAL_BUDGET);
    expect(cut).toBeGreaterThan(0);
  });

  it("keeps the newest turns, not the oldest", () => {
    const cells = exchanges(400);
    const items = group(cells);
    const cut = budgetCutIndex(items, INITIAL_BUDGET);
    const visible = items.filter((i) => i.index >= cut);
    expect(visible.at(-1)).toEqual(items.at(-1));
  });

  it("never splits a collapsed tool run", () => {
    // A run that straddles the budget line must be shown whole or not at all —
    // half a run would report half the work and key against a partial list.
    const heavy = Array.from({ length: 60 }, (_, i) => tool(`t${i}`, "x".repeat(4_000)));
    const cells = [...exchanges(1), ...heavy, user("last"), reply("done")];
    const items = group(cells);
    const runs = items.filter((i) => i.type === "run");
    expect(runs).toHaveLength(1);

    const cut = budgetCutIndex(items, INITIAL_BUDGET);
    const visible = items.filter((i) => i.index >= cut);
    // What is rendered is always a whole suffix of the grouped items, so no
    // item — least of all a run — can be shown in part.
    expect(visible).toEqual(items.slice(items.length - visible.length));
  });

  it("always keeps the last few questions, however heavy they are", () => {
    // Two enormous turns alone blow any budget; hiding them would open the
    // conversation on a reply with no visible question.
    const huge = [
      user("first question"),
      reply("x".repeat(400_000)),
      user("second question"),
      reply("x".repeat(400_000)),
    ];
    const items = group(huge);
    const cut = budgetCutIndex(items, INITIAL_BUDGET);
    const visible = items.filter((i) => i.index >= cut);
    const userCells = visible.filter((i) => i.type === "cell" && i.cell.kind === "user");
    expect(userCells.length).toBeGreaterThanOrEqual(MIN_USER_TURNS);
  });

  it("does not move when the transcript grows at the tail", () => {
    // The streaming case: new cells arrive constantly, and a recomputed cut
    // would keep sliding forward and hide history under the reader.
    const items = group(exchanges(400));
    const cut = budgetCutIndex(items, INITIAL_BUDGET);
    const wasVisible = items.filter((i) => i.index >= cut);
    const grown = group([...exchanges(400), user("new"), reply("fresh")]);
    const nowVisible = grown.filter((i) => i.index >= cut);

    // Everything the reader could already see is still on screen, plus the new
    // cells — nothing scrolled out from under them.
    expect(nowVisible.slice(0, wasVisible.length)).toEqual(wasVisible);
    expect(nowVisible).toHaveLength(wasVisible.length + 2);
  });
});

describe("revealEarlier", () => {
  it("walks the cut back to zero in a bounded number of presses", () => {
    const items = group(exchanges(400));
    let cut = budgetCutIndex(items, INITIAL_BUDGET);
    expect(cut).toBeGreaterThan(0);

    const seen = [cut];
    for (let press = 0; press < 100 && cut > 0; press += 1) {
      const next = revealEarlier(items, cut);
      expect(next).toBeLessThan(cut);
      cut = next;
      seen.push(cut);
    }
    expect(cut).toBe(0);
    expect(seen.length).toBeGreaterThan(2);
  });

  it("reveals nothing more once everything is shown", () => {
    expect(revealEarlier(group(exchanges(3)), 0)).toBe(0);
  });
});

describe("cutHidesEverything", () => {
  it("leaves a cut alone while anything is still visible", () => {
    const items = group(exchanges(6));
    const cut = budgetCutIndex(items, 1);
    expect(cut).toBeGreaterThan(0);
    expect(cutHidesEverything(items, cut)).toBe(false);
  });

  it("catches a cut that lands inside the last collapsed run", () => {
    // The failure this guards: a cut can sit well inside the cell count and
    // still hide every item, because a run reports the index of its FIRST
    // cell. Rewinding an edit, or a turn settling and folding live tool cells
    // into one run, both produce exactly this — and the page would render
    // nothing but its own "Show earlier messages" button.
    const cells = [user(), reply(), tool("t1"), tool("t2"), tool("t3")];
    const items = group(cells);
    const lastRun = items.at(-1)!;
    expect(lastRun.type).toBe("run");
    // A cut past the run's own index but still below the cell count — the case
    // a `cutIndex >= cells.length` test would wave through.
    const cut = lastRun.index + 1;
    expect(cut).toBeLessThan(cells.length);
    expect(cutHidesEverything(items, cut)).toBe(true);
  });

  it("treats showing everything as never hiding anything", () => {
    expect(cutHidesEverything(group(exchanges(3)), 0)).toBe(false);
    expect(cutHidesEverything([], 0)).toBe(false);
  });
});

describe("shouldRevealOnScroll", () => {
  it("reveals when an upward scroll reaches the top edge", () => {
    expect(shouldRevealOnScroll(400, TOP_EDGE_PX)).toBe(true);
    expect(shouldRevealOnScroll(50, 0)).toBe(true);
  });

  it("ignores an upward scroll that stops short of the edge", () => {
    expect(shouldRevealOnScroll(800, TOP_EDGE_PX + 1)).toBe(false);
  });

  it("ignores the reveal's own re-anchor, which moves the scroller down", () => {
    // Mounting older cells above the reader pushes `scrollTop` down by their
    // height; that is not a request for more.
    expect(shouldRevealOnScroll(20, 900)).toBe(false);
    expect(shouldRevealOnScroll(0, 0)).toBe(false);
  });
});
