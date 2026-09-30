import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { render } from "@testing-library/react";
import { PlatformProvider } from "../../lib/platform";
import { fakeDaemon, fakeNative } from "../../test/harness";
import { pendingPrompts, useTurnNotifications } from "./useTurnNotifications";
import type { Cell } from "./useConversation";

const cell = (over: Partial<Cell>): Cell => ({ id: "c", kind: "assistant", ...over }) as Cell;

let native: ReturnType<typeof fakeNative>;
let focused = false;

let title: string | undefined;

function Probe({ cells, busy }: { cells: Cell[]; busy: boolean }) {
  useTurnNotifications(cells, busy, "sess-1", title);
  return null;
}

function mount(cells: Cell[], busy: boolean) {
  const daemon = fakeDaemon();
  const view = render(
    <PlatformProvider
      platform={{ kind: "desktop", transport: daemon.transport, titleBarDrag: false, native }}
    >
      <Probe cells={cells} busy={busy} />
    </PlatformProvider>,
  );
  const rerender = (nextCells: Cell[], nextBusy: boolean) =>
    view.rerender(
      <PlatformProvider
        platform={{ kind: "desktop", transport: daemon.transport, titleBarDrag: false, native }}
      >
        <Probe cells={nextCells} busy={nextBusy} />
      </PlatformProvider>,
    );
  return { ...view, rerender };
}

beforeEach(() => {
  native = fakeNative();
  focused = false;
  title = undefined;
  vi.spyOn(document, "hasFocus").mockImplementation(() => focused);
});

afterEach(() => {
  vi.restoreAllMocks();
});

describe("pendingPrompts", () => {
  it("counts only the questions still waiting on the user", () => {
    expect(
      pendingPrompts([
        cell({ kind: "shell-confirm", answered: false }),
        cell({ kind: "input-request", answered: false }),
        cell({ kind: "input-request", answered: true }),
        cell({ kind: "assistant" }),
      ]),
    ).toBe(2);
  });

  it("counts nothing in a settled conversation", () => {
    expect(pendingPrompts([cell({ kind: "assistant" }), cell({ kind: "user" })])).toBe(0);
  });
});

describe("useTurnNotifications", () => {
  it("says the turn is done when the window is in the background", () => {
    const { rerender } = mount([], true);
    rerender([], false);
    expect(native.notifications).toEqual([
      {
        title: "Borg",
        body: "Finished.",
        sessionId: "sess-1",
        key: "turn-complete:sess-1",
      },
    ]);
  });

  it("stays quiet for a turn the user is sitting and watching", () => {
    focused = true;
    const { rerender } = mount([], true);
    rerender([], false);
    expect(native.notifications).toEqual([]);
  });

  it("says nothing while a turn is still running", () => {
    const { rerender } = mount([], false);
    rerender([], true);
    expect(native.notifications).toEqual([]);
  });

  it("collapses a burst of finished turns onto one key", () => {
    // A wall of identical banners is not more news than one.
    const { rerender } = mount([], true);
    rerender([], false);
    rerender([], true);
    rerender([], false);
    expect(new Set(native.notifications.map((n) => (n as { key: string }).key)).size).toBe(1);
  });

  it("badges the pending questions and asks for an answer", () => {
    const { rerender } = mount([], false);
    native.badges.length = 0;

    rerender([cell({ kind: "input-request", answered: false })], false);
    expect(native.badges).toEqual([1]);
    expect(native.notifications.at(-1)).toMatchObject({
      title: "Borg",
      body: "Borg is waiting on an answer.",
      key: "prompt:sess-1",
    });
  });

  it("titles the banner with the conversation and quotes the reply", () => {
    title = "Ship it";
    const { rerender } = mount([cell({ kind: "user", text: "go" })], true);
    rerender(
      [cell({ kind: "user", text: "go" }), cell({ kind: "assistant", text: "All 12 tests pass." })],
      false,
    );
    expect(native.notifications).toEqual([
      {
        title: "Ship it",
        body: "All 12 tests pass.",
        sessionId: "sess-1",
        key: "turn-complete:sess-1",
      },
    ]);
  });

  it("names the command a prompt is asking to run", () => {
    title = "Ship it";
    const { rerender } = mount([], true);
    rerender([cell({ kind: "shell-confirm", command: "git push", answered: false })], true);
    expect(native.notifications.at(-1)).toMatchObject({
      title: "Ship it",
      body: "Borg wants to run: git push",
    });
  });

  it("does not notify again because the reply or the title changed", () => {
    const { rerender } = mount([], true);
    rerender([cell({ kind: "assistant", text: "one" })], false);
    title = "Named late";
    rerender([cell({ kind: "assistant", text: "one two" })], false);
    expect(native.notifications).toHaveLength(1);
  });

  it("does not ask again while the count is unchanged", () => {
    // `cells` is a fresh array on every streamed token, and each badge call is
    // an IPC round trip.
    const cells = [cell({ kind: "input-request", answered: false })];
    const { rerender } = mount(cells, false);
    const badgesAfterMount = native.badges.length;

    rerender([...cells], false);
    rerender([...cells], false);
    expect(native.badges).toHaveLength(badgesAfterMount);
  });

  it("clears the badge when a question is answered, without a notification", () => {
    const { rerender } = mount([cell({ kind: "input-request", answered: false })], false);
    const notificationsBefore = native.notifications.length;

    rerender([cell({ kind: "input-request", answered: true })], false);
    expect(native.badges.at(-1)).toBe(0);
    expect(native.notifications).toHaveLength(notificationsBefore);
  });

  it("badges without nagging when the user is already looking at the window", () => {
    focused = true;
    const { rerender } = mount([], false);
    rerender([cell({ kind: "shell-confirm", answered: false })], false);
    expect(native.badges.at(-1)).toBe(1);
    expect(native.notifications).toEqual([]);
  });

  it("clears the badge when the conversation closes", () => {
    // A badge left behind would never clear.
    const { unmount } = mount([cell({ kind: "input-request", answered: false })], false);
    unmount();
    expect(native.badges.at(-1)).toBe(0);
  });
});
