import { describe, expect, it } from "vitest";
import type { Cell } from "../useConversation";
import { placeTurnUsage, usageLabel } from "./turnUsage";

const usage = (input: number, output: number, over: Partial<Cell> = {}): Cell =>
  ({ kind: "usage", input, output, cacheRead: 0, cacheWrite: 0, ...over }) as Cell;
const user: Cell = { kind: "user", text: "hi" };
const reply: Cell = { kind: "assistant", text: "hello" };

describe("usageLabel", () => {
  it("leads with the time the turn took", () => {
    expect(
      usageLabel({ input: 1200, output: 340, cacheRead: 0, cacheWrite: 0, elapsedMs: 12_400 }),
    ).toBe("12.4s · 1.2K in · 340 out");
  });

  it("counts cache reads and writes together, only when there were any", () => {
    expect(usageLabel({ input: 10, output: 5, cacheRead: 900, cacheWrite: 100 })).toBe(
      "10 in · 5 out · 1K cached",
    );
  });

  it("never claims a turn cost zero tokens", () => {
    expect(usageLabel({ input: 0, output: 0, cacheRead: 0, cacheWrite: 0, elapsedMs: 800 })).toBe(
      "0.8s",
    );
    expect(usageLabel({ input: 0, output: 0, cacheRead: 0, cacheWrite: 0 })).toBe("");
  });
});

describe("placeTurnUsage", () => {
  it("puts a turn's total on its last reply, with the time the turn was stamped with", () => {
    const cells: Cell[] = [
      user,
      reply,
      usage(100, 10),
      {
        kind: "tool",
        id: "t1",
        name: "read_file",
        args: "{}",
        output: "",
        done: true,
        stderr: false,
      },
      reply,
      usage(200, 20, { elapsedMs: 4300 }),
    ];
    expect([...placeTurnUsage(cells)]).toEqual([[4, "4.3s · 300 in · 30 out"]]);
  });

  it("keeps each turn's numbers to that turn", () => {
    const cells = [user, reply, usage(1, 1), user, reply, usage(2, 2)];
    expect([...placeTurnUsage(cells)]).toEqual([
      [1, "1 in · 1 out"],
      [4, "2 in · 2 out"],
    ]);
  });

  it("leaves a turn with no reply its numbers, on its last usage cell", () => {
    const cells = [
      user,
      usage(5, 0),
      { kind: "error", text: "boom" } as Cell,
      usage(0, 0, { elapsedMs: 900 }),
    ];
    expect([...placeTurnUsage(cells)]).toEqual([[3, "0.9s · 5 in · 0 out"]]);
  });

  it("shows nothing for a turn with nothing to report", () => {
    expect(placeTurnUsage([user, reply, usage(0, 0)]).size).toBe(0);
    expect(placeTurnUsage([user, reply]).size).toBe(0);
  });
});
