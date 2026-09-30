import { describe, expect, it } from "vitest";
import { createReloadBudget } from "./reload-budget";

/** A budget on a clock the test moves by hand. */
function budgetAt(options: { limit?: number; windowMs?: number } = {}) {
  const clock = { now: 0 };
  const budget = createReloadBudget({ ...options, now: () => clock.now });
  return { budget, clock };
}

describe("createReloadBudget", () => {
  it("allows three reloads in a minute, then refuses", () => {
    const { budget, clock } = budgetAt();
    const answers = [0, 1_000, 2_000, 3_000, 4_000].map((at) => {
      clock.now = at;
      return budget.take();
    });
    expect(answers).toEqual([true, true, true, false, false]);
  });

  it.each([
    // The window slides: a reload stops counting once it is a full window old.
    ["one millisecond before the oldest reload expires", 59_999, false],
    ["exactly when the oldest reload expires", 60_000, true],
    ["long after the crash loop", 3_600_000, true],
  ])("answers a crash %s", (_case, at, allowed) => {
    const { budget, clock } = budgetAt();
    for (const t of [0, 10, 20]) {
      clock.now = t;
      budget.take();
    }
    clock.now = at;
    expect(budget.take()).toBe(allowed);
  });

  it("does not charge the budget for a refused reload", () => {
    // Refusals that counted would keep a window locked out for as long as
    // something kept asking.
    const { budget, clock } = budgetAt({ limit: 1, windowMs: 1_000 });
    expect(budget.take()).toBe(true);
    clock.now = 900;
    expect(budget.take()).toBe(false);
    clock.now = 1_000;
    expect(budget.take()).toBe(true);
  });

  it("frees only the reloads that have expired", () => {
    const { budget, clock } = budgetAt();
    for (const t of [0, 30_000, 40_000]) {
      clock.now = t;
      budget.take();
    }
    clock.now = 61_000; // the reload at 0 is gone; the other two still count
    expect(budget.take()).toBe(true);
    expect(budget.take()).toBe(false);
  });

  it("honors a custom limit", () => {
    const { budget } = budgetAt({ limit: 2 });
    expect([budget.take(), budget.take(), budget.take()]).toEqual([true, true, false]);
  });
});
