import { describe, expect, it, vi } from "vitest";
import { screen } from "@testing-library/react";
import { BorgError, GrpcStatus } from "@borg/client";
import { STORAGE_KEYS } from "@borg/ui";
import { fakeNative, renderApp } from "../test/harness";
import { DaemonState } from "./DaemonState";

const unavailable = {
  state: "error" as const,
  error: new BorgError(GrpcStatus.Unavailable, "connect failed"),
};
const unauthenticated = {
  state: "error" as const,
  error: new BorgError(GrpcStatus.Unauthenticated, "bad token"),
};

const body = (data: string) => <span>{data}</span>;

describe("DaemonState on desktop", () => {
  it("offers to start Borg, and re-runs the call that failed", async () => {
    // Starting the daemon is only half the remedy: without the refetch the
    // outage stays on screen and the button looks like it did nothing.
    const refetch = vi.fn();
    const retry = vi.fn(async () => {});
    const native = fakeNative();
    native.boot.retry = retry;

    renderApp(
      <DaemonState query={{ ...unavailable, refetch }} label="your usage">
        {body}
      </DaemonState>,
      { native, kind: "desktop" },
    );

    expect(screen.getByText("Borg has stopped running.")).toBeTruthy();
    screen.getByRole("button", { name: "Start Borg" }).click();
    await vi.waitFor(() => expect(refetch).toHaveBeenCalledTimes(1));
    expect(retry).toHaveBeenCalledTimes(1);
  });

  it("never tells a desktop user to run a terminal command", () => {
    renderApp(
      <DaemonState query={unavailable} label="sessions">
        {body}
      </DaemonState>,
      {
        kind: "desktop",
      },
    );
    expect(screen.queryByText(/borg daemon/)).toBeNull();
  });

  it("offers no sign-in remedy where there is no token", () => {
    // The desktop socket is the credential; a sign-out button would be a dead
    // end there.
    renderApp(
      <DaemonState query={unauthenticated} label="sessions">
        {body}
      </DaemonState>,
      {
        kind: "desktop",
      },
    );
    expect(screen.queryByRole("button", { name: "Sign in again" })).toBeNull();
  });
});

describe("DaemonState on the web", () => {
  it("names the command that starts the daemon", () => {
    renderApp(
      <DaemonState query={unavailable} label="sessions">
        {body}
      </DaemonState>,
      {
        kind: "web",
      },
    );
    expect(screen.getByText("Borg is not running. Start it with `borg daemon`.")).toBeTruthy();
    expect(screen.queryByRole("button", { name: "Start Borg" })).toBeNull();
  });

  it("clears a rejected token and returns to the sign-in screen", () => {
    const reload = vi.fn();
    vi.stubGlobal("location", { reload });
    localStorage.setItem(STORAGE_KEYS.token, "tok_dead");

    renderApp(
      <DaemonState query={unauthenticated} label="sessions">
        {body}
      </DaemonState>,
      {
        kind: "web",
      },
    );
    screen.getByRole("button", { name: "Sign in again" }).click();
    expect(localStorage.getItem(STORAGE_KEYS.token)).toBeNull();
    expect(reload).toHaveBeenCalledTimes(1);
    vi.unstubAllGlobals();
  });
});

describe("DaemonState in general", () => {
  it("reports an unexpected failure verbatim rather than guessing a remedy", () => {
    renderApp(
      <DaemonState
        query={{ state: "error", error: new BorgError(GrpcStatus.Internal, "index corrupt") }}
        label="sessions"
      >
        {body}
      </DaemonState>,
    );
    expect(screen.getByText("index corrupt")).toBeTruthy();
    expect(screen.queryByRole("button")).toBeNull();
  });

  it("renders the data once it is there", () => {
    renderApp(
      <DaemonState query={{ state: "ready", data: "the goods" }} label="sessions">
        {body}
      </DaemonState>,
    );
    expect(screen.getByText("the goods")).toBeTruthy();
  });
});
