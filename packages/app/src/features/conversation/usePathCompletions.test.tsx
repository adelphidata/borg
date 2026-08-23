import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, render } from "@testing-library/react";
import { BorgError, GrpcStatus } from "@borg/client";
import { PlatformProvider } from "../../lib/platform";
import { fakeDaemon } from "../../test/harness";
import { usePathCompletions, type PathRow } from "./usePathCompletions";

let rows: PathRow[] = [];

function Probe({ sessionId, prefix }: { sessionId?: string; prefix: string | null }) {
  rows = usePathCompletions(sessionId, prefix);
  return null;
}

function mount(
  daemon: ReturnType<typeof fakeDaemon>,
  props: { sessionId?: string; prefix: string | null },
) {
  const tree = (next: typeof props) => (
    <PlatformProvider
      platform={{ kind: "desktop", transport: daemon.transport, titleBarDrag: false }}
    >
      <Probe {...next} />
    </PlatformProvider>
  );
  const view = render(tree(props));
  return { rerender: (next: typeof props) => view.rerender(tree(next)) };
}

/** Let the settle delay expire and the reply promise resolve. */
async function settle(ms = 200) {
  await act(async () => {
    vi.advanceTimersByTime(ms);
  });
  await act(async () => {});
}

beforeEach(() => {
  rows = [];
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
});

describe("usePathCompletions", () => {
  it("offers nothing when the composer holds no path token", async () => {
    const daemon = fakeDaemon({ ListPaths: { paths: [] } });
    mount(daemon, { sessionId: "s1", prefix: null });
    await settle();
    expect(daemon.calls).toEqual([]);
    expect(rows).toEqual([]);
  });

  it("waits for a session before asking", async () => {
    const daemon = fakeDaemon({ ListPaths: { paths: [] } });
    mount(daemon, { sessionId: undefined, prefix: "src/" });
    await settle();
    expect(daemon.calls).toEqual([]);
  });

  it("returns the daemon's paths, keeping directories marked", async () => {
    const daemon = fakeDaemon({
      ListPaths: {
        paths: [
          { path: "src/", isDir: true },
          { path: "src/main.rs", isDir: false },
        ],
      },
    });
    mount(daemon, { sessionId: "s1", prefix: "src" });
    await settle();
    expect(rows).toEqual([
      { path: "src/", isDir: true },
      { path: "src/main.rs", isDir: false },
    ]);
  });

  it("asks once for a burst of keystrokes, with the last prefix typed", async () => {
    // Every keystroke inside an `@` token would otherwise be a directory read.
    const daemon = fakeDaemon({ ListPaths: { paths: [] } });
    const { rerender } = mount(daemon, { sessionId: "s1", prefix: "s" });
    for (const prefix of ["sr", "src", "src/m"]) {
      await act(async () => {
        vi.advanceTimersByTime(40);
      });
      rerender({ sessionId: "s1", prefix });
    }
    await settle();
    expect(daemon.calls).toHaveLength(1);
    expect(daemon.calls[0].request).toMatchObject({ prefix: "src/m", sessionId: "s1" });
  });

  it("ignores an answer the user has already typed past", async () => {
    // A slow first answer must not repopulate the menu with the wrong list.
    let releaseFirst!: (value: { paths: PathRow[] }) => void;
    const daemon = fakeDaemon({
      ListPaths: (request: unknown) =>
        (request as { prefix: string }).prefix === "src"
          ? new Promise((resolve) => (releaseFirst = resolve))
          : { paths: [{ path: "docs/plan.md", isDir: false }] },
    });
    const { rerender } = mount(daemon, { sessionId: "s1", prefix: "src" });
    await settle();

    rerender({ sessionId: "s1", prefix: "docs" });
    await settle();
    expect(rows).toEqual([{ path: "docs/plan.md", isDir: false }]);

    await act(async () => {
      releaseFirst({ paths: [{ path: "src/main.rs", isDir: false }] });
    });
    expect(rows).toEqual([{ path: "docs/plan.md", isDir: false }]);
  });

  it("leaves the box alone when the daemon cannot list the workspace", async () => {
    // A completion menu is an offer, not an answer — an error must not land
    // where file names go.
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    const daemon = fakeDaemon({
      ListPaths: new BorgError(GrpcStatus.NotFound, "workspace missing"),
    });
    mount(daemon, { sessionId: "s1", prefix: "src" });
    await settle();
    expect(rows).toEqual([]);
    expect(warn).toHaveBeenCalled();
  });

  it("drops the rows as soon as the token goes away", async () => {
    const daemon = fakeDaemon({ ListPaths: { paths: [{ path: "src/main.rs", isDir: false }] } });
    const { rerender } = mount(daemon, { sessionId: "s1", prefix: "src" });
    await settle();
    expect(rows).toHaveLength(1);

    rerender({ sessionId: "s1", prefix: null });
    await settle();
    expect(rows).toEqual([]);
  });
});
