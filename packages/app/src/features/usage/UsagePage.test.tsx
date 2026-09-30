import { describe, expect, it } from "vitest";
import { act, screen, waitFor } from "@testing-library/react";
import { BorgError, GrpcStatus } from "@borg/client";
import { fakeDaemon, renderApp } from "../../test/harness";
import { UsagePage } from "./UsagePage";

const MONTH = {
  GetMonthlyTokenTotal: { total: 1_234_567 },
  GetMonthlyTotalCost: { present: true, cost: 12.5 },
};

const costEvent = (over: Partial<Record<string, unknown>> = {}) => ({
  provider: "anthropic",
  model: "claude-opus-5",
  costUsd: "0.25",
  costUsdPresent: true,
  promptTokens: 1000,
  completionTokens: 500,
  ...over,
});

/** Push a cost event down the open stream. */
function emit(daemon: ReturnType<typeof fakeDaemon>, event: unknown) {
  const stream = daemon.streams.find((s) => s.method === "StreamCostEvents");
  if (!stream) throw new Error("cost stream was never opened");
  act(() => stream.handlers.onMessage(event as never));
}

describe("UsagePage", () => {
  it("shows the daemon's month-to-date totals", async () => {
    const daemon = fakeDaemon(MONTH);
    renderApp(<UsagePage />, { daemon });

    await waitFor(() => expect(screen.getByText("$12.50")).toBeTruthy());
    expect(screen.getByText("1.2M")).toBeTruthy();
  });

  it("distinguishes no priced calls from a genuine zero", async () => {
    // A provider that reports nothing must not read as "this month was free".
    const daemon = fakeDaemon({
      ...MONTH,
      GetMonthlyTotalCost: { present: false, cost: 0 },
    });
    renderApp(<UsagePage />, { daemon });

    await waitFor(() => expect(screen.getByText("No priced calls yet")).toBeTruthy());
    expect(screen.getByText("—")).toBeTruthy();
  });

  it("says the totals could not be loaded rather than showing zeros", async () => {
    const daemon = fakeDaemon({
      GetMonthlyTokenTotal: new BorgError(GrpcStatus.Unavailable, "daemon down"),
      GetMonthlyTotalCost: { present: true, cost: 0 },
    });
    renderApp(<UsagePage />, { daemon });

    await waitFor(() => expect(screen.getByText("Could not load your usage.")).toBeTruthy());
  });

  it("waits for a call rather than implying no spend", async () => {
    const daemon = fakeDaemon(MONTH);
    renderApp(<UsagePage />, { daemon });
    await waitFor(() => expect(screen.getByText("Waiting for the next model call.")).toBeTruthy());
  });

  it("breaks live spend down per model, most expensive first", async () => {
    const daemon = fakeDaemon(MONTH);
    renderApp(<UsagePage />, { daemon });
    await waitFor(() => expect(screen.getByText("Waiting for the next model call.")).toBeTruthy());

    emit(daemon, costEvent({ model: "haiku-4-5", costUsd: "0.01" }));
    emit(daemon, costEvent({ model: "claude-opus-5", costUsd: "0.75" }));

    await waitFor(() =>
      expect(screen.getByText("2 calls since this page opened · $0.76")).toBeTruthy(),
    );
    const models = screen.getAllByText(/claude-opus-5|haiku-4-5/).map((el) => el.textContent);
    expect(models[0]).toBe("claude-opus-5");
  });

  it("counts tokens for a provider that reports no cost", async () => {
    // Otherwise a local model would drag the total toward zero.
    const daemon = fakeDaemon(MONTH);
    renderApp(<UsagePage />, { daemon });
    await waitFor(() => expect(screen.getByText("Waiting for the next model call.")).toBeTruthy());

    emit(
      daemon,
      costEvent({ provider: "ollama", model: "llama", costUsd: "", costUsdPresent: false }),
    );
    await waitFor(() =>
      expect(screen.getByText("1 call since this page opened · $0.00")).toBeTruthy(),
    );
    expect(screen.getByText("1.5K tok")).toBeTruthy();
  });

  it("clears the live list when asked, keeping the month intact", async () => {
    const daemon = fakeDaemon(MONTH);
    renderApp(<UsagePage />, { daemon });
    await waitFor(() => expect(screen.getByText("Waiting for the next model call.")).toBeTruthy());
    emit(daemon, costEvent());
    await waitFor(() => expect(screen.getByText("Clear")).toBeTruthy());

    act(() => screen.getByText("Clear").click());
    expect(screen.getByText("Waiting for the next model call.")).toBeTruthy();
    expect(screen.getByText("$12.50")).toBeTruthy();
  });

  it("offers no Clear when there is nothing to clear", async () => {
    const daemon = fakeDaemon(MONTH);
    renderApp(<UsagePage />, { daemon });
    await waitFor(() => expect(screen.getByText("Waiting for the next model call.")).toBeTruthy());
    expect(screen.queryByText("Clear")).toBeNull();
  });

  it("says the stream is down rather than claiming it is live", async () => {
    const daemon = fakeDaemon(MONTH);
    renderApp(<UsagePage />, { daemon });
    await waitFor(() => expect(screen.getByText("Waiting for the next model call.")).toBeTruthy());

    const stream = daemon.streams.find((s) => s.method === "StreamCostEvents")!;
    act(() => stream.handlers.onError?.(new BorgError(GrpcStatus.Unavailable, "gone")));
    await waitFor(() => expect(screen.getByText("The cost stream is not connected.")).toBeTruthy());
  });

  it("redials the cost stream after a drop, so a daemon restart does not leave it dead", async () => {
    const daemon = fakeDaemon(MONTH);
    renderApp(<UsagePage />, { daemon });
    await waitFor(() => expect(screen.getByText("Waiting for the next model call.")).toBeTruthy());

    const first = daemon.streams.find((s) => s.method === "StreamCostEvents")!;
    act(() => first.handlers.onError?.(new BorgError(GrpcStatus.Unavailable, "gone")));
    await waitFor(() => expect(screen.getByText("The cost stream is not connected.")).toBeTruthy());

    // The first retry waits at most FIRST_DELAY_MS.
    await waitFor(
      () => expect(daemon.streams.filter((s) => s.method === "StreamCostEvents").length).toBe(2),
      { timeout: 2_000 },
    );
    expect(screen.getByText("Waiting for the next model call.")).toBeTruthy();
  });

  it("drops live events when the stream fails, rather than freezing a stale total", async () => {
    const daemon = fakeDaemon(MONTH);
    renderApp(<UsagePage />, { daemon });
    await waitFor(() => expect(screen.getByText("Waiting for the next model call.")).toBeTruthy());
    emit(daemon, costEvent());

    const stream = daemon.streams.find((s) => s.method === "StreamCostEvents")!;
    act(() => stream.handlers.onError?.(new BorgError(GrpcStatus.Unavailable, "gone")));
    await waitFor(() => expect(screen.getByText("The cost stream is not connected.")).toBeTruthy());
  });
});
