import { describe, expect, it } from "vitest";
import { groupCells, runSummary, type ToolCellData } from "./toolRuns";
import type { Cell } from "../useConversation";

function tool(id: string, over: Partial<ToolCellData> = {}): ToolCellData {
  return {
    kind: "tool",
    id,
    name: "read_file",
    args: "",
    output: "",
    done: true,
    stderr: false,
    ...over,
  };
}

const user = (text: string): Cell => ({ kind: "user", text });

describe("groupCells", () => {
  it("folds a run of settled calls into one item", () => {
    const items = groupCells([tool("a"), tool("b"), tool("c")], false);
    expect(items).toHaveLength(1);
    expect(items[0]).toMatchObject({ type: "run", key: "run:a" });
  });

  it("leaves a lone call as its own row — one row is already one line", () => {
    const items = groupCells([tool("a")], false);
    expect(items[0]!.type).toBe("cell");
  });

  it("keys a run by its first call, so settling does not remount it", () => {
    // The same calls arrive live at one set of positions and rehydrate at
    // another; an index-derived key would change and throw the run away.
    const live = groupCells([user("go"), tool("a"), tool("b")], true);
    const reloaded = groupCells([user("earlier"), user("go"), tool("a"), tool("b")], false);
    const keyOf = (items: ReturnType<typeof groupCells>) =>
      items.find((i) => i.type === "run")?.key;
    expect(keyOf(live)).toBe("run:a");
    expect(keyOf(reloaded)).toBe("run:a");
  });

  it("keeps a still-running call out of the fold and visible", () => {
    const items = groupCells([tool("a"), tool("b"), tool("c", { done: false })], false);
    expect(items).toMatchObject([{ type: "run" }, { type: "cell" }]);
  });

  it.each([
    ["failed", { output: "Exit code: 1\n--- stdout ---\n--- stderr ---\nnope" }],
    ["was stopped", { output: "[tool call cancelled by user]" }],
  ])("keeps a call that %s visible, and splits the run around it", (_, over) => {
    const items = groupCells(
      [tool("a"), tool("b"), tool("bad", over), tool("d"), tool("e")],
      false,
    );
    expect(items).toMatchObject([
      { type: "run", key: "run:a" },
      { type: "cell" },
      { type: "run", key: "run:d" },
    ]);
  });

  it("folds a call that only wrote to stderr, because stderr is not failure", () => {
    const noisy = tool("b", {
      stderr: true,
      output: "Exit code: 0\n--- stdout ---\n--- stderr ---\nwarn",
    });
    expect(groupCells([tool("a"), noisy, tool("c")], false)).toMatchObject([{ type: "run" }]);
  });

  it("numbers a second run whose first call reuses an id, so keys stay unique", () => {
    const items = groupCells(
      [
        tool("a"),
        tool("b"),
        user("more"),
        tool("a"),
        tool("b"),
        user("again"),
        tool("a"),
        tool("c"),
      ],
      false,
    );
    expect(items.filter((i) => i.type === "run").map((i) => i.key)).toEqual([
      "run:a",
      "run:a#2",
      "run:a#3",
    ]);
  });

  it("breaks a run at anything that is not a tool call", () => {
    const items = groupCells([tool("a"), tool("b"), user("hi"), tool("c"), tool("d")], false);
    expect(items.map((i) => i.type)).toEqual(["run", "cell", "run"]);
  });

  it("reports each passed-through cell's own index, so ordinals still line up", () => {
    const items = groupCells([user("one"), tool("a"), tool("b"), user("two")], false);
    const cells = items.filter((i) => i.type === "cell");
    expect(cells.map((c) => c.index)).toEqual([0, 3]);
  });

  it("marks only the trailing run live, and only while a turn is running", () => {
    const cells = [tool("a"), tool("b"), user("steer"), tool("c"), tool("d")];
    const busy = groupCells(cells, true).filter((i) => i.type === "run");
    expect(busy.map((r) => r.live)).toEqual([false, true]);
    const settled = groupCells(cells, false).filter((i) => i.type === "run");
    expect(settled.map((r) => r.live)).toEqual([false, false]);
  });

  it("does not call a trailing plain cell live", () => {
    const items = groupCells([tool("a"), tool("b"), user("hi")], true);
    expect(items.filter((i) => i.type === "run")[0]!.live).toBe(false);
  });

  it("passes an empty transcript through untouched", () => {
    expect(groupCells([], false)).toEqual([]);
  });
});

