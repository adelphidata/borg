import { describe, expect, it, vi } from "vitest";
import { act, screen } from "@testing-library/react";
import { dispatchKeybind } from "../lib/keybinds";
import { fakeNative, renderApp } from "../test/harness";
import type { UpdateStatus } from "../lib/native";
import { UpdatePill } from "./UpdatePill";

/** A native bridge whose update status the test drives. */
function updatableNative() {
  const native = fakeNative();
  let push: ((status: UpdateStatus) => void) | undefined;
  const checks: number[] = [];
  const installs: number[] = [];
  native.updates = {
    subscribe: (handler) => {
      push = handler;
      return () => {
        push = undefined;
      };
    },
    check: () => checks.push(1),
    install: () => installs.push(1),
  };
  return {
    native,
    checks,
    installs,
    emit: (status: UpdateStatus) => act(() => push?.(status)),
    subscribed: () => push !== undefined,
  };
}

describe("UpdatePill", () => {
  it("stays out of the way until a build is actually ready", () => {
    // No modal, no progress bar competing with the transcript.
    const harness = updatableNative();
    const { container } = renderApp(<UpdatePill />, { native: harness.native });

    for (const state of ["idle", "checking", "available", "downloading", "error"] as const) {
      harness.emit({ state });
      expect(container.firstChild).toBeNull();
    }
  });

  it("offers the restart once the build is downloaded", () => {
    const harness = updatableNative();
    renderApp(<UpdatePill />, { native: harness.native });
    harness.emit({ state: "ready", version: "0.5.0" });

    expect(screen.getByText("Borg 0.5.0 is ready")).toBeTruthy();
    act(() => screen.getByText("Restart").click());
    expect(harness.installs).toHaveLength(1);
  });

  it("survives a ready build that reports no version", () => {
    const harness = updatableNative();
    renderApp(<UpdatePill />, { native: harness.native });
    harness.emit({ state: "ready" });
    expect(screen.getByText(/is ready/)).toBeTruthy();
  });

  it("can be dismissed, and stays dismissed", () => {
    const harness = updatableNative();
    const { container } = renderApp(<UpdatePill />, { native: harness.native });
    harness.emit({ state: "ready", version: "0.5.0" });

    act(() => screen.getByRole("button", { name: "Dismiss" }).click());
    expect(container.firstChild).toBeNull();

    harness.emit({ state: "ready", version: "0.5.0" });
    expect(container.firstChild).toBeNull();
  });

  it("comes back when the user asks for a check", () => {
    // Otherwise dismissing once would hide the pill for the rest of the session.
    const harness = updatableNative();
    renderApp(<UpdatePill />, { native: harness.native });
    harness.emit({ state: "ready", version: "0.5.0" });
    act(() => screen.getByRole("button", { name: "Dismiss" }).click());

    // The action a menu item and a typed chord both resolve to.
    act(() => {
      expect(dispatchKeybind("check-for-updates")).toBe(true);
    });
    expect(harness.checks).toHaveLength(1);
    expect(screen.getByText("Borg 0.5.0 is ready")).toBeTruthy();
  });

  it("unsubscribes when it goes away", () => {
    const harness = updatableNative();
    const { unmount } = renderApp(<UpdatePill />, { native: harness.native });
    expect(harness.subscribed()).toBe(true);
    unmount();
    expect(harness.subscribed()).toBe(false);
  });
});
