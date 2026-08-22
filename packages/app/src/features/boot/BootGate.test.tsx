import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import type { BorgTransport } from "@borg/client";
import { NO_NATIVE, type BootStatus, type NativeBridge } from "../../lib/native";
import { PlatformProvider, type Platform } from "../../lib/platform";
import { BootGate } from "./BootGate";

afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

const transport = {} as BorgTransport;

/** A shell whose boot status the test drives. */
function bridge(initial: BootStatus) {
  let push: ((status: BootStatus) => void) | undefined;
  const retry = vi.fn();
  const native: NativeBridge = {
    ...NO_NATIVE,
    boot: {
      subscribe: (handler) => {
        push = handler;
        handler(initial);
        return () => {};
      },
      retry,
    },
  };
  return { native, retry, emit: (status: BootStatus) => act(() => push?.(status)) };
}

function mount(native: NativeBridge) {
  const platform: Platform = { kind: "desktop", transport, titleBarDrag: true, native };
  return render(
    <PlatformProvider platform={platform}>
      <BootGate>
        <div>the app</div>
      </BootGate>
    </PlatformProvider>,
  );
}

const STARTING: BootStatus = {
  state: "starting",
  message: "Starting Borg…",
  progress: 0.45,
  canRetry: false,
};
const READY: BootStatus = { state: "ready", message: "Ready", progress: 1, canRetry: false };
const FAILED: BootStatus = {
  state: "failed",
  message: "Borg did not finish starting. Try again, or restart your computer.",
  progress: 1,
  canRetry: true,
};

describe("BootGate", () => {
  it("shows the app at once when the daemon is already up", () => {
    // A warm start must not flash a loading screen at anyone.
    const { native } = bridge(READY);
    mount(native);
    expect(screen.getByText("the app")).toBeTruthy();
    expect(screen.queryByText("Starting Borg…")).toBeNull();
  });

  it("stays blank for a short boot rather than flashing a screen", () => {
    vi.useFakeTimers();
    const { native } = bridge(STARTING);
    mount(native);
    expect(screen.queryByText("Starting Borg…")).toBeNull();
    expect(screen.queryByText("the app")).toBeNull();
  });

  it("explains the wait once it outlasts the grace period", () => {
    vi.useFakeTimers();
    const { native } = bridge(STARTING);
    mount(native);
    act(() => void vi.advanceTimersByTime(500));
    expect(screen.getByText("Starting Borg…")).toBeTruthy();
    // The shell is still held back — pages that would all report an outage
    // must not render behind the wait.
    expect(screen.queryByText("the app")).toBeNull();
  });

  it("hands over to the app the moment the daemon answers", () => {
    vi.useFakeTimers();
    const { native, emit } = bridge(STARTING);
    mount(native);
    act(() => void vi.advanceTimersByTime(500));
    emit(READY);
    expect(screen.getByText("the app")).toBeTruthy();
  });

  it("offers a retry on failure and asks the shell to try again", () => {
    // A failure is shown without waiting out the grace period: there is
    // nothing left to wait for.
    const { native, retry } = bridge(FAILED);
    mount(native);
    expect(screen.getByText("Borg could not start.")).toBeTruthy();
    expect(screen.getByText(FAILED.message)).toBeTruthy();
    fireEvent.click(screen.getByText("Try again"));
    expect(retry).toHaveBeenCalled();
  });

  it("never traps the user: the app is reachable even from a failed boot", () => {
    const { native } = bridge(FAILED);
    mount(native);
    fireEvent.click(screen.getByText("Look around anyway"));
    expect(screen.getByText("the app")).toBeTruthy();
  });

  it("does not gate a surface with no native shell", () => {
    // The browser reached the gateway to load this page at all, so there is
    // no local daemon for it to start and no screen to show.
    const platform: Platform = { kind: "web", transport, titleBarDrag: false };
    render(
      <PlatformProvider platform={platform}>
        <BootGate>
          <div>the app</div>
        </BootGate>
      </PlatformProvider>,
    );
    expect(screen.getByText("the app")).toBeTruthy();
  });
});