describe("runSummary", () => {
  const withPath = (id: string, path: string, name = "read_file") =>
    tool(id, { name, args: JSON.stringify({ path }) });

  /**
   * A patch call as the daemon actually sends it: the arguments carry the patch
   * text and nothing else, so the files it touched are legible only in the
   * result. A fixture with a `path` argument would test a shape that never
   * arrives — and would hide that the summary cannot read one.
   */
  const patch = (id: string, ...paths: string[]) =>
    tool(id, {
      name: "apply_patch",
      args: JSON.stringify({ patch: "*** Begin Patch" }),
      output: `Patch applied successfully.\n\n${paths
        .map((p) => `--- a/${p}\n+++ b/${p}\n@@\n+one`)
        .join("\n\n")}`,
    });

  it("names the file when a category holds exactly one", () => {
    const summary = runSummary([patch("a", "src/toolRuns.ts"), withPath("b", "a.ts")], false);
    expect(summary).toBe("Edited toolRuns.ts, read a.ts");
  });

  it("counts the files a patch touched, not the calls that touched them", () => {
    // Three calls editing one file is one changed file. Counting calls would
    // report "3 files" for a turn that changed exactly one.
    const summary = runSummary(
      [patch("a", "src/main.rs"), patch("b", "src/main.rs"), patch("c", "src/main.rs")],
      false,
    );
    expect(summary).toBe("Edited main.rs");
  });

  it("counts every file of a patch that spans several", () => {
    expect(runSummary([patch("a", "a.rs", "b.rs", "c.rs")], false)).toBe("Edited 3 files");
  });

  it("names the activity without a number when no path can be read", () => {
    // A failed patch reports no diff, so there is nothing to count. "0 files"
    // next to a call that plainly ran would read as a bug.
    const failed = tool("a", { name: "apply_patch", output: "Error applying patch: no match" });
    expect(runSummary([failed], false)).toBe("Edited files");
  });

  it("counts when a category holds several", () => {
    const summary = runSummary([withPath("a", "a.ts"), withPath("b", "b.ts")], false);
    expect(summary).toBe("Read 2 files");
  });

  it("counts distinct subjects, so reading one file twice is not two files", () => {
    const summary = runSummary([withPath("a", "same.ts"), withPath("b", "same.ts")], false);
    expect(summary).toBe("Read same.ts");
  });

  it("does not call a web search a file", () => {
    const search = (id: string, query: string) =>
      tool(id, { name: "web_search", args: JSON.stringify({ query }) });
    expect(runSummary([search("a", "one"), search("b", "two")], false)).toBe("Looked up 2 things");
  });

  it("speaks in the present tense while the run is live", () => {
    const tools = [withPath("a", "a.ts"), withPath("b", "b.ts")];
    expect(runSummary(tools, true)).toBe("Reading 2 files");
  });

  it("orders edits before exploration before commands", () => {
    const summary = runSummary(
      [
        tool("s", { name: "run_shell", args: JSON.stringify({ command: "cargo test" }) }),
        withPath("r", "a.ts"),
        patch("p", "b.ts"),
      ],
      false,
    );
    expect(summary).toBe("Edited b.ts, read a.ts, ran cargo test");
  });

  it("quotes only the first line of a multi-line command", () => {
    const summary = runSummary(
      [
        tool("s", {
          name: "run_shell",
          args: JSON.stringify({ command: "cargo test\ncargo fmt" }),
        }),
        withPath("r", "a.ts"),
      ],
      false,
    );
    expect(summary).toContain("ran cargo test");
    expect(summary).not.toContain("\n");
  });

  it("falls back to a count when a call names no subject", () => {
    expect(runSummary([tool("a", { name: "list_agents", args: "" })], false)).toBe("Used 1 tool");
  });

  it("counts an unrecognized tool as a tool rather than guessing", () => {
    const summary = runSummary(
      [tool("a", { name: "gmail", args: "" }), tool("b", { name: "notion", args: "" })],
      false,
    );
    expect(summary).toBe("Used 2 tools");
  });
});
