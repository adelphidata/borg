import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, screen } from "@testing-library/react";
import { dispatchKeybind } from "./keybinds";
import { clearNotice, pendingNotice } from "./notice";
import { fakeNative, renderApp } from "../test/harness";
import type { UpdateStatus } from "./native";
import { checkFailureReason, sidebarUpdate, useUpdater } from "./updater";

beforeEach(() => {
  clearNotice();
});

afterEach(() => {
  clearNotice();
});

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

/** Renders what the sidebar would get, so the hook can be asserted on. */
function Probe() {
  const { status, install } = useUpdater();
  const update = sidebarUpdate(status);
  if (!update) return null;
  return (
    <button type="button" onClick={install}>
      {update.state} {update.version ?? "?"} {update.percent ?? "-"}
    </button>
  );
}

describe("checkFailureReason", () => {
  it.each([
    ["net::ERR_NAME_NOT_RESOLVED", "Check your internet connection, then try again."],
    ["getaddrinfo ENOTFOUND github.com", "Check your internet connection, then try again."],
    ["HttpError: 404 Not Found", "Try again in a little while."],
    [undefined, "Try again in a little while."],
  ])("turns %s into a next step", (message, reason) => {
    vi.spyOn(navigator, "onLine", "get").mockReturnValue(true);
    expect(checkFailureReason(message)).toBe(reason);
  });

  it("trusts the browser's own word that the machine is offline", () => {
    vi.spyOn(navigator, "onLine", "get").mockReturnValue(false);
    expect(checkFailureReason("HttpError: 404 Not Found")).toBe(
      "Check your internet connection, then try again.",
    );
  });
});

describe("sidebarUpdate", () => {
  it("shows nothing until there is a build on its way", () => {
    for (const state of ["idle", "checking", "error"] as const) {
      expect(sidebarUpdate({ state })).toBeUndefined();
    }
  });

  it("carries the build from first sight to ready", () => {
    expect(sidebarUpdate({ state: "available", version: "0.5.0" })).toEqual({
      state: "available",
      version: "0.5.0",
      percent: undefined,
    });
    expect(sidebarUpdate({ state: "downloading", version: "0.5.0", percent: 42 })).toEqual({
      state: "downloading",
      version: "0.5.0",
      percent: 42,
    });
    expect(sidebarUpdate({ state: "ready", version: "0.5.0" })).toEqual({
      state: "ready",
      version: "0.5.0",
      percent: undefined,
    });
  });
});

describe("useUpdater", () => {
  it("stays out of the way until a build is found", () => {
    const harness = updatableNative();
    const { container } = renderApp(<Probe />, { native: harness.native });
    for (const state of ["idle", "checking", "error"] as const) {
      harness.emit({ state });
      expect(container.firstChild).toBeNull();
    }
  });

  it("follows the download through to the restart", () => {
    const harness = updatableNative();
    renderApp(<Probe />, { native: harness.native });
    harness.emit({ state: "available", version: "0.5.0" });
    expect(screen.getByText("available 0.5.0 -")).toBeTruthy();
    harness.emit({ state: "downloading", version: "0.5.0", percent: 41 });
    expect(screen.getByText("downloading 0.5.0 41")).toBeTruthy();
    harness.emit({ state: "ready", version: "0.5.0" });
    act(() => screen.getByText("ready 0.5.0 -").click());
    expect(harness.installs).toHaveLength(1);
  });

  it("survives a ready build that reports no version", () => {
    const harness = updatableNative();
    renderApp(<Probe />, { native: harness.native });
    harness.emit({ state: "ready" });
    expect(screen.getByText("ready ? -")).toBeTruthy();
  });

  it("asks the shell to check when the user does", () => {
    const harness = updatableNative();
    renderApp(<Probe />, { native: harness.native });
    // The action a menu item and a typed chord both resolve to.
    act(() => {
      expect(dispatchKeybind("check-for-updates")).toBe(true);
    });
    expect(harness.checks).toHaveLength(1);
  });

  describe("a check the user asked for", () => {
    const ask = () =>
      act(() => {
        dispatchKeybind("check-for-updates");
      });

    it("says so when there is nothing newer", () => {
      // The menu item used to do nothing visible: the updater answers "idle"
      // and the sidebar only ever shows a build.
      const harness = updatableNative();
      renderApp(<Probe />, { native: harness.native });
      ask();
      harness.emit({ state: "checking" });
      expect(pendingNotice()).toBeUndefined();
      harness.emit({ state: "idle" });
      expect(pendingNotice()?.message).toBe("Borg is up to date");
    });

    it("says why when the check could not run", () => {
      const harness = updatableNative();
      renderApp(<Probe />, { native: harness.native });
      ask();
      harness.emit({ state: "error", message: "net::ERR_INTERNET_DISCONNECTED" });
      expect(pendingNotice()?.message).toBe(
        "Couldn't check for updates. Check your internet connection, then try again.",
      );
      // Never the raw code.
      expect(pendingNotice()?.message).not.toContain("net::");
    });

    it.each(["available", "downloading"] as const)(
      "says the download failed, not the check, when it fails after %s",
      (found) => {
        const harness = updatableNative();
        renderApp(<Probe />, { native: harness.native });
        ask();
        harness.emit({ state: found, version: "0.6.0" });
        harness.emit({ state: "error", message: "boom" });
        expect(pendingNotice()?.message).toMatch(/^Couldn't download the update\. /);
      },
    );

    it("says the check failed on a new request after an earlier download failed", () => {
      // What the last request found must not leak into the next one.
      const harness = updatableNative();
      renderApp(<Probe />, { native: harness.native });
      ask();
      harness.emit({ state: "downloading" });
      harness.emit({ state: "error", message: "boom" });
      clearNotice();
      ask();
      harness.emit({ state: "error", message: "boom" });
      expect(pendingNotice()?.message).toMatch(/^Couldn't check for updates\. /);
    });

    it("answers once, not on every later status", () => {
      // The updater reports the same failure through two paths; two toasts
      // for one check would be the same news twice.
      const harness = updatableNative();
      renderApp(<Probe />, { native: harness.native });
      ask();
      harness.emit({ state: "error", message: "boom" });
      clearNotice();
      harness.emit({ state: "error", message: "boom" });
      harness.emit({ state: "idle" });
      expect(pendingNotice()).toBeUndefined();
    });

    it("lets the sidebar answer when the check found a build", () => {
      const harness = updatableNative();
      renderApp(<Probe />, { native: harness.native });
      ask();
      harness.emit({ state: "available", version: "0.6.0" });
      harness.emit({ state: "ready", version: "0.6.0" });
      expect(screen.getByText("ready 0.6.0 -")).toBeTruthy();
      expect(pendingNotice()).toBeUndefined();
    });
  });

  it("stays silent for a check it ran on its own", () => {
    // Automatic checks happen at startup and on a timer; a toast each time
    // would be the update system talking to a user who did not ask.
    const harness = updatableNative();
    renderApp(<Probe />, { native: harness.native });
    harness.emit({ state: "checking" });
    harness.emit({ state: "idle" });
    harness.emit({ state: "error", message: "boom" });
    expect(pendingNotice()).toBeUndefined();
  });

  it("unsubscribes when it goes away", () => {
    const harness = updatableNative();
    const { unmount } = renderApp(<Probe />, { native: harness.native });
    expect(harness.subscribed()).toBe(true);
    unmount();
    expect(harness.subscribed()).toBe(false);
  });
});
