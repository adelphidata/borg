import { afterEach, describe, expect, it } from "vitest";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { ChangedFilesCard } from "./ChangedFilesCard";
import type { FileEdit } from "../turnEdits";

const EDITS: FileEdit[] = [
  {
    path: "crates/cortex/src/session.rs",
    added: 12,
    removed: 3,
    diff: "--- a/crates/cortex/src/session.rs\n+++ b/crates/cortex/src/session.rs\n@@\n+let title = derive();",
  },
  { path: "packages/app/src/App.tsx", added: 1, removed: 0, diff: "--- a\n+++ b\n@@\n+import x;" },
];

afterEach(cleanup);

describe("ChangedFilesCard", () => {
  it("answers what is different now, file by file", () => {
    render(<ChangedFilesCard edits={EDITS} />);
    expect(screen.getByText("2 files changed")).toBeTruthy();
    expect(screen.getByText("crates/cortex/src/session.rs")).toBeTruthy();
    expect(screen.getByText("+12")).toBeTruthy();
    expect(screen.getByText("−3")).toBeTruthy();
  });

  it("counts one changed file in the singular", () => {
    render(<ChangedFilesCard edits={[EDITS[0]!]} />);
    expect(screen.getByText("1 file changed")).toBeTruthy();
  });

  it("leaves out a count of zero rather than printing +0", () => {
    render(<ChangedFilesCard edits={[EDITS[1]!]} />);
    expect(screen.getByText("+1")).toBeTruthy();
    expect(screen.queryByText("−0")).toBeNull();
  });

  it("opens that file's diff on click, and only that one", () => {
    render(<ChangedFilesCard edits={EDITS} />);
    fireEvent.click(screen.getByText("crates/cortex/src/session.rs"));
    expect(screen.getByText(/let title = derive/)).toBeTruthy();
    expect(screen.queryByText(/import x/)).toBeNull();
  });

  it("closes the diff again on a second click", () => {
    render(<ChangedFilesCard edits={EDITS} />);
    const row = screen.getByText("packages/app/src/App.tsx");
    fireEvent.click(row);
    expect(screen.getByText(/import x/)).toBeTruthy();
    fireEvent.click(row);
    expect(screen.queryByText(/import x/)).toBeNull();
  });
});
