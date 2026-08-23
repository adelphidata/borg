import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { BorgError, GrpcStatus, type BorgTransport } from "@borg/client";
import { NO_NATIVE, type NativeBridge } from "../lib/native";
import { PlatformProvider, type Platform } from "../lib/platform";
import { DaemonState } from "./DaemonState";

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
});
