import { describe, expect, it } from "vitest";
import { screen, waitFor } from "@testing-library/react";
import { BorgError, GrpcStatus } from "@borg/client";
import { fakeDaemon, renderApp } from "../../test/harness";
import { UsagePage } from "./UsagePage";

const MONTH = {
  GetMonthlyTokenTotal: { total: 1_234_567 },
  GetMonthlyTotalCost: { present: true, cost: 12.5 },
};

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

  it("opens no live cost stream, now that nothing on the page reads one", async () => {
    const daemon = fakeDaemon(MONTH);
    renderApp(<UsagePage />, { daemon });

    await waitFor(() => expect(screen.getByText("$12.50")).toBeTruthy());
    expect(daemon.streams.some((s) => s.method === "StreamCostEvents")).toBe(false);
    expect(screen.queryByText(/by model/i)).toBeNull();
  });
});
