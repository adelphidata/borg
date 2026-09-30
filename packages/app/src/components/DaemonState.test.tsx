import { afterEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { BorgError, GrpcStatus, type BorgTransport } from "@borg/client";
import { NO_NATIVE, type NativeBridge } from "../lib/native";
import { PlatformProvider, type Platform } from "../lib/platform";
import { useQuery } from "../lib/useDaemon";
import { DAEMON_RETRY_MS, DaemonState } from "./DaemonState";

afterEach(cleanup);

const transport = {} as BorgTransport;
const offline = {
  state: "error",
  error: new BorgError(GrpcStatus.Unavailable, "connect ENOENT"),
} as const;

function mount(kind: Platform["kind"], native?: NativeBridge, refetch?: () => void) {
  const platform: Platform = { kind, transport, titleBarDrag: kind === "desktop", native };
  return render(
    <PlatformProvider platform={platform}>
      <DaemonState query={{ ...offline, refetch }} label="settings">
        {() => <div>loaded</div>}
      </DaemonState>
    </PlatformProvider>,
  );
}

describe("DaemonState", () => {
  it("re-runs the failed call once the shell has started Borg", async () => {
    // Starting the daemon is only half the remedy: without the refetch the
    // outage stays on screen and the button looks like it did nothing.
    const refetch = vi.fn();
    const retry = vi.fn(() => Promise.resolve());
    const native: NativeBridge = { ...NO_NATIVE, boot: { ...NO_NATIVE.boot, retry } };
    mount("desktop", native, refetch);
    fireEvent.click(screen.getByText("Start Borg"));
    expect(retry).toHaveBeenCalled();
    await waitFor(() => expect(refetch).toHaveBeenCalled());
  });

  it("sends the browser to the terminal, which is the only remedy it has", () => {
    mount("web");
    expect(screen.getByText("Borg is not running. Start it with `borg daemon`.")).toBeTruthy();
    expect(screen.queryByText("Start Borg")).toBeNull();
  });

  it("recovers by itself once the daemon answers again, without a loading flash", async () => {
    // A daemon restart used to leave "Borg has stopped running" on screen
    // until someone clicked, long after Borg was back.
    vi.useFakeTimers();
    try {
      let answer: ((value: string) => void) | undefined;
      const attempt = vi.fn(() =>
        attempt.mock.calls.length === 1
          ? Promise.reject(new BorgError(GrpcStatus.Unavailable, "connect ENOENT"))
          : new Promise<string>((resolve) => (answer = resolve)),
      );
      function Page() {
        const query = useQuery(attempt);
        return (
          <DaemonState query={query} label="settings">
            {(data) => <div>{data}</div>}
          </DaemonState>
        );
      }
      render(
        <PlatformProvider platform={{ kind: "web", transport, titleBarDrag: false }}>
          <Page />
        </PlatformProvider>,
      );
      await act(async () => {
        await vi.advanceTimersByTimeAsync(0);
      });
      expect(screen.getByText("Could not load settings.")).toBeTruthy();

      // The retry is in flight: the failure stays put rather than blinking
      // to a loading line and back every couple of seconds.
      await act(async () => {
        await vi.advanceTimersByTimeAsync(DAEMON_RETRY_MS);
      });
      expect(attempt).toHaveBeenCalledTimes(2);
      expect(screen.getByText("Could not load settings.")).toBeTruthy();
      expect(screen.queryByText(/Loading/)).toBeNull();
      // A slow answer is waited for, not abandoned for another attempt.
      await act(async () => {
        await vi.advanceTimersByTimeAsync(DAEMON_RETRY_MS * 3);
      });
      expect(attempt).toHaveBeenCalledTimes(2);

      await act(async () => answer?.("fresh"));
      expect(screen.getByText("fresh")).toBeTruthy();
      // Answered: the polling stops.
      await act(async () => {
        await vi.advanceTimersByTimeAsync(DAEMON_RETRY_MS * 3);
      });
      expect(attempt).toHaveBeenCalledTimes(2);
    } finally {
      vi.useRealTimers();
    }
  });
});
