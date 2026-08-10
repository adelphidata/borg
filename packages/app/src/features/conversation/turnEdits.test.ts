import { describe, expect, it } from "vitest";
import { collectEdits, truncatedPatchCount, turnEditCells } from "./turnEdits";
import type { Cell } from "./useConversation";

/**
 * A patch result in the daemon's own shape: banner, affected-path summary, and
 * the unified diff it applied, joined by blank lines
 * (`format_patch_success` in crates/cortex/src/tool_handlers/filesystem/patch.rs).
 */
function success(summary: string, diff: string): string {
  return `Patch applied successfully.\n\n${summary}\n\n${diff}`;
}

function tool(
  output: string,
  over: Partial<Cell & { kind: "tool" }> = {},
): Cell & { kind: "tool" } {
  return {
    kind: "tool",
    id: "call-1",
    name: "apply_patch",
    args: "",
    output,
    done: true,
    stderr: false,
    ...over,
  };
}

const MODIFY = success(
  "M src/main.rs",
  `--- a/src/main.rs
+++ b/src/main.rs
@@ -1,4 +1,5 @@
 fn main() {
-    println!("old");
+    println!("new");
+    flush();
 }`,
);

describe("collectEdits", () => {
  it("counts the lines a patch added and removed", () => {
    expect(collectEdits([tool(MODIFY)])).toEqual([
      { path: "src/main.rs", added: 2, removed: 1, diff: expect.stringContaining("@@") },
    ]);
  });

  it("does not count the file headers as changed lines", () => {
    // `+++ b/…` starts with "+" and `--- a/…` with "-": counted naively, every
    // file would report one phantom addition and one phantom removal.
    const [edit] = collectEdits([tool(success("A a.txt", "--- /dev/null\n+++ b/a.txt\n@@\n+one"))]);
    expect(edit).toMatchObject({ path: "a.txt", added: 1, removed: 0 });
  });

  it("names a deleted file from the source side, not /dev/null", () => {
    const output = success("D gone.txt", "--- a/gone.txt\n+++ /dev/null\n@@\n-was here");
    expect(collectEdits([tool(output)])[0]).toMatchObject({
      path: "gone.txt",
      added: 0,
      removed: 1,
    });
  });

  it("splits a multi-file patch into one row per file", () => {
    const output = success(
      "M a.rs\nM b.rs",
      "--- a/a.rs\n+++ b/a.rs\n@@\n+one\n\n--- a/b.rs\n+++ b/b.rs\n@@\n-two\n-three",
    );
    expect(collectEdits([tool(output)])).toMatchObject([
      { path: "a.rs", added: 1, removed: 0 },
      { path: "b.rs", added: 0, removed: 2 },
    ]);
  });

  it("sums repeat edits to one file into a single row", () => {
    const again = success("M src/main.rs", "--- a/src/main.rs\n+++ b/src/main.rs\n@@\n+one more");
    const edits = collectEdits([tool(MODIFY), tool(again, { id: "call-2" })]);
    expect(edits).toHaveLength(1);
    expect(edits[0]).toMatchObject({ path: "src/main.rs", added: 3, removed: 1 });
  });

  it("ignores a failed patch, which the daemon reports as an ordinary result", () => {
    // No stderr, no error status — the banner is the only signal there is.
    const failed = tool("Error applying patch: context did not match at line 4");
    expect(collectEdits([failed])).toEqual([]);
  });

  it("ignores a call that has not finished", () => {
    expect(collectEdits([tool(MODIFY, { done: false })])).toEqual([]);
  });

  it("is not fooled by a diff body that quotes the success banner", () => {
    const quoted = tool(
      success("M doc.md", '--- a/doc.md\n+++ b/doc.md\n@@\n+It said "applied successfully."'),
    );
    expect(collectEdits([quoted])).toHaveLength(1);
    // And the reverse: a failure whose body quotes it stays a failure.
    const lying = tool('Error applying patch: expected "Patch applied successfully."');
    expect(collectEdits([lying])).toEqual([]);
  });

  it("reads a removed line beginning with -- as content, not a file header", () => {
    // A deleted `-- comment` line renders as `--- comment` at column zero and
    // is only distinguishable from a header by the `+++` that must follow one.
    const output = success("M q.sql", "--- a/q.sql\n+++ b/q.sql\n@@\n--- comment\n+SELECT 1");
    expect(collectEdits([tool(output)])).toMatchObject([{ path: "q.sql", added: 1, removed: 1 }]);
  });

  it("keeps the diff for each file so the row can show it", () => {
    const [edit] = collectEdits([tool(MODIFY)]);
    expect(edit!.diff).toContain('-    println!("old");');
    expect(edit!.diff).toContain('+    println!("new");');
  });

  it("refuses to count a patch whose output was truncated in transit", () => {
    // The daemon elides the middle of a long result and the banner survives in
    // the head, so this looks like a clean success with a hole in its diff:
    // the tail's lines would be charged to the wrong file and any file whose
    // header fell in the hole would vanish. Wrong totals presented as fact are
    // worse than none.
    const truncated = tool(
      success(
        "M big.rs",
        "--- a/big.rs\n+++ b/big.rs\n@@\n+one\n\n" +
          "…[head+tail of 10000 tokens / 4000 total lines; 38000 tokens omitted]…\n\n" +
          "+two\n-three",
      ),
    );
    expect(collectEdits([truncated])).toEqual([]);
    expect(truncatedPatchCount([truncated])).toBe(1);
  });

  it("does not flag an intact patch as truncated", () => {
    expect(truncatedPatchCount([tool(MODIFY)])).toBe(0);
  });

  it("is not fooled by content that forges both header lines", () => {
    // A removed `-- a/x` beside an added `++ b/x` renders exactly like a file
    // header. Only a real header is followed by a hunk marker.
    const output = success(
      "M real.rs",
      "--- a/real.rs\n+++ b/real.rs\n@@\n--- a/fake.rs\n+++ b/fake.rs\n+kept",
    );
    expect(collectEdits([tool(output)])).toMatchObject([{ path: "real.rs", added: 2, removed: 1 }]);
  });

  it("leaves out a file whose diff came back empty", () => {
    const noop = success("= same.txt (already applied — no changes made)", "");
    expect(collectEdits([tool(noop)])).toEqual([]);
  });
});

describe("turnEditCells", () => {
  const user = (text: string): Cell => ({ kind: "user", text });

  it("takes only the newest turn, so the card retires when the next lands", () => {
    const cells: Cell[] = [
      user("first"),
      tool(MODIFY, { id: "old" }),
      user("second"),
      tool(MODIFY, { id: "new" }),
    ];
    expect(turnEditCells(cells).map((c) => c.id)).toEqual(["new"]);
  });

  it("is empty for a turn that changed nothing", () => {
    expect(turnEditCells([user("hello")])).toEqual([]);
  });

  it("ignores tools that do not write files", () => {
    const cells: Cell[] = [user("go"), tool(MODIFY, { name: "read_file", id: "r" })];
    expect(turnEditCells(cells)).toEqual([]);
  });

  it("covers a transcript that opens with tool calls and no user cell", () => {
    expect(turnEditCells([tool(MODIFY)])).toHaveLength(1);
  });
});
